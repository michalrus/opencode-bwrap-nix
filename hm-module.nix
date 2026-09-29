{inputs}: {
  config,
  lib,
  pkgs,
  ...
}: let
  inherit (lib) mkEnableOption mkOption mkIf types literalExpression;

  cfg = config.programs.opencode-bwrap;
  notifCfg = cfg.notifications;
  inherit (pkgs.stdenv.hostPlatform) system;

  # -- Flake-provided dependencies -----------------------------------------

  bun2nix = inputs.bun2nix.packages.${system}.default;
  serena = inputs.serena.packages.${system}.default;

  playwright-mcp = pkgs.buildNpmPackage {
    pname = "playwright-mcp";
    inherit (builtins.fromJSON (builtins.readFile "${inputs.playwright-mcp}/package.json")) version;
    src = inputs.playwright-mcp;
    npmDepsHash = "sha256-9ezjwWu4tXgO868iDMT9Cst5Ke9ADISvbNbeEOJNmxw=";
    npmFlags = ["--ignore-scripts"];
    dontNpmBuild = true;
    PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD = "1";

    meta = {
      description = "Playwright MCP server for browser automation";
      homepage = "https://github.com/microsoft/playwright-mcp";
      license = lib.licenses.asl20;
      platforms = lib.platforms.linux;
      mainProgram = "playwright-mcp";
    };
  };

  plugins = pkgs.callPackage ./plugins {inherit inputs bun2nix;};

  # -- Playwright browser extensions ---------------------------------------

  playwrightCfg = cfg.playwright;

  ublockOrigin =
    if playwrightCfg.adblock.lite
    then pkgs.callPackage ./playwright-extensions/ublock-origin-lite.nix {}
    else pkgs.callPackage ./playwright-extensions/ublock-origin.nix {};
  twocaptchaSolver = pkgs.callPackage ./playwright-extensions/2captcha-solver.nix {
    apiKeyPlaceholder = "@${playwrightCfg.captchaSolver.apiKeyEnv}@";
  };

  playwrightExtensions =
    lib.optional playwrightCfg.adblock.enable ublockOrigin
    ++ lib.optional playwrightCfg.captchaSolver.enable twocaptchaSolver
    ++ playwrightCfg.extensions;

  playwrightAllowManifestV2 =
    playwrightCfg.allowManifestV2
    || (playwrightCfg.adblock.enable && !playwrightCfg.adblock.lite);

  playwrightExtensionEnvPlaceholders =
    lib.optional playwrightCfg.captchaSolver.enable playwrightCfg.captchaSolver.apiKeyEnv
    ++ playwrightCfg.extensionEnvPlaceholders;

  captchaSolverEnvFiles = lib.optionalAttrs (playwrightCfg.captchaSolver.enable && playwrightCfg.captchaSolver.apiKeyFile != null) {
    ${playwrightCfg.captchaSolver.apiKeyEnv} = playwrightCfg.captchaSolver.apiKeyFile;
  };

  escapeHatch = pkgs.callPackage ./bwrap-escape-hatch {};

  environmentPreambleScript = pkgs.callPackage ./preamble/environment.nix {};
  projectInstructionsPreambleScript = pkgs.callPackage ./preamble/project-instructions.nix {};
  preambleScriptType = types.coercedTo types.package lib.getExe types.path;

  # -- Notification sounds -------------------------------------------------

  # Default sound source repository (only fetched when sounds are needed).
  defaultSoundRepo = inputs.macos-system-sounds;

  # Convert any audio file to 44.1 kHz stereo WAV at build time.
  toWav = name: src:
    pkgs.runCommand "opencode-sound-${name}.wav" {
      nativeBuildInputs = [pkgs.ffmpeg-headless];
    } ''
      ffmpeg -y -i ${lib.escapeShellArg "${src}"} -ar 44100 -ac 2 "$out"
    '';

  enabledSounds = lib.filterAttrs (_: v: v != null) notifCfg.sounds;
  convertedSounds = lib.mapAttrs toWav enabledSounds;

  # Single directory holding all converted WAVs (for escape-hatch rules).
  soundsDir = pkgs.linkFarm "opencode-notifier-sounds" (
    lib.mapAttrsToList (name: wav: {
      name = "${name}.wav";
      path = wav;
    })
    convertedSounds
  );

  # -- Notifier config -----------------------------------------------------

  # Focus detection shells out to xdotool/hyprctl/swaymsg/gdbus and needs
  # DISPLAY or WAYLAND_DISPLAY, none of which reach into the sandbox. Left on,
  # it would hinge on whether the user happens to forward those via
  # `extraFwdEnv`, so pin it off and keep notifications unconditional.
  notifierConfigCommon = {
    showSessionTitle = true;
    suppressWhenFocused = false;
    inherit (notifCfg) messages;
  };

  notifierConfig =
    notifierConfigCommon
    // {
      sounds =
        if notifCfg.enable
        then lib.mapAttrs (name: _: "${soundsDir}/${name}.wav") convertedSounds
        # Plugin is still mounted; give it a valid but silent config.
        else {};
    };

  # -- Escape-hatch rules -------------------------------------------------

  notifRules =
    [
      {
        note = "basic notification";
        argv = [
          "${pkgs.libnotify}/bin/notify-send"
          "--app-name"
          "opencode"
          "--expire-time"
          "*"
          "--"
          "*"
          "*"
        ];
      }
      {
        note = "notify-send version check";
        argv = ["${pkgs.libnotify}/bin/notify-send" "--version"];
      }
      {
        note = "notification with icon";
        argv = [
          "${pkgs.libnotify}/bin/notify-send"
          "--app-name"
          "opencode"
          "--icon"
          "${plugins.opencode-notifier}/logos/*.png"
          "--expire-time"
          "*"
          "--"
          "*"
          "*"
        ];
      }
    ]
    ++ lib.optionals (convertedSounds != {}) [
      {
        note = "notification sounds";
        argv = ["${pkgs.alsa-utils}/bin/aplay" "${soundsDir}/*.wav"];
      }
    ]
    ++ notifCfg.extraRules;

  rulesFile =
    (pkgs.formats.json {}).generate "bwrap-escape-hatch-rules.json" notifRules;

  # -- Main package --------------------------------------------------------

  package = pkgs.callPackage ./opencode-bwrap {
    inherit bun2nix plugins notifierConfig;
    inherit (inputs) nixpkgs-opencode;
    serena =
      if cfg.serena.enable
      then serena
      else null;
    playwright-mcp =
      if cfg.playwright.enable
      then playwright-mcp
      else null;
    playwright = {
      inherit (playwrightCfg) userAgent extraArgs;
      extensions = playwrightExtensions;
      extensionEnvPlaceholders = playwrightExtensionEnvPlaceholders;
      allowManifestV2 = playwrightAllowManifestV2;
      captchaSolverEnabled = playwrightCfg.captchaSolver.enable;
    };
    image-generation-mcp =
      if cfg.imageGeneration.enable
      then pkgs.callPackage ./image-generation-mcp {}
      else null;
    inherit (cfg) imageGeneration;
    treefmtEnabled = cfg.treefmt.enable;
    bwrap-escape-hatch = escapeHatch;
    preamblePath = cfg.preamble;
    preambleScriptPaths = cfg.preambleScripts;
    bashrcSource = cfg.bashrc;
    zshrcSource = cfg.zshrc;
    compactionConfig =
      {inherit (cfg.compaction) auto prune;}
      // lib.optionalAttrs (cfg.compaction.reserved != null) {
        inherit (cfg.compaction) reserved;
      };
    providerJSON = cfg.provider;
    extraEnv =
      cfg.extraEnv
      // {OPENCODE_MAX_CONTEXT_TOKENS = toString cfg.maxContextTokens;}
      // lib.optionalAttrs (!cfg.pasteAttachments) {OPENCODE_DISABLE_PASTE_ATTACHMENTS = "true";}
      // lib.optionalAttrs (cfg.databaseName != null) {OPENCODE_DB = cfg.databaseName;};
    commandPaths = cfg.commands;
    extraEnvFiles = captchaSolverEnvFiles // cfg.extraEnvFiles;
    inherit (cfg) dataDirPrefix extraConfig extraTuiConfig extraPackages extraFwdEnv;
  };

  # -- Option helpers (DRY) ------------------------------------------------

  mkSoundOption = event: default:
    mkOption {
      type = types.nullOr types.path;
      inherit default;
      description = "Sound file for the '${event}' event (any format; converted to WAV at build time). null disables the sound.";
    };

  mkMessageOption = event: default:
    mkOption {
      type = types.str;
      inherit default;
      description = "Notification body for the '${event}' event. {sessionTitle} is replaced at runtime.";
    };

  rulesSubmodule = types.submodule {
    options = {
      note = mkOption {
        type = types.str;
        description = "Human-readable description of the rule.";
      };
      argv = mkOption {
        type = types.listOf types.str;
        description = "Positional fnmatch(3) patterns for the command's argv.";
      };
    };
  };
