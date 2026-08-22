# niri-xmb

`niri-xmb` is a workspace navigator and bar integration for [Niri](https://github.com/YaLTeR/niri).
It keeps Niri state in a persistent daemon, so the panel opens without a new startup delay.

The project supports workspace and window navigation, drag and drop, passive workspace peek,
themes, window urgency, and Noctalia taskbar integration.

## Requirements

- A Linux Wayland session that runs Niri.
- Niri IPC access from the process that runs `niri-xmb`.

A Rust toolchain that supports edition 2024 is only required for a source build.

## Quick setup

Install the latest prebuilt release for Linux on `x86_64` or `aarch64`:

```sh
curl --proto '=https' --tlsv1.2 -fsSL \
  https://raw.githubusercontent.com/amarinne/niri-xmb/main/setup.sh | sh
```

The script verifies the release checksum. It installs the binary and examples.
It also adds a marked `spawn-at-startup` block to the Niri configuration. It does
not replace an existing XMB configuration or theme. Reload Niri after setup.

Run `niri-xmb toggle` to open the navigator. The default settings detect all
connected outputs. Copy the example only when you want to change the settings:

```sh
cp ~/.config/niri-xmb/config.toml.example ~/.config/niri-xmb/config.toml
```

Use `--init-config` to create this file during setup. The example contains
explicit output names, so edit those names before you reload Niri:

```sh
curl --proto '=https' --tlsv1.2 -fsSL \
  https://raw.githubusercontent.com/amarinne/niri-xmb/main/setup.sh | \
  sh -s -- install --init-config
```

Remove the binary, user service, and managed Niri block:

```sh
curl --proto '=https' --tlsv1.2 -fsSL \
  https://raw.githubusercontent.com/amarinne/niri-xmb/main/setup.sh | \
  sh -s -- uninstall
```

Uninstall keeps `~/.config/niri-xmb`. Add `--purge` to remove that directory.

## Build

Build an optimized binary:

```sh
cargo build --release -p niri-xmb
```

The binary is `target/release/niri-xmb`.

## Install

Install the binary in a directory on your `PATH`:

```sh
install -Dm755 target/release/niri-xmb ~/.local/bin/niri-xmb
```

Create a user configuration only when you need custom settings. Review output
names, placement, and theme settings before you use it:

```sh
mkdir -p ~/.config/niri-xmb
cp config/niri-xmb.toml.example ~/.config/niri-xmb/config.toml
```

Start the daemon from the Niri session. The daemon needs `NIRI_SOCKET`, which Niri provides to
commands that it starts:

```sh
niri-xmb daemon
```

Use `niri-xmb restart` only from an environment that has `NIRI_SOCKET`.

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

`niri-xmb update` downloads the latest setup script and verified release
archive. It replaces the current binary and restarts the daemon when
`NIRI_SOCKET` is available. Use `--version TAG` to install one release, or use
`--no-restart` to defer the daemon restart.

`subscribe` streams daemon state as newline-delimited JSON frames until
interrupted, reconnecting with bounded backoff while the daemon restarts.

The default configuration path is `~/.config/niri-xmb/config.toml`.

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
