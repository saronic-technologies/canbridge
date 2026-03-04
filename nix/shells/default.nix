{ self, nixpkgs, rust-overlay, flake-utils, ... }:
flake-utils.lib.eachDefaultSystem (system:
  let
    overlays = [ (import rust-overlay) ];
    pkgs = import nixpkgs {
      inherit system overlays;
    };
  in
    with pkgs;
    {
      devShells.default = mkShell {
        nativeBuildInputs = [
          rust-bin.nightly.latest.default
        ];
      };
    }
)
