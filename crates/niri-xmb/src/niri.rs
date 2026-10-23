use crate::model::{NiriState, Snapshot};
use crate::wayland::WakeHandle;
use crossbeam_channel::{Receiver, Sender};
use niri_ipc::socket::Socket;
use niri_ipc::{Action, Event, Request, Response, WorkspaceReferenceArg};
use std::thread;
use std::time::Duration;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NiriHealth {
    pub event_stream_connected: bool,
    pub last_event_error: Option<String>,
    pub last_action_error: Option<String>,
}

impl NiriHealth {
    pub fn apply(&mut self, update: NiriHealthUpdate) {
        match update {
            NiriHealthUpdate::EventConnected => self.event_stream_connected = true,
            NiriHealthUpdate::EventDisconnected(error) => {
                self.event_stream_connected = false;
                self.last_event_error = Some(error);
            }
            NiriHealthUpdate::ActionFailed(error) => self.last_action_error = Some(error),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NiriHealthUpdate {
    EventConnected,
    EventDisconnected(String),
    ActionFailed(String),
}

#[derive(Debug, Clone)]
pub enum NiriCommand {
    FocusWindow(u64),
    FocusWorkspace(u64),
    MoveTiled {
        window_id: u64,
        source_workspace_id: u64,
        workspace_id: u64,
        column: usize,
        join: Option<JoinDirection>,
        source_joined: bool,
        restore_focus: Option<u64>,
        restore_cursor: Option<(f64, f64)>,
    },
    MoveFloating {
        window_id: u64,
        source_workspace_id: u64,
        workspace_id: u64,
        x: f64,
        y: f64,
        make_floating: bool,
        restore_focus: Option<u64>,
        restore_cursor: Option<(f64, f64)>,
    },
}

/// One command for the action worker.
#[derive(Debug, Clone)]
pub struct NiriRequest {
    /// Set when the caller is waiting for a structured result -- a drop
    /// transaction. `None` is fire-and-forget: run it and log any failure.
    pub request_id: Option<u64>,
    pub command: NiriCommand,
}

impl From<NiriCommand> for NiriRequest {
    fn from(command: NiriCommand) -> Self {
        Self {
            request_id: None,
            command,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionStep {
    FocusWindow,
    FocusWorkspace,
    DetachJoinedSource,
    MoveToTiling,
    MoveToFloating,
    MoveToWorkspace,
    FocusMovedWindow,
    MoveColumn,
    JoinLeft,
    MoveFloatingWindow,
    RestoreFocus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionStepFailure {
    pub step: ActionStep,
    pub error: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestorationStatus {
    NotRequested,
    Completed,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionTransactionResult {
    pub request_id: u64,
    pub completed_steps: Vec<ActionStep>,
    pub failure: Option<ActionStepFailure>,
    pub focus_restoration: RestorationStatus,
    pub cursor_restoration: RestorationStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinDirection {
    Left,
}

pub fn spawn(
    snapshot_tx: Sender<Snapshot>,
    command_rx: Receiver<NiriRequest>,
    cursor_restore_tx: Sender<(f64, f64)>,
    health_tx: Sender<NiriHealthUpdate>,
    transaction_tx: Sender<ActionTransactionResult>,
    wake: WakeHandle,
) {
    let event_wake = wake.clone();
    let event_health_tx = health_tx.clone();
    thread::Builder::new()
        .name("niri-xmb-events".into())
        .spawn(move || run_events(snapshot_tx, event_health_tx, event_wake))
        .expect("failed to spawn niri IPC thread");
    thread::Builder::new()
        .name("niri-xmb-actions".into())
        .spawn(move || {
            while let Ok(NiriRequest {
                request_id,
                command,
            }) = command_rx.recv()
            {
                let restore_cursor = command.restore_cursor();
                let mut transport = SocketActionTransport::default();
                let mut execution = execute_command(command, &mut transport);
                restore_cursor_position(
                    restore_cursor,
                    &cursor_restore_tx,
                    &mut execution.cursor_restoration,
                );
                if let Some(failure) = &execution.failure {
                    tracing::warn!(
                        request_id = ?request_id,
                        step = ?failure.step,
                        error = %failure.error,
                        "Niri action failed"
                    );
                    let _ = health_tx.send(NiriHealthUpdate::ActionFailed(failure.error.clone()));
                }
                // Only a transaction has someone waiting on a structured result.
                if let Some(request_id) = request_id {
                    let _ = transaction_tx.send(ActionTransactionResult {
                        request_id,
                        completed_steps: execution.completed_steps,
                        failure: execution.failure,
                        focus_restoration: execution.focus_restoration,
                        cursor_restoration: execution.cursor_restoration,
                    });
                }
                wake.wake();
            }
        })
        .expect("failed to spawn niri action thread");
}

/// Connect and start the event stream.
///
/// `None` means the attempt failed; the reason has already been logged and
/// published as a health update, and the caller should back off and retry.
fn connect_event_stream(health_tx: &Sender<NiriHealthUpdate>, wake: &WakeHandle) -> Option<Socket> {
    let mut socket = match Socket::connect() {
        Ok(socket) => socket,
        Err(_) => {
            tracing::warn!("cannot connect to Niri IPC; retrying");
            publish_health(
                health_tx,
                wake,
                NiriHealthUpdate::EventDisconnected("cannot connect to Niri IPC".into()),
            );
            return None;
        }
    };
    match socket.send(Request::EventStream) {
        Ok(Ok(Response::Handled)) => {}
        Ok(Err(error)) => {
            tracing::warn!(%error, "Niri rejected event stream");
            publish_health(
                health_tx,
                wake,
                NiriHealthUpdate::EventDisconnected(format!("Niri rejected event stream: {error}")),
            );
            return None;
        }
        Ok(Ok(response)) => {
            tracing::warn!(?response, "unexpected Niri event-stream response");
            publish_health(
                health_tx,
                wake,
                NiriHealthUpdate::EventDisconnected(format!(
                    "unexpected Niri event-stream response: {response:?}"
                )),
            );
            return None;
        }
        Err(error) => {
            tracing::warn!(%error, "failed to start Niri event stream");
            publish_health(
                health_tx,
                wake,
                NiriHealthUpdate::EventDisconnected(format!(
                    "failed to start Niri event stream: {error}"
                )),
            );
            return None;
        }
    }
    Some(socket)
}

/// Read events until the stream drops, pushing a snapshot after any event
/// that changed something.
fn pump_event_stream(
    socket: Socket,
    state: &mut NiriState,
    snapshot_tx: &Sender<Snapshot>,
    health_tx: &Sender<NiriHealthUpdate>,
    wake: &WakeHandle,
) {
    let mut read_event = socket.read_events();
    loop {
        let event = match read_event() {
            Ok(event) => event,
            // Newer Niri releases add event variants (e.g. TouchpadToggled) that the
            // pinned niri-ipc cannot decode. Skip them instead of dropping the stream.
            Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                tracing::debug!(%error, "skipping undecodable Niri event");
                continue;
            }
            Err(error) => {
                tracing::warn!(%error, "Niri event stream disconnected");
                publish_health(
                    health_tx,
                    wake,
                    NiriHealthUpdate::EventDisconnected(format!(
                        "Niri event stream disconnected: {error}"
                    )),
                );
                break;
            }
        };
        let refresh = apply_event(state, event);
        if refresh && snapshot_tx.try_send(state.snapshot()).is_ok() {
            wake.wake();
        }
    }
}

fn run_events(
    snapshot_tx: Sender<Snapshot>,
    health_tx: Sender<NiriHealthUpdate>,
    wake: WakeHandle,
) {
    loop {
        let mut state = NiriState::default();
        refresh_outputs(&mut state);

        let Some(socket) = connect_event_stream(&health_tx, &wake) else {
            thread::sleep(Duration::from_secs(2));
            continue;
        };

        publish_health(&health_tx, &wake, NiriHealthUpdate::EventConnected);
        pump_event_stream(socket, &mut state, &snapshot_tx, &health_tx, &wake);
        thread::sleep(Duration::from_millis(500));
    }
}

fn publish_health(
    health_tx: &Sender<NiriHealthUpdate>,
    wake: &WakeHandle,
    update: NiriHealthUpdate,
) {
    if health_tx.send(update).is_ok() {
        wake.wake();
    }
}

/// Re-read the output layout and the overview flag.
///
/// This runs from the event-read path, so it asks both questions on one
/// connection rather than dialling twice.
fn refresh_outputs(state: &mut NiriState) {
    let Ok(mut socket) = Socket::connect() else {
        return;
    };
    if let Ok(Ok(Response::Outputs(outputs))) = socket.send(Request::Outputs) {
        state.replace_outputs(outputs);
    }
    // Overview state only arrives as an event on change, so seed it here. Niri
    // sends ConfigLoaded on connect, which brings us through this path.
    if let Ok(Ok(Response::OverviewState(overview))) = socket.send(Request::OverviewState) {
        state.set_overview_open(overview.is_open);
    }
}

fn apply_event(state: &mut NiriState, event: Event) -> bool {
    match event {
        Event::WorkspacesChanged { workspaces } => {
            state.replace_workspaces(workspaces);
            refresh_outputs(state);
            true
        }
        Event::WorkspaceUrgencyChanged { id, urgent } => {
            if let Some(workspace) = state.workspace_mut(id) {
                workspace.is_urgent = urgent;
            }
            true
        }
        Event::WorkspaceActivated { id, focused } => {
            state.activate_workspace(id, focused);
            true
        }
        Event::WorkspaceActiveWindowChanged {
            workspace_id,
            active_window_id,
        } => {
            state.set_workspace_active_window(workspace_id, active_window_id);
            true
        }
        Event::WindowsChanged { windows } => {
            state.replace_windows(windows);
            true
        }
        Event::WindowOpenedOrChanged { window } => state.upsert_window(window),
        Event::WindowClosed { id } => {
            state.remove_window(id);
            true
        }
        Event::WindowFocusChanged { id } => {
            state.set_focused_window(id);
            true
        }
        Event::WindowFocusTimestampChanged {
            id,
            focus_timestamp,
        } => {
            if let Some(window) = state.window_mut(id) {
                window.focus_timestamp = focus_timestamp;
            }
            false
        }
        Event::WindowUrgencyChanged { id, urgent } => {
            if let Some(window) = state.window_mut(id) {
                window.is_urgent = urgent;
            }
            true
        }
        Event::WindowLayoutsChanged { changes } => {
            let mut changed = false;
            for (id, layout) in changes {
                changed = state.set_window_layout(id, layout) || changed;
            }
            changed
        }
        Event::OverviewOpenedOrClosed { is_open } => {
            state.set_overview_open(is_open);
            true
        }
        Event::ConfigLoaded { .. } => {
            refresh_outputs(state);
            true
        }
        _ => false,
    }
}

impl NiriCommand {
    fn restore_cursor(&self) -> Option<(f64, f64)> {
        match self {
            Self::MoveTiled { restore_cursor, .. } | Self::MoveFloating { restore_cursor, .. } => {
                *restore_cursor
            }
            Self::FocusWindow(_) | Self::FocusWorkspace(_) => None,
        }
    }

    /// The window a move acts on, if the command moves one.
    fn window_id(&self) -> Option<u64> {
        match self {
            Self::MoveTiled { window_id, .. } | Self::MoveFloating { window_id, .. } => {
                Some(*window_id)
            }
            Self::FocusWindow(_) | Self::FocusWorkspace(_) => None,
        }
    }

    /// The window focus should return to if the move fails partway.
    fn restore_focus(&self) -> Option<u64> {
        match self {
            Self::MoveTiled { restore_focus, .. } | Self::MoveFloating { restore_focus, .. } => {
                *restore_focus
            }
            Self::FocusWindow(_) | Self::FocusWorkspace(_) => None,
        }
    }
}

trait ActionTransport {
    fn send(&mut self, action: Action) -> anyhow::Result<()>;
}

/// A Niri action socket held open across the steps of one command.
///
/// A tiled move issues up to six actions and reconnected for each one. Niri
/// serves many requests per connection, so keep one and rebuild it only on
/// error -- that way a compositor restart still recovers on the next action.
///
/// This is tidying, not a latency fix: measured against a live compositor a
/// connect costs about 0.024 ms against 0.047 ms for the query itself, and a
/// whole five-step move completes in about 4 ms. The drop that reported
/// failure for a successful move was caused by the two timeouts inverting,
/// not by socket setup.
#[derive(Default)]
struct SocketActionTransport {
    socket: Option<Socket>,
}

impl SocketActionTransport {
    /// Send on the existing connection, opening one if there is none.
    fn send_once(&mut self, action: Action) -> anyhow::Result<()> {
        let socket = match self.socket {
            Some(ref mut socket) => socket,
            None => self.socket.insert(Socket::connect()?),
        };
        match socket.send(Request::Action(action))? {
            Ok(Response::Handled) => Ok(()),
            Ok(response) => anyhow::bail!("unexpected Niri response: {response:?}"),
            Err(error) => anyhow::bail!(error),
        }
    }
}

impl ActionTransport for SocketActionTransport {
    fn send(&mut self, action: Action) -> anyhow::Result<()> {
        let Err(error) = self.send_once(action.clone()) else {
            return Ok(());
        };
        tracing::debug!(%error, "retrying Niri action on a fresh socket");
        self.socket = None;
        self.send_once(action)
    }
}

struct CommandExecution {
    completed_steps: Vec<ActionStep>,
    failure: Option<ActionStepFailure>,
    focus_restoration: RestorationStatus,
    cursor_restoration: RestorationStatus,
}

impl CommandExecution {
    fn new(command: &NiriCommand) -> Self {
        let focus_restoration = match command {
            NiriCommand::MoveTiled { restore_focus, .. }
            | NiriCommand::MoveFloating { restore_focus, .. }
                if restore_focus.is_some() =>
            {
                RestorationStatus::Failed("focus restoration did not run".into())
            }
            _ => RestorationStatus::NotRequested,
        };
        let cursor_restoration = if command.restore_cursor().is_some() {
            RestorationStatus::Failed("cursor restoration did not run".into())
        } else {
            RestorationStatus::NotRequested
        };
        Self {
            completed_steps: Vec::new(),
            failure: None,
            focus_restoration,
            cursor_restoration,
        }
    }
}

fn run_step<T: ActionTransport>(
    transport: &mut T,
    execution: &mut CommandExecution,
    step: ActionStep,
    action: Action,
) -> bool {
    match transport.send(action) {
        Ok(()) => {
            execution.completed_steps.push(step);
            if step == ActionStep::RestoreFocus {
                execution.focus_restoration = RestorationStatus::Completed;
            }
            true
        }
        Err(error) => {
            execution.failure = Some(ActionStepFailure {
                step,
                error: error.to_string(),
            });
            false
        }
    }
}

fn restore_focus_after_failure<T: ActionTransport>(
    transport: &mut T,
    execution: &mut CommandExecution,
    window_id: u64,
    restore_focus: Option<u64>,
) {
    let Some(id) = restore_focus.filter(|id| *id != window_id) else {
        return;
    };
    if execution
        .completed_steps
        .contains(&ActionStep::RestoreFocus)
        || execution
            .failure
            .as_ref()
            .is_some_and(|failure| failure.step == ActionStep::RestoreFocus)
    {
        return;
    }
    execution.focus_restoration = match transport.send(Action::FocusWindow { id }) {
        Ok(()) => RestorationStatus::Completed,
        Err(error) => RestorationStatus::Failed(error.to_string()),
    };
}

fn restore_cursor_position(
    position: Option<(f64, f64)>,
    cursor_restore_tx: &Sender<(f64, f64)>,
    status: &mut RestorationStatus,
) {
    let Some(position) = position else {
        return;
    };
    *status = if cursor_restore_tx.send(position).is_ok() {
        RestorationStatus::Completed
    } else {
        RestorationStatus::Failed("cursor restore channel is unavailable".into())
    };
}

/// The ordered actions a command turns into.
///
/// Deciding the whole sequence up front is what lets the failure path --
/// run the compensating focus restore, then stop -- be written once in
/// `execute_command` instead of at every step boundary.
fn command_steps(command: &NiriCommand) -> Vec<(ActionStep, Action)> {
    match command {
        NiriCommand::FocusWindow(id) => {
            vec![(ActionStep::FocusWindow, Action::FocusWindow { id: *id })]
        }
        NiriCommand::FocusWorkspace(id) => vec![(
            ActionStep::FocusWorkspace,
            Action::FocusWorkspace {
                reference: WorkspaceReferenceArg::Id(*id),
            },
        )],
        NiriCommand::MoveTiled {
            window_id,
            source_workspace_id,
            workspace_id,
            column,
            join,
            source_joined,
            restore_focus,
            restore_cursor: _,
        } => {
            let window_id = *window_id;
            let mut steps = Vec::with_capacity(7);
            if *source_joined {
                steps.push((
                    ActionStep::DetachJoinedSource,
                    Action::ConsumeOrExpelWindowLeft {
                        id: Some(window_id),
                    },
                ));
            }
            steps.push((
                ActionStep::MoveToTiling,
                Action::MoveWindowToTiling {
                    id: Some(window_id),
                },
            ));
            if source_workspace_id != workspace_id {
                steps.push((
                    ActionStep::MoveToWorkspace,
                    Action::MoveWindowToWorkspace {
                        window_id: Some(window_id),
                        reference: WorkspaceReferenceArg::Id(*workspace_id),
                        focus: false,
                    },
                ));
            }
            steps.push((
                ActionStep::FocusMovedWindow,
                Action::FocusWindow { id: window_id },
            ));
            steps.push((
                ActionStep::MoveColumn,
                Action::MoveColumnToIndex {
                    index: (*column).max(1),
                },
            ));
            if let Some(direction) = join {
                let action = match direction {
                    JoinDirection::Left => Action::ConsumeOrExpelWindowLeft {
                        id: Some(window_id),
                    },
                };
                steps.push((ActionStep::JoinLeft, action));
            }
            if let Some(id) = restore_focus.filter(|id| *id != window_id) {
                steps.push((ActionStep::RestoreFocus, Action::FocusWindow { id }));
            }
            steps
        }
        NiriCommand::MoveFloating {
            window_id,
            source_workspace_id,
            workspace_id,
            x,
            y,
            make_floating,
            restore_focus,
            restore_cursor: _,
        } => {
            let window_id = *window_id;
            let mut steps = Vec::with_capacity(4);
            if *make_floating {
                steps.push((
                    ActionStep::MoveToFloating,
                    Action::MoveWindowToFloating {
                        id: Some(window_id),
                    },
                ));
            }
            if source_workspace_id != workspace_id {
                steps.push((
                    ActionStep::MoveToWorkspace,
                    Action::MoveWindowToWorkspace {
                        window_id: Some(window_id),
                        reference: WorkspaceReferenceArg::Id(*workspace_id),
                        focus: false,
                    },
                ));
            }
            steps.push((
                ActionStep::MoveFloatingWindow,
                Action::MoveFloatingWindow {
                    id: Some(window_id),
                    x: niri_ipc::PositionChange::SetFixed(*x),
                    y: niri_ipc::PositionChange::SetFixed(*y),
                },
            ));
            if let Some(id) = restore_focus.filter(|id| *id != window_id) {
                steps.push((ActionStep::RestoreFocus, Action::FocusWindow { id }));
            }
            steps
        }
    }
}

fn execute_command<T: ActionTransport>(
    command: NiriCommand,
    transport: &mut T,
) -> CommandExecution {
    let mut execution = CommandExecution::new(&command);
    let window_id = command.window_id();
    let restore_focus = command.restore_focus();
    for (step, action) in command_steps(&command) {
        if run_step(transport, &mut execution, step, action) {
            continue;
        }
        // A move that stops partway leaves focus wherever it landed, so put
        // it back. restore_focus_after_failure is a no-op when the step that
        // failed was the restore itself.
        if let Some(window_id) = window_id {
            restore_focus_after_failure(transport, &mut execution, window_id, restore_focus);
        }
        break;
    }
    execution
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn health_keeps_last_errors_across_reconnects() {
        let mut health = NiriHealth::default();
        health.apply(NiriHealthUpdate::EventDisconnected("lost stream".into()));
        health.apply(NiriHealthUpdate::ActionFailed("focus failed".into()));
        health.apply(NiriHealthUpdate::EventConnected);

        assert!(health.event_stream_connected);
        assert_eq!(health.last_event_error.as_deref(), Some("lost stream"));
        assert_eq!(health.last_action_error.as_deref(), Some("focus failed"));
    }

    /// Records every action it is handed, and fails the next send once when
    /// `fail_next` is set.
    #[derive(Default)]
    struct FlakyTransport {
        sent: RefCell<Vec<Action>>,
        fail_next: RefCell<bool>,
    }

    impl ActionTransport for FlakyTransport {
        fn send(&mut self, action: Action) -> anyhow::Result<()> {
            self.sent.borrow_mut().push(action);
            if *self.fail_next.borrow() {
                *self.fail_next.borrow_mut() = false;
                anyhow::bail!("transport is down");
            }
            Ok(())
        }
    }

    #[test]
    fn a_failed_step_records_the_failure_and_stops() {
        let mut transport = FlakyTransport::default();
        *transport.fail_next.borrow_mut() = true;
        let execution = execute_command(NiriCommand::FocusWindow(1), &mut transport);
        assert!(execution.failure.is_some());
        assert!(execution.completed_steps.is_empty());
    }

    #[test]
    fn a_successful_step_is_recorded() {
        let mut transport = FlakyTransport::default();
        let execution = execute_command(NiriCommand::FocusWindow(1), &mut transport);
        assert!(execution.failure.is_none());
        assert_eq!(execution.completed_steps, vec![ActionStep::FocusWindow]);
    }

    /// The exact action sequence for a cross-workspace tiled move, in order.
    /// This is the shape the compositor sees; changing it changes behaviour.
    #[test]
    fn a_cross_workspace_tiled_move_runs_its_steps_in_order() {
        let steps = command_steps(&NiriCommand::MoveTiled {
            window_id: 5,
            source_workspace_id: 1,
            workspace_id: 2,
            column: 3,
            join: None,
            source_joined: true,
            restore_focus: Some(9),
            restore_cursor: None,
        });

        let order: Vec<ActionStep> = steps.iter().map(|(step, _)| *step).collect();
        assert_eq!(
            order,
            vec![
                ActionStep::DetachJoinedSource,
                ActionStep::MoveToTiling,
                ActionStep::MoveToWorkspace,
                ActionStep::FocusMovedWindow,
                ActionStep::MoveColumn,
                ActionStep::RestoreFocus,
            ]
        );
    }

    /// A move that stays on its own workspace skips the workspace hop, and a
    /// restore target equal to the moved window is not a restore at all.
    #[test]
    fn a_same_workspace_move_skips_the_workspace_step() {
        let steps = command_steps(&NiriCommand::MoveTiled {
            window_id: 5,
            source_workspace_id: 1,
            workspace_id: 1,
            column: 1,
            join: None,
            source_joined: false,
            restore_focus: Some(5),
            restore_cursor: None,
        });

        let order: Vec<ActionStep> = steps.iter().map(|(step, _)| *step).collect();
        assert_eq!(
            order,
            vec![
                ActionStep::MoveToTiling,
                ActionStep::FocusMovedWindow,
                ActionStep::MoveColumn,
            ]
        );
    }
}
