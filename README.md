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
- Provides [Playwright MCP](https://github.com/microsoft/playwright-mcp) for
  headless browser automation with [Camoufox](https://camoufox.com) (a
  Firefox fork that resists bot detection) inside the sandbox
  (enabled by default), with a separate browser for every opencode session
  and subagent.
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

| Option                                | Type             | Description                                                                      |
| ------------------------------------- | ---------------- | -------------------------------------------------------------------------------- |
| `enable`                              | bool             | Enable the sandbox wrapper                                                       |
| `preamble`                            | path             | Instructions file mounted into the sandbox                                       |
| `preambleScripts`                     | list             | Ordered executable packages or paths appended at runtime                         |
| `dataDirPrefix`                       | string           | Relative path under `$HOME` for persistent sandbox state                         |
| `bashrc` / `zshrc`                    | path             | Shell configs sourced inside the sandbox                                         |
| `commands`                            | attrs of paths   | Global command names and their Markdown source files                             |
| `extraPackages`                       | list of packages | Additional packages on the sandbox PATH                                          |
| `extraEnv`                            | attrs of strings | Static env vars set in the sandbox                                               |
| `extraEnvFiles`                       | attrs of strings | Env vars read from files in the persistent sandbox home                          |
| `extraFwdEnv`                         | list of strings  | Host env vars forwarded into the sandbox                                         |
| `maxContextTokens`                    | positive integer | Maximum model context and input tokens (default: 224\*1024)                      |
| `pasteAttachments`                    | bool             | Turn pasted image/SVG/PDF paths into attachments (default: false)                |
| `databaseName`                        | string or null   | Session-history database file (default: `opencode.db`)                           |
| `treefmt.enable`                      | bool             | Use treefmt as exclusive formatter (default: true)                               |
| `serena.enable`                       | bool             | Serena MCP integration for code navigation (default: true)                       |
| `playwright.enable`                   | bool             | Playwright MCP with headless Camoufox (default: true)                            |
| `playwright.package`                  | package          | The Camoufox package (default: `camoufox` from this flake)                       |
| `playwright.sessionIdleTimeout`       | unsigned integer | Seconds without tool calls before a session's browser closes (default: 1800)     |
| `playwright.adblock.enable`           | bool             | uBlock Origin in the browser (default: false)                                    |
| `playwright.captchaSolver.enable`     | bool             | 2Captcha solver add-on in the browser (default: false)                           |
| `playwright.captchaSolver.apiKeyFile` | string or null   | 2Captcha key file in the sandbox home (default: `.config/opencode/2captcha.key`) |
| `playwright.captchaSolver.apiKeyEnv`  | string           | Env var carrying the key (default: `TWOCAPTCHA_API_KEY`)                         |
| `playwright.extensions`               | list of packages | Extra unpacked Firefox add-ons loaded into every session                         |
| `playwright.extensionEnvPlaceholders` | list of strings  | Env var names substituted for `@NAME@` inside add-ons                            |
| `playwright.userAgent`                | string or null   | Browser user agent (default: stock Firefox of the same version)                  |
| `playwright.fingerprint`              | attrs            | Camoufox fingerprint properties merged over the defaults                         |
| `playwright.prefs`                    | attrs            | Additional Firefox preferences                                                   |
| `playwright.extraArgs`                | list of strings  | Additional Firefox command-line arguments                                        |
| `notifications.enable`                | bool             | Desktop notifications + sounds via escape hatch (default: true)                  |
| `notifications.sounds.*`              | path or null     | Per-event sound files (converted to WAV at build time)                           |
| `notifications.messages.*`            | string           | Per-event notification body templates                                            |
| `notifications.extraRules`            | list of rules    | Additional escape-hatch allow-list entries                                       |

### Playwright MCP

`playwright.enable` defaults to `true`. The MCP server uses headless
[Camoufox](https://github.com/daijro/camoufox), a Firefox fork patched for
Playwright that hides automation and reports a configurable fingerprint below
the JavaScript layer. Sites behind Cloudflare and DataDome that block headless
and headed Chromium alike load in it. The [`camoufox/`](camoufox/) package
repackages the official Linux release (`nix build .#camoufox`), with only the
Linux fonts of its bundled set, so that font probing matches the reported OS.

opencode starts one process per MCP server and shares it between the main
session and all subagents. For Playwright that would mean one browser, where
every subagent sees the same tabs and steals the "current tab" from the others.
Instead, `mcp-session-mux` ([`mcp-session-mux/`](mcp-session-mux/)) sits in
front of Playwright MCP. A patch to opencode adds the session ID to the
`_meta` of every MCP tool call, and the mux starts a separate Playwright MCP
process, and with it a separate browser, for each session ID it sees. Each
browser has a profile in a temporary directory that is deleted when the
process exits, so sessions and subagents do not share cookies, logins, or
tabs, and their tool calls run in parallel. A browser that receives no tool
calls for `playwright.sessionIdleTimeout` seconds (default: 30 minutes) is
closed; the next call starts a fresh one with no tabs. Set the option to `0`
to keep browsers open until opencode exits.

The mux answers `initialize` and `tools/list` from a manifest that the Nix
build captures by probing Playwright MCP once, so a session that never touches
the browser costs about 7 MiB for the mux itself, and Node.js and Camoufox
start only on the first browser call.

The agent's instructions gain a section
([`opencode-bwrap/playwright-instructions.md`](opencode-bwrap/playwright-instructions.md))
that explains the per-session browser and asks the agent to call
`browser_close` when a browsing task is done. That closes the session's
browser at once, about 1 GiB of memory, instead of waiting for the idle
timeout; the next browser call starts a fresh one.

The browser reports the user agent of a stock Firefox on Linux of the same
major version instead of `Camoufox/…`, a 1920×1080 screen with a maximized
window, and `navigator.webdriver` is `false`. Override the string with
`playwright.userAgent` and any other
[property](https://camoufox.com/fingerprint/) with `playwright.fingerprint`,
for example `{"navigator.hardwareConcurrency" = 8;}`. The sandbox also carries
the host time zone (`/etc/localtime`, `TZ`, `TZDIR`), so pages see the same
zone as the host. DNS over HTTPS is off, and names resolve through the host.

The Playwright MCP source is pinned in `flake.lock`. Nix fetches the npm dependencies with a
fixed hash and installs them offline in the build sandbox. No `npx` command or
browser download is needed at runtime.

To disable the integration:

```nix
programs.opencode-bwrap.playwright.enable = false;
```

#### Ad blocking

```nix
programs.opencode-bwrap.playwright.adblock.enable = true;
```

Loads [uBlock Origin](https://github.com/gorhill/uBlock) with its default
filter lists into every browser session. It is the Firefox build of the full
uBlock Origin, with dynamic filtering and the full filter syntax.

#### CAPTCHA solving

```nix
programs.opencode-bwrap.playwright.captchaSolver.enable = true;
```

Loads the [2Captcha solver](https://github.com/rucaptcha/2captcha-solver)
add-on, in its newest Firefox build from addons.mozilla.org. It solves image
captchas, reCAPTCHA v2 (including invisible), GeeTest v3/v4, KeyCAPTCHA,
Arkose Labs (FunCaptcha), Lemin, Yandex, Capy, Amazon WAF, Cloudflare
Turnstile, and MTCaptcha automatically as they appear.
reCAPTCHA v3 is left on manual because it is invisible and would be billed on
every page load. 2Captcha does not support hCaptcha.

Put the API key from <https://2captcha.com> into the persistent sandbox home:

```
install -m 600 /dev/stdin ~/.local/share/opencode-bwrap/home/.config/opencode/2captcha.key <<< 'YOUR_KEY'
```

The sandbox reads this file at start (`captchaSolver.apiKeyFile`, relative to
the sandbox home) and injects the key into the session's private copy of the
add-on, so it never enters the Nix store. Set `apiKeyFile = null` to provide
`captchaSolver.apiKeyEnv` through `extraEnvFiles` or `extraFwdEnv` instead.

With the solver enabled, the agent's instructions gain a section that tells it
to wait for the extension's `Solve with 2Captcha` control to reach `solved`
instead of trying to solve the challenge itself, and to stop after three
minutes because every attempt costs money.

#### Custom add-ons

`playwright.extensions` lists further unpacked Firefox add-ons (directories
with a `manifest.json`, for example an `.xpi` unpacked with `pkgs.fetchzip`)
to load into every browser session. They load as temporary add-ons, so they
need no signature. Before the browser starts, each one is copied into the
session's temporary directory, and every `@NAME@` token inside the copy, for
each `NAME` in `playwright.extensionEnvPlaceholders`, is replaced with the
value of that environment variable. Combined with `extraEnvFiles`, this lets an
add-on carry an API key that never enters the Nix store:

```nix
programs.opencode-bwrap = {
  extraEnvFiles.MY_EXTENSION_TOKEN = ".config/opencode/my-extension.token";
  playwright = {
    # An unpacked add-on whose config reads `token: "@MY_EXTENSION_TOKEN@"`.
    extensions = [pkgs.my-unpacked-addon];
    extensionEnvPlaceholders = ["MY_EXTENSION_TOKEN"];
  };
};
```

An add-on that opens its options page on first install does so in every
session; strip
`options_ui` from its manifest at build time to avoid the extra tab.

### Custom commands

`commands` maps each slash-command name to a Markdown source path. The module
mounts each file read-only at `~/.config/opencode/commands/<name>.md` in the
sandbox.

```nix
programs.opencode-bwrap.commands = {
  review = ./commands/review.md;
  "create-component" = ./commands/create-component.md;
};
```

The files provide `/review` and `/create-component`. Other command files in
the persistent sandbox remain available.

### Secret files

`extraEnvFiles` reads each non-empty file when the sandbox starts and exports
its contents as the mapped environment variable. Paths are relative to the
persistent sandbox home, so the key is neither stored in a Nix derivation nor
read from the host environment.

```nix
programs.opencode-bwrap.extraEnvFiles = {
  SOME_API_KEY = ".config/opencode/some-api.key";
};
```

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
nix build -L .#mcp-session-mux     # per-session MCP proxy (runs its tests)
nix build -L .#camoufox            # browser for the Playwright MCP
nix build -L .#preamble-environment # default runtime preamble
nix build -L .#preamble-project-instructions
```

Supported systems: `x86_64-linux`, `aarch64-linux`.

## Layout

```
flake.nix                 Flake entry point
hm-module.nix             Home Manager module (options + systemd units)
opencode-bwrap/           Sandbox wrapper package (Nix + shell + seccomp)
camoufox/                 Camoufox browser package for the Playwright MCP
playwright-extensions/    Browser add-ons for the Playwright MCP
mcp-session-mux/          Per-session MCP server proxy (Rust)
image-generation-mcp/     Image generation MCP for CLIProxyAPI (Rust)
bwrap-escape-hatch/       Escape-hatch service (Rust)
plugins/                  opencode plugins (anthropic-auth, notifier)
```

## License

[Apache 2.0](LICENSE)
