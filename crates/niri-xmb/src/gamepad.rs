//! Optional evdev gamepad button bridge.
//!
//! Gamepads are not part of the Wayland seat. Each enabled device gets a
//! reader thread that maps configured key codes to manual-popup actions and
//! forwards them to the daemon loop through a bounded channel. The threads
//! never block the Wayland or Niri event paths. The device is not grabbed in
//! game mode, so other clients keep receiving its input; navi mode grabs it so
//! desktop navigation does not also reach the game.

use crate::config::{
    GamepadAction, GamepadConfig, GamepadMode, GamepadNaviConfig, GamepadPointerAccelConfig,
    GamepadPointerStick, GamepadPreset, GamepadTriggerPoint, NaviAction, PointerButton,
    gamepad_axis_code, gamepad_button_code, parse_navi_action,
};
use crate::wayland::WakeHandle;
use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use evdev::{AbsoluteAxisCode, Device, EventType, InputEvent, KeyCode};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const ACTION_QUEUE: usize = 512;
const POLL_TIMEOUT: Timespec = Timespec {
    tv_sec: 0,
    tv_nsec: 200_000_000,
};
const NAVI_POLL_TIMEOUT: Timespec = Timespec {
    tv_sec: 0,
    tv_nsec: 8_000_000,
};
const NAVI_TICK: Duration = Duration::from_millis(8);
const HOTPLUG_POLL_INTERVAL: Duration = Duration::from_millis(500);
const HOTPLUG_SLEEP_SLICE: Duration = Duration::from_millis(100);
const READER_RETRY_BACKOFF: Duration = Duration::from_secs(2);
const GAMEPAD_EVENT_SUFFIXES: [&str; 2] = ["-event-joystick", "-event-gamepad"];
const BY_ID_DIR: &str = "/dev/input/by-id";

/// High-level input or output change emitted by a controller.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GamepadEvent {
    /// An existing manual-XMB action (including Open).
    Xmb(GamepadAction),
    /// An immediate desktop-navigation action.
    Navi(NaviAction),
    PointerMotion {
        dx: f32,
        dy: f32,
    },
    PointerButton {
        button: u16,
        pressed: bool,
    },
    PointerScroll {
        steps: f32,
    },
    ModeChanged(GamepadMode),
}

#[derive(Debug, Clone, Copy)]
struct AccelRuntime {
    enabled: bool,
    decel_floor: f32,
    boost_cap: f32,
    smoothing_secs: f32,
}

impl AccelRuntime {
    fn from_config(config: &GamepadPointerAccelConfig) -> Self {
        Self {
            enabled: config.enabled,
            decel_floor: config.decel_floor,
            boost_cap: config.boost_cap,
            smoothing_secs: config.smoothing_ms / 1000.0,
        }
    }
}

#[derive(Debug, Clone)]
struct PointerRuntime {
    enabled: bool,
    stick_left: bool,
    deadzone: f32,
    curve: f32,
    speed: f32,
    max_step: f32,
    invert_y: bool,
    scroll_left: bool,
    scroll_deadzone: f32,
    scroll_speed: f32,
    trigger_deadzone: f32,
    trigger_curve: f32,
    trigger_points: Vec<GamepadTriggerPoint>,
    accel: AccelRuntime,
    scroll_accel: AccelRuntime,
}

#[derive(Debug, Clone)]
struct LayerRuntime {
    hold_buttons: HashSet<u16>,
    hold_axes: HashMap<u16, f32>,
    bindings: HashMap<u16, NaviAction>,
}

#[derive(Debug, Clone)]
struct NaviRuntime {
    mode_toggle: HashSet<u16>,
    default_mode: GamepadMode,
    grab_in_navi: bool,
    reset_on_disconnect: bool,
    pointer: PointerRuntime,
    bindings: HashMap<u16, NaviAction>,
    layers: Vec<LayerRuntime>,
}

impl NaviRuntime {
    fn from_config(config: &GamepadConfig) -> Result<Self> {
        let navi: &GamepadNaviConfig = &config.navi;
        let pointer = &navi.pointer;
        Ok(Self {
            mode_toggle: config.mode_toggle_codes()?,
            default_mode: config.mode.default,
            grab_in_navi: config.mode.grab_in_navi,
            reset_on_disconnect: config.mode.reset_on_disconnect,
            pointer: PointerRuntime {
                enabled: pointer.enabled,
                stick_left: pointer.stick == GamepadPointerStick::Left,
                deadzone: pointer.deadzone,
                curve: pointer.curve,
                speed: pointer.speed,
                max_step: pointer.max_step,
                invert_y: pointer.invert_y,
                scroll_left: pointer.scroll_stick == GamepadPointerStick::Left,
                scroll_deadzone: pointer.scroll_deadzone,
                scroll_speed: pointer.scroll_speed,
                trigger_deadzone: pointer.trigger_deadzone,
                trigger_curve: pointer.trigger_curve,
                trigger_points: pointer.trigger_points.clone(),
                accel: AccelRuntime::from_config(&pointer.accel),
                scroll_accel: AccelRuntime::from_config(&pointer.scroll_accel),
            },
            bindings: navi_action_map(&navi.bindings)?,
            layers: navi
                .layers
                .values()
                .map(|layer| {
                    let mut hold_buttons = HashSet::new();
                    for button in &layer.hold {
                        hold_buttons.insert(gamepad_button_code(button)?);
                    }
                    let mut hold_axes = HashMap::new();
                    for axis in &layer.hold_axes {
                        hold_axes.insert(gamepad_axis_code(&axis.axis)?, axis.threshold);
                    }
                    Ok(LayerRuntime {
                        hold_buttons,
                        hold_axes,
                        bindings: navi_action_map(&layer.bindings)?,
                    })
                })
                .collect::<Result<Vec<_>>>()?,
        })
    }
}

