{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    flake-utils.url = "github:numtide/flake-utils";
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
      rust-overlay,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };
      in
      {
        # Crane-based package build (à la npc) can be added once the tool does
        # something; for now the dev shell is enough to build and run.
        devShells.default = pkgs.mkShell {
          buildInputs = [
            pkgs.rust-bin.stable.latest.default
            # npd shells out to these:
            pkgs.nix
            pkgs.nix-eval-jobs
            # for poking at the store during development:
            pkgs.sqlite
          ];
        };
      }
    );
}
