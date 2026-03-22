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

  plugins = pkgs.callPackage ./plugins {inherit bun2nix;};

  escapeHatch = pkgs.callPackage ./bwrap-escape-hatch {};

  # -- Notification sounds -------------------------------------------------

  # Default sound source repository (only fetched when sounds are needed).
  defaultSoundRepo = pkgs.fetchFromGitHub {
    owner = "extratone";
    repo = "macOSsystemsounds";
    rev = "f3e8dcd8d2318d099ade479ad1b9778ce4e65cc7";
    hash = "sha256-7Qa/MpYykTIOWkAhhoV1rrhScCkQKgcQAmmR38PdNRc=";
  };

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

  notifierConfig =
    if notifCfg.enable
    then {
      showSessionTitle = true;
      inherit (notifCfg) messages;
      sounds = lib.mapAttrs (name: _: "${soundsDir}/${name}.wav") convertedSounds;
    }
    else {
      # Plugin is still mounted; give it a valid but silent config.
      showSessionTitle = true;
      inherit (notifCfg) messages;
      sounds = {};
    };

  # -- Escape-hatch rules -------------------------------------------------

  notifRules =
    [
      {
        note = "basic notification";
        argv = ["${pkgs.libnotify}/bin/notify-send" "--" "*" "*"];
      }
      {
        note = "notify-send version check";
        argv = ["${pkgs.libnotify}/bin/notify-send" "--version"];
      }
      {
        note = "notification with icon";
        argv = [
          "${pkgs.libnotify}/bin/notify-send"
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
    serena =
      if cfg.serena.enable
      then serena
      else null;
    bwrap-escape-hatch = escapeHatch;
    preamblePath = cfg.preamble;
    bashrcSource = cfg.bashrc;
    zshrcSource = cfg.zshrc;
    inherit (cfg) extraPackages extraEnv extraFwdEnv;
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

    preamble = mkOption {
      type = types.path;
      default = ./opencode-bwrap/preamble.md;
      description = "Path to the preamble / instructions file mounted into the sandbox.";
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

    serena = {
      enable =
        mkEnableOption "Serena LSP/MCP integration (provides semantic code-navigation tools)"
        // {default = true;};
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
        assertion = lib.all (name: builtins.match "[a-zA-Z_][a-zA-Z_0-9]*" name != null) (builtins.attrNames cfg.extraEnv);
        message = "programs.opencode-bwrap.extraEnv: every key must be a valid POSIX variable name ([a-zA-Z_][a-zA-Z_0-9]*)";
      }
      {
        assertion = lib.all (name: builtins.match "[a-zA-Z_][a-zA-Z_0-9]*" name != null) cfg.extraFwdEnv;
        message = "programs.opencode-bwrap.extraFwdEnv: every entry must be a valid POSIX variable name ([a-zA-Z_][a-zA-Z_0-9]*)";
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
