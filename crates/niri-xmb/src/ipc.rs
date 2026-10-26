use crate::subscribe::SubscriptionHub;
use crate::wayland::WakeHandle;
use anyhow::{Context, Result, bail};
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use std::fs::{File, OpenOptions, Permissions};
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

const MAX_REQUEST_BYTES: usize = 4096;
const CLIENT_WORKERS: usize = 4;
const CLIENT_QUEUE_CAPACITY: usize = 16;
const CLIENT_IO_TIMEOUT: Duration = Duration::from_secs(1);
/// Everything a drop is allowed, end to end: the Niri actions, then the
/// wait for the event stream to confirm them.
const DROP_BUDGET_MS: u64 = 2400;

/// The share of the budget reserved for confirmation, measured from when
/// the last Niri action returns.
const DROP_CONFIRM_MS: u64 = 1200;

/// What is left for the actions themselves, measured from submission.
///
/// This is enforced, not merely documented: a transaction whose actions
/// never come back is finished by the daemon when this expires, instead of
/// hanging until the client gives up.
pub const DROP_ACTION_BUDGET: Duration = Duration::from_millis(DROP_BUDGET_MS - DROP_CONFIRM_MS);

/// Maximum wait for the event stream after all Niri actions complete.
pub const DROP_CONFIRM_TIMEOUT: Duration = Duration::from_millis(DROP_CONFIRM_MS);

/// The whole budget, as a `Duration`.
#[cfg(test)]
pub const DROP_BUDGET: Duration = Duration::from_millis(DROP_BUDGET_MS);

/// How long the control socket waits for the daemon's reply.
///
/// Must strictly outlast the transaction it is waiting on. When it did
/// not, the socket reported "daemon timeout" and dropped the reply channel
/// for a drop still being confirmed, and the drop then replied into a dead
/// channel -- so a move that succeeded was reported as a failure.
const DAEMON_REPLY_TIMEOUT: Duration = Duration::from_millis(DROP_BUDGET_MS + 600);

/// How long the CLI waits on its socket read.
///
/// Strictly longer again than the daemon's own reply wait, so the two
/// cannot expire in the same instant and race over who reports the
/// timeout. These were both exactly three seconds.
const COMMAND_IO_TIMEOUT: Duration = Duration::from_millis(DROP_BUDGET_MS + 1200);

#[derive(Debug, Clone, PartialEq)]
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
    CycleWindow {
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
    /// Convert this connection into a persistent state stream. Never reaches
    /// the daemon loop; the control worker hands the stream to the hub.
    Subscribe {
        version: u32,
    },
    Quit,
}

pub struct ControlRequest {
    pub command: ControlCommand,
    pub reply: Sender<String>,
}

/// Owns the pathname for one bound control socket.
///
/// The identity check prevents an old daemon from removing a socket that was
/// replaced after external interference.
pub struct ControlServer {
    path: PathBuf,
    device: u64,
    inode: u64,
    _instance_lock: File,
}

impl Drop for ControlServer {
    fn drop(&mut self) {
        let matches_bound_socket = std::fs::symlink_metadata(&self.path).is_ok_and(|metadata| {
            metadata.file_type().is_socket()
                && metadata.dev() == self.device
                && metadata.ino() == self.inode
        });
        if matches_bound_socket
            && let Err(error) = std::fs::remove_file(&self.path)
            && error.kind() != ErrorKind::NotFound
        {
            tracing::debug!(%error, path = %self.path.display(), "failed to remove control socket");
        }
    }
}

pub fn socket_path() -> PathBuf {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe { libc::geteuid() })));
    runtime.join("niri-xmb.sock")
}

pub fn serve_with_subscriptions(
    request_tx: Sender<ControlRequest>,
    wake: WakeHandle,
    subscriptions: Arc<SubscriptionHub>,
) -> Result<ControlServer> {
    serve_at(socket_path(), request_tx, wake, Some(subscriptions))
}

