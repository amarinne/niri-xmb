//! Optional evdev gamepad button bridge.
//!
//! Gamepads are not part of the Wayland seat. Each enabled device gets a
//! reader thread that maps configured key codes to manual-popup actions and
//! forwards them to the daemon loop through a bounded channel. The threads
//! never block the Wayland or Niri event paths, and the device is never
//! grabbed, so other clients keep receiving its input.

use crate::config::{GamepadAction, GamepadConfig, GamepadPreset, gamepad_button_code};
use crate::wayland::WakeHandle;
use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use evdev::{AbsoluteAxisCode, Device, EventType, InputEvent, KeyCode};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const ACTION_QUEUE: usize = 32;
const POLL_TIMEOUT: Timespec = Timespec {
    tv_sec: 0,
    tv_nsec: 200_000_000,
};
const HOTPLUG_POLL_INTERVAL: Duration = Duration::from_millis(500);
const HOTPLUG_SLEEP_SLICE: Duration = Duration::from_millis(100);
const READER_RETRY_BACKOFF: Duration = Duration::from_secs(2);
const GAMEPAD_EVENT_SUFFIXES: [&str; 2] = ["-event-joystick", "-event-gamepad"];
const BY_ID_DIR: &str = "/dev/input/by-id";

/// A running gamepad hotplug watcher.
///
/// Dropping the service stops the watcher and joins every reader. The receiver
/// is drained by the daemon loop after it is woken.
pub struct GamepadService {
    rx: Receiver<GamepadAction>,
    stop: Arc<AtomicBool>,
    supervisor: Option<JoinHandle<()>>,
}

struct ActiveReader {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

impl GamepadService {
    /// Start the hotplug watcher when the feature is enabled.
    ///
    /// Configuration errors are already rejected by `Config::validate`, so a
    /// failure here is only logged. An absent, unreadable, or unplugged device
    /// must not stop the daemon; the watcher retries while it stays present.
    pub fn start(config: &GamepadConfig, wake: WakeHandle) -> Option<Self> {
        if !config.enabled {
            return None;
        }
        let combo_codes = match config.open_combo_codes() {
            Ok(codes) => codes,
            Err(error) => {
                tracing::error!(%error, "gamepad open combo is invalid; gamepad input disabled");
                return None;
            }
        };
        let (tx, rx) = bounded(ACTION_QUEUE);
        let stop = Arc::new(AtomicBool::new(false));
        let config = Arc::new(config.clone());
        let combo_codes = Arc::new(combo_codes);
        let supervisor = {
            let tx = tx.clone();
            let wake = wake.clone();
            let stop = Arc::clone(&stop);
            let config = Arc::clone(&config);
            let combo_codes = Arc::clone(&combo_codes);
            thread::Builder::new()
                .name("niri-xmb-gamepad-hotplug".into())
                .spawn(move || supervisor_loop(config, combo_codes, tx, wake, stop))
        };
        match supervisor {
            Ok(handle) => Some(Self {
                rx,
                stop,
                supervisor: Some(handle),
            }),
            Err(error) => {
                tracing::error!(
                    %error,
                    "failed to start gamepad hotplug watcher; gamepad input disabled"
                );
                None
            }
        }
    }

    /// Next queued action, if any. Called by the daemon loop after a wake.
    pub fn try_recv(&self) -> Option<GamepadAction> {
        self.rx.try_recv().ok()
    }
}

impl Drop for GamepadService {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(supervisor) = self.supervisor.take() {
            let _ = supervisor.join();
        }
    }
}

/// Standard Xbox/xpad positional layout.
///
/// The Flydigi VADER4 exposes this profile through xpad even though its
/// D-pad arrives as `ABS_HAT0X/Y` rather than `BTN_DPAD_*` key events.
const XBOX_PRESET: &[(&str, GamepadAction)] = &[
    ("BTN_DPAD_UP", GamepadAction::Up),
    ("BTN_DPAD_DOWN", GamepadAction::Down),
    ("BTN_DPAD_LEFT", GamepadAction::Left),
    ("BTN_DPAD_RIGHT", GamepadAction::Right),
    ("BTN_SOUTH", GamepadAction::Confirm),
    ("BTN_EAST", GamepadAction::Close),
    ("BTN_WEST", GamepadAction::Back),
    ("BTN_NORTH", GamepadAction::Tab),
    ("BTN_TL", GamepadAction::Back),
    ("BTN_TR", GamepadAction::Tab),
    ("BTN_SELECT", GamepadAction::Back),
    ("BTN_START", GamepadAction::Confirm),
    ("BTN_MODE", GamepadAction::Close),
];

