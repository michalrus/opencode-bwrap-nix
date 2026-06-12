{
  pkgs,
  nixpkgs-opencode,
  lib,
  bun2nix,
  serena ? null,
  plugins,
  bwrap-escape-hatch,
  # Overridable by the home-manager module:
  preamblePath ? ./preamble.md,
  preambleScriptPath ? null,
  dataDirPrefix ? ".local/share/opencode-bwrap",
  bashrcSource ? ./bashrc,
  zshrcSource ? ./zshrc,
  extraConfig ? {},
  extraTuiConfig ? {},
  extraPackages ? [],
  extraEnv ? {},
  extraFwdEnv ? [],
  notifierConfig ? plugins.opencode-notifier-config,
  treefmtEnabled ? true,
  compactionConfig ? {
    auto = true;
    prune = true;
  },
  providerJSON ? {},
}: let
  unsafe = nixpkgs-opencode.legacyPackages.${pkgs.stdenv.hostPlatform.system}.opencode.overrideAttrs (prev: {
    patches =
      (prev.patches or [])
      ++ [
        ./opencode--instructions_command.patch
        ./opencode--cursor-beam.patch
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
      mcp = lib.optionalAttrs (serena != null) {
        serena = {
          type = "local";
          command = ["serena" "start-mcp-server"];
          enabled = true;
        };
      };
      autoupdate = false;
      provider = providerJSON;
      experimental = {
        disable_paste_summary = true;
      };
      instructions = ["${preamblePath}"];
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
    }
  ];

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

      # Host env vars forwarded into the sandbox (skipped if unset).
      host_env_forward=(
        COLORTERM
        HOME
        LANG
        LOCALE_ARCHIVE
        LOCALE_ARCHIVE_2_27
        TERM
        USER
      )
      for v in "''${host_env_forward[@]}"; do
        [ -n "''${!v+x}" ] && bwrap_opts+=( --setenv "$v" "''${!v}" )
      done

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
      fi

      if [ -f "$HOME"/.config/git/ignore ] ; then
        bwrap_opts+=( --ro-bind "$HOME"/.config/git/ignore "$HOME"/.config/git/ignore )
      fi

      bwrap_opts+=( --ro-bind "${pkgs.nix-direnv}/share/nix-direnv/direnvrc" "$HOME"/.config/direnv/lib/nix-direnv.sh )

      ${lib.concatStringsSep "\n" (lib.mapAttrsToList (name: value: ''
          bwrap_opts+=( --setenv ${lib.escapeShellArg name} ${lib.escapeShellArg value} )
        '')
        extraEnv)}

      ${lib.optionalString (extraFwdEnv != []) ''
        # Forward host environment variables into the sandbox
        for _var in ${lib.concatMapStringsSep " " lib.escapeShellArg extraFwdEnv}; do
          if [ -n "''${!_var+x}" ]; then
            bwrap_opts+=( --setenv "$_var" "''${!_var}" )
          fi
        done
      ''}

      ${lib.optionalString (preambleScriptPath != null) ''
        bwrap_opts+=( --setenv OPENCODE_EXTRA_INSTRUCTIONS_COMMAND "${preambleScriptPath}" )
      ''}

      # OpenCode plugins (pinned via fetchFromGitHub, mounted read-only)
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
        inherit plugins config tuiConfig;
      };
    };
  };
in
  safe
