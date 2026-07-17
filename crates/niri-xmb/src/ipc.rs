use anyhow::{Context, Result};
use crossbeam_channel::{Sender, bounded};
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

#[derive(Debug, Clone, Copy)]
pub enum ControlCommand {
    Show,
    Hide,
    Toggle,
    Status,
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
                let command = match command.trim() {
                    "show" => ControlCommand::Show,
                    "hide" => ControlCommand::Hide,
                    "toggle" => ControlCommand::Toggle,
                    "status" => ControlCommand::Status,
                    "quit" => ControlCommand::Quit,
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
    let command = match command {
        ControlCommand::Show => "show",
        ControlCommand::Hide => "hide",
        ControlCommand::Toggle => "toggle",
        ControlCommand::Status => "status",
        ControlCommand::Quit => "quit",
    };
    stream.write_all(command.as_bytes())?;
    stream.shutdown(std::net::Shutdown::Write)?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    Ok(response)
}
