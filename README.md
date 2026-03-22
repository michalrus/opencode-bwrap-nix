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
  inputs.opencode-bwrap.url = "github:anthropic/opencod3-bwrap-nix";

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
```

This drops you into a sandboxed Zsh shell with the listed project
directories mounted read-write. Run `opencode` (aliased `oc`) from there.

## Home Manager options

| Option                     | Type             | Description                                                     |
| -------------------------- | ---------------- | --------------------------------------------------------------- |
| `enable`                   | bool             | Enable the sandbox wrapper                                      |
| `preamble`                 | path             | Instructions file mounted into the sandbox                      |
| `bashrc` / `zshrc`         | path             | Shell configs sourced inside the sandbox                        |
| `extraPackages`            | list of packages | Additional packages on the sandbox PATH                         |
| `extraEnv`                 | attrs of strings | Static env vars set in the sandbox                              |
| `extraFwdEnv`              | list of strings  | Host env vars forwarded into the sandbox                        |
| `serena.enable`            | bool             | Serena MCP integration for code navigation (default: true)      |
| `notifications.enable`     | bool             | Desktop notifications + sounds via escape hatch (default: true) |
| `notifications.sounds.*`   | path or null     | Per-event sound files (converted to WAV at build time)          |
| `notifications.messages.*` | string           | Per-event notification body templates                           |
| `notifications.extraRules` | list of rules    | Additional escape-hatch allow-list entries                      |

## Building from source

```
nix build -L .#opencode-bwrap      # main sandboxed wrapper
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