fn serve_at(
    path: PathBuf,
    request_tx: Sender<ControlRequest>,
    wake: WakeHandle,
    subscriptions: Option<Arc<SubscriptionHub>>,
) -> Result<ControlServer> {
    let instance_lock = lock_instance(&path)?;
    let listener = bind_listener(&path)?;
    let metadata = std::fs::symlink_metadata(&path)
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    let server = ControlServer {
        path: path.clone(),
        device: metadata.dev(),
        inode: metadata.ino(),
        _instance_lock: instance_lock,
    };
    std::fs::set_permissions(&path, Permissions::from_mode(0o600))
        .with_context(|| format!("failed to protect {}", path.display()))?;

    let (client_tx, client_rx) = bounded::<UnixStream>(CLIENT_QUEUE_CAPACITY);
    for index in 0..CLIENT_WORKERS {
        let request_tx = request_tx.clone();
        let wake = wake.clone();
        let subscriptions = subscriptions.clone();
        let client_rx = client_rx.clone();
        thread::Builder::new()
            .name(format!("niri-xmb-control-{index}"))
            .spawn(move || client_worker(client_rx, request_tx, wake, subscriptions))?;
    }

    thread::Builder::new()
        .name("niri-xmb-control-listener".into())
        .spawn(move || {
            for incoming in listener.incoming() {
                let stream = match incoming {
                    Ok(stream) => stream,
                    Err(error) => {
                        tracing::warn!(%error, "failed to accept control client");
                        continue;
                    }
                };
                match client_tx.try_send(stream) {
                    Ok(()) => {}
                    Err(TrySendError::Full(mut stream)) => {
                        configure_stream(&stream);
                        let _ = stream.write_all(b"error: daemon busy\n");
                    }
                    Err(TrySendError::Disconnected(_)) => break,
                }
            }
        })?;
    Ok(server)
}

fn lock_instance(socket_path: &Path) -> Result<File> {
    let lock_path = socket_path.with_extension("lock");
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&lock_path)
        .with_context(|| format!("failed to open instance lock {}", lock_path.display()))?;
    std::fs::set_permissions(&lock_path, Permissions::from_mode(0o600))
        .with_context(|| format!("failed to protect instance lock {}", lock_path.display()))?;
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        Ok(file)
    } else {
        let error = std::io::Error::last_os_error();
        if error.kind() == ErrorKind::WouldBlock {
            bail!(
                "niri-xmb daemon is already running ({})",
                socket_path.display()
            );
        }
        Err(error).with_context(|| format!("failed to lock {}", lock_path.display()))
    }
}

fn bind_listener(path: &Path) -> Result<UnixListener> {
    for _ in 0..3 {
        match UnixListener::bind(path) {
            Ok(listener) => return Ok(listener),
            Err(error) if error.kind() == ErrorKind::AddrInUse => {}
            Err(error) => {
                return Err(error).with_context(|| format!("failed to bind {}", path.display()));
            }
        }

        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("failed to inspect {}", path.display()));
            }
        };
        if !metadata.file_type().is_socket() {
            bail!(
                "refusing to replace non-socket control path {}",
                path.display()
            );
        }

        match UnixStream::connect(path) {
            Ok(_) => bail!("niri-xmb daemon is already running ({})", path.display()),
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::ConnectionRefused | ErrorKind::NotFound
                ) =>
            {
                match std::fs::remove_file(path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("failed to remove stale control socket {}", path.display())
                        });
                    }
                }
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("cannot verify existing control socket {}", path.display())
                });
            }
        }
    }
    bail!("failed to claim control socket {}", path.display())
}

fn client_worker(
    client_rx: Receiver<UnixStream>,
    request_tx: Sender<ControlRequest>,
    wake: WakeHandle,
    subscriptions: Option<Arc<SubscriptionHub>>,
) {
    while let Ok(mut stream) = client_rx.recv() {
        configure_stream(&stream);
        if let Err(error) = handle_client(&mut stream, &request_tx, &wake, subscriptions.as_ref()) {
            tracing::debug!(%error, "control client failed");
        }
    }
}

fn configure_stream(stream: &UnixStream) {
    if let Err(error) = stream.set_read_timeout(Some(CLIENT_IO_TIMEOUT)) {
        tracing::debug!(%error, "failed to set control read timeout");
    }
    if let Err(error) = stream.set_write_timeout(Some(CLIENT_IO_TIMEOUT)) {
        tracing::debug!(%error, "failed to set control write timeout");
    }
}

