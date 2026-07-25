//! Pointer events and the input datagram.
//!
//! Input travels over QUIC **datagrams**, not reliable streams. This is the one
//! place where dropping data is the correct behaviour: a lost mouse-move is
//! superseded by the next one 4ms later, so retransmitting it only delays the
//! fresh position behind stale bytes. Reliable, ordered delivery for pointer
//! motion is precisely how head-of-line blocking turns a brief packet loss into
//! a visible cursor stall — the defect users describe as "laggy" in TCP-based
//! KVM software.
//!
//! Button and key events are a different matter: a dropped release strands a
//! modifier down. Those carry [`Reliability::Reliable`] and go over a stream.

use serde::{Deserialize, Serialize};

use crate::geom::NormPos;
use crate::ids::MonitorId;
use crate::keyboard::KeyEvent;

/// Physical mouse buttons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
    /// Additional buttons on gaming mice, zero-indexed beyond the named five.
    Extra(u8),
}

/// How to interpret scroll magnitudes.
///
/// Platforms disagree fundamentally: Windows reports detents in units of 120,
/// macOS reports pixel-precise continuous deltas from trackpads, and X11 reports
/// discrete button clicks. Tagging the unit lets the receiver convert into
/// whatever its own OS expects instead of guessing at a scale factor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScrollUnit {
    /// Notched wheel clicks. `1.0` is one detent.
    Lines,
    /// Continuous, pixel-precise scrolling from a trackpad or free-spin wheel.
    Pixels,
}

/// A pointer event.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum PointerEvent {
    /// Absolute position within the target monitor, as a fraction of its extent.
    ///
    /// Absolute rather than relative for normal operation: it is self-correcting.
    /// A dropped datagram leaves no accumulated error, because the next position
    /// is authoritative on its own.
    MoveTo {
        pos: NormPos,
    },
    /// Relative motion, in the sender's device units.
    ///
    /// For pointer-locked contexts — games and 3D viewports — where absolute
    /// positioning is meaningless because the application hides and recentres the
    /// cursor every frame.
    MoveBy {
        dx: f64,
        dy: f64,
    },
    Button {
        button: MouseButton,
        pressed: bool,
    },
    Scroll {
        dx: f64,
        dy: f64,
        unit: ScrollUnit,
    },
}

/// Delivery guarantee a payload requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reliability {
    /// May be dropped; a newer message supersedes it.
    BestEffort,
    /// Must arrive, or state desynchronises.
    Reliable,
}

/// An input event addressed to one monitor on the peer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum InputEvent {
    Pointer(PointerEvent),
    Key(KeyEvent),
    /// The cursor has left this peer entirely; restore its local pointer and
    /// release any modifiers still held.
    ///
    /// Also sent on graceful disconnect: without it, a peer that had Ctrl down
    /// when control moved away is left with a stuck modifier.
    ReleaseControl,
}

impl InputEvent {
    /// Whether losing this event is recoverable.
    ///
    /// Motion and absolute scroll are self-correcting. Anything that latches
    /// state — button and key transitions, control handoff — is not.
    pub fn reliability(&self) -> Reliability {
        match self {
            InputEvent::Pointer(PointerEvent::MoveTo { .. }) => Reliability::BestEffort,
            // Relative motion accumulates: a dropped delta is lost distance that
            // no later event corrects.
            InputEvent::Pointer(PointerEvent::MoveBy { .. }) => Reliability::Reliable,
            InputEvent::Pointer(PointerEvent::Scroll { .. }) => Reliability::Reliable,
            InputEvent::Pointer(PointerEvent::Button { .. }) => Reliability::Reliable,
            InputEvent::Key(_) => Reliability::Reliable,
            InputEvent::ReleaseControl => Reliability::Reliable,
        }
    }
}

/// An input event with the sequencing needed to survive unordered delivery.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputFrame {
    /// Monotonic per-connection counter.
    ///
    /// `u64` rather than a smaller wrapping counter: postcard varint-encodes it,
    /// so ordinary values cost one or two bytes, and there is no wraparound
    /// comparison to get subtly wrong. At 1000 events/sec it lasts 500 million
    /// years.
    pub seq: u64,
    /// Which of the peer's monitors this addresses. The destination *node* is
    /// implied by the connection.
    pub target: MonitorId,
    pub event: InputEvent,
}

impl InputFrame {
    pub fn new(seq: u64, target: MonitorId, event: InputEvent) -> Self {
        Self { seq, target, event }
    }
}

/// Drops input frames that arrive out of order.
///
/// Datagrams can be reordered by the network, and a stale absolute position
/// would yank the cursor backwards. Reordering-tolerant state kept per
/// connection.
#[derive(Debug, Default)]
pub struct SequenceGate {
    highest_seen: Option<u64>,
}

