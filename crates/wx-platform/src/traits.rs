//! The platform contract.
//!
//! Five narrow traits rather than one wide `Platform` trait, because the
//! combinations really do vary: a headless Raspberry Pi captures input and has no
//! displays, clipboard, or screensaver; a locked-down macOS machine can enumerate
//! displays but cannot capture until the user grants Input Monitoring. Splitting
//! them lets [`crate::PlatformBackend`] report honest
//! [`wx_proto::Capabilities`] instead of a single all-or-nothing flag.
//!
//! Every trait here is object-safe on purpose. The agent holds
//! `Box<dyn InputInjector>` and friends so that the compile-time backend choice
//! does not leak into the engine's type signatures, and so tests can substitute
//! recording fakes.

use wx_proto::{
    ClipboardFormat, Edge, InputEvent, KeyEvent, Monitor, MouseButton, NormPos, Point, Rect,
    ScrollUnit,
};

use crate::error::Result;

/// One local input event, already translated out of OS terms.
///
/// Note what is *not* here: scancodes and virtual key codes. Keys arrive already
/// resolved to text through the local layout (see [`crate::keyres`]), because
/// resolution has to happen on the machine whose layout is authoritative. Any
/// backend that hands raw keycodes upwards has moved the cross-layout guarantee
/// to the wrong side of the network.
#[derive(Debug, Clone, PartialEq)]
pub enum CapturedEvent {
    /// Pointer motion, in the local desktop's pixel space.
    ///
    /// Both forms are reported: `delta` drives the virtual cursor (which may be
    /// on another machine, where the local absolute position is meaningless), and
    /// `position` lets the engine notice that something else moved the real
    /// cursor — a focus-follows-mouse warp, a game recentring the pointer — and
    /// resynchronise instead of accumulating drift.
    PointerMotion {
        dx: f64,
        dy: f64,
        position: Point,
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
    Key(KeyEvent),
}

/// Where captured events are delivered.
///
/// A callback rather than a channel so that the backend does not dictate the
/// runtime: the agent boxes a closure that pushes into whatever queue it already
/// has. `Send` because on every platform the callback runs on a backend-owned
/// thread, never the caller's.
///
/// # The callback must not block
///
/// On Windows this runs inside a `WH_KEYBOARD_LL` hook, which is synchronous for
/// the whole desktop: exceed `LowLevelHooksTimeout` (300ms by default) and
/// Windows silently removes the hook, killing capture until the agent restarts.
/// macOS event taps behave the same way. Push into a queue and return.
pub type CaptureSink = Box<dyn FnMut(CapturedEvent) + Send + 'static>;

/// Reports the displays attached to this machine.
pub trait DisplayEnumerator: Send {
    /// Current displays, in the machine's own desktop coordinate space.
    ///
    /// Re-read rather than cached: monitors appear and vanish as docks are
    /// connected and lids closed, and a stale rectangle silently routes the
    /// cursor into a screen that is no longer there.
    fn monitors(&self) -> Result<Vec<Monitor>>;

    /// The primary display, if the OS designates one.
    fn primary(&self) -> Result<Option<Monitor>> {
        Ok(self.monitors()?.into_iter().find(|m| m.primary))
    }

    /// Bounding rectangle of all displays together.
    ///
    /// Needed by injection backends that address the desktop as one plane
    /// (Windows `MOUSEEVENTF_VIRTUALDESK`), and by the UI to fit the layout
    /// canvas.
    fn virtual_bounds(&self) -> Result<wx_proto::Rect> {
        Ok(crate::coords::union_bounds(
            self.monitors()?.iter().map(|m| m.local_bounds),
        ))
    }
}

/// Captures local keyboard and mouse input.
pub trait InputCapture: Send {
    /// Begin delivering events to `sink`.
    ///
    /// Fails with [`crate::PlatformError::AlreadyCapturing`] if capture is live:
    /// two overlapping hook chains would duplicate every keystroke, which reads
    /// as "my machine types everything twice" and is hard to attribute.
    fn start(&mut self, sink: CaptureSink) -> Result<()>;

