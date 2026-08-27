//! Deciding whether a focus change was the user navigating.
//!
//! Niri reports one user action as a burst of events whose order cannot be
//! relied on -- closing a window emits the focus change before the window
//! removal -- so judging each event on arrival mistakes the fallback focus
//! for navigation. This waits for the burst to settle and compares against
//! the last state it acted on, which removes that whole class of ordering
//! bug.

use crate::model::Snapshot;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

/// How long focus must hold still before deciding whether to raise the overlay.
///
/// One user action reaches us as several events — a monitor change activates a
/// workspace and then focuses a window inside it; closing a window reports the
/// focus change before the removal. Judging each event as it arrives therefore
/// misreads half of them. Waiting for the burst to settle costs an
/// imperceptible delay on an overlay that then lingers for a second or more.
pub(crate) const FOCUS_SETTLE: Duration = Duration::from_millis(120);

/// How long a window counts as newly appeared.
///
/// Focus landing on a window this young is that window starting up and taking
/// focus, not the user navigating to it. A settle window alone is not enough:
/// an app can map its surface and only claim focus a second later, well after
/// any fixed grace period, and the focus event on its own is indistinguishable
/// from navigation unless the window's age is known.
const NEW_WINDOW_GRACE: Duration = Duration::from_millis(1200);

/// Why focus moved, once an event burst has settled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FocusVerdict {
    /// The user navigated between workspaces on one monitor. This is the
    /// only case that should raise a peek overlay.
    Navigated,
    /// Focus moved for some other reason: a window opened or closed, a
    /// slow-starting app claimed it, or it crossed to another monitor.
    Incidental,
}

/// Decides whether a focus change was the user navigating.
///
/// Niri reports one user action as a burst of events whose order cannot be
/// relied on -- closing a window emits the focus change BEFORE the window
/// removal -- so judging each event as it arrives mistakes the fallback
/// focus for navigation. This waits for the burst to settle and compares
/// against the last state it acted on, which removes that whole class of
/// ordering bug.
#[derive(Default)]
pub(crate) struct FocusWatcher {
    /// Focus as last seen, including which window.
    observed: Option<FocusMark>,
    /// Focus as last acted on, workspace granularity only.
    acted_focus: Option<WorkspaceFocusMark>,
    /// The window set as last acted on.
    acted_windows: Option<HashSet<u64>>,
    /// When the current burst started, if one is in flight.
    pending_since: Option<Instant>,
    /// First time each window was seen, for the new-window grace.
    window_first_seen: HashMap<u64, Instant>,
}

impl FocusWatcher {
    /// Take a new snapshot. Returns true when focus changed, which means
    /// the panel should re-sync to live state immediately -- separately
    /// from whether a peek is warranted, which only `settle` can say.
    pub(crate) fn observe(&mut self, snapshot: &Snapshot, now: Instant) -> bool {
        let windows = live_window_ids(snapshot);
        for id in &windows {
            self.window_first_seen.entry(*id).or_insert(now);
        }
        self.window_first_seen.retain(|id, _| windows.contains(id));

        let signature = focus_signature(snapshot);
        let changed = self
            .observed
            .replace(signature)
            .is_some_and(|previous| previous != *self.observed.as_ref().unwrap());
        if changed {
            self.pending_since = Some(now);
        }
        changed
    }

    /// When the in-flight burst started, so the loop can wake to settle it.
    pub(crate) fn pending_since(&self) -> Option<Instant> {
        self.pending_since
    }

