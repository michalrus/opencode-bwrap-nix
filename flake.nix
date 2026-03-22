{
  inputs = {
    nixpkgs-2511.url = "github:NixOS/nixpkgs/nixos-25.11";
    nixpkgs-unstable.url = "github:NixOS/nixpkgs/nixos-unstable";
    home-manager-2511.url = "github:nix-community/home-manager/release-25.11";
    home-manager-2511.inputs.nixpkgs.follows = "nixpkgs-2511";
  };

  outputs = inputs: {
    packages =
      inputs.nixpkgs-2511.lib.genAttrs [
        "x86_64-linux"
        "aarch64-linux"
      ] (system: let
        inherit (inputs.nixpkgs-2511.legacyPackages.${system}) callPackage;
      in {
        opencode-vm = callPackage ./opencode-vm {
          nixpkgs = inputs.nixpkgs-2511;
          inherit (inputs) nixpkgs-unstable;
          home-manager = inputs.home-manager-2511;
        };
        opencode-bwrap = callPackage ./opencode-bwrap {
          inherit (inputs) nixpkgs-unstable;
        };
      });
  };
}
