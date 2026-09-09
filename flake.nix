{
  description = "netui, a terminal network manager";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    crane.url = "github:ipetkov/crane";
    flake-compat = {
      url = "github:edolstra/flake-compat/b4a34015c698c7793d592d66adbab377907a2be8";
      flake = false;
    };
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, crane, rust-overlay, ... }:
    let
      nixosModule =
        { config, lib, pkgs, ... }:
        let
          cfg = config.programs.netui;
        in
        {
          options.programs.netui = {
            enable = lib.mkEnableOption "netui terminal network manager";

            package = lib.mkOption {
              type = lib.types.package;
              default = self.packages.${pkgs.stdenv.hostPlatform.system}.netui;
              defaultText = lib.literalExpression "inputs.netui.packages.\${pkgs.stdenv.hostPlatform.system}.netui";
              description = "The netui package to install.";
            };
          };

          config = lib.mkIf cfg.enable {
            environment.systemPackages = [ cfg.package ];
          };
        };
    in
    (flake-utils.lib.eachDefaultSystem
      (system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ rust-overlay.overlays.default ];
          };
          rustToolchain = pkgs.rust-bin.stable.latest.default;
          craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;
          commonArgs = {
            pname = "netui";
            version = "0.1.0";
            src = craneLib.cleanCargoSource ./.;
            strictDeps = true;
            nativeBuildInputs = [ pkgs.pkg-config ];
            buildInputs = [ pkgs.dbus ];
          };
          cargoArtifacts = craneLib.buildDepsOnly commonArgs;
          netui = craneLib.buildPackage (commonArgs // {
            inherit cargoArtifacts;
          });
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
              pkgs.gcc
              pkgs.pkg-config
              pkgs.dbus
            ];
          };
        })
    // {
      nixosModules = {
        default = nixosModule;
        netui = nixosModule;
      };
    });
}
