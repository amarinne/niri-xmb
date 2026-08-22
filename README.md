# niri-xmb

Workspace navigator and bar integration for [Niri](https://github.com/YaLTeR/niri).

Features:

- workspace and window navigation
- drag and drop between workspaces and columns
- per-output workspace bars
- cursor-positioned workspace navigator
- passive workspace peek
- themes and window urgency
- Noctalia taskbar integration
- persistent daemon for fast panel startup

## Installation

Requires Linux, Niri, and an `x86_64` or `aarch64` system.

Run:

```sh
curl -fsSL https://raw.githubusercontent.com/amarinne/niri-xmb/main/setup.sh | sh
```

The script:

- downloads and verifies the latest release
- installs `niri-xmb` in `~/.local/bin`
- installs the config example and themes in `~/.config/niri-xmb`
- adds `niri-xmb` to the Niri startup config
- keeps existing config and theme files

Reload Niri, then run `niri-xmb toggle` to open the navigator.

No config file is required. The default config detects connected outputs.

For custom settings, copy the example:

```sh
cp ~/.config/niri-xmb/config.toml.example ~/.config/niri-xmb/config.toml
```

Edit the output names in the copied file. To copy the example during setup:

```sh
curl -fsSL https://raw.githubusercontent.com/amarinne/niri-xmb/main/setup.sh | \
  sh -s -- --init-config
```

## Updating

Run:

```sh
niri-xmb update
```

This downloads the latest verified release, replaces the binary, and restarts
the daemon. Use `niri-xmb update --no-restart` to restart it later.

The installation command also updates an existing install.

## Uninstalling

Run:

```sh
curl -fsSL https://raw.githubusercontent.com/amarinne/niri-xmb/main/setup.sh | \
  sh -s -- uninstall
```

This keeps `~/.config/niri-xmb`. Add `--purge` to remove the config and themes.

## Building from source

Requires a Rust toolchain with edition 2024 support and the xkbcommon development
package for your distribution.

Build and install:

```sh
cargo build --release -p niri-xmb
install -Dm755 target/release/niri-xmb ~/.local/bin/niri-xmb
```

Add this line to the Niri config:

```kdl
spawn-at-startup "niri-xmb" "daemon"
```

Optional config:

```sh
mkdir -p ~/.config/niri-xmb
cp config/niri-xmb.toml.example ~/.config/niri-xmb/config.toml
```

## Commands

```sh
niri-xmb daemon
niri-xmb toggle
niri-xmb restart
niri-xmb update
niri-xmb status
niri-xmb subscribe
niri-xmb print-default-config
```

`subscribe` streams daemon state as newline-delimited JSON frames until
interrupted, reconnecting with bounded backoff while the daemon restarts.

The default configuration path is `~/.config/niri-xmb/config.toml`.

`restart` and `update` need `NIRI_SOCKET` to restart the daemon without a Niri
reload.

## Diagnostics

Restart with raised verbosity to debug drops. `-v` enables debug logs, `-vv`
enables trace logs including full drop plans; a set `RUST_LOG` always wins:

```sh
niri-xmb -v restart
niri-xmb -vv restart
```

Verbosity is carried through `restart`, so the replacement daemon keeps the
selected level. Follow the daemon's drop lifecycle through the Noctalia log:

```sh
tail -F ~/.cache/noctalia/noctalia.log | rg --line-buffered niri-xmb
```

Inspect the last drop transaction — request id, completed and failed steps,
generations, and outcome — without raising verbosity:

```sh
niri-xmb status | jq .drop_transaction
```

Rejected drops always answer with their exact reason, for example
`error: drop rejected: target window changed workspace`. Logs contain window
and workspace ids only, never window titles.

### Output scale changes

XMB handles live Niri output scale changes without a restart. When Wayland
reports a new scale factor, XMB discards the old SHM buffers and damage history.
XMB then renders and uploads one complete frame for each affected surface.

This reset prevents old partial-damage data from producing horizontal stripes
or repeated content after a scale change.

## Repository layout

```text
crates/niri-xmb/       Rust daemon, CLI, panel, model, and icon code
ports/noctalia-v5/     Maintained Noctalia integration
archive/noctalia-v4/   Reference copy of the retired Quickshell integration
config/                Example configuration
themes/                Optional panel themes
UX_SPEC.md             Product interaction specification
niri-xmb.service       Optional user-service unit
```

## More information

- See [UX_SPEC.md](UX_SPEC.md) for product interaction rules.
- See [ports/noctalia-v5/README.md](ports/noctalia-v5/README.md) for Noctalia v5 setup.
