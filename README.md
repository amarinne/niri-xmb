# niri-xmb

`niri-xmb` is a workspace navigator and bar integration for [Niri](https://github.com/YaLTeR/niri).
It keeps Niri state in a persistent daemon, so the panel opens without a new startup delay.

The project supports workspace and window navigation, drag and drop, passive workspace peek,
themes, window urgency, and Noctalia taskbar integration.

## Requirements

- A Linux Wayland session that runs Niri.
- A Rust toolchain that supports edition 2024.
- Niri IPC access from the process that runs `niri-xmb`.

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

Create a user configuration from the example. Review output names, placement, and theme settings
before you use it:

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
niri-xmb status
niri-xmb print-default-config
```

The default configuration path is `~/.config/niri-xmb/config.toml`.

## Repository layout

```text
crates/niri-xmb/       Rust daemon, CLI, panel, model, and icon code
plugins/noctalia-v4/   Legacy Noctalia integration
ports/noctalia-v5/     Maintained Noctalia integration
config/                Example configuration
themes/                Optional panel themes
UX_SPEC.md             Interaction specification
niri-xmb.service       Optional user-service unit
```

## More information

- See [UX_SPEC.md](UX_SPEC.md) for interaction rules.
- See [ports/noctalia-v5/README.md](ports/noctalia-v5/README.md) for Noctalia v5 setup.
