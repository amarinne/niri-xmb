mod anim;
mod config;
mod draw;
mod drop_plan;
mod icons;
mod ipc;
mod model;
mod niri;
mod panel;
mod render;
mod selection;
mod subscribe;
mod text;
mod wayland;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use config::{BarConfig, Config, NavigationMode, PopupOutputs, PopupPosition, PopupSurfaceMode};
use crossbeam_channel::unbounded;
use drop_plan::{DropPoint, DropPolicy, DropRequest, DropTarget, plan_drop};
use ipc::{ControlCommand, ControlRequest, DROP_CONFIRM_TIMEOUT};
use model::Snapshot;
use niri::{
    ActionStep, ActionTransactionResult, NiriCommand, NiriHealth, NiriRequest, RestorationStatus,
    spawn as spawn_niri,
};
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
use wayland::{PopupInput, SurfaceKind, SurfaceManager, UiEvent, UiEventKind, WakePipe};

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

/// Let events from a failed partial action sequence reach the canonical model.
const DROP_FAILURE_RECONCILE: Duration = Duration::from_millis(120);

/// Keep a bar wheel burst on its queued workspace target while Niri catches
/// up. Without this, each high-resolution packet starts from an old snapshot
/// and can focus the wrong workspace.
const BAR_SCROLL_TARGET_GRACE: Duration = Duration::from_millis(500);

/// Adjacent high-resolution packets identify the free-spinning wheel. A
/// ratchet click arrives after this gap and must not be rate limited.
const FREE_SPIN_PACKET_GAP: Duration = Duration::from_millis(30);

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

/// Last workspace requested by wheel input on one output.
#[derive(Default)]
struct BarScrollState {
    target_workspace: Option<u64>,
    last_step_at: Option<Instant>,
    hovered_workspace: Option<u64>,
    last_packet_at: Option<Instant>,
    free_spin_until: Option<Instant>,
}

#[derive(Default)]
struct DropTransactionState {
    next_request_id: u64,
    pending: Option<PendingDropTransaction>,
    last: Option<CompletedDropTransaction>,
}

struct PendingDropTransaction {
    request_id: u64,
    plan: drop_plan::DropPlan,
    submitted_at: Instant,
    action_result: Option<ActionTransactionResult>,
    action_result_at: Option<Instant>,
    reply: Option<crossbeam_channel::Sender<String>>,
}

struct CompletedDropTransaction {
    request_id: u64,
    base_generation: u64,
    final_generation: u64,
    outcome: DropOutcome,
    action_result: Option<ActionTransactionResult>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DropOutcome {
    Confirmed,
    ActionFailed(String),
    ConfirmationTimedOut,
    EventStreamUnavailable,
}

impl DropTransactionState {
    fn submit(
        &mut self,
        plan: drop_plan::DropPlan,
        snapshot: &Snapshot,
        reply: Option<crossbeam_channel::Sender<String>>,
        niri_tx: &crossbeam_channel::Sender<NiriRequest>,
    ) -> std::result::Result<u64, &'static str> {
        if self.pending.is_some() {
            return Err("drop transaction already active");
        }
        if plan.base_generation != snapshot.generation {
            return Err("stale drop plan");
        }
        self.next_request_id = self.next_request_id.wrapping_add(1).max(1);
        let request_id = self.next_request_id;
        tracing::info!(
            request_id,
            source = plan.source_window_id,
            base_generation = plan.base_generation,
            postcondition = %plan.confirmation.describe(),
            "drop transaction submitted"
        );
        let command = plan.clone().into_command();
        niri_tx
            .send(NiriRequest::Transaction {
                request_id,
                command,
            })
            .map_err(|_| "Niri command worker unavailable")?;
        self.pending = Some(PendingDropTransaction {
            request_id,
            plan,
            submitted_at: Instant::now(),
            action_result: None,
            action_result_at: None,
            reply,
        });
        Ok(request_id)
    }

    fn record_action_result(&mut self, result: ActionTransactionResult, now: Instant) {
        let Some(pending) = self.pending.as_mut() else {
            tracing::warn!(
                request_id = result.request_id,
                "orphan Niri transaction result"
            );
            return;
        };
        if pending.request_id != result.request_id {
            tracing::warn!(
                expected = pending.request_id,
                received = result.request_id,
                "out-of-order Niri transaction result"
            );
            return;
        }
        let steps = result
            .completed_steps
            .iter()
            .map(|step| action_step_name(*step))
            .collect::<Vec<_>>()
            .join(",");
        if let Some(failure) = &result.failure {
            tracing::warn!(
                request_id = result.request_id,
                completed_steps = %steps,
                failed_step = action_step_name(failure.step),
                error = %failure.error,
                "drop action step failed"
            );
        } else {
            tracing::debug!(
                request_id = result.request_id,
                completed_steps = %steps,
                "drop action steps completed"
            );
        }
        pending.action_result = Some(result);
        pending.action_result_at = Some(now);
    }

