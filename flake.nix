{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
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
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };
        craneLib = crane.mkLib pkgs;
        # npd shells out to these at runtime; wrap them onto its PATH so the
        # packaged binary works outside the dev shell (`nix shell .`).
        runtimeDeps = [
          pkgs.nix
          pkgs.nix-eval-jobs
          pkgs.nix-output-monitor # `nom`, the build front-end
          pkgs.git
          pkgs.nettools # `hostname`
        ];
        commonArgs = {
          src = craneLib.cleanCargoSource ./.;
          strictDeps = true;
          # ring (via ureq's TLS) needs perl at build; rusqlite bundles sqlite (cc).
          nativeBuildInputs = [ pkgs.perl ];
        };
        # Built once and shared by the package and every check.
        cargoArtifacts = craneLib.buildDepsOnly commonArgs;
        npd = craneLib.buildPackage (
          commonArgs
          // {
            inherit cargoArtifacts;
            nativeBuildInputs = commonArgs.nativeBuildInputs ++ [ pkgs.makeWrapper ];
            postInstall = ''
              wrapProgram $out/bin/npd --prefix PATH : ${pkgs.lib.makeBinPath runtimeDeps}
            '';
          }
        );
      in
      {
        packages.default = npd;
        # `nix flake check` runs these in the sandbox. Only the default (non-
        # `#[ignore]`d) tests run here — the sole ignored test builds real
        # derivations and needs the daemon, which the sandbox rightly denies.
        checks = {
          inherit npd;
          npd-test = craneLib.cargoTest (commonArgs // { inherit cargoArtifacts; });
          npd-clippy = craneLib.cargoClippy (
            commonArgs
            // {
              inherit cargoArtifacts;
              cargoClippyExtraArgs = "--all-targets -- --deny warnings";
            }
          );
          npd-fmt = craneLib.cargoFmt { inherit (commonArgs) src; };
        };
        devShells.default = pkgs.mkShell {
          buildInputs = [
            pkgs.rust-bin.stable.latest.default
            pkgs.nix
            pkgs.nix-eval-jobs
            pkgs.nix-output-monitor
            pkgs.sqlite # for poking at the store during development
          ];
        };
      }
    );
}