fn handle_client(
    stream: &mut UnixStream,
    request_tx: &Sender<ControlRequest>,
    wake: &WakeHandle,
    subscriptions: Option<&Arc<SubscriptionHub>>,
) -> Result<()> {
    let input = match read_request(stream) {
        Ok(input) => input,
        Err(error) => {
            let response = if error.kind() == ErrorKind::InvalidData {
                "error: request too large or invalid\n"
            } else if matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) {
                "error: request timeout\n"
            } else {
                "error: failed to read request\n"
            };
            let _ = stream.write_all(response.as_bytes());
            return Err(error).context("failed to read control request");
        }
    };
    let command = match parse_command(&input) {
        Ok(command) => command,
        Err(response) => {
            stream.write_all(response.as_bytes())?;
            return Ok(());
        }
    };

    // A subscription never returns to the request/reply path: the connection
    // becomes a one-way frame stream owned by the hub.
    if let ControlCommand::Subscribe { version } = command {
        return match subscriptions {
            Some(hub) => {
                hub.attach(stream.try_clone()?, version);
                Ok(())
            }
            None => {
                stream.write_all(b"error: subscriptions unavailable\n")?;
                Ok(())
            }
        };
    }

    let (reply_tx, reply_rx) = bounded(1);
    if request_tx
        .send(ControlRequest {
            command,
            reply: reply_tx,
        })
        .is_err()
    {
        stream.write_all(b"error: daemon unavailable\n")?;
        return Ok(());
    }
    wake.wake();
    let response = reply_rx
        .recv_timeout(DAEMON_REPLY_TIMEOUT)
        .unwrap_or_else(|_| "error: daemon timeout\n".into());
    stream.write_all(response.as_bytes())?;
    Ok(())
}

fn read_request(stream: &mut UnixStream) -> std::io::Result<String> {
    let mut bytes = Vec::with_capacity(128);
    let mut limited = (&mut *stream).take((MAX_REQUEST_BYTES + 1) as u64);
    // Read one line instead of waiting for the peer to close its write half.
    // A subscriber keeps that half open for the life of the stream, and the
    // subscription's EOF watcher treats a read EOF as a disconnect. One-shot
    // clients still work: they either send a newline or shut down for EOF.
    BufReader::new(&mut limited).read_until(b'\n', &mut bytes)?;
    if bytes.len() > MAX_REQUEST_BYTES {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            "control request exceeds size limit",
        ));
    }
    String::from_utf8(bytes)
        .map_err(|_| std::io::Error::new(ErrorKind::InvalidData, "control request is not UTF-8"))
}

