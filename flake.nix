{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
    nixpkgs-opencode.url = "github:NixOS/nixpkgs/99b76fd9b396189197d2ecce519ab6d7cd522ab5"; # opencode 1.18.31
    bun2nix = {
      url = "github:nix-community/bun2nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    serena = {
      url = "github:oraios/serena/main";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    playwright-mcp = {
      url = "github:microsoft/playwright-mcp/v0.0.82";
      flake = false;
    };
    opencode-anthropic-auth = {
      url = "github:ex-machina-co/opencode-anthropic-auth/v1.8.5";
      flake = false;
    };
    opencode-notifier = {
      url = "github:mohak34/opencode-notifier/v0.3.0";
      flake = false;
    };
    macos-system-sounds = {
      url = "github:extratone/macOSsystemsounds";
      flake = false;
    };
    ublock-origin = {
      url = "https://github.com/gorhill/uBlock/releases/download/1.75.0/uBlock0_1.75.0.chromium.zip";
      flake = false;
    };
    ublock-origin-lite = {
      url = "https://github.com/uBlockOrigin/uBOL-home/releases/download/2026.926.2202/uBOLite_2026.926.2202.chromium.zip";
      flake = false;
    };
    twocaptcha-solver = {
      # GitHub releases stop at 3.7.2; newer versions are only in the Chrome Web
      # Store. This is where the update service redirects for id
      # ifibfemgeogfhoebkmokieepdoobkbpo, version 3.7.4.
      #url = "https://github.com/rucaptcha/2captcha-solver/releases/download/v3.7.2/2captcha-solver-chrome-3.7.2.zip";
      url = "file+https://clients2.googleusercontent.com/crx/blobs/AZPVhcQggW9qkp9JsJTfK1mZyPEb8Wdc3YSdckG3lVJK3uhX3HFw6cZ5VDv9cy2mFd4ZUVAI8anfVcm_rkizu0WUZjGkKKfEeRrlSmbiDiQrJc7I9FJKZZvPz5Ut47okAMwAxlKa5fDvdbhzybg948P2NDdAZRNJ2D4h/IFIBFEMGEOGFHOEBKMOKIEEPDOOBKBPO_3_7_4_0.crx";
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
        image-generation-mcp = pkgs.callPackage ./image-generation-mcp {};
        preamble-environment = pkgs.callPackage ./preamble/environment.nix {};
        preamble-project-instructions = pkgs.callPackage ./preamble/project-instructions.nix {};
      });
  };
}
