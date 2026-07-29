mod anim;
mod config;
mod draw;
mod icons;
mod ipc;
mod model;
mod niri;
mod render;
mod text;
mod wayland;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use config::{BarConfig, Config, NavigationMode, PopupOutputs, PopupPosition, PopupSurfaceMode};
use crossbeam_channel::unbounded;
use ipc::{ControlCommand, ControlRequest};
use model::Snapshot;
use niri::{NiriCommand, spawn as spawn_niri};
use niri_ipc::socket::Socket;
use niri_ipc::{Request, Response};
use render::{DragVisual, HitTarget, Renderer, SelectionVisual};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Command as ProcessCommand;
use std::time::{Duration, Instant};
use wayland::{SurfaceKind, SurfaceManager, UiEvent, UiEventKind, WakePipe};

/// How long after niri-xmb moves focus itself that the resulting snapshot is
/// treated as our own echo rather than as external navigation.
const SELF_FOCUS_GRACE: Duration = Duration::from_millis(400);

/// How long focus must hold still before deciding whether to raise the overlay.
///
/// One user action reaches us as several events — a monitor change activates a
/// workspace and then focuses a window inside it; closing a window reports the
/// focus change before the removal. Judging each event as it arrives therefore
/// misreads half of them. Waiting for the burst to settle costs an
/// imperceptible delay on an overlay that then lingers for a second or more.
const FOCUS_SETTLE: Duration = Duration::from_millis(120);

/// How long a window counts as newly appeared.
///
/// Focus landing on a window this young is that window starting up and taking
/// focus, not the user navigating to it. A settle window alone is not enough:
/// an app can map its surface and only claim focus a second later, well after
/// any fixed grace period, and the focus event on its own is indistinguishable
/// from navigation unless the window's age is known.
const NEW_WINDOW_GRACE: Duration = Duration::from_millis(1200);

const BTN_LEFT: u32 = 272;
const KEY_ESCAPE: u32 = 0xff1b;
const KEY_RETURN: u32 = 0xff0d;
const KEY_LEFT: u32 = 0xff51;
const KEY_UP: u32 = 0xff52;
const KEY_RIGHT: u32 = 0xff53;
const KEY_DOWN: u32 = 0xff54;
const KEY_TAB: u32 = 0xff09;
const KEY_SHIFT_TAB: u32 = 0xfe20;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    Daemon,
    Show,
    Hide,
    Toggle,
    Status,
    Snapshot {
        #[arg(long)]
        output: Option<String>,
    },
    FocusWindow {
        #[arg(long)]
        window_id: u64,
    },
    FocusWorkspace {
        #[arg(long)]
        workspace_id: u64,
    },
    CycleWorkspace {
        #[arg(long)]
        output: String,
        #[arg(long)]
        delta: i32,
    },
    Drop {
        #[arg(long)]
        source_window_id: u64,
        #[arg(long)]
        target_workspace_id: u64,
        #[arg(long)]
        target_window_id: Option<u64>,
        #[arg(long, default_value_t = 0.5)]
        fraction: f32,
        #[arg(long)]
        cursor_x: Option<f64>,
        #[arg(long)]
        cursor_y: Option<f64>,
    },
    Quit,
    /// Validate the config, gracefully stop the daemon, and relaunch through Niri.
    Restart,
    /// Print the fully commented configuration template.
    PrintDefaultConfig,
}

/// Why the popup is currently on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PopupMode {
    Hidden,
    /// Opened by the user; takes input and stages a selection.
    Manual,
    /// Opened by itself to mirror Niri navigation; takes no input.
    Peek,
}

#[derive(Debug, Clone)]
struct PressState {
    window_id: u64,
    app_id: String,
    start_x: f64,
    start_y: f64,
    current_x: f64,
    current_y: f64,
    dragging: bool,
}

#[derive(Debug, Clone, Default)]
struct Selection {
    output: String,
    workspace: usize,
    window: usize,
    /// Where the cursor was left in each workspace, keyed by workspace id.
    ///
    /// The popup is a staging area, not a live view: navigating away from a
    /// workspace and back must return to where you were, not to whatever Niri
    /// is focusing right now. Discarded when the popup closes or a move is
    /// confirmed, so a fresh popup always starts from live state.
    visited: HashMap<u64, usize>,
}

impl Selection {
    /// Record the current position so returning to this workspace restores it.
    fn remember(&mut self, snapshot: &Snapshot) {
        if let Some(workspace) = selected_workspace(self, snapshot) {
            self.visited.insert(workspace, self.window);
        }
    }

    /// Where to land when entering `workspace`: the remembered position if
    /// this popup session has one, otherwise the window Niri would focus.
    fn entry_index(&self, workspace: &model::WorkspaceModel) -> usize {
        let count = workspace.windows().count();
        match self.visited.get(&workspace.id) {
            Some(&remembered) if remembered < count => remembered,
            _ => workspace.active_index(),
        }
    }

    /// End the staging session. The next popup starts from live state again.
    fn clear_session(&mut self) {
        self.visited.clear();
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "niri_xmb=info".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Daemon) {
        Command::Daemon => run_daemon(cli.config.unwrap_or_else(Config::default_path)),
        Command::Show => print_control(ControlCommand::Show),
        Command::Hide => print_control(ControlCommand::Hide),
        Command::Toggle => print_control(ControlCommand::Toggle),
        Command::Status => print_control(ControlCommand::Status),
        Command::Snapshot { output } => print_control(ControlCommand::Snapshot { output }),
        Command::FocusWindow { window_id } => {
            print_control(ControlCommand::FocusWindow { window_id })
        }
        Command::FocusWorkspace { workspace_id } => {
            print_control(ControlCommand::FocusWorkspace { workspace_id })
        }
        Command::CycleWorkspace { output, delta } => {
            print_control(ControlCommand::CycleWorkspace { output, delta })
        }
        Command::Drop {
            source_window_id,
            target_workspace_id,
            target_window_id,
            fraction,
            cursor_x,
            cursor_y,
        } => print_control(ControlCommand::Drop {
            source_window_id,
            target_workspace_id,
            target_window_id,
            fraction: fraction.clamp(0.0, 1.0),
            cursor_x,
            cursor_y,
        }),
        Command::Quit => print_control(ControlCommand::Quit),
        Command::Restart => restart_daemon(cli.config.unwrap_or_else(Config::default_path)),
        Command::PrintDefaultConfig => {
            print!("{}", include_str!("../../../config/niri-xmb.toml.example"));
            Ok(())
        }
    }
}

fn print_control(command: ControlCommand) -> Result<()> {
    print!("{}", ipc::send(command)?);
    Ok(())
}

fn restart_daemon(config_path: PathBuf) -> Result<()> {
    let config_path = if config_path.is_absolute() {
        config_path
    } else {
        std::env::current_dir()
            .context("failed to resolve the config path")?
            .join(config_path)
    };

    Config::load(&config_path)
        .with_context(|| "config validation failed; the running daemon was not stopped")?;
    println!("config valid: {}", config_path.display());

    if daemon_status().is_some() {
        let response = ipc::send(ControlCommand::Quit).context("failed to stop the daemon")?;
        if response.trim() != "ok" {
            anyhow::bail!("daemon rejected restart: {}", response.trim());
        }
        if wait_for_daemon(false, Duration::from_secs(3)).is_none() {
            anyhow::bail!("daemon did not stop within 3 seconds");
        }
        println!("daemon stopped");
    } else {
        println!("daemon was not running");
    }

    let executable = std::env::current_exe().context("failed to locate niri-xmb executable")?;
    let output = ProcessCommand::new("niri")
        .args(["msg", "action", "spawn", "--"])
        .arg(&executable)
        .arg("--config")
        .arg(&config_path)
        .arg("daemon")
        .output()
        .context("failed to ask Niri to launch the daemon")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Niri rejected daemon launch: {}", stderr.trim());
    }

    let Some(status) = wait_for_daemon(true, Duration::from_secs(5)) else {
        anyhow::bail!("daemon did not become ready within 5 seconds");
    };
    println!("daemon started");
    println!("{}", status.trim());
    Ok(())
}