    /// Deliver a verdict once the burst has been quiet for `FOCUS_SETTLE`.
    /// `None` means there is nothing to settle yet.
    pub(crate) fn settle(&mut self, snapshot: &Snapshot, now: Instant) -> Option<FocusVerdict> {
        let since = self.pending_since?;
        if now.duration_since(since) < FOCUS_SETTLE {
            return None;
        }
        self.pending_since = None;

        let signature = workspace_focus_signature(snapshot);
        let windows = live_window_ids(snapshot);
        let movement = self
            .acted_focus
            .as_ref()
            .map(|previous| classify_focus_move(previous, &signature))
            .unwrap_or(FocusMove::None);
        // A window appearing or disappearing moves focus without the user
        // navigating anywhere.
        let lifecycle = self
            .acted_windows
            .as_ref()
            .is_some_and(|previous| *previous != windows);
        // A slow-starting app can map its surface and only claim focus a
        // second later, long after the burst settled, so window age is
        // checked separately from the lifecycle comparison.
        let focused_is_new = snapshot
            .focused_window
            .and_then(|id| self.window_first_seen.get(&id))
            .is_some_and(|seen| now.duration_since(*seen) < NEW_WINDOW_GRACE);

        self.acted_focus = Some(signature);
        self.acted_windows = Some(windows);

        if movement == FocusMove::WithinOutput && !lifecycle && !focused_is_new {
            Some(FocusVerdict::Navigated)
        } else {
            Some(FocusVerdict::Incidental)
        }
    }
}