    fn reconcile(&mut self, snapshot: &Snapshot, event_stream_connected: bool, now: Instant) {
        let Some(pending) = self.pending.as_ref() else {
            return;
        };
        let Some(result) = pending.action_result.as_ref() else {
            return;
        };
        let result_at = pending.action_result_at.unwrap_or(pending.submitted_at);
        let outcome = if let Some(failure) = &result.failure {
            (now.duration_since(result_at) >= DROP_FAILURE_RECONCILE).then(|| {
                DropOutcome::ActionFailed(format!(
                    "{}: {}",
                    action_step_name(failure.step),
                    failure.error
                ))
            })
        } else if pending.plan.postcondition_matches(snapshot) {
            Some(DropOutcome::Confirmed)
        } else if !event_stream_connected {
            Some(DropOutcome::EventStreamUnavailable)
        } else if now.duration_since(result_at) >= DROP_CONFIRM_TIMEOUT {
            Some(DropOutcome::ConfirmationTimedOut)
        } else {
            None
        };
        if let Some(outcome) = outcome {
            self.finish(snapshot.generation, outcome);
        }
    }

    fn finish(&mut self, final_generation: u64, outcome: DropOutcome) {
        let Some(mut pending) = self.pending.take() else {
            return;
        };
        tracing::info!(
            request_id = pending.request_id,
            base_generation = pending.plan.base_generation,
            final_generation,
            outcome = drop_outcome_name(&outcome),
            "drop transaction finished"
        );
        let response = match &outcome {
            DropOutcome::Confirmed => "ok\n".to_string(),
            DropOutcome::ActionFailed(error) => format!("error: drop action failed: {error}\n"),
            DropOutcome::ConfirmationTimedOut => "error: drop confirmation timeout\n".to_string(),
            DropOutcome::EventStreamUnavailable => {
                "error: Niri event stream unavailable\n".to_string()
            }
        };
        if let Some(reply) = pending.reply.take() {
            let _ = reply.send(response);
        }
        self.last = Some(CompletedDropTransaction {
            request_id: pending.request_id,
            base_generation: pending.plan.base_generation,
            final_generation,
            outcome,
            action_result: pending.action_result,
        });
    }

    fn next_deadline(&self) -> Option<Instant> {
        let pending = self.pending.as_ref()?;
        pending.action_result_at.map(|result_at| {
            if pending
                .action_result
                .as_ref()
                .is_some_and(|result| result.failure.is_some())
            {
                result_at + DROP_FAILURE_RECONCILE
            } else {
                result_at + DROP_CONFIRM_TIMEOUT
            }
        })
    }

    fn status_json(&self) -> serde_json::Value {
        if let Some(pending) = &self.pending {
            return serde_json::json!({
                "state": if pending.action_result.is_some() { "confirming" } else { "executing" },
                "request_id": pending.request_id,
                "base_generation": pending.plan.base_generation,
                "completed_steps": pending.action_result.as_ref().map(|result| {
                    result.completed_steps.iter().map(|step| action_step_name(*step)).collect::<Vec<_>>()
                }).unwrap_or_default(),
                "failed_step": pending.action_result.as_ref().and_then(|result| result.failure.as_ref())
                    .map(|failure| action_step_name(failure.step)),
            });
        }
        if let Some(last) = &self.last {
            return serde_json::json!({
                "state": drop_outcome_name(&last.outcome),
                "request_id": last.request_id,
                "base_generation": last.base_generation,
                "final_generation": last.final_generation,
                "completed_steps": last.action_result.as_ref().map(|result| {
                    result.completed_steps.iter().map(|step| action_step_name(*step)).collect::<Vec<_>>()
                }).unwrap_or_default(),
                "failed_step": last.action_result.as_ref().and_then(|result| result.failure.as_ref())
                    .map(|failure| action_step_name(failure.step)),
                "focus_restoration": last.action_result.as_ref().map(|result| restoration_name(&result.focus_restoration)),
                "cursor_restoration": last.action_result.as_ref().map(|result| restoration_name(&result.cursor_restoration)),
            });
        }
        serde_json::json!({ "state": "idle" })
    }
}