fn parse_command(command: &str) -> std::result::Result<ControlCommand, String> {
    let input = command.trim();
    let fields = input.split_whitespace().collect::<Vec<_>>();
    match fields.as_slice() {
        ["show"] => Ok(ControlCommand::Show),
        ["hide"] => Ok(ControlCommand::Hide),
        ["toggle"] => Ok(ControlCommand::Toggle),
        ["status"] => Ok(ControlCommand::Status),
        ["snapshot"] => Ok(ControlCommand::Snapshot { output: None }),
        ["snapshot", output] => Ok(ControlCommand::Snapshot {
            output: Some((*output).to_string()),
        }),
        ["focus-window", window_id] => window_id
            .parse()
            .map(|window_id| ControlCommand::FocusWindow { window_id })
            .map_err(|_| "error: invalid focus-window command\n".into()),
        ["focus-workspace", workspace_id] => workspace_id
            .parse()
            .map(|workspace_id| ControlCommand::FocusWorkspace { workspace_id })
            .map_err(|_| "error: invalid focus-workspace command\n".into()),
        ["cycle-workspace", output, delta] => delta
            .parse()
            .map(|delta| ControlCommand::CycleWorkspace {
                output: (*output).to_string(),
                delta,
            })
            .map_err(|_| "error: invalid cycle-workspace command\n".into()),
        ["cycle-window", output, delta] => delta
            .parse()
            .map(|delta| ControlCommand::CycleWindow {
                output: (*output).to_string(),
                delta,
            })
            .map_err(|_| "error: invalid cycle-window command\n".into()),
        ["quit"] => Ok(ControlCommand::Quit),
        ["subscribe", version] => version
            .parse()
            .map(|version| ControlCommand::Subscribe { version })
            .map_err(|_| "error: invalid subscribe command\n".into()),
        ["drop", source, workspace, target, fraction]
        | ["drop", source, workspace, target, fraction, _, _] => {
            let parsed = (|| {
                let source_window_id = source.parse().ok()?;
                let target_workspace_id = workspace.parse().ok()?;
                let target_window_id = target.parse::<u64>().ok().filter(|id| *id != 0);
                let fraction = fraction.parse::<f32>().ok()?;
                if !fraction.is_finite() || !(0.0..=1.0).contains(&fraction) {
                    return None;
                }
                let (cursor_x, cursor_y) = if fields.len() == 7 {
                    let x = parse_optional_coordinate(fields[5])?;
                    let y = parse_optional_coordinate(fields[6])?;
                    (x, y)
                } else {
                    (None, None)
                };
                Some(ControlCommand::Drop {
                    source_window_id,
                    target_workspace_id,
                    target_window_id,
                    fraction,
                    cursor_x,
                    cursor_y,
                })
            })();
            parsed.ok_or_else(|| "error: invalid drop command\n".into())
        }
        ["snapshot", ..] => Err("error: invalid snapshot command\n".into()),
        ["focus-window", ..] => Err("error: invalid focus-window command\n".into()),
        ["focus-workspace", ..] => Err("error: invalid focus-workspace command\n".into()),
        ["cycle-workspace", ..] => Err("error: invalid cycle-workspace command\n".into()),
        ["cycle-window", ..] => Err("error: invalid cycle-window command\n".into()),
        ["drop", ..] => Err("error: invalid drop command\n".into()),
        ["subscribe", ..] => Err("error: invalid subscribe command\n".into()),
        _ => Err("error: unknown command\n".into()),
    }
}

fn parse_optional_coordinate(value: &str) -> Option<Option<f64>> {
    let coordinate = value.parse::<f64>().ok()?;
    if coordinate.is_nan() {
        Some(None)
    } else if coordinate.is_finite() {
        Some(Some(coordinate))
    } else {
        None
    }
}

pub fn send(command: ControlCommand) -> Result<String> {
    let path = socket_path();
    let mut stream = UnixStream::connect(&path)
        .with_context(|| format!("niri-xmb daemon is not running ({})", path.display()))?;
    stream.set_write_timeout(Some(CLIENT_IO_TIMEOUT))?;
    stream.set_read_timeout(Some(COMMAND_IO_TIMEOUT))?;
    let command = serialize_command(command);
    stream.write_all(command.as_bytes())?;
    stream.shutdown(std::net::Shutdown::Write)?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    Ok(response)
}

