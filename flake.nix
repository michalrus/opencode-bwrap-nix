{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
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
    # Only the version is taken from this input (see `camoufox/default.nix`).
    # The release zips are about 1.3 GB per system, and as flake inputs
    # `nix flake lock` would download all of them.
    camoufox = {
      url = "file+https://raw.githubusercontent.com/daijro/camoufox/refs/tags/v156.0.1-beta.33/README.md";
      flake = false;
    };
    # Firefox add-ons (`.xpi`) are plain zip files; `tarball+` unpacks them.
    ublock-origin = {
      url = "tarball+https://github.com/gorhill/uBlock/releases/download/1.75.0/uBlock0_1.75.0.firefox.signed.xpi";
      flake = false;
    };
    twocaptcha-solver = {
      # The GitHub releases have Chromium builds only. This is the newest
      # Firefox build on addons.mozilla.org (slug `2captcha-solver`).
      url = "tarball+https://addons.mozilla.org/firefox/downloads/file/4353807/2captcha_solver-3.7.1.xpi";
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
        camoufox = pkgs.callPackage ./camoufox {};
        image-generation-mcp = pkgs.callPackage ./image-generation-mcp {};
        mcp-session-mux = pkgs.callPackage ./mcp-session-mux {};
        preamble-environment = pkgs.callPackage ./preamble/environment.nix {};
        preamble-project-instructions = pkgs.callPackage ./preamble/project-instructions.nix {};
      });
  };
}