fn action_step_name(step: ActionStep) -> &'static str {
    match step {
        ActionStep::FocusWindow => "focus-window",
        ActionStep::FocusWorkspace => "focus-workspace",
        ActionStep::DetachJoinedSource => "detach-joined-source",
        ActionStep::MoveToTiling => "move-to-tiling",
        ActionStep::MoveToFloating => "move-to-floating",
        ActionStep::MoveToWorkspace => "move-to-workspace",
        ActionStep::FocusMovedWindow => "focus-moved-window",
        ActionStep::MoveColumn => "move-column",
        ActionStep::JoinLeft => "join-left",
        ActionStep::MoveFloatingWindow => "move-floating-window",
        ActionStep::RestoreFocus => "restore-focus",
    }
}

fn drop_outcome_name(outcome: &DropOutcome) -> &'static str {
    match outcome {
        DropOutcome::Confirmed => "confirmed",
        DropOutcome::ActionFailed(_) => "action_failed",
        DropOutcome::ConfirmationTimedOut => "confirmation_timeout",
        DropOutcome::EventStreamUnavailable => "event_stream_unavailable",
    }
}

fn restoration_name(status: &RestorationStatus) -> &'static str {
    match status {
        RestorationStatus::NotRequested => "not_requested",
        RestorationStatus::Completed => "completed",
        RestorationStatus::Failed(_) => "failed",
    }
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
    if std::env::var_os("NIRI_SOCKET").is_none() {
        println!("NIRI_SOCKET is not set; reload Niri to run the new build");
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
    subscribers: usize,
) -> String {
    serde_json::json!({
        "visible": !popup_outputs.is_empty(),
        "mode": popup_mode_name(popup_mode),
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

    let (control_tx, control_rx) = unbounded::<ControlRequest>();
    let subscriptions = std::sync::Arc::new(subscribe::SubscriptionHub::new());
    let _control_server =
        ipc::serve_with_subscriptions(control_tx, wake.handle(), Arc::clone(&subscriptions))?;

    let mut snapshot = wait_for_snapshot(&snapshot_rx).unwrap_or_default();
    subscriptions.publish(snapshot.generation, &snapshot_json(&snapshot, None));
    let mut niri_health = NiriHealth::default();
    let mut drop_transactions = DropTransactionState::default();
    let mut panel = PanelState::default();
    let mut bar_scrolls: HashMap<String, BarScrollState> = HashMap::new();
    let mut selection = Selection::default();
    let mut popup_was_open = false;
    // Focus changes we caused ourselves must not bounce back as a peek.
    let mut peek_suppressed_until = Instant::now();
    let mut window_first_seen: HashMap<u64, Instant> = HashMap::new();
    // Focus state as last observed, versus as last acted on. The gap between
    // them is the burst of events still settling.
    let mut observed: Option<FocusMark> = None;
    let mut acted_focus: Option<WorkspaceFocusMark> = None;
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
            subscriptions.publish(snapshot.generation, &snapshot_json(&snapshot, None));
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
            let signature = focus_signature(&snapshot);
            let changed = observed
                .replace(signature)
                .is_some_and(|previous| previous != *observed.as_ref().unwrap());
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

        let now = Instant::now();
        while let Ok(result) = niri_transaction_rx.try_recv() {
            drop_transactions.record_action_result(result, now);
        }
        drop_transactions.reconcile(
            &snapshot,
            niri_health.event_stream_connected,
            Instant::now(),
        );

        if let Some(since) = pending_focus_since {
            let now = Instant::now();
            if now.duration_since(since) >= FOCUS_SETTLE {
                pending_focus_since = None;
                let signature = workspace_focus_signature(&snapshot);
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
                    let was_hidden = panel.mode() == PopupMode::Hidden;
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
                    if was_hidden && panel.mode() == PopupMode::Peek && config.peek.pointer_promote
                    {
                        panel.peek_mut().arm_pointer(
                            Instant::now() + Duration::from_millis(config.peek.arm_grace_ms),
                        );
                    }
                    dirty = true;
                }
                acted_focus = Some(signature);
                acted_windows = Some(windows);
            }
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
                        pending_focus_since,
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
                    if panel.mode() != PopupMode::Manual {
                        panel.set_origin_focus(snapshot.focused_window);
                    }
                    let origin_focus = panel.origin_focus();
                    show_popup(
                        &mut surfaces,
                        &snapshot,
                        &config,
                        panel.outputs_mut(),
                        PopupInput::FULL,
                    )?;
                    // Taking control cancels the peek countdown.
                    panel.promote(origin_focus);
                    reset_selection(&mut selection, &snapshot);
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
                        let origin_focus = snapshot.focused_window;
                        show_popup(
                            &mut surfaces,
                            &snapshot,
                            &config,
                            panel.outputs_mut(),
                            PopupInput::FULL,
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
                        let len = windows.len() as i64;
                        let next = (current as i64 + i64::from(delta)).rem_euclid(len) as usize;
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
                    let target = target_window_id
                        .map(|window_id| HitTarget::Window {
                            workspace_id: target_workspace_id,
                            window_id,
                            rect: render::Rect {
                                x: 0.0,
                                y: 0.0,
                                width: 1.0,
                                height: 1.0,
                            },
                            canvas: None,
                        })
                        .unwrap_or_else(|| HitTarget::Canvas {
                            workspace_id: target_workspace_id,
                            rect: render::Rect {
                                x: 0.0,
                                y: 0.0,
                                width: 1.0,
                                height: 1.0,
                            },
                        });
                    // A drop is answered by its transaction, not here: the
                    // reply is staged until Niri confirms the move, the action
                    // fails, or the confirmation times out. Rejections before a
                    // transaction exists also go out through that channel.
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
                        let capture = popup_capture_outside(&config, PopupInput::FULL);
                        surfaces.set_popup_input(PopupInput::FULL, capture)?;
                        panel.promote(snapshot.focused_window);
                        reset_selection(&mut selection, &snapshot);
                        dirty = true;
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
            PopupMode::Manual => !config.behavior.close_on_outside_click,
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
        wait_timeout = if animating || dirty {
            Duration::from_millis(16)
        } else {
            Duration::from_millis(250)
        };
        if let Some(since) = pending_focus_since {
            wait_timeout =
                wait_timeout.min(FOCUS_SETTLE.saturating_sub(now.saturating_duration_since(since)));
        }
        if let Some(deadline) = panel.peek().deadline() {
            wait_timeout = wait_timeout.min(deadline.saturating_duration_since(now));
        }
        if let Some(at) = panel.peek().pointer_at() {
            wait_timeout = wait_timeout.min(at.saturating_duration_since(now));
        }
        if let Some((_, since)) = panel.follow_candidate() {
            let delay = Duration::from_millis(config.popup.follow_delay_ms);
            wait_timeout =
                wait_timeout.min(delay.saturating_sub(now.saturating_duration_since(*since)));
        }
        if let Some(deadline) = drop_transactions.next_deadline() {
            wait_timeout = wait_timeout.min(deadline.saturating_duration_since(now));
        }
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

/// Where Niri's keyboard focus currently is.
type FocusMark = (Option<String>, Option<u64>, Option<u64>);

/// Where Niri's focused workspace currently is.
///
/// Peek is a workspace navigator. Window focus changes happen for pointer
/// focus and floating dialogs too, so they are deliberately not part of this
/// marker.
type WorkspaceFocusMark = (Option<String>, Option<u64>);

/// What kind of focus movement a new snapshot represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FocusMove {
    None,
    /// Keyboard focus moved to another workspace on this monitor.
    WithinOutput,
    /// Focus landed on a different monitor.
    AcrossOutputs,
}

fn classify_focus_move(previous: &WorkspaceFocusMark, current: &WorkspaceFocusMark) -> FocusMove {
    if previous == current {
        FocusMove::None
    } else if previous.0 != current.0 {
        FocusMove::AcrossOutputs
    } else {
        FocusMove::WithinOutput
    }
}

fn focus_signature(snapshot: &Snapshot) -> FocusMark {
    let output = snapshot.focused_output().map(str::to_string);
    let workspace = snapshot
        .outputs
        .values()
        .flat_map(|output| &output.workspaces)
        .find(|workspace| workspace.focused)
        .map(|workspace| workspace.id);
    (output, workspace, snapshot.focused_window)
}

fn workspace_focus_signature(snapshot: &Snapshot) -> WorkspaceFocusMark {
    let (output, workspace, _) = focus_signature(snapshot);
    (output, workspace)
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
        UiEventKind::Motion | UiEventKind::Scroll { .. } => PeekPointer::Arm,
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
        show_popup(surfaces, snapshot, config, visible, input)?;
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

/// Whether the popup should capture clicks landing outside its rectangle.
///
/// This follows `behavior.close_on_outside_click` and nothing else.
/// Replaying the click needs a virtual pointer, but that is decided at the
/// dismissal site, not here, so pointer availability does not belong in
/// this predicate.
fn should_capture_outside_click(config: &Config) -> bool {
    config.behavior.close_on_outside_click
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

/// Whether the popup should swallow the whole output to catch outside clicks.
///
/// Only a manual session may do this. A peek overlay claiming the full output
/// would turn a passive readout into a modal one, so it is held to the panel
/// rectangle whatever the outside-click settings say.
fn popup_capture_outside(config: &Config, input: PopupInput) -> bool {
    input.keyboard && should_capture_outside_click(config)
}

fn show_popup(
    surfaces: &mut SurfaceManager,
    snapshot: &Snapshot,
    config: &Config,
    visible: &mut HashSet<String>,
    input: PopupInput,
) -> Result<()> {
    let outputs = popup_output_targets(snapshot, config);
    let capture_outside = popup_capture_outside(config, input);
    surfaces.show_popups_with(&outputs, input, capture_outside)?;
    visible.clear();
    visible.extend(outputs);
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
    let capture_outside = should_capture_outside_click(config);
    surfaces.show_popups(std::slice::from_ref(&active_output), capture_outside)?;
    panel.outputs_mut().clear();
    panel.outputs_mut().insert(active_output.clone());
    selection.select_output(active_output, snapshot);
    Ok(true)
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
                    HitTarget::Canvas { .. } | HitTarget::Panel => {}
                }
            } else if matches!(event.kind, SurfaceKind::Popup)
                && config.behavior.close_on_outside_click
            {
                surfaces.hide_all_popups()?;
                panel.outputs_mut().clear();
                if config.behavior.replay_outside_click {
                    surfaces.replay_pointer_click(event.global_x, event.global_y, button);
                }
            }
            Ok(true)
        }
        UiEventKind::Motion => {
            if let Some(press) = panel.press_mut() {
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
            let Some(released) = panel.take_press() else {
                return Ok(false);
            };
            if released.dragging {
                let surface_infos = surfaces.surfaces();
                if let Some((_surface, target, local_x, local_y)) =
                    renderer.hit_test_global(&surface_infos, released.current_x, released.current_y)
                {
                    let mode = panel.mode();
                    let is_open = panel.is_open();
                    let dropped = dispatch_drop(
                        released.window_id,
                        target,
                        local_x,
                        local_y,
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
                    let (submitted, close) = if let Some(window_id) =
                        selected_window(selection, snapshot)
                    {
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
                        // The queued move is resolved after successful
                        // submission. If the worker is unavailable, preserve
                        // the staged selection so the user can retry.
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
        UiEventKind::Scroll { steps } => {
            if let SurfaceKind::Bar(index) = event.kind {
                if let Some(bar) = config.bars.get(index).filter(|bar| bar.scroll_to_switch) {
                    let mut delta = steps;
                    if bar.scroll_reverse {
                        delta = -delta;
                    }
                    let hovered_workspace =
                        renderer.workspace_at(event.surface_id, event.x, event.y);
                    if focus_adjacent_workspace(
                        snapshot,
                        &event.output,
                        hovered_workspace,
                        delta,
                        bar.scroll_wrap,
                        Duration::from_millis(bar.scroll_repeat_ms),
                        bar_scrolls.entry(event.output.clone()).or_default(),
                        niri_tx,
                    ) {
                        // Bar wheel input is routed through niri-xmb. It is a
                        // direct widget command, not external workspace
                        // navigation that should raise a passive peek.
                        *suppress_peek = Instant::now() + SELF_FOCUS_GRACE;
                    }
                }
            } else {
                move_selection(selection, snapshot, steps, 0);
                follow_selection(config, selection, snapshot, niri_tx, suppress_peek);
            }
            Ok(true)
        }
        _ => Ok(false),
    }
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
        .send(NiriRequest::Command(NiriCommand::FocusWorkspace(
            workspace_id,
        )))
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
    let next = if wrap {
        wrap_index(current, delta, count)
    } else {
        (current as i32 + delta).clamp(0, count.saturating_sub(1) as i32) as usize
    };
    scroll.target_workspace = Some(output.workspaces[next].id);
    scroll.last_step_at = Some(now);
    (next != current).then_some(output.workspaces[next].id)
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
    transactions: &mut DropTransactionState,
    reply: Option<crossbeam_channel::Sender<String>>,
    niri_tx: &crossbeam_channel::Sender<NiriRequest>,
    panel_mode: PopupMode,
    panel_visible: bool,
) -> bool {
    let (target, allow_column_join) = match target {
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
            snapshot
                .outputs
                .values()
                .find(|output| {
                    output
                        .workspaces
                        .iter()
                        .any(|workspace| workspace.id == *workspace_id)
                })
                .and_then(|output| config.bars.iter().find(|bar| bar.output == output.name))
                .is_some_and(|bar| bar.allow_column_join),
        ),
    };
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

fn normalized_fraction(value: f32, origin: f32, length: f32) -> f32 {
    let fraction = (value - origin) / length.max(1.0);
    if fraction.is_finite() {
        fraction.clamp(0.0, 1.0)
    } else {
        fraction
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
        }
    }

    #[test]
    fn snapshot_json_schema_is_stable() {
        let value: serde_json::Value =
            serde_json::from_str(&snapshot_json(&protocol_snapshot(), None)).unwrap();

        assert_eq!(value["generation"], 9);
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
            vec!["focused_window", "generation", "outputs"]
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
            0,
        ))
        .unwrap();

        assert_eq!(value["visible"], false);
        assert_eq!(value["mode"], "hidden");
        assert_eq!(value["cursor_restore"], true);
        assert_eq!(value["niri"]["event_stream_connected"], false);
        assert_eq!(value["niri"]["last_event_error"], "stream lost");
        assert_eq!(value["niri"]["last_action_error"], "focus failed");
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
            &HitTarget::Window {
                workspace_id: 7,
                window_id: 999,
                rect: render::Rect {
                    x: 0.0,
                    y: 0.0,
                    width: 100.0,
                    height: 20.0,
                },
                canvas: None,
            },
            10.0,
            10.0,
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
            &HitTarget::Canvas {
                workspace_id: 999,
                rect: render::Rect {
                    x: 0.0,
                    y: 0.0,
                    width: 1.0,
                    height: 1.0,
                },
            },
            0.5,
            0.5,
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

    /// Capture follows `close_on_outside_click` alone; the replay setting
    /// governs what happens after dismissal, not whether the click is caught.
    #[test]
    fn outside_click_capture_ignores_the_replay_setting() {
        let mut config = Config::default();
        config.behavior.close_on_outside_click = true;

        config.behavior.replay_outside_click = true;
        assert!(should_capture_outside_click(&config));

        config.behavior.replay_outside_click = false;
        assert!(should_capture_outside_click(&config));

        config.behavior.close_on_outside_click = false;
        assert!(!should_capture_outside_click(&config));
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

    fn transaction_plan(generation: u64) -> drop_plan::DropPlan {
        drop_plan::DropPlan {
            base_generation: generation,
            source_window_id: 42,
            source_workspace_id: 7,
            source_joined: false,
            destination: drop_plan::DropDestination::Tiled {
                workspace_id: 7,
                column: 1,
                join: None,
            },
            // A projection no live snapshot can ever satisfy: the workspace
            // does not exist and the expectation is non-empty. (An empty
            // expectation would be satisfied by absence, since Niri destroys
            // emptied workspaces.)
            projection: drop_plan::DropProjection::Exact(vec![drop_plan::ProjectedWorkspace {
                workspace_id: u64::MAX,
                columns: vec![vec![42]],
                floating: Vec::new(),
            }]),
            confirmation: drop_plan::DropConfirmation::ExactProjection,
            restore_focus: None,
            restore_cursor: None,
        }
    }

    fn confirmed_plan(generation: u64) -> drop_plan::DropPlan {
        let mut plan = transaction_plan(generation);
        // An empty exact projection is vacuously satisfied, so any snapshot
        // after a successful action confirms the transaction.
        plan.projection = drop_plan::DropProjection::Exact(Vec::new());
        plan
    }

    fn failed_result(request_id: u64) -> niri::ActionTransactionResult {
        niri::ActionTransactionResult {
            request_id,
            completed_steps: vec![ActionStep::MoveToWorkspace],
            failure: Some(niri::ActionStepFailure {
                step: ActionStep::MoveColumn,
                error: "column move rejected".into(),
            }),
            focus_restoration: niri::RestorationStatus::Completed,
            cursor_restoration: niri::RestorationStatus::NotRequested,
        }
    }

    #[test]
    fn drop_transaction_confirms_after_successful_action() {
        let snapshot = protocol_snapshot();
        let (niri_tx, niri_rx) = crossbeam_channel::unbounded();
        let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
        let mut state = DropTransactionState::default();

        let request_id = state
            .submit(
                confirmed_plan(snapshot.generation),
                &snapshot,
                Some(reply_tx),
                &niri_tx,
            )
            .unwrap();
        assert!(niri_rx.try_recv().is_ok());
        assert!(reply_rx.try_recv().is_err(), "reply must stay deferred");

        let submitted_at = Instant::now();
        // Before the action worker reports, nothing may confirm or reply.
        state.reconcile(&snapshot, true, submitted_at);
        assert!(reply_rx.try_recv().is_err());

        state.record_action_result(
            niri::ActionTransactionResult {
                request_id,
                completed_steps: vec![ActionStep::FocusWindow, ActionStep::MoveColumn],
                failure: None,
                focus_restoration: niri::RestorationStatus::Completed,
                cursor_restoration: niri::RestorationStatus::NotRequested,
            },
            submitted_at,
        );

        state.reconcile(&snapshot, true, submitted_at + Duration::from_millis(1));
        assert_eq!(reply_rx.try_recv().unwrap(), "ok\n");
        assert!(state.next_deadline().is_none());
    }

    #[test]
    fn drop_transaction_reports_failed_step_and_keeps_reply() {
        let snapshot = protocol_snapshot();
        let (niri_tx, _niri_rx) = crossbeam_channel::unbounded();
        let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
        let mut state = DropTransactionState::default();

        let request_id = state
            .submit(
                transaction_plan(snapshot.generation),
                &snapshot,
                Some(reply_tx),
                &niri_tx,
            )
            .unwrap();

        let result_at = Instant::now();
        state.record_action_result(failed_result(request_id), result_at);
        // The failure holds briefly so an immediately following event burst
        // cannot race the structured result.
        state.reconcile(&snapshot, true, result_at + Duration::from_millis(1));
        assert!(reply_rx.try_recv().is_err());

        state.reconcile(
            &snapshot,
            true,
            result_at + DROP_FAILURE_RECONCILE + Duration::from_millis(1),
        );
        assert_eq!(
            reply_rx.try_recv().unwrap(),
            "error: drop action failed: move-column: column move rejected\n"
        );
    }

    #[test]
    fn drop_transaction_times_out_without_confirmation() {
        let snapshot = protocol_snapshot();
        let (niri_tx, _niri_rx) = crossbeam_channel::unbounded();
        let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
        let mut state = DropTransactionState::default();

        let request_id = state
            .submit(
                transaction_plan(snapshot.generation),
                &snapshot,
                Some(reply_tx),
                &niri_tx,
            )
            .unwrap();

        let result_at = Instant::now();
        state.record_action_result(
            niri::ActionTransactionResult {
                request_id,
                completed_steps: vec![ActionStep::MoveColumn],
                failure: None,
                focus_restoration: niri::RestorationStatus::Completed,
                cursor_restoration: niri::RestorationStatus::NotRequested,
            },
            result_at,
        );
        state.reconcile(
            &snapshot,
            true,
            result_at + DROP_CONFIRM_TIMEOUT + Duration::from_millis(1),
        );
        assert_eq!(
            reply_rx.try_recv().unwrap(),
            "error: drop confirmation timeout\n"
        );
    }

    /// Niri actions used to have to finish inside about 800 ms or the control
    /// socket gave up and reported a failure for a drop that was still being
    /// confirmed. A slow action sequence must still be able to reply ok.
    #[test]
    fn a_slow_action_sequence_still_confirms() {
        let snapshot = protocol_snapshot();
        let (niri_tx, _niri_rx) = crossbeam_channel::unbounded();
        let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
        let mut state = DropTransactionState::default();

        let request_id = state
            .submit(
                confirmed_plan(snapshot.generation),
                &snapshot,
                Some(reply_tx),
                &niri_tx,
            )
            .unwrap();

        // The actions take longer than the old 2 s reply bound allowed.
        let submitted_at = Instant::now();
        let result_at = submitted_at + Duration::from_millis(2500);
        state.record_action_result(
            niri::ActionTransactionResult {
                request_id,
                completed_steps: vec![ActionStep::MoveColumn],
                failure: None,
                focus_restoration: niri::RestorationStatus::Completed,
                cursor_restoration: niri::RestorationStatus::NotRequested,
            },
            result_at,
        );
        state.reconcile(&snapshot, true, result_at + Duration::from_millis(1));

        assert_eq!(reply_rx.try_recv().unwrap(), "ok\n");
    }

    #[test]
    fn drop_transaction_rejects_overlap_and_stale_plans() {
        let snapshot = protocol_snapshot();
        let (niri_tx, _niri_rx) = crossbeam_channel::unbounded();
        let mut state = DropTransactionState::default();

        state
            .submit(
                transaction_plan(snapshot.generation),
                &snapshot,
                None,
                &niri_tx,
            )
            .unwrap();
        assert_eq!(
            state.submit(
                transaction_plan(snapshot.generation),
                &snapshot,
                None,
                &niri_tx
            ),
            Err("drop transaction already active")
        );

        let mut stale = DropTransactionState::default();
        assert_eq!(
            stale.submit(
                transaction_plan(snapshot.generation - 1),
                &snapshot,
                None,
                &niri_tx
            ),
            Err("stale drop plan")
        );
    }

    #[test]
    fn drop_transaction_survives_event_stream_loss() {
        let snapshot = protocol_snapshot();
        let (niri_tx, _niri_rx) = crossbeam_channel::unbounded();
        let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
        let mut state = DropTransactionState::default();

        let request_id = state
            .submit(
                transaction_plan(snapshot.generation),
                &snapshot,
                Some(reply_tx),
                &niri_tx,
            )
            .unwrap();

        let result_at = Instant::now();
        state.record_action_result(
            niri::ActionTransactionResult {
                request_id,
                completed_steps: vec![ActionStep::MoveColumn],
                failure: None,
                focus_restoration: niri::RestorationStatus::Completed,
                cursor_restoration: niri::RestorationStatus::NotRequested,
            },
            result_at,
        );
        // Without events no postcondition can ever be observed; report that
        // instead of waiting for a timeout that proves nothing.
        state.reconcile(&snapshot, false, result_at + Duration::from_millis(1));
        assert_eq!(
            reply_rx.try_recv().unwrap(),
            "error: Niri event stream unavailable\n"
        );
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

    /// Full-output click capture turns a passive readout into a modal one, so
    /// an overlay that only holds the pointer stays inside its panel however
    /// the outside-click settings are configured.
    #[test]
    fn only_a_manual_session_may_capture_the_whole_output() {
        let mut config = Config::default();
        config.behavior.close_on_outside_click = true;
        config.behavior.replay_outside_click = false;

        assert!(popup_capture_outside(&config, PopupInput::FULL));
        assert!(!popup_capture_outside(&config, PopupInput::POINTER));
        assert!(!popup_capture_outside(&config, PopupInput::NONE));
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
        let a: WorkspaceFocusMark = (Some("DP-1".into()), Some(1));
        let b: WorkspaceFocusMark = (Some("DP-2".into()), Some(4));
        assert_eq!(classify_focus_move(&a, &b), FocusMove::AcrossOutputs);
        assert_eq!(classify_focus_move(&b, &a), FocusMove::AcrossOutputs);
    }

    #[test]
    fn only_workspace_changes_are_within_output_navigation() {
        let base: WorkspaceFocusMark = (Some("DP-1".into()), Some(1));
        let other_workspace: WorkspaceFocusMark = (Some("DP-1".into()), Some(2));
        assert_eq!(
            classify_focus_move(&base, &other_workspace),
            FocusMove::WithinOutput
        );
        assert_eq!(
            classify_focus_move(&base, &base),
            FocusMove::None,
            "window focus is absent from a workspace peek marker"
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

    #[test]
    fn an_unchanged_signature_is_not_a_move() {
        let a: WorkspaceFocusMark = (Some("DP-1".into()), Some(1));
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
    fn focus_signature_tracks_workspace_and_window_but_peek_ignores_window() {
        let mut snapshot = snapshot();
        let base = focus_signature(&snapshot);

        // Manual mode needs the exact focused window to stay in sync.
        snapshot.focused_window = Some(101);
        let moved = focus_signature(&snapshot);
        assert_ne!(base, moved);

        // Peek only sees the workspace, so moving the pointer between tiled or
        // floating windows cannot raise it.
        assert_eq!(
            workspace_focus_signature(&snapshot),
            workspace_focus_signature(&{
                let mut other = snapshot.clone();
                other.focused_window = Some(102);
                other
            }),
            "window-level changes must be invisible in workspace-only mode"
        );
    }

    #[test]
    fn focus_signature_notices_a_workspace_change() {
        let mut snapshot = snapshot();
        let before = workspace_focus_signature(&snapshot);
        let workspaces = &mut snapshot.outputs.get_mut("DP-1").unwrap().workspaces;
        workspaces[0].focused = false;
        workspaces[1].focused = true;
        assert_ne!(before, workspace_focus_signature(&snapshot));
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
