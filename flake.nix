{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
    # We take opencode 1.16.2 from:
    nixpkgs-opencode.url = "github:NixOS/nixpkgs/f37978477bb931ceac4e730d201fe2a089c8c9f6";
    bun2nix = {
      url = "github:nix-community/bun2nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    serena = {
      url = "github:oraios/serena/main";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    opencode-anthropic-auth = {
      url = "github:ex-machina-co/opencode-anthropic-auth/v1.8.1";
      flake = false;
    };
    opencode-notifier = {
      url = "github:mohak34/opencode-notifier/v0.1.28";
      flake = false;
    };
    macos-system-sounds = {
      url = "github:extratone/macOSsystemsounds/f3e8dcd8d2318d099ade479ad1b9778ce4e65cc7";
      flake = false;
    };
  };

  outputs = inputs: let
    inherit (inputs.nixpkgs) lib;

    # Minimal stubs for the home-manager options our module sets.
    # This lets us evaluate the module with lib.evalModules so that
    # `nix build` exercises the same code path as a real HM configuration.
    hmOptionStubs = {
      options = {
        home.packages = lib.mkOption {
          type = lib.types.listOf lib.types.package;
          default = [];
        };
        systemd.user = lib.mkOption {
          type = lib.types.anything;
          default = {};
        };
        assertions = lib.mkOption {
          type = lib.types.listOf lib.types.anything;
          default = [];
        };
      };
    };
  in {
    homeManagerModules.default = import ./hm-module.nix {inherit inputs;};

    packages =
      lib.genAttrs [
        "x86_64-linux"
        "aarch64-linux"
      ] (system: let
        pkgs = inputs.nixpkgs.legacyPackages.${system};

        hmEval = lib.evalModules {
          specialArgs = {inherit pkgs;};
          modules = [
            (import ./hm-module.nix {inherit inputs;})
            hmOptionStubs
            {
              programs.opencode-bwrap = {
                enable = true;
              };
            }
          ];
        };
      in rec {
        default = opencode-bwrap;
        opencode-bwrap = builtins.head hmEval.config.home.packages;
        bwrap-escape-hatch = (pkgs.callPackage ./bwrap-escape-hatch {}).package;
        preamble-environment = pkgs.callPackage ./preamble/environment.nix {};
        preamble-project-instructions = pkgs.callPackage ./preamble/project-instructions.nix {};
      });
  };
}