/// Standard DualShock 4 positional layout.
///
/// Linux exposes the same positional key codes as Xbox. The preset is kept
/// separate so the two families can diverge without changing user overrides.
const DS4_PRESET: &[(&str, GamepadAction)] = &[
    ("BTN_DPAD_UP", GamepadAction::Up),
    ("BTN_DPAD_DOWN", GamepadAction::Down),
    ("BTN_DPAD_LEFT", GamepadAction::Left),
    ("BTN_DPAD_RIGHT", GamepadAction::Right),
    ("BTN_SOUTH", GamepadAction::Confirm),
    ("BTN_EAST", GamepadAction::Close),
    ("BTN_WEST", GamepadAction::Back),
    ("BTN_NORTH", GamepadAction::Tab),
    ("BTN_TL", GamepadAction::Back),
    ("BTN_TR", GamepadAction::Tab),
    ("BTN_SELECT", GamepadAction::Back),
    ("BTN_START", GamepadAction::Confirm),
    ("BTN_MODE", GamepadAction::Close),
];

fn has_gamepad_input(bindings: &HashMap<u16, GamepadAction>, combo_codes: &HashSet<u16>) -> bool {
    !bindings.is_empty() || !combo_codes.is_empty()
}

fn preset_bindings(preset: GamepadPreset) -> &'static [(&'static str, GamepadAction)] {
    match preset {
        GamepadPreset::Xbox => XBOX_PRESET,
        GamepadPreset::Ds4 => DS4_PRESET,
        GamepadPreset::None | GamepadPreset::Auto => &[],
    }
}

/// Merge a preset with the explicit config map. Explicit entries win by
/// evdev code, so `0x130` overrides a preset's `BTN_SOUTH` entry.
fn binding_map_for(
    config: &GamepadConfig,
    preset: GamepadPreset,
) -> Result<HashMap<u16, GamepadAction>> {
    let mut bindings = HashMap::new();
    for (name, action) in preset_bindings(preset) {
        bindings.insert(gamepad_button_code(name)?, *action);
    }
    for (name, action) in &config.buttons {
        bindings.insert(gamepad_button_code(name)?, *action);
    }
    Ok(bindings)
}

fn preset_from_identity(
    path: &str,
    name: &str,
    vendor: u16,
    has_face_buttons: bool,
) -> GamepadPreset {
    let identity = format!("{path} {name}").to_ascii_lowercase();
    if vendor == 0x054c
        || identity.contains("dualshock")
        || identity.contains("dualsense")
        || identity.contains("sony")
        || identity.contains("wireless controller")
        || identity.contains("ds4")
    {
        return GamepadPreset::Ds4;
    }
    if identity.contains("flydigi")
        || identity.contains("vader")
        || identity.contains("xbox")
        || identity.contains("x-box")
        || identity.contains("xpad")
        || identity.contains("microsoft")
        || has_face_buttons
    {
        return GamepadPreset::Xbox;
    }
    GamepadPreset::None
}