in {
  options.programs.opencode-bwrap = {
    enable = mkEnableOption "opencode-bwrap bubblewrap sandbox";

    package = mkOption {
      type = types.package;
      readOnly = true;
      default = package;
      description = "The final configured package that will be added to `home.packages`.";
    };

    preamble = mkOption {
      type = types.path;
      default = ./preamble/preamble.md;
      description = "Path to the preamble / instructions file mounted into the sandbox.";
    };

    preambleScripts = mkOption {
      type = types.listOf preambleScriptType;
      default = [
        environmentPreambleScript
        projectInstructionsPreambleScript
      ];
      example = literalExpression "[ pkgs.my-preamble ./another-preamble.sh ]";
      description = "Ordered list of executable packages or absolute executable paths whose stdout is appended to the preamble at runtime. An empty list disables the feature.";
    };

    dataDirPrefix = mkOption {
      type = types.str;
      default = ".local/share/opencode-bwrap";
      example = ".cache/opencode-bwrap";
      description = "Relative path under the host home directory where opencode-bwrap stores its persistent sandbox state.";
    };

    bashrc = mkOption {
      type = types.path;
      default = ./opencode-bwrap/bashrc;
      description = "Bash configuration sourced inside the sandbox.";
    };

    zshrc = mkOption {
      type = types.path;
      default = ./opencode-bwrap/zshrc;
      description = "Zsh configuration sourced inside the sandbox.";
    };

    commands = mkOption {
      type = types.attrsOf types.path;
      default = {};
      example = literalExpression ''
        {
          review = ./commands/review.md;
          "create-component" = ./commands/create-component.md;
        }
      '';
      description = "Global OpenCode command files mounted read-only in the sandbox. Attribute names become command names, and values are Markdown file paths.";
    };

    extraConfig = mkOption {
      type = types.submodule {
        freeformType = (pkgs.formats.json {}).type;
      };
      default = {};
      description = "Extra config.json.";
    };

    extraTuiConfig = mkOption {
      type = types.submodule {
        freeformType = (pkgs.formats.json {}).type;
      };
      default = {};
      description = "Extra tui.json.";
    };

    extraPackages = mkOption {
      type = types.listOf types.package;
      default = [];
      example = literalExpression "[ pkgs.ripgrep pkgs.fd ]";
      description = "Extra packages whose bin/ directories are prepended to the sandbox PATH.";
    };

    extraEnv = mkOption {
      type = types.attrsOf types.str;
      default = {};
      example = literalExpression ''{ MY_SETTING = "value"; }'';
      description = "Static environment variables (name-value pairs) to set in the sandbox.";
    };

    extraFwdEnv = mkOption {
      type = types.listOf types.str;
      default = [];
      example = ["ANTHROPIC_API_KEY" "GITHUB_TOKEN"];
      description = "Host environment variable names to forward into the sandbox (only set when non-empty on the host).";
    };

    extraEnvFiles = mkOption {
      type = types.attrsOf types.str;
      default = {};
      example = {ANTHROPIC_API_KEY = ".config/opencode/anthropic-api.key";};
      description = "Environment variables read from non-empty files in the persistent sandbox home. Values are paths relative to that home.";
    };

    maxContextTokens = mkOption {
      type = types.ints.positive;
      default = 224 * 1024;
      example = 200000;
      description = "Maximum context and input token count that OpenCode uses for any model. Smaller model limits stay unchanged.";
    };

    pasteAttachments = mkOption {
      type = types.bool;
      default = false;
      description = ''
        Whether the TUI turns a pasted path to an existing image, SVG, or PDF
        file into an inline attachment (shown as `[Image 1]`, `[SVG: name]`,
        or `[PDF 1]`). When false, such paths stay plain text; use `@path` to
        attach a file explicitly.
      '';
    };

    databaseName = mkOption {
      type = types.nullOr types.str;
      default = "opencode.db";
      example = "opencode-stable.db";
      description = ''
        Name of the SQLite database file holding session history, relative to
        OpenCode's data directory inside the sandbox.

        OpenCode otherwise derives this name from the channel it was built
        with, which made the file move to `opencode-stable.db` while nixpkgs
        built against a `stable` channel that upstream never had. Pinning the
        name keeps session history reachable across such changes.

        Set to null to let OpenCode choose the name itself.
      '';
    };

    serena = {
      enable =
        mkEnableOption "Serena LSP/MCP integration (provides semantic code-navigation tools)"
        // {default = true;};
    };

    playwright = {
      enable =
        mkEnableOption "Playwright MCP with headless Nixpkgs Chromium in a throwaway per-session profile"
        // {default = true;};

      adblock = {
        enable = mkEnableOption "uBlock Origin in the Playwright browser";

        lite = mkOption {
          type = types.bool;
          default = false;
          description = ''
            Load uBlock Origin Lite (manifest v3) instead of the full uBlock
            Origin (manifest v2). The full version blocks more, but depends on
            `allowManifestV2`, which it switches on by itself.
          '';
        };
      };

      allowManifestV2 = mkOption {
        type = types.bool;
        default = false;
        description = ''
          Start Chromium with `--enable-features=AllowLegacyMV2Extensions` so
          that unpacked manifest v2 extensions still load after the MV2
          deprecation. Only affects `--load-extension` (unpacked) extensions.
          Undocumented developer switch: Chromium may drop it in any release,
          in which case the affected extensions silently stop loading.
        '';
      };

      captchaSolver = {
        enable = mkEnableOption ''
          the 2Captcha solver extension in the Playwright browser. It solves
          image captchas, reCAPTCHA v2, GeeTest, Arkose Labs, Cloudflare
          Turnstile, Amazon WAF, and others on its own as they appear, billed
          to the 2Captcha account behind the API key (hCaptcha is not
          supported by 2Captcha). The agent's instructions gain a section on
          waiting for the solver
        '';

        apiKeyFile = mkOption {
          type = types.nullOr types.str;
          default = ".config/opencode/2captcha.key";
          example = ".secrets/2captcha";
          description = ''
            File holding the 2Captcha API key, relative to the persistent
            sandbox home, read when the sandbox starts (like `extraEnvFiles`).
            Set to null to supply `apiKeyEnv` yourself through `extraEnvFiles`
            or `extraFwdEnv`.
          '';
        };

        apiKeyEnv = mkOption {
          type = types.str;
          default = "TWOCAPTCHA_API_KEY";
          description = "Name of the environment variable that carries the 2Captcha API key inside the sandbox.";
        };
      };

      extensions = mkOption {
        type = types.listOf types.package;
        default = [];
        example = literalExpression "[ pkgs.my-unpacked-extension ]";
        description = ''
          Additional unpacked Chromium extensions (directories containing
          `manifest.json`) loaded into every browser session. Each is copied
          to a private writable directory before Chromium starts.
        '';
      };

      extensionEnvPlaceholders = mkOption {
        type = types.listOf types.str;
        default = [];
        example = ["MY_EXTENSION_TOKEN"];
        description = ''
          Environment variable names whose values replace the literal
          `@NAME@` tokens inside the staged copies of `extensions` at startup.
          Lets an extension carry a secret (supplied through `extraEnvFiles`
          or `extraFwdEnv`) without the secret entering the Nix store.
        '';
      };

      userAgent = mkOption {
        type = types.nullOr types.str;
        default = null;
        example = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36";
        description = ''
          Browser user agent. The default mirrors headed Chromium of the same
          major version, instead of the `HeadlessChrome/…` token that many
          sites reject outright.
        '';
      };

      extraArgs = mkOption {
        type = types.listOf types.str;
        default = [];
        example = ["--lang=pl-PL"];
        description = "Additional Chromium command-line switches.";
      };
    };

    imageGeneration = {
      enable = mkEnableOption "image-generation MCP through CLIProxyAPI";
      baseUrl = mkOption {
        type = types.str;
        default = "http://127.0.0.1:8317/v1";
        example = "https://llm-proxy.example.com/v1";
        description = "CLIProxyAPI base URL, with or without /v1. Use HTTPS except for a loopback proxy.";
      };
      apiKeyEnv = mkOption {
        type = types.str;
        default = "LLM_PROXY_KEY";
        description = "Name of the API key environment variable in the sandbox. Supply its value through extraEnvFiles or extraFwdEnv, not Nix.";
      };
    };

    treefmt = {
      enable =
        mkEnableOption "treefmt as the exclusive formatter (disables all built-in formatters)"
        // {default = true;};
    };

    compaction = {
      auto =
        mkEnableOption "automatic compaction when context is full"
        // {default = true;};
      prune =
        mkEnableOption "pruning of old tool outputs"
        // {default = true;};
      reserved = mkOption {
        type = types.nullOr types.ints.unsigned;
        default = null;
        example = 16384;
        description = "Token buffer for compaction. Leaves enough headroom to avoid overflow during the compaction pass itself.";
      };
    };

    provider = mkOption {
      type = types.attrsOf types.anything;
      default = {};
      example = literalExpression ''
        {
          openai.models."gpt-5.4".limit = { context = 100 * 1000; output = 32768; };
          anthropic.models."claude-opus-4-6".limit = { context = 100 * 1000; output = 32768; };
        }
      '';
      description = ''
        Per-provider configuration and model overrides, passed verbatim as the
        top-level "provider" key in `opencode.json`. See
        <https://opencode.ai/config.json> for the full `ProviderConfig` schema.
      '';
    };

    notifications = {
      enable =
        mkEnableOption "desktop notifications and sounds via the escape-hatch service"
        // {
          default = true;
        };

      sounds = {
        permission = mkSoundOption "permission" "${defaultSoundRepo}/m4r/Illuminate.m4r";
        complete = mkSoundOption "complete" "${defaultSoundRepo}/m4r/Chord.m4r";
        subagent_complete = mkSoundOption "subagent_complete" "${defaultSoundRepo}/aiff/Pop.aiff";
        error = mkSoundOption "error" "${defaultSoundRepo}/m4r/Hillside.m4r";
        question = mkSoundOption "question" "${defaultSoundRepo}/m4r/Illuminate.m4r";
        user_cancelled = mkSoundOption "user_cancelled" "${defaultSoundRepo}/aiff/Frog.aiff";
      };

      messages = {
        permission = mkMessageOption "permission" "{sessionTitle}\n→ needs permission";
        complete = mkMessageOption "complete" "{sessionTitle}\n→ session finished";
        subagent_complete = mkMessageOption "subagent_complete" "{sessionTitle}\n→ subagent completed";
        error = mkMessageOption "error" "{sessionTitle}\n→ error";
        question = mkMessageOption "question" "{sessionTitle}\n→ question(s)";
        user_cancelled = mkMessageOption "user_cancelled" "{sessionTitle}\n→ cancelled by user";
      };

      extraRules = mkOption {
        type = types.listOf rulesSubmodule;
        default = [];
        description = "Additional escape-hatch allow-list rules appended after the built-in notification and sound rules.";
      };
    };
  };

  config = mkIf cfg.enable {
    assertions = [
      {
        assertion = !cfg.imageGeneration.enable || builtins.match "[a-zA-Z_][a-zA-Z_0-9]*" cfg.imageGeneration.apiKeyEnv != null;
        message = "programs.opencode-bwrap.imageGeneration.apiKeyEnv must be a valid environment variable name";
      }
      {
        assertion = !cfg.imageGeneration.enable || builtins.match "https?://[^[:space:]?#@]+" cfg.imageGeneration.baseUrl != null;
        message = "programs.opencode-bwrap.imageGeneration.baseUrl must be an HTTP(S) URL without credentials, a query, or a fragment";
      }
      {
        assertion = lib.all (name: builtins.match "[a-zA-Z_][a-zA-Z_0-9]*" name != null) (builtins.attrNames cfg.extraEnv);
        message = "programs.opencode-bwrap.extraEnv: every key must be a valid POSIX variable name ([a-zA-Z_][a-zA-Z_0-9]*)";
      }
      {
        assertion = lib.all (segment: segment != "" && segment != "." && segment != "..") (lib.splitString "/" cfg.dataDirPrefix);
        message = "programs.opencode-bwrap.dataDirPrefix: must be a normalized relative path under $HOME (no empty, '.' or '..' segments)";
      }
      {
        assertion = lib.all (name: builtins.match "[a-zA-Z_][a-zA-Z_0-9]*" name != null) cfg.extraFwdEnv;
        message = "programs.opencode-bwrap.extraFwdEnv: every entry must be a valid POSIX variable name ([a-zA-Z_][a-zA-Z_0-9]*)";
      }
      {
        assertion = lib.all (name: builtins.match "[a-zA-Z_][a-zA-Z_0-9]*" name != null) (builtins.attrNames cfg.extraEnvFiles);
        message = "programs.opencode-bwrap.extraEnvFiles: every key must be a valid POSIX variable name ([a-zA-Z_][a-zA-Z_0-9]*)";
      }
      {
        assertion = lib.all (path: lib.all (segment: segment != "" && segment != "." && segment != "..") (lib.splitString "/" path)) (builtins.attrValues cfg.extraEnvFiles);
        message = "programs.opencode-bwrap.extraEnvFiles: every value must be a normalized relative path under the persistent sandbox home (no empty, '.' or '..' segments)";
      }
      {
        assertion = lib.all (name: builtins.match "[a-zA-Z0-9][a-zA-Z0-9_-]*" name != null) (builtins.attrNames cfg.commands);
        message = "programs.opencode-bwrap.commands: every command name must match [a-zA-Z0-9][a-zA-Z0-9_-]*";
      }
      {
        assertion = cfg.databaseName == null || builtins.match "[a-zA-Z0-9][a-zA-Z0-9._-]*" cfg.databaseName != null;
        message = "programs.opencode-bwrap.databaseName: must be a bare file name under OpenCode's data directory (no path separators)";
      }
      {
        assertion = lib.all (name: builtins.match "[a-zA-Z_][a-zA-Z_0-9]*" name != null) cfg.playwright.extensionEnvPlaceholders;
        message = "programs.opencode-bwrap.playwright.extensionEnvPlaceholders: every entry must be a valid POSIX variable name ([a-zA-Z_][a-zA-Z_0-9]*)";
      }
      {
        assertion = builtins.match "[a-zA-Z_][a-zA-Z_0-9]*" cfg.playwright.captchaSolver.apiKeyEnv != null;
        message = "programs.opencode-bwrap.playwright.captchaSolver.apiKeyEnv must be a valid environment variable name";
      }
      {
        assertion = let
          path = cfg.playwright.captchaSolver.apiKeyFile;
        in
          path == null || lib.all (segment: segment != "" && segment != "." && segment != "..") (lib.splitString "/" path);
        message = "programs.opencode-bwrap.playwright.captchaSolver.apiKeyFile: must be a normalized relative path under the persistent sandbox home (no empty, '.' or '..' segments)";
      }
      {
        assertion = !(cfg.playwright.adblock.enable || cfg.playwright.captchaSolver.enable || cfg.playwright.extensions != [] || cfg.playwright.allowManifestV2) || cfg.playwright.enable;
        message = "programs.opencode-bwrap.playwright: adblock, captchaSolver, extensions, and allowManifestV2 require playwright.enable";
      }
      {
        assertion = lib.all (arg: lib.hasPrefix "--" arg && !lib.hasPrefix "--user-data-dir" arg && !lib.hasPrefix "--load-extension" arg && !lib.hasPrefix "--user-agent" arg) cfg.playwright.extraArgs;
        message = "programs.opencode-bwrap.playwright.extraArgs: entries must be `--switches`; use the dedicated options for the profile directory, extensions, and user agent";
      }
      {
        assertion = !playwrightAllowManifestV2 || lib.all (arg: !lib.hasPrefix "--enable-features" arg) cfg.playwright.extraArgs;
        message = "programs.opencode-bwrap.playwright.extraArgs: Chromium keeps only the last `--enable-features` switch, which would drop `AllowLegacyMV2Extensions`; leave it out when manifest v2 extensions are enabled";
      }
    ];

    home.packages = [package];

    # Escape-hatch systemd units (socket-activated, one-shot handler).
    systemd.user = mkIf notifCfg.enable {
      sockets.bwrap-escape-hatch = {
        Unit.Description = "bwrap-escape-hatch sandbox escape socket";
        Socket = {
          ListenStream = "%t/bwrap-escape-hatch.sock";
          Accept = true;
          SocketMode = "0600";
        };
        Install.WantedBy = ["sockets.target"];
      };

      services."bwrap-escape-hatch@" = {
        Unit.Description = "bwrap-escape-hatch request handler";
        Service = {
          Type = "oneshot";
          StandardInput = "socket";
          StandardOutput = "socket";
          StandardError = "journal";
          ExecStart = "${lib.getExe escapeHatch.package} --rules ${rulesFile}";
          TimeoutStartSec = 10;
          MemoryMax = "64M";
        };
      };
    };
  };
}