impl SequenceGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns `true` if the frame should be processed.
    ///
    /// Frames that latch state are accepted even when late: a stale
    /// button-release still needs applying, or the button stays stuck forever.
    /// Only self-superseding motion is discarded on arrival out of order.
    pub fn accept(&mut self, frame: &InputFrame) -> bool {
        let superseded = matches!(self.highest_seen, Some(h) if frame.seq <= h);
        // Track the high-water mark regardless, so a late arrival does not
        // rewind the gate and let genuinely old frames back through.
        if !superseded {
            self.highest_seen = Some(frame.seq);
        }
        if !superseded {
            return true;
        }
        frame.event.reliability() == Reliability::Reliable
    }

    /// Forget history, after a reconnect restarts the peer's counter.
    pub fn reset(&mut self) {
        self.highest_seen = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keyboard::{KeyAction, Modifiers};

    fn move_to(seq: u64, x: f64) -> InputFrame {
        InputFrame::new(
            seq,
            MonitorId(0),
            InputEvent::Pointer(PointerEvent::MoveTo {
                pos: NormPos::new(x, 0.5),
            }),
        )
    }

    fn button(seq: u64, pressed: bool) -> InputFrame {
        InputFrame::new(
            seq,
            MonitorId(0),
            InputEvent::Pointer(PointerEvent::Button {
                button: MouseButton::Left,
                pressed,
            }),
        )
    }

    #[test]
    fn absolute_motion_is_droppable_relative_motion_is_not() {
        let abs = InputEvent::Pointer(PointerEvent::MoveTo {
            pos: NormPos::new(0.5, 0.5),
        });
        assert_eq!(abs.reliability(), Reliability::BestEffort);

        // Deltas accumulate, so a lost one is lost distance.
        let rel = InputEvent::Pointer(PointerEvent::MoveBy { dx: 1.0, dy: 0.0 });
        assert_eq!(rel.reliability(), Reliability::Reliable);
    }

    #[test]
    fn state_latching_events_are_reliable() {
        for ev in [
            InputEvent::Pointer(PointerEvent::Button {
                button: MouseButton::Left,
                pressed: true,
            }),
            InputEvent::Key(crate::keyboard::KeyEvent::text(
                "a",
                KeyAction::Press,
                Modifiers::NONE,
            )),
            InputEvent::ReleaseControl,
        ] {
            assert_eq!(ev.reliability(), Reliability::Reliable, "{ev:?}");
        }
    }

    #[test]
    fn gate_passes_frames_in_order() {
        let mut gate = SequenceGate::new();
        for seq in 1..=5 {
            assert!(gate.accept(&move_to(seq, 0.1 * seq as f64)));
        }
    }

    #[test]
    fn gate_drops_reordered_motion() {
        let mut gate = SequenceGate::new();
        assert!(gate.accept(&move_to(10, 0.9)));
        // Arrives late; applying it would yank the cursor backwards.
        assert!(!gate.accept(&move_to(9, 0.1)));
        assert!(!gate.accept(&move_to(10, 0.1)));
        assert!(gate.accept(&move_to(11, 0.95)));
    }

    #[test]
    fn gate_never_drops_a_late_button_release() {
        // A discarded release leaves the button stuck down on the receiver, which
        // is unrecoverable without user intervention.
        let mut gate = SequenceGate::new();
        assert!(gate.accept(&move_to(100, 0.5)));
        assert!(gate.accept(&button(42, false)));
    }

    #[test]
    fn late_frames_do_not_rewind_the_gate() {
        let mut gate = SequenceGate::new();
        assert!(gate.accept(&move_to(100, 0.5)));
        assert!(!gate.accept(&move_to(50, 0.1)));
        // The high-water mark must still be 100, not 50.
        assert!(!gate.accept(&move_to(70, 0.2)));
        assert!(gate.accept(&move_to(101, 0.6)));
    }

    #[test]
    fn reset_allows_a_reconnecting_peer_to_restart_numbering() {
        let mut gate = SequenceGate::new();
        assert!(gate.accept(&move_to(9999, 0.5)));
        gate.reset();
        assert!(gate.accept(&move_to(1, 0.1)));
    }

    #[test]
    fn frame_serde_round_trips() {
        let f = InputFrame::new(
            7,
            MonitorId(2),
            InputEvent::Pointer(PointerEvent::Scroll {
                dx: 0.0,
                dy: -3.0,
                unit: ScrollUnit::Lines,
            }),
        );
        let bytes = postcard::to_allocvec(&f).unwrap();
        assert_eq!(postcard::from_bytes::<InputFrame>(&bytes).unwrap(), f);
    }

    #[test]
    fn motion_frames_stay_small_enough_for_a_datagram() {
        // Input frames must never approach the QUIC datagram limit, or they
        // fragment and lose the latency benefit that justified datagrams.
        let bytes = postcard::to_allocvec(&move_to(1_000_000, 0.5)).unwrap();
        assert!(bytes.len() < 32, "motion frame is {} bytes", bytes.len());
    }

    #[test]
    fn extra_mouse_buttons_round_trip() {
        let f = InputFrame::new(
            1,
            MonitorId(0),
            InputEvent::Pointer(PointerEvent::Button {
                button: MouseButton::Extra(9),
                pressed: true,
            }),
        );
        let bytes = postcard::to_allocvec(&f).unwrap();
        assert_eq!(postcard::from_bytes::<InputFrame>(&bytes).unwrap(), f);
    }
}
