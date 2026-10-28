mod anim;
mod config;
mod draw;
mod drop_plan;
mod drop_transaction;
mod focus;
mod gamepad;
mod icons;
mod ipc;
mod mirror;
mod model;
mod niri;
mod panel;
mod render;
mod selection;
mod subscribe;
mod surface;
mod text;
mod wayland;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use config::{
    BarConfig, Config, GamepadAction, GamepadMode, NaviAction, NavigationMode, PopupOutputs,
    PopupPosition, PopupSurfaceMode,
};
use crossbeam_channel::unbounded;
use drop_plan::{DropPoint, DropPolicy, DropRequest, DropTarget, plan_drop};
use drop_transaction::DropTransactionState;
use focus::{FOCUS_SETTLE, FocusVerdict, FocusWatcher};
use gamepad::{GamepadEvent, GamepadStatus, KeyboardEmitter};
use ipc::{ControlCommand, ControlRequest};
use model::Snapshot;
use niri::{NiriCommand, NiriHealth, NiriRequest, spawn as spawn_niri};
use niri_ipc::socket::Socket;
use niri_ipc::{Request, Response};
use panel::PanelState;
use render::{DragVisual, HitTarget, Renderer, SelectionVisual};
use selection::{
    Selection, move_output as move_output_selection, move_within as move_selection,
    normalize as normalize_selection, reset as reset_selection, selected_window,
    selected_workspace, visual as selection_visual, wrap_index,
};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::Command as ProcessCommand;
use std::sync::Arc;
use std::time::{Duration, Instant};
use wayland::{
    PopupInput, ScrollAxis, ScrollTarget, SurfaceInfo, SurfaceKind, SurfaceManager, UiEvent,
    UiEventKind, WakePipe,
};

/// How long after niri-xmb moves focus itself that the resulting snapshot is
/// treated as our own echo rather than as external navigation.
const SELF_FOCUS_GRACE: Duration = Duration::from_millis(400);

/// Keep a bar wheel burst on its queued workspace target while Niri catches
/// up. Without this, each high-resolution packet starts from an old snapshot
/// and can focus the wrong workspace.
const BAR_SCROLL_TARGET_GRACE: Duration = Duration::from_millis(500);

/// Adjacent high-resolution packets identify the free-spinning wheel. A
/// ratchet click arrives after this gap and must not be rate limited.
const FREE_SPIN_PACKET_GAP: Duration = Duration::from_millis(30);
const POPUP_MOVE_PRESS: u64 = u64::MAX;

/// Keep free-spin classification briefly after its last rapid packet.
const FREE_SPIN_IDLE: Duration = Duration::from_millis(140);

const BTN_LEFT: u32 = 272;
const KEY_ESCAPE: u32 = 0xff1b;
const KEY_RETURN: u32 = 0xff0d;
const KEY_LEFT: u32 = 0xff51;
const KEY_UP: u32 = 0xff52;
const KEY_RIGHT: u32 = 0xff53;
const KEY_DOWN: u32 = 0xff54;
const KEY_TAB: u32 = 0xff09;
const KEY_SHIFT_TAB: u32 = 0xfe20;

fn drag_threshold_reached(start_x: f64, start_y: f64, x: f64, y: f64, threshold: f64) -> bool {
    (x - start_x).hypot(y - start_y) >= threshold
}

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Raise daemon log verbosity: -v logs debug, -vv logs trace.
    /// A set RUST_LOG always wins over this flag.
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count, global = true)]
    verbose: u8,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    Daemon,
    /// Launch the native configuration studio.
    Gui,
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
        #[arg(long, allow_negative_numbers = true)]
        delta: i32,
    },
    CycleWindow {
        #[arg(long)]
        output: String,
        #[arg(long, allow_negative_numbers = true)]
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
    /// Download, verify, and install a prebuilt release.
    Update {
        /// Install this release tag instead of the latest release.
        #[arg(long)]
        version: Option<String>,
        /// Install the update without restarting the daemon.
        #[arg(long)]
        no_restart: bool,
        /// Install without asking for confirmation.
        #[arg(short, long)]
        yes: bool,
        /// Install the requested version even if it is older than the running binary.
        #[arg(long)]
        force: bool,
    },
    /// Print the fully commented configuration template.
    PrintDefaultConfig,
    /// Validate a config file with the daemon's loader without restarting it.
    ConfigCheck,
    /// Print the loaded config as JSON, including defaults and theme merge.
    ConfigJson,
    /// Stream daemon state as NDJSON frames until interrupted.
    Subscribe,
}

/// Why the popup is currently on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum PopupMode {
    #[default]
    Hidden,
    /// Opened by the user; takes input and stages a selection.
    Manual,
    /// Opened by itself to mirror Niri navigation; takes no input.
    Peek,
    /// A peek overlay the pointer has moved onto.
    ///
    /// Still a peek — no keyboard, no staged session — but it holds itself open
    /// under the pointer and highlights what is under it, so reaching for it and
    /// clicking is one continuous movement instead of a race against expiry.
    PeekArmed,
}

impl PopupMode {
    fn is_peek(self) -> bool {
        matches!(self, Self::Peek | Self::PeekArmed)
    }
}

/// Whether the border must show that XMB has an input path to the panel.
/// Passive peek can be visible but must keep the normal border.
fn panel_input_active(mode: PopupMode, peek_pointer_live: bool) -> bool {
    mode == PopupMode::Manual || (mode == PopupMode::PeekArmed && peek_pointer_live)
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

impl PressState {
    fn popup_move(&self) -> bool {
        self.window_id == POPUP_MOVE_PRESS
    }
}

/// Last workspace requested by wheel input on one output.
#[derive(Default)]
struct BarScrollState {
    target_workspace: Option<u64>,
    last_step_at: Option<Instant>,
    hovered_workspace: Option<u64>,
    last_packet_at: Option<Instant>,
    free_spin_until: Option<Instant>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let filter = match effective_rust_log(std::env::var("RUST_LOG").ok().as_deref()) {
        Some(explicit) => tracing_subscriber::EnvFilter::try_new(explicit)
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(log_filter(cli.verbose))),
        None => tracing_subscriber::EnvFilter::new(log_filter(cli.verbose)),
    };
    tracing_subscriber::fmt().with_env_filter(filter).init();

    match cli.command.unwrap_or(Command::Daemon) {
        Command::Daemon => run_daemon(cli.config.unwrap_or_else(Config::default_path)),
        Command::Gui => launch_gui(cli.config),
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
        Command::CycleWindow { output, delta } => {
            print_control(ControlCommand::CycleWindow { output, delta })
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
        Command::ConfigCheck => {
            let path = cli.config.unwrap_or_else(Config::default_path);
            Config::load(&path)?;
            println!("config valid: {}", path.display());
            Ok(())
        }
        Command::ConfigJson => {
            let path = cli.config.unwrap_or_else(Config::default_path);
            let config = Config::load(&path)?;
            println!("{}", serde_json::to_string_pretty(&config)?);
            Ok(())
        }
        Command::Restart => {
            restart_daemon(cli.config.unwrap_or_else(Config::default_path), cli.verbose)
        }
        Command::Update {
            version,
            no_restart,
            yes,
            force,
        } => update_binary(
            version.as_deref(),
            no_restart,
            yes,
            force,
            cli.config.unwrap_or_else(Config::default_path),
            cli.verbose,
        ),
        Command::Subscribe => {
            subscribe::run_subscribe_client();
        }
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

fn launch_gui(config: Option<PathBuf>) -> Result<()> {
    let executable = std::env::current_exe().context("failed to locate niri-xmb executable")?;
    let studio_name = "niri-xmb-config-studio";
    let studio = executable
        .parent()
        .into_iter()
        .flat_map(|directory| {
            [
                directory.join(studio_name),
                directory.join("bin").join(studio_name),
                directory
                    .parent()
                    .map(|parent| parent.join(studio_name))
                    .unwrap_or_default(),
            ]
        })
        .find(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from(studio_name));
    let mut command = ProcessCommand::new(&studio);
    if let Some(config) = config {
        command.args(["--config", &config.to_string_lossy()]);
    }
    command.arg("--niri-xmb").arg(executable);
    let status = command
        .status()
        .with_context(|| format!("failed to launch {}", studio.display()))?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("config studio exited with {status}");
    }
}

/// The log filter a daemon runs at for the given `-v` count. A set RUST_LOG
/// overrides this, so the mapping only has to cover the flag's own levels.
fn log_filter(verbosity: u8) -> String {
    let level = match verbosity {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    format!("niri_xmb={level}")
}

/// An explicit RUST_LOG value, or None when it is unset or blank.
///
/// A blank-but-set RUST_LOG parses as an empty filter and silences every
/// level, so an environment that clears the variable must not also defeat
/// `-v`/`-vv`.
fn effective_rust_log(rust_log: Option<&str>) -> Option<String> {
    rust_log
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// Arguments for the daemon Niri is asked to spawn.
///
/// Verbosity must travel with the restart, or the replacement daemon silently
/// drops back to info no matter how the old one was running.
fn daemon_spawn_args(config_path: &std::path::Path, verbosity: u8) -> Vec<String> {
    let mut args = vec!["--config".to_string(), config_path.display().to_string()];
    for _ in 0..verbosity.min(2) {
        args.push("-v".to_string());
    }
    args.push("daemon".to_string());
    args
}

fn restart_daemon(config_path: PathBuf, verbosity: u8) -> Result<()> {
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
        .args(daemon_spawn_args(&config_path, verbosity))
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

fn update_binary(
    version: Option<&str>,
    no_restart: bool,
    yes: bool,
    force: bool,
    config_path: PathBuf,
    verbosity: u8,
) -> Result<()> {
    let target_version = match version {
        Some(version) => version.to_string(),
        None => latest_release_version()?,
    };
    let current_version = env!("CARGO_PKG_VERSION");
    match plan_update(current_version, &target_version, force)? {
        UpdatePlan::AlreadyCurrent => {
            println!("niri-xmb {current_version} is already up to date");
            return Ok(());
        }
        UpdatePlan::Upgrade => println!(
            "New niri-xmb version available: {current_version} -> {}",
            normalized_version(&target_version)
        ),
        UpdatePlan::ForcedDowngrade => println!(
            "Downgrading niri-xmb: {current_version} -> {}",
            normalized_version(&target_version)
        ),
    }
    if !yes {
        print!("Install update? [y/N] ");
        io::stdout()
            .flush()
            .context("failed to show update prompt")?;
        let mut answer = String::new();
        io::stdin()
            .read_line(&mut answer)
            .context("failed to read update confirmation")?;
        if !confirmation_is_yes(&answer) {
            println!("update canceled");
            return Ok(());
        }
    }

    let executable = std::env::current_exe().context("failed to locate niri-xmb executable")?;
    let install_dir = executable
        .parent()
        .context("niri-xmb executable has no parent directory")?;
    let custom_setup_url = std::env::var("NIRI_XMB_SETUP_URL").ok();
    let setup_url = custom_setup_url
        .as_deref()
        .unwrap_or("https://raw.githubusercontent.com/amarinne/niri-xmb/main/setup.sh");
    let setup_path = std::env::temp_dir().join(format!("niri-xmb-setup-{}.sh", std::process::id()));

    let mut curl = ProcessCommand::new("curl");
    curl.args(["--fail", "--location", "--silent", "--show-error"]);
    if custom_setup_url.is_none() {
        curl.args(["--proto", "=https", "--tlsv1.2"]);
    }
    let download = curl
        .arg("--output")
        .arg(&setup_path)
        .arg(setup_url)
        .status()
        .context("failed to run curl; install curl before updating")?;
    if !download.success() {
        let _ = std::fs::remove_file(&setup_path);
        anyhow::bail!("failed to download update setup script");
    }

    let mut setup = ProcessCommand::new("sh");
    setup
        .arg(&setup_path)
        .args(["install", "--install-dir"])
        .arg(install_dir)
        .args(["--no-niri-config", "--no-service"]);
    setup.args(["--version", &target_version]);
    let install = setup.status();
    let _ = std::fs::remove_file(&setup_path);
    let install = install.context("failed to run update setup script")?;
    if !install.success() {
        anyhow::bail!("update setup script failed");
    }

    println!("update installed: {}", executable.display());
    if no_restart {
        println!("daemon restart skipped");
        return Ok(());
    }
    let mut restart = ProcessCommand::new(&executable);
    restart.arg("--config").arg(config_path);
    for _ in 0..verbosity.min(2) {
        restart.arg("-v");
    }
    let status = restart
        .arg("restart")
        .status()
        .context("failed to restart the updated daemon")?;
    if !status.success() {
        anyhow::bail!("updated binary could not restart the daemon");
    }
    Ok(())
}

fn latest_release_version() -> Result<String> {
    if let Ok(version) = std::env::var("NIRI_XMB_LATEST_VERSION") {
        let version = version.trim();
        if !version.is_empty() {
            return Ok(version.to_string());
        }
    }

    let output = ProcessCommand::new("curl")
        .args([
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--proto",
            "=https",
            "--tlsv1.2",
            "--output",
            "/dev/null",
            "--write-out",
            "%{url_effective}",
            "https://github.com/amarinne/niri-xmb/releases/latest",
        ])
        .output()
        .context("failed to run curl; install curl before updating")?;
    if !output.status.success() {
        anyhow::bail!("failed to check the latest niri-xmb release");
    }
    let url = String::from_utf8(output.stdout).context("latest release URL was not UTF-8")?;
    let version = url
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|version| !version.is_empty())
        .context("latest release URL had no version tag")?;
    Ok(version.to_string())
}

fn normalized_version(version: &str) -> &str {
    version.trim().strip_prefix('v').unwrap_or(version.trim())
}

/// What `update` should do about a requested target version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpdatePlan {
    /// The target is the version already running; nothing to install.
    AlreadyCurrent,
    /// The target is newer than the running binary.
    Upgrade,
    /// The target is older, and the user asked for it explicitly.
    ForcedDowngrade,
}

/// Order the running version against a requested release tag. Refuses to
/// go backwards unless `force` is set, so a mistyped `--version` cannot
/// silently replace a newer build with an older one.
fn plan_update(current: &str, target: &str, force: bool) -> Result<UpdatePlan> {
    let running = semver::Version::parse(normalized_version(current))
        .with_context(|| format!("could not parse the running version {current:?}"))?;
    let requested = semver::Version::parse(normalized_version(target))
        .with_context(|| format!("could not parse the requested version {target:?}"))?;
    match requested.cmp(&running) {
        Ordering::Equal => Ok(UpdatePlan::AlreadyCurrent),
        Ordering::Greater => Ok(UpdatePlan::Upgrade),
        Ordering::Less if force => Ok(UpdatePlan::ForcedDowngrade),
        Ordering::Less => anyhow::bail!(
            "refusing to install {} over the running {}; pass --force to downgrade",
            normalized_version(target),
            normalized_version(current)
        ),
    }
}

fn confirmation_is_yes(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
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
                        "active_window": workspace.active_window,
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
        "content_revision": snapshot.content_revision,
        "focused_window": snapshot.focused_window,
        "outputs": outputs,
    })
    .to_string()
}

fn snapshot_response(snapshot: &Snapshot, output_filter: Option<&str>) -> String {
    if output_filter.is_some_and(|name| !snapshot.outputs.contains_key(name)) {
        return "error: unknown output\n".into();
    }
    snapshot_json(snapshot, output_filter) + "\n"
}

fn popup_mode_name(mode: PopupMode) -> &'static str {
    match mode {
        PopupMode::Hidden => "hidden",
        PopupMode::Manual => "manual",
        PopupMode::Peek => "peek",
        PopupMode::PeekArmed => "peek_armed",
    }
}

