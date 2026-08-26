//! Panel state for the daemon loop.

use crate::PressState;
use std::time::Instant;

/// Timing state for the peek overlay.
///
/// The three fields always move together: every path that stops peeking has to
/// drop the arming deadline, the pointer-grace mark, and the live pointer
/// region at once. `disarm` is the only way to do that, so no caller can clear
/// two of the three and leave the daemon in a state that cannot be reached by
/// the state machine on paper.
#[derive(Debug, Default)]
pub struct PeekState {
    /// When a peek overlay expires and takes itself off screen.
    deadline: Option<Instant>,
    /// When the overlay stops being click-through. The pointer region is
    /// withheld for a moment after it appears so that a click already on its
    /// way to whatever the overlay just covered still lands there.
    pointer_at: Option<Instant>,
    /// Whether the overlay is currently claiming the panel rectangle. It
    /// outlives arming, and following focus to another output has to carry it
    /// across: re-showing the overlay as passive would drop the region with
    /// nothing left to hand it back.
    pointer_live: bool,
}

impl PeekState {
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    pub fn set_deadline(&mut self, deadline: Option<Instant>) {
        self.deadline = deadline;
    }

    pub fn pointer_at(&self) -> Option<Instant> {
        self.pointer_at
    }

    /// Start the grace period after which the overlay takes the pointer.
    pub fn arm_pointer(&mut self, at: Instant) {
        self.pointer_at = Some(at);
    }

    pub fn pointer_live(&self) -> bool {
        self.pointer_live
    }

    /// The grace period has passed: the overlay claims the pointer region.
    pub fn take_pointer(&mut self) {
        self.pointer_at = None;
        self.pointer_live = true;
    }

    /// Stop peeking, clearing every field in one go.
    pub fn disarm(&mut self) {
        *self = Self::default();
    }
}

/// State belonging to one panel session.
///
/// These fields only mean anything while the panel is on screen, and every way
/// of taking it off screen has to drop all of them together. Three paths used
/// to do that by hand, and the Escape path forgot `press` -- so a drag
/// cancelled with Escape still dispatched its drop when the button came up.
/// `close` is the one way to end a session, so that subset cannot drift again.
#[derive(Debug, Default)]
pub struct PanelState {
    /// Where Niri's focus was when the session opened, so a drop can put it
    /// back if it fails.
    origin_focus: Option<u64>,
    /// An output the manual panel is about to follow focus onto, with the
    /// instant it became a candidate.
    follow_candidate: Option<(String, Instant)>,
    /// The pointer press being tracked, if the button is down.
    press: Option<PressState>,
    /// Peek overlay timing.
    peek: PeekState,
}

impl PanelState {
    pub fn origin_focus(&self) -> Option<u64> {
        self.origin_focus
    }

    pub fn set_origin_focus(&mut self, focus: Option<u64>) {
        self.origin_focus = focus;
    }

    pub fn follow_candidate_mut(&mut self) -> &mut Option<(String, Instant)> {
        &mut self.follow_candidate
    }

    pub fn follow_candidate(&self) -> Option<&(String, Instant)> {
        self.follow_candidate.as_ref()
    }

    pub fn clear_follow_candidate(&mut self) {
        self.follow_candidate = None;
    }

    pub fn is_pressed(&self) -> bool {
        self.press.is_some()
    }

    pub fn press(&self) -> Option<&PressState> {
        self.press.as_ref()
    }

    pub fn press_mut(&mut self) -> Option<&mut PressState> {
        self.press.as_mut()
    }

    pub fn begin_press(&mut self, press: PressState) {
        self.press = Some(press);
    }

    pub fn take_press(&mut self) -> Option<PressState> {
        self.press.take()
    }

    pub fn peek(&self) -> &PeekState {
        &self.peek
    }

    pub fn peek_mut(&mut self) -> &mut PeekState {
        &mut self.peek
    }

    /// End the session. Everything that only means something while the panel
    /// is up goes at once -- including the tracked press, so a drag the user
    /// cancelled cannot be completed by the release that follows.
    pub fn close(&mut self) {
        self.origin_focus = None;
        self.follow_candidate = None;
        self.press = None;
        self.peek.disarm();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn disarm_clears_every_peek_field() {
        let now = Instant::now();
        let mut peek = PeekState::default();
        peek.set_deadline(Some(now + Duration::from_millis(50)));
        peek.arm_pointer(now);
        peek.take_pointer();
        assert!(peek.pointer_live());

        peek.disarm();

        assert_eq!(peek.deadline(), None);
        assert_eq!(peek.pointer_at(), None);
        assert!(!peek.pointer_live());
    }

    #[test]
    fn taking_the_pointer_consumes_the_grace_mark() {
        let mut peek = PeekState::default();
        peek.arm_pointer(Instant::now());
        assert!(peek.pointer_at().is_some());
        assert!(!peek.pointer_live());

        peek.take_pointer();

        assert_eq!(peek.pointer_at(), None);
        assert!(peek.pointer_live());
    }

    fn drag_press() -> PressState {
        PressState {
            window_id: 7,
            app_id: "firefox".to_string(),
            start_x: 0.0,
            start_y: 0.0,
            current_x: 400.0,
            current_y: 400.0,
            dragging: true,
        }
    }

    /// Escape during a drag used to leave `press` behind, so the release that
    /// followed dispatched the drop the user had just cancelled. Closing the
    /// panel must consume the press.
    #[test]
    fn closing_the_panel_cancels_a_drag_in_progress() {
        let mut panel = PanelState::default();
        panel.set_origin_focus(Some(3));
        panel.begin_press(drag_press());
        assert!(panel.is_pressed());

        // Escape, outside click, IPC hide -- all of them land here.
        panel.close();

        assert!(
            panel.take_press().is_none(),
            "a cancelled drag must not survive to be dropped on release"
        );
        assert_eq!(panel.origin_focus(), None);
    }

    #[test]
    fn close_clears_every_session_field() {
        let mut panel = PanelState::default();
        panel.set_origin_focus(Some(1));
        *panel.follow_candidate_mut() = Some(("DP-1".to_string(), Instant::now()));
        panel.begin_press(drag_press());
        panel.peek_mut().arm_pointer(Instant::now());
        panel.peek_mut().take_pointer();

        panel.close();

        assert_eq!(panel.origin_focus(), None);
        assert!(panel.follow_candidate().is_none());
        assert!(!panel.is_pressed());
        assert!(!panel.peek().pointer_live());
        assert_eq!(panel.peek().deadline(), None);
    }
}
