{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
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
      # The commit npd is built on, baked in so `--version` and the report
      # heading can link to the exact source tree on GitHub (like npc's
      # `NPC_REV`). `self.rev` is absent for a dirty tree, so fall back to the
      # branch. Set only on the crate's own build (not the shared
      # `cargoArtifacts`), so bumping the commit never rebuilds the deps.
      npdRev = self.rev or "main";
      # Everything needed to build npd from a given `pkgs`, derived so the
      # overlay and the per-system checks share one definition. The overlay
      # exposes only `npd`; the checks additionally reach for `craneLib` /
      # `commonArgs` / `cargoArtifacts` and the dev shell for the pinned tools.
      npdFor =
        pkgs:
        let
          craneLib = crane.mkLib pkgs;
          # npd needs Nix ≥2.35, whose lazy source-copying is load-bearing for
          # its disk story: `build_expr`'s `fetchGit` tree is only ever *read*,
          # so 2.35 hashes it without materializing a ~400 MB
          # `/nix/store/…-source` object per reviewed tree (DESIGN §4). nixpkgs'
          # default `nix` is still the 2.34 series, so pin 2.35 explicitly.
          nix = pkgs.nixVersions.nix_2_35;
          # `nix-eval-jobs` links Nix's internals, so it must be the same series
          # — otherwise the shard eval would still copy the tree. nixpkgs only
          # packages the 2.34 release yet, so build the 2.35.0 release candidate
          # (nix-eval-jobs#428) against the matching components. That commit also
          # carries the macOS `ru_maxrss` fix (#426), letting `stream_jobs` drop
          # its ×1024 workaround (DESIGN §9).
          nix-eval-jobs = (pkgs.nix-eval-jobs.override {
            nixComponents = pkgs.nixVersions.nixComponents_2_35;
          }).overrideAttrs (_: {
            version = "2.35.0-unstable-2026-07-16";
            src = pkgs.fetchFromGitHub {
              owner = "nix-community";
              repo = "nix-eval-jobs";
              rev = "97997acae32aece824bc4c07bcd97f0823783919"; # pr #428 "release 2.35.0"
              hash = "sha256-/C5wyGYe4uMKKH26vy3knpwP/hvjOHO/58cySL8ADC4=";
            };
          });
          # npd shells out to these at runtime; wrap them onto its PATH so the
          # packaged binary works outside the dev shell (`nix shell .`).
          runtimeDeps = [
            nix
            nix-eval-jobs
            pkgs.nix-output-monitor # `nom`, the build front-end
            pkgs.git
            pkgs.nettools # `hostname`
          ];
          # crane's default source filter keeps only Cargo/`.rs` files, which
          # would drop `src/schema.sql` (embedded via `include_str!`) and break
          # the build. Widen it to also keep `.sql` — crane's documented idiom
          # for exactly this.
          sqlOrCargo =
            path: type:
            (pkgs.lib.hasSuffix ".sql" path) || (craneLib.filterCargoSources path type);
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
          npd = craneLib.buildPackage (
            commonArgs
            // {
              inherit cargoArtifacts;
              NPD_REV = npdRev;
              # makeBinaryWrapper, not makeWrapper: the bash wrapper costs ~4 ms
              # of PATH munging per invocation, the compiled one ~0.1 ms.
              nativeBuildInputs = commonArgs.nativeBuildInputs ++ [ pkgs.makeBinaryWrapper ];
              postInstall = ''
                wrapProgram $out/bin/npd --prefix PATH : ${pkgs.lib.makeBinPath runtimeDeps}
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
            npd
            ;
        };
      # The overlay adds `npd` to a package set, so downstream flakes can install
      # it via `npd.overlays.default` (like `npc.overlays.default`).
      overlay = final: _prev: {
        npd = (npdFor final).npd;
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
        build = npdFor pkgs;
        inherit (build) craneLib commonArgs cargoArtifacts;
      in
      {
        packages.default = pkgs.npd;
        # `nix flake check` runs these in the sandbox. Only the default (non-
        # `#[ignore]`d) tests run here — the sole ignored test builds real
        # derivations and needs the daemon, which the sandbox rightly denies.
        checks = {
          npd = pkgs.npd;
          npd-test = craneLib.cargoTest (commonArgs // { inherit cargoArtifacts; NPD_REV = npdRev; });
          npd-clippy = craneLib.cargoClippy (
            commonArgs
            // {
              inherit cargoArtifacts;
              NPD_REV = npdRev;
              cargoClippyExtraArgs = "--all-targets -- --deny warnings";
            }
          );
          npd-fmt = craneLib.cargoFmt { inherit (commonArgs) src; };
        };
        devShells.default = pkgs.mkShell {
          # `env!("NPD_REV")` is resolved at compile time, so a bare `cargo
          # build` in the dev shell needs it set too (nix builds set it above).
          NPD_REV = npdRev;
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