fn serialize_command(command: ControlCommand) -> String {
    match command {
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
        ControlCommand::CycleWindow { output, delta } => {
            format!("cycle-window {output} {delta}")
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
        ControlCommand::Subscribe { version } => format!("subscribe {version}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wayland::WakePipe;
    use crossbeam_channel::unbounded;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let sequence = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "niri-xmb-ipc-test-{}-{sequence}",
                std::process::id()
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn socket(&self) -> PathBuf {
            self.0.join("control.sock")
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn raw_request(path: &Path, request: &[u8]) -> String {
        let mut stream = UnixStream::connect(path).unwrap();
        stream.write_all(request).unwrap();
        stream.shutdown(std::net::Shutdown::Write).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }

    fn sandbox_denied(error: &anyhow::Error) -> bool {
        error.chain().any(|cause| {
            cause
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == ErrorKind::PermissionDenied)
        })
    }

    macro_rules! socket_or_skip {
        ($operation:expr) => {
            match $operation {
                Ok(value) => value,
                Err(error) if sandbox_denied(&error) => return,
                Err(error) => panic!("socket operation failed: {error:#}"),
            }
        };
    }

    #[test]
    fn active_socket_cannot_be_taken_over() {
        let directory = TestDir::new();
        let path = directory.socket();
        let first = socket_or_skip!(bind_listener(&path));

        let error = bind_listener(&path).unwrap_err();

        assert!(error.to_string().contains("already running"));
        drop(first);
    }

    #[test]
    fn instance_lock_prevents_concurrent_start() {
        let directory = TestDir::new();
        let path = directory.socket();
        let first = lock_instance(&path).unwrap();

        let error = lock_instance(&path).unwrap_err();

        assert!(error.to_string().contains("already running"));
        drop(first);
        assert!(lock_instance(&path).is_ok());
    }

    #[test]
    fn stale_socket_is_removed_before_bind() {
        let directory = TestDir::new();
        let path = directory.socket();
        let stale = match UnixListener::bind(&path) {
            Ok(listener) => listener,
            Err(error) if error.kind() == ErrorKind::PermissionDenied => return,
            Err(error) => panic!("socket operation failed: {error}"),
        };
        drop(stale);

        let listener = bind_listener(&path).unwrap();

        assert!(path.exists());
        drop(listener);
    }

    #[test]
    fn non_socket_path_is_not_removed() {
        let directory = TestDir::new();
        let path = directory.socket();
        std::fs::write(&path, "keep").unwrap();

        let error = match bind_listener(&path) {
            Ok(_) => panic!("non-socket path was replaced"),
            Err(error) if sandbox_denied(&error) => return,
            Err(error) => error,
        };

        assert!(error.to_string().contains("non-socket"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "keep");
    }

    #[test]
    fn stalled_client_does_not_block_next_request() {
        let directory = TestDir::new();
        let path = directory.socket();
        let (request_tx, request_rx) = unbounded();
        let wake = WakePipe::new().unwrap();
        let _server = socket_or_skip!(serve_at(path.clone(), request_tx, wake.handle(), None));
        let _stalled = UnixStream::connect(&path).unwrap();

        let requester = thread::spawn({
            let path = path.clone();
            move || raw_request(&path, b"status")
        });
        let request = request_rx.recv_timeout(Duration::from_millis(500)).unwrap();
        assert_eq!(request.command, ControlCommand::Status);
        request.reply.send("ok\n".into()).unwrap();

        assert_eq!(requester.join().unwrap(), "ok\n");
    }

    #[test]
    fn oversized_request_is_rejected_without_dispatch() {
        let directory = TestDir::new();
        let path = directory.socket();
        let (request_tx, request_rx) = unbounded();
        let wake = WakePipe::new().unwrap();
        let _server = socket_or_skip!(serve_at(path.clone(), request_tx, wake.handle(), None));

        let response = raw_request(&path, &vec![b'x'; MAX_REQUEST_BYTES + 1]);

        assert_eq!(response, "error: request too large or invalid\n");
        assert!(request_rx.try_recv().is_err());
    }

    #[test]
    fn parser_rejects_trailing_fields_and_invalid_numbers() {
        assert!(parse_command("status extra").is_err());
        assert!(parse_command("cycle-workspace DP-1 1 extra").is_err());
        assert!(parse_command("cycle-window DP-1 nope").is_err());
        assert!(parse_command("drop 1 2 0 NaN").is_err());
        assert!(parse_command("drop 1 2 0 0.5 10").is_err());
    }

    #[test]
    fn cycle_commands_accept_negative_deltas() {
        assert_eq!(
            parse_command("cycle-workspace DP-1 -1"),
            Ok(ControlCommand::CycleWorkspace {
                output: "DP-1".into(),
                delta: -1,
            })
        );
        assert_eq!(
            parse_command("cycle-window DP-1 -1"),
            Ok(ControlCommand::CycleWindow {
                output: "DP-1".into(),
                delta: -1,
            })
        );
    }

    #[test]
    fn parser_returns_stable_control_errors() {
        assert_eq!(
            parse_command("bogus"),
            Err("error: unknown command\n".into())
        );
        assert_eq!(
            parse_command("snapshot eDP-1 extra"),
            Err("error: invalid snapshot command\n".into())
        );
        assert_eq!(
            parse_command("focus-window nope"),
            Err("error: invalid focus-window command\n".into())
        );
    }

    #[test]
    fn command_serialization_round_trips() {
        let commands = [
            ControlCommand::Status,
            ControlCommand::Snapshot {
                output: Some("eDP-1".into()),
            },
            ControlCommand::Subscribe { version: 1 },
            ControlCommand::CycleWindow {
                output: "DP-1".into(),
                delta: -1,
            },
            ControlCommand::Drop {
                source_window_id: 1,
                target_workspace_id: 2,
                target_window_id: None,
                fraction: 0.5,
                cursor_x: None,
                cursor_y: Some(20.0),
            },
        ];
        for command in commands {
            assert_eq!(
                parse_command(&serialize_command(command.clone())),
                Ok(command)
            );
        }
    }

    #[test]
    fn subscribe_request_becomes_a_frame_stream() {
        let directory = TestDir::new();
        let path = directory.socket();
        let (request_tx, _request_rx) = unbounded();
        let wake = WakePipe::new().unwrap();
        let subscriptions = Arc::new(SubscriptionHub::new());
        let _server = socket_or_skip!(serve_at(
            path.clone(),
            request_tx,
            wake.handle(),
            Some(subscriptions.clone())
        ));

        let mut stream = UnixStream::connect(&path).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream.write_all(b"subscribe 1\n").unwrap();
        // Keep the write half open. `niri-xmb subscribe` does the same now;
        // half-closing after the request used to make the server's EOF watcher
        // disconnect a healthy subscriber right after hello.
        let mut reader = std::io::BufReader::new(&mut stream);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let frame: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(frame["type"], "hello");
        assert_eq!(frame["protocol"], 1);

        // The request path leaves CLIENT_IO_TIMEOUT on the socket. A healthy
        // subscriber sends nothing after its request; sitting past that
        // timeout must not be mistaken for a disconnect.
        std::thread::sleep(CLIENT_IO_TIMEOUT + Duration::from_millis(250));

        subscriptions.publish(7, r#"{"generation":7}"#);
        line.clear();
        reader.read_line(&mut line).unwrap();
        let frame: serde_json::Value = serde_json::from_str(&line)
            .expect("a subscriber that keeps its write half open must still receive frames");
        assert_eq!(frame["type"], "snapshot");
        assert_eq!(frame["generation"], 7);
    }

    #[test]
    fn unsupported_subscribe_version_gets_an_error_frame() {
        let directory = TestDir::new();
        let path = directory.socket();
        let (request_tx, _request_rx) = unbounded();
        let wake = WakePipe::new().unwrap();
        let subscriptions = Arc::new(SubscriptionHub::new());
        let _server = socket_or_skip!(serve_at(
            path.clone(),
            request_tx,
            wake.handle(),
            Some(subscriptions)
        ));

        let mut stream = UnixStream::connect(&path).unwrap();
        stream.write_all(b"subscribe 7\n").unwrap();
        stream.shutdown(std::net::Shutdown::Write).unwrap();

        let mut line = String::new();
        std::io::BufRead::read_line(&mut std::io::BufReader::new(&mut stream), &mut line).unwrap();
        let frame: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(frame["type"], "error");
        assert_eq!(frame["code"], "unsupported_protocol");
    }

    /// The three waits are derived from one budget so they cannot invert or
    /// tie. A tie is as bad as an inversion: client and daemon expire in the
    /// same instant and race over who reports the timeout.
    #[test]
    fn the_control_socket_outlasts_the_drop_it_waits_on() {
        assert_eq!(
            DROP_ACTION_BUDGET + DROP_CONFIRM_TIMEOUT,
            DROP_BUDGET,
            "the phases must add up to the budget they are taken from"
        );
        assert!(
            DAEMON_REPLY_TIMEOUT > DROP_BUDGET,
            "the daemon's reply wait must outlast the whole drop"
        );
        assert!(
            COMMAND_IO_TIMEOUT > DAEMON_REPLY_TIMEOUT,
            "the client must outlast the daemon, or they race at the boundary"
        );
    }
}