#[allow(clippy::too_many_arguments)]
fn status_response(
    popup_outputs: &HashSet<String>,
    popup_mode: PopupMode,
    selection: Option<SelectionVisual>,
    snapshot: &Snapshot,
    cursor_restore: bool,
    niri_health: &NiriHealth,
    drop_transactions: &DropTransactionState,
    gamepad: Option<GamepadStatus>,
    subscribers: usize,
) -> String {
    serde_json::json!({
        "visible": !popup_outputs.is_empty(),
        "mode": popup_mode_name(popup_mode),
        // Live pressed buttons, axes, and discovery diagnostics for the Config Studio overlay.
        // The shared serializer keeps one-shot status and pushed stream frames identical.
        "gamepad": gamepad::gamepad_status_json(gamepad.as_ref()),
        "selection": selection.map(|visual| {
            serde_json::json!({
                "output": visual.output,
                "workspace_id": visual.workspace_id,
                "window_id": visual.window_id,
            })
        }),
        "outputs": popup_outputs,
        "generation": snapshot.generation,
        "socket": ipc::socket_path(),
        // The dock polls this file instead of forking a process per tick; it
        // cannot read the environment, so the path is published here.
        "snapshot_mirror": mirror::mirror_path(),
        "cursor_restore": cursor_restore,
        "niri": {
            "event_stream_connected": niri_health.event_stream_connected,
            "last_event_error": niri_health.last_event_error,
            "last_action_error": niri_health.last_action_error,
        },
        "drop_transaction": drop_transactions.status_json(),
        "subscribers": subscribers,
    })
    .to_string()
        + "\n"
}

/// Match configured bars against the connected outputs.
///
/// An empty `[[bars]]` list means "one bar per connected output", which is
/// what a fresh install should get. A non-empty list is filtered down to
/// connectors that actually exist; if that drops every bar the daemon would
/// otherwise start silently bar-less, so name both sides loudly instead.
fn resolve_bars(config: &mut Config, output_names: &[String]) {
    if config.bars_enabled && config.bars.is_empty() {
        config.bars = output_names
            .iter()
            .map(|output| BarConfig {
                output: output.clone(),
                ..BarConfig::default()
            })
            .collect();
        return;
    }

    let configured: Vec<String> = config.bars.iter().map(|bar| bar.output.clone()).collect();
    config.bars.retain(|bar| {
        if output_names.contains(&bar.output) {
            true
        } else {
            tracing::warn!(output = %bar.output, "configured bar output is not connected");
            false
        }
    });

    if config.bars_enabled && config.bars.is_empty() && !configured.is_empty() {
        tracing::error!(
            configured = %configured.join(", "),
            connected = %output_names.join(", "),
            "no configured bar output is connected; starting without any bar"
        );
    }
}

/// Mirror the newest snapshot and keep the latest frame for stream subscribers.
///
/// The hub retains the frame even when nobody is listening yet. A subscriber
/// that connects between publishes must receive the last known state right
/// after hello; gating this call on `subscriber_count` left every new
/// subscriber waiting for the next Niri change.
fn publish_snapshot(
    snapshot: &Snapshot,
    subscriptions: &subscribe::SubscriptionHub,
    mirror: Option<&mirror::SnapshotMirror>,
) {
    let body = snapshot_json(snapshot, None);
    if let Some(mirror) = mirror {
        mirror.write(&body);
    }
    subscriptions.publish(snapshot.generation, &body);
}

/// The soonest of everything the loop is waiting for.
///
/// Six hand-written `min` calls used to do this, so a new deadline that
/// forgot to add a seventh produced not a crash but a silent 250 ms
/// latency bug. Register them all here instead.
struct Deadlines {
    wait: Duration,
    now: Instant,
}

impl Deadlines {
    fn new(idle: Duration, now: Instant) -> Self {
        Self { wait: idle, now }
    }

    /// Wait no longer than until `deadline`.
    fn at(&mut self, deadline: Instant) {
        self.wait = self.wait.min(deadline.saturating_duration_since(self.now));
    }

    /// Wait no longer than `total` measured from `since`.
    fn after(&mut self, since: Instant, total: Duration) {
        self.wait = self
            .wait
            .min(total.saturating_sub(self.now.saturating_duration_since(since)));
    }

    fn wait(self) -> Duration {
        self.wait
    }
}

