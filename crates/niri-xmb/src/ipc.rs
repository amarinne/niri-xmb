use anyhow::{Context, Result};
use crossbeam_channel::{Sender, bounded};
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

#[derive(Debug, Clone)]
pub enum ControlCommand {
    Show,
    Hide,
    Toggle,
    Status,
    Snapshot {
        output: Option<String>,
    },
    FocusWindow {
        window_id: u64,
    },
    FocusWorkspace {
        workspace_id: u64,
    },
    CycleWorkspace {
        output: String,
        delta: i32,
    },
    Drop {
        source_window_id: u64,
        target_workspace_id: u64,
        target_window_id: Option<u64>,
        fraction: f32,
        cursor_x: Option<f64>,
        cursor_y: Option<f64>,
    },
    Quit,
}

pub struct ControlRequest {
    pub command: ControlCommand,
    pub reply: Sender<String>,
}

pub fn socket_path() -> PathBuf {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe { libc::geteuid() })));
    runtime.join("niri-xmb.sock")
}

pub fn cleanup() {
    let path = socket_path();
    if let Err(error) = std::fs::remove_file(&path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::debug!(%error, path = %path.display(), "failed to remove control socket");
    }
}

pub fn serve(request_tx: Sender<ControlRequest>) -> Result<()> {
    let path = socket_path();
    cleanup();
    let listener =
        UnixListener::bind(&path).with_context(|| format!("failed to bind {}", path.display()))?;
    thread::Builder::new()
        .name("niri-xmb-control".into())
        .spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    continue;
                };
                let mut command = String::new();
                if stream.read_to_string(&mut command).is_err() {
                    continue;
                }
                let input = command.trim();
                let command = match input {
                    "show" => ControlCommand::Show,
                    "hide" => ControlCommand::Hide,
                    "toggle" => ControlCommand::Toggle,
                    "status" => ControlCommand::Status,
                    "snapshot" => ControlCommand::Snapshot { output: None },
                    _ if input.starts_with("snapshot ") => {
                        let output = input["snapshot ".len()..].trim();
                        if output.is_empty() {
                            let _ = stream.write_all(b"error: invalid snapshot command\n");
                            continue;
                        }
                        ControlCommand::Snapshot {
                            output: Some(output.to_string()),
                        }
                    }
                    _ if input.starts_with("focus-window ") => {
                        let Ok(window_id) = input["focus-window ".len()..].trim().parse() else {
                            let _ = stream.write_all(b"error: invalid focus-window command\n");
                            continue;
                        };
                        ControlCommand::FocusWindow { window_id }
                    }
                    _ if input.starts_with("focus-workspace ") => {
                        let Ok(workspace_id) = input["focus-workspace ".len()..].trim().parse()
                        else {
                            let _ = stream.write_all(b"error: invalid focus-workspace command\n");
                            continue;
                        };
                        ControlCommand::FocusWorkspace { workspace_id }
                    }
                    _ if input.starts_with("cycle-workspace ") => {
                        let fields = input.split_whitespace().collect::<Vec<_>>();
                        let parsed = (|| {
                            Some(ControlCommand::CycleWorkspace {
                                output: fields.get(1)?.to_string(),
                                delta: fields.get(2)?.parse().ok()?,
                            })
                        })();
                        let Some(command) = parsed else {
                            let _ = stream.write_all(b"error: invalid cycle-workspace command\n");
                            continue;
                        };
                        command
                    }
                    "quit" => ControlCommand::Quit,
                    _ if input.starts_with("drop ") => {
                        let fields = input.split_whitespace().collect::<Vec<_>>();
                        let parsed = (|| {
                            let source_window_id = fields.get(1)?.parse().ok()?;
                            let target_workspace_id = fields.get(2)?.parse().ok()?;
                            let target_window_id =
                                fields.get(3)?.parse::<u64>().ok().filter(|id| *id != 0);
                            let fraction = fields.get(4)?.parse::<f32>().ok()?.clamp(0.0, 1.0);
                            let cursor_x =
                                fields.get(5).and_then(|value| value.parse::<f64>().ok());
                            let cursor_y =
                                fields.get(6).and_then(|value| value.parse::<f64>().ok());
                            Some(ControlCommand::Drop {
                                source_window_id,
                                target_workspace_id,
                                target_window_id,
                                fraction,
                                cursor_x,
                                cursor_y,
                            })
                        })();
                        let Some(command) = parsed else {
                            let _ = stream.write_all(b"error: invalid drop command\n");
                            continue;
                        };
                        command
                    }
                    _ => {
                        let _ = stream.write_all(b"error: unknown command\n");
                        continue;
                    }
                };
                let (reply_tx, reply_rx) = bounded(1);
                if request_tx
                    .send(ControlRequest {
                        command,
                        reply: reply_tx,
                    })
                    .is_err()
                {
                    let _ = stream.write_all(b"error: daemon unavailable\n");
                    continue;
                }
                let response = reply_rx
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap_or_else(|_| "error: daemon timeout\n".into());
                let _ = stream.write_all(response.as_bytes());
            }
        })?;
    Ok(())
}

pub fn send(command: ControlCommand) -> Result<String> {
    let path = socket_path();
    let mut stream = UnixStream::connect(&path)
        .with_context(|| format!("niri-xmb daemon is not running ({})", path.display()))?;
    let command: String = match command {
        ControlCommand::Show => "show".into(),
        ControlCommand::Hide => "hide".into(),
        ControlCommand::Toggle => "toggle".into(),
        ControlCommand::Status => "status".into(),
        ControlCommand::Snapshot { output } => {
            output.map_or_else(|| "snapshot".into(), |name| format!("snapshot {name}"))
        }
        ControlCommand::FocusWindow { window_id } => format!("focus-window {window_id}"),
        ControlCommand::FocusWorkspace { workspace_id } => {
            format!("focus-workspace {workspace_id}")
        }
        ControlCommand::CycleWorkspace { output, delta } => {
            format!("cycle-workspace {output} {delta}")
        }
        ControlCommand::Drop {
            source_window_id,
            target_workspace_id,
            target_window_id,
            fraction,
            cursor_x,
            cursor_y,
        } => format!(
            "drop {source_window_id} {target_workspace_id} {} {fraction} {} {}",
            target_window_id.unwrap_or(0),
            cursor_x.map_or_else(|| "nan".into(), |value| value.to_string()),
            cursor_y.map_or_else(|| "nan".into(), |value| value.to_string())
        ),
        ControlCommand::Quit => "quit".into(),
    };
    stream.write_all(command.as_bytes())?;
    stream.shutdown(std::net::Shutdown::Write)?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    Ok(response)
}