/// Wait for the control socket to enter the requested state. When waiting for
/// startup, return the first healthy status response. When waiting for
/// shutdown, return a marker once no healthy daemon answers.
fn wait_for_daemon(running: bool, timeout: Duration) -> Option<String> {
    let deadline = Instant::now() + timeout;
    loop {
        let status = daemon_status();
        if running {
            if status.is_some() {
                return status;
            }
        } else if status.is_none() {
            return Some(String::new());
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn daemon_status() -> Option<String> {
    ipc::send(ControlCommand::Status)
        .ok()
        .filter(|response| !response.trim_start().starts_with("error:"))
}

fn snapshot_json(snapshot: &Snapshot, output_filter: Option<&str>) -> String {
    let outputs = snapshot
        .outputs
        .values()
        .filter(|output| output_filter.is_none_or(|name| output.name == name))
        .map(|output| {
            let workspaces = output
                .workspaces
                .iter()
                .map(|workspace| {
                    let windows = workspace
                        .columns
                        .iter()
                        .flat_map(|(column, rows)| {
                            rows.iter().map(move |(row, window)| {
                                serde_json::json!({
                                    "id": window.id,
                                    "workspace_id": window.workspace_id,
                                    "app_id": window.app_id,
                                    "title": window.title,
                                    "focused": window.focused,
                                    "urgent": window.urgent,
                                    "floating": false,
                                    "column": column,
                                    "row": row,
                                    "tile_width": window.tile_width,
                                    "tile_height": window.tile_height,
                                    "x": window.x,
                                    "y": window.y,
                                })
                            })
                        })
                        .chain(workspace.floating.iter().map(|window| {
                            serde_json::json!({
                                "id": window.id,
                                "workspace_id": window.workspace_id,
                                "app_id": window.app_id,
                                "title": window.title,
                                "focused": window.focused,
                                "urgent": window.urgent,
                                "floating": true,
                                "column": null,
                                "row": null,
                                "tile_width": window.tile_width,
                                "tile_height": window.tile_height,
                                "x": window.x,
                                "y": window.y,
                            })
                        }))
                        .collect::<Vec<_>>();
                    serde_json::json!({
                        "id": workspace.id,
                        "index": workspace.index,
                        "name": workspace.name,
                        "label": workspace.label(),
                        "active": workspace.active,
                        "focused": workspace.focused,
                        "urgent": workspace.urgent,
                        "windows": windows,
                    })
                })
                .collect::<Vec<_>>();
            serde_json::json!({
                "name": output.name,
                "x": output.x,
                "y": output.y,
                "width": output.width,
                "height": output.height,
                "workspaces": workspaces,
            })
        })
        .collect::<Vec<_>>();

    serde_json::json!({
        "generation": snapshot.generation,
        "focused_window": snapshot.focused_window,
        "outputs": outputs,
    })
    .to_string()
        + "\n"
}

fn run_daemon(config_path: PathBuf) -> Result<()> {
    let mut config = Config::load(&config_path)?;
    let output_names = connected_outputs()?;
    if output_names.is_empty() {
        anyhow::bail!("Niri reported no connected outputs");
    }
    if config.bars_enabled && config.bars.is_empty() {
        config.bars = output_names
            .iter()
            .map(|output| BarConfig {
                output: output.clone(),
                ..BarConfig::default()
            })
            .collect();
    }
    config.bars.retain(|bar| {
        if output_names.contains(&bar.output) {
            true
        } else {
            tracing::warn!(output = %bar.output, "configured bar output is not connected");
            false
        }
    });

    let (snapshot_tx, snapshot_rx) = unbounded();
    let (niri_command_tx, niri_command_rx) = unbounded();
    let (cursor_restore_tx, cursor_restore_rx) = unbounded();
    let wake = WakePipe::new()?;
    spawn_niri(
        snapshot_tx,
        niri_command_rx,
        cursor_restore_tx,
        wake.handle(),
    );

    let (ui_tx, ui_rx) = unbounded();
    let surface_bars = if config.bars_enabled {
        config.bars.clone()
    } else {
        Vec::new()
    };
    let mut surfaces = SurfaceManager::new(&surface_bars, &output_names, ui_tx)
        .context("failed to initialize layer-shell surfaces")?;
    let mut renderer = Renderer::new();

    let (control_tx, control_rx) = unbounded::<ControlRequest>();
    ipc::serve(control_tx, wake.handle())?;

    let mut snapshot = wait_for_snapshot(&snapshot_rx).unwrap_or_default();
    let mut popup_outputs = HashSet::new();
    let mut popup_origin_focus: Option<u64> = None;
    let mut follow_candidate: Option<(String, Instant)> = None;
    let mut press: Option<PressState> = None;
    let mut selection = Selection::default();
    let mut popup_was_open = false;
    let mut popup_mode = PopupMode::Hidden;
    let mut peek_deadline: Option<Instant> = None;
    // Focus changes we caused ourselves must not bounce back as a peek.
    let mut peek_suppressed_until = Instant::now();
    let mut window_first_seen: HashMap<u64, Instant> = HashMap::new();
    // Focus state as last observed, versus as last acted on. The gap between
    // them is the burst of events still settling.
    let mut observed: Option<FocusMark> = None;
    let mut acted_focus: Option<FocusMark> = None;
    let mut acted_windows: Option<HashSet<u64>> = None;
    let mut pending_focus_since: Option<Instant> = None;
    let mut running = true;
    let mut dirty = true;

    let mut wait_timeout = Duration::ZERO;
    tracing::info!(
        config = %config_path.display(),
        background = %config.style.background,
        accent = %config.style.accent,
        "niri-xmb daemon started"
    );
    while running {
        surfaces.dispatch(&wake, wait_timeout)?;

        while let Ok((x, y)) = cursor_restore_rx.try_recv() {
            if !surfaces.restore_pointer(x, y) {
                tracing::warn!("cursor restore requested but virtual-pointer is unavailable");
            }
        }

        while let Ok(next) = snapshot_rx.try_recv() {
            snapshot = next;
            normalize_selection(&mut selection, &snapshot);
            let now = Instant::now();
            let windows = live_window_ids(&snapshot);
            for id in &windows {
                window_first_seen.entry(*id).or_insert(now);
            }
            window_first_seen.retain(|id, _| windows.contains(id));

            // Re-syncing the panel to live state is safe to do immediately.
            // The manual panel always mirrors complete live focus, including
            // left/right and stacked-window changes. Peek granularity is a
            // separate concern handled after the event burst settles.
            let signature = focus_signature(&snapshot, true);
            let changed = observed
                .replace(signature)
                .is_some_and(|previous| previous != *observed.as_ref().unwrap());
            if changed {
                if should_close_on_external_focus_change(
                    &config,
                    popup_mode,
                    now,
                    peek_suppressed_until,
                ) {
                    surfaces.hide_all_popups()?;
                    popup_outputs.clear();
                    popup_mode = PopupMode::Hidden;
                    popup_origin_focus = None;
                    follow_candidate = None;
                    press = None;
                }
                if Instant::now() >= peek_suppressed_until {
                    // Whatever the popup is showing, it now shows where Niri
                    // actually is. Peek and the interactive panel behave as one
                    // thing here: an external move re-syncs both, so the panel
                    // never sits on a stale workspace while Niri has moved on.
                    selection.clear_session();
                    reset_selection(&mut selection, &snapshot);
                }
                // Whether to *raise* an overlay is decided only once focus has
                // settled. Niri reports one user action as a burst of events in
                // an order that cannot be relied on — closing a window emits
                // the focus change before the window removal, so judging each
                // event as it arrives mistakes the fallback focus for
                // navigation. Waiting for the burst to finish and then
                // comparing against the last state we acted on removes the
                // whole class of ordering bug.
                pending_focus_since = Some(now);
            }
            dirty = true;
        }

        if let Some(since) = pending_focus_since {
            let now = Instant::now();
            if now.duration_since(since) >= FOCUS_SETTLE {
                pending_focus_since = None;
                let signature = focus_signature(&snapshot, config.peek.on_window_focus);
                let windows = live_window_ids(&snapshot);
                let movement = acted_focus
                    .as_ref()
                    .map(|previous| classify_focus_move(previous, &signature))
                    .unwrap_or(FocusMove::None);
                // A window appearing or disappearing moves focus without the
                // user navigating anywhere.
                let lifecycle = acted_windows
                    .as_ref()
                    .is_some_and(|previous| *previous != windows);
                // A slow-starting app can map its surface and only claim focus
                // a second later, long after the burst settled, so window age
                // is checked separately from the lifecycle comparison.
                let focused_is_new = snapshot
                    .focused_window
                    .and_then(|id| window_first_seen.get(&id))
                    .is_some_and(|seen| now.duration_since(*seen) < NEW_WINDOW_GRACE);

                if movement == FocusMove::WithinOutput
                    && !lifecycle
                    && !focused_is_new
                    && now >= peek_suppressed_until
                {
                    peek_deadline = trigger_peek(
                        &mut surfaces,
                        &snapshot,
                        &config,
                        &mut popup_outputs,
                        &mut popup_mode,
                        &mut selection,
                        peek_suppressed_until,
                    )?;
                    dirty = true;
                }
                acted_focus = Some(signature);
                acted_windows = Some(windows);
            }
        }
        // Guarded on Peek: a manual session must never inherit this expiry.
        // The overview opening counts as an expiry, so an overlay already up
        // gets out of the way immediately.
        if popup_mode == PopupMode::Peek
            && (snapshot.overview_open
                || peek_deadline_reached(peek_deadline, pending_focus_since, Instant::now()))
        {
            surfaces.hide_all_popups()?;
            popup_outputs.clear();
            popup_mode = PopupMode::Hidden;
            peek_deadline = None;
            dirty = true;
        }

        while let Ok(request) = control_rx.try_recv() {
            let response = match request.command {
                ControlCommand::Show => {
                    if popup_mode != PopupMode::Manual {
                        popup_origin_focus = snapshot.focused_window;
                    }
                    show_popup(&mut surfaces, &snapshot, &config, &mut popup_outputs, true)?;
                    popup_mode = PopupMode::Manual;
                    // Taking control cancels the peek countdown.
                    peek_deadline = None;
                    follow_candidate = None;
                    reset_selection(&mut selection, &snapshot);
                    dirty = true;
                    "ok\n".into()
                }
                ControlCommand::Hide => {
                    surfaces.hide_all_popups()?;
                    popup_outputs.clear();
                    popup_mode = PopupMode::Hidden;
                    popup_origin_focus = None;
                    follow_candidate = None;
                    press = None;
                    dirty = true;
                    "ok\n".into()
                }
                ControlCommand::Toggle => {
                    // A peek overlay is promoted rather than dismissed: the
                    // keybind means "let me drive this", not "hide it".
                    if popup_mode == PopupMode::Manual {
                        surfaces.hide_all_popups()?;
                        popup_outputs.clear();
                        popup_mode = PopupMode::Hidden;
                        popup_origin_focus = None;
                        follow_candidate = None;
                        press = None;
                    } else {
                        popup_origin_focus = snapshot.focused_window;
                        show_popup(&mut surfaces, &snapshot, &config, &mut popup_outputs, true)?;
                        popup_mode = PopupMode::Manual;
                        peek_deadline = None;
                        follow_candidate = None;
                        reset_selection(&mut selection, &snapshot);
                    }
                    dirty = true;
                    "ok\n".into()
                }
                ControlCommand::Status => {
                    serde_json::json!({
                        "visible": !popup_outputs.is_empty(),
                        "mode": match popup_mode {
                            PopupMode::Hidden => "hidden",
                            PopupMode::Manual => "manual",
                            PopupMode::Peek => "peek",
                        },
                        "selection": selection_visual(&selection, &snapshot).map(|visual| {
                            serde_json::json!({
                                "output": visual.output,
                                "workspace_id": visual.workspace_id,
                                "window_id": visual.window_id,
                            })
                        }),
                        "outputs": popup_outputs,
                        "generation": snapshot.generation,
                        "socket": ipc::socket_path(),
                        "cursor_restore": surfaces.cursor_restore_available(),
                    })
                    .to_string()
                        + "\n"
                }
                ControlCommand::Snapshot { output } => snapshot_json(&snapshot, output.as_deref()),
                ControlCommand::FocusWindow { window_id } => {
                    if snapshot.window(window_id).is_none() {
                        "error: unknown window\n".into()
                    } else {
                        // IPC focus commands come from external controls such as
                        // the Noctalia taskbar. Mark them as self-focus before
                        // the worker can deliver the focus event back to us.
                        peek_suppressed_until = Instant::now() + SELF_FOCUS_GRACE;
                        if niri_command_tx
                            .send(NiriCommand::FocusWindow(window_id))
                            .is_ok()
                        {
                            "ok\n".into()
                        } else {
                            "error: Niri command worker unavailable\n".into()
                        }
                    }
                }
                ControlCommand::FocusWorkspace { workspace_id } => {
                    if snapshot.workspace(workspace_id).is_none() {
                        "error: unknown workspace\n".into()
                    } else {
                        peek_suppressed_until = Instant::now() + SELF_FOCUS_GRACE;
                        if niri_command_tx
                            .send(NiriCommand::FocusWorkspace(workspace_id))
                            .is_ok()
                        {
                            "ok\n".into()
                        } else {
                            "error: Niri command worker unavailable\n".into()
                        }
                    }
                }
                ControlCommand::CycleWorkspace { output, delta } => {
                    let target = snapshot.outputs.get(&output).and_then(|model| {
                        if model.workspaces.is_empty() {
                            return None;
                        }
                        let current = model
                            .workspaces
                            .iter()
                            .position(|workspace| workspace.active)
                            .unwrap_or(0);
                        let len = model.workspaces.len() as i64;
                        let next = (current as i64 + i64::from(delta)).rem_euclid(len) as usize;
                        Some(model.workspaces[next].id)
                    });
                    match target {
                        Some(workspace_id) => {
                            peek_suppressed_until = Instant::now() + SELF_FOCUS_GRACE;
                            if niri_command_tx
                                .send(NiriCommand::FocusWorkspace(workspace_id))
                                .is_ok()
                            {
                                "ok\n".into()
                            } else {
                                "error: Niri command worker unavailable\n".into()
                            }
                        }
                        None => "error: unknown output or no workspaces\n".into(),
                    }
                }
                ControlCommand::Drop {
                    source_window_id,
                    target_workspace_id,
                    target_window_id,
                    fraction,
                    cursor_x,
                    cursor_y,
                } => {
                    let output = snapshot.outputs.values().find(|output| {
                        output
                            .workspaces
                            .iter()
                            .any(|workspace| workspace.id == target_workspace_id)
                    });
                    let target = output.map(|output| {
                        target_window_id
                            .and_then(|window_id| snapshot.window(window_id))
                            .map(|window| HitTarget::Window {
                                output: output.name.clone(),
                                workspace_id: target_workspace_id,
                                window_id: window.id,
                                column: window.column,
                                floating: window.floating,
                                rect: render::Rect {
                                    x: 0.0,
                                    y: 0.0,
                                    width: 1.0,
                                    height: 1.0,
                                },
                                canvas: None,
                            })
                            .unwrap_or_else(|| HitTarget::Canvas {
                                output: output.name.clone(),
                                workspace_id: target_workspace_id,
                                rect: render::Rect {
                                    x: 0.0,
                                    y: 0.0,
                                    width: 1.0,
                                    height: 1.0,
                                },
                            })
                    });
                    let dropped = target.is_some_and(|target| {
                        dispatch_drop(
                            source_window_id,
                            &target,
                            fraction as f64,
                            0.5,
                            &snapshot,
                            &config,
                            None,
                            if config.behavior.snap_cursor_after_drop {
                                None
                            } else {
                                cursor_x
                                    .zip(cursor_y)
                                    .filter(|(x, y)| x.is_finite() && y.is_finite())
                            },
                            &niri_command_tx,
                        )
                    });
                    if dropped {
                        "ok\n".into()
                    } else {
                        "error: drop rejected\n".into()
                    }
                }
                ControlCommand::Quit => {
                    running = false;
                    "ok\n".into()
                }
            };
            let _ = request.reply.send(response);
        }

        while let Ok(event) = ui_rx.try_recv() {
            dirty |= handle_ui_event(
                event,
                &mut surfaces,
                &mut renderer,
                &snapshot,
                &config,
                &niri_command_tx,
                &mut popup_outputs,
                popup_origin_focus,
                &mut press,
                &mut selection,
                &mut peek_suppressed_until,
            )?;
        }

        // Only the manual popup uses the delayed follow. A peek overlay is
        // re-placed on the focused output the moment focus moves, and routing
        // it through here would re-show it as an input-grabbing surface.
        if popup_mode == PopupMode::Manual {
            dirty |= update_following_popup(
                &mut surfaces,
                &snapshot,
                &config,
                &mut popup_outputs,
                press.is_some(),
                &mut follow_candidate,
                &mut selection,
            )?;
        }

        // Catch every way the popup can close — Escape, outside click, a
        // confirmed pick, IPC — in one place, and end the staging session.
        let popup_open = !popup_outputs.is_empty();
        if popup_was_open && !popup_open {
            selection.clear_session();
            popup_mode = PopupMode::Hidden;
            peek_deadline = None;
            // Confirming a pick moves Niri's focus, which would otherwise come
            // straight back as a peek for the move just made.
            peek_suppressed_until = Instant::now() + SELF_FOCUS_GRACE;
        }
        popup_was_open = popup_open;

        // Animation targets are recomputed every iteration, not only on
        // redraw, so a settled UI stops requesting frames entirely.
        let selection_visual = selection_visual(&selection, &snapshot);
        let animating = renderer.update_animations(
            &snapshot,
            &config,
            selection_visual.as_ref(),
            &popup_outputs,
        );
        dirty |= animating;

        if dirty {
            let surface_infos = surfaces.surfaces();
            let all_ready = surface_infos
                .iter()
                .all(|surface| surface.configured && surface.frame_ready);
            if all_ready {
                let drag = press
                    .as_ref()
                    .filter(|press| press.dragging)
                    .map(|press| DragVisual {
                        window_id: press.window_id,
                        app_id: press.app_id.clone(),
                        global_x: press.current_x,
                        global_y: press.current_y,
                    });
                for surface in &surface_infos {
                    // A None result means the surface is already blank.
                    if let Some(frame) = renderer.render(
                        surface,
                        &snapshot,
                        &config,
                        drag.as_ref(),
                        selection_visual.as_ref(),
                    ) {
                        surfaces.present(surface.id, frame.pixels, frame.damage)?;
                    }
                    if surface.kind == SurfaceKind::Popup
                        && popup_mode == PopupMode::Manual
                        && !config.behavior.close_on_outside_click
                        && let Some(rect) =
                            renderer.panel_input_rect(surface.id, surface.width, surface.height)
                    {
                        surfaces.set_popup_input_rect(surface.id, rect)?;
                    }
                }
                dirty = false;
            }
        }
        // Wayland frame callbacks and producer wakeups drive the loop. Timers
        // only provide deadlines for animation/focus/peek state and a safety
        // fallback; settled input does not burn CPU in a fixed sleep loop.
        let now = Instant::now();
        wait_timeout = if animating || dirty {
            Duration::from_millis(16)
        } else {
            Duration::from_millis(250)
        };
        if let Some(since) = pending_focus_since {
            wait_timeout =
                wait_timeout.min(FOCUS_SETTLE.saturating_sub(now.saturating_duration_since(since)));
        }
        if let Some(deadline) = peek_deadline {
            wait_timeout = wait_timeout.min(deadline.saturating_duration_since(now));
        }
        if let Some((_, since)) = &follow_candidate {
            let delay = Duration::from_millis(config.popup.follow_delay_ms);
            wait_timeout =
                wait_timeout.min(delay.saturating_sub(now.saturating_duration_since(*since)));
        }
    }

    let result = surfaces.hide_all_popups();
    ipc::cleanup();
    result
}

fn connected_outputs() -> Result<Vec<String>> {
    let mut socket = Socket::connect().context("NIRI_SOCKET is unavailable")?;
    match socket.send(Request::Outputs)? {
        Ok(Response::Outputs(outputs)) => {
            let mut names: Vec<_> = outputs.into_keys().collect();
            names.sort();
            Ok(names)
        }
        Ok(response) => anyhow::bail!("unexpected Niri response: {response:?}"),
        Err(error) => anyhow::bail!(error),
    }
}

fn wait_for_snapshot(receiver: &crossbeam_channel::Receiver<Snapshot>) -> Option<Snapshot> {
    receiver.recv_timeout(Duration::from_secs(2)).ok()
}

/// Ids of every window currently mapped, across all outputs.
fn live_window_ids(snapshot: &Snapshot) -> HashSet<u64> {
    snapshot
        .outputs
        .values()
        .flat_map(|output| &output.workspaces)
        .flat_map(|workspace| workspace.windows())
        .map(|window| window.id)
        .collect()
}

/// Where Niri's focus currently is, at the granularity peek cares about.
type FocusMark = (Option<String>, Option<u64>, Option<u64>);

/// What kind of focus movement a new snapshot represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FocusMove {
    None,
    /// Navigation inside one monitor: a workspace or window change.
    WithinOutput,
    /// Focus landed on a different monitor.
    AcrossOutputs,
}

fn classify_focus_move(previous: &FocusMark, current: &FocusMark) -> FocusMove {
    if previous == current {
        FocusMove::None
    } else if previous.0 != current.0 {
        FocusMove::AcrossOutputs
    } else {
        FocusMove::WithinOutput
    }
}

fn focus_signature(snapshot: &Snapshot, include_window: bool) -> FocusMark {
    let output = snapshot.focused_output().map(str::to_string);
    let workspace = snapshot
        .outputs
        .values()
        .flat_map(|output| &output.workspaces)
        .find(|workspace| workspace.focused)
        .map(|workspace| workspace.id);
    let window = include_window.then_some(snapshot.focused_window).flatten();
    (output, workspace, window)
}

/// Whether a focus change should be ignored rather than raising a peek overlay.
///
/// Peek stays out of the way of anything that already covers the same ground:
/// a session the user is driving, and Niri's own full-screen overview.
fn peek_should_skip(config: &Config, snapshot: &Snapshot, mode: PopupMode) -> bool {
    !config.peek.enabled || mode == PopupMode::Manual || snapshot.overview_open
}

/// Keep an existing peek visible while a focus event burst settles.
///
/// The settled event can extend the peek deadline. Hiding during the settle
/// window causes a visible close-and-reopen flicker between hotkey moves.
fn peek_deadline_reached(
    deadline: Option<Instant>,
    pending_focus_since: Option<Instant>,
    now: Instant,
) -> bool {
    pending_focus_since.is_none() && deadline.is_some_and(|deadline| now >= deadline)
}

/// Raise (or extend) the passive peek overlay after Niri focus moved.
///
/// Returns the new expiry, or `None` when peek should not be showing. A manual
/// popup always wins: peek never steals a session the user is driving.
#[allow(clippy::too_many_arguments)]
fn trigger_peek(
    surfaces: &mut SurfaceManager,
    snapshot: &Snapshot,
    config: &Config,
    visible: &mut HashSet<String>,
    mode: &mut PopupMode,
    selection: &mut Selection,
    suppressed_until: Instant,
) -> Result<Option<Instant>> {
    if peek_should_skip(config, snapshot, *mode) || Instant::now() < suppressed_until {
        return Ok(None);
    }
    // Showing a surface costs a blocking Wayland roundtrip, so while already
    // peeking only re-show when focus actually moved to a different monitor.
    let targets = popup_output_targets(snapshot, config);
    let same_outputs =
        targets.len() == visible.len() && targets.iter().all(|output| visible.contains(output));
    if *mode == PopupMode::Hidden || !same_outputs {
        show_popup(surfaces, snapshot, config, visible, false)?;
        *mode = PopupMode::Peek;
    }
    reset_selection(selection, snapshot);
    Ok(Some(
        Instant::now() + Duration::from_millis(config.peek.duration_ms.max(1)),
    ))
}

/// Outputs the popup should occupy for the current focus and configuration.
fn popup_output_targets(snapshot: &Snapshot, config: &Config) -> Vec<String> {
    match (config.popup.outputs, config.popup.surface_mode) {
        (PopupOutputs::All, _) | (_, PopupSurfaceMode::Mirror) => {
            snapshot.outputs.keys().cloned().collect::<Vec<_>>()
        }
        (PopupOutputs::Focused | PopupOutputs::Combined, _) => snapshot
            .focused_output()
            .map(|output| vec![output.to_string()])
            .or_else(|| {
                snapshot
                    .outputs
                    .keys()
                    .next()
                    .map(|output| vec![output.clone()])
            })
            .unwrap_or_default(),
    }
}

/// Capture outside clicks only when niri-xmb can preserve their action.
fn should_capture_outside_click(config: &Config, virtual_pointer_available: bool) -> bool {
    config.behavior.close_on_outside_click
        && (!config.behavior.replay_outside_click || virtual_pointer_available)
}

fn should_close_on_external_focus_change(
    config: &Config,
    popup_mode: PopupMode,
    now: Instant,
    self_focus_suppressed_until: Instant,
) -> bool {
    config.behavior.close_on_external_focus_change
        && popup_mode == PopupMode::Manual
        && now >= self_focus_suppressed_until
}

fn show_popup(
    surfaces: &mut SurfaceManager,
    snapshot: &Snapshot,
    config: &Config,
    visible: &mut HashSet<String>,
    interactive: bool,
) -> Result<()> {
    let outputs = popup_output_targets(snapshot, config);
    let capture_outside = should_capture_outside_click(config, surfaces.cursor_restore_available());
    surfaces.show_popups_with(&outputs, interactive, capture_outside)?;
    visible.clear();
    visible.extend(outputs);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn update_following_popup(
    surfaces: &mut SurfaceManager,
    snapshot: &Snapshot,
    config: &Config,
    visible: &mut HashSet<String>,
    pointer_active: bool,
    candidate: &mut Option<(String, Instant)>,
    selection: &mut Selection,
) -> Result<bool> {
    if visible.is_empty()
        || config.popup.surface_mode != PopupSurfaceMode::FollowActive
        || config.popup.outputs == PopupOutputs::All
        || pointer_active
    {
        *candidate = None;
        return Ok(false);
    }

    let Some(active_output) = snapshot.focused_output().map(str::to_string) else {
        *candidate = None;
        return Ok(false);
    };
    if visible.contains(&active_output) {
        *candidate = None;
        return Ok(false);
    }

    let now = Instant::now();
    match candidate {
        Some((output, since)) if output == &active_output => {
            if now.duration_since(*since) < Duration::from_millis(config.popup.follow_delay_ms) {
                return Ok(false);
            }
        }
        _ => {
            *candidate = Some((active_output, now));
            return Ok(false);
        }
    }

    let active_output = candidate
        .take()
        .map(|(output, _)| output)
        .unwrap_or(active_output);
    let capture_outside = should_capture_outside_click(config, surfaces.cursor_restore_available());
    surfaces.show_popups(std::slice::from_ref(&active_output), capture_outside)?;
    visible.clear();
    visible.insert(active_output.clone());
    selection.output = active_output;
    select_active_workspace(selection, snapshot);
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
fn handle_ui_event(
    event: UiEvent,
    surfaces: &mut SurfaceManager,
    renderer: &mut Renderer,
    snapshot: &Snapshot,
    config: &Config,
    niri_tx: &crossbeam_channel::Sender<NiriCommand>,
    popup_outputs: &mut HashSet<String>,
    popup_origin_focus: Option<u64>,
    press: &mut Option<PressState>,
    selection: &mut Selection,
    suppress_peek: &mut Instant,
) -> Result<bool> {
    if matches!(event.kind, SurfaceKind::Popup)
        && config.popup.position == PopupPosition::Cursor
        && matches!(event.event, UiEventKind::Enter)
    {
        renderer.set_popup_cursor_origin(&event.output, event.x, event.y);
        return Ok(true);
    }

    match event.event {
        UiEventKind::Press { button } if button == BTN_LEFT => {
            let target = renderer
                .hit_test(event.surface_id, event.x, event.y)
                .cloned();
            if let Some(target) = target {
                match target {
                    HitTarget::Window { window_id, .. } => {
                        if let Some(window) = snapshot.window(window_id) {
                            *press = Some(PressState {
                                window_id,
                                app_id: window.app_id.clone(),
                                start_x: event.global_x,
                                start_y: event.global_y,
                                current_x: event.global_x,
                                current_y: event.global_y,
                                dragging: false,
                            });
                        }
                    }
                    HitTarget::Workspace { workspace_id, .. } => {
                        let _ = niri_tx.send(NiriCommand::FocusWorkspace(workspace_id));
                        *suppress_peek = Instant::now() + SELF_FOCUS_GRACE;
                        if matches!(event.kind, SurfaceKind::Popup)
                            && config.behavior.close_on_workspace_click
                        {
                            surfaces.hide_all_popups()?;
                            popup_outputs.clear();
                        }
                    }
                    HitTarget::Canvas { .. } | HitTarget::Panel => {}
                }
            } else if matches!(event.kind, SurfaceKind::Popup)
                && config.behavior.close_on_outside_click
            {
                surfaces.hide_all_popups()?;
                popup_outputs.clear();
                if config.behavior.replay_outside_click {
                    surfaces.replay_pointer_click(event.global_x, event.global_y, button);
                }
            }
            Ok(true)
        }
        UiEventKind::Motion => {
            if let Some(press) = press {
                press.current_x = event.global_x;
                press.current_y = event.global_y;
                let dx = press.current_x - press.start_x;
                let dy = press.current_y - press.start_y;
                if dx.hypot(dy) >= config.behavior.drag_threshold {
                    press.dragging = true;
                }
                return Ok(true);
            }
            Ok(renderer.update_hover(event.surface_id, event.x, event.y))
        }
        UiEventKind::Enter => Ok(renderer.update_hover(event.surface_id, event.x, event.y)),
        UiEventKind::Leave => Ok(renderer.clear_hover(event.surface_id)),
        UiEventKind::Release { button } if button == BTN_LEFT => {
            let Some(released) = press.take() else {
                return Ok(false);
            };
            if released.dragging {
                let surface_infos = surfaces.surfaces();
                if let Some((_surface, target, local_x, local_y)) =
                    renderer.hit_test_global(&surface_infos, released.current_x, released.current_y)
                {
                    let dropped = dispatch_drop(
                        released.window_id,
                        target,
                        local_x,
                        local_y,
                        snapshot,
                        config,
                        if popup_outputs.is_empty() {
                            None
                        } else {
                            popup_origin_focus
                        },
                        if config.behavior.snap_cursor_after_drop {
                            None
                        } else {
                            Some((released.current_x, released.current_y))
                        },
                        niri_tx,
                    );
                    if dropped {
                        *suppress_peek = Instant::now() + SELF_FOCUS_GRACE;
                    }
                    if dropped && config.behavior.close_on_drop && !popup_outputs.is_empty() {
                        surfaces.hide_all_popups()?;
                        popup_outputs.clear();
                    }
                }
            } else {
                let _ = niri_tx.send(NiriCommand::FocusWindow(released.window_id));
                *suppress_peek = Instant::now() + SELF_FOCUS_GRACE;
                if matches!(event.kind, SurfaceKind::Popup) && config.behavior.close_on_window_click
                {
                    surfaces.hide_all_popups()?;
                    popup_outputs.clear();
                }
            }
            Ok(true)
        }
        UiEventKind::Key { keysym } if matches!(event.kind, SurfaceKind::Popup) => {
            match keysym {
                KEY_ESCAPE => {
                    if config.behavior.close_on_escape {
                        surfaces.hide_all_popups()?;
                        popup_outputs.clear();
                    }
                }
                KEY_LEFT => {
                    move_selection(selection, snapshot, 0, -1);
                    follow_selection(config, selection, snapshot, niri_tx, suppress_peek);
                }
                KEY_RIGHT => {
                    move_selection(selection, snapshot, 0, 1);
                    follow_selection(config, selection, snapshot, niri_tx, suppress_peek);
                }
                KEY_UP => {
                    move_selection(selection, snapshot, -1, 0);
                    follow_selection(config, selection, snapshot, niri_tx, suppress_peek);
                }
                KEY_DOWN => {
                    move_selection(selection, snapshot, 1, 0);
                    follow_selection(config, selection, snapshot, niri_tx, suppress_peek);
                }
                KEY_TAB => {
                    move_output_selection(selection, snapshot, 1);
                    follow_selection(config, selection, snapshot, niri_tx, suppress_peek);
                }
                KEY_SHIFT_TAB => {
                    move_output_selection(selection, snapshot, -1);
                    follow_selection(config, selection, snapshot, niri_tx, suppress_peek);
                }
                KEY_RETURN => {
                    let close = if let Some(window_id) = selected_window(selection, snapshot) {
                        let _ = niri_tx.send(NiriCommand::FocusWindow(window_id));
                        config.behavior.close_on_window_click
                    } else if let Some(workspace_id) = selected_workspace(selection, snapshot) {
                        let _ = niri_tx.send(NiriCommand::FocusWorkspace(workspace_id));
                        config.behavior.close_on_workspace_click
                    } else {
                        false
                    };
                    // The queued move is resolved either way; a popup left open
                    // after confirming should track live state again.
                    selection.clear_session();
                    *suppress_peek = Instant::now() + SELF_FOCUS_GRACE;
                    if close {
                        surfaces.hide_all_popups()?;
                        popup_outputs.clear();
                    }
                }
                _ => {}
            }
            Ok(true)
        }
        UiEventKind::Scroll { dy } => {
            if let SurfaceKind::Bar(index) = event.kind {
                if let Some(bar) = config.bars.get(index).filter(|bar| bar.scroll_to_switch) {
                    let mut delta = if dy > 0.0 { 1 } else { -1 };
                    if bar.scroll_reverse {
                        delta = -delta;
                    }
                    focus_adjacent_workspace(
                        snapshot,
                        &event.output,
                        delta,
                        bar.scroll_wrap,
                        niri_tx,
                    );
                }
            } else {
                move_selection(selection, snapshot, if dy > 0.0 { 1 } else { -1 }, 0);
                follow_selection(config, selection, snapshot, niri_tx, suppress_peek);
            }
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn focus_adjacent_workspace(
    snapshot: &Snapshot,
    output_name: &str,
    delta: i32,
    wrap: bool,
    niri_tx: &crossbeam_channel::Sender<NiriCommand>,
) {
    let Some(output) = snapshot.outputs.get(output_name) else {
        return;
    };
    let count = output.workspaces.len();
    if count == 0 {
        return;
    }
    let current = output
        .workspaces
        .iter()
        .position(|workspace| workspace.focused || workspace.active)
        .unwrap_or(0);
    let next = if wrap {
        wrap_index(current, delta, count)
    } else {
        (current as i32 + delta).clamp(0, count.saturating_sub(1) as i32) as usize
    };
    if next != current {
        let _ = niri_tx.send(NiriCommand::FocusWorkspace(output.workspaces[next].id));
    }
}

#[allow(clippy::too_many_arguments)]
fn dispatch_drop(
    source_window_id: u64,
    target: &HitTarget,
    local_x: f64,
    local_y: f64,
    snapshot: &Snapshot,
    config: &Config,
    preserved_focus: Option<u64>,
    restore_cursor: Option<(f64, f64)>,
    niri_tx: &crossbeam_channel::Sender<NiriCommand>,
) -> bool {
    let Some(source_window) = snapshot.window(source_window_id) else {
        return false;
    };
    let source_workspace_id = source_window.workspace_id;
    let source_joined = source_window.column.is_some_and(|column| {
        snapshot
            .workspace(source_workspace_id)
            .and_then(|workspace| workspace.columns.get(&column))
            .is_some_and(|windows| windows.len() > 1)
    });
    let restore_focus = if config.behavior.focus_after_drop {
        None
    } else {
        preserved_focus.or(snapshot.focused_window)
    };
    match target {
        HitTarget::Window {
            output,
            workspace_id,
            window_id,
            column,
            floating,
            rect,
            canvas,
            ..
        } => {
            if *window_id == source_window_id {
                return false;
            }
            if *floating {
                if let Some(canvas) = canvas {
                    return dispatch_floating_drop(
                        source_window_id,
                        output,
                        *workspace_id,
                        *canvas,
                        local_x,
                        local_y,
                        snapshot,
                        restore_focus,
                        restore_cursor,
                        niri_tx,
                    );
                }
                return false;
            }
            if snapshot
                .window(source_window_id)
                .is_some_and(|window| window.floating)
                && !config.behavior.floating_drop_tiles
            {
                return dispatch_preserved_floating_drop(
                    source_window_id,
                    output,
                    *workspace_id,
                    snapshot,
                    restore_focus,
                    restore_cursor,
                    niri_tx,
                );
            }
            let allow_join = config
                .bars
                .iter()
                .find(|bar| &bar.output == output)
                .map(|bar| bar.allow_column_join)
                .unwrap_or(false);
            let fraction = ((local_x as f32 - rect.x) / rect.width.max(1.0)).clamp(0.0, 1.0);
            let target_column = column.unwrap_or_else(|| {
                snapshot
                    .workspace(*workspace_id)
                    .map(|workspace| workspace.columns.len().max(1))
                    .unwrap_or(1)
            });
            let (column, join) =
                tiled_drop_position(target_column, fraction, allow_join && !floating);
            let column = adjust_same_workspace_index(
                column,
                source_window.column,
                source_workspace_id == *workspace_id,
            );
            niri_tx
                .send(NiriCommand::MoveTiled {
                    window_id: source_window_id,
                    source_workspace_id,
                    workspace_id: *workspace_id,
                    column,
                    join,
                    source_joined,
                    restore_focus,
                    restore_cursor,
                })
                .is_ok()
        }
        HitTarget::Workspace {
            workspace_id,
            column_count,
            ..
        } => niri_tx
            .send(NiriCommand::MoveTiled {
                window_id: source_window_id,
                source_workspace_id,
                workspace_id: *workspace_id,
                column: column_count + 1,
                join: None,
                source_joined,
                restore_focus,
                restore_cursor,
            })
            .is_ok(),
        HitTarget::Canvas {
            output,
            workspace_id,
            rect,
        } if source_window.floating || config.behavior.empty_drop_floats => dispatch_floating_drop(
            source_window_id,
            output,
            *workspace_id,
            *rect,
            local_x,
            local_y,
            snapshot,
            restore_focus,
            restore_cursor,
            niri_tx,
        ),
        HitTarget::Canvas {
            workspace_id, rect, ..
        } => {
            let column_count = snapshot
                .workspace(*workspace_id)
                .map(|workspace| workspace.columns.len())
                .unwrap_or(0);
            let fraction = ((local_x as f32 - rect.x) / rect.width.max(1.0)).clamp(0.0, 1.0);
            let insertion_slots = column_count + 1;
            let column = (fraction * insertion_slots as f32).floor() as usize + 1;
            let column = column.min(insertion_slots);
            let column = adjust_same_workspace_index(
                column,
                source_window.column,
                source_workspace_id == *workspace_id,
            );
            niri_tx
                .send(NiriCommand::MoveTiled {
                    window_id: source_window_id,
                    source_workspace_id,
                    workspace_id: *workspace_id,
                    column,
                    join: None,
                    source_joined,
                    restore_focus,
                    restore_cursor,
                })
                .is_ok()
        }
        HitTarget::Panel => false,
    }
}

#[allow(clippy::too_many_arguments)]
fn dispatch_floating_drop(
    source_window_id: u64,
    output: &str,
    workspace_id: u64,
    canvas: render::Rect,
    local_x: f64,
    local_y: f64,
    snapshot: &Snapshot,
    restore_focus: Option<u64>,
    restore_cursor: Option<(f64, f64)>,
    niri_tx: &crossbeam_channel::Sender<NiriCommand>,
) -> bool {
    let Some(output_model) = snapshot.outputs.get(output) else {
        return false;
    };
    let source = snapshot.window(source_window_id);
    let width = source.map(|window| window.tile_width).unwrap_or(0.0);
    let height = source.map(|window| window.tile_height).unwrap_or(0.0);
    let center_x = ((local_x as f32 - canvas.x) / canvas.width.max(1.0)).clamp(0.0, 1.0)
        * output_model.width as f32;
    let center_y = ((local_y as f32 - canvas.y) / canvas.height.max(1.0)).clamp(0.0, 1.0)
        * output_model.height as f32;
    let x =
        (center_x as f64 - width * 0.5).clamp(0.0, (output_model.width as f64 - width).max(0.0));
    let y =
        (center_y as f64 - height * 0.5).clamp(0.0, (output_model.height as f64 - height).max(0.0));
    niri_tx
        .send(NiriCommand::MoveFloating {
            window_id: source_window_id,
            source_workspace_id: source
                .map(|window| window.workspace_id)
                .unwrap_or(workspace_id),
            workspace_id,
            x,
            y,
            make_floating: true,
            restore_focus,
            restore_cursor,
        })
        .is_ok()
}

fn dispatch_preserved_floating_drop(
    source_window_id: u64,
    output: &str,
    workspace_id: u64,
    snapshot: &Snapshot,
    restore_focus: Option<u64>,
    restore_cursor: Option<(f64, f64)>,
    niri_tx: &crossbeam_channel::Sender<NiriCommand>,
) -> bool {
    let Some(source) = snapshot.window(source_window_id) else {
        return false;
    };
    let Some(source_output) = snapshot.outputs.values().find(|output| {
        output
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.windows())
            .any(|window| window.id == source_window_id)
    }) else {
        return false;
    };
    let Some(target_output) = snapshot.outputs.get(output) else {
        return false;
    };
    let normalized_x = source.x.unwrap_or(0.0) / source_output.width.max(1) as f64;
    let normalized_y = source.y.unwrap_or(0.0) / source_output.height.max(1) as f64;
    let x = (normalized_x * target_output.width as f64).clamp(
        0.0,
        (target_output.width as f64 - source.tile_width).max(0.0),
    );
    let y = (normalized_y * target_output.height as f64).clamp(
        0.0,
        (target_output.height as f64 - source.tile_height).max(0.0),
    );
    niri_tx
        .send(NiriCommand::MoveFloating {
            window_id: source_window_id,
            source_workspace_id: source.workspace_id,
            workspace_id,
            x,
            y,
            make_floating: false,
            restore_focus,
            restore_cursor,
        })
        .is_ok()
}

fn normalize_selection(selection: &mut Selection, snapshot: &Snapshot) {
    if selection.output.is_empty() || !snapshot.outputs.contains_key(&selection.output) {
        selection.output = snapshot
            .focused_output()
            .or_else(|| snapshot.outputs.keys().next().map(String::as_str))
            .unwrap_or_default()
            .to_string();
    }
    if let Some(output) = snapshot.outputs.get(&selection.output) {
        selection.workspace = selection
            .workspace
            .min(output.workspaces.len().saturating_sub(1));
        let window_count = output
            .workspaces
            .get(selection.workspace)
            .map(|workspace| workspace.windows().count())
            .unwrap_or(0);
        selection.window = selection.window.min(window_count.saturating_sub(1));
    }
}

fn reset_selection(selection: &mut Selection, snapshot: &Snapshot) {
    // Opening the popup starts a fresh session against live state.
    selection.clear_session();
    selection.output = snapshot
        .focused_output()
        .or_else(|| snapshot.outputs.keys().next().map(String::as_str))
        .unwrap_or_default()
        .to_string();
    select_active_workspace(selection, snapshot);
}

fn select_active_workspace(selection: &mut Selection, snapshot: &Snapshot) {
    let Some(output) = snapshot.outputs.get(&selection.output) else {
        selection.workspace = 0;
        selection.window = 0;
        return;
    };
    selection.workspace = output
        .workspaces
        .iter()
        .position(|workspace| workspace.focused)
        .or_else(|| {
            output
                .workspaces
                .iter()
                .position(|workspace| workspace.active)
        })
        .unwrap_or(0);
    // Land on the window Niri would focus, not on the first one. `focused` is
    // only ever set for the globally focused window, so searching for it would
    // fall back to index 0 on every other workspace.
    selection.window = output
        .workspaces
        .get(selection.workspace)
        .map(|workspace| selection.entry_index(workspace))
        .unwrap_or(0);
}

fn move_output_selection(selection: &mut Selection, snapshot: &Snapshot, delta: i32) {
    if snapshot.outputs.is_empty() {
        return;
    }
    let outputs = snapshot.outputs.keys().collect::<Vec<_>>();
    let current = outputs
        .iter()
        .position(|output| output.as_str() == selection.output)
        .unwrap_or(0);
    selection.output = outputs[wrap_index(current, delta, outputs.len())].to_string();
    select_active_workspace(selection, snapshot);
}

fn move_selection(
    selection: &mut Selection,
    snapshot: &Snapshot,
    workspace_delta: i32,
    window_delta: i32,
) {
    normalize_selection(selection, snapshot);
    let Some(output) = snapshot.outputs.get(&selection.output) else {
        return;
    };
    if workspace_delta != 0 && !output.workspaces.is_empty() {
        // Leaving a workspace stores the cursor so coming back restores it.
        selection.remember(snapshot);
        selection.workspace = wrap_index(
            selection.workspace,
            workspace_delta,
            output.workspaces.len(),
        );
        // Arrive on the entry window. The popup parks that window on the focal
        // column, so moving between workspaces travels straight up or down
        // instead of also sliding the row sideways.
        selection.window = output
            .workspaces
            .get(selection.workspace)
            .map(|workspace| selection.entry_index(workspace))
            .unwrap_or(0);
    }
    if window_delta != 0 {
        let count = output
            .workspaces
            .get(selection.workspace)
            .map(|workspace| workspace.windows().count())
            .unwrap_or(0);
        if count > 0 {
            selection.window = wrap_index(selection.window, window_delta, count);
            selection.remember(snapshot);
        }
    }
}

/// In live mode, apply the selection to Niri as soon as it moves.
///
/// In deferred mode this does nothing: the move stays queued in the popup
/// until Enter or a click confirms it.
fn follow_selection(
    config: &Config,
    selection: &Selection,
    snapshot: &Snapshot,
    niri_tx: &crossbeam_channel::Sender<NiriCommand>,
    suppress_peek: &mut Instant,
) {
    if config.behavior.navigation != NavigationMode::Live {
        return;
    }
    if let Some(window_id) = selected_window(selection, snapshot) {
        let _ = niri_tx.send(NiriCommand::FocusWindow(window_id));
    } else if let Some(workspace_id) = selected_workspace(selection, snapshot) {
        // An empty workspace has nothing to focus, but is still reachable.
        let _ = niri_tx.send(NiriCommand::FocusWorkspace(workspace_id));
    } else {
        return;
    }
    *suppress_peek = Instant::now() + SELF_FOCUS_GRACE;
}

fn wrap_index(current: usize, delta: i32, length: usize) -> usize {
    (current as i32 + delta).rem_euclid(length as i32) as usize
}

fn selected_workspace(selection: &Selection, snapshot: &Snapshot) -> Option<u64> {
    snapshot
        .outputs
        .get(&selection.output)?
        .workspaces
        .get(selection.workspace)
        .map(|workspace| workspace.id)
}

fn selected_window(selection: &Selection, snapshot: &Snapshot) -> Option<u64> {
    snapshot
        .outputs
        .get(&selection.output)?
        .workspaces
        .get(selection.workspace)?
        .windows()
        .nth(selection.window)
        .map(|window| window.id)
}

fn selection_visual(selection: &Selection, snapshot: &Snapshot) -> Option<SelectionVisual> {
    Some(SelectionVisual {
        output: selection.output.clone(),
        workspace_id: selected_workspace(selection, snapshot)?,
        window_id: selected_window(selection, snapshot),
        // The renderer parks these on the focal column. They must match where
        // navigation will actually land, or arriving on a row makes it slide.
        anchors: selection.visited.clone(),
    })
}

fn tiled_drop_position(
    target_column: usize,
    fraction: f32,
    allow_join: bool,
) -> (usize, Option<niri::JoinDirection>) {
    let target_index = target_column.max(1);
    if allow_join && (1.0 / 3.0..=2.0 / 3.0).contains(&fraction) {
        (target_index + 1, Some(niri::JoinDirection::Left))
    } else if fraction < 0.5 {
        (target_index, None)
    } else {
        (target_index + 1, None)
    }
}

fn adjust_same_workspace_index(
    insertion_index: usize,
    source_column: Option<usize>,
    same_workspace: bool,
) -> usize {
    if same_workspace && source_column.is_some_and(|source| source < insertion_index) {
        insertion_index.saturating_sub(1).max(1)
    } else {
        insertion_index.max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_is_available_as_a_cli_subcommand() {
        let cli = Cli::try_parse_from(["niri-xmb", "restart"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Restart)));
    }

    #[test]
    fn outside_click_replay_requires_virtual_pointer() {
        let mut config = Config::default();
        config.behavior.close_on_outside_click = true;
        config.behavior.replay_outside_click = true;

        assert!(!should_capture_outside_click(&config, false));
        assert!(should_capture_outside_click(&config, true));

        config.behavior.replay_outside_click = false;
        assert!(should_capture_outside_click(&config, false));
    }

    #[test]
    fn external_focus_close_ignores_xmb_focus_echoes() {
        let mut config = Config::default();
        config.behavior.close_on_external_focus_change = true;
        let now = Instant::now();

        assert!(should_close_on_external_focus_change(
            &config,
            PopupMode::Manual,
            now,
            now
        ));
        assert!(!should_close_on_external_focus_change(
            &config,
            PopupMode::Manual,
            now,
            now + SELF_FOCUS_GRACE
        ));
        assert!(!should_close_on_external_focus_change(
            &config,
            PopupMode::Peek,
            now,
            now
        ));
    }

    #[test]
    fn pending_focus_holds_expired_peek_open() {
        let now = Instant::now();
        let expired = Some(now - Duration::from_millis(1));

        assert!(!peek_deadline_reached(expired, Some(now), now));
        assert!(peek_deadline_reached(expired, None, now));
        assert!(!peek_deadline_reached(None, None, now));
    }

    #[test]
    fn tiled_drop_preserves_niris_one_based_indices() {
        assert!(matches!(tiled_drop_position(1, 0.1, false), (1, None)));
        assert!(matches!(tiled_drop_position(1, 0.9, false), (2, None)));
        assert!(matches!(tiled_drop_position(4, 0.1, false), (4, None)));
        assert!(matches!(tiled_drop_position(4, 0.9, false), (5, None)));
    }

    #[test]
    fn center_zone_places_then_joins_from_the_right() {
        assert!(matches!(
            tiled_drop_position(1, 0.5, true),
            (2, Some(niri::JoinDirection::Left))
        ));
        assert!(matches!(
            tiled_drop_position(3, 0.5, true),
            (4, Some(niri::JoinDirection::Left))
        ));
    }

    #[test]
    fn disabled_join_routes_center_to_a_side() {
        assert!(matches!(tiled_drop_position(2, 0.49, false), (2, None)));
        assert!(matches!(tiled_drop_position(2, 0.50, false), (3, None)));
    }

    #[test]
    fn same_workspace_move_accounts_for_source_removal() {
        assert_eq!(adjust_same_workspace_index(3, Some(1), true), 2);
        assert_eq!(adjust_same_workspace_index(3, Some(4), true), 3);
        assert_eq!(adjust_same_workspace_index(3, Some(1), false), 3);
        assert_eq!(adjust_same_workspace_index(1, Some(1), true), 1);
    }
}

#[cfg(test)]
mod selection_tests {
    use super::*;
    use crate::model::{OutputModel, Snapshot, WindowModel, WorkspaceModel};
    use std::collections::BTreeMap;

    fn window(id: u64, column: usize) -> WindowModel {
        WindowModel {
            id,
            workspace_id: 0,
            app_id: "app".into(),
            title: String::new(),
            focused: false,
            urgent: false,
            floating: false,
            column: Some(column),
            tile_width: 100.0,
            tile_height: 100.0,
            x: None,
            y: None,
        }
    }

    /// `active` is the index of the window Niri would focus on this workspace.
    fn workspace(id: u64, count: usize, active: usize) -> WorkspaceModel {
        let mut columns: BTreeMap<usize, BTreeMap<usize, WindowModel>> = BTreeMap::new();
        for index in 0..count {
            columns
                .entry(index)
                .or_default()
                .insert(0, window(id * 100 + index as u64, index));
        }
        WorkspaceModel {
            id,
            index: id as u8,
            name: None,
            active: id == 1,
            focused: id == 1,
            urgent: false,
            active_window: Some(id * 100 + active as u64),
            columns,
            floating: Vec::new(),
        }
    }

    fn snapshot() -> Snapshot {
        let mut outputs = BTreeMap::new();
        outputs.insert(
            "DP-1".to_string(),
            OutputModel {
                name: "DP-1".into(),
                x: 0,
                y: 0,
                width: 2560,
                height: 1440,
                // Each workspace's active window sits at a different index.
                workspaces: vec![workspace(1, 4, 2), workspace(2, 3, 1), workspace(3, 5, 4)],
            },
        );
        Snapshot {
            outputs,
            focused_window: None,
            overview_open: false,
            generation: 1,
        }
    }

    /// A peek overlay would only duplicate Niri's own overview, so navigation
    /// while the overview is open must not raise one.
    #[test]
    fn peek_is_suppressed_while_the_overview_is_open() {
        let mut config = Config::default();
        config.peek.enabled = true;
        let mut snapshot = snapshot();
        snapshot.overview_open = true;

        assert!(
            peek_should_skip(&config, &snapshot, PopupMode::Hidden),
            "overview open must suppress peek"
        );
        snapshot.overview_open = false;
        assert!(
            !peek_should_skip(&config, &snapshot, PopupMode::Hidden),
            "with the overview closed peek should be allowed"
        );
    }

    #[test]
    fn peek_never_takes_over_a_manual_session() {
        let mut config = Config::default();
        config.peek.enabled = true;
        let snapshot = snapshot();
        assert!(
            peek_should_skip(&config, &snapshot, PopupMode::Manual),
            "a session the user is driving must win"
        );
    }

    /// The pointer crossing a monitor edge changes the focused output. That is
    /// not navigation the user asked to see a panel for.
    #[test]
    fn crossing_to_another_monitor_is_not_within_output_navigation() {
        let a: FocusMark = (Some("DP-1".into()), Some(1), Some(100));
        let b: FocusMark = (Some("DP-2".into()), Some(4), Some(400));
        assert_eq!(classify_focus_move(&a, &b), FocusMove::AcrossOutputs);
        assert_eq!(classify_focus_move(&b, &a), FocusMove::AcrossOutputs);
    }

    #[test]
    fn workspace_and_window_changes_are_within_output_navigation() {
        let base: FocusMark = (Some("DP-1".into()), Some(1), Some(100));
        let other_workspace: FocusMark = (Some("DP-1".into()), Some(2), Some(200));
        let other_window: FocusMark = (Some("DP-1".into()), Some(1), Some(101));
        assert_eq!(
            classify_focus_move(&base, &other_workspace),
            FocusMove::WithinOutput
        );
        assert_eq!(
            classify_focus_move(&base, &other_window),
            FocusMove::WithinOutput
        );
    }

    #[test]
    fn an_unchanged_signature_is_not_a_move() {
        let a: FocusMark = (Some("DP-1".into()), Some(1), Some(100));
        assert_eq!(classify_focus_move(&a, &a.clone()), FocusMove::None);
    }

    /// A file picker or dialog appearing takes focus without the user
    /// navigating, so the window set changing must be detectable.
    #[test]
    fn a_window_opening_or_closing_changes_the_live_window_set() {
        let snapshot = snapshot();
        let before = live_window_ids(&snapshot);
        assert!(!before.is_empty());

        // A dialog appears on the focused workspace.
        let mut opened = snapshot.clone();
        let workspace = &mut opened.outputs.get_mut("DP-1").unwrap().workspaces[0];
        workspace
            .columns
            .entry(99)
            .or_default()
            .insert(0, window(9999, 99));
        let after_open = live_window_ids(&opened);
        assert_ne!(before, after_open);
        assert!(after_open.contains(&9999));

        // ...and closing one is likewise visible.
        let mut closed = snapshot.clone();
        closed.outputs.get_mut("DP-1").unwrap().workspaces[0]
            .columns
            .remove(&0);
        assert_ne!(before, live_window_ids(&closed));
    }

    #[test]
    fn an_unchanged_window_set_compares_equal() {
        let snapshot = snapshot();
        assert_eq!(
            live_window_ids(&snapshot),
            live_window_ids(&snapshot.clone())
        );
    }

    #[test]
    fn peek_is_off_unless_enabled() {
        let config = Config::default();
        let snapshot = snapshot();
        assert!(!config.peek.enabled, "peek must default to off");
        assert!(peek_should_skip(&config, &snapshot, PopupMode::Hidden));
    }

    #[test]
    fn moving_between_workspaces_lands_on_the_active_window() {
        let snapshot = snapshot();
        let mut selection = Selection {
            output: "DP-1".into(),
            workspace: 0,
            window: 2,
            ..Selection::default()
        };
        move_selection(&mut selection, &snapshot, 1, 0);
        assert_eq!((selection.workspace, selection.window), (1, 1));
        move_selection(&mut selection, &snapshot, 1, 0);
        assert_eq!((selection.workspace, selection.window), (2, 4));
        // Wrapping back round must not reset to the first window either.
        move_selection(&mut selection, &snapshot, 1, 0);
        assert_eq!((selection.workspace, selection.window), (0, 2));
    }

    /// The renderer parks each row's anchor on the focal column, so vertical
    /// movement must land exactly on that anchor. If the two ever disagree the
    /// row visibly slides sideways on arrival.
    #[test]
    fn vertical_movement_does_not_scroll_the_row_sideways() {
        let snapshot = snapshot();
        let output = &snapshot.outputs["DP-1"];
        let mut selection = Selection {
            output: "DP-1".into(),
            workspace: 0,
            window: 0,
            ..Selection::default()
        };
        // Twice around, so the second lap exercises remembered positions.
        for _ in 0..output.workspaces.len() * 2 {
            let before = selection_visual(&selection, &snapshot).expect("a selection");
            move_selection(&mut selection, &snapshot, 1, 0);
            let arrived = &output.workspaces[selection.workspace];
            assert_eq!(
                selection.window,
                before.anchor(arrived),
                "selection must arrive on the column the row was already parked at"
            );
        }
    }

    #[test]
    fn selecting_the_active_workspace_uses_its_active_window() {
        let snapshot = snapshot();
        let mut selection = Selection {
            output: "DP-1".into(),
            workspace: 0,
            window: 0,
            ..Selection::default()
        };
        select_active_workspace(&mut selection, &snapshot);
        assert_eq!((selection.workspace, selection.window), (0, 2));
    }

    #[test]
    fn navigating_away_and_back_returns_to_where_you_were() {
        let snapshot = snapshot();
        let mut selection = Selection {
            output: "DP-1".into(),
            workspace: 0,
            window: 0,
            ..Selection::default()
        };
        // Walk to the third window of workspace 0, which is not its active one.
        move_selection(&mut selection, &snapshot, 0, 1);
        move_selection(&mut selection, &snapshot, 0, 1);
        assert_eq!(selection.window, 2);

        move_selection(&mut selection, &snapshot, 1, 0);
        assert_eq!((selection.workspace, selection.window), (1, 1));
        move_selection(&mut selection, &snapshot, -1, 0);
        assert_eq!(
            (selection.workspace, selection.window),
            (0, 2),
            "must restore the staged position, not snap to the live active window"
        );
    }

    #[test]
    fn a_new_session_starts_from_live_state_again() {
        let snapshot = snapshot();
        let mut selection = Selection {
            output: "DP-1".into(),
            workspace: 0,
            window: 0,
            ..Selection::default()
        };
        move_selection(&mut selection, &snapshot, 0, 1);
        assert_eq!(selection.window, 1);
        assert!(!selection.visited.is_empty());

        // Closing the popup ends the session.
        selection.clear_session();
        reset_selection(&mut selection, &snapshot);
        assert_eq!(
            (selection.workspace, selection.window),
            (0, 2),
            "workspace 0's active window index"
        );
    }

    #[test]
    fn a_remembered_index_past_the_end_falls_back_to_the_active_window() {
        let snapshot = snapshot();
        let workspace = &snapshot.outputs["DP-1"].workspaces[1];
        let mut selection = Selection {
            output: "DP-1".into(),
            workspace: 1,
            window: 0,
            ..Selection::default()
        };
        // Stale entry from before windows closed on that workspace.
        selection.visited.insert(workspace.id, 99);
        assert_eq!(selection.entry_index(workspace), workspace.active_index());
    }

    #[test]
    fn focus_signature_tracks_workspace_and_window() {
        let mut snapshot = snapshot();
        let base = focus_signature(&snapshot, true);

        // Focusing a different window is a move when windows are included.
        snapshot.focused_window = Some(101);
        let moved = focus_signature(&snapshot, true);
        assert_ne!(base, moved);

        // ...and is not, when peek only cares about workspaces.
        assert_eq!(
            focus_signature(&snapshot, false),
            focus_signature(
                &{
                    let mut other = snapshot.clone();
                    other.focused_window = Some(102);
                    other
                },
                false
            ),
            "window-level changes must be invisible in workspace-only mode"
        );
    }

    #[test]
    fn focus_signature_notices_a_workspace_change() {
        let mut snapshot = snapshot();
        let before = focus_signature(&snapshot, false);
        let workspaces = &mut snapshot.outputs.get_mut("DP-1").unwrap().workspaces;
        workspaces[0].focused = false;
        workspaces[1].focused = true;
        assert_ne!(before, focus_signature(&snapshot, false));
    }

    #[test]
    fn horizontal_movement_still_walks_one_window_at_a_time() {
        let snapshot = snapshot();
        let mut selection = Selection {
            output: "DP-1".into(),
            workspace: 1,
            window: 1,
            ..Selection::default()
        };
        move_selection(&mut selection, &snapshot, 0, 1);
        assert_eq!((selection.workspace, selection.window), (1, 2));
        move_selection(&mut selection, &snapshot, 0, 1);
        assert_eq!((selection.workspace, selection.window), (1, 0), "wraps");
    }

    #[test]
    fn stacked_windows_keep_their_column_and_row_slots() {
        let mut workspace = workspace(9, 0, 0);
        workspace.columns.clear();
        workspace
            .columns
            .entry(4)
            .or_default()
            .insert(2, window(901, 4));
        workspace
            .columns
            .entry(4)
            .or_default()
            .insert(7, window(902, 4));
        workspace
            .columns
            .entry(8)
            .or_default()
            .insert(1, window(903, 8));

        assert_eq!(
            workspace.window_slot(901),
            Some(crate::model::WindowSlot { column: 0, row: 0 })
        );
        assert_eq!(
            workspace.window_slot(902),
            Some(crate::model::WindowSlot { column: 0, row: 1 })
        );
        assert_eq!(
            workspace.window_slot(903),
            Some(crate::model::WindowSlot { column: 1, row: 0 })
        );
        assert_eq!(workspace.max_stack(), 2);
    }
}
