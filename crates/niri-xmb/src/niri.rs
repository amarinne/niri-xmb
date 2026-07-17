use crate::model::{NiriState, Snapshot};
use crossbeam_channel::{Receiver, Sender};
use niri_ipc::socket::Socket;
use niri_ipc::{Action, Event, Request, Response, WorkspaceReferenceArg};
use std::thread;
use std::time::Duration;

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
    },
    MoveFloating {
        window_id: u64,
        source_workspace_id: u64,
        workspace_id: u64,
        x: f64,
        y: f64,
        make_floating: bool,
        restore_focus: Option<u64>,
    },
}

#[derive(Debug, Clone, Copy)]
pub enum JoinDirection {
    Left,
}

pub fn spawn(snapshot_tx: Sender<Snapshot>, command_rx: Receiver<NiriCommand>) {
    thread::Builder::new()
        .name("niri-xmb-events".into())
        .spawn(move || run_events(snapshot_tx))
        .expect("failed to spawn niri IPC thread");
    thread::Builder::new()
        .name("niri-xmb-actions".into())
        .spawn(move || {
            while let Ok(command) = command_rx.recv() {
                if let Err(error) = dispatch(command) {
                    tracing::warn!(%error, "Niri action failed");
                }
            }
        })
        .expect("failed to spawn niri action thread");
}

fn run_events(snapshot_tx: Sender<Snapshot>) {
    loop {
        let mut state = NiriState::default();
        refresh_outputs(&mut state);

        let Ok(mut socket) = Socket::connect() else {
            tracing::warn!("cannot connect to Niri IPC; retrying");
            thread::sleep(Duration::from_secs(2));
            continue;
        };
        match socket.send(Request::EventStream) {
            Ok(Ok(Response::Handled)) => {}
            Ok(Err(error)) => {
                tracing::warn!(%error, "Niri rejected event stream");
                thread::sleep(Duration::from_secs(2));
                continue;
            }
            Ok(Ok(response)) => {
                tracing::warn!(?response, "unexpected Niri event-stream response");
                thread::sleep(Duration::from_secs(2));
                continue;
            }
            Err(error) => {
                tracing::warn!(%error, "failed to start Niri event stream");
                thread::sleep(Duration::from_secs(2));
                continue;
            }
        }

        let mut read_event = socket.read_events();
        loop {
            let event = match read_event() {
                Ok(event) => event,
                Err(error) => {
                    tracing::warn!(%error, "Niri event stream disconnected");
                    break;
                }
            };
            let refresh = apply_event(&mut state, event);
            if refresh {
                let _ = snapshot_tx.try_send(state.snapshot());
            }
        }
        thread::sleep(Duration::from_millis(500));
    }
}

fn refresh_outputs(state: &mut NiriState) {
    let Ok(mut socket) = Socket::connect() else {
        return;
    };
    if let Ok(Ok(Response::Outputs(outputs))) = socket.send(Request::Outputs) {
        state.replace_outputs(outputs);
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
            if let Some(workspace) = state.workspace_mut(workspace_id) {
                workspace.active_window_id = active_window_id;
            }
            true
        }
        Event::WindowsChanged { windows } => {
            state.replace_windows(windows);
            true
        }
        Event::WindowOpenedOrChanged { window } => {
            state.upsert_window(window);
            true
        }
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
            for (id, layout) in changes {
                if let Some(window) = state.window_mut(id) {
                    window.layout = layout;
                }
            }
            true
        }
        Event::ConfigLoaded { .. } => {
            refresh_outputs(state);
            true
        }
        _ => false,
    }
}

fn send(action: Action) -> anyhow::Result<()> {
    let mut socket = Socket::connect()?;
    match socket.send(Request::Action(action))? {
        Ok(Response::Handled) => Ok(()),
        Ok(response) => anyhow::bail!("unexpected Niri response: {response:?}"),
        Err(error) => anyhow::bail!(error),
    }
}

fn dispatch(command: NiriCommand) -> anyhow::Result<()> {
    match command {
        NiriCommand::FocusWindow(id) => send(Action::FocusWindow { id }),
        NiriCommand::FocusWorkspace(id) => send(Action::FocusWorkspace {
            reference: WorkspaceReferenceArg::Id(id),
        }),
        NiriCommand::MoveTiled {
            window_id,
            source_workspace_id,
            workspace_id,
            column,
            join,
            source_joined,
            restore_focus,
        } => {
            if source_joined {
                send(Action::ConsumeOrExpelWindowLeft {
                    id: Some(window_id),
                })?;
            }
            send(Action::MoveWindowToTiling {
                id: Some(window_id),
            })?;
            if source_workspace_id != workspace_id {
                send(Action::MoveWindowToWorkspace {
                    window_id: Some(window_id),
                    reference: WorkspaceReferenceArg::Id(workspace_id),
                    focus: false,
                })?;
            }
            send(Action::FocusWindow { id: window_id })?;
            send(Action::MoveColumnToIndex {
                index: column.max(1),
            })?;
            if let Some(direction) = join {
                let action = match direction {
                    JoinDirection::Left => Action::ConsumeOrExpelWindowLeft {
                        id: Some(window_id),
                    },
                };
                send(action)?;
            }
            if let Some(id) = restore_focus.filter(|id| *id != window_id) {
                send(Action::FocusWindow { id })?;
            }
            Ok(())
        }
        NiriCommand::MoveFloating {
            window_id,
            source_workspace_id,
            workspace_id,
            x,
            y,
            make_floating,
            restore_focus,
        } => {
            if make_floating {
                send(Action::MoveWindowToFloating {
                    id: Some(window_id),
                })?;
            }
            if source_workspace_id != workspace_id {
                send(Action::MoveWindowToWorkspace {
                    window_id: Some(window_id),
                    reference: WorkspaceReferenceArg::Id(workspace_id),
                    focus: false,
                })?;
            }
            send(Action::MoveFloatingWindow {
                id: Some(window_id),
                x: niri_ipc::PositionChange::SetFixed(x),
                y: niri_ipc::PositionChange::SetFixed(y),
            })?;
            if let Some(id) = restore_focus.filter(|id| *id != window_id) {
                send(Action::FocusWindow { id })?;
            }
            Ok(())
        }
    }
}
