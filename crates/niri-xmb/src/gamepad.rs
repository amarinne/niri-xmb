//! Optional evdev gamepad button bridge.
//!
//! Gamepads are not part of the Wayland seat. Each enabled device gets a
//! reader thread that maps configured key codes to manual-popup actions and
//! forwards them to the daemon loop through a bounded channel. The threads
//! never block the Wayland or Niri event paths, and the device is never
//! grabbed, so other clients keep receiving its input.

use crate::config::{GamepadAction, GamepadConfig, gamepad_button_code};
use crate::wayland::WakeHandle;
use anyhow::Result;
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use evdev::{Device, EventType, InputEvent};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};

const ACTION_QUEUE: usize = 32;
const POLL_TIMEOUT: Timespec = Timespec {
    tv_sec: 0,
    tv_nsec: 200_000_000,
};
const GAMEPAD_EVENT_SUFFIXES: [&str; 2] = ["-event-joystick", "-event-gamepad"];
const BY_ID_DIR: &str = "/dev/input/by-id";

/// A running set of gamepad reader threads.
///
/// Dropping the service stops and joins every reader. The receiver is drained
/// by the daemon loop after it is woken.
pub struct GamepadService {
    rx: Receiver<GamepadAction>,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl GamepadService {
    /// Start readers when the feature is enabled.
    ///
    /// Configuration errors are already rejected by `Config::validate`, so a
    /// failure here is only logged. An unreadable device or a missing input
    /// permission must not stop the daemon.
    pub fn start(config: &GamepadConfig, wake: WakeHandle) -> Option<Self> {
        if !config.enabled {
            return None;
        }
        let bindings = match binding_map(&config.buttons) {
            Ok(bindings) => Arc::new(bindings),
            Err(error) => {
                tracing::error!(%error, "gamepad bindings are invalid; gamepad input disabled");
                return None;
            }
        };
        let (tx, rx) = bounded(ACTION_QUEUE);
        let stop = Arc::new(AtomicBool::new(false));
        let devices = discover_devices(&config.devices);
        if devices.is_empty() {
            tracing::warn!(
                "gamepad.enabled is true but no gamepad event devices were configured or discovered"
            );
            return Some(Self {
                rx,
                stop,
                threads: Vec::new(),
            });
        }

        let mut threads = Vec::new();
        for path in devices {
            let path_for_log = path.clone();
            let tx = tx.clone();
            let bindings = Arc::clone(&bindings);
            let wake = wake.clone();
            let stop = Arc::clone(&stop);
            match thread::Builder::new()
                .name("niri-xmb-gamepad".into())
                .spawn(move || reader_loop(path, bindings, tx, wake, stop))
            {
                Ok(handle) => threads.push(handle),
                Err(error) => {
                    tracing::warn!(
                        path = %path_for_log.display(),
                        %error,
                        "failed to spawn gamepad reader"
                    );
                }
            }
        }
        tracing::info!(
            devices = threads.len(),
            bindings = bindings.len(),
            "gamepad input enabled"
        );
        Some(Self { rx, stop, threads })
    }

    /// Next queued action, if any. Called by the daemon loop after a wake.
    pub fn try_recv(&self) -> Option<GamepadAction> {
        self.rx.try_recv().ok()
    }
}

impl Drop for GamepadService {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

fn binding_map(buttons: &BTreeMap<String, GamepadAction>) -> Result<HashMap<u16, GamepadAction>> {
    let mut bindings = HashMap::with_capacity(buttons.len());
    for (name, action) in buttons {
        let code = gamepad_button_code(name)?;
        if let Some(existing) = bindings.insert(code, *action) {
            anyhow::bail!(
                "gamepad buttons {name:?} and another binding both map to code {code:#06x} ({existing:?})"
            );
        }
    }
    Ok(bindings)
}

pub(crate) fn is_gamepad_event_name(name: &str) -> bool {
    GAMEPAD_EVENT_SUFFIXES
        .iter()
        .any(|suffix| name.ends_with(suffix))
}

/// Stable by-id paths when no explicit list is configured.
///
/// Configured paths are trimmed and deduplicated by their canonical device
/// node. Prefer `/dev/input/by-id/...` so a reconnect keeps the same path;
/// `/dev/input/eventN` is accepted for tests and unusual setups.
fn discover_devices(configured: &[String]) -> Vec<PathBuf> {
    if !configured.is_empty() {
        return dedupe_paths(configured.iter().map(|path| PathBuf::from(path.trim())));
    }

    let Ok(entries) = std::fs::read_dir(BY_ID_DIR) else {
        return Vec::new();
    };
    let mut paths = entries
        .flatten()
        .filter(|entry| is_gamepad_event_name(&entry.file_name().to_string_lossy()))
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    paths.sort();
    dedupe_paths(paths)
}

fn dedupe_paths(paths: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut unique = Vec::new();
    for path in paths {
        let identity = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if seen.insert(identity) {
            unique.push(path);
        }
    }
    unique
}

fn reader_loop(
    path: PathBuf,
    bindings: Arc<HashMap<u16, GamepadAction>>,
    tx: Sender<GamepadAction>,
    wake: WakeHandle,
    stop: Arc<AtomicBool>,
) {
    let mut device = match Device::open(&path) {
        Ok(device) => device,
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                %error,
                "cannot open gamepad; check the input group and udev permissions"
            );
            return;
        }
    };
    if let Err(error) = device.set_nonblocking(true) {
        tracing::warn!(
            path = %path.display(),
            %error,
            "cannot make gamepad nonblocking"
        );
        return;
    }
    tracing::info!(path = %path.display(), "gamepad reader started");