    /// Stop capturing and release OS resources. Idempotent-safe callers should
    /// still tolerate [`crate::PlatformError::NotCapturing`].
    fn stop(&mut self) -> Result<()>;

    fn is_capturing(&self) -> bool;

    /// Whether captured input is also allowed to reach local applications.
    ///
    /// Must be true exactly while the cursor is on a remote machine. Without it,
    /// driving a peer also types into whatever local window has focus — the
    /// classic "my keystrokes went to both computers" failure.
    ///
    /// Kept separate from [`InputCapture::start`] because it flips on every edge
    /// crossing, and tearing the hooks down and back up that often loses events.
    fn set_suppress_local(&mut self, suppress: bool) -> Result<()>;

    fn suppresses_local(&self) -> bool;

    /// Which edges of which local screens the cursor is allowed to leave by.
    ///
    /// A backend that grabs the pointer at a screen edge must only do so where the
    /// layout has somewhere for the cursor to go. On every other edge the pointer
    /// has to stop dead, exactly as it does with nothing running — a machine that
    /// takes the cursor at an edge with nothing beyond it has taken the desktop
    /// away from whoever is sitting at it, and no local action gives it back.
    ///
    /// The caller is the only thing that knows: adjacency is a fact about the
    /// global layout, which is the agent's, and a platform backend can see no
    /// further than its own screens. So this is pushed down rather than asked for,
    /// and pushed down *again* on every layout change — adding, removing or
    /// moving a machine changes which edges are live, and must take effect without
    /// a restart.
    ///
    /// Until it is called, no edge is live. Failing closed is deliberate: an
    /// unarmed edge is an ordinary screen edge, where an edge armed on a guess is
    /// the failure above.
    ///
    /// Idempotent. Callers push the whole set whenever it may have changed rather
    /// than tracking deltas, and a backend for which nothing changed must do
    /// nothing — on Wayland re-arming means a portal round trip.
    fn set_exits(&mut self, exits: &[ScreenExits]) -> Result<()>;
}

/// The edges of one local screen the cursor may leave this machine by.
///
/// Keyed by geometry rather than by [`wx_proto::MonitorId`] because the thing a
/// backend has to match it against may not be a monitor at all: on Wayland the
/// capture portal reports its own regions, which are rectangles in this machine's
/// desktop space and carry no monitor identity. Bounds are the one description
/// both ends already agree on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenExits {
    /// The screen, in this machine's own desktop space — the same coordinates
    /// [`DisplayEnumerator::monitors`] reports [`Monitor::local_bounds`] in.
    pub bounds: Rect,
    /// Edges of it with somewhere for the cursor to go. Empty means this screen is
    /// surrounded by nothing, and the pointer stops on all four sides.
    pub edges: Vec<Edge>,
}

impl ScreenExits {
    pub fn contains(&self, edge: Edge) -> bool {
        self.edges.contains(&edge)
    }
}

/// Injects received input into this machine's OS.
pub trait InputInjector: Send {
    /// Inject one event.
    ///
    /// `monitor` is the local display the event is addressed to; it is what
    /// [`wx_proto::NormPos`] is denormalized against. Passed per call rather than
    /// held as state so the injector stays stateless with respect to layout, and
    /// so a monitor hot-unplug cannot leave it pointing at a stale rectangle.
    fn inject(&mut self, monitor: &Monitor, event: &InputEvent) -> Result<()>;

    /// Move the cursor without generating a click, used when control enters this
    /// machine so the pointer appears at the seam rather than wherever it was
    /// left.
    fn warp_cursor(&mut self, monitor: &Monitor, pos: NormPos) -> Result<()>;

    /// Release every key and button this injector has pressed.
    ///
    /// Called on `ReleaseControl` and on any disconnect. A dropped release
    /// otherwise strands Ctrl or a mouse button down, and the user cannot clear
    /// it without pressing the physical key on the affected machine.
    fn release_all(&mut self) -> Result<()>;
}

