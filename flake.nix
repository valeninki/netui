{
  description = "netui, a terminal network manager";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    crane.url = "github:ipetkov/crane";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { nixpkgs, flake-utils, crane, rust-overlay, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };
        rustToolchain = pkgs.rust-bin.stable.latest.default;
        craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;
        netui = craneLib.buildPackage {
          pname = "netui";
          version = "0.1.0";
          src = craneLib.cleanCargoSource ./.;
          nativeBuildInputs = [ pkgs.pkg-config ];
          buildInputs = [ pkgs.dbus ];
        };
      in
      {
        packages = {
          default = netui;
          netui = netui;
        };

        devShells.default = craneLib.devShell {
          inputsFrom = [ netui ];
          packages = [
            rustToolchain
            pkgs.rust-analyzer
            pkgs.pkg-config
            pkgs.dbus
          ];
        };
      });
}
