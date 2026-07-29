# niri-xmb UX specification

This document defines the intended user experience for the personal desktop profile.
The words **must**, **should**, and **may** state requirement levels.

## Design goals

- The popup must show Niri state without changing window layout.
- Window organization must not move pointer focus to the target output.
- Surfaces outside the panel must keep normal hover, drag, and click behavior.
- Window activation from inside the panel must close the popup with one action.
- Multi-window organization must keep the popup open.
- Missing optional configuration must use documented defaults.
- Invalid configuration must not stop the active daemon.

## Runtime modes

| Mode | Trigger | Pointer input | Keyboard input | Close rule |
| --- | --- | --- | --- | --- |
| Hidden | Startup or explicit hide | None | None | Not applicable |
| Manual | `Mod+Z`, `show`, or `toggle` | Panel only | Yes | Behavior settings control closure |
| Peek | Niri workspace or window navigation | None | None | Hide after `peek.duration_ms` |

Manual mode must take keyboard focus for navigation. Peek mode must remain passive.
Opening manual mode while peek is visible must promote the existing popup.
The desktop profile must show peek for deliberate left/right window focus changes.
Window creation, window removal, and XMB-generated focus changes must not raise peek.
Each qualifying focus move must extend the active peek without hiding it first.
Peek expiry must pause while a focus event burst settles.

## Panel placement and outputs

The desktop profile uses one combined panel. The panel must contain maps for all connected outputs.

| Setting | Desktop value | Required behavior |
| --- | --- | --- |
| `popup.outputs` | `"combined"` | Put all output maps in one panel |
| `popup.surface_mode` | `"fixed"` | Keep panel on its spawn output |
| `popup.position` | `"anchored"` | Use output placement rules |
| Horizontal anchor | `"center"` | Center panel on spawn output |
| Vertical anchor | `"top"` | Pin panel to top edge |

The panel must support `HDMI-A-1`, `HDMI-A-2`, and `Virtual-1`.
An output connected after daemon startup must receive a popup surface without a restart.
An absent virtual output must not create visible empty content.

Set `popup.surface_mode = "fixed"` to keep the panel on its initial output.
Set `popup.surface_mode = "follow_active"` to follow active-output focus after `follow_delay_ms`.

Workspaces must run from top to bottom. Windows must follow Niri column order from left to right.
Panel size must shrink to visible content and stop at configured maximum sizes.

## Pointer behavior

Manual mode must advertise only the visible panel as its pointer input region.
Windows, bars, and docks outside the panel must receive normal pointer motion.
Dock magnification and drag operations must work while the popup is open.

The desktop profile uses these settings:

```toml
close_on_external_focus_change = false
close_on_outside_click = false
replay_outside_click = false
```

External focus changes must keep a manual popup open.
This rule gives hotkey navigation and pass-through clicks the same result.
Use `Escape` or `Mod+Z` to close a manual popup after an outside interaction.

Full-output click capture must remain optional. Click replay cannot reproduce hover or drag motion.
Do not enable capture and replay for the desktop profile.

## Popup lifetime

| Action | Desktop result | Setting |
| --- | --- | --- |
| Click a window icon | Focus window and close | `close_on_window_click = true` |
| Click a workspace | Focus workspace and keep open | `close_on_workspace_click = false` |
| Complete drag and drop | Move window and keep open | `close_on_drop = false` |
| Click outside and change focus | Pass click through and keep open | `close_on_external_focus_change = false` |
| Press `Escape` | Close | `close_on_escape = true` |
| Press `Mod+Z` while open | Close | Toggle command |

## Keyboard navigation

The desktop profile uses `navigation = "deferred"`.
Arrow keys must move only the popup selection until confirmation.
Enter must apply the selected window or workspace action.
The selection must remember its position on each visited workspace during one session.
Closing the popup must clear the staged selection.

The optional `live` mode may move Niri focus on every navigation step.
XMB-generated focus changes must not close the popup as external changes.
External Niri hotkeys must not change manual mode into hidden or peek mode.

## Drag and drop

Pointer travel must exceed `drag_threshold` before a press becomes a drag.
The desktop value is `7.0` logical pixels.

Window drops must preserve tiled mode unless a floating behavior setting requests another result.
The desktop profile uses these values:

```toml
empty_drop_floats = false
floating_drop_tiles = false
focus_after_drop = false
snap_cursor_after_drop = false
```

A tiled window dropped on empty workspace space must remain tiled.
A floating window dropped on a window or workspace must remain floating.
A completed drop must preserve the user's current focus.
The pointer must remain at the release location.

When column joining is disabled, a window-icon target has two zones.
The left half inserts before the target. The right half inserts after the target.
The center must not create a joined Niri column.

## Noctalia taskbar integration

Only HDMI-A-2 must show the desktop top bar.
The bar must contain independent taskbar widgets for these output targets:

| Widget | Target output |
| --- | --- |
| `taskbar-hdmi1` | `HDMI-A-1` |
| `taskbar-hdmi2` | `HDMI-A-2` |
| `taskbar-virtual1` | `Virtual-1` |

Each taskbar widget must call `niri-xmb drop` for window drag and drop.
The `Virtual-1` widget must remain empty when the output is absent.

Noctalia dock item reordering applies only to pinned applications.
The user must hold a pinned icon for 300 milliseconds before moving it.

## Urgency indicators

Urgency means that a Niri window requests attention. It does not mean normal focus or selection.

`style.urgent_indicator` supports these presets:

| Preset | Result |
| --- | --- |
| `"none"` | Hide window urgency indicator |
| `"ring"` | Draw an outline around the icon |
| `"glow"` | Draw a soft radial background glow |
| `"badge"` | Draw a dot at the icon's top-right corner |

The desktop profile uses `urgent_indicator = "badge"`.
The current badge radius is `3.5` logical pixels.
The theme field `style.urgent` controls indicator color.

The indicator must remain visible in Niri overview mode.
The indicator must follow Niri window urgency changes without a daemon restart.

Use these commands to test urgency:

```bash
niri msg windows
niri msg action set-window-urgent --id WINDOW_ID
niri msg action unset-window-urgent --id WINDOW_ID
```

Niri does not set urgency on the focused window. Select a non-focused window ID.

## Configuration resolution

An omitted field must use its Rust `Default` value.
An omitted section must use all defaults for that section.
A missing configuration file must use the complete built-in configuration.

An empty value is not an omitted value. Invalid TOML, a wrong type, or an unknown preset must return an error.
`niri-xmb restart` must validate the configuration before it stops the active daemon.
If validation fails, the active daemon must continue with its previous configuration.

A referenced theme may contain a partial `[style]` table.
Theme fields must override matching fields from the main configuration.
Fields absent from the theme must keep main configuration or built-in values.

## Verification checklist

1. Run `niri-xmb restart` and confirm `config valid`.
2. Open the popup with `Mod+Z`.
3. Move the pointer over the Noctalia dock and confirm magnification works.
4. Click another window and confirm the click works and the popup closes.
5. Drag a window icon between workspaces and confirm tiled mode remains active.
6. Confirm the pointer and focus stay at the interaction source after the drop.
7. Connect `Virtual-1` and confirm its map and taskbar widget appear.
8. Set urgency on a non-focused window and confirm the configured indicator appears.
