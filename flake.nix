{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
    bun2nix = {
      url = "github:nix-community/bun2nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    serena = {
      url = "github:oraios/serena/main";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = inputs: {
    homeManagerModules.default = import ./hm-module.nix {inherit inputs;};

    packages =
      inputs.nixpkgs.lib.genAttrs [
        "x86_64-linux"
        "aarch64-linux"
      ] (system: let
        inherit (inputs.nixpkgs.legacyPackages.${system}) callPackage;
        bun2nix = inputs.bun2nix.packages.${system}.default;
        serena = inputs.serena.packages.${system}.default;
        plugins = callPackage ./plugins {inherit bun2nix;};
        escapeHatch = callPackage ./bwrap-escape-hatch {};
      in rec {
        default = opencode-bwrap;
        opencode-bwrap = callPackage ./opencode-bwrap {
          inherit bun2nix serena plugins;
          bwrap-escape-hatch = escapeHatch;
        };
        bwrap-escape-hatch = escapeHatch.package;
      });
  };
}
