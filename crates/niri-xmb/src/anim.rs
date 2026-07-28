//! Interpolated UI state for the popup.
//!
//! Every animated quantity is a `Track` easing toward a target that is
//! recomputed from authoritative state once per main-loop iteration. Targets
//! are never derived from what was drawn last frame, so there is no feedback
//! between rendering and animation.

use crate::config::AnimationConfig;
use std::collections::HashMap;
use std::time::Instant;

/// Below this distance a track is considered settled and snaps to its target.
const REST_EPSILON: f32 = 0.002;
/// Clamp for the frame delta so a stalled loop cannot produce a jump.
const MAX_DELTA: f32 = 0.100;

/// Time constants driving each family of tracks, in seconds.
///
/// An exponential approach covers roughly 95% of its distance in three time
/// constants, so a configured duration maps to `duration / 3`. A constant of
/// zero means "apply immediately".
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    panel: f32,
    emphasis: f32,
    scroll: f32,
}

impl Timing {
    pub fn from_config(config: &AnimationConfig) -> Self {
        if !config.enabled {
            return Self {
                panel: 0.0,
                emphasis: 0.0,
                scroll: 0.0,
            };
        }
        let tau = |ms: f32| (ms.max(0.0) / 3000.0).max(0.0);
        Self {
            panel: tau(config.panel_ms),
            emphasis: tau(config.emphasis_ms),
            scroll: tau(config.scroll_ms),
        }
    }

