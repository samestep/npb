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
          pkgs.git
          pkgs.nettools # `hostname`
        ];
        commonArgs = {
          src = craneLib.cleanCargoSource ./.;
          strictDeps = true;
          # ring (via ureq's TLS) needs perl at build; rusqlite bundles sqlite (cc).
          nativeBuildInputs = [ pkgs.perl ];
        };
        npd = craneLib.buildPackage (
          commonArgs
          // {
            cargoArtifacts = craneLib.buildDepsOnly commonArgs;
            nativeBuildInputs = commonArgs.nativeBuildInputs ++ [ pkgs.makeWrapper ];
            postInstall = ''
              wrapProgram $out/bin/npd --prefix PATH : ${pkgs.lib.makeBinPath runtimeDeps}
            '';
          }
        );
      in
      {
        packages.default = npd;
        devShells.default = pkgs.mkShell {
          buildInputs = [
            pkgs.rust-bin.stable.latest.default
            pkgs.nix
            pkgs.nix-eval-jobs
            pkgs.sqlite # for poking at the store during development
          ];
        };
      }
    );
}