    let mut full_logged = false;
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let mut fds = [PollFd::new(&device, PollFlags::IN)];
        match poll(&mut fds, Some(&POLL_TIMEOUT)) {
            Ok(0) => continue,
            Ok(_) => {
                let ready = fds[0].revents();
                if ready.intersects(PollFlags::ERR | PollFlags::HUP | PollFlags::NVAL) {
                    break;
                }
                if !ready.contains(PollFlags::IN) {
                    continue;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "gamepad poll failed");
                break;
            }
        }

        let mut sent = false;
        match device.fetch_events() {
            Ok(events) => {
                for event in events {
                    let Some(action) = action_for_event(&event, &bindings) else {
                        continue;
                    };
                    match tx.try_send(action) {
                        Ok(()) => sent = true,
                        Err(TrySendError::Full(_)) => {
                            if !full_logged {
                                full_logged = true;
                                tracing::warn!("gamepad action queue full; dropping button input");
                            }
                        }
                        Err(TrySendError::Disconnected(_)) => return,
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "gamepad read failed");
                break;
            }
        }
        if sent {
            wake.wake();
        }
    }
    tracing::debug!(path = %path.display(), "gamepad reader stopped");
}

fn action_for_event(
    event: &InputEvent,
    bindings: &HashMap<u16, GamepadAction>,
) -> Option<GamepadAction> {
    if event.event_type() != EventType::KEY || event.value() != 1 {
        return None;
    }
    bindings.get(&event.code()).copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wayland::WakePipe;
    use evdev::KeyCode;
    use std::path::Path;

    fn bindings() -> BTreeMap<String, GamepadAction> {
        BTreeMap::from([
            ("BTN_SOUTH".into(), GamepadAction::Confirm),
            ("0x221".into(), GamepadAction::Down),
        ])
    }

    #[test]
    fn gamepad_event_names_use_stable_joystick_or_gamepad_suffixes() {
        assert!(is_gamepad_event_name("usb-test-event-joystick"));
        assert!(is_gamepad_event_name("usb-test-event-gamepad"));
        assert!(!is_gamepad_event_name("usb-test-event-kbd"));
        assert!(!is_gamepad_event_name("usb-test-event-mouse"));
    }

    #[test]
    fn binding_map_accepts_names_and_numeric_codes() {
        let map = binding_map(&bindings()).unwrap();
        assert_eq!(map.get(&0x130), Some(&GamepadAction::Confirm));
        assert_eq!(map.get(&0x221), Some(&GamepadAction::Down));
    }

    #[test]
    fn binding_map_rejects_duplicate_codes() {
        let duplicate = BTreeMap::from([
            ("BTN_SOUTH".into(), GamepadAction::Confirm),
            ("0x130".into(), GamepadAction::Close),
        ]);
        assert!(binding_map(&duplicate).is_err());
    }

    #[test]
    fn only_key_presses_become_actions() {
        let map = binding_map(&bindings()).unwrap();
        let press = InputEvent::new(EventType::KEY.0, KeyCode::BTN_SOUTH.0, 1);
        assert_eq!(action_for_event(&press, &map), Some(GamepadAction::Confirm));

        let release = InputEvent::new(EventType::KEY.0, KeyCode::BTN_SOUTH.0, 0);
        assert_eq!(action_for_event(&release, &map), None);
        let repeat = InputEvent::new(EventType::KEY.0, KeyCode::BTN_SOUTH.0, 2);
        assert_eq!(action_for_event(&repeat, &map), None);
        let other_type = InputEvent::new(EventType::SYNCHRONIZATION.0, 0, 1);
        assert_eq!(action_for_event(&other_type, &map), None);
        let unbound = InputEvent::new(EventType::KEY.0, KeyCode::BTN_EAST.0, 1);
        assert_eq!(action_for_event(&unbound, &map), None);
    }

    #[test]
    fn configured_device_paths_are_trimmed_and_used() {
        let devices = discover_devices(&[" /dev/input/event0 ".into()]);
        assert_eq!(devices, vec![PathBuf::from("/dev/input/event0")]);
    }

    #[test]
    fn disabled_gamepad_does_not_start_a_service() {
        let wake = WakePipe::new().unwrap().handle();
        assert!(GamepadService::start(&GamepadConfig::default(), wake).is_none());
    }

    #[test]
    fn gamepad_event_device_paths_are_preferred_by_suffix() {
        assert!(
            Path::new("/dev/input/by-id/usb-test-event-joystick")
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(is_gamepad_event_name)
        );
    }
}