/// Ids of every window currently mapped, across all outputs.
pub(crate) fn live_window_ids(snapshot: &Snapshot) -> HashSet<u64> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{OutputModel, WindowModel, WorkspaceModel};
    use std::collections::BTreeMap;

    fn focus_test_snapshot(
        output: &str,
        workspace_id: u64,
        focused_window: Option<u64>,
    ) -> Snapshot {
        let window_id = focused_window.unwrap_or(1);
        let window = WindowModel {
            id: window_id,
            workspace_id,
            app_id: "app".into(),
            title: String::new(),
            focused: true,
            urgent: false,
            floating: false,
            column: Some(0),
            tile_width: 100.0,
            tile_height: 100.0,
            x: None,
            y: None,
        };
        Snapshot {
            outputs: BTreeMap::from([(
                output.into(),
                OutputModel {
                    name: output.into(),
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1080,
                    workspaces: vec![WorkspaceModel {
                        id: workspace_id,
                        index: 1,
                        name: None,
                        active: true,
                        focused: true,
                        urgent: false,
                        active_window: Some(window_id),
                        columns: BTreeMap::from([(0, BTreeMap::from([(0, window)]))]),
                        floating: Vec::new(),
                    }],
                },
            )]),
            focused_window,
            overview_open: false,
            generation: 1,
            content_revision: 0,
        }
    }

    /// Snapshot with several windows on one workspace, so tests can hold the
    /// window set fixed while focus moves -- which is what isolates the
    /// new-window grace from the lifecycle check.
    fn focus_snapshot_with(
        output: &str,
        workspace_id: u64,
        focused_window: u64,
        window_ids: &[u64],
    ) -> Snapshot {
        let mut column = BTreeMap::new();
        for (row, id) in window_ids.iter().enumerate() {
            column.insert(
                row,
                WindowModel {
                    id: *id,
                    workspace_id,
                    app_id: "app".into(),
                    title: String::new(),
                    focused: *id == focused_window,
                    urgent: false,
                    floating: false,
                    column: Some(0),
                    tile_width: 100.0,
                    tile_height: 100.0,
                    x: None,
                    y: None,
                },
            );
        }
        Snapshot {
            outputs: BTreeMap::from([(
                output.into(),
                OutputModel {
                    name: output.into(),
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1080,
                    workspaces: vec![WorkspaceModel {
                        id: workspace_id,
                        index: 1,
                        name: None,
                        active: true,
                        focused: true,
                        urgent: false,
                        active_window: Some(focused_window),
                        columns: BTreeMap::from([(0, column)]),
                        floating: Vec::new(),
                    }],
                },
            )]),
            focused_window: Some(focused_window),
            overview_open: false,
            generation: 1,
            content_revision: 0,
        }
    }

    /// Establish an acted-on baseline of (eDP-1, workspace 2) with `windows`
    /// all first seen at `t0`.
    fn focus_baseline_with(w: &mut FocusWatcher, t0: Instant, windows: &[u64]) {
        let start = focus_snapshot_with("eDP-1", 1, windows[0], windows);
        w.observe(&start, t0);
        let moved = focus_snapshot_with("eDP-1", 2, windows[0], windows);
        w.observe(&moved, t0 + FOCUS_SETTLE);
        assert_eq!(
            w.settle(&moved, t0 + FOCUS_SETTLE * 2),
            Some(FocusVerdict::Incidental),
            "the first settle only establishes the baseline"
        );
    }

    /// Run one observe+settle cycle to bootstrap the acted-on baseline.
    fn focus_baseline(w: &mut FocusWatcher, t0: Instant) {
        let s = focus_test_snapshot("eDP-1", 1, Some(10));
        w.observe(&s, t0);
        let s2 = focus_test_snapshot("eDP-1", 2, Some(20));
        w.observe(&s2, t0 + FOCUS_SETTLE);
        // First settle fires and establishes acted_on. It is always Incidental
        // because acted_focus is None (no prior baseline).
        assert_eq!(
            w.settle(&s2, t0 + FOCUS_SETTLE * 2),
            Some(FocusVerdict::Incidental)
        );
        // acted_focus = (eDP-1, 2), acted_windows = {20}
    }

    /// Closing a window emits the focus change BEFORE the window removal, so a
    /// verdict taken per event mistakes the fallback focus for navigation.
    ///
    /// Constructed so ONLY the lifecycle check can produce Incidental: the
    /// workspace really did move within the output, and the window that ends
    /// up focused is far older than the new-window grace.
    #[test]
    fn a_window_closing_is_not_navigation() {
        let t0 = Instant::now();
        let mut w = FocusWatcher::default();
        focus_baseline_with(&mut w, t0, &[10, 30]);

        // Window 10 closes; focus falls back to 30, and the workspace moves
        // back to 1 -- WithinOutput movement.
        let closed = focus_snapshot_with("eDP-1", 1, 30, &[30]);
        let observed_at = t0 + Duration::from_millis(1500);
        w.observe(&closed, observed_at);
        // Settled well past NEW_WINDOW_GRACE, so window 30 is not "new".
        let settled_at = t0 + Duration::from_millis(2000);
        assert!(settled_at.duration_since(t0) > NEW_WINDOW_GRACE);
        assert_eq!(
            w.settle(&closed, settled_at),
            Some(FocusVerdict::Incidental),
            "a window closing must not raise a peek, even though focus moved \
             within the output"
        );
    }

    /// A slow-starting app can map its surface and only claim focus a second
    /// later, long after the burst settled.
    ///
    /// Constructed so ONLY the new-window grace can produce Incidental: the
    /// window set is identical to the one last acted on, so the lifecycle
    /// check cannot fire.
    #[test]
    fn a_newly_mapped_window_taking_focus_is_not_navigation() {
        let t0 = Instant::now();
        let mut w = FocusWatcher::default();
        focus_baseline_with(&mut w, t0, &[10, 99]);

        // Window 99 was already mapped, so the set is unchanged, but it was
        // first seen at t0 and claims focus while still inside the grace.
        let claimed = focus_snapshot_with("eDP-1", 1, 99, &[10, 99]);
        w.observe(&claimed, t0 + FOCUS_SETTLE * 3);
        let settled_at = t0 + FOCUS_SETTLE * 4;
        assert!(settled_at.duration_since(t0) < NEW_WINDOW_GRACE);
        assert_eq!(
            w.settle(&claimed, settled_at),
            Some(FocusVerdict::Incidental),
            "a young window claiming focus must not raise a peek"
        );
    }

    /// Moving between workspaces on one monitor is the case peek exists for.
    #[test]
    fn moving_between_workspaces_on_one_output_is_navigation() {
        let t0 = Instant::now();
        let mut w = FocusWatcher::default();
        focus_baseline(&mut w, t0);

        // Move to ws3, same output, same window set (lifecycle = false).
        // Window 20 was first seen at t0+FOCUS_SETTLE during the baseline;
        // settling at t0+FOCUS_SETTLE*11 makes it 1200ms old, equal to
        // NEW_WINDOW_GRACE, so focused_is_new is false.
        let s = focus_test_snapshot("eDP-1", 3, Some(20));
        w.observe(&s, t0 + FOCUS_SETTLE * 3);
        assert_eq!(
            w.settle(&s, t0 + FOCUS_SETTLE * 11),
            Some(FocusVerdict::Navigated)
        );
    }

    /// Focus landing on another monitor is not within-output navigation.
    #[test]
    fn focus_crossing_outputs_is_not_navigation() {
        let t0 = Instant::now();
        let mut w = FocusWatcher::default();
        focus_baseline(&mut w, t0);

        // Focus crosses to a different output.
        let s = focus_test_snapshot("DP-2", 5, Some(50));
        w.observe(&s, t0 + FOCUS_SETTLE * 3);
        assert_eq!(
            w.settle(&s, t0 + FOCUS_SETTLE * 4),
            Some(FocusVerdict::Incidental)
        );
    }

    /// Nothing settles until the burst has been quiet for FOCUS_SETTLE.
    #[test]
    fn nothing_settles_before_the_burst_is_quiet() {
        let t0 = Instant::now();
        let mut w = FocusWatcher::default();

        let s1 = focus_test_snapshot("eDP-1", 1, Some(10));
        w.observe(&s1, t0);
        // First observe never sets pending (no prior state), so settle is None.
        assert_eq!(
            w.settle(&s1, t0 + FOCUS_SETTLE - Duration::from_millis(1)),
            None
        );

        // Trigger a real change to set pending_since.
        let s2 = focus_test_snapshot("eDP-1", 2, Some(20));
        w.observe(&s2, t0 + FOCUS_SETTLE);
        // Settle before FOCUS_SETTLE has elapsed — should return None.
        assert_eq!(
            w.settle(
                &s2,
                t0 + FOCUS_SETTLE + FOCUS_SETTLE - Duration::from_millis(1)
            ),
            None
        );
        // At exactly FOCUS_SETTLE it fires.
        assert!(w.settle(&s2, t0 + FOCUS_SETTLE * 2).is_some());
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
    fn an_unchanged_signature_is_not_a_move() {
        let a: WorkspaceFocusMark = (Some("DP-1".into()), Some(1));
        assert_eq!(classify_focus_move(&a, &a.clone()), FocusMove::None);
    }

    fn multi_ws_snapshot(focused_ws: u64, focused_window: Option<u64>) -> Snapshot {
        let window_id = focused_window.unwrap_or(1);
        let make_window = |id: u64, ws_id: u64| WindowModel {
            id,
            workspace_id: ws_id,
            app_id: "app".into(),
            title: String::new(),
            focused: id == window_id,
            urgent: false,
            floating: false,
            column: Some(0),
            tile_width: 100.0,
            tile_height: 100.0,
            x: None,
            y: None,
        };
        let ws1_active = if focused_ws == 1 { window_id } else { 10 };
        let ws2_active = if focused_ws == 2 { window_id } else { 20 };
        Snapshot {
            outputs: BTreeMap::from([(
                "DP-1".into(),
                OutputModel {
                    name: "DP-1".into(),
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1080,
                    workspaces: vec![
                        WorkspaceModel {
                            id: 1,
                            index: 1,
                            name: None,
                            active: focused_ws == 1,
                            focused: focused_ws == 1,
                            urgent: false,
                            active_window: Some(ws1_active),
                            columns: BTreeMap::from([(
                                0,
                                BTreeMap::from([(0, make_window(10, 1))]),
                            )]),
                            floating: Vec::new(),
                        },
                        WorkspaceModel {
                            id: 2,
                            index: 2,
                            name: None,
                            active: focused_ws == 2,
                            focused: focused_ws == 2,
                            urgent: false,
                            active_window: Some(ws2_active),
                            columns: BTreeMap::from([(
                                0,
                                BTreeMap::from([(0, make_window(20, 2))]),
                            )]),
                            floating: Vec::new(),
                        },
                    ],
                },
            )]),
            focused_window,
            overview_open: false,
            generation: 1,
            content_revision: 0,
        }
    }

    #[test]
    fn focus_signature_tracks_workspace_and_window_but_peek_ignores_window() {
        let mut snapshot = multi_ws_snapshot(1, Some(10));
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
        let mut snapshot = multi_ws_snapshot(1, Some(10));
        let before = workspace_focus_signature(&snapshot);
        let workspaces = &mut snapshot.outputs.get_mut("DP-1").unwrap().workspaces;
        workspaces[0].focused = false;
        workspaces[1].focused = true;
        assert_ne!(before, workspace_focus_signature(&snapshot));
    }
}