fn detect_preset(path: &Path, device: &Device) -> GamepadPreset {
    let has_face_buttons = device
        .supported_keys()
        .is_some_and(|keys| keys.contains(KeyCode::BTN_SOUTH) && keys.contains(KeyCode::BTN_EAST));
    preset_from_identity(
        &path.display().to_string(),
        device.name().unwrap_or(""),
        device.input_id().vendor(),
        has_face_buttons,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HatAxis {
    X,
    Y,
}

const DPAD_UP: usize = 0;
const DPAD_DOWN: usize = 1;
const DPAD_LEFT: usize = 2;
const DPAD_RIGHT: usize = 3;
const DPAD_UP_CODE: u16 = KeyCode::BTN_DPAD_UP.0;
const DPAD_DOWN_CODE: u16 = KeyCode::BTN_DPAD_DOWN.0;
const DPAD_LEFT_CODE: u16 = KeyCode::BTN_DPAD_LEFT.0;
const DPAD_RIGHT_CODE: u16 = KeyCode::BTN_DPAD_RIGHT.0;

fn dpad_index(code: u16) -> Option<usize> {
    match code {
        DPAD_UP_CODE => Some(DPAD_UP),
        DPAD_DOWN_CODE => Some(DPAD_DOWN),
        DPAD_LEFT_CODE => Some(DPAD_LEFT),
        DPAD_RIGHT_CODE => Some(DPAD_RIGHT),
        _ => None,
    }
}

fn dpad_code(index: usize) -> u16 {
    match index {
        DPAD_UP => DPAD_UP_CODE,
        DPAD_DOWN => DPAD_DOWN_CODE,
        DPAD_LEFT => DPAD_LEFT_CODE,
        DPAD_RIGHT => DPAD_RIGHT_CODE,
        _ => unreachable!("gamepad D-pad index"),
    }
}

fn hat_direction(axis: HatAxis, value: i32) -> Option<usize> {
    match (axis, value.signum()) {
        (HatAxis::X, -1) => Some(DPAD_LEFT),
        (HatAxis::X, 1) => Some(DPAD_RIGHT),
        (HatAxis::Y, -1) => Some(DPAD_UP),
        (HatAxis::Y, 1) => Some(DPAD_DOWN),
        _ => None,
    }
}

/// Translate key, hat, and open-combo events into one action while
/// suppressing duplicate reports when a device exposes both `BTN_DPAD_*` and
/// `ABS_HAT0*`.
#[derive(Debug, Default)]
struct HatTracker {
    hat_x: i32,
    hat_y: i32,
    /// A direction whose hat transition already dispatched an action.
    hat_active: [bool; 4],
    /// A bound `BTN_DPAD_*` key currently owns this direction.
    dpad_down: [bool; 4],
    /// Configured simultaneous codes; empty disables the open combo.
    combo_codes: HashSet<u16>,
    /// Combo codes currently held down.
    combo_down: HashSet<u16>,
    /// The current completion already emitted Open.
    combo_fired: bool,
}

impl HatTracker {
    fn new(combo_codes: HashSet<u16>) -> Self {
        Self {
            combo_codes,
            ..Self::default()
        }
    }

    fn update(
        &mut self,
        event: &InputEvent,
        bindings: &HashMap<u16, GamepadAction>,
    ) -> Option<GamepadAction> {
        match event.event_type() {
            EventType::KEY => {
                let completed = self.combo_event(event.code(), event.value());
                let action = self.key_event(event.code(), event.value(), bindings);
                if completed {
                    Some(GamepadAction::Open)
                } else {
                    action
                }
            }
            EventType::ABSOLUTE => self.absolute_event(event.code(), event.value(), bindings),
            _ => None,
        }
    }

    /// Track simultaneous combo presses. Returns true only on the press that
    /// completes the combo; a release re-arms it for the next completion.
    fn combo_event(&mut self, code: u16, value: i32) -> bool {
        if !self.combo_codes.contains(&code) {
            return false;
        }
        match value {
            1 => {
                self.combo_down.insert(code);
                if !self.combo_fired && self.combo_down.len() == self.combo_codes.len() {
                    self.combo_fired = true;
                    return true;
                }
            }
            0 => {
                self.combo_down.remove(&code);
                self.combo_fired = false;
            }
            _ => {}
        }
        false
    }

    fn key_event(
        &mut self,
        code: u16,
        value: i32,
        bindings: &HashMap<u16, GamepadAction>,
    ) -> Option<GamepadAction> {
        if let Some(index) = dpad_index(code) {
            return match value {
                1 => {
                    let bound = bindings.contains_key(&code);
                    let duplicate = self.hat_active[index];
                    self.dpad_down[index] = bound;
                    if bound && !duplicate {
                        bindings.get(&code).copied()
                    } else {
                        None
                    }
                }
                0 => {
                    self.dpad_down[index] = false;
                    None
                }
                _ => None,
            };
        }
        if value == 1 {
            bindings.get(&code).copied()
        } else {
            None
        }
    }

    fn absolute_event(
        &mut self,
        code: u16,
        value: i32,
        bindings: &HashMap<u16, GamepadAction>,
    ) -> Option<GamepadAction> {
        let axis = if code == AbsoluteAxisCode::ABS_HAT0X.0 {
            HatAxis::X
        } else if code == AbsoluteAxisCode::ABS_HAT0Y.0 {
            HatAxis::Y
        } else {
            return None;
        };
        let old = match axis {
            HatAxis::X => self.hat_x,
            HatAxis::Y => self.hat_y,
        };
        let new = value.signum();
        if new == old {
            return None;
        }
        if let Some(index) = hat_direction(axis, old) {
            self.hat_active[index] = false;
        }
        match axis {
            HatAxis::X => self.hat_x = new,
            HatAxis::Y => self.hat_y = new,
        }
        let index = hat_direction(axis, new)?;
        let code = dpad_code(index);
        let bound = bindings.contains_key(&code);
        self.hat_active[index] = bound;
        if bound && !self.dpad_down[index] {
            bindings.get(&code).copied()
        } else {
            None
        }
    }
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

fn hotplug_diff(desired: &[PathBuf], active: &HashSet<PathBuf>) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let desired_set: HashSet<&PathBuf> = desired.iter().collect();
    let start = desired
        .iter()
        .filter(|path| !active.contains(*path))
        .cloned()
        .collect();
    let stop = active
        .iter()
        .filter(|path| !desired_set.contains(path))
        .cloned()
        .collect();
    (start, stop)
}

fn supervisor_loop(
    config: Arc<GamepadConfig>,
    combo_codes: Arc<HashSet<u16>>,
    tx: Sender<GamepadAction>,
    wake: WakeHandle,
    stop: Arc<AtomicBool>,
) {
    let mut active: HashMap<PathBuf, ActiveReader> = HashMap::new();
    let mut retry_at: HashMap<PathBuf, Instant> = HashMap::new();
    let mut failures: HashSet<PathBuf> = HashSet::new();
    let mut skipped: HashSet<PathBuf> = HashSet::new();
    tracing::info!(combo = combo_codes.len(), "gamepad hotplug watcher started");

    while !stop.load(Ordering::Relaxed) {
        let desired = discover_devices(&config.devices);
        let desired_set: HashSet<PathBuf> = desired.iter().cloned().collect();
        retry_at.retain(|path, _| desired_set.contains(path));
        failures.retain(|path| desired_set.contains(path));
        skipped.retain(|path| desired_set.contains(path));

        let finished: Vec<PathBuf> = active
            .iter()
            .filter(|(_, reader)| reader.handle.is_finished())
            .map(|(path, _)| path.clone())
            .collect();
        for path in finished {
            if let Some(reader) = active.remove(&path) {
                let _ = reader.handle.join();
                tracing::info!(path = %path.display(), "gamepad reader stopped");
            }
            if desired_set.contains(&path) {
                retry_at.insert(path, Instant::now() + READER_RETRY_BACKOFF);
            }
        }

        let active_paths: HashSet<PathBuf> = active.keys().cloned().collect();
        let (start_paths, stale_paths) = hotplug_diff(&desired, &active_paths);
        for path in stale_paths {
            if let Some(reader) = active.remove(&path) {
                reader.stop.store(true, Ordering::Relaxed);
                let _ = reader.handle.join();
                tracing::info!(path = %path.display(), "gamepad disconnected");
            }
            retry_at.remove(&path);
            failures.remove(&path);
            skipped.remove(&path);
        }

        for path in start_paths {
            if skipped.contains(&path) {
                continue;
            }
            let now = Instant::now();
            if retry_at.get(&path).is_some_and(|until| now < *until) {
                continue;
            }
            match start_reader(&path, &config, &combo_codes, &tx, &wake) {
                Ok(Some(reader)) => {
                    active.insert(path.clone(), reader);
                    retry_at.remove(&path);
                    failures.remove(&path);
                    skipped.remove(&path);
                }
                Ok(None) => {
                    retry_at.remove(&path);
                    failures.remove(&path);
                    skipped.insert(path);
                }
                Err(error) => {
                    if failures.insert(path.clone()) {
                        tracing::warn!(
                            path = %path.display(),
                            %error,
                            "cannot start gamepad reader; will retry"
                        );
                    }
                    retry_at.insert(path, now + READER_RETRY_BACKOFF);
                }
            }
        }

        let deadline = Instant::now() + HOTPLUG_POLL_INTERVAL;
        while !stop.load(Ordering::Relaxed) && Instant::now() < deadline {
            thread::sleep(HOTPLUG_SLEEP_SLICE);
        }
    }

    for (path, reader) in active.drain() {
        reader.stop.store(true, Ordering::Relaxed);
        let _ = reader.handle.join();
        tracing::debug!(path = %path.display(), "gamepad reader stopped");
    }
    tracing::info!("gamepad hotplug watcher stopped");
}

fn start_reader(
    path: &Path,
    config: &GamepadConfig,
    combo_codes: &HashSet<u16>,
    tx: &Sender<GamepadAction>,
    wake: &WakeHandle,
) -> Result<Option<ActiveReader>> {
    let device =
        Device::open(path).with_context(|| format!("cannot open gamepad {}", path.display()))?;
    device
        .set_nonblocking(true)
        .with_context(|| format!("cannot make gamepad nonblocking: {}", path.display()))?;
    let preset = match config.preset {
        GamepadPreset::Auto => detect_preset(path, &device),
        preset => preset,
    };
    let bindings = binding_map_for(config, preset)
        .with_context(|| format!("gamepad bindings for {}", path.display()))?;
    if !has_gamepad_input(&bindings, combo_codes) {
        tracing::warn!(
            path = %path.display(),
            preset = ?preset,
            "gamepad has no button bindings or open combo; skipping"
        );
        return Ok(None);
    }

    let name = device.name().unwrap_or("unknown").to_string();
    let binding_count = bindings.len();
    let bindings = Arc::new(bindings);
    let combo_codes = combo_codes.clone();
    let reader_stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&reader_stop);
    let thread_path = path.to_path_buf();
    let tx = tx.clone();
    let wake = wake.clone();
    let handle = thread::Builder::new()
        .name("niri-xmb-gamepad".into())
        .spawn(move || {
            reader_loop(
                thread_path,
                device,
                bindings,
                combo_codes,
                tx,
                wake,
                thread_stop,
            )
        })
        .context("failed to spawn gamepad reader")?;
    tracing::info!(
        path = %path.display(),
        name = %name,
        preset = ?preset,
        bindings = binding_count,
        "gamepad reader enabled"
    );
    Ok(Some(ActiveReader {
        stop: reader_stop,
        handle,
    }))
}

fn reader_loop(
    path: PathBuf,
    mut device: Device,
    bindings: Arc<HashMap<u16, GamepadAction>>,
    combo_codes: HashSet<u16>,
    tx: Sender<GamepadAction>,
    wake: WakeHandle,
    stop: Arc<AtomicBool>,
) {
    let mut tracker = HatTracker::new(combo_codes);
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
                    let Some(action) = tracker.update(&event, &bindings) else {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wayland::WakePipe;
    use evdev::KeyCode;
    use std::collections::BTreeMap;
    use std::path::Path;

    fn config_with(buttons: &[(&str, GamepadAction)]) -> GamepadConfig {
        GamepadConfig {
            enabled: true,
            preset: GamepadPreset::None,
            devices: Vec::new(),
            buttons: buttons
                .iter()
                .map(|(name, action)| ((*name).to_string(), *action))
                .collect(),
            open_combo: Vec::new(),
        }
    }

    fn explicit_bindings() -> HashMap<u16, GamepadAction> {
        binding_map_for(
            &config_with(&[
                ("BTN_SOUTH", GamepadAction::Confirm),
                ("0x221", GamepadAction::Down),
            ]),
            GamepadPreset::None,
        )
        .unwrap()
    }

    fn preset_map(preset: GamepadPreset) -> HashMap<u16, GamepadAction> {
        binding_map_for(&GamepadConfig::default(), preset).unwrap()
    }

    fn key(code: KeyCode, value: i32) -> InputEvent {
        InputEvent::new(EventType::KEY.0, code.0, value)
    }

    fn hat(axis: AbsoluteAxisCode, value: i32) -> InputEvent {
        InputEvent::new(EventType::ABSOLUTE.0, axis.0, value)
    }

    fn combo_tracker(codes: &[KeyCode]) -> HatTracker {
        HatTracker::new(codes.iter().map(|code| code.0).collect())
    }

    #[test]
    fn gamepad_event_names_use_stable_joystick_or_gamepad_suffixes() {
        assert!(is_gamepad_event_name("usb-test-event-joystick"));
        assert!(is_gamepad_event_name("usb-test-event-gamepad"));
        assert!(!is_gamepad_event_name("usb-test-event-kbd"));
        assert!(!is_gamepad_event_name("usb-test-event-mouse"));
    }

    #[test]
    fn explicit_bindings_accept_names_and_numeric_codes() {
        let map = explicit_bindings();
        assert_eq!(map.get(&0x130), Some(&GamepadAction::Confirm));
        assert_eq!(map.get(&0x221), Some(&GamepadAction::Down));
    }

    #[test]
    fn combo_only_input_still_starts_a_reader() {
        assert!(has_gamepad_input(
            &HashMap::new(),
            &HashSet::from([KeyCode::BTN_SELECT.0, KeyCode::BTN_START.0])
        ));
        assert!(has_gamepad_input(&explicit_bindings(), &HashSet::new()));
        assert!(!has_gamepad_input(&HashMap::new(), &HashSet::new()));
    }

    #[test]
    fn xbox_preset_covers_dpad_face_and_system_buttons() {
        let map = preset_map(GamepadPreset::Xbox);
        assert_eq!(map.get(&KeyCode::BTN_DPAD_UP.0), Some(&GamepadAction::Up));
        assert_eq!(
            map.get(&KeyCode::BTN_DPAD_DOWN.0),
            Some(&GamepadAction::Down)
        );
        assert_eq!(
            map.get(&KeyCode::BTN_DPAD_LEFT.0),
            Some(&GamepadAction::Left)
        );
        assert_eq!(
            map.get(&KeyCode::BTN_DPAD_RIGHT.0),
            Some(&GamepadAction::Right)
        );
        assert_eq!(
            map.get(&KeyCode::BTN_SOUTH.0),
            Some(&GamepadAction::Confirm)
        );
        assert_eq!(map.get(&KeyCode::BTN_EAST.0), Some(&GamepadAction::Close));
        assert_eq!(map.get(&KeyCode::BTN_WEST.0), Some(&GamepadAction::Back));
        assert_eq!(map.get(&KeyCode::BTN_NORTH.0), Some(&GamepadAction::Tab));
        assert_eq!(
            map.get(&KeyCode::BTN_START.0),
            Some(&GamepadAction::Confirm)
        );
        assert_eq!(map.get(&KeyCode::BTN_MODE.0), Some(&GamepadAction::Close));
        assert_eq!(map.len(), XBOX_PRESET.len());
    }

    #[test]
    fn ds4_preset_covers_dpad_face_and_system_buttons() {
        let map = preset_map(GamepadPreset::Ds4);
        assert_eq!(map.get(&KeyCode::BTN_DPAD_UP.0), Some(&GamepadAction::Up));
        assert_eq!(
            map.get(&KeyCode::BTN_DPAD_LEFT.0),
            Some(&GamepadAction::Left)
        );
        assert_eq!(
            map.get(&KeyCode::BTN_SOUTH.0),
            Some(&GamepadAction::Confirm)
        );
        assert_eq!(map.get(&KeyCode::BTN_EAST.0), Some(&GamepadAction::Close));
        assert_eq!(
            map.get(&KeyCode::BTN_START.0),
            Some(&GamepadAction::Confirm)
        );
        assert_eq!(map.len(), DS4_PRESET.len());
    }

    #[test]
    fn explicit_overrides_win_over_a_preset_by_code() {
        let config = GamepadConfig {
            enabled: true,
            preset: GamepadPreset::Xbox,
            devices: Vec::new(),
            buttons: BTreeMap::from([
                ("0x130".into(), GamepadAction::Close),
                ("BTN_EAST".into(), GamepadAction::Tab),
            ]),
            open_combo: Vec::new(),
        };
        let map = binding_map_for(&config, GamepadPreset::Xbox).unwrap();
        assert_eq!(map.get(&KeyCode::BTN_SOUTH.0), Some(&GamepadAction::Close));
        assert_eq!(map.get(&KeyCode::BTN_EAST.0), Some(&GamepadAction::Tab));
    }

    #[test]
    fn auto_detection_maps_flydigi_vader_to_xbox() {
        assert_eq!(
            preset_from_identity(
                "/dev/input/by-id/usb-Flydigi_Flydigi_VADER4-event-joystick",
                "Flydigi VADER4",
                0x045e,
                true
            ),
            GamepadPreset::Xbox
        );
    }

    #[test]
    fn auto_detection_maps_sony_devices_to_ds4() {
        assert_eq!(
            preset_from_identity(
                "/dev/input/by-id/usb-Sony_Interactive_Entertainment_Wireless_Controller-event-joystick",
                "Sony Interactive Entertainment Wireless Controller",
                0,
                true
            ),
            GamepadPreset::Ds4
        );
        assert_eq!(
            preset_from_identity("", "", 0x054c, true),
            GamepadPreset::Ds4
        );
    }

    #[test]
    fn auto_detection_uses_xbox_for_other_face_button_gamepads() {
        assert_eq!(
            preset_from_identity("", "Microsoft X-Box 360 pad", 0x045e, true),
            GamepadPreset::Xbox
        );
        assert_eq!(
            preset_from_identity("", "Unknown pad", 0, true),
            GamepadPreset::Xbox
        );
        assert_eq!(
            preset_from_identity("", "Unknown hat-only device", 0, false),
            GamepadPreset::None
        );
    }

    #[test]
    fn only_key_presses_become_actions() {
        let map = explicit_bindings();
        let mut tracker = HatTracker::default();
        assert_eq!(
            tracker.update(&key(KeyCode::BTN_SOUTH, 1), &map),
            Some(GamepadAction::Confirm)
        );
        assert_eq!(tracker.update(&key(KeyCode::BTN_SOUTH, 0), &map), None);
        assert_eq!(tracker.update(&key(KeyCode::BTN_SOUTH, 2), &map), None);
        assert_eq!(
            tracker.update(&InputEvent::new(EventType::SYNCHRONIZATION.0, 0, 1), &map),
            None
        );
        assert_eq!(tracker.update(&key(KeyCode::BTN_EAST, 1), &map), None);
    }

    #[test]
    fn combo_requires_simultaneous_presses_and_fires_once() {
        let map = preset_map(GamepadPreset::Xbox);
        let mut tracker = combo_tracker(&[KeyCode::BTN_SELECT, KeyCode::BTN_START]);

        assert_eq!(
            tracker.update(&key(KeyCode::BTN_SELECT, 1), &map),
            Some(GamepadAction::Back)
        );
        assert_eq!(tracker.update(&key(KeyCode::BTN_SELECT, 0), &map), None);
        assert_eq!(
            tracker.update(&key(KeyCode::BTN_START, 1), &map),
            Some(GamepadAction::Confirm)
        );
        assert_eq!(tracker.update(&key(KeyCode::BTN_START, 0), &map), None);

        assert_eq!(
            tracker.update(&key(KeyCode::BTN_SELECT, 1), &map),
            Some(GamepadAction::Back)
        );
        assert_eq!(
            tracker.update(&key(KeyCode::BTN_START, 1), &map),
            Some(GamepadAction::Open)
        );
        assert_eq!(tracker.update(&key(KeyCode::BTN_START, 2), &map), None);
    }

    #[test]
    fn combo_rearms_after_one_combo_button_releases() {
        let map = HashMap::new();
        let mut tracker = combo_tracker(&[KeyCode::BTN_SELECT, KeyCode::BTN_START]);

        assert_eq!(tracker.update(&key(KeyCode::BTN_SELECT, 1), &map), None);
        assert_eq!(
            tracker.update(&key(KeyCode::BTN_START, 1), &map),
            Some(GamepadAction::Open)
        );
        assert_eq!(tracker.update(&key(KeyCode::BTN_START, 0), &map), None);
        assert_eq!(
            tracker.update(&key(KeyCode::BTN_START, 1), &map),
            Some(GamepadAction::Open)
        );
        assert_eq!(tracker.update(&key(KeyCode::BTN_SELECT, 0), &map), None);
        assert_eq!(
            tracker.update(&key(KeyCode::BTN_SELECT, 1), &map),
            Some(GamepadAction::Open)
        );
        assert_eq!(tracker.update(&key(KeyCode::BTN_SELECT, 1), &map), None);
    }

    #[test]
    fn combo_completion_replaces_that_buttons_action() {
        let map = preset_map(GamepadPreset::Xbox);
        let mut tracker = combo_tracker(&[KeyCode::BTN_SELECT, KeyCode::BTN_START]);

        assert_eq!(
            tracker.update(&key(KeyCode::BTN_SELECT, 1), &map),
            Some(GamepadAction::Back)
        );
        assert_eq!(
            tracker.update(&key(KeyCode::BTN_START, 1), &map),
            Some(GamepadAction::Open),
            "the completing press must not also confirm"
        );
    }

    #[test]
    fn empty_combo_preserves_normal_button_actions() {
        let map = preset_map(GamepadPreset::Xbox);
        let mut tracker = HatTracker::default();

        assert_eq!(
            tracker.update(&key(KeyCode::BTN_SELECT, 1), &map),
            Some(GamepadAction::Back)
        );
        assert_eq!(
            tracker.update(&key(KeyCode::BTN_START, 1), &map),
            Some(GamepadAction::Confirm)
        );
    }

    #[test]
    fn hat_axis_values_map_to_dpad_actions() {
        let map = preset_map(GamepadPreset::Xbox);
        let mut tracker = HatTracker::default();

        assert_eq!(
            tracker.update(&hat(AbsoluteAxisCode::ABS_HAT0X, -1), &map),
            Some(GamepadAction::Left)
        );
        assert_eq!(
            tracker.update(&hat(AbsoluteAxisCode::ABS_HAT0X, -1), &map),
            None,
            "a held hat value must not repeat"
        );
        assert_eq!(
            tracker.update(&hat(AbsoluteAxisCode::ABS_HAT0X, 0), &map),
            None,
            "neutral is a release, not an action"
        );
        assert_eq!(
            tracker.update(&hat(AbsoluteAxisCode::ABS_HAT0X, -1), &map),
            Some(GamepadAction::Left)
        );
        assert_eq!(
            tracker.update(&hat(AbsoluteAxisCode::ABS_HAT0X, 1), &map),
            Some(GamepadAction::Right),
            "a direct reversal dispatches the new direction"
        );
        assert_eq!(
            tracker.update(&hat(AbsoluteAxisCode::ABS_HAT0Y, -1), &map),
            Some(GamepadAction::Up)
        );
        assert_eq!(
            tracker.update(&hat(AbsoluteAxisCode::ABS_HAT0Y, 1), &map),
            Some(GamepadAction::Down)
        );
    }

    #[test]
    fn diagonal_hat_axes_emit_both_directions() {
        let map = preset_map(GamepadPreset::Xbox);
        let mut tracker = HatTracker::default();
        assert_eq!(
            tracker.update(&hat(AbsoluteAxisCode::ABS_HAT0X, -1), &map),
            Some(GamepadAction::Left)
        );
        assert_eq!(
            tracker.update(&hat(AbsoluteAxisCode::ABS_HAT0Y, -1), &map),
            Some(GamepadAction::Up)
        );
    }

    #[test]
    fn dpad_key_and_hat_do_not_double_dispatch() {
        let map = preset_map(GamepadPreset::Xbox);

        // Key event first.
        let mut tracker = HatTracker::default();
        assert_eq!(
            tracker.update(&key(KeyCode::BTN_DPAD_UP, 1), &map),
            Some(GamepadAction::Up)
        );
        assert_eq!(
            tracker.update(&hat(AbsoluteAxisCode::ABS_HAT0Y, -1), &map),
            None
        );
        assert_eq!(tracker.update(&key(KeyCode::BTN_DPAD_UP, 0), &map), None);
        assert_eq!(
            tracker.update(&hat(AbsoluteAxisCode::ABS_HAT0Y, 0), &map),
            None
        );

        // Hat event first.
        let mut tracker = HatTracker::default();
        assert_eq!(
            tracker.update(&hat(AbsoluteAxisCode::ABS_HAT0Y, -1), &map),
            Some(GamepadAction::Up)
        );
        assert_eq!(tracker.update(&key(KeyCode::BTN_DPAD_UP, 1), &map), None);
        assert_eq!(tracker.update(&key(KeyCode::BTN_DPAD_UP, 0), &map), None);
        assert_eq!(
            tracker.update(&hat(AbsoluteAxisCode::ABS_HAT0Y, 0), &map),
            None
        );
    }

    #[test]
    fn non_hat_absolute_axes_are_ignored() {
        let map = preset_map(GamepadPreset::Xbox);
        let mut tracker = HatTracker::default();
        assert_eq!(
            tracker.update(&hat(AbsoluteAxisCode::ABS_X, 100), &map),
            None
        );
    }

    #[test]
    fn configured_device_paths_are_trimmed_and_used() {
        let devices = discover_devices(&[" /dev/input/event0 ".into()]);
        assert_eq!(devices, vec![PathBuf::from("/dev/input/event0")]);
    }

    #[test]
    fn hotplug_diff_starts_new_and_stops_removed_devices() {
        let a = PathBuf::from("/dev/input/by-id/a-event-joystick");
        let b = PathBuf::from("/dev/input/by-id/b-event-joystick");
        let c = PathBuf::from("/dev/input/by-id/c-event-joystick");

        let (start, stop) = hotplug_diff(
            &[a.clone(), b.clone()],
            &HashSet::from([b.clone(), c.clone()]),
        );
        assert_eq!(start, vec![a.clone()]);
        assert_eq!(stop, vec![c]);

        let (start, stop) = hotplug_diff(std::slice::from_ref(&a), &HashSet::from([a.clone()]));
        assert!(start.is_empty());
        assert!(stop.is_empty());
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
