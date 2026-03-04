{
  description = "A devShell example";

  inputs = {
    crane.url = "github:ipetkov/crane";
    flake-utils.url  = "github:numtide/flake-utils";
    nixpkgs.url      = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
  };

  outputs = inputs@{ flake-utils, ... }:
    flake-utils.lib.meld inputs [
      ./nix/shells
      ./nix/modules
      ./nix/overlays
      ./nix/packages
      ./nix/tests
    ];
}
