# opencode-bwrap-nix

Nix flake that runs [opencode](https://opencode.ai) inside a
[Bubblewrap](https://github.com/containers/bubblewrap) sandbox on Linux,
with a Home Manager module for declarative installation.

## What it does

- Isolates the AI coding agent with `--unshare-all` (PID, net, mount, etc.)
  and a clean environment.
- Mounts `.git` directories **read-only** so the agent cannot rewrite
  history (override with `OPENCODE_UNSAFE_RW_GIT=1`).
- Applies a seccomp-BPF filter that blocks the `TIOCSTI` ioctl, preventing
  keystroke injection into the host terminal.
- Provides a socket-activated **escape hatch** for operations that must run
  on the host (desktop notifications, sound playback), gated by an
  fnmatch allow-list. See [`bwrap-escape-hatch/README.md`](bwrap-escape-hatch/README.md).
- Optionally integrates [Serena](https://github.com/oraios/serena) as an
  MCP server for LSP-powered code navigation inside the sandbox (enabled
  by default).
- Supports [direnv](https://direnv.net/) + nix-direnv for per-project Nix
  dev shells.

## Quick start

Add the flake to your Home Manager configuration:

```nix
# flake.nix
{
  inputs.opencode-bwrap.url = "github:michalrus/opencode-bwrap-nix";

  outputs = { self, home-manager, opencode-bwrap, ... }: {
    homeConfigurations."you" = home-manager.lib.homeManagerConfiguration {
      modules = [
        opencode-bwrap.homeManagerModules.default
        {
          programs.opencode-bwrap = {
            enable = true;
          };
        }
      ];
    };
  };
}
```

Then run:

```
opencode-bwrap /path/to/project [/path/to/other/project ...]
opencode-bwrap /path/to/project -- opencode --help
```

This drops you into a sandboxed Zsh shell with the listed project
directories mounted read-write. Run `opencode` (aliased `oc`) from there.

Pass `--` to stop parsing mount directories and run a command inside the
sandbox instead of starting an interactive shell.

## Home Manager options

| Option                     | Type             | Description                                                     |
| -------------------------- | ---------------- | --------------------------------------------------------------- |
| `enable`                   | bool             | Enable the sandbox wrapper                                      |
| `preamble`                 | path             | Instructions file mounted into the sandbox                      |
| `preambleScripts`          | list             | Ordered executable packages or paths appended at runtime        |
| `dataDirPrefix`            | string           | Relative path under `$HOME` for persistent sandbox state        |
| `bashrc` / `zshrc`         | path             | Shell configs sourced inside the sandbox                        |
| `extraPackages`            | list of packages | Additional packages on the sandbox PATH                         |
| `extraEnv`                 | attrs of strings | Static env vars set in the sandbox                              |
| `extraFwdEnv`              | list of strings  | Host env vars forwarded into the sandbox                        |
| `treefmt.enable`           | bool             | Use treefmt as exclusive formatter (default: true)              |
| `serena.enable`            | bool             | Serena MCP integration for code navigation (default: true)      |
| `notifications.enable`     | bool             | Desktop notifications + sounds via escape hatch (default: true) |
| `notifications.sounds.*`   | path or null     | Per-event sound files (converted to WAV at build time)          |
| `notifications.messages.*` | string           | Per-event notification body templates                           |
| `notifications.extraRules` | list of rules    | Additional escape-hatch allow-list entries                      |

### Preamble scripts

`preambleScripts` defaults to the environment and repository summary followed
by hierarchical project instructions. Setting the option replaces that default.
Include both provided packages explicitly when composing them with your own
executable:

```nix
programs.opencode-bwrap.preambleScripts = [
  inputs.opencode-bwrap.packages.${pkgs.system}.preamble-environment
  inputs.opencode-bwrap.packages.${pkgs.system}.preamble-project-instructions
  (pkgs.writeShellApplication {
    name = "my-opencode-preamble";
    text = ''
      printf '%s\n' 'Additional runtime instructions'
    '';
  })
];
```

To keep the environment summary without loading project instruction files:

```nix
programs.opencode-bwrap.preambleScripts = [
  inputs.opencode-bwrap.packages.${pkgs.system}.preamble-environment
];
```

Scripts run serially in the OpenCode working directory. Their standard output
is added to the system prompt in list order. A failing script is reported and
does not prevent later scripts from running. The
`preamble-project-instructions` package searches from the working directory to
the Git root. In each directory from the root through the working directory, it
loads at most one file in this priority order: `AGENTS.md`, `CLAUDE.md`, then
`CONTEXT.md`. Files nearer the working directory are appended later so they can
specialize broader repository instructions.

## Building from source

```
nix build -L .#opencode-bwrap      # main sandboxed wrapper
nix build -L .#preamble-environment # default runtime preamble
nix build -L .#preamble-project-instructions
```

Supported systems: `x86_64-linux`, `aarch64-linux`.

## Layout

```
flake.nix                 Flake entry point
hm-module.nix             Home Manager module (options + systemd units)
opencode-bwrap/           Sandbox wrapper package (Nix + shell + seccomp)
bwrap-escape-hatch/       Escape-hatch service (Rust)
plugins/                  opencode plugins (anthropic-auth, notifier)
```

## License

[Apache 2.0](LICENSE)
