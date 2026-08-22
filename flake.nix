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
      # Everything needed to build npb from a given `pkgs`, derived so the
      # overlay and the per-system checks share one definition. The overlay
      # exposes only `npb`; the checks additionally reach for `craneLib` /
      # `commonArgs` / `cargoArtifacts` / `binEnv`, and the dev shell for the
      # last of those.
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
          # The tools npb shells out to at runtime, baked into the binary as
          # absolute store paths at compile time (`env!("NIX_BIN")` &c. in
          # `src/main.rs`) rather than wrapped onto its PATH — npc's scheme. npb
          # then runs the exact tools it was packaged against whatever the
          # caller's PATH holds (the 2.35 `nix` above, not the user's 2.34), the
          # unwrapped binary works anywhere, and Nix keeps the closure alive off
          # these very strings. Set only on the crate's own build (not the shared
          # `cargoArtifacts`), so a tool bump never rebuilds the deps.
          binEnv = {
            GIT_BIN = "${pkgs.git}/bin/git";
            NIX_BIN = "${nix}/bin/nix";
            NIX_STORE_BIN = "${nix}/bin/nix-store";
            NIX_INSTANTIATE_BIN = "${nix}/bin/nix-instantiate";
            NIX_EVAL_JOBS_BIN = "${nix-eval-jobs}/bin/nix-eval-jobs";
            NOM_BIN = "${pkgs.nix-output-monitor}/bin/nom"; # the build front-end
          };
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
            # The `--pr` resolution tests shell out to git, but at `GIT_BIN` — no
            # build input needed for a tool named by absolute path.
            nativeBuildInputs = [ pkgs.perl ];
          };
          # Built once and shared by the package and every check.
          cargoArtifacts = craneLib.buildDepsOnly commonArgs;
          npb = craneLib.buildPackage (
            commonArgs
            // binEnv
            // {
              inherit cargoArtifacts;
              nativeBuildInputs = commonArgs.nativeBuildInputs ++ [ pkgs.installShellFiles ];
              # Completions come from the binary itself (`--completions <shell>`),
              # so they can never drift from the parser. `--completions` shells out
              # to nothing and touches no cache, so it runs fine in the sandbox —
              # and it's skipped when the build machine can't execute what we just
              # built (a cross build).
              postInstall = pkgs.lib.optionalString (pkgs.stdenv.buildPlatform.canExecute pkgs.stdenv.hostPlatform) ''
                installShellCompletion --cmd npb \
                  --bash <($out/bin/npb --completions bash) \
                  --fish <($out/bin/npb --completions fish) \
                  --zsh <($out/bin/npb --completions zsh)
              '';
            }
          );
        in
        {
          inherit
            craneLib
            commonArgs
            cargoArtifacts
            binEnv
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
        inherit (build)
          craneLib
          commonArgs
          cargoArtifacts
          binEnv
          ;
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
            // binEnv
            // {
              inherit cargoArtifacts;
            }
          );
          npb-clippy = craneLib.cargoClippy (
            commonArgs
            // binEnv
            // {
              inherit cargoArtifacts;
              cargoClippyExtraArgs = "--all-targets -- --deny warnings";
            }
          );
          npb-fmt = craneLib.cargoFmt { inherit (commonArgs) src; };
        };
        devShells.default = pkgs.mkShell (
          # `env!("NIX_BIN")` &c. are resolved at compile time, so a bare `cargo
          # build` in the dev shell needs them set too (nix builds set them
          # above). They're also why the dev shell puts none of those tools on
          # PATH: a locally built npb reaches them by absolute path already.
          binEnv
          // {
            buildInputs = [
              pkgs.rust-bin.stable.latest.default
              pkgs.markdown-toc
              pkgs.sqlite # for poking at the store during development
            ];
          }
        );
      }
    );
}