fn run_daemon(config_path: PathBuf) -> Result<()> {
    let mut config = Config::load(&config_path)?;
    let output_names = connected_outputs()?;
    if output_names.is_empty() {
        anyhow::bail!("Niri reported no connected outputs");
    }
    resolve_bars(&mut config, &output_names);

    let (snapshot_tx, snapshot_rx) = unbounded();
    let (niri_command_tx, niri_command_rx) = unbounded();
    let (cursor_restore_tx, cursor_restore_rx) = unbounded();
    let (niri_health_tx, niri_health_rx) = unbounded();
    let (niri_transaction_tx, niri_transaction_rx) = unbounded();
    let wake = WakePipe::new()?;
    spawn_niri(
        snapshot_tx,
        niri_command_rx,
        cursor_restore_tx,
        niri_health_tx,
        niri_transaction_tx,
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
    // The hub must exist before the gamepad service starts so its reader
    // threads can push live frames to subscribers from the first event.
    let subscriptions = std::sync::Arc::new(subscribe::SubscriptionHub::new());
    let gamepad = gamepad::GamepadService::start(
        &config.gamepad,
        wake.handle(),
        Some(Arc::clone(&subscriptions)),
    );
    let mut keyboard_emitter = match KeyboardEmitter::new(&config.gamepad) {
        Ok(emitter) => emitter,
        Err(error) => {
            tracing::warn!(%error, "gamepad keyboard bindings unavailable");
            None
        }
    };
    if gamepad.is_none() {
        // No readers will ever notify; subscribers still deserve one
        // (disabled/empty) gamepad frame instead of silence.
        subscriptions.publish_gamepad(&gamepad::gamepad_status_json(None).to_string());
    }

    let (control_tx, control_rx) = unbounded::<ControlRequest>();
    // Dropped with the daemon, which removes the file.
    let mirror = mirror::SnapshotMirror::new();
    let _control_server =
        ipc::serve_with_subscriptions(control_tx, wake.handle(), Arc::clone(&subscriptions))?;

    let mut snapshot = wait_for_snapshot(&snapshot_rx).unwrap_or_default();
    publish_snapshot(&snapshot, &subscriptions, mirror.as_ref());
    let mut niri_health = NiriHealth::default();
    let mut drop_transactions = DropTransactionState::default();
    let mut panel = PanelState::default();
    let mut bar_scrolls: HashMap<String, BarScrollState> = HashMap::new();
    let mut selection = Selection::default();
    let mut gamepad_pointer_buttons: HashSet<u16> = HashSet::new();
    let mut popup_was_open = false;
    // Focus changes we caused ourselves must not bounce back as a peek.
    let mut peek_suppressed_until = Instant::now();
    let mut focus = FocusWatcher::default();
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
        dirty |= surfaces.needs_redraw();

        while let Ok((x, y)) = cursor_restore_rx.try_recv() {
            if !surfaces.restore_pointer(x, y) {
                tracing::warn!("cursor restore requested but virtual-pointer is unavailable");
            }
        }

        while let Ok(update) = niri_health_rx.try_recv() {
            niri_health.apply(update);
        }

        while let Ok(next) = snapshot_rx.try_recv() {
            snapshot = next;
            publish_snapshot(&snapshot, &subscriptions, mirror.as_ref());
            normalize_selection(&mut selection, &snapshot);
            let now = Instant::now();
            let changed = focus.observe(&snapshot, now);
            if changed {
                if should_close_on_external_focus_change(
                    &config,
                    panel.mode(),
                    now,
                    peek_suppressed_until,
                ) {
                    surfaces.hide_all_popups()?;
                    panel.close();
                }
                if Instant::now() >= peek_suppressed_until {
                    // Whatever the popup is showing, it now shows where Niri
                    // actually is. Peek and the interactive panel behave as one
                    // thing here: an external move re-syncs both, so the panel
                    // never sits on a stale workspace while Niri has moved on.
                    selection.clear_session();
                    reset_selection(&mut selection, &snapshot);
                }
            }
            dirty = true;
        }

        let now = Instant::now();
        while let Ok(result) = niri_transaction_rx.try_recv() {
            drop_transactions.record_action_result(result, now);
        }
        drop_transactions.reconcile(
            &snapshot,
            niri_health.event_stream_connected,
            Instant::now(),
        );

        if focus.settle(&snapshot, now) == Some(FocusVerdict::Navigated)
            && now >= peek_suppressed_until
        {
            let was_hidden = panel.mode() == PopupMode::Hidden;
            if was_hidden {
                renderer.clear_popup_offsets();
            }
            let pointer_live = panel.peek().pointer_live();
            let mut mode = panel.mode();
            let deadline = trigger_peek(
                &mut surfaces,
                &snapshot,
                &config,
                panel.outputs_mut(),
                &mut mode,
                &mut selection,
                peek_suppressed_until,
                pointer_live,
            )?;
            panel.set_mode(mode);
            panel.peek_mut().set_deadline(deadline);
            if was_hidden && panel.mode() == PopupMode::Peek && config.peek.pointer_promote {
                panel
                    .peek_mut()
                    .arm_pointer(Instant::now() + Duration::from_millis(config.peek.arm_grace_ms));
            }
            dirty = true;
        }
        // Guarded on Peek: a manual session must never inherit this expiry.
        // The overview opening counts as an expiry, so an overlay already up
        // gets out of the way immediately.
        // An armed overlay has no deadline to reach: the pointer resting on it
        // holds it open. The overview still clears it either way.
        if panel.mode().is_peek()
            && (snapshot.overview_open
                || (panel.mode() == PopupMode::Peek
                    && peek_deadline_reached(
                        panel.peek().deadline(),
                        focus.pending_since(),
                        Instant::now(),
                    )))
        {
            surfaces.hide_all_popups()?;
            panel.close();
            dirty = true;
        }

        // Hand the overlay to the pointer once the grace has passed. It keeps
        // the panel rectangle only, and never the keyboard, so everything
        // outside the panel — and every keybind — behaves as before.
        if let Some(at) = panel.peek().pointer_at()
            && panel.mode().is_peek()
            && Instant::now() >= at
        {
            panel.peek_mut().take_pointer();
            surfaces.set_popup_input(PopupInput::POINTER, false)?;
            dirty = true;
        }

        while let Ok(request) = control_rx.try_recv() {
            let response = match request.command {
                ControlCommand::Show => {
                    show_manual_popup(
                        &mut surfaces,
                        &mut renderer,
                        &snapshot,
                        &config,
                        &mut panel,
                        &mut selection,
                    )?;
                    dirty = true;
                    "ok\n".into()
                }
                ControlCommand::Hide => {
                    surfaces.hide_all_popups()?;
                    panel.close();
                    dirty = true;
                    "ok\n".into()
                }
                ControlCommand::Toggle => {
                    // A peek overlay is promoted rather than dismissed: the
                    // keybind means "let me drive this", not "hide it".
                    if panel.mode() == PopupMode::Manual {
                        surfaces.hide_all_popups()?;
                        panel.close();
                    } else {
                        renderer.clear_popup_offsets();
                        let origin_focus = snapshot.focused_window;
                        show_popup(
                            &mut surfaces,
                            &snapshot,
                            &config,
                            panel.outputs_mut(),
                            PopupInput::FULL,
                            PopupMode::Manual,
                        )?;
                        panel.promote(origin_focus);
                        reset_selection(&mut selection, &snapshot);
                    }
                    dirty = true;
                    "ok\n".into()
                }
                ControlCommand::Status => status_response(
                    panel.outputs(),
                    panel.mode(),
                    selection_visual(&selection, &snapshot),
                    &snapshot,
                    surfaces.cursor_restore_available(),
                    &niri_health,
                    &drop_transactions,
                    gamepad.as_ref().map(|gamepad| gamepad.status()),
                    subscriptions.subscriber_count(),
                ),
                ControlCommand::Snapshot { output } => {
                    snapshot_response(&snapshot, output.as_deref())
                }
                ControlCommand::FocusWindow { window_id } => {
                    if snapshot.window(window_id).is_none() {
                        "error: unknown window\n".into()
                    } else {
                        // IPC focus commands come from external controls such as
                        // the Noctalia taskbar. Mark them as self-focus before
                        // the worker can deliver the focus event back to us.
                        peek_suppressed_until = Instant::now() + SELF_FOCUS_GRACE;
                        if niri_command_tx
                            .send(NiriCommand::FocusWindow(window_id).into())
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
                            .send(NiriCommand::FocusWorkspace(workspace_id).into())
                            .is_ok()
                        {
                            "ok\n".into()
                        } else {
                            "error: Niri command worker unavailable\n".into()
                        }
                    }
                }
                ControlCommand::CycleWorkspace { output, delta } => {
                    let wrap = scroll_wrap_for(&config, &output);
                    let target = snapshot.outputs.get(&output).and_then(|model| {
                        if model.workspaces.is_empty() {
                            return None;
                        }
                        let current = model
                            .workspaces
                            .iter()
                            .position(|workspace| workspace.active)
                            .unwrap_or(0);
                        let next = step_index(current, delta, model.workspaces.len(), wrap);
                        Some(model.workspaces[next].id)
                    });
                    match target {
                        Some(workspace_id) => {
                            peek_suppressed_until = Instant::now() + SELF_FOCUS_GRACE;
                            if niri_command_tx
                                .send(NiriCommand::FocusWorkspace(workspace_id).into())
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
                ControlCommand::CycleWindow { output, delta } => {
                    let wrap = scroll_wrap_for(&config, &output);
                    let target = snapshot.outputs.get(&output).and_then(|model| {
                        let workspace = model
                            .workspaces
                            .iter()
                            .find(|workspace| workspace.active)
                            .or_else(|| model.workspaces.first())?;
                        let windows: Vec<u64> =
                            workspace.windows().map(|window| window.id).collect();
                        if windows.is_empty() {
                            return None;
                        }
                        let current = windows
                            .iter()
                            .position(|id| Some(*id) == snapshot.focused_window)
                            .or_else(|| {
                                windows
                                    .iter()
                                    .position(|id| Some(*id) == workspace.active_window)
                            })
                            .unwrap_or(0);
                        let next = step_index(current, delta, windows.len(), wrap);
                        Some(windows[next])
                    });
                    match target {
                        Some(window_id) => {
                            peek_suppressed_until = Instant::now() + SELF_FOCUS_GRACE;
                            if niri_command_tx
                                .send(NiriCommand::FocusWindow(window_id).into())
                                .is_ok()
                            {
                                "ok\n".into()
                            } else {
                                "error: Niri command worker unavailable\n".into()
                            }
                        }
                        None => "error: unknown output or no windows\n".into(),
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
                    // The planner owns every rejection reason, so the target is
                    // built unconditionally and an unknown workspace surfaces
                    // as its own exact error instead of a generic refusal.
                    let fraction = clamp_fraction(fraction);
                    let target = match target_window_id {
                        Some(window_id) => DropTarget::Window {
                            workspace_id: target_workspace_id,
                            window_id,
                            fraction,
                            canvas_point: None,
                        },
                        None => DropTarget::Canvas {
                            workspace_id: target_workspace_id,
                            point: DropPoint {
                                x: fraction,
                                y: 0.5,
                            },
                        },
                    };
                    let allow_column_join = target_window_id.is_some()
                        && column_join_allowed(&snapshot, &config, target_workspace_id);
                    // A drop is answered by its transaction, not here: the
                    // reply is staged until Niri confirms the move, the action
                    // fails, or the confirmation times out. Rejections before a
                    // transaction exists also go out through that channel.
                    dispatch_drop(
                        source_window_id,
                        target,
                        allow_column_join,
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
                        &mut drop_transactions,
                        Some(request.reply),
                        &niri_command_tx,
                        panel.mode(),
                        panel.is_open(),
                    );
                    continue;
                }
                // Subscribe connections never reach the daemon loop: the
                // control worker hands them straight to the hub.
                ControlCommand::Subscribe { .. } => {
                    unreachable!("subscribe requests are handled by the control worker")
                }
                ControlCommand::Quit => {
                    running = false;
                    "ok\n".into()
                }
            };
            let _ = request.reply.send(response);
        }

        while let Ok(event) = ui_rx.try_recv() {
            // A peek overlay decides what a pointer event means to it before
            // the event is handled as panel input, so the click that promotes
            // it is also the click that acts.
            if panel.mode().is_peek() {
                match classify_peek_pointer(&event) {
                    PeekPointer::Promote => {
                        if panel.mode() == PopupMode::PeekArmed
                            && config.peek.move_popup
                            && matches!(event.kind, SurfaceKind::Popup)
                            && renderer.panel_grab_hit(
                                event.surface_id,
                                event.x,
                                event.y,
                                config.behavior.popup_grab_region,
                            )
                        {
                            // An armed peek with movement enabled keeps a
                            // border press local: it moves instead of
                            // promoting into a focus-owning session. Passive
                            // peek still promotes when it is configured to
                            // accept the pointer.
                        } else {
                            let capture =
                                popup_capture_outside(&config, PopupMode::Manual, PopupInput::FULL);
                            surfaces.set_popup_input(PopupInput::FULL, capture)?;
                            panel.promote(snapshot.focused_window);
                            reset_selection(&mut selection, &snapshot);
                            dirty = true;
                        }
                    }
                    PeekPointer::Arm => {
                        if panel.mode() != PopupMode::PeekArmed {
                            dirty = true;
                        }
                        panel.arm();
                    }
                    PeekPointer::Disarm => {
                        if panel.mode() == PopupMode::PeekArmed {
                            panel.disarm(
                                Instant::now()
                                    + Duration::from_millis(config.peek.demote_ms.max(1)),
                            );
                            dirty = true;
                        }
                    }
                    PeekPointer::Ignore => {}
                }
                // The pointer region going live produces an enter, and a
                // cursor-positioned panel re-anchors itself on one. An overlay
                // that jumped out from under the hand reaching for it would
                // undo the whole point, so peek ignores enter entirely.
                if panel.mode().is_peek()
                    && matches!(event.kind, SurfaceKind::Popup)
                    && matches!(event.event, UiEventKind::Enter)
                {
                    continue;
                }
            }
            let origin_focus = panel.origin_focus();
            dirty |= handle_ui_event(
                event,
                &mut surfaces,
                &mut renderer,
                &snapshot,
                &config,
                &niri_command_tx,
                &mut drop_transactions,
                origin_focus,
                &mut panel,
                &mut bar_scrolls,
                &mut selection,
                &mut peek_suppressed_until,
            )?;
        }

        if let Some(gamepad) = &gamepad {
            let mut mode = gamepad.mode();
            while let Some(event) = gamepad.try_recv() {
                if let GamepadEvent::ModeChanged(next) = event {
                    if next == GamepadMode::Game {
                        release_gamepad_pointer_buttons(
                            &mut surfaces,
                            &mut gamepad_pointer_buttons,
                        );
                        if let Some(emitter) = keyboard_emitter.as_mut() {
                            let _ = emitter.release_all();
                        }
                    }
                    mode = next;
                    dirty = true;
                    continue;
                }
                dirty |= handle_gamepad_event(
                    event,
                    mode,
                    &mut renderer,
                    &mut surfaces,
                    &config,
                    &snapshot,
                    &niri_command_tx,
                    &mut panel,
                    &mut selection,
                    &mut peek_suppressed_until,
                    &mut gamepad_pointer_buttons,
                    &mut keyboard_emitter,
                )?;
            }
        }
        if panel.mode() == PopupMode::Manual {
            release_gamepad_pointer_buttons(&mut surfaces, &mut gamepad_pointer_buttons);
        }

        // Only the manual popup uses the delayed follow. A peek overlay is
        // re-placed on the focused output the moment focus moves, and routing
        // it through here would re-show it as an input-grabbing surface.
        if panel.mode() == PopupMode::Manual {
            let pressed = panel.is_pressed();
            dirty |= update_following_popup(
                &mut surfaces,
                &snapshot,
                &config,
                &mut panel,
                pressed,
                &mut selection,
            )?;
        }

        // Catch every way the popup can close — Escape, outside click, a
        // confirmed pick, IPC — in one place, and end the staging session.
        let popup_open = panel.is_open();
        if popup_was_open && !popup_open {
            renderer.clear_popup_offsets();
            selection.clear_session();
            // Escape and outside clicks land here with a press still
            // tracked; closing must cancel it, or the release that follows
            // completes a drop the user just cancelled.
            panel.close();
            // Confirming a pick moves Niri's focus, which would otherwise come
            // straight back as a peek for the move just made.
            peek_suppressed_until = Instant::now() + SELF_FOCUS_GRACE;
        }
        popup_was_open = popup_open;

        // Animation targets are recomputed every iteration, not only on
        // The accent means XMB has an active input mode. An armed peek has
        // claimed the pointer region, but normal peek remains passive.
        dirty |= renderer.set_panel_input_active(panel_input_active(
            panel.mode(),
            panel.peek().pointer_live(),
        ));

        // redraw, so a settled UI stops requesting frames entirely.
        let selection_visual = selection_visual(&selection, &snapshot);
        let animating = renderer.update_animations(
            &snapshot,
            &config,
            selection_visual.as_ref(),
            panel.outputs(),
        );
        dirty |= animating;

        // The panel rectangle is what a pointer region is built from. A peek
        // overlay needs it too, and needs it before the pointer arrives, so it
        // is published from the same place whichever mode asks for it.
        let panel_input_rect_wanted = match panel.mode() {
            PopupMode::Manual => false,
            PopupMode::Peek | PopupMode::PeekArmed => config.peek.pointer_promote,
            PopupMode::Hidden => false,
        };

        if dirty {
            let surface_infos = surfaces.surfaces();
            renderer.retain_surfaces(&surface_infos);
            let all_ready = surface_infos
                .iter()
                .all(|surface| surface.configured && surface.frame_ready);
            if all_ready {
                let drag = panel
                    .press()
                    .filter(|press| press.dragging)
                    .and_then(|press| {
                        (!press.popup_move()).then_some(DragVisual {
                            window_id: press.window_id,
                            app_id: press.app_id.clone(),
                            global_x: press.current_x,
                            global_y: press.current_y,
                        })
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
                        && panel_input_rect_wanted
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
        let idle = if animating || dirty {
            Duration::from_millis(16)
        } else {
            Duration::from_millis(250)
        };
        let mut deadlines = Deadlines::new(idle, now);
        if let Some(since) = focus.pending_since() {
            deadlines.after(since, FOCUS_SETTLE);
        }
        if let Some(deadline) = panel.peek().deadline() {
            deadlines.at(deadline);
        }
        if let Some(at) = panel.peek().pointer_at() {
            deadlines.at(at);
        }
        if let Some((_, since)) = panel.follow_candidate() {
            deadlines.after(*since, Duration::from_millis(config.popup.follow_delay_ms));
        }
        if let Some(deadline) = drop_transactions.next_deadline() {
            deadlines.at(deadline);
        }
        wait_timeout = deadlines.wait();
    }

    surfaces.hide_all_popups()
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

/// What a pointer event over a peek overlay means for the overlay itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PeekPointer {
    /// The pointer is on the panel: hold the overlay open under it.
    Arm,
    /// The pointer left the panel: hand the countdown back.
    Disarm,
    /// The pointer clicked the panel: the user wants to drive it.
    Promote,
    Ignore,
}

/// Read a UI event as an intention towards the peek overlay.
///
/// Motion is the arming signal rather than enter: the input region becoming
/// live under a resting pointer also produces an enter, and an overlay that
/// pinned itself open merely because it appeared under the cursor would never
/// go away on its own.
fn classify_peek_pointer(event: &UiEvent) -> PeekPointer {
    if !matches!(event.kind, SurfaceKind::Popup) {
        return PeekPointer::Ignore;
    }
    match event.event {
        UiEventKind::Motion
        | UiEventKind::Scroll {
            target: ScrollTarget::Hovered,
            ..
        } => PeekPointer::Arm,
        UiEventKind::Leave => PeekPointer::Disarm,
        UiEventKind::Press { button } if button == BTN_LEFT => PeekPointer::Promote,
        _ => PeekPointer::Ignore,
    }
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
    pointer_live: bool,
) -> Result<Option<Instant>> {
    if peek_should_skip(config, snapshot, *mode) || Instant::now() < suppressed_until {
        return Ok(None);
    }
    // Showing a surface costs a blocking Wayland roundtrip, so while already
    // peeking only re-show when focus actually moved to a different monitor.
    let targets = popup_output_targets(snapshot, config);
    let same_outputs =
        targets.len() == visible.len() && targets.iter().all(|output| visible.contains(output));
    let armed = *mode == PopupMode::PeekArmed;
    if *mode == PopupMode::Hidden || !same_outputs {
        // An overlay that already claimed the panel rectangle keeps it across
        // the move. Re-showing it as passive would silently drop the region,
        // and nothing would hand it back until the overlay hid and rose again.
        let input = if pointer_live {
            PopupInput::POINTER
        } else {
            PopupInput::NONE
        };
        show_popup(surfaces, snapshot, config, visible, input, *mode)?;
        if !armed {
            *mode = PopupMode::Peek;
        }
    }
    reset_selection(selection, snapshot);
    if armed {
        // The pointer owns the countdown while it rests on the panel.
        return Ok(None);
    }
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

/// Whether a visible popup should swallow the whole output to catch outside
/// clicks. Manual mode captures only when `close_on_outside_click` is set,
/// including pointer-only capture; peek remains panel-only so it never
/// becomes modal.
fn popup_capture_outside(config: &Config, mode: PopupMode, input: PopupInput) -> bool {
    config.behavior.close_on_outside_click && mode == PopupMode::Manual && input.pointer
}

fn should_close_manual_outside(config: &Config, mode: PopupMode, press_active: bool) -> bool {
    config.behavior.close_on_outside_click && mode == PopupMode::Manual && !press_active
}

fn should_replay_outside_click(config: &Config) -> bool {
    config.behavior.replay_outside_click
}

fn show_popup(
    surfaces: &mut SurfaceManager,
    snapshot: &Snapshot,
    config: &Config,
    visible: &mut HashSet<String>,
    input: PopupInput,
    mode: PopupMode,
) -> Result<()> {
    let outputs = popup_output_targets(snapshot, config);
    let capture_outside = popup_capture_outside(config, mode, input);
    surfaces.show_popups_with(&outputs, input, capture_outside)?;
    visible.clear();
    visible.extend(outputs);
    Ok(())
}

fn show_manual_popup(
    surfaces: &mut SurfaceManager,
    renderer: &mut Renderer,
    snapshot: &Snapshot,
    config: &Config,
    panel: &mut PanelState,
    selection: &mut Selection,
) -> Result<()> {
    if panel.mode() != PopupMode::Manual {
        renderer.clear_popup_offsets();
        panel.set_origin_focus(snapshot.focused_window);
    }
    let origin_focus = panel.origin_focus();
    show_popup(
        surfaces,
        snapshot,
        config,
        panel.outputs_mut(),
        PopupInput::FULL,
        PopupMode::Manual,
    )?;
    // Taking control cancels the peek countdown.
    panel.promote(origin_focus);
    reset_selection(selection, snapshot);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn update_following_popup(
    surfaces: &mut SurfaceManager,
    snapshot: &Snapshot,
    config: &Config,
    panel: &mut PanelState,
    pointer_active: bool,
    selection: &mut Selection,
) -> Result<bool> {
    if !panel.is_open()
        || config.popup.surface_mode != PopupSurfaceMode::FollowActive
        || config.popup.outputs == PopupOutputs::All
        || pointer_active
    {
        *panel.follow_candidate_mut() = None;
        return Ok(false);
    }

    let Some(active_output) = snapshot.focused_output().map(str::to_string) else {
        *panel.follow_candidate_mut() = None;
        return Ok(false);
    };
    if panel.outputs().contains(&active_output) {
        *panel.follow_candidate_mut() = None;
        return Ok(false);
    }

    let now = Instant::now();
    match panel.follow_candidate() {
        Some((output, since)) if output == &active_output => {
            if now.duration_since(*since) < Duration::from_millis(config.popup.follow_delay_ms) {
                return Ok(false);
            }
        }
        _ => {
            *panel.follow_candidate_mut() = Some((active_output, now));
            return Ok(false);
        }
    }

    let active_output = panel
        .follow_candidate_mut()
        .take()
        .map(|(output, _)| output)
        .unwrap_or(active_output);
    surfaces.show_popups(std::slice::from_ref(&active_output), true)?;
    panel.outputs_mut().clear();
    panel.outputs_mut().insert(active_output.clone());
    selection.select_output(active_output, snapshot);
    Ok(true)
}

/// A resolved wheel target.
///
/// A hovered bar keeps the surface id so the existing row-under-pointer target
/// still starts the gesture. The focused fallback has no pointer row.
#[derive(Debug, Clone, PartialEq, Eq)]
enum WheelTarget {
    Bar {
        index: usize,
        output: String,
        surface_id: Option<usize>,
    },
    Popup {
        output: String,
    },
}

fn wheel_target_from_surface(surface: &SurfaceInfo, hovered: bool) -> WheelTarget {
    match surface.kind {
        SurfaceKind::Bar(index) => WheelTarget::Bar {
            index,
            output: surface.output.clone(),
            surface_id: hovered.then_some(surface.id),
        },
        SurfaceKind::Popup => WheelTarget::Popup {
            output: surface.output.clone(),
        },
    }
}

/// Pick the panel that owns wheel input.
///
/// The surface under the pointer always wins. Without one, the open panel is
/// the focused panel: prefer its surface on the focused output, then any
/// visible popup (a fixed panel can sit on another output). With no open
/// popup, use the bar on the focused output. `keyboard_surface` is never
/// consulted because it moves independently from the pointer.
fn resolve_wheel_target(
    hovered: Option<&SurfaceInfo>,
    focused_output: Option<&str>,
    surfaces: &[SurfaceInfo],
    popup_open: bool,
) -> Option<WheelTarget> {
    if let Some(surface) = hovered.filter(|surface| surface.configured) {
        return Some(wheel_target_from_surface(surface, true));
    }
    if popup_open {
        let popup = surfaces
            .iter()
            .find(|surface| {
                surface.configured
                    && matches!(surface.kind, SurfaceKind::Popup)
                    && Some(surface.output.as_str()) == focused_output
            })
            .or_else(|| {
                surfaces.iter().find(|surface| {
                    surface.configured && matches!(surface.kind, SurfaceKind::Popup)
                })
            });
        if let Some(popup) = popup {
            return Some(wheel_target_from_surface(popup, false));
        }
    }
    let focused_output = focused_output?;
    let bar = surfaces.iter().find(|surface| {
        surface.configured
            && matches!(surface.kind, SurfaceKind::Bar(_))
            && surface.output.as_str() == focused_output
    })?;
    Some(wheel_target_from_surface(bar, false))
}

fn scroll_deltas(axis: ScrollAxis, steps: i32) -> (i32, i32) {
    match axis {
        ScrollAxis::Vertical => (steps, 0),
        ScrollAxis::Horizontal => (0, steps),
    }
}

/// Map a gamepad action onto the popup keyboard controls.
///
/// `Back` is previous output (Shift+Tab); `Close` is Escape and therefore
/// follows `close_on_escape`. The mapping is applied only while a manual
/// popup is open, matching keyboard focus. `Open` has no keysym; it is
/// handled before this mapping.
fn gamepad_keysym(action: GamepadAction) -> Option<u32> {
    match action {
        GamepadAction::Up => Some(KEY_UP),
        GamepadAction::Down => Some(KEY_DOWN),
        GamepadAction::Left => Some(KEY_LEFT),
        GamepadAction::Right => Some(KEY_RIGHT),
        GamepadAction::Tab => Some(KEY_TAB),
        GamepadAction::Back => Some(KEY_SHIFT_TAB),
        GamepadAction::Confirm => Some(KEY_RETURN),
        GamepadAction::Close => Some(KEY_ESCAPE),
        GamepadAction::Open => None,
    }
}

fn gamepad_open_allowed(mode: PopupMode) -> bool {
    mode != PopupMode::Manual
}

#[allow(clippy::too_many_arguments)]
fn handle_popup_keysym(
    keysym: u32,
    surfaces: &mut SurfaceManager,
    config: &Config,
    snapshot: &Snapshot,
    niri_tx: &crossbeam_channel::Sender<NiriRequest>,
    panel: &mut PanelState,
    selection: &mut Selection,
    suppress_peek: &mut Instant,
) -> Result<bool> {
    match keysym {
        KEY_ESCAPE => {
            if config.behavior.close_on_escape {
                surfaces.hide_all_popups()?;
                panel.outputs_mut().clear();
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
            let (submitted, close) = if let Some(window_id) = selected_window(selection, snapshot) {
                (
                    submit_niri_command(niri_tx, NiriCommand::FocusWindow(window_id)),
                    config.behavior.close_on_window_click,
                )
            } else if let Some(workspace_id) = selected_workspace(selection, snapshot) {
                (
                    submit_niri_command(niri_tx, NiriCommand::FocusWorkspace(workspace_id)),
                    config.behavior.close_on_workspace_click,
                )
            } else {
                (false, false)
            };
            if submitted {
                // The queued move is resolved after successful submission. If
                // the worker is unavailable, preserve the staged selection so
                // the user can retry.
                selection.clear_session();
                *suppress_peek = Instant::now() + SELF_FOCUS_GRACE;
            }
            if submitted && close {
                surfaces.hide_all_popups()?;
                panel.outputs_mut().clear();
            }
        }
        _ => {}
    }
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
fn handle_gamepad_action(
    action: GamepadAction,
    renderer: &mut Renderer,
    surfaces: &mut SurfaceManager,
    config: &Config,
    snapshot: &Snapshot,
    niri_tx: &crossbeam_channel::Sender<NiriRequest>,
    panel: &mut PanelState,
    selection: &mut Selection,
    suppress_peek: &mut Instant,
) -> Result<bool> {
    if action == GamepadAction::Open {
        if !gamepad_open_allowed(panel.mode()) {
            return Ok(false);
        }
        show_manual_popup(surfaces, renderer, snapshot, config, panel, selection)?;
        return Ok(true);
    }
    if panel.mode() != PopupMode::Manual {
        return Ok(false);
    }
    let Some(keysym) = gamepad_keysym(action) else {
        return Ok(false);
    };
    handle_popup_keysym(
        keysym,
        surfaces,
        config,
        snapshot,
        niri_tx,
        panel,
        selection,
        suppress_peek,
    )
}

fn release_gamepad_pointer_buttons(surfaces: &mut SurfaceManager, buttons: &mut HashSet<u16>) {
    for button in buttons.drain() {
        surfaces.pointer_button(u32::from(button), false);
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_gamepad_event(
    event: GamepadEvent,
    mode: GamepadMode,
    renderer: &mut Renderer,
    surfaces: &mut SurfaceManager,
    config: &Config,
    snapshot: &Snapshot,
    niri_tx: &crossbeam_channel::Sender<NiriRequest>,
    panel: &mut PanelState,
    selection: &mut Selection,
    suppress_peek: &mut Instant,
    pointer_buttons: &mut HashSet<u16>,
    keyboard_emitter: &mut Option<KeyboardEmitter>,
) -> Result<bool> {
    match event {
        GamepadEvent::ModeChanged(_) => Ok(false),
        GamepadEvent::Xmb(action) => {
            if mode != GamepadMode::Navi {
                return Ok(false);
            }
            handle_gamepad_action(
                action,
                renderer,
                surfaces,
                config,
                snapshot,
                niri_tx,
                panel,
                selection,
                suppress_peek,
            )
        }
        GamepadEvent::Navi(action) => {
            if mode != GamepadMode::Navi {
                return Ok(false);
            }
            match action {
                NaviAction::XmbOpen => {
                    if panel.mode() == PopupMode::Manual {
                        return Ok(false);
                    }
                    handle_gamepad_action(
                        GamepadAction::Open,
                        renderer,
                        surfaces,
                        config,
                        snapshot,
                        niri_tx,
                        panel,
                        selection,
                        suppress_peek,
                    )
                }
                NaviAction::Niri(action) => {
                    if panel.mode() == PopupMode::Manual {
                        return Ok(false);
                    }
                    if submit_niri_command(niri_tx, NiriCommand::Navi(action)) {
                        *suppress_peek = Instant::now() + SELF_FOCUS_GRACE;
                    }
                    Ok(false)
                }
                NaviAction::Scroll(direction) => {
                    if panel.mode() != PopupMode::Manual {
                        surfaces.pointer_scroll(f64::from(direction));
                    }
                    Ok(false)
                }
                NaviAction::Pointer(_) => Ok(false),
                NaviAction::Move(..) => Ok(false),
                NaviAction::Key(_) => Ok(false),
            }
        }
        GamepadEvent::PointerMotion { dx, dy } => {
            if mode == GamepadMode::Navi {
                surfaces.pointer_motion(f64::from(dx), f64::from(dy));
            }
            Ok(false)
        }
        GamepadEvent::PointerButton { button, pressed } => {
            if mode != GamepadMode::Navi {
                return Ok(false);
            }
            if panel.mode() == PopupMode::Manual {
                if !pressed && pointer_buttons.remove(&button) {
                    surfaces.pointer_button(u32::from(button), false);
                }
                return Ok(false);
            }
            surfaces.pointer_button(u32::from(button), pressed);
            if pressed {
                pointer_buttons.insert(button);
            } else {
                pointer_buttons.remove(&button);
            }
            Ok(false)
        }
        GamepadEvent::PointerScroll { steps } => {
            if mode == GamepadMode::Navi && panel.mode() != PopupMode::Manual {
                surfaces.pointer_scroll(f64::from(steps));
            }
            Ok(false)
        }
        GamepadEvent::Keyboard { code, pressed } => {
            if mode == GamepadMode::Navi
                && let Some(emitter) = keyboard_emitter.as_mut()
            {
                emitter.emit(code, pressed)?;
            }
            Ok(false)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_ui_event(
    event: UiEvent,
    surfaces: &mut SurfaceManager,
    renderer: &mut Renderer,
    snapshot: &Snapshot,
    config: &Config,
    niri_tx: &crossbeam_channel::Sender<NiriRequest>,
    transactions: &mut DropTransactionState,
    popup_origin_focus: Option<u64>,
    panel: &mut PanelState,
    bar_scrolls: &mut HashMap<String, BarScrollState>,
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
                            panel.begin_press(PressState {
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
                        let submitted =
                            submit_niri_command(niri_tx, NiriCommand::FocusWorkspace(workspace_id));
                        if submitted {
                            *suppress_peek = Instant::now() + SELF_FOCUS_GRACE;
                        }
                        if submitted
                            && matches!(event.kind, SurfaceKind::Popup)
                            && config.behavior.close_on_workspace_click
                        {
                            surfaces.hide_all_popups()?;
                            panel.outputs_mut().clear();
                        }
                    }
                    HitTarget::Canvas { .. } => {}
                    HitTarget::Panel => {
                        let can_move = panel.mode() == PopupMode::Manual
                            && config.behavior.move_popup
                            || panel.mode() == PopupMode::PeekArmed && config.peek.move_popup;
                        if can_move
                            && matches!(event.kind, SurfaceKind::Popup)
                            && renderer.panel_grab_hit(
                                event.surface_id,
                                event.x,
                                event.y,
                                config.behavior.popup_grab_region,
                            )
                        {
                            panel.begin_press(PressState {
                                window_id: POPUP_MOVE_PRESS,
                                app_id: event.output.clone(),
                                start_x: event.global_x,
                                start_y: event.global_y,
                                current_x: event.global_x,
                                current_y: event.global_y,
                                dragging: false,
                            });
                        }
                    }
                }
            } else if matches!(event.kind, SurfaceKind::Popup)
                && should_close_manual_outside(config, panel.mode(), panel.press().is_some())
            {
                surfaces.hide_all_popups()?;
                panel.outputs_mut().clear();
                if should_replay_outside_click(config) {
                    surfaces.replay_pointer_click(event.global_x, event.global_y, button);
                }
            }
            Ok(true)
        }
        UiEventKind::Motion => {
            if let Some(press) = panel.press_mut() {
                let previous_x = press.current_x;
                let previous_y = press.current_y;
                press.current_x = event.global_x;
                press.current_y = event.global_y;
                let was_dragging = press.dragging;
                let dx = press.current_x - press.start_x;
                let dy = press.current_y - press.start_y;
                if drag_threshold_reached(
                    press.start_x,
                    press.start_y,
                    press.current_x,
                    press.current_y,
                    config.behavior.drag_threshold,
                ) {
                    press.dragging = true;
                }
                if press.dragging && press.popup_move() {
                    let moved_x = if was_dragging {
                        event.global_x - previous_x
                    } else {
                        dx
                    };
                    let moved_y = if was_dragging {
                        event.global_y - previous_y
                    } else {
                        dy
                    };
                    renderer.move_popup(&press.app_id, moved_x, moved_y);
                }
                return Ok(true);
            }
            Ok(renderer.update_hover(event.surface_id, event.x, event.y))
        }
        UiEventKind::Enter => Ok(renderer.update_hover(event.surface_id, event.x, event.y)),
        UiEventKind::Leave => Ok(renderer.clear_hover(event.surface_id)),
        UiEventKind::Release { button } if button == BTN_LEFT => {
            let Some(released) = panel.take_press() else {
                return Ok(false);
            };
            if released.popup_move() {
                // Moving the panel is session state only and never dispatches
                // a Niri focus or window action, even below the threshold.
            } else if released.dragging {
                let surface_infos = surfaces.surfaces();
                if let Some((_surface, target, local_x, local_y)) =
                    renderer.hit_test_global(&surface_infos, released.current_x, released.current_y)
                {
                    let mode = panel.mode();
                    let is_open = panel.is_open();
                    let (drop_target, allow_column_join) =
                        drop_target_from_hit(target, local_x, local_y, snapshot, config);
                    let dropped = dispatch_drop(
                        released.window_id,
                        drop_target,
                        allow_column_join,
                        snapshot,
                        config,
                        if !is_open { None } else { popup_origin_focus },
                        if config.behavior.snap_cursor_after_drop {
                            None
                        } else {
                            Some((released.current_x, released.current_y))
                        },
                        transactions,
                        None,
                        niri_tx,
                        mode,
                        is_open,
                    );
                    if dropped {
                        *suppress_peek = Instant::now() + SELF_FOCUS_GRACE;
                    }
                    if dropped && config.behavior.close_on_drop && panel.is_open() {
                        surfaces.hide_all_popups()?;
                        panel.outputs_mut().clear();
                    }
                }
            } else {
                let submitted =
                    submit_niri_command(niri_tx, NiriCommand::FocusWindow(released.window_id));
                if submitted {
                    *suppress_peek = Instant::now() + SELF_FOCUS_GRACE;
                }
                if submitted
                    && matches!(event.kind, SurfaceKind::Popup)
                    && config.behavior.close_on_window_click
                {
                    surfaces.hide_all_popups()?;
                    panel.outputs_mut().clear();
                }
            }
            Ok(true)
        }
        UiEventKind::Key { keysym } if matches!(event.kind, SurfaceKind::Popup) => {
            handle_popup_keysym(
                keysym,
                surfaces,
                config,
                snapshot,
                niri_tx,
                panel,
                selection,
                suppress_peek,
            )
        }
        UiEventKind::Scroll {
            axis,
            steps,
            target,
        } => {
            let surface_infos = surfaces.surfaces();
            let hovered = match target {
                ScrollTarget::Hovered => surface_infos
                    .iter()
                    .find(|surface| surface.id == event.surface_id),
                ScrollTarget::Focused => None,
            };
            let Some(wheel_target) = resolve_wheel_target(
                hovered,
                snapshot.focused_output(),
                &surface_infos,
                panel.is_open(),
            ) else {
                return Ok(true);
            };
            match wheel_target {
                WheelTarget::Bar {
                    index,
                    output,
                    surface_id,
                } => {
                    if axis != ScrollAxis::Vertical {
                        return Ok(true);
                    }
                    if let Some(bar) = config.bars.get(index).filter(|bar| bar.scroll_to_switch) {
                        let mut delta = steps;
                        if bar.scroll_reverse {
                            delta = -delta;
                        }
                        let hovered_workspace =
                            surface_id.and_then(|id| renderer.workspace_at(id, event.x, event.y));
                        if focus_adjacent_workspace(
                            snapshot,
                            &output,
                            hovered_workspace,
                            delta,
                            bar.scroll_wrap,
                            Duration::from_millis(bar.scroll_repeat_ms),
                            bar_scrolls.entry(output.clone()).or_default(),
                            niri_tx,
                        ) {
                            // Bar wheel input is routed through niri-xmb. It is
                            // a direct widget command, not external workspace
                            // navigation that should raise a passive peek.
                            *suppress_peek = Instant::now() + SELF_FOCUS_GRACE;
                        }
                    }
                }
                WheelTarget::Popup { output } => {
                    // Keep the staged selection when the target panel is
                    // unchanged; only a panel/output transition starts from
                    // that output's live state.
                    selection.ensure_output(&output, snapshot);
                    let (workspace_delta, window_delta) = scroll_deltas(axis, steps);
                    move_selection(selection, snapshot, workspace_delta, window_delta);
                    follow_selection(config, selection, snapshot, niri_tx, suppress_peek);
                }
            }
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Step an index by `delta`, wrapping at the ends or stopping there.
///
/// The bar honoured `scroll_wrap` while the control socket hard-coded
/// wrapping, so the dock and the CLI ignored the setting and the same
/// gesture behaved differently depending on which surface received it.
/// Every path goes through here.
fn step_index(current: usize, delta: i32, count: usize, wrap: bool) -> usize {
    if count == 0 {
        return 0;
    }
    if wrap {
        wrap_index(current, delta, count)
    } else {
        (current as i32 + delta).clamp(0, count.saturating_sub(1) as i32) as usize
    }
}

/// The wheel wrap policy for `output`, taken from its bar.
///
/// An output with no configured bar keeps the default, which is what the
/// control socket did for every output before this.
fn scroll_wrap_for(config: &Config, output: &str) -> bool {
    config
        .bars
        .iter()
        .find(|bar| bar.output == output)
        .map_or_else(|| BarConfig::default().scroll_wrap, |bar| bar.scroll_wrap)
}

#[allow(clippy::too_many_arguments)]
fn focus_adjacent_workspace(
    snapshot: &Snapshot,
    output_name: &str,
    hovered_workspace: Option<u64>,
    delta: i32,
    wrap: bool,
    repeat: Duration,
    scroll: &mut BarScrollState,
    niri_tx: &crossbeam_channel::Sender<NiriRequest>,
) -> bool {
    let now = Instant::now();
    let Some(workspace_id) = queue_workspace_scroll(
        snapshot,
        output_name,
        hovered_workspace,
        delta,
        wrap,
        repeat,
        scroll,
        now,
    ) else {
        return false;
    };
    niri_tx
        .send(NiriCommand::FocusWorkspace(workspace_id).into())
        .is_ok()
}

/// Resolve wheel input against the last target requested by this bar, rather
/// than the last workspace Niri has reported. High-resolution wheel packets
/// arrive faster than compositor snapshots, so snapshot-only routing sends a
/// later packet to the wrong row.
#[allow(clippy::too_many_arguments)]
fn queue_workspace_scroll(
    snapshot: &Snapshot,
    output_name: &str,
    hovered_workspace: Option<u64>,
    delta: i32,
    wrap: bool,
    repeat: Duration,
    scroll: &mut BarScrollState,
    now: Instant,
) -> Option<u64> {
    if delta == 0 {
        return None;
    }
    if scroll.hovered_workspace != hovered_workspace {
        // Moving to another workspace cell starts a new gesture from that
        // cell. Do not let the prior bar target steer this scroll.
        scroll.target_workspace = hovered_workspace;
        scroll.last_step_at = None;
        scroll.hovered_workspace = hovered_workspace;
    }
    let rapid_packet = scroll
        .last_packet_at
        .is_some_and(|last| now.duration_since(last) < FREE_SPIN_PACKET_GAP);
    scroll.last_packet_at = Some(now);
    if rapid_packet {
        scroll.free_spin_until = Some(now + FREE_SPIN_IDLE);
    }
    let free_spinning = scroll.free_spin_until.is_some_and(|until| now < until);
    if free_spinning
        && scroll
            .last_step_at
            .is_some_and(|last| now.duration_since(last) < repeat)
    {
        return None;
    }
    let output = snapshot.outputs.get(output_name)?;
    let count = output.workspaces.len();
    if count == 0 {
        return None;
    }
    let gesture_start = scroll.last_step_at.is_none().then(|| {
        hovered_workspace.and_then(|workspace_id| {
            output
                .workspaces
                .iter()
                .position(|workspace| workspace.id == workspace_id)
        })
    });
    let pending_current = scroll
        .last_step_at
        .filter(|last| now.duration_since(*last) < BAR_SCROLL_TARGET_GRACE)
        .and(scroll.target_workspace)
        .and_then(|workspace_id| {
            output
                .workspaces
                .iter()
                .position(|workspace| workspace.id == workspace_id)
        });
    let current = gesture_start
        .flatten()
        .or(pending_current)
        .unwrap_or_else(|| {
            output
                .workspaces
                .iter()
                .position(|workspace| workspace.focused || workspace.active)
                .unwrap_or(0)
        });
    let next = step_index(current, delta, count, wrap);
    scroll.target_workspace = Some(output.workspaces[next].id);
    scroll.last_step_at = Some(now);
    (next != current).then_some(output.workspaces[next].id)
}

/// Whether the bar showing this workspace lets a centre drop join a column.
fn column_join_allowed(snapshot: &Snapshot, config: &Config, workspace_id: u64) -> bool {
    snapshot
        .outputs
        .values()
        .find(|output| {
            output
                .workspaces
                .iter()
                .any(|workspace| workspace.id == workspace_id)
        })
        .and_then(|output| config.bars.iter().find(|bar| bar.output == output.name))
        .is_some_and(|bar| bar.allow_column_join)
}

/// Turn a pointer hit into a drop target.
///
/// Only the pointer has pixels to convert. The control socket already
/// speaks in fractions and builds its target directly, rather than
/// inventing a unit rectangle for `normalized_fraction` to undo.
fn drop_target_from_hit(
    target: &HitTarget,
    local_x: f64,
    local_y: f64,
    snapshot: &Snapshot,
    config: &Config,
) -> (DropTarget, bool) {
    match target {
        HitTarget::Panel => (DropTarget::Panel, false),
        HitTarget::Workspace { workspace_id, .. } => (
            DropTarget::Workspace {
                workspace_id: *workspace_id,
            },
            false,
        ),
        HitTarget::Canvas {
            workspace_id, rect, ..
        } => (
            DropTarget::Canvas {
                workspace_id: *workspace_id,
                point: normalized_drop_point(*rect, local_x, local_y),
            },
            false,
        ),
        HitTarget::Window {
            workspace_id,
            window_id,
            rect,
            canvas,
            ..
        } => (
            DropTarget::Window {
                workspace_id: *workspace_id,
                window_id: *window_id,
                fraction: normalized_fraction(local_x as f32, rect.x, rect.width),
                canvas_point: canvas.map(|canvas| normalized_drop_point(canvas, local_x, local_y)),
            },
            column_join_allowed(snapshot, config, *workspace_id),
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn dispatch_drop(
    source_window_id: u64,
    target: DropTarget,
    allow_column_join: bool,
    snapshot: &Snapshot,
    config: &Config,
    preserved_focus: Option<u64>,
    restore_cursor: Option<(f64, f64)>,
    transactions: &mut DropTransactionState,
    reply: Option<crossbeam_channel::Sender<String>>,
    niri_tx: &crossbeam_channel::Sender<NiriRequest>,
    panel_mode: PopupMode,
    panel_visible: bool,
) -> bool {
    tracing::info!(
        source = source_window_id,
        target = %target.describe(),
        generation = snapshot.generation,
        panel = %popup_mode_name(panel_mode),
        panel_visible,
        "drop requested"
    );
    let request = DropRequest {
        source_window_id,
        target,
        policy: DropPolicy {
            allow_column_join,
            empty_drop_floats: config.behavior.empty_drop_floats,
            floating_drop_tiles: config.behavior.floating_drop_tiles,
            focus_after_drop: config.behavior.focus_after_drop,
        },
        preserved_focus,
        restore_cursor,
    };
    let outcome = match plan_drop(snapshot, request) {
        Ok(plan) => {
            // The full plan is a trace-level structure dump; ids and columns
            // only, never titles.
            tracing::debug!(
                plan = ?plan,
                postcondition = %plan.confirmation.describe(),
                "drop plan built"
            );
            transactions
                .submit(plan, snapshot, reply.clone(), niri_tx)
                .map_err(|error| {
                    tracing::warn!(%error, "drop transaction rejected");
                    format!("error: {error}\n")
                })
        }
        Err(error) => {
            tracing::info!(source = source_window_id, reason = %error, "drop rejected");
            Err(format!("error: drop rejected: {error}\n"))
        }
    };
    // A reply staged for the transaction outcome must still arrive when the
    // drop never became a transaction.
    if let Some(reply) = reply
        && let Err(response) = &outcome
    {
        let _ = reply.send(response.clone());
    }
    outcome.is_ok()
}

fn normalized_drop_point(rect: render::Rect, local_x: f64, local_y: f64) -> DropPoint {
    DropPoint {
        x: normalized_fraction(local_x as f32, rect.x, rect.width),
        y: normalized_fraction(local_y as f32, rect.y, rect.height),
    }
}

/// Hold a fraction inside the unit range, leaving a non-finite value alone
/// so the planner rejects it rather than silently reading it as an edge.
fn clamp_fraction(fraction: f32) -> f32 {
    if fraction.is_finite() {
        fraction.clamp(0.0, 1.0)
    } else {
        fraction
    }
}

fn normalized_fraction(value: f32, origin: f32, length: f32) -> f32 {
    clamp_fraction((value - origin) / length.max(1.0))
}

/// In live mode, apply the selection to Niri as soon as it moves.
///
/// In deferred mode this does nothing: the move stays queued in the popup
/// until Enter or a click confirms it.
fn follow_selection(
    config: &Config,
    selection: &Selection,
    snapshot: &Snapshot,
    niri_tx: &crossbeam_channel::Sender<NiriRequest>,
    suppress_peek: &mut Instant,
) {
    if config.behavior.navigation != NavigationMode::Live {
        return;
    }
    let submitted = if let Some(window_id) = selected_window(selection, snapshot) {
        submit_niri_command(niri_tx, NiriCommand::FocusWindow(window_id))
    } else if let Some(workspace_id) = selected_workspace(selection, snapshot) {
        // An empty workspace has nothing to focus, but is still reachable.
        submit_niri_command(niri_tx, NiriCommand::FocusWorkspace(workspace_id))
    } else {
        return;
    };
    if submitted {
        *suppress_peek = Instant::now() + SELF_FOCUS_GRACE;
    }
}

fn submit_niri_command(
    niri_tx: &crossbeam_channel::Sender<NiriRequest>,
    command: NiriCommand,
) -> bool {
    if niri_tx.send(command.into()).is_ok() {
        true
    } else {
        tracing::error!("Niri action worker unavailable");
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{OutputModel, WindowModel, WorkspaceModel};
    use std::collections::BTreeMap;

    #[test]
    fn a_new_subscriber_receives_the_last_snapshot_published_while_idle() {
        use std::io::{BufRead, BufReader};
        use std::os::unix::net::UnixStream;

        let subscriptions = Arc::new(subscribe::SubscriptionHub::new());
        let snapshot = protocol_snapshot();
        publish_snapshot(&snapshot, &subscriptions, None);

        let (client, server) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        subscriptions.attach(server, subscribe::STREAM_PROTOCOL);
        let mut reader = BufReader::new(client);
        let mut line = String::new();

        reader.read_line(&mut line).expect("hello frame");
        let hello: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(hello["type"], "hello");

        line.clear();
        reader
            .read_line(&mut line)
            .expect("a new subscriber must receive the last published snapshot");
        let frame: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(frame["type"], "snapshot");
        assert_eq!(frame["generation"], snapshot.generation);
    }

    fn protocol_snapshot() -> Snapshot {
        let window = WindowModel {
            id: 42,
            workspace_id: 7,
            app_id: "org.example.App".into(),
            title: "Example".into(),
            focused: true,
            urgent: false,
            floating: false,
            column: Some(2),
            tile_width: 800.0,
            tile_height: 600.0,
            x: Some(10.0),
            y: Some(20.0),
        };
        Snapshot {
            outputs: BTreeMap::from([(
                "eDP-1".into(),
                OutputModel {
                    name: "eDP-1".into(),
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1080,
                    workspaces: vec![WorkspaceModel {
                        id: 7,
                        index: 1,
                        name: Some("main".into()),
                        active: true,
                        focused: true,
                        urgent: false,
                        active_window: Some(42),
                        columns: BTreeMap::from([(2, BTreeMap::from([(3, window)]))]),
                        floating: Vec::new(),
                    }],
                },
            )]),
            focused_window: Some(42),
            overview_open: false,
            generation: 9,
            content_revision: 0,
        }
    }

    #[test]
    fn snapshot_json_schema_is_stable() {
        let value: serde_json::Value =
            serde_json::from_str(&snapshot_json(&protocol_snapshot(), None)).unwrap();

        assert_eq!(value["generation"], 9);
        assert_eq!(value["content_revision"], 0);
        assert_eq!(value["focused_window"], 42);
        assert_eq!(value["outputs"][0]["name"], "eDP-1");
        assert_eq!(value["outputs"][0]["workspaces"][0]["label"], "main");
        assert_eq!(value["outputs"][0]["workspaces"][0]["windows"][0]["id"], 42);
        assert_eq!(
            value["outputs"][0]["workspaces"][0]["windows"][0]["column"],
            2
        );
        assert_eq!(value["outputs"][0]["workspaces"][0]["windows"][0]["row"], 3);
        assert_eq!(
            value.as_object().unwrap().keys().collect::<Vec<_>>(),
            vec![
                "content_revision",
                "focused_window",
                "generation",
                "outputs"
            ]
        );
    }

    #[test]
    fn snapshot_filter_rejects_unknown_output() {
        let snapshot = protocol_snapshot();
        assert_eq!(
            snapshot_response(&snapshot, Some("missing")),
            "error: unknown output\n"
        );
        let value: serde_json::Value =
            serde_json::from_str(&snapshot_response(&snapshot, Some("eDP-1"))).unwrap();
        assert_eq!(value["outputs"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn status_response_reports_niri_health_additively() {
        let mut health = NiriHealth::default();
        health.apply(niri::NiriHealthUpdate::EventDisconnected(
            "stream lost".into(),
        ));
        health.apply(niri::NiriHealthUpdate::ActionFailed("focus failed".into()));
        let value: serde_json::Value = serde_json::from_str(&status_response(
            &HashSet::new(),
            PopupMode::Hidden,
            None,
            &protocol_snapshot(),
            true,
            &health,
            &DropTransactionState::default(),
            None,
            0,
        ))
        .unwrap();

        assert_eq!(value["visible"], false);
        assert_eq!(value["mode"], "hidden");
        assert_eq!(value["cursor_restore"], true);
        assert_eq!(value["niri"]["event_stream_connected"], false);
        assert_eq!(value["niri"]["last_event_error"], "stream lost");
        assert_eq!(value["niri"]["last_action_error"], "focus failed");

        // The dock discovers the mirror through status; without this it would
        // have to keep forking a process every tick.
        assert!(
            value.get("snapshot_mirror").is_some(),
            "status must publish where the snapshot mirror lives"
        );
    }

    #[test]
    fn status_response_reports_gamepad_mode() {
        let value: serde_json::Value = serde_json::from_str(&status_response(
            &HashSet::new(),
            PopupMode::Hidden,
            None,
            &protocol_snapshot(),
            true,
            &NiriHealth::default(),
            &DropTransactionState::default(),
            Some(GamepadStatus {
                mode: GamepadMode::Navi,
                preset: crate::config::GamepadPreset::Auto,
                open_combo: true,
                devices: 1,
                connected: vec![gamepad::GamepadDeviceStatus {
                    path: "/dev/input/event10".into(),
                    name: "Flydigi VADER4".into(),
                    preset: crate::config::GamepadPreset::Xbox,
                    vendor: 0x37d7,
                    product: 0x2401,
                }],
                live: vec![gamepad::GamepadDeviceLive {
                    path: "/dev/input/event10".into(),
                    buttons: [evdev::KeyCode::BTN_SOUTH.0].into_iter().collect(),
                    axes: [(evdev::AbsoluteAxisCode::ABS_Z.0, 0.5)]
                        .into_iter()
                        .collect(),
                }],
                discovery: None,
            }),
            0,
        ))
        .unwrap();

        assert_eq!(value["gamepad"]["enabled"], true);
        assert_eq!(value["gamepad"]["mode"], "navi");
        assert_eq!(value["gamepad"]["preset"], "auto");
        assert_eq!(value["gamepad"]["open_combo"], true);
        assert_eq!(value["gamepad"]["devices"], 1);
        assert_eq!(value["gamepad"]["connected"][0]["name"], "Flydigi VADER4");
        assert_eq!(value["gamepad"]["connected"][0]["vendor"], 0x37d7);
        assert_eq!(value["gamepad"]["connected"][0]["product"], 0x2401);
        assert_eq!(value["gamepad"]["live"][0]["buttons"][0], "BTN_SOUTH");
        assert_eq!(value["gamepad"]["live"][0]["axes"]["ABS_Z"], 0.5);
    }

    #[test]
    fn config_check_and_json_are_cli_subcommands() {
        assert!(matches!(
            Cli::try_parse_from(["niri-xmb", "config-check"])
                .unwrap()
                .command,
            Some(Command::ConfigCheck)
        ));
        assert!(matches!(
            Cli::try_parse_from(["niri-xmb", "config-json"])
                .unwrap()
                .command,
            Some(Command::ConfigJson)
        ));
    }

    #[test]
    fn gui_is_available_as_a_cli_subcommand() {
        let cli = Cli::try_parse_from(["niri-xmb", "gui"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Gui)));

        let cli =
            Cli::try_parse_from(["niri-xmb", "gui", "--config", "/tmp/niri-xmb.toml"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Gui)));
        assert_eq!(cli.config, Some(PathBuf::from("/tmp/niri-xmb.toml")));
    }

    #[test]
    fn restart_is_available_as_a_cli_subcommand() {
        let cli = Cli::try_parse_from(["niri-xmb", "restart"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Restart)));
    }

    #[test]
    fn update_parses_version_restart_policy_and_confirmation() {
        let cli = Cli::try_parse_from([
            "niri-xmb",
            "update",
            "--version",
            "v0.2.0",
            "--no-restart",
            "--yes",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Update {
                version: Some(ref version),
                no_restart: true,
                yes: true,
                force: false,
            }) if version == "v0.2.0"
        ));
    }

    #[test]
    fn update_version_comparison_accepts_v_prefix() {
        assert_eq!(
            plan_update("0.1.2", "v0.1.2", false).unwrap(),
            UpdatePlan::AlreadyCurrent
        );
        assert_eq!(
            plan_update("0.1.2", "0.1.2", false).unwrap(),
            UpdatePlan::AlreadyCurrent
        );
        assert_eq!(
            plan_update("0.1.1", "v0.1.2", false).unwrap(),
            UpdatePlan::Upgrade
        );
    }

    #[test]
    fn update_refuses_downgrade() {
        let error = plan_update("0.1.2", "v0.1.1", false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("--force"), "unexpected error: {error}");
        assert_eq!(
            plan_update("0.1.2", "v0.1.1", true).unwrap(),
            UpdatePlan::ForcedDowngrade
        );
        // Ordering is numeric, not lexicographic: 0.1.10 is newer than 0.1.9.
        assert_eq!(
            plan_update("0.1.9", "v0.1.10", false).unwrap(),
            UpdatePlan::Upgrade
        );
        assert!(plan_update("0.1.2", "not-a-version", false).is_err());
    }

    #[test]
    fn update_confirmation_accepts_only_explicit_yes() {
        assert!(confirmation_is_yes("y"));
        assert!(confirmation_is_yes(" YES \n"));
        assert!(!confirmation_is_yes(""));
        assert!(!confirmation_is_yes("no"));
    }

    #[test]
    fn verbosity_count_maps_to_log_filters() {
        assert_eq!(log_filter(0), "niri_xmb=info");
        assert_eq!(log_filter(1), "niri_xmb=debug");
        assert_eq!(log_filter(2), "niri_xmb=trace");
        assert_eq!(log_filter(9), "niri_xmb=trace");
    }

    #[test]
    fn only_a_non_empty_rust_log_overrides_verbosity() {
        assert_eq!(effective_rust_log(Some("trace")), Some("trace".into()));
        assert_eq!(
            effective_rust_log(Some("  niri_xmb=debug  ")),
            Some("niri_xmb=debug".into())
        );
        assert_eq!(effective_rust_log(None), None);
        assert_eq!(effective_rust_log(Some("")), None);
        assert_eq!(effective_rust_log(Some("   ")), None);
    }

    #[test]
    fn cli_parses_repeated_verbosity_before_and_after_the_subcommand() {
        let cli = Cli::try_parse_from(["niri-xmb", "-vv", "restart"]).unwrap();
        assert_eq!(cli.verbose, 2);
        let cli = Cli::try_parse_from(["niri-xmb", "restart", "-v"]).unwrap();
        assert_eq!(cli.verbose, 1);
        let cli = Cli::try_parse_from(["niri-xmb"]).unwrap();
        assert_eq!(cli.verbose, 0);
    }

    #[test]
    fn restart_arguments_carry_config_and_verbosity_to_the_daemon() {
        let path = std::path::Path::new("/tmp/niri-xmb.toml");
        assert_eq!(
            daemon_spawn_args(path, 0),
            vec!["--config", "/tmp/niri-xmb.toml", "daemon"]
        );
        assert_eq!(
            daemon_spawn_args(path, 1),
            vec!["--config", "/tmp/niri-xmb.toml", "-v", "daemon"]
        );
        // Levels past trace collapse so a long flag chain cannot grow the
        // spawned command line without bound.
        assert_eq!(
            daemon_spawn_args(path, 7),
            vec!["--config", "/tmp/niri-xmb.toml", "-v", "-v", "daemon"]
        );
    }

    #[test]
    fn rejected_drop_reply_names_the_exact_reason() {
        let snapshot = protocol_snapshot();
        let (niri_tx, _niri_rx) = crossbeam_channel::unbounded();
        let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
        let mut state = DropTransactionState::default();

        let submitted = dispatch_drop(
            42,
            DropTarget::Window {
                workspace_id: 7,
                window_id: 999,
                fraction: normalized_fraction(10.0, 0.0, 100.0),
                canvas_point: None,
            },
            false,
            &snapshot,
            &Config::default(),
            None,
            None,
            &mut state,
            Some(reply_tx),
            &niri_tx,
            PopupMode::Manual,
            true,
        );

        assert!(!submitted);
        assert_eq!(
            reply_rx.try_recv().unwrap(),
            "error: drop rejected: missing target window\n"
        );
    }

    #[test]
    fn unknown_workspace_drop_reply_names_the_workspace() {
        let snapshot = protocol_snapshot();
        let (niri_tx, _niri_rx) = crossbeam_channel::unbounded();
        let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
        let mut state = DropTransactionState::default();

        let submitted = dispatch_drop(
            42,
            DropTarget::Canvas {
                workspace_id: 999,
                point: DropPoint {
                    x: normalized_fraction(0.5, 0.0, 1.0),
                    y: normalized_fraction(0.5, 0.0, 1.0),
                },
            },
            false,
            &snapshot,
            &Config::default(),
            None,
            None,
            &mut state,
            Some(reply_tx),
            &niri_tx,
            PopupMode::Hidden,
            false,
        );

        assert!(!submitted);
        assert_eq!(
            reply_rx.try_recv().unwrap(),
            "error: drop rejected: unknown workspace\n"
        );
    }

    /// Manual capture follows `close_on_outside_click`, and replay remains
    /// an independent post-dismissal option.
    #[test]
    fn manual_outside_capture_follows_close_and_replay_settings() {
        let mut config = Config::default();
        config.behavior.close_on_outside_click = true;

        config.behavior.replay_outside_click = true;
        assert!(popup_capture_outside(
            &config,
            PopupMode::Manual,
            PopupInput::FULL
        ));
        assert!(popup_capture_outside(
            &config,
            PopupMode::Manual,
            PopupInput::POINTER
        ));
        assert!(should_close_manual_outside(
            &config,
            PopupMode::Manual,
            false
        ));
        assert!(!should_close_manual_outside(
            &config,
            PopupMode::Manual,
            true
        ));
        assert!(should_replay_outside_click(&config));

        config.behavior.replay_outside_click = false;
        assert!(popup_capture_outside(
            &config,
            PopupMode::Manual,
            PopupInput::FULL
        ));
        assert!(popup_capture_outside(
            &config,
            PopupMode::Manual,
            PopupInput::POINTER
        ));
        assert!(!should_replay_outside_click(&config));

        config.behavior.close_on_outside_click = false;
        assert!(!popup_capture_outside(
            &config,
            PopupMode::Manual,
            PopupInput::FULL
        ));
        assert!(!popup_capture_outside(
            &config,
            PopupMode::Manual,
            PopupInput::POINTER
        ));
        assert!(!should_close_manual_outside(
            &config,
            PopupMode::Manual,
            false
        ));
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
    fn empty_bar_list_auto_detects_every_connected_output() {
        let mut config = Config {
            bars_enabled: true,
            bars: vec![],
            ..Default::default()
        };

        resolve_bars(&mut config, &["eDP-1".to_string(), "DP-3".to_string()]);

        let outputs: Vec<&str> = config.bars.iter().map(|bar| bar.output.as_str()).collect();
        assert_eq!(outputs, vec!["eDP-1", "DP-3"]);
    }

    /// The shipped example is what the README tells a new user to copy, so it
    /// has to produce bars on hardware that is not the author's.
    #[test]
    fn shipped_example_auto_detects_bars_on_unfamiliar_outputs() {
        let source = include_str!("../../../config/niri-xmb.toml.example");
        let mut config: Config = toml::from_str(source).unwrap();
        config.validate().unwrap();

        resolve_bars(&mut config, &["eDP-1".to_string(), "DP-3".to_string()]);

        let outputs: Vec<&str> = config.bars.iter().map(|bar| bar.output.as_str()).collect();
        assert_eq!(outputs, vec!["eDP-1", "DP-3"]);
    }

    #[test]
    fn bars_naming_disconnected_outputs_are_dropped() {
        let mut config = Config {
            bars_enabled: true,
            bars: vec![
                BarConfig {
                    output: "HDMI-A-1".to_string(),
                    ..BarConfig::default()
                },
                BarConfig {
                    output: "eDP-1".to_string(),
                    ..BarConfig::default()
                },
            ],
            ..Default::default()
        };

        resolve_bars(&mut config, &["eDP-1".to_string()]);

        let outputs: Vec<&str> = config.bars.iter().map(|bar| bar.output.as_str()).collect();
        assert_eq!(outputs, vec!["eDP-1"]);

        // Every bar disconnected must not silently re-auto-detect.
        config.bars = vec![BarConfig {
            output: "HDMI-A-1".to_string(),
            ..BarConfig::default()
        }];
        resolve_bars(&mut config, &["eDP-1".to_string()]);
        assert!(config.bars.is_empty());
    }

    /// The loop sleeps until the soonest thing it is waiting for, and an
    /// already-passed deadline means do not sleep at all.
    #[test]
    fn deadlines_take_the_soonest_wait() {
        let now = Instant::now();

        // Nothing registered: the idle wait stands.
        let deadlines = Deadlines::new(Duration::from_millis(250), now);
        assert_eq!(deadlines.wait(), Duration::from_millis(250));

        // The soonest of several wins, whichever order they arrive in.
        let mut deadlines = Deadlines::new(Duration::from_millis(250), now);
        deadlines.at(now + Duration::from_millis(90));
        deadlines.at(now + Duration::from_millis(30));
        deadlines.at(now + Duration::from_millis(60));
        assert_eq!(deadlines.wait(), Duration::from_millis(30));

        // A longer deadline never extends the wait.
        let mut deadlines = Deadlines::new(Duration::from_millis(250), now);
        deadlines.at(now + Duration::from_secs(5));
        assert_eq!(deadlines.wait(), Duration::from_millis(250));

        // `after` measures a total from a start instant.
        let mut deadlines = Deadlines::new(Duration::from_millis(250), now);
        deadlines.after(now - Duration::from_millis(80), Duration::from_millis(120));
        assert_eq!(deadlines.wait(), Duration::from_millis(40));

        // Anything already due means return immediately.
        let mut deadlines = Deadlines::new(Duration::from_millis(250), now);
        deadlines.at(now - Duration::from_millis(5));
        assert_eq!(deadlines.wait(), Duration::ZERO);
    }
}

#[cfg(test)]
mod ui_tests {
    use super::*;
    use crate::focus::live_window_ids;
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
            content_revision: 0,
        }
    }

    fn focused_dp2_snapshot() -> Snapshot {
        let mut snapshot = snapshot();
        snapshot.outputs.get_mut("DP-1").unwrap().workspaces[0].focused = false;
        let mut dp2_workspace = workspace(4, 2, 1);
        dp2_workspace.active = true;
        dp2_workspace.focused = true;
        snapshot.outputs.insert(
            "DP-2".into(),
            OutputModel {
                name: "DP-2".into(),
                x: 2560,
                y: 0,
                width: 1920,
                height: 1080,
                workspaces: vec![dp2_workspace],
            },
        );
        snapshot
    }

    fn surface_info(id: usize, kind: SurfaceKind, output: &str, configured: bool) -> SurfaceInfo {
        SurfaceInfo {
            id,
            kind,
            output: output.into(),
            global_x: 0.0,
            global_y: 0.0,
            width: 1920,
            height: 1080,
            configured,
            frame_ready: true,
            needs_full_redraw: false,
            scale_factor: 1,
        }
    }

    #[test]
    fn a_hovered_panel_wins_over_the_focused_output() {
        let snapshot = focused_dp2_snapshot();
        let surfaces = vec![
            surface_info(1, SurfaceKind::Bar(0), "DP-2", true),
            surface_info(2, SurfaceKind::Popup, "DP-1", true),
        ];
        assert_eq!(
            resolve_wheel_target(
                Some(&surfaces[1]),
                snapshot.focused_output(),
                &surfaces,
                true
            ),
            Some(WheelTarget::Popup {
                output: "DP-1".into()
            })
        );
    }

    #[test]
    fn pointer_away_falls_back_to_the_focused_output_bar() {
        let snapshot = focused_dp2_snapshot();
        let surfaces = vec![
            surface_info(1, SurfaceKind::Popup, "DP-1", true),
            surface_info(2, SurfaceKind::Bar(1), "DP-2", true),
        ];
        assert_eq!(
            resolve_wheel_target(None, snapshot.focused_output(), &surfaces, false),
            Some(WheelTarget::Bar {
                index: 1,
                output: "DP-2".into(),
                surface_id: None,
            })
        );
    }

    #[test]
    fn pointer_away_prefers_the_open_panel_on_the_focused_output() {
        let snapshot = focused_dp2_snapshot();
        let surfaces = vec![
            surface_info(1, SurfaceKind::Popup, "DP-1", true),
            surface_info(2, SurfaceKind::Popup, "DP-2", true),
        ];
        assert_eq!(
            resolve_wheel_target(None, snapshot.focused_output(), &surfaces, true),
            Some(WheelTarget::Popup {
                output: "DP-2".into()
            })
        );
    }

    #[test]
    fn pointer_away_uses_a_fixed_popup_when_the_focused_output_has_no_panel() {
        let snapshot = focused_dp2_snapshot();
        let surfaces = vec![
            surface_info(1, SurfaceKind::Popup, "DP-1", true),
            surface_info(2, SurfaceKind::Bar(1), "DP-2", true),
        ];
        // The open panel is the focused panel even when Niri focus moved on.
        assert_eq!(
            resolve_wheel_target(None, snapshot.focused_output(), &surfaces, true),
            Some(WheelTarget::Popup {
                output: "DP-1".into()
            })
        );
    }

    #[test]
    fn panel_transitions_switch_the_wheel_target_output() {
        let snapshot = focused_dp2_snapshot();
        let surfaces = vec![
            surface_info(1, SurfaceKind::Bar(0), "DP-1", true),
            surface_info(2, SurfaceKind::Bar(1), "DP-2", true),
        ];
        assert_eq!(
            resolve_wheel_target(
                Some(&surfaces[0]),
                snapshot.focused_output(),
                &surfaces,
                false
            ),
            Some(WheelTarget::Bar {
                index: 0,
                output: "DP-1".into(),
                surface_id: Some(1),
            })
        );
        assert_eq!(
            resolve_wheel_target(
                Some(&surfaces[1]),
                snapshot.focused_output(),
                &surfaces,
                false
            ),
            Some(WheelTarget::Bar {
                index: 1,
                output: "DP-2".into(),
                surface_id: Some(2),
            })
        );
        assert_eq!(
            resolve_wheel_target(None, snapshot.focused_output(), &surfaces, false),
            Some(WheelTarget::Bar {
                index: 1,
                output: "DP-2".into(),
                surface_id: None,
            })
        );
    }

    #[test]
    fn an_unconfigured_hovered_panel_falls_back_to_focus() {
        let snapshot = focused_dp2_snapshot();
        let surfaces = vec![
            surface_info(1, SurfaceKind::Popup, "DP-1", false),
            surface_info(2, SurfaceKind::Bar(0), "DP-2", true),
        ];
        assert_eq!(
            resolve_wheel_target(
                Some(&surfaces[0]),
                snapshot.focused_output(),
                &surfaces,
                true
            ),
            Some(WheelTarget::Bar {
                index: 0,
                output: "DP-2".into(),
                surface_id: None,
            })
        );
    }

    #[test]
    fn no_panel_means_no_wheel_target() {
        let snapshot = focused_dp2_snapshot();
        let surfaces = vec![surface_info(1, SurfaceKind::Popup, "DP-1", true)];
        assert_eq!(
            resolve_wheel_target(None, snapshot.focused_output(), &surfaces, false),
            None
        );
    }

    #[test]
    fn wheel_axes_keep_their_existing_deltas() {
        assert_eq!(scroll_deltas(ScrollAxis::Vertical, 3), (3, 0));
        assert_eq!(scroll_deltas(ScrollAxis::Horizontal, -2), (0, -2));
    }

    #[test]
    fn gamepad_actions_map_to_popup_keyboard_controls() {
        assert_eq!(gamepad_keysym(GamepadAction::Up), Some(KEY_UP));
        assert_eq!(gamepad_keysym(GamepadAction::Down), Some(KEY_DOWN));
        assert_eq!(gamepad_keysym(GamepadAction::Left), Some(KEY_LEFT));
        assert_eq!(gamepad_keysym(GamepadAction::Right), Some(KEY_RIGHT));
        assert_eq!(gamepad_keysym(GamepadAction::Tab), Some(KEY_TAB));
        assert_eq!(gamepad_keysym(GamepadAction::Back), Some(KEY_SHIFT_TAB));
        assert_eq!(gamepad_keysym(GamepadAction::Confirm), Some(KEY_RETURN));
        assert_eq!(gamepad_keysym(GamepadAction::Close), Some(KEY_ESCAPE));
        assert_eq!(gamepad_keysym(GamepadAction::Open), None);
    }

    #[test]
    fn gamepad_open_is_ignored_when_already_manual() {
        assert!(gamepad_open_allowed(PopupMode::Hidden));
        assert!(gamepad_open_allowed(PopupMode::Peek));
        assert!(gamepad_open_allowed(PopupMode::PeekArmed));
        assert!(!gamepad_open_allowed(PopupMode::Manual));
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

    fn popup_event(event: UiEventKind) -> UiEvent {
        UiEvent {
            surface_id: 1,
            kind: SurfaceKind::Popup,
            output: "DP-1".into(),
            x: 10.0,
            y: 10.0,
            global_x: 10.0,
            global_y: 10.0,
            event,
        }
    }

    /// The input region becoming live under a resting pointer produces an
    /// enter. Arming on that would pin the overlay open for a pointer that
    /// never moved, so only real movement counts as reaching for it.
    #[test]
    fn a_resting_pointer_does_not_arm_the_peek_overlay() {
        assert_eq!(
            classify_peek_pointer(&popup_event(UiEventKind::Enter)),
            PeekPointer::Ignore
        );
        assert_eq!(
            classify_peek_pointer(&popup_event(UiEventKind::Motion)),
            PeekPointer::Arm
        );
    }

    #[test]
    fn a_focused_scroll_does_not_arm_the_peek_overlay() {
        assert_eq!(
            classify_peek_pointer(&popup_event(UiEventKind::Scroll {
                axis: ScrollAxis::Vertical,
                steps: 1,
                target: ScrollTarget::Hovered,
            })),
            PeekPointer::Arm
        );
        assert_eq!(
            classify_peek_pointer(&popup_event(UiEventKind::Scroll {
                axis: ScrollAxis::Vertical,
                steps: 1,
                target: ScrollTarget::Focused,
            })),
            PeekPointer::Ignore
        );
    }

    #[test]
    fn input_border_tracks_armed_input_not_passive_peek_visibility() {
        assert!(!panel_input_active(PopupMode::Hidden, false));
        assert!(!panel_input_active(PopupMode::Peek, true));
        assert!(!panel_input_active(PopupMode::PeekArmed, false));
        assert!(panel_input_active(PopupMode::PeekArmed, true));
        assert!(panel_input_active(PopupMode::Manual, false));
    }

    #[test]
    fn pointer_intent_maps_to_overlay_transitions() {
        assert_eq!(
            classify_peek_pointer(&popup_event(UiEventKind::Leave)),
            PeekPointer::Disarm
        );
        assert_eq!(
            classify_peek_pointer(&popup_event(UiEventKind::Press { button: BTN_LEFT })),
            PeekPointer::Promote
        );
        assert_eq!(
            classify_peek_pointer(&popup_event(UiEventKind::Press {
                button: BTN_LEFT + 1
            })),
            PeekPointer::Ignore
        );
        let mut bar = popup_event(UiEventKind::Motion);
        bar.kind = SurfaceKind::Bar(0);
        assert_eq!(classify_peek_pointer(&bar), PeekPointer::Ignore);
    }

    /// Full-output click capture is limited to manual mode with
    /// `close_on_outside_click` set. Peek and peek armed remain panel-only,
    /// including their pointer-only input.
    #[test]
    fn only_a_manual_session_may_capture_the_whole_output() {
        let mut config = Config::default();
        config.behavior.replay_outside_click = false;
        config.behavior.close_on_outside_click = true;

        assert!(popup_capture_outside(
            &config,
            PopupMode::Manual,
            PopupInput::FULL
        ));
        assert!(popup_capture_outside(
            &config,
            PopupMode::Manual,
            PopupInput::POINTER
        ));
        assert!(!popup_capture_outside(
            &config,
            PopupMode::Manual,
            PopupInput::NONE
        ));
        assert!(!popup_capture_outside(
            &config,
            PopupMode::Peek,
            PopupInput::POINTER
        ));
        assert!(!popup_capture_outside(
            &config,
            PopupMode::PeekArmed,
            PopupInput::POINTER
        ));
        assert!(!popup_capture_outside(
            &config,
            PopupMode::Peek,
            PopupInput::NONE
        ));

        config.behavior.close_on_outside_click = false;
        assert!(!popup_capture_outside(
            &config,
            PopupMode::Manual,
            PopupInput::FULL
        ));
    }

    #[test]
    fn outside_click_closes_manual_mode_but_waits_for_an_active_press() {
        let mut config = Config::default();
        config.behavior.close_on_outside_click = true;
        assert!(should_close_manual_outside(
            &config,
            PopupMode::Manual,
            false
        ));
        assert!(!should_close_manual_outside(
            &config,
            PopupMode::Manual,
            true
        ));
        assert!(!should_close_manual_outside(
            &config,
            PopupMode::Peek,
            false
        ));
        assert!(!should_close_manual_outside(
            &config,
            PopupMode::PeekArmed,
            false
        ));
        config.behavior.close_on_outside_click = false;
        assert!(!should_close_manual_outside(
            &config,
            PopupMode::Manual,
            false
        ));
    }

    #[test]
    fn manual_capture_applies_when_the_popup_has_multiple_output_surfaces() {
        let mut config = Config::default();
        config.popup.outputs = PopupOutputs::All;
        let mut snapshot = snapshot();
        snapshot.outputs.insert(
            "HDMI-A-1".into(),
            OutputModel {
                name: "HDMI-A-1".into(),
                x: 2560,
                y: 0,
                width: 1920,
                height: 1080,
                workspaces: Vec::new(),
            },
        );

        assert_eq!(
            popup_output_targets(&snapshot, &config),
            ["DP-1", "HDMI-A-1"]
        );
        assert!(popup_capture_outside(
            &config,
            PopupMode::Manual,
            PopupInput::FULL
        ));
        assert!(popup_capture_outside(
            &config,
            PopupMode::Manual,
            PopupInput::POINTER
        ));
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

    #[test]
    fn bar_scroll_advances_from_the_last_queued_workspace_target() {
        let snapshot = snapshot();
        let now = Instant::now();
        let repeat = Duration::from_millis(75);
        let mut scroll = BarScrollState::default();

        // Niri has not sent a newer snapshot yet. A second wheel packet must
        // still advance from workspace 2 to 3, not re-target workspace 2.
        assert_eq!(
            queue_workspace_scroll(
                &snapshot,
                "DP-1",
                Some(1),
                1,
                true,
                repeat,
                &mut scroll,
                now,
            ),
            Some(2)
        );
        assert_eq!(
            queue_workspace_scroll(
                &snapshot,
                "DP-1",
                Some(1),
                1,
                true,
                repeat,
                &mut scroll,
                now + Duration::from_millis(12),
            ),
            None,
            "free-spin packets inside the repeat limit must not flood Niri"
        );
        assert_eq!(
            queue_workspace_scroll(
                &snapshot,
                "DP-1",
                Some(1),
                1,
                true,
                repeat,
                &mut scroll,
                now + Duration::from_millis(80),
            ),
            Some(3)
        );
    }

    #[test]
    fn bar_scroll_starts_from_the_hovered_workspace() {
        let snapshot = snapshot();
        let now = Instant::now();
        let mut scroll = BarScrollState::default();

        // Workspace 1 is active, but the pointer is over workspace 2. Scroll
        // from the hovered row, not from the active row.
        assert_eq!(
            queue_workspace_scroll(
                &snapshot,
                "DP-1",
                Some(2),
                1,
                true,
                Duration::from_millis(75),
                &mut scroll,
                now,
            ),
            Some(3)
        );
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

    /// The bar and the control socket must agree about what happens at the
    /// end of the list; the socket used to always wrap regardless of config.
    #[test]
    fn stepping_honours_the_wrap_setting_at_both_ends() {
        // Wrapping.
        assert_eq!(step_index(2, 1, 3, true), 0);
        assert_eq!(step_index(0, -1, 3, true), 2);
        // Stopping.
        assert_eq!(step_index(2, 1, 3, false), 2);
        assert_eq!(step_index(0, -1, 3, false), 0);
        // Interior moves are the same either way.
        assert_eq!(step_index(0, 1, 3, true), 1);
        assert_eq!(step_index(0, 1, 3, false), 1);
        // Multi-step deltas, which the dock now sends after coalescing.
        assert_eq!(step_index(0, 2, 3, false), 2);
        assert_eq!(step_index(0, 5, 3, false), 2);
        assert_eq!(step_index(0, 4, 3, true), 1);
        // An empty list must not panic.
        assert_eq!(step_index(0, 1, 0, true), 0);
        assert_eq!(step_index(0, 1, 0, false), 0);
    }

    /// An output with a bar follows that bar's setting; one without keeps the
    /// default, which is what every output used to get.
    #[test]
    fn wrap_policy_comes_from_the_bar_for_that_output() {
        let config = Config {
            bars: vec![
                BarConfig {
                    output: "eDP-1".into(),
                    scroll_wrap: false,
                    ..BarConfig::default()
                },
                BarConfig {
                    output: "DP-2".into(),
                    scroll_wrap: true,
                    ..BarConfig::default()
                },
            ],
            ..Config::default()
        };

        assert!(!scroll_wrap_for(&config, "eDP-1"));
        assert!(scroll_wrap_for(&config, "DP-2"));
        assert_eq!(
            scroll_wrap_for(&config, "HDMI-A-9"),
            BarConfig::default().scroll_wrap
        );
    }
}
#[test]
fn popup_move_uses_the_existing_drag_threshold() {
    assert!(!drag_threshold_reached(0.0, 0.0, 3.0, 4.0, 7.0));
    assert!(drag_threshold_reached(0.0, 0.0, 5.0, 5.0, 7.0));
}

#[test]
fn popup_move_press_cannot_be_treated_as_a_window_drag() {
    let press = PressState {
        window_id: POPUP_MOVE_PRESS,
        app_id: "DP-1".into(),
        start_x: 10.0,
        start_y: 10.0,
        current_x: 30.0,
        current_y: 30.0,
        dragging: true,
    };
    assert!(press.popup_move());
}
