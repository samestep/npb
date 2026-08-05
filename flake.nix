{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    crane.url = "github:ipetkov/crane";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };
  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      crane,
      rust-overlay,
    }:
    let
      # The commit npb is built on, baked in so `--version` and the report
      # heading can link to the exact source tree on GitHub (like npc's
      # `NPC_REV`). `self.rev` is absent for a dirty tree, so fall back to the
      # branch. Set only on the crate's own build (not the shared
      # `cargoArtifacts`), so bumping the commit never rebuilds the deps.
      npbRev = self.rev or "main";
      # Everything needed to build npb from a given `pkgs`, derived so the
      # overlay and the per-system checks share one definition. The overlay
      # exposes only `npb`; the checks additionally reach for `craneLib` /
      # `commonArgs` / `cargoArtifacts` and the dev shell for the pinned tools.
      npbFor =
        pkgs:
        let
          craneLib = crane.mkLib pkgs;
          nix-eval-jobs = pkgs.nix-eval-jobs;
          # npb needs Nix ≥2.35, whose lazy source-copying is load-bearing for its
          # disk story: `build_expr`'s `fetchGit` tree is only ever *read*, so 2.35
          # hashes it without materializing a ~400 MB `/nix/store/…-source` object
          # per reviewed tree (DESIGN §4). It has to be the *same series* as
          # `nix-eval-jobs`, which links Nix's internals — a mismatched pair and
          # the shard eval would still copy the tree — so take Nix from
          # `nix-eval-jobs` itself rather than naming a version here. That
          # `passthru` exists for exactly this ("we propagate the Nix used for
          # building it to make it easier for users downstream to reference it"),
          # and it makes the invariant structural: nixpkgs' default `nix` is still
          # the 2.34 series, so npb can't use it, but a hand-written 2.35 pin
          # beside a `nix-eval-jobs` that nixpkgs later builds against 2.36
          # components would silently drift apart. It's also one runtime closure
          # instead of two Nix builds (`nixVersions.nix_2_35` and these
          # components) that only happen to agree.
          nix = nix-eval-jobs.nix;
          # npb shells out to these at runtime; wrap them onto its PATH so the
          # packaged binary works outside the dev shell (`nix shell .`).
          runtimeDeps = [
            nix
            nix-eval-jobs
            pkgs.nix-output-monitor # `nom`, the build front-end
            pkgs.git
          ];
          # crane's default source filter keeps only Cargo/`.rs` files, which
          # would drop `src/schema.sql` (embedded via `include_str!`) and break
          # the build. Widen it to also keep `.sql` — crane's documented idiom
          # for exactly this.
          sqlOrCargo =
            path: type: (pkgs.lib.hasSuffix ".sql" path) || (craneLib.filterCargoSources path type);
          commonArgs = {
            src = pkgs.lib.cleanSourceWith {
              src = ./.;
              filter = sqlOrCargo;
              name = "source"; # keep the store path stable regardless of the dir name
            };
            strictDeps = true;
            # ring (via ureq's TLS) needs perl at build; rusqlite bundles sqlite (cc).
            # git: the `--pr` resolution tests shell out to it against a fixture repo.
            nativeBuildInputs = [
              pkgs.perl
              pkgs.git
            ];
          };
          # Built once and shared by the package and every check.
          cargoArtifacts = craneLib.buildDepsOnly commonArgs;
          npb = craneLib.buildPackage (
            commonArgs
            // {
              inherit cargoArtifacts;
              NPB_REV = npbRev;
              # makeBinaryWrapper, not makeWrapper: the bash wrapper costs ~4 ms
              # of PATH munging per invocation, the compiled one ~0.1 ms.
              nativeBuildInputs = commonArgs.nativeBuildInputs ++ [
                pkgs.installShellFiles
                pkgs.makeBinaryWrapper
              ];
              # Completions come from the binary itself (`--completions <shell>`),
              # so they can never drift from the parser. Generated before the wrap
              # — `--completions` shells out to nothing and touches no cache, so it
              # runs fine unwrapped in the sandbox — and skipped when the build
              # machine can't execute what we just built (a cross build).
              postInstall = ''
                ${pkgs.lib.optionalString (pkgs.stdenv.buildPlatform.canExecute pkgs.stdenv.hostPlatform) ''
                  installShellCompletion --cmd npb \
                    --bash <($out/bin/npb --completions bash) \
                    --fish <($out/bin/npb --completions fish) \
                    --zsh <($out/bin/npb --completions zsh)
                ''}
                wrapProgram $out/bin/npb --prefix PATH : ${pkgs.lib.makeBinPath runtimeDeps}
              '';
            }
          );
        in
        {
          inherit
            craneLib
            commonArgs
            cargoArtifacts
            nix
            nix-eval-jobs
            npb
            ;
        };
      # The overlay adds `npb` to a package set, so downstream flakes can install
      # it via `npb.overlays.default` (like `npc.overlays.default`).
      overlay = final: _prev: {
        npb = (npbFor final).npb;
      };
    in
    {
      overlays.default = overlay;
    }
    // flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [
            (import rust-overlay)
            overlay
          ];
        };
        build = npbFor pkgs;
        inherit (build) craneLib commonArgs cargoArtifacts;
      in
      {
        packages.default = pkgs.npb;
        # `nix flake check` runs these in the sandbox. Only the default (non-
        # `#[ignore]`d) tests run here — the sole ignored test builds real
        # derivations and needs the daemon, which the sandbox rightly denies.
        checks = {
          npb = pkgs.npb;
          npb-test = craneLib.cargoTest (
            commonArgs
            // {
              inherit cargoArtifacts;
              NPB_REV = npbRev;
            }
          );
          npb-clippy = craneLib.cargoClippy (
            commonArgs
            // {
              inherit cargoArtifacts;
              NPB_REV = npbRev;
              cargoClippyExtraArgs = "--all-targets -- --deny warnings";
            }
          );
          npb-fmt = craneLib.cargoFmt { inherit (commonArgs) src; };
        };
        devShells.default = pkgs.mkShell {
          # `env!("NPB_REV")` is resolved at compile time, so a bare `cargo
          # build` in the dev shell needs it set too (nix builds set it above).
          NPB_REV = npbRev;
          buildInputs = [
            pkgs.rust-bin.stable.latest.default
            build.nix
            build.nix-eval-jobs
            pkgs.nix-output-monitor
            pkgs.markdown-toc
            pkgs.sqlite # for poking at the store during development
          ];
        };
      }
    );
}