/// Reads and writes the system clipboard.
///
/// Byte-oriented rather than typed: the payloads travel over the wire as opaque
/// bytes ([`wx_proto::ControlMsg::ClipboardData`]), and re-parsing a 40MB PNG on
/// the way through would be pure waste.
pub trait ClipboardAccess: Send {
    /// Formats the clipboard currently holds, in the order the platform prefers.
    fn available_formats(&self) -> Result<Vec<ClipboardFormat>>;

    /// Read the clipboard in one format.
    ///
    /// `Utf8Text` and `Html` are UTF-8 bytes, `Png` is a PNG file, and
    /// `FileList` is newline-separated absolute paths — the same shapes the wire
    /// format promises, so no conversion happens above this trait.
    fn read(&self, format: ClipboardFormat) -> Result<Vec<u8>>;

    fn write(&self, format: ClipboardFormat, data: &[u8]) -> Result<()>;

    /// A counter that changes whenever the clipboard contents change.
    ///
    /// Cheap change detection: polling `read` and hashing would pull megabytes of
    /// image data across the process boundary many times a second just to notice
    /// that nothing happened.
    fn change_serial(&self) -> Result<u64>;
}

/// Locks the local session.
pub trait ScreenSaverControl: Send {
    /// Lock this machine's session, so a peer's screensaver locks the whole desk.
    fn lock_session(&self) -> Result<()>;

    /// Whether the session is locked right now.
    ///
    /// Best-effort: no OS exposes this as a first-class query, so backends use
    /// heuristics and may be briefly wrong across a lock transition. Never gate
    /// correctness on it.
    fn is_locked(&self) -> Result<bool>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use wx_proto::{MonitorId, Rect};

    /// Minimal enumerator, to exercise the provided methods without an OS.
    struct FakeDisplays(Vec<Monitor>);

    impl DisplayEnumerator for FakeDisplays {
        fn monitors(&self) -> Result<Vec<Monitor>> {
            Ok(self.0.clone())
        }
    }

    fn mon(id: u32, bounds: Rect, primary: bool) -> Monitor {
        Monitor {
            id: MonitorId(id),
            name: format!("fake{id}"),
            local_bounds: bounds,
            scale: 1.0,
            primary,
        }
    }

    #[test]
    fn traits_are_object_safe_so_the_agent_can_box_them() {
        // Compile-time assertion: if any trait above gains a generic method or a
        // `Self: Sized` bound, this stops building.
        let boxed: Box<dyn DisplayEnumerator> = Box::new(FakeDisplays(vec![]));
        assert_eq!(boxed.monitors().unwrap().len(), 0);
    }

    #[test]
    fn primary_display_is_the_one_the_os_flagged() {
        let d = FakeDisplays(vec![
            mon(0, Rect::new(-1920, 0, 1920, 1080), false),
            mon(1, Rect::new(0, 0, 2560, 1440), true),
        ]);
        assert_eq!(d.primary().unwrap().unwrap().id, MonitorId(1));
    }

    #[test]
    fn primary_is_none_when_no_display_claims_it() {
        // A headless node, or an X11 setup with no RRPrimaryOutput set.
        let d = FakeDisplays(vec![mon(0, Rect::new(0, 0, 800, 600), false)]);
        assert!(d.primary().unwrap().is_none());
    }

    #[test]
    fn virtual_bounds_span_monitors_at_negative_offsets() {
        let d = FakeDisplays(vec![
            mon(0, Rect::new(0, 0, 1920, 1080), true),
            mon(1, Rect::new(-2560, -300, 2560, 1440), false),
        ]);
        assert_eq!(
            d.virtual_bounds().unwrap(),
            Rect::new(-2560, -300, 4480, 1440)
        );
    }

    #[test]
    fn virtual_bounds_of_a_headless_node_are_empty_not_a_panic() {
        let d = FakeDisplays(vec![]);
        assert!(d.virtual_bounds().unwrap().is_empty());
    }
}
