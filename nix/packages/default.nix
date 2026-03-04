{ self, nixpkgs, rust-overlay, crane, flake-utils, ... }:
flake-utils.lib.eachDefaultSystem (system:
  let
    overlays = [ (import rust-overlay) ];
    pkgs = import nixpkgs {
      inherit system overlays;
    };
    craneLib = (crane.mkLib pkgs).overrideToolchain (p: p.rust-bin.stable.latest.default);
  in
    {
      packages.canbridge = craneLib.buildPackage {
        src = craneLib.cleanCargoSource self;
      };
    }
)
