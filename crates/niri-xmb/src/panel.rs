//! Panel state for the daemon loop.

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
}
