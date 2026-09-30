{
  pkgs,
  nixpkgs-opencode,
  lib,
  bun2nix,
  serena ? null,
  playwright-mcp ? null,
  mcp-session-mux ? null,
  playwright ? {},
  image-generation-mcp ? null,
  imageGeneration ? {},
  plugins,
  bwrap-escape-hatch,
  # Overridable by the home-manager module:
  preamblePath ? ./preamble.md,
  preambleScriptPaths ? [],
  dataDirPrefix ? ".local/share/opencode-bwrap",
  bashrcSource ? ./bashrc,
  zshrcSource ? ./zshrc,
  extraConfig ? {},
  extraTuiConfig ? {},
  extraPackages ? [],
  extraEnv ? {},
  extraEnvFiles ? {},
  extraFwdEnv ? [],
  commandPaths ? {},
  notifierConfig ? {},
  treefmtEnabled ? true,
  compactionConfig ? {
    auto = true;
    prune = true;
  },
  providerJSON ? {},
}:
assert lib.assertMsg (playwright-mcp == null || mcp-session-mux != null) "opencode-bwrap: `playwright-mcp` needs `mcp-session-mux` to run one browser per opencode session"; let
  unsafe = nixpkgs-opencode.legacyPackages.${pkgs.stdenv.hostPlatform.system}.opencode.overrideAttrs (prev: {
    patches =
      (prev.patches or [])
      ++ [
        ./opencode--instructions_command.patch
        ./opencode--max-context-tokens.patch
        ./opencode--disable-paste-attachments.patch
        ./opencode--mcp-session-meta.patch
      ];
  });

  escapeHatchShims = bwrap-escape-hatch.mkGuestWrappers [
    {
      name = "notify-send";
      hostBin = "${pkgs.libnotify}/bin/notify-send";
    }
    {
      name = "aplay";
      hostBin = "${pkgs.alsa-utils}/bin/aplay";
    }
  ];

  configFormat = pkgs.formats.json {};

  # opencode starts a single Playwright MCP process and shares it between the
  # main session and every subagent, which would make them all fight over one
  # browser and its tabs. Our opencode patch stamps each `tools/call` with the
  # session ID, and `mcp-session-mux` routes it to a `playwright-mcp-ephemeral`
  # child started for that session: a private Chromium with a throwaway on-disk
  # profile, removed when the child exits. On-disk (not `--isolated` in-memory)
  # profiles are what lets Chromium load unpacked extensions. A session's child
  # is stopped after `sessionIdleTimeout` seconds without tool calls; the next
  # call starts a fresh one with no tabs.
  playwrightFontsConf = pkgs.makeFontsConf {
    fontDirectories = with pkgs; [
      dejavu_fonts
      freefont_ttf
      gyre-fonts
      liberation_ttf
      unifont
      noto-fonts-color-emoji
    ];
    impureFontDirectories = [];
    includes = ["${pkgs.fontconfig.out}/etc/fonts/conf.d"];
  };

  playwrightUserAgent =
    if playwright.userAgent or null != null
    then playwright.userAgent
    else "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/${lib.versions.major pkgs.chromium.version}.0.0.0 Safari/537.36";

  playwrightExtensions = playwright.extensions or [];
  playwrightExtensionEnvPlaceholders = playwright.extensionEnvPlaceholders or [];

  # Chromium honours only the last `--enable-features` switch, and Playwright
  # already passes one of its own (`CDPScreenshotNewSurface`, see
  # `chromiumSwitches.ts`), so it has to be repeated here alongside ours.
  # `AllowLegacyMV2Extensions` is the developer escape hatch that lets unpacked
  # manifest v2 extensions load after the MV2 deprecation.
  playwrightEnabledFeatures =
    ["CDPScreenshotNewSurface"]
    ++ lib.optional (playwright.allowManifestV2 or false) "AllowLegacyMV2Extensions";

  playwrightMcpConfig = configFormat.generate "playwright-mcp-config.json" {
    browser = {
      browserName = "chromium";
      launchOptions = {
        executablePath = lib.getExe pkgs.chromium;
        headless = true;
        chromiumSandbox = true;
        args =
          [
            "--user-agent=${playwrightUserAgent}"
          ]
          ++ lib.optional (playwrightExtensions != []) "--load-extension=@PLAYWRIGHT_EXTENSIONS@"
          ++ lib.optional (playwright.allowManifestV2 or false) "--enable-features=${lib.concatStringsSep "," playwrightEnabledFeatures}"
          ++ (playwright.extraArgs or []);
      };
      userDataDir = "@PLAYWRIGHT_USER_DATA_DIR@";
    };
  };

  playwrightMcpWrapper = pkgs.writeShellApplication {
    name = "playwright-mcp-ephemeral";
    runtimeInputs = with pkgs; [coreutils findutils gnugrep gnused jq];
    text = ''
      export PLAYWRIGHT_MCP=${lib.escapeShellArg (lib.getExe playwright-mcp)}
      export PLAYWRIGHT_MCP_CONFIG=${lib.escapeShellArg "${playwrightMcpConfig}"}
      export PLAYWRIGHT_EXTENSIONS=${lib.escapeShellArg (lib.concatMapStrings (ext: "${ext}\n") playwrightExtensions)}
      export PLAYWRIGHT_PLACEHOLDERS=${lib.escapeShellArg (lib.concatMapStrings (var: "${var}\n") playwrightExtensionEnvPlaceholders)}
      ${builtins.readFile ./playwright-mcp-ephemeral.sh}
    '';
  };

  # The WebMCP bridge adds tools at runtime as pages expose them; the mux
  # serves a fixed tool list, so keep the list static.
  playwrightMcpCommand = [(lib.getExe playwrightMcpWrapper) "--no-webmcp"];

  # Playwright MCP's server info and tool list, captured once at build time so
  # that at runtime the mux answers `initialize` and `tools/list` itself and
  # Node.js only starts when a session makes its first browser call.
  playwrightMcpManifest =
    pkgs.runCommand "playwright-mcp-manifest.json" {
      nativeBuildInputs = [mcp-session-mux];
      env.PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD = "1";
    } ''
      mcp-session-mux --probe -- ${lib.escapeShellArgs playwrightMcpCommand} >"$out"
    '';

  evalConfig = modules:
    (lib.evalModules {
      modules = [
        {
          options.conf = lib.mkOption {
            type = lib.types.submodule {
              freeformType = configFormat.type;
            };
          };

          config.conf = lib.mkMerge modules;
        }
      ];
    }).config.conf;

  config = evalConfig [
    extraConfig
    {
      "$schema" = "https://opencode.ai/config.json";
      shell = lib.mkDefault (lib.getExe pkgs.bash);
      compaction = compactionConfig;
      share = "disabled";
      lsp = false;
      formatter = lib.optionalAttrs treefmtEnabled {
        biome.disabled = true;
        cargofmt.disabled = true;
        oxfmt.disabled = true;
        ruff.disabled = true;
        rubocop.disabled = true;
        rustfmt.disabled = true;
        shfmt.disabled = true;
        standardrb.disabled = true;
        uv.disabled = true;
        nixfmt.disabled = true;
        prettier.disabled = true;
        gofmt.disabled = true;
        treefmt = {
          command = ["treefmt" "$FILE"];
          extensions = [
            ".bash"
            ".cjs"
            ".css"
            ".envrc"
            ".envrc.*"
            ".go"
            ".html"
            ".js"
            ".json"
            ".json5"
            ".jsonc"
            ".jsx"
            ".md"
            ".mdx"
            ".mjs"
            ".nix"
            ".py"
            ".pyi"
            ".rb"
            ".rs"
            ".scss"
            ".sh"
            ".toml"
            ".ts"
            ".tsx"
            ".vue"
            ".yaml"
            ".yml"
          ];
        };
      };
      mcp =
        lib.optionalAttrs (serena != null) {
          serena = {
            type = "local";
            command = ["serena" "start-mcp-server"];
            enabled = true;
          };
        }
        // lib.optionalAttrs (playwright-mcp != null) {
          playwright = {
            type = "local";
            command =
              [
                (lib.getExe mcp-session-mux)
                "--idle-timeout"
                (toString (playwright.sessionIdleTimeout or 1800))
                "--meta-key"
                "ai.opencode/sessionID"
                "--manifest"
                (toString playwrightMcpManifest)
                "--"
              ]
              ++ playwrightMcpCommand;
            environment = {
              PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD = "1";
              FONTCONFIG_FILE = toString playwrightFontsConf;
            };
            enabled = true;
          };
        }
        // lib.optionalAttrs (image-generation-mcp != null) {
          image_generation = {
            type = "local";
            command = [
              (lib.getExe image-generation-mcp)
              "--base-url"
              imageGeneration.baseUrl
              "--api-key-env"
              imageGeneration.apiKeyEnv
            ];
            enabled = true;
            timeout = 300000;
          };
        };
      autoupdate = false;
      provider = providerJSON;
      experimental = {
        disable_paste_summary = true;
      };
      instructions =
        ["${preamblePath}"]
        ++ lib.optional (playwright-mcp != null) "${./playwright-instructions.md}"
        ++ lib.optional (playwright-mcp != null && (playwright.captchaSolverEnabled or false)) "${../playwright-extensions/captcha-solver-instructions.md}";
      # We're running in a strict sandbox, so let's relax the default permissions.
      # Set at top level so all agents (build, plan, custom) inherit them.
      permission = {
        "*" = "allow";
        lsp =
          if serena != null
          then "deny" # we have a better serena for this
          else "allow";
        doom_loop = "deny";
      };
    }
  ];

  tuiConfig = evalConfig [
    extraTuiConfig
    {
      "$schema" = "https://opencode.ai/tui.json";
      diff_style = "stacked";
      theme = "solarized";
      cursor = {
        style = "line";
        blinking = true;
      };
    }
  ];

  preambleCommand = pkgs.writeShellScript "opencode-preamble-command" (
    lib.concatMapStringsSep "\n" (script: let
      command = lib.escapeShellArg (toString script);
    in ''
      if ! ${command}; then
        printf >&2 'Preamble command failed: %s\n' ${command}
      fi
      printf '\n'
    '')
    preambleScriptPaths
  );

  commandSources = lib.mapAttrs (name: source:
    if lib.hasPrefix "${builtins.storeDir}/" (toString source)
    then source
    else
      builtins.path {
        path = source;
        name = "opencode-command-${name}.md";
      })
  commandPaths;

  # Runs inside the sandbox before the interactive shell.
  sandboxInit = pkgs.writeShellScript "sandbox-init" ''
    ${lib.optionalString (serena != null) ''
      # Serena’s global config needs to be writable.
      mkdir -p "$HOME/.serena"
      install -m 644 ${./serena-config.yml} "$HOME/.serena/serena_config.yml"
    ''}
    exec "$@"
  '';

  inherit (plugins) opencode-plugins;

  bashrc = pkgs.writeText "opencode-bashrc" ''
    ${builtins.readFile bashrcSource}
    eval "$(${lib.getExe pkgs.direnv} hook bash)"
  '';

  zshrc = pkgs.writeText "opencode-zshrc" ''
    ${builtins.readFile zshrcSource}
    eval "$(${lib.getExe pkgs.direnv} hook zsh)"
  '';

  # With `--new-session` we don’t have a controlling TTY for the Bash inside the
  # sandbox, so everything works a little weird. But with it, keystrokes could
  # be injected into the controlling terminal from within the sandbox using the
  # TIOCSTI ioctl.
  #
  # The following program emits a seccomp BPF program that blocks ioctl(...,
  # TIOCSTI, ...).
  bwrapTiocstiFilter = pkgs.stdenv.mkDerivation rec {
    name = "bwrap-tiocsti-seccomp-filter";
    dontUnpack = true;
    nativeBuildInputs = [pkgs.pkg-config];
    buildInputs = [pkgs.libseccomp];
    src = ./seccomp-tiocsti-filter.c;
    buildPhase = ''
      cc -O2 -Wall -Wextra -o gen "$src" -lseccomp
    '';
    installPhase = ''
      mkdir -p "$out/bin"
      install -m755 gen "$out/bin/${name}"
    '';
    meta.mainProgram = name;
  };

  safe = pkgs.writeShellApplication {
    name = "opencode-bwrap";
    runtimeInputs = with pkgs; [bubblewrap coreutils findutils];
    text = ''
      # Keep build-time-only dependencies alive (prevent GC):
      # ${lib.getExe bun2nix}

      data_dir="$HOME"/${lib.escapeShellArg dataDirPrefix}

      sandbox_home="$data_dir"/home
      mkdir -p "$sandbox_home"

      # Only these will persist in the sandbox $HOME:
      persist_dirs=(
        .bin
        .cache .config .local
        .cargo
        .bun .npm .yarn
      )
      persist_files=(
        .bash_history .python_history
      )

      shell_exe=${lib.getExe pkgs.zsh}

      GID=$(id -g)

      etc_passwd="$data_dir/etc-passwd"
      echo "$USER:x:$UID:$GID::$HOME:$shell_exe" >"$etc_passwd"

      etc_group="$data_dir/etc-group"
      printf "users:x:%s:\nnogroup:x:65534:" "$GID" >"$etc_group"

      bwrap_opts=(
        --unshare-all
        --die-with-parent
        --clearenv
        --proc /proc
        --dev /dev
        --share-net
        --tmpfs /tmp
        --tmpfs /run/user/"$UID"
        --setenv XDG_RUNTIME_DIR /run/user/"$UID"
        --tmpfs "$HOME"
        --ro-bind ${pkgs.writeText "etc-hosts" "127.0.0.1 localhost\n"} /etc/hosts
        --ro-bind "$etc_passwd" /etc/passwd
        --ro-bind "$etc_group" /etc/group
        --ro-bind ${bashrc} /etc/bashrc
        --ro-bind ${zshrc} /etc/zshrc
        --ro-bind ${pkgs.emptyFile} "$HOME"/.zshrc
        --ro-bind "${pkgs.coreutils}/bin/env" /usr/bin/env
        --setenv SHELL "$shell_exe"
        --setenv PATH ${lib.makeBinPath ([
          unsafe
        ]
        ++ lib.optional (serena != null) serena
        ++ [
          escapeHatchShims
        ]
        ++ extraPackages)}:/etc/profiles/per-user/"$USER"/bin:/run/current-system/sw/bin:"$HOME"/.bin
        --setenv TERMINFO_DIRS /etc/profiles/per-user/"$USER"/share/terminfo:/run/current-system/sw/share/terminfo
        --setenv NIX_PATH ${lib.escapeShellArg "nixpkgs=${pkgs.path}"}
        --setenv OPENCODE_DISABLE_LSP_DOWNLOAD "true"
        --setenv OPENCODE_DISABLE_PROJECT_CONFIG "true"
      )

      # Host paths bind-mounted read-only at the same location.
      # Paths that don't exist on the host are silently skipped.
      host_ro_mounts=(
        /bin/sh
        /etc/machine-id
        /etc/nix
        /etc/profiles/per-user/"$USER"
        /etc/resolv.conf
        /etc/ssl
        /etc/static/nix
        /etc/static/ssl
        /etc/static/terminfo
        /etc/terminfo
        /nix
        /run/current-system/sw
      )
      for p in "''${host_ro_mounts[@]}"; do
        [ -e "$p" ] && bwrap_opts+=( --ro-bind "$p" "$p" )
      done

      # The host time zone, otherwise the sandbox (and every website the
      # headless browser visits) sees UTC. `TZ` and `TZDIR` are forwarded
      # below: glibc needs `TZDIR` to resolve a zone name in `TZ`, ICU (in
      # Chromium and Node) reads `TZ` first, then falls back to this symlink.
      if localtime=$(readlink -f /etc/localtime 2>/dev/null) && [ -f "$localtime" ]; then
        bwrap_opts+=( --symlink "$localtime" /etc/localtime )
      fi

      # Host env vars forwarded into the sandbox (skipped if unset).
      host_env_forward=(
        COLORTERM
        HOME
        LANG
        LOCALE_ARCHIVE
        LOCALE_ARCHIVE_2_27
        TERM
        TZ
        TZDIR
        USER
      )
      for v in "''${host_env_forward[@]}"; do
        [ -n "''${!v+x}" ] && bwrap_opts+=( --setenv "$v" "''${!v}" )
      done

      # A zone name in `TZ` (e.g. `Europe/Warsaw` on a UTC server) needs a
      # zoneinfo database to resolve against, or glibc silently falls back to
      # UTC. Point at Nixpkgs' tzdata when the host does not say otherwise.
      if [ -n "''${TZ+x}" ] && [ -z "''${TZDIR+x}" ]; then
        bwrap_opts+=( --setenv TZDIR ${pkgs.tzdata}/share/zoneinfo )
      fi

      for d in "''${persist_dirs[@]}" ; do
        mkdir -p "$sandbox_home"/"$d"
        bwrap_opts+=( --bind "$sandbox_home"/"$d" "$HOME"/"$d" )
      done

      # Host's Nix evaluation cache (tarballs, git archives fetched during
      # flake eval) mounted read-only with a tmpfs overlay so the sandbox
      # appears to have a writable cache without leaking writes to the host.
      if [ -d "$HOME/.cache/nix" ]; then
        bwrap_opts+=(
          --overlay-src "$HOME/.cache/nix"
          --tmp-overlay "$HOME/.cache/nix"
        )
      fi

      for f in "''${persist_files[@]}" ; do
        touch "$sandbox_home"/"$f"
        bwrap_opts+=( --bind "$sandbox_home"/"$f" "$HOME"/"$f" )
      done

      if [ -S /run/user/"$UID"/bwrap-escape-hatch.sock ]; then
        bwrap_opts+=( --ro-bind /run/user/"$UID"/bwrap-escape-hatch.sock /run/user/"$UID"/bwrap-escape-hatch.sock )
        # opencode-notifier refuses to run notify-send unless this is set. The
        # session bus itself stays outside the sandbox: our notify-send is an
        # escape-hatch shim that talks to the host over the socket above, so
        # the address only needs to be present, never reachable from here.
        bwrap_opts+=( --setenv DBUS_SESSION_BUS_ADDRESS unix:path=/run/user/"$UID"/bus )
      fi

      if [ -f "$HOME"/.config/git/ignore ] ; then
        bwrap_opts+=( --ro-bind "$HOME"/.config/git/ignore "$HOME"/.config/git/ignore )
      fi

      bwrap_opts+=( --ro-bind "${pkgs.nix-direnv}/share/nix-direnv/direnvrc" "$HOME"/.config/direnv/lib/nix-direnv.sh )

      ${lib.concatStringsSep "\n" (lib.mapAttrsToList (name: value: ''
          bwrap_opts+=( --setenv ${lib.escapeShellArg name} ${lib.escapeShellArg value} )
        '')
        extraEnv)}

      # Read secrets from files in the persistent sandbox home. This happens at
      # runtime, so neither their paths' contents nor their values enter Nix.
      ${lib.concatStringsSep "\n" (lib.mapAttrsToList (name: path: ''
          _secret_file="$sandbox_home"/${lib.escapeShellArg path}
          if [ -f "$_secret_file" ]; then
            _secret=$(<"$_secret_file")
            if [ -n "$_secret" ]; then
              bwrap_opts+=( --setenv ${lib.escapeShellArg name} "$_secret" )
            fi
          fi
        '')
        extraEnvFiles)}

      ${lib.optionalString (extraFwdEnv != []) ''
        # Forward host environment variables into the sandbox
        # shellcheck disable=SC2043
        for _var in ${lib.concatMapStringsSep " " lib.escapeShellArg extraFwdEnv}; do
          if [ -n "''${!_var+x}" ]; then
            bwrap_opts+=( --setenv "$_var" "''${!_var}" )
          fi
        done
      ''}

      ${lib.optionalString (preambleScriptPaths != []) ''
        bwrap_opts+=( --setenv OPENCODE_EXTRA_INSTRUCTIONS_COMMAND "${preambleCommand}" )
      ''}

      ${lib.optionalString (commandSources != {}) ''
        mkdir -p "$sandbox_home"/.config/opencode/commands
        ${lib.concatStringsSep "\n" (lib.mapAttrsToList (name: source: ''
            bwrap_opts+=( --ro-bind ${lib.escapeShellArg (toString source)} "$HOME"/${lib.escapeShellArg ".config/opencode/commands/${name}.md"} )
          '')
          commandSources)}
      ''}

      # OpenCode plugins (pinned via Nix flake inputs, mounted read-only)
      bwrap_opts+=( --ro-bind ${opencode-plugins} "$HOME"/.config/opencode/plugins )

      # opencode-notifier config
      bwrap_opts+=( --ro-bind ${configFormat.generate "opencode-notifier.json" notifierConfig} "$HOME"/.config/opencode/opencode-notifier.json )

      # OpenCode config
      bwrap_opts+=( --ro-bind ${configFormat.generate "config.json" config} "$HOME"/.config/opencode/config.json )
      bwrap_opts+=( --ro-bind ${configFormat.generate "tui.json" tuiConfig} "$HOME"/.config/opencode/tui.json )

      rw_opts=()
      ro_git_opts=()
      mount_dirs=()
      sandbox_cmd=( "$shell_exe" )
      parsing_cmd=0

      # Make argv absolute without resolving symlinks (pwd -L)
      abspath() {
        local p="$1"
        if [[ "$p" == /* ]]; then
          printf '%s\n' "$p"
        else
          ( cd "$(dirname -- "$p")" && printf '%s/%s\n' "$(pwd -L)" "$(basename -- "$p")" )
        fi
      }

      for arg in "$@"; do
        if [ "$parsing_cmd" -eq 1 ]; then
          sandbox_cmd+=( "$arg" )
          continue
        fi

        if [ "$arg" = -- ]; then
          parsing_cmd=1
          sandbox_cmd=()
          continue
        fi

        mount_dirs+=( "$arg" )
      done

      if [ "$parsing_cmd" -eq 1 ] && [ "''${#sandbox_cmd[@]}" -eq 0 ]; then
        sandbox_cmd=( "$shell_exe" )
      fi

      for d in "''${mount_dirs[@]}"; do
        [ -d "$d" ] || {
          echo >&2 "$0: cannot access '$d': No such file or directory"
          exit 1
        }

        d="$(abspath "$d")"

        # Mount project dir at same path (read-write)
        rw_opts+=( --bind "$d" "$d" )

        # Then over-mount any `.git` entries inside it as read-only (dir, file, or symlink)
        if [ -z "''${OPENCODE_UNSAFE_RW_GIT-}" ]; then
          while IFS= read -r -d "" gitpath; do
            ro_git_opts+=( --ro-bind "$gitpath" "$gitpath" )
          done < <(
            find "$d" \
              -name .git \
              \( -type d -o -type f -o -type l \) \
              -print0 2>/dev/null || true
          )
        fi
      done

      exec bwrap \
        "''${bwrap_opts[@]}" \
        "''${rw_opts[@]}" \
        "''${ro_git_opts[@]}" \
        --seccomp 3 3< <(${lib.getExe bwrapTiocstiFilter}) \
        -- ${sandboxInit} "''${sandbox_cmd[@]}"
    '';
    derivationArgs = {
      meta = {
        description = "Enters a (multi-)project sandbox to run `opencode` inside; `.git` entries are mounted read-only unless OPENCODE_UNSAFE_RW_GIT is set.";
        platforms = lib.platforms.linux;
      };
      passthru = {
        bwrap-escape-hatch = bwrap-escape-hatch // {inherit escapeHatchShims;};
        inherit plugins config tuiConfig playwright-mcp mcp-session-mux playwrightMcpWrapper playwrightMcpConfig playwrightMcpManifest image-generation-mcp;
      };
    };
  };
in
  safe
