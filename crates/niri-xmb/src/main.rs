mod config;
mod ipc;
mod model;
mod niri;
mod render;
mod wayland;

#[path = "../../woven-bar/src/icons.rs"]
mod icons;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use config::{BarConfig, Config, PopupOutputs, PopupPosition, PopupSurfaceMode};
use crossbeam_channel::unbounded;
use ipc::{ControlCommand, ControlRequest};
use model::Snapshot;
use niri::{NiriCommand, spawn as spawn_niri};
use niri_ipc::socket::Socket;
use niri_ipc::{Request, Response};
use render::{DragVisual, HitTarget, Renderer, SelectionVisual};
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use wayland::{SurfaceKind, SurfaceManager, UiEvent, UiEventKind};

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
    PrintDefaultConfig,
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
        Command::PrintDefaultConfig => {
            println!("{}", toml::to_string_pretty(&Config::default())?);
            Ok(())
        }
    }
}

fn print_control(command: ControlCommand) -> Result<()> {
    print!("{}", ipc::send(command)?);
    Ok(())
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
    spawn_niri(snapshot_tx, niri_command_rx, cursor_restore_tx);

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
    ipc::serve(control_tx)?;

    let mut snapshot = wait_for_snapshot(&snapshot_rx).unwrap_or_default();
    let mut popup_outputs = HashSet::new();
    let mut popup_origin_focus: Option<u64> = None;
    let mut follow_candidate: Option<(String, Instant)> = None;
    let mut press: Option<PressState> = None;
    let mut selection = Selection::default();
    let mut running = true;
    let mut dirty = true;
    let mut last_redraw = Instant::now() - Duration::from_secs(1);

    tracing::info!(
        config = %config_path.display(),
        background = %config.style.background,
        accent = %config.style.accent,
        "niri-xmb daemon started"
    );
    while running {
        surfaces.dispatch()?;

        while let Ok((x, y)) = cursor_restore_rx.try_recv() {
            if !surfaces.restore_pointer(x, y) {
                tracing::warn!("cursor restore requested but virtual-pointer is unavailable");
            }
        }

        while let Ok(next) = snapshot_rx.try_recv() {
            snapshot = next;
            normalize_selection(&mut selection, &snapshot);
            dirty = true;
        }

        while let Ok(request) = control_rx.try_recv() {
            let response = match request.command {
                ControlCommand::Show => {
                    if popup_outputs.is_empty() {
                        popup_origin_focus = snapshot.focused_window;
                    }
                    show_popup(&mut surfaces, &snapshot, &config, &mut popup_outputs)?;
                    follow_candidate = None;
                    reset_selection(&mut selection, &snapshot);
                    dirty = true;
                    "ok\n".into()
                }
                ControlCommand::Hide => {
                    surfaces.hide_all_popups()?;
                    popup_outputs.clear();
                    popup_origin_focus = None;
                    follow_candidate = None;
                    press = None;
                    dirty = true;
                    "ok\n".into()
                }
                ControlCommand::Toggle => {
                    if popup_outputs.is_empty() {
                        popup_origin_focus = snapshot.focused_window;
                        show_popup(&mut surfaces, &snapshot, &config, &mut popup_outputs)?;
                        follow_candidate = None;
                        reset_selection(&mut selection, &snapshot);
                    } else {
                        surfaces.hide_all_popups()?;
                        popup_outputs.clear();
                        popup_origin_focus = None;
                        follow_candidate = None;
                        press = None;
                    }
                    dirty = true;
                    "ok\n".into()
                }
                ControlCommand::Status => {
                    serde_json::json!({
                        "visible": !popup_outputs.is_empty(),
                        "outputs": popup_outputs,
                        "generation": snapshot.generation,
                        "socket": ipc::socket_path(),
                        "cursor_restore": surfaces.cursor_restore_available(),
                    })
                    .to_string()
                        + "\n"
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
            )?;
        }

        dirty |= update_following_popup(
            &mut surfaces,
            &snapshot,
            &config,
            &mut popup_outputs,
            press.is_some(),
            &mut follow_candidate,
            &mut selection,
        )?;

        if dirty || last_redraw.elapsed() >= Duration::from_millis(250) {
            let surface_infos = surfaces.surfaces();
            let drag = press
                .as_ref()
                .filter(|press| press.dragging)
                .map(|press| DragVisual {
                    window_id: press.window_id,
                    app_id: press.app_id.clone(),
                    global_x: press.current_x,
                    global_y: press.current_y,
                });
            let selection_visual = selection_visual(&selection, &snapshot);
            for surface in &surface_infos {
                if !surface.configured {
                    continue;
                }
                let visible = popup_outputs.contains(&surface.output);
                let pixels = renderer.render(
                    surface,
                    &snapshot,
                    &config,
                    visible,
                    drag.as_ref(),
                    selection_visual.as_ref(),
                );
                surfaces.present(surface.id, &pixels)?;
            }
            dirty = false;
            last_redraw = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(8));
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

fn show_popup(
    surfaces: &mut SurfaceManager,
    snapshot: &Snapshot,
    config: &Config,
    visible: &mut HashSet<String>,
) -> Result<()> {
    let outputs = match (config.popup.outputs, config.popup.surface_mode) {
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
    };
    surfaces.show_popups(&outputs)?;
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
    surfaces.show_popups(std::slice::from_ref(&active_output))?;
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
                    if dropped && config.behavior.close_on_drop && !popup_outputs.is_empty() {
                        surfaces.hide_all_popups()?;
                        popup_outputs.clear();
                    }
                }
            } else {
                let _ = niri_tx.send(NiriCommand::FocusWindow(released.window_id));
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
                KEY_LEFT => move_selection(selection, snapshot, 0, -1),
                KEY_RIGHT => move_selection(selection, snapshot, 0, 1),
                KEY_UP => move_selection(selection, snapshot, -1, 0),
                KEY_DOWN => move_selection(selection, snapshot, 1, 0),
                KEY_TAB => move_output_selection(selection, snapshot, 1),
                KEY_SHIFT_TAB => move_output_selection(selection, snapshot, -1),
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
    selection.window = output
        .workspaces
        .get(selection.workspace)
        .and_then(|workspace| workspace.windows().position(|window| window.focused))
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
        selection.workspace = wrap_index(
            selection.workspace,
            workspace_delta,
            output.workspaces.len(),
        );
        selection.window = 0;
    }
    if window_delta != 0 {
        let count = output
            .workspaces
            .get(selection.workspace)
            .map(|workspace| workspace.windows().count())
            .unwrap_or(0);
        if count > 0 {
            selection.window = wrap_index(selection.window, window_delta, count);
        }
    }
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