    /// Timing used by tests and by callers that just want the defaults.
    #[cfg(test)]
    fn default_timing() -> Self {
        Self::from_config(&AnimationConfig::default())
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Track {
    current: f32,
    target: f32,
}

impl Track {
    /// Advance toward the target, covering a fixed fraction of the remaining
    /// distance per unit time. Returns whether the track is still moving.
    fn advance(&mut self, dt: f32, tau: f32) -> bool {
        let delta = self.target - self.current;
        if delta.abs() < REST_EPSILON {
            self.current = self.target;
            return false;
        }
        if tau <= 0.0 {
            // Animation disabled: land on the target in one step.
            self.current = self.target;
            return false;
        }
        self.current += delta * (1.0 - (-dt / tau).exp());
        true
    }

    fn settled_off(&self) -> bool {
        self.target <= 0.0 && self.current < REST_EPSILON
    }
}

/// Animated popup state, keyed by output name, window id, and workspace id.
#[derive(Default)]
pub struct Animator {
    panels: HashMap<String, Track>,
    windows: HashMap<u64, Track>,
    workspaces: HashMap<u64, Track>,
    /// Vertical scroll per output: which workspace sits on the focal row.
    columns: HashMap<String, Track>,
    /// Horizontal scroll per workspace: which window sits on the focal column.
    ///
    /// Per workspace rather than per output on purpose. With one shared offset,
    /// a row that was already drawn in the right place would jump to the
    /// previously selected row's offset the moment it became selected, then
    /// slide back — visible as a spurious sideways slide on workspace change.
    rows: HashMap<u64, Track>,
    /// Vertical offset inside a workspace's selected Niri column.
    stacks: HashMap<u64, Track>,
    last_tick: Option<Instant>,
}

impl Animator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Clear every target ahead of a sync pass. Callers then re-assert the
    /// targets that are still live; anything not re-asserted eases to zero and
    /// is eventually dropped.
    pub fn begin_sync(&mut self) {
        for track in self.panels.values_mut() {
            track.target = 0.0;
        }
        for track in self.windows.values_mut() {
            track.target = 0.0;
        }
        for track in self.workspaces.values_mut() {
            track.target = 0.0;
        }
    }

    pub fn set_panel_open(&mut self, output: &str, open: bool) {
        let target = if open { 1.0 } else { 0.0 };
        if let Some(track) = self.panels.get_mut(output) {
            track.target = target;
        } else {
            self.panels.insert(
                output.to_string(),
                Track {
                    current: 0.0,
                    target,
                },
            );
        }
    }

    pub fn set_window_emphasis(&mut self, window: u64, on: bool) {
        self.windows.entry(window).or_default().target = if on { 1.0 } else { 0.0 };
    }

    pub fn set_workspace_emphasis(&mut self, workspace: u64, on: bool) {
        self.workspaces.entry(workspace).or_default().target = if on { 1.0 } else { 0.0 };
    }

    /// Set an output's vertical scroll. Unlike the emphasis tracks these are
    /// absolute pixel offsets, so `begin_sync` leaves them alone and the caller
    /// re-states them every pass.
    pub fn set_column_scroll(&mut self, output: &str, y: f32) {
        if let Some(track) = self.columns.get_mut(output) {
            track.target = y;
            return;
        }
        // Appearing for the first time starts already in position: opening the
        // popup should not look like a scroll.
        self.columns.insert(
            output.to_string(),
            Track {
                current: y,
                target: y,
            },
        );
    }

    pub fn column_scroll(&self, output: &str) -> f32 {
        self.columns.get(output).map_or(0.0, |track| track.current)
    }

    /// Set a workspace row's horizontal scroll.
    pub fn set_row_scroll(&mut self, workspace: u64, x: f32) {
        if let Some(track) = self.rows.get_mut(&workspace) {
            track.target = x;
            return;
        }
        self.rows.insert(
            workspace,
            Track {
                current: x,
                target: x,
            },
        );
    }

    pub fn row_scroll(&self, workspace: u64) -> f32 {
        self.rows.get(&workspace).map_or(0.0, |track| track.current)
    }

    /// Set which window in a vertically stacked Niri column sits on the focal
    /// point for this workspace.
    pub fn set_stack_scroll(&mut self, workspace: u64, y: f32) {
        if let Some(track) = self.stacks.get_mut(&workspace) {
            track.target = y;
            return;
        }
        self.stacks.insert(
            workspace,
            Track {
                current: y,
                target: y,
            },
        );
    }

    pub fn stack_scroll(&self, workspace: u64) -> f32 {
        self.stacks
            .get(&workspace)
            .map_or(0.0, |track| track.current)
    }

    /// Drop scroll state for outputs and workspaces that no longer exist.
    pub fn retain_scrolls(
        &mut self,
        output_live: impl Fn(&str) -> bool,
        workspace_live: impl Fn(u64) -> bool,
    ) {
        self.columns.retain(|output, _| output_live(output));
        self.rows.retain(|workspace, _| workspace_live(*workspace));
        self.stacks
            .retain(|workspace, _| workspace_live(*workspace));
    }

    /// Advance every track and drop the ones that have faded out. Returns true
    /// while anything is still in motion, which the caller uses to keep
    /// requesting redraws.
    pub fn tick(&mut self, timing: Timing) -> bool {
        let now = Instant::now();
        let dt = self
            .last_tick
            .map(|last| now.duration_since(last).as_secs_f32())
            .unwrap_or(0.0)
            .min(MAX_DELTA);
        self.last_tick = Some(now);
        self.advance(dt, timing)
    }

    /// Advance every track by `dt` seconds. Split out from [`Animator::tick`]
    /// so behaviour can be exercised without depending on the wall clock.
    fn advance(&mut self, dt: f32, timing: Timing) -> bool {
        if dt <= 0.0 {
            return self.in_motion();
        }

        let mut moving = false;
        for track in self.panels.values_mut() {
            moving |= track.advance(dt, timing.panel);
        }
        for track in self.windows.values_mut() {
            moving |= track.advance(dt, timing.emphasis);
        }
        for track in self.workspaces.values_mut() {
            moving |= track.advance(dt, timing.emphasis);
        }
        for track in self.columns.values_mut() {
            moving |= track.advance(dt, timing.scroll);
        }
        for track in self.rows.values_mut() {
            moving |= track.advance(dt, timing.scroll);
        }
        for track in self.stacks.values_mut() {
            moving |= track.advance(dt, timing.scroll);
        }
        self.panels.retain(|_, track| !track.settled_off());
        self.windows.retain(|_, track| !track.settled_off());
        self.workspaces.retain(|_, track| !track.settled_off());
        moving
    }

    fn in_motion(&self) -> bool {
        let moving = |track: &Track| (track.target - track.current).abs() >= REST_EPSILON;
        self.panels.values().any(moving)
            || self.windows.values().any(moving)
            || self.workspaces.values().any(moving)
            || self.columns.values().any(moving)
            || self.rows.values().any(moving)
            || self.stacks.values().any(moving)
    }

    /// Panel presence for an output, 0.0 (hidden) to 1.0 (fully open).
    pub fn panel(&self, output: &str) -> f32 {
        self.panels.get(output).map_or(0.0, |track| track.current)
    }

    pub fn window(&self, window: u64) -> f32 {
        self.windows.get(&window).map_or(0.0, |track| track.current)
    }

    pub fn workspace(&self, workspace: u64) -> f32 {
        self.workspaces
            .get(&workspace)
            .map_or(0.0, |track| track.current)
    }
}

/// Linear interpolation used to drive sizes and opacities from a track value.
pub fn lerp(from: f32, to: f32, t: f32) -> f32 {
    from + (to - from) * t.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_settles_at_its_target() {
        let tau = Timing::default_timing().panel;
        let mut track = Track {
            current: 0.0,
            target: 1.0,
        };
        for _ in 0..200 {
            track.advance(0.008, tau);
        }
        assert!(!track.advance(0.008, tau), "should come to rest");
        assert_eq!(track.current, 1.0);
    }

    /// Run a fixed 8ms cadence, matching the main loop, for `frames` frames.
    fn run(animator: &mut Animator, frames: usize) -> bool {
        let mut moving = false;
        for _ in 0..frames {
            moving = animator.advance(0.008, Timing::default_timing());
        }
        moving
    }

    #[test]
    fn emphasis_rises_then_is_dropped_once_it_fades_out() {
        let mut animator = Animator::new();
        animator.set_window_emphasis(7, true);
        run(&mut animator, 100);
        assert!(animator.window(7) > 0.9, "should have eased in");

        // A sync pass that no longer emphasises the window releases it.
        animator.begin_sync();
        assert!(!run(&mut animator, 100), "should come to rest");
        assert_eq!(animator.window(7), 0.0);
        assert!(animator.windows.is_empty(), "settled tracks must be pruned");
    }

    #[test]
    fn a_settled_animator_stops_requesting_frames() {
        let mut animator = Animator::new();
        animator.set_panel_open("DP-1", true);
        animator.set_column_scroll("DP-1", 80.0);
        animator.set_row_scroll(1, 40.0);
        animator.set_stack_scroll(1, 24.0);
        run(&mut animator, 100);
        assert!(
            !animator.advance(0.008, Timing::default_timing()),
            "no redraw once everything settled"
        );
        assert_eq!(animator.panel("DP-1"), 1.0);
    }

    #[test]
    fn a_new_canvas_starts_already_scrolled_into_place() {
        let mut animator = Animator::new();
        animator.set_column_scroll("DP-1", 80.0);
        animator.set_row_scroll(1, 40.0);
        animator.set_stack_scroll(1, 24.0);
        // Opening the popup must not look like the content scrolled in.
        assert_eq!(
            (animator.row_scroll(1), animator.column_scroll("DP-1")),
            (40.0, 80.0)
        );
        assert_eq!(animator.stack_scroll(1), 24.0);
    }

    #[test]
    fn scroll_survives_a_sync_pass() {
        let mut animator = Animator::new();
        animator.set_column_scroll("DP-1", 80.0);
        animator.set_row_scroll(1, 40.0);
        animator.set_stack_scroll(1, 24.0);
        animator.begin_sync();
        run(&mut animator, 50);
        assert_eq!(
            (animator.row_scroll(1), animator.column_scroll("DP-1")),
            (40.0, 80.0),
            "begin_sync clears emphasis targets, not absolute offsets"
        );
        assert_eq!(animator.stack_scroll(1), 24.0);
    }

    #[test]
    fn disabling_animation_applies_targets_immediately() {
        let instant = Timing::from_config(&AnimationConfig {
            enabled: false,
            ..AnimationConfig::default()
        });
        let mut animator = Animator::new();
        animator.set_panel_open("DP-1", true);
        animator.set_column_scroll("DP-1", 0.0);
        animator.set_row_scroll(1, 0.0);
        animator.set_stack_scroll(1, 0.0);
        animator.set_column_scroll("DP-1", 80.0);
        animator.set_row_scroll(1, 40.0);
        animator.set_stack_scroll(1, 24.0);
        assert!(!animator.advance(0.008, instant), "nothing left to animate");
        assert_eq!(animator.panel("DP-1"), 1.0);
        assert_eq!(
            (animator.row_scroll(1), animator.column_scroll("DP-1")),
            (40.0, 80.0)
        );
        assert_eq!(animator.stack_scroll(1), 24.0);
    }

    #[test]
    fn a_longer_configured_duration_settles_more_slowly() {
        let quick = Timing::from_config(&AnimationConfig {
            panel_ms: 100.0,
            ..AnimationConfig::default()
        });
        let slow = Timing::from_config(&AnimationConfig {
            panel_ms: 600.0,
            ..AnimationConfig::default()
        });
        let mut a = Track {
            current: 0.0,
            target: 1.0,
        };
        let mut b = Track {
            current: 0.0,
            target: 1.0,
        };
        for _ in 0..10 {
            a.advance(0.008, quick.panel);
            b.advance(0.008, slow.panel);
        }
        assert!(a.current > b.current, "{} !> {}", a.current, b.current);
    }

    #[test]
    fn a_stalled_loop_does_not_jump_past_the_target() {
        let mut track = Track {
            current: 0.0,
            target: 1.0,
        };
        track.advance(MAX_DELTA, Timing::default_timing().panel);
        assert!(track.current <= 1.0);
    }
}