fn navi_action_map(
    bindings: &std::collections::BTreeMap<String, String>,
) -> Result<HashMap<u16, NaviAction>> {
    bindings
        .iter()
        .map(|(button, action)| Ok((gamepad_button_code(button)?, parse_navi_action(action)?)))
        .collect()
}

/// A running gamepad hotplug watcher.
///
/// Dropping the service stops the watcher and joins every reader. The receiver
/// is drained by the daemon loop after it is woken.
pub struct GamepadService {
    rx: Receiver<GamepadEvent>,
    stop: Arc<AtomicBool>,
    mode: Arc<AtomicU8>,
    preset: GamepadPreset,
    open_combo: bool,
    devices: Arc<AtomicUsize>,
    supervisor: Option<JoinHandle<()>>,
}

/// Read-only gamepad state exposed by the daemon status response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GamepadStatus {
    pub mode: GamepadMode,
    pub preset: GamepadPreset,
    pub open_combo: bool,
    pub devices: usize,
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
        let navi = match NaviRuntime::from_config(config) {
            Ok(navi) => Arc::new(navi),
            Err(error) => {
                tracing::error!(%error, "gamepad navi config is invalid; gamepad input disabled");
                return None;
            }
        };
        let (tx, rx) = bounded(ACTION_QUEUE);
        let stop = Arc::new(AtomicBool::new(false));
        let mode = Arc::new(AtomicU8::new(mode_to_u8(navi.default_mode)));
        let devices = Arc::new(AtomicUsize::new(0));
        let preset = config.preset;
        let open_combo = !combo_codes.is_empty();
        let config = Arc::new(config.clone());
        let combo_codes = Arc::new(combo_codes);
        let supervisor = {
            let tx = tx.clone();
            let wake = wake.clone();
            let stop = Arc::clone(&stop);
            let config = Arc::clone(&config);
            let combo_codes = Arc::clone(&combo_codes);
            let navi = Arc::clone(&navi);
            let mode = Arc::clone(&mode);
            let devices = Arc::clone(&devices);
            thread::Builder::new()
                .name("niri-xmb-gamepad-hotplug".into())
                .spawn(move || {
                    supervisor_loop(config, combo_codes, navi, mode, devices, tx, wake, stop)
                })
        };
        match supervisor {
            Ok(handle) => Some(Self {
                rx,
                stop,
                mode,
                preset,
                open_combo,
                devices,
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

    /// Current game/navi mode.
    pub fn mode(&self) -> GamepadMode {
        mode_from_u8(self.mode.load(Ordering::Relaxed))
    }

    /// Current mode, configured preset, combo availability, and reader count.
    pub fn status(&self) -> GamepadStatus {
        GamepadStatus {
            mode: self.mode(),
            preset: self.preset,
            open_combo: self.open_combo,
            devices: self.devices.load(Ordering::Relaxed),
        }
    }

    /// Next queued event, if any. Called by the daemon loop after a wake.
    pub fn try_recv(&self) -> Option<GamepadEvent> {
        self.rx.try_recv().ok()
    }
}

fn mode_to_u8(mode: GamepadMode) -> u8 {
    match mode {
        GamepadMode::Game => 0,
        GamepadMode::Navi => 1,
    }
}

fn mode_from_u8(mode: u8) -> GamepadMode {
    if mode == 1 {
        GamepadMode::Navi
    } else {
        GamepadMode::Game
    }
}

fn toggle_mode(mode: &AtomicU8) -> GamepadMode {
    loop {
        let current = mode.load(Ordering::Relaxed);
        let next = if current == mode_to_u8(GamepadMode::Game) {
            GamepadMode::Navi
        } else {
            GamepadMode::Game
        };
        if mode
            .compare_exchange(
                current,
                mode_to_u8(next),
                Ordering::Relaxed,
                Ordering::Relaxed,
            )
            .is_ok()
        {
            return next;
        }
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

#[derive(Debug, Default)]
struct ComboTracker {
    codes: HashSet<u16>,
    down: HashSet<u16>,
    fired: bool,
}

impl ComboTracker {
    fn new(codes: HashSet<u16>) -> Self {
        Self {
            codes,
            ..Self::default()
        }
    }

    /// Returns true on the press that completes the combo.
    fn feed(&mut self, code: u16, value: i32) -> bool {
        if !self.codes.contains(&code) {
            return false;
        }
        match value {
            1 => {
                self.down.insert(code);
                if !self.fired && self.down.len() == self.codes.len() {
                    self.fired = true;
                    true
                } else {
                    false
                }
            }
            0 => {
                self.down.remove(&code);
                self.fired = false;
                false
            }
            _ => false,
        }
    }
}

#[derive(Debug, Clone)]
struct TriggerAccel {
    config: AccelRuntime,
    points: Vec<GamepadTriggerPoint>,
    multiplier: f32,
}

impl TriggerAccel {
    fn new(config: AccelRuntime, points: Vec<GamepadTriggerPoint>) -> Self {
        Self {
            config,
            points,
            multiplier: 1.0,
        }
    }

    fn reset(&mut self) {
        self.multiplier = 1.0;
    }

    fn step(&mut self, lt: f32, rt: f32, deadzone: f32, curve: f32, dt: f32) -> f32 {
        let target = if self.config.enabled {
            let lt = shape_trigger(lt, deadzone, curve, &self.points);
            let rt = shape_trigger(rt, deadzone, curve, &self.points);
            self.config.decel_floor.powf(lt) * self.config.boost_cap.powf(rt)
        } else {
            1.0
        };
        if self.config.smoothing_secs <= 0.0 {
            self.multiplier = target;
        } else {
            let alpha = 1.0 - (-dt / self.config.smoothing_secs).exp();
            self.multiplier += (target - self.multiplier) * alpha;
        }
        self.multiplier
    }
}

fn shape_trigger(value: f32, deadzone: f32, curve: f32, points: &[GamepadTriggerPoint]) -> f32 {
    let value = value.clamp(0.0, 1.0);
    if value <= deadzone {
        return 0.0;
    }
    if points.is_empty() {
        let normalized = ((value - deadzone) / (1.0 - deadzone).max(0.001)).clamp(0.0, 1.0);
        return normalized.powf(curve.max(0.05));
    }
    let mut previous_pull = deadzone;
    let mut previous_output = 0.0_f32;
    for point in points {
        if value <= point.pull {
            let span = (point.pull - previous_pull).max(0.001);
            let t = ((value - previous_pull) / span).clamp(0.0, 1.0);
            return previous_output + (point.output - previous_output) * t;
        }
        previous_pull = point.pull;
        previous_output = point.output;
    }
    let span = (1.0 - previous_pull).max(0.001);
    previous_output + (1.0 - previous_output) * ((value - previous_pull) / span).clamp(0.0, 1.0)
}

struct NaviTracker {
    navi: Arc<NaviRuntime>,
    ranges: HashMap<u16, (i32, i32)>,
    pressed: HashSet<u16>,
    pointer_hold: HashMap<u16, PointerButton>,
    axes: HashMap<u16, f32>,
    hat_x: i32,
    hat_y: i32,
    hat_active: [bool; 4],
    key_down: [bool; 4],
    last_tick: Instant,
    scroll_remainder: f32,
    pointer_accel: TriggerAccel,
    scroll_accel: TriggerAccel,
}

impl NaviTracker {
    fn new(navi: Arc<NaviRuntime>, ranges: HashMap<u16, (i32, i32)>) -> Self {
        let pointer_accel =
            TriggerAccel::new(navi.pointer.accel, navi.pointer.trigger_points.clone());
        let scroll_accel = TriggerAccel::new(
            navi.pointer.scroll_accel,
            navi.pointer.trigger_points.clone(),
        );
        Self {
            navi,
            ranges,
            pressed: HashSet::new(),
            pointer_hold: HashMap::new(),
            axes: HashMap::new(),
            hat_x: 0,
            hat_y: 0,
            hat_active: [false; 4],
            key_down: [false; 4],
            last_tick: Instant::now(),
            scroll_remainder: 0.0,
            pointer_accel,
            scroll_accel,
        }
    }

    fn reset(&mut self) {
        self.pressed.clear();
        self.pointer_hold.clear();
        self.hat_x = 0;
        self.hat_y = 0;
        self.hat_active = [false; 4];
        self.key_down = [false; 4];
        self.axes.clear();
        self.last_tick = Instant::now();
        self.scroll_remainder = 0.0;
        self.pointer_accel.reset();
        self.scroll_accel.reset();
    }

    fn update(&mut self, event: &InputEvent) -> Vec<GamepadEvent> {
        let mut events = Vec::new();
        match event.event_type() {
            EventType::KEY => self.key_event(event.code(), event.value(), &mut events),
            EventType::ABSOLUTE => self.absolute_event(event.code(), event.value(), &mut events),
            _ => {}
        }
        events
    }

    fn key_event(&mut self, code: u16, value: i32, events: &mut Vec<GamepadEvent>) {
        if let Some(index) = dpad_index(code) {
            match value {
                1 if !self.key_down[index] => {
                    self.key_down[index] = true;
                    if !self.hat_active[index]
                        && let Some(event) = self.button_event(code, true)
                    {
                        events.push(event);
                    }
                }
                0 if self.key_down[index] => {
                    self.key_down[index] = false;
                    if !self.hat_active[index]
                        && let Some(event) = self.button_event(code, false)
                    {
                        events.push(event);
                    }
                }
                _ => {}
            }
            return;
        }

        match value {
            1 if self.pressed.insert(code) => {
                if let Some(event) = self.action_event(code) {
                    events.push(event);
                }
            }
            0 if self.pressed.remove(&code) => {
                if let Some(button) = self.pointer_hold.remove(&code) {
                    events.push(GamepadEvent::PointerButton {
                        button: button.code(),
                        pressed: false,
                    });
                }
            }
            _ => {}
        }
    }

    fn absolute_event(&mut self, code: u16, value: i32, events: &mut Vec<GamepadEvent>) {
        if code == AbsoluteAxisCode::ABS_HAT0X.0 {
            self.hat_event(HatAxis::X, value, events);
        } else if code == AbsoluteAxisCode::ABS_HAT0Y.0 {
            self.hat_event(HatAxis::Y, value, events);
        } else if code == AbsoluteAxisCode::ABS_Z.0 || code == AbsoluteAxisCode::ABS_RZ.0 {
            let (min, max) = self.ranges.get(&code).copied().unwrap_or((0, 255));
            self.axes.insert(code, normalize_unsigned(value, min, max));
        } else if code == AbsoluteAxisCode::ABS_X.0
            || code == AbsoluteAxisCode::ABS_Y.0
            || code == AbsoluteAxisCode::ABS_RX.0
            || code == AbsoluteAxisCode::ABS_RY.0
        {
            let (min, max) = self.ranges.get(&code).copied().unwrap_or((-32768, 32767));
            self.axes.insert(code, normalize_signed(value, min, max));
        }
    }

    fn hat_event(&mut self, axis: HatAxis, value: i32, events: &mut Vec<GamepadEvent>) {
        let old = match axis {
            HatAxis::X => self.hat_x,
            HatAxis::Y => self.hat_y,
        };
        let new = value.signum();
        if new == old {
            return;
        }
        if let Some(index) = hat_direction(axis, old)
            && self.hat_active[index]
        {
            self.hat_active[index] = false;
            // If the corresponding key is still held, it takes ownership and
            // its release will emit the button-up.
            if !self.key_down[index]
                && let Some(event) = self.button_event(dpad_code(index), false)
            {
                events.push(event);
            }
        }
        match axis {
            HatAxis::X => self.hat_x = new,
            HatAxis::Y => self.hat_y = new,
        }
        let Some(index) = hat_direction(axis, new) else {
            return;
        };
        self.hat_active[index] = true;
        if !self.key_down[index]
            && let Some(event) = self.button_event(dpad_code(index), true)
        {
            events.push(event);
        }
    }

    fn button_event(&mut self, code: u16, pressed: bool) -> Option<GamepadEvent> {
        if pressed {
            if !self.pressed.insert(code) {
                return None;
            }
            let action = self.action_for(code)?;
            match action {
                NaviAction::Pointer(button) => {
                    self.pointer_hold.insert(code, button);
                    Some(GamepadEvent::PointerButton {
                        button: button.code(),
                        pressed: true,
                    })
                }
                NaviAction::Scroll(direction) => Some(GamepadEvent::PointerScroll {
                    steps: direction as f32,
                }),
                NaviAction::Niri(action) => Some(GamepadEvent::Navi(NaviAction::Niri(action))),
                NaviAction::XmbOpen => Some(GamepadEvent::Navi(NaviAction::XmbOpen)),
            }
        } else {
            self.pressed.remove(&code);
            self.pointer_hold
                .remove(&code)
                .map(|button| GamepadEvent::PointerButton {
                    button: button.code(),
                    pressed: false,
                })
        }
    }

    fn action_event(&mut self, code: u16) -> Option<GamepadEvent> {
        let action = self.action_for(code)?;
        match action {
            NaviAction::Pointer(button) => {
                self.pointer_hold.insert(code, button);
                Some(GamepadEvent::PointerButton {
                    button: button.code(),
                    pressed: true,
                })
            }
            NaviAction::Scroll(direction) => Some(GamepadEvent::PointerScroll {
                steps: direction as f32,
            }),
            NaviAction::Niri(action) => Some(GamepadEvent::Navi(NaviAction::Niri(action))),
            NaviAction::XmbOpen => Some(GamepadEvent::Navi(NaviAction::XmbOpen)),
        }
    }

    fn action_for(&self, code: u16) -> Option<NaviAction> {
        self.active_layer()
            .and_then(|layer| layer.bindings.get(&code).copied())
            .or_else(|| self.navi.bindings.get(&code).copied())
    }

    fn active_layer(&self) -> Option<&LayerRuntime> {
        self.navi.layers.iter().find(|layer| {
            layer
                .hold_buttons
                .iter()
                .all(|code| self.pressed.contains(code))
                && layer.hold_axes.iter().all(|(code, threshold)| {
                    self.axes.get(code).is_some_and(|value| value >= threshold)
                })
        })
    }

    fn tick(&mut self) -> Vec<GamepadEvent> {
        let now = Instant::now();
        let dt = now
            .saturating_duration_since(self.last_tick)
            .as_secs_f32()
            .min(0.05);
        if dt < NAVI_TICK.as_secs_f32() * 0.5 {
            return Vec::new();
        }
        self.last_tick = now;
        if !self.navi.pointer.enabled {
            return Vec::new();
        }

        let mut events = Vec::with_capacity(2);
        let lt = self
            .axes
            .get(&AbsoluteAxisCode::ABS_Z.0)
            .copied()
            .unwrap_or(0.0);
        let rt = self
            .axes
            .get(&AbsoluteAxisCode::ABS_RZ.0)
            .copied()
            .unwrap_or(0.0);
        let trigger_deadzone = self.navi.pointer.trigger_deadzone;
        let trigger_curve = self.navi.pointer.trigger_curve;
        let pointer_multiplier =
            self.pointer_accel
                .step(lt, rt, trigger_deadzone, trigger_curve, dt);

        let (stick_x, stick_y) = if self.navi.pointer.stick_left {
            self.stick(AbsoluteAxisCode::ABS_X, AbsoluteAxisCode::ABS_Y)
        } else {
            self.stick(AbsoluteAxisCode::ABS_RX, AbsoluteAxisCode::ABS_RY)
        };
        let (x, y) = apply_radial_deadzone(stick_x, stick_y, self.navi.pointer.deadzone);
        let x = shape(x, self.navi.pointer.curve);
        let mut y = shape(y, self.navi.pointer.curve);
        if self.navi.pointer.invert_y {
            y = -y;
        }
        let dx = clamp_step(
            x * self.navi.pointer.speed * pointer_multiplier * dt,
            self.navi.pointer.max_step,
        );
        let dy = clamp_step(
            y * self.navi.pointer.speed * pointer_multiplier * dt,
            self.navi.pointer.max_step,
        );
        if dx.abs() > 0.01 || dy.abs() > 0.01 {
            events.push(GamepadEvent::PointerMotion { dx, dy });
        }

        let (_scroll_x, scroll_y) = if self.navi.pointer.scroll_left {
            self.stick(AbsoluteAxisCode::ABS_X, AbsoluteAxisCode::ABS_Y)
        } else {
            self.stick(AbsoluteAxisCode::ABS_RX, AbsoluteAxisCode::ABS_RY)
        };
        let scroll = apply_deadzone(scroll_y, self.navi.pointer.scroll_deadzone);
        let scroll_multiplier = self
            .scroll_accel
            .step(lt, rt, trigger_deadzone, trigger_curve, dt);
        self.scroll_remainder += scroll * self.navi.pointer.scroll_speed * scroll_multiplier * dt;
        if self.scroll_remainder.abs() >= 0.01 {
            events.push(GamepadEvent::PointerScroll {
                steps: self.scroll_remainder,
            });
            self.scroll_remainder = 0.0;
        }
        events
    }

    fn stick(&self, axis_x: AbsoluteAxisCode, axis_y: AbsoluteAxisCode) -> (f32, f32) {
        (
            self.axes.get(&axis_x.0).copied().unwrap_or(0.0),
            self.axes.get(&axis_y.0).copied().unwrap_or(0.0),
        )
    }
}

fn device_axis_ranges(device: &Device) -> HashMap<u16, (i32, i32)> {
    device
        .get_absinfo()
        .ok()
        .into_iter()
        .flatten()
        .map(|(code, info)| (code.0, (info.minimum(), info.maximum())))
        .collect()
}

fn normalize_signed(value: i32, min: i32, max: i32) -> f32 {
    let center = (min as f32 + max as f32) * 0.5;
    let half = (max as f32 - min as f32) * 0.5;
    if half <= 0.0 {
        0.0
    } else {
        ((value as f32 - center) / half).clamp(-1.0, 1.0)
    }
}

fn normalize_unsigned(value: i32, min: i32, max: i32) -> f32 {
    if max <= min {
        0.0
    } else {
        ((value as f32 - min as f32) / (max as f32 - min as f32)).clamp(0.0, 1.0)
    }
}

fn apply_radial_deadzone(x: f32, y: f32, deadzone: f32) -> (f32, f32) {
    let magnitude = (x * x + y * y).sqrt();
    if magnitude <= deadzone || magnitude == 0.0 {
        return (0.0, 0.0);
    }
    let scale = ((magnitude - deadzone) / (1.0 - deadzone).max(0.001)).min(1.0);
    (x / magnitude * scale, y / magnitude * scale)
}

fn apply_deadzone(value: f32, deadzone: f32) -> f32 {
    let magnitude = value.abs();
    if magnitude <= deadzone {
        0.0
    } else {
        value.signum() * ((magnitude - deadzone) / (1.0 - deadzone).max(0.001))
    }
}

fn shape(value: f32, curve: f32) -> f32 {
    value.signum() * value.abs().powf(curve.max(0.05))
}

fn clamp_step(value: f32, max_step: f32) -> f32 {
    if max_step <= 0.0 {
        0.0
    } else {
        value.clamp(-max_step, max_step)
    }
}

fn apply_mode_grab(device: &mut Device, mode: GamepadMode, grab_in_navi: bool, path: &Path) {
    match (mode, grab_in_navi) {
        (GamepadMode::Navi, true) => {
            if let Err(error) = device.grab() {
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "cannot grab gamepad for navi mode"
                );
            }
        }
        (GamepadMode::Game, _) => {
            if let Err(error) = device.ungrab() {
                tracing::debug!(
                    path = %path.display(),
                    %error,
                    "cannot ungrab gamepad after navi mode"
                );
            }
        }
        (GamepadMode::Navi, false) => {}
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

#[allow(clippy::too_many_arguments)]
fn supervisor_loop(
    config: Arc<GamepadConfig>,
    combo_codes: Arc<HashSet<u16>>,
    navi: Arc<NaviRuntime>,
    mode: Arc<AtomicU8>,
    devices: Arc<AtomicUsize>,
    tx: Sender<GamepadEvent>,
    wake: WakeHandle,
    stop: Arc<AtomicBool>,
) {
    let mut active: HashMap<PathBuf, ActiveReader> = HashMap::new();
    let mut retry_at: HashMap<PathBuf, Instant> = HashMap::new();
    let mut failures: HashSet<PathBuf> = HashSet::new();
    let mut skipped: HashSet<PathBuf> = HashSet::new();
    let mut had_active = false;
    tracing::info!(
        combo = combo_codes.len(),
        mode = ?navi.default_mode,
        "gamepad hotplug watcher started"
    );

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
            match start_reader(&path, &config, &combo_codes, &navi, &mode, &tx, &wake) {
                Ok(Some(reader)) => {
                    active.insert(path.clone(), reader);
                    had_active = true;
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

        devices.store(active.len(), Ordering::Relaxed);

        if had_active
            && active.is_empty()
            && desired.is_empty()
            && navi.reset_on_disconnect
            && mode_from_u8(mode.load(Ordering::Relaxed)) != GamepadMode::Game
        {
            mode.store(mode_to_u8(GamepadMode::Game), Ordering::Relaxed);
            let _ = tx.try_send(GamepadEvent::ModeChanged(GamepadMode::Game));
            had_active = false;
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
    devices.store(0, Ordering::Relaxed);
    tracing::info!("gamepad hotplug watcher stopped");
}

fn start_reader(
    path: &Path,
    config: &GamepadConfig,
    combo_codes: &HashSet<u16>,
    navi: &NaviRuntime,
    mode: &Arc<AtomicU8>,
    tx: &Sender<GamepadEvent>,
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
    let navi_has_input =
        navi.pointer.enabled || !navi.bindings.is_empty() || !navi.layers.is_empty();
    if !has_gamepad_input(&bindings, combo_codes) && !navi_has_input {
        tracing::warn!(
            path = %path.display(),
            preset = ?preset,
            "gamepad has no button bindings, open combo, or navi input; skipping"
        );
        return Ok(None);
    }

    let name = device.name().unwrap_or("unknown").to_string();
    let binding_count = bindings.len();
    let bindings = Arc::new(bindings);
    let combo_codes = combo_codes.clone();
    let navi = Arc::new(navi.clone());
    let mode = Arc::clone(mode);
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
                navi,
                mode,
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

fn queue_gamepad_event(
    tx: &Sender<GamepadEvent>,
    event: GamepadEvent,
    full_logged: &mut bool,
) -> Result<bool, ()> {
    match tx.try_send(event) {
        Ok(()) => Ok(true),
        Err(TrySendError::Full(_)) => {
            if !*full_logged {
                *full_logged = true;
                tracing::warn!("gamepad event queue full; dropping input");
            }
            Ok(false)
        }
        Err(TrySendError::Disconnected(_)) => Err(()),
    }
}

#[allow(clippy::too_many_arguments)]
fn reader_loop(
    path: PathBuf,
    mut device: Device,
    bindings: Arc<HashMap<u16, GamepadAction>>,
    combo_codes: HashSet<u16>,
    navi: Arc<NaviRuntime>,
    mode: Arc<AtomicU8>,
    tx: Sender<GamepadEvent>,
    wake: WakeHandle,
    stop: Arc<AtomicBool>,
) {
    let ranges = device_axis_ranges(&device);
    let mut xmb_tracker = HatTracker::new(combo_codes);
    let mut navi_tracker = NaviTracker::new(Arc::clone(&navi), ranges);
    let mut mode_combo = ComboTracker::new(navi.mode_toggle.clone());
    let mut last_mode = mode_from_u8(mode.load(Ordering::Relaxed));
    apply_mode_grab(&mut device, last_mode, navi.grab_in_navi, &path);
    let mut full_logged = false;

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }

        let observed_mode = mode_from_u8(mode.load(Ordering::Relaxed));
        if observed_mode != last_mode {
            apply_mode_grab(&mut device, observed_mode, navi.grab_in_navi, &path);
            navi_tracker.reset();
            last_mode = observed_mode;
        }

        let timeout = if last_mode == GamepadMode::Navi {
            &NAVI_POLL_TIMEOUT
        } else {
            &POLL_TIMEOUT
        };
        let mut fds = [PollFd::new(&device, PollFlags::IN)];
        match poll(&mut fds, Some(timeout)) {
            Ok(0) => {
                if last_mode == GamepadMode::Navi {
                    let mut sent = false;
                    for event in navi_tracker.tick() {
                        let Ok(queued) = queue_gamepad_event(&tx, event, &mut full_logged) else {
                            return;
                        };
                        sent |= queued;
                    }
                    if sent {
                        wake.wake();
                    }
                }
                continue;
            }
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

        let events = match device.fetch_events() {
            Ok(events) => events.collect::<Vec<_>>(),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Vec::new(),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => Vec::new(),
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "gamepad read failed");
                break;
            }
        };

        let mut sent = false;
        for event in events {
            if event.event_type() == EventType::KEY && mode_combo.feed(event.code(), event.value())
            {
                let next = toggle_mode(&mode);
                apply_mode_grab(&mut device, next, navi.grab_in_navi, &path);
                navi_tracker.reset();
                last_mode = next;
                let Ok(queued) =
                    queue_gamepad_event(&tx, GamepadEvent::ModeChanged(next), &mut full_logged)
                else {
                    return;
                };
                sent |= queued;
                continue;
            }

            if let Some(action) = xmb_tracker.update(&event, &bindings)
                && last_mode == GamepadMode::Navi
            {
                let Ok(queued) =
                    queue_gamepad_event(&tx, GamepadEvent::Xmb(action), &mut full_logged)
                else {
                    return;
                };
                sent |= queued;
            }

            if last_mode == GamepadMode::Navi {
                for navi_event in navi_tracker.update(&event) {
                    let Ok(queued) = queue_gamepad_event(&tx, navi_event, &mut full_logged) else {
                        return;
                    };
                    sent |= queued;
                }
            }
        }

        if last_mode == GamepadMode::Navi {
            for event in navi_tracker.tick() {
                let Ok(queued) = queue_gamepad_event(&tx, event, &mut full_logged) else {
                    return;
                };
                sent |= queued;
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
            ..GamepadConfig::default()
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

    fn navi_tracker() -> NaviTracker {
        let config = GamepadConfig::default();
        NaviTracker::new(
            Arc::new(NaviRuntime::from_config(&config).unwrap()),
            HashMap::new(),
        )
    }

    fn abs(axis: AbsoluteAxisCode, value: i32) -> InputEvent {
        InputEvent::new(EventType::ABSOLUTE.0, axis.0, value)
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
    fn xbox_and_ds4_presets_are_intentionally_positionally_identical() {
        // Linux exposes both families through the same positional key codes.
        // Keep separate constants for future divergence; this catches an
        // accidental one-sided edit without treating the duplication as drift.
        assert_eq!(XBOX_PRESET, DS4_PRESET);
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
            ..GamepadConfig::default()
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
    fn mode_combo_toggles_once_until_release() {
        let mut combo =
            ComboTracker::new(HashSet::from([KeyCode::BTN_MODE.0, KeyCode::BTN_START.0]));
        assert!(!combo.feed(KeyCode::BTN_MODE.0, 1));
        assert!(combo.feed(KeyCode::BTN_START.0, 1));
        assert!(!combo.feed(KeyCode::BTN_START.0, 2));
        assert!(!combo.feed(KeyCode::BTN_START.0, 0));
        assert!(combo.feed(KeyCode::BTN_START.0, 1));
    }

    #[test]
    fn navi_mode_atomic_round_trips() {
        let mode = AtomicU8::new(mode_to_u8(GamepadMode::Game));
        assert_eq!(toggle_mode(&mode), GamepadMode::Navi);
        assert_eq!(
            mode_from_u8(mode.load(Ordering::Relaxed)),
            GamepadMode::Navi
        );
        assert_eq!(toggle_mode(&mode), GamepadMode::Game);
        assert_eq!(
            mode_from_u8(mode.load(Ordering::Relaxed)),
            GamepadMode::Game
        );
    }

    #[test]
    fn navi_base_maps_clicks_and_xmb_open() {
        let mut tracker = navi_tracker();
        assert_eq!(
            tracker.update(&key(KeyCode::BTN_SOUTH, 1)),
            vec![GamepadEvent::PointerButton {
                button: PointerButton::Left.code(),
                pressed: true,
            }]
        );
        assert_eq!(
            tracker.update(&key(KeyCode::BTN_SOUTH, 0)),
            vec![GamepadEvent::PointerButton {
                button: PointerButton::Left.code(),
                pressed: false,
            }]
        );
        assert_eq!(
            tracker.update(&key(KeyCode::BTN_NORTH, 1)),
            vec![GamepadEvent::Navi(NaviAction::XmbOpen)]
        );
    }

    #[test]
    fn navi_layers_resolve_dpad_and_trigger_holds() {
        let mut tracker = navi_tracker();
        assert!(tracker.update(&key(KeyCode::BTN_TL, 1)).is_empty());
        assert_eq!(
            tracker.update(&key(KeyCode::BTN_DPAD_LEFT, 1)),
            vec![GamepadEvent::Navi(NaviAction::Niri(
                crate::config::NaviNiriAction::FocusWorkspacePrev
            ))]
        );
        tracker.update(&key(KeyCode::BTN_DPAD_LEFT, 0));
        tracker.update(&key(KeyCode::BTN_TL, 0));

        tracker.update(&abs(AbsoluteAxisCode::ABS_Z, 255));
        tracker.update(&abs(AbsoluteAxisCode::ABS_RZ, 255));
        assert_eq!(
            tracker.update(&key(KeyCode::BTN_DPAD_RIGHT, 1)),
            vec![GamepadEvent::Navi(NaviAction::Niri(
                crate::config::NaviNiriAction::FocusMonitorNext
            ))]
        );
    }

    #[test]
    fn navi_hat_dpad_uses_the_same_layers() {
        let mut tracker = navi_tracker();
        tracker.update(&key(KeyCode::BTN_TL, 1));
        assert_eq!(
            tracker.update(&abs(AbsoluteAxisCode::ABS_HAT0X, -1)),
            vec![GamepadEvent::Navi(NaviAction::Niri(
                crate::config::NaviNiriAction::FocusWorkspacePrev
            ))]
        );
        assert!(
            tracker
                .update(&abs(AbsoluteAxisCode::ABS_HAT0X, 0))
                .is_empty()
        );
    }

    #[test]
    fn navi_pointer_deadzone_and_motion() {
        let mut tracker = navi_tracker();
        tracker.update(&abs(AbsoluteAxisCode::ABS_X, 0));
        tracker.last_tick = Instant::now() - Duration::from_millis(20);
        assert!(tracker.tick().is_empty());

        tracker.update(&abs(AbsoluteAxisCode::ABS_X, 32767));
        tracker.last_tick = Instant::now() - Duration::from_millis(20);
        let events = tracker.tick();
        assert!(matches!(
            events.as_slice(),
            [GamepadEvent::PointerMotion { dx, .. }] if *dx > 0.0
        ));
    }

    #[test]
    fn trigger_accel_multiplier_targets_and_deadzone() {
        let mut accel = TriggerAccel::new(
            AccelRuntime {
                enabled: true,
                decel_floor: 0.30,
                boost_cap: 2.50,
                smoothing_secs: 0.0,
            },
            Vec::new(),
        );
        assert!((accel.step(0.0, 0.0, 0.08, 1.0, 0.008) - 1.0).abs() < 1e-5);
        assert!((accel.step(1.0, 0.0, 0.08, 1.0, 0.008) - 0.30).abs() < 1e-5);
        assert!((accel.step(0.0, 1.0, 0.08, 1.0, 0.008) - 2.50).abs() < 1e-5);
        assert!((accel.step(1.0, 1.0, 0.08, 1.0, 0.008) - 0.75).abs() < 1e-5);
        assert!((accel.step(0.08, 0.0, 0.08, 1.0, 0.008) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn trigger_points_interpolate_and_fall_back() {
        let points = [
            GamepadTriggerPoint {
                pull: 0.25,
                output: 0.10,
            },
            GamepadTriggerPoint {
                pull: 0.50,
                output: 0.30,
            },
            GamepadTriggerPoint {
                pull: 0.75,
                output: 0.70,
            },
        ];
        assert!((shape_trigger(0.08, 0.08, 1.0, &points) - 0.0).abs() < 1e-6);
        assert!((shape_trigger(0.25, 0.08, 1.0, &points) - 0.10).abs() < 1e-6);
        assert!((shape_trigger(0.50, 0.08, 1.0, &points) - 0.30).abs() < 1e-6);
        assert!((shape_trigger(0.75, 0.08, 1.0, &points) - 0.70).abs() < 1e-6);
        assert!((shape_trigger(1.0, 0.08, 1.0, &points) - 1.0).abs() < 1e-6);
        assert!((shape_trigger(0.50, 0.08, 3.0, &[]) - (0.42_f32 / 0.92).powf(3.0)).abs() < 1e-6);
    }

    #[test]
    fn trigger_accel_smoothing_converges() {
        let mut accel = TriggerAccel::new(
            AccelRuntime {
                enabled: true,
                decel_floor: 0.30,
                boost_cap: 2.50,
                smoothing_secs: 0.09,
            },
            Vec::new(),
        );
        let first = accel.step(0.0, 1.0, 0.08, 1.0, 0.09);
        assert!(first > 1.0 && first < 2.5);
        for _ in 0..20 {
            accel.step(0.0, 1.0, 0.08, 1.0, 0.09);
        }
        assert!((accel.step(0.0, 1.0, 0.08, 1.0, 0.09) - 2.5).abs() < 0.01);
    }

    #[test]
    fn navi_pointer_tick_uses_trigger_accel() {
        let mut tracker = navi_tracker();
        tracker.pointer_accel = TriggerAccel::new(
            AccelRuntime {
                enabled: true,
                decel_floor: 0.30,
                boost_cap: 2.50,
                smoothing_secs: 0.0,
            },
            Vec::new(),
        );
        tracker.update(&abs(AbsoluteAxisCode::ABS_X, 32767));

        tracker.last_tick = Instant::now() - Duration::from_millis(8);
        let base = tracker.tick();
        let base_dx = match base.as_slice() {
            [GamepadEvent::PointerMotion { dx, .. }] => *dx,
            _ => panic!("expected pointer motion: {base:?}"),
        };

        tracker.update(&abs(AbsoluteAxisCode::ABS_RZ, 255));
        tracker.last_tick = Instant::now() - Duration::from_millis(8);
        let boosted = tracker.tick();
        let boosted_dx = match boosted.as_slice() {
            [GamepadEvent::PointerMotion { dx, .. }] => *dx,
            _ => panic!("expected boosted pointer motion: {boosted:?}"),
        };
        assert!(boosted_dx > base_dx);

        tracker.update(&abs(AbsoluteAxisCode::ABS_RZ, 0));
        tracker.update(&abs(AbsoluteAxisCode::ABS_Z, 255));
        tracker.last_tick = Instant::now() - Duration::from_millis(8);
        let slowed = tracker.tick();
        let slowed_dx = match slowed.as_slice() {
            [GamepadEvent::PointerMotion { dx, .. }] => *dx,
            _ => panic!("expected slowed pointer motion: {slowed:?}"),
        };
        assert!(slowed_dx < base_dx);
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
