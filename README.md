# niri-xmb

`niri-xmb` provides a workspace navigator for Niri. It keeps Niri state in a persistent daemon, so the panel opens without a new startup delay.

Noctalia v5 is the primary integration. The Noctalia v4 port is legacy compatibility code.

## Current scope

`niri-xmb` owns these functions:

- The daemon reads Niri IPC events and stores output, workspace, window, focus, and urgency state.
- The panel shows an icon map for all configured outputs.
- The panel supports window focus, workspace focus, drag and drop, peek, pinning, and themes.
- The CLI sends low-cost commands to the running daemon.
- The Noctalia plugins show per-output workspace and window strips in the bar.
- The icon resolver loads application icons from desktop files and system icon themes.
- The configuration controls panel layout, monitor rules, interaction, and visual style.

`niri-xmb` does not own these functions:

- The Niri fork owns smart workspace movement.
- The Noctalia fork owns the native dock and taskbar.
- The dotfiles repository owns machine configuration and deployment.
- The Moonlight stack owns virtual monitor setup.

## Main commands

```sh
niri-xmb daemon
niri-xmb toggle
niri-xmb restart
niri-xmb status
niri-xmb print-default-config
```

The default configuration path is `~/.config/niri-xmb/config.toml`.

## Current repository

The repository currently uses this layout:

```text
crates/niri-xmb/       Rust daemon, CLI, panel, model, and icon code
plugins/noctalia-v4/   Legacy Noctalia integration
ports/noctalia-v5/     Maintained Noctalia integration
config/                Example configuration
themes/                Optional panel themes
docs/                  Focused design documents
niri-xmb.service       User service
```

## Suggested cleanup

Use one `integrations/noctalia/` tree for both Noctalia versions. Move `niri-xmb.service` to `packaging/systemd/`.

Keep shared behavior in the Rust crate. Keep Noctalia code as a small adapter. Keep machine deployment in the dotfiles repository.

Do not move the Niri fork, Moonlight setup, or research clones into this repository.

## More information

- See [UX_SPEC.md](UX_SPEC.md) for interaction rules.
- See [ports/noctalia-v5/README.md](ports/noctalia-v5/README.md) for Noctalia v5 setup.
