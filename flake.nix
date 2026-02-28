{
  inputs = {
    nixpkgs-2511.url = "github:NixOS/nixpkgs/nixos-25.11";
    nixpkgs-unstable.url = "github:NixOS/nixpkgs/nixos-unstable";
    bun2nix = {
      url = "github:nix-community/bun2nix";
      inputs.nixpkgs.follows = "nixpkgs-2511";
    };
    serena = {
      url = "github:oraios/serena/main";
      inputs.nixpkgs.follows = "nixpkgs-unstable";
    };
  };

  outputs = inputs: {
    packages =
      inputs.nixpkgs-2511.lib.genAttrs [
        "x86_64-linux"
        "aarch64-linux"
      ] (system: let
        inherit (inputs.nixpkgs-2511.legacyPackages.${system}) callPackage;
      in {
        opencode-bwrap = callPackage ./opencode-bwrap {
          inherit (inputs) nixpkgs-unstable;
          bun2nix = inputs.bun2nix.packages.${system}.default;
          serena = inputs.serena.packages.${system}.default;
        };
      });
  };
}
