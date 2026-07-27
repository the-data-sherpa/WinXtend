//! Input capture: what the `InputCapture` portal delivers, turned into
//! [`CapturedEvent`]s.
//!
//! Everything here is pure. It holds no D-Bus connection, no libei socket and no
//! runtime, so the decisions that actually matter — what a keycode means on this
//! machine's layout, when suppression is really in force, and when a pinned
//! pointer has to be handed back — are compiled and tested on every platform.
//! [`super::capture_driver`] is the half that needs a desktop.
//!
//! # Why this is a second portal session
//!
//! `org.freedesktop.portal.RemoteDesktop` — the session [`super::driver`] owns and
//! injection sends on — **cannot capture**. Its session object has no zones, no
//! barriers, no `Enable`/`Disable`/`Release` and no `Activated`/`Deactivated`;
//! only `Notify*` methods, and the devices on its libei fd are client-owned
//! *emulation* devices. Measured on the alpha target: ten seconds of real typing
//! and mousing through it produced zero events, handshaking as both Receiver and
//! Sender.
//!
//! Capture is `org.freedesktop.portal.InputCapture`, which GNOME 50 ships for the
//! first time (backed by `org.gnome.Mutter.InputCapture`). Two portals, two
//! sessions, two consent dialogs — unavoidable, and the reason the "one session
//! for both directions" rule in [`super`] stops at injection.
//!
//! Talking to `org.gnome.Mutter.InputCapture` directly would avoid the prompt
//! entirely. It is a private, unstable interface and it routes around the decision
//! the user is being asked to make, so it is explicitly not an option here.
//!
//! # Suppression is real, and measured
//!
//! While the portal session is *activated* the compositor sends this client every
//! keystroke, button and scroll, and sends local windows none of them. Across five
//! runs on the alpha target a focused full-screen editor received 0 keys, 0
//! buttons and 0 scroll events during capture, including real physical
//! keystrokes, and the pointer stayed at exactly one coordinate rather than
//! merely going quiet. A pointer-only session still suppressed the keyboard and a
//! keyboard-only session still suppressed buttons and scroll.
//!
//! So [`crate::traits::InputCapture::set_suppress_local`] is implemented rather
//! than refused, and [`CaptureState::suppresses_local`] answers with the one thing
//! that is actually true: whether the compositor has this session activated.
//!
//! # What the compositor does *not* send, and what follows from it
//!
//! Measured on the alpha target, capturing as a Receiver:
//!
//! * **No `ei_keyboard.modifiers`.** Not once, across shifted and unshifted
//!   typing. So modifier state cannot be read off the compositor and is tracked
//!   here from the captured key stream itself — which is what every other backend
//!   does anyway, and is why [`Modifiers`] on a captured event is trustworthy.
//!   The cost is stated on [`CaptureState::set_keymap`]: the active layout group
//!   cannot be observed either.
//! * **No absolute pointer motion.** The seat offers one "captured relative
//!   pointer" and one "captured keyboard"; there is no absolute device and no
//!   `ei_pointer_absolute.motion_absolute` event. The real cursor position comes
//!   from the portal's `Activated` signal instead, which is exactly what
//!   [`CaptureState::activated`] does with it.
//! * **No `Deactivated` for a client-initiated `Release`.** The signal exists and
//!   is watched, but GNOME does not emit it when we are the ones letting go. Our
//!   own release is therefore authoritative for our own state; the signal is kept
//!   for the case where the compositor ends the activation.
//!
//! # Where `position` comes from, and why it is not a fiction
//!
//! [`CapturedEvent::PointerMotion`] must carry a real delta *and* a real absolute
//! position, and must not compute one from the other. The delta is `ei_pointer`'s,
//! verbatim. The position is the pointer's real location, which during an
//! activation is a constant: the compositor pins the pointer at the barrier for as
//! long as capture is active. `Activated` reports that point, and it is reported
//! unchanged for every event of that activation because that is where the pointer
//! actually is. Integrating the deltas into it would be the fiction — it would
//! describe a cursor that is on another machine.

use std::sync::Mutex;

use wx_proto::{KeyAction, Modifiers, MouseButton, Point, ScrollUnit, SpecialKey};

use crate::error::{PlatformError, Result};
use crate::keyres::{KeyResolver, RawKey};
use crate::traits::{CaptureSink, CapturedEvent};

use super::keymap::{Keymap, LevelMods};
use super::keys::{
    button_from_evdev, char_for_keysym, dead_accent, special_from_evdev, special_from_keysym,
    KEY_LEFTALT, KEY_LEFTCTRL, KEY_LEFTMETA, KEY_LEFTSHIFT, KEY_RIGHTALT,
};
use super::BACKEND;

/// One notch of a notched wheel, in the units `ei_scroll.scroll_discrete` uses.
const SCROLL_DETENT: f64 = 120.0;

/// How far the pointer has to be pulled back from the barrier it crossed before
/// capture is handed back on its own.
///
/// The portal has no way for a client to ask to be activated: the compositor
/// decides, when the pointer crosses an armed barrier. That is the right gesture,
/// but it means capture can activate on an edge the layout has no peer beyond —
/// and then the pointer is pinned, the engine never asks for suppression because
/// the cursor never left this machine, and nothing in the trait would ever release
/// it. A user cannot recover from that without killing the agent.
///
/// So a pull-back releases: once the accumulated motion has travelled this far
/// *away* from the barrier while nothing has claimed the cursor, the session lets
/// go. Deliberately a distance rather than a timer, so it is a decision the user
/// makes with the mouse rather than one a clock makes for them — and so this is
/// testable with no desktop and no clock.
///
/// A fraction of the screen rather than a pixel count, and a large one. Two things
/// set the floor. A user arriving at an edge does not stop dead — measured on the
/// alpha target, an ordinary approach and settle produced well over a hundred
/// pixels of inward travel within tens of milliseconds — and the engine's claim on
/// the cursor arrives over a channel, so there is a window after a real crossing in
/// which nothing has claimed it yet. A small threshold fires in both, and takes the
/// pointer back from under the user. A quarter of a screen is unmistakably a
/// decision to come back.
///
/// Only while unclaimed. Once the engine has asked for suppression a peer owns the
/// cursor, and pulling back is how the user steers it home; releasing then would
/// snatch the pointer back mid-gesture.
const RELEASE_PULLBACK: f64 = 0.25;

/// How far clear of the barrier the pointer is handed back.
///
/// Returning it exactly where the compositor pinned it puts it back on the barrier
/// line, where the next twitch re-triggers capture — which reads as a pointer glued
/// to the edge of the screen. Far enough to be clear of that, small enough that
/// the pointer visibly comes back where the user left it.
const RELEASE_CLEARANCE: f64 = 16.0;

/// The edge of a zone a barrier sits on.
///
/// Kept here rather than reusing `wx_core::Edge`: this crate does not depend on
/// `wx-core`, and the two mean different things — one is a seam in the global
/// layout, this is a side of a physical screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarrierEdge {
    Top,
    Bottom,
    Left,
    Right,
}

impl BarrierEdge {
    /// How far `(dx, dy)` moves the pointer *away* from this edge.
    ///
    /// Negative for motion pushing further into the barrier, which is what a user
    /// trying to cross is doing.
    fn inward(self, dx: f64, dy: f64) -> f64 {
        match self {
            BarrierEdge::Left => dx,
            BarrierEdge::Right => -dx,
            BarrierEdge::Top => dy,
            BarrierEdge::Bottom => -dy,
        }
    }

    /// The screen's extent along the axis this edge is crossed on.
    fn span(self, zone: Zone) -> f64 {
        match self {
            BarrierEdge::Left | BarrierEdge::Right => f64::from(zone.w),
            BarrierEdge::Top | BarrierEdge::Bottom => f64::from(zone.h),
        }
    }
}

/// One region the portal will let this session capture in.
///
/// The portal's own `GetZones` answer, in the same coordinates
/// [`super::display`] enumerates monitors in — measured to match exactly on the
/// alpha target, offsets and all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Zone {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Zone {
    /// Pull a point onto the last pixel inside this zone.
    ///
    /// The portal reports an activation on the *boundary line* — a right-hand
    /// crossing of a 3072-wide screen comes back at x=3072, one past the last
    /// pixel — and everything above this treats a monitor rectangle as half-open,
    /// so an unclamped point belongs to no screen at all. It is then silently
    /// dropped by whatever was going to use it, which is how the position that
    /// exists precisely to resynchronise the cursor ends up resynchronising
    /// nothing.
    ///
    /// Truthful rather than convenient: the pointer really is on the last pixel of
    /// the screen, not on a column that does not exist.
    fn clamp(self, p: Point) -> Point {
        let span = |v: f64, origin: i32, extent: i32| {
            let low = f64::from(origin);
            // A zone one pixel wide has only its origin to offer.
            let high = low + f64::from(extent.max(1) - 1);
            if v.is_nan() {
                low
            } else {
                v.clamp(low, high)
            }
        };
        Point::new(span(p.x, self.x, self.w), span(p.y, self.y, self.h))
    }

    /// Whether `p` is on this screen, the half-open way everything above this
    /// counts a monitor rectangle.
    fn contains(self, p: Point) -> bool {
        let inside = |v: f64, origin: i32, extent: i32| {
            v >= f64::from(origin) && v < f64::from(origin) + f64::from(extent)
        };
        self.w > 0 && self.h > 0 && inside(p.x, self.x, self.w) && inside(p.y, self.y, self.h)
    }

    /// The same, with the boundary line counted in.
    ///
    /// The set of points an activation can name: the portal reports the line, which
    /// is one past the last pixel of the screen the pointer is actually on.
    fn touches(self, p: Point) -> bool {
        let inside = |v: f64, origin: i32, extent: i32| {
            v >= f64::from(origin) && v <= f64::from(origin) + f64::from(extent)
        };
        self.w > 0 && self.h > 0 && inside(p.x, self.x, self.w) && inside(p.y, self.y, self.h)
    }
}

/// Which zone an activation position belongs to.
///
/// The barrier the compositor names is the better answer where there is one — it
/// says which screen the pointer left, not merely which one the coordinate lands
/// on — but an activation the compositor did not attribute to a barrier still has
/// to be clamped onto a real screen, or the position that exists to resynchronise
/// the cursor resynchronises nothing. See [`Zone::clamp`].
///
/// Containment first, then the boundary line, so a point on a seam belongs to the
/// screen it is really on rather than to the one it just left.
pub fn zone_at(zones: &[Zone], p: Point) -> Option<Zone> {
    zones
        .iter()
        .copied()
        .find(|z| z.contains(p))
        .or_else(|| zones.iter().copied().find(|z| z.touches(p)))
}

/// One barrier to ask the compositor for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BarrierPlan {
    pub id: u32,
    pub edge: BarrierEdge,
    /// `(x1, y1, x2, y2)`, the line the barrier runs along.
    pub position: (i32, i32, i32, i32),
    /// The zone this barrier bounds, so an activation on it can be clamped back
    /// inside a screen that actually exists.
    pub zone: Zone,
}

/// A barrier on every edge of the *outer* boundary of the zone set, in the
/// geometry the compositor accepts.
///
/// The two coordinates are not symmetric, which is not obvious and was measured
/// rather than guessed. The barrier *line* sits on the zone boundary, so the far
/// edge is at `offset + size`: a right-hand barrier on a 3072-wide zone at the
/// origin is at x=3072, and one at x=3071 is refused. The barrier's *extent* along
/// the edge, though, must stay inside the zone: a top barrier running to x=3072 is
/// refused where one running to x=3071 is accepted.
///
/// Every outer edge is armed, not just the ones with a peer beyond them, because
/// nothing at this layer knows the global layout — a barrier is the only way the
/// compositor will tell us the pointer reached an edge at all. An edge with
/// nothing behind it is handled where it becomes knowable: see
/// [`RELEASE_PULLBACK`], which hands the pointer straight back.
///
/// An edge with *another local screen* behind it is a different case, and the one
/// this skips. Two side-by-side monitors would otherwise both put a barrier on
/// their shared line, so moving between the user's own two screens activates
/// capture: the pointer pins at the seam, local windows are suppressed, and the
/// router walks the virtual cursor onto local monitor 2 with no handoff — so
/// `owns_cursor()` stays true and `wanted` stays false. Worse, the pull-back guard
/// measures the axis in the crossing direction, so a user carrying on across their
/// own seam accumulates nothing and stays stuck until they deliberately drag a
/// quarter-screen back.
///
/// The tradeoff, stated plainly: a peer can never be reached across a physically
/// internal seam. That costs nothing today because the global layout model does not
/// express that placement anyway.
pub fn barriers_for(zones: &[Zone]) -> Vec<BarrierPlan> {
    let mut out = Vec::new();
    for (index, zone) in zones.iter().enumerate() {
        // A zero-extent zone has no edge to put anything on, and `size - 1` would
        // run the barrier backwards.
        if zone.w <= 0 || zone.h <= 0 {
            continue;
        }
        let (x, y, w, h) = (zone.x, zone.y, zone.w, zone.h);
        for (edge, position) in [
            (BarrierEdge::Top, (x, y, x + w - 1, y)),
            (BarrierEdge::Bottom, (x, y + h, x + w - 1, y + h)),
            (BarrierEdge::Left, (x, y, x, y + h - 1)),
            (BarrierEdge::Right, (x + w, y, x + w, y + h - 1)),
        ] {
            if is_internal_seam(zones, index, *zone, edge) {
                continue;
            }
            out.push(BarrierPlan {
                // 1-based and gap-free, over the barriers actually planned: the
                // portal's `BarrierID` is a `NonZeroU32`, and an id the compositor
                // cannot name is a barrier whose failure cannot be attributed.
                id: out.len() as u32 + 1,
                edge,
                position,
                zone: *zone,
            });
        }
    }
    out
}

/// Whether another zone in the same set sits flush against this edge, so the line
/// the barrier would run along is inside the desktop rather than around it.
fn is_internal_seam(zones: &[Zone], index: usize, zone: Zone, edge: BarrierEdge) -> bool {
    zones.iter().enumerate().any(|(other_index, other)| {
        if other_index == index || other.w <= 0 || other.h <= 0 {
            return false;
        }
        match edge {
            BarrierEdge::Right => {
                other.x == zone.x.saturating_add(zone.w)
                    && overlap(zone.y, zone.h, other.y, other.h)
            }
            BarrierEdge::Left => {
                other.x.saturating_add(other.w) == zone.x
                    && overlap(zone.y, zone.h, other.y, other.h)
            }
            BarrierEdge::Bottom => {
                other.y == zone.y.saturating_add(zone.h)
                    && overlap(zone.x, zone.w, other.x, other.w)
            }
            BarrierEdge::Top => {
                other.y.saturating_add(other.h) == zone.y
                    && overlap(zone.x, zone.w, other.x, other.w)
            }
        }
    })
}

/// Whether two half-open spans share any pixel.
fn overlap(a: i32, a_len: i32, b: i32, b_len: i32) -> bool {
    a.max(b) < a.saturating_add(a_len).min(b.saturating_add(b_len))
}

/// What the trait side asks the driver thread to do.
///
/// A command rather than a direct call because both `Enable` and `Release` are
/// D-Bus round trips and [`crate::traits::InputCapture`] is synchronous by
/// design — see the note at the top of [`super::driver`] for why that is not
/// negotiable.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Arm the barriers. Sent by `start`.
    Enable,
    /// Disarm them. Sent by `stop`.
    Disable,
    /// Hand the pointer back at `position`, ending this activation.
    Release {
        activation_id: Option<u32>,
        position: Option<(f64, f64)>,
    },
}

/// Somewhere to put a [`Command`].
///
/// Installed by the driver when the session comes up and taken away when it goes.
/// A plain closure rather than a channel so this module needs no runtime and
/// compiles on Windows and macOS along with the rest of the backend.
type Control = Box<dyn Fn(Command) + Send + 'static>;

/// Capture as the rest of the backend sees it.
pub struct CaptureState {
    inner: Mutex<Inner>,
}

struct Inner {
    /// Where captured events go. `Some` exactly while
    /// [`crate::traits::InputCapture::is_capturing`] is true.
    sink: Option<CaptureSink>,
    control: Option<Control>,
    keyboard: Option<Keyboard>,
    resolver: KeyResolver,
    /// Mouse buttons this capture has reported pressed and not yet released.
    ///
    /// Tracked for the same reason [`KeyResolver`] tracks keys: an activation that
    /// ends mid-drag would otherwise leave a button down on the peer with nothing
    /// left to lift it.
    buttons: Vec<MouseButton>,
    /// The live activation, or `None` when the compositor is not capturing.
    activation: Option<Activation>,
    /// What the engine last asked for. Distinct from whether it is in force.
    wanted: bool,
}

/// One capture activation: the window between the portal's `Activated` and
/// whatever ends it.
struct Activation {
    id: Option<u32>,
    /// Where the compositor pinned the real pointer. Constant for the activation.
    pinned: Point,
    edge: Option<BarrierEdge>,
    /// Motion accumulated away from `edge`, for [`RELEASE_PULLBACK`].
    pullback: f64,
    /// How far that has to travel before it counts, in the same units. `None` when
    /// the compositor did not say which barrier fired, in which case there is no
    /// axis to measure along and the guard cannot run at all.
    pullback_limit: Option<f64>,
}

impl CaptureState {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                sink: None,
                control: None,
                keyboard: None,
                resolver: KeyResolver::new(),
                buttons: Vec::new(),
                activation: None,
                wanted: false,
            }),
        }
    }

    // -- the trait side ---------------------------------------------------

    pub fn start(&self, sink: CaptureSink) -> Result<()> {
        let mut inner = self.lock();
        if inner.sink.is_some() {
            return Err(PlatformError::AlreadyCapturing);
        }
        inner.sink = Some(sink);
        inner.send(Command::Enable);
        Ok(())
    }

    pub fn stop(&self) -> Result<()> {
        let mut inner = self.lock();
        if inner.sink.is_none() {
            return Err(PlatformError::NotCapturing);
        }
        // Everything still held goes up first, while there is still a sink to put
        // the releases in. Dropping the sink on a held Ctrl strands it on whatever
        // machine the cursor was on, and no local action there will clear it.
        inner.end_activation();
        inner.send(Command::Disable);
        inner.sink = None;
        inner.wanted = false;
        Ok(())
    }

    pub fn is_capturing(&self) -> bool {
        self.lock().sink.is_some()
    }

    /// Whether local applications are being kept from this machine's input right
    /// now.
    ///
    /// The compositor's answer, not ours: exactly while the portal session is
    /// activated, and measured to be exclusive on the alpha target.
    pub fn suppresses_local(&self) -> bool {
        self.lock().activation.is_some()
    }

    /// Ask for local input to be suppressed, or handed back.
    ///
    /// Under this portal the two states are one state — capture is active or it is
    /// not — so this is not a separate switch to flip but a claim on the
    /// activation the compositor has already granted, and a way to give it up.
    ///
    /// Asking for suppression that is not in force is an **error**, not a silent
    /// success. The portal offers a client no way to start an activation: only the
    /// user crossing an armed barrier does that. So if the cursor reached a peer by
    /// some other route — the layout editor, a peer taking control — then local
    /// input really is still going to local windows, and saying otherwise is the
    /// one answer this backend must never give.
    pub fn set_suppress_local(&self, suppress: bool) -> Result<()> {
        let mut inner = self.lock();
        if inner.sink.is_none() {
            return Err(PlatformError::NotCapturing);
        }
        if !suppress {
            inner.wanted = false;
            inner.release();
            return Ok(());
        }
        if inner.activation.is_none() {
            // Recorded as *not* claimed, which matters beyond the error: a claim
            // that failed must not leave `RELEASE_PULLBACK` disabled, or the next
            // crossing would pin the pointer with nothing able to hand it back.
            inner.wanted = false;
            return Err(PlatformError::Other(
                "the input-capture portal is not capturing: local input can only be suppressed \
                 while the compositor has activated the session, which happens when the pointer \
                 crosses a screen edge"
                    .into(),
            ));
        }
        inner.wanted = true;
        Ok(())
    }

    // -- the driver side --------------------------------------------------

    /// Install the hook the trait side sends [`Command`]s through.
    pub fn attach(&self, control: Control) {
        let mut inner = self.lock();
        inner.control = Some(control);
        // A session that came up after `start` was called still has to be armed.
        if inner.sink.is_some() {
            inner.send(Command::Enable);
        }
    }

    /// The session has gone. Everything held goes up, and nothing is suppressed.
    pub fn detach(&self) {
        let mut inner = self.lock();
        inner.end_activation();
        inner.control = None;
        inner.keyboard = None;
    }

    /// The keymap the compositor attached to the captured keyboard.
    ///
    /// Parsed against layout group 1, always. The active group is carried on
    /// `ei_keyboard.modifiers`, which the alpha target does not send to a
    /// capturing client (see the module docs), and there is no other route to it:
    /// the keymap text holds every configured layout but says nothing about which
    /// one is live. So a user with two input sources has their keys resolved
    /// against the first of them.
    ///
    /// Worth revisiting whenever the target moves. [`Keymap::parse`] already takes
    /// the group and the injector re-reads on a group change, so the day a
    /// compositor reports it, this is a one-line change.
    pub fn set_keymap(&self, source: String) {
        let keymap = Keymap::parse(&source, 0);
        if keymap.is_empty() {
            tracing::warn!(
                "the captured keyboard offered no keymap this backend could read; keys will \
                 cross the wire as raw keycodes"
            );
        }
        self.lock().keyboard = Some(Keyboard::new(keymap));
    }

    /// The compositor has started capturing.
    ///
    /// `position` is where it pinned the real pointer, in the zone coordinates the
    /// portal reports — the same space [`super::display`] enumerates monitors in.
    pub fn activated(
        &self,
        id: Option<u32>,
        position: Point,
        edge: Option<BarrierEdge>,
        zone: Option<Zone>,
    ) {
        let position = zone.map_or(position, |z| z.clamp(position));
        let mut inner = self.lock();
        if inner.activation.is_some() {
            // The compositor does not do this, but a second activation with no
            // deactivation between would otherwise leave the old one's held keys
            // stranded.
            inner.end_activation();
        }
        inner.activation = Some(Activation {
            id,
            pinned: position,
            edge,
            pullback: 0.0,
            pullback_limit: edge
                .zip(zone)
                .map(|(edge, zone)| edge.span(zone) * RELEASE_PULLBACK),
        });
        tracing::debug!(
            ?id,
            x = position.x,
            y = position.y,
            ?edge,
            "input capture activated"
        );
        // The resynchronisation event, and the reason `CapturedEvent` carries a
        // position at all. No motion happened, so the delta is honestly zero; what
        // this says is "the real pointer is *here* now", which is the one moment
        // the engine can learn it. Without it the virtual cursor stays wherever
        // the last activation left it, because nothing is captured in between.
        inner.emit(CapturedEvent::PointerMotion {
            dx: 0.0,
            dy: 0.0,
            position,
        });
    }

    /// The compositor has stopped capturing, by its own decision.
    pub fn deactivated(&self, id: Option<u32>) {
        let mut inner = self.lock();
        if inner.activation.is_some() {
            tracing::debug!(?id, "input capture deactivated by the compositor");
        }
        inner.end_activation();
    }

    pub fn motion(&self, dx: f64, dy: f64) {
        let mut inner = self.lock();
        let Some(activation) = inner.activation.as_mut() else {
            return inner.ignored("pointer motion");
        };
        let position = activation.pinned;
        if let Some(edge) = activation.edge {
            // Only the net pull-back counts: pushing at the barrier and easing off
            // is not the same gesture as giving up and coming back.
            activation.pullback = (activation.pullback + edge.inward(dx, dy)).max(0.0);
        }
        let unclaimed = !inner.wanted;
        inner.emit(CapturedEvent::PointerMotion { dx, dy, position });
        if unclaimed
            && inner
                .activation
                .as_ref()
                .is_some_and(|a| a.pullback_limit.is_some_and(|limit| a.pullback >= limit))
        {
            tracing::debug!(
                "the pointer was pulled back from the barrier with no peer holding the cursor; \
                 handing capture back"
            );
            inner.release();
        }
    }

    /// The compositor reported where the pointer really is.
    ///
    /// Not offered by the alpha target — its capture seat has a relative pointer
    /// and nothing else — but taken when it arrives, because a live absolute
    /// position beats the pinned point from `Activated`: it is the one thing that
    /// would let the engine notice something else moved the real cursor *during*
    /// an activation.
    pub fn absolute(&self, position: Point) {
        let mut inner = self.lock();
        if let Some(activation) = inner.activation.as_mut() {
            activation.pinned = position;
        }
    }

    /// The compositor reported modifier state.
    ///
    /// Never observed from a capturing session on the alpha target, which is why
    /// [`Keyboard`] tracks modifiers from the key stream instead. Taken when it
    /// does arrive, because the compositor knows about keys pressed before capture
    /// started and this side cannot.
    pub fn modifiers(&self, depressed: u32, locked: u32) {
        let mut inner = self.lock();
        if let Some(keyboard) = inner.keyboard.as_mut() {
            keyboard.observe_xkb(depressed, locked);
        }
    }

    pub fn button(&self, code: u32, pressed: bool) {
        let mut inner = self.lock();
        if inner.activation.is_none() {
            return inner.ignored("a button");
        }
        let Some(button) = button_from_evdev(code) else {
            tracing::debug!(
                code,
                "no wire button for this evdev code; not forwarding it"
            );
            return;
        };
        inner.track_button(button, pressed);
        inner.emit(CapturedEvent::Button { button, pressed });
    }

    /// Smooth scrolling, in logical pixels.
    pub fn scroll(&self, dx: f64, dy: f64) {
        self.emit_scroll(dx, dy, ScrollUnit::Pixels);
    }

    /// A notched wheel, in the 120-per-detent units libei uses.
    pub fn scroll_discrete(&self, dx: i32, dy: i32) {
        self.emit_scroll(
            f64::from(dx) / SCROLL_DETENT,
            f64::from(dy) / SCROLL_DETENT,
            ScrollUnit::Lines,
        );
    }

    fn emit_scroll(&self, dx: f64, dy: f64, unit: ScrollUnit) {
        let mut inner = self.lock();
        if inner.activation.is_none() {
            return inner.ignored("a scroll");
        }
        // The wire follows the Windows sense — positive is away from the user —
        // and libei follows Wayland's, where positive is towards it. `inject`
        // negates on the way out for the same reason; sharing the sign would make
        // scrolling backwards on exactly one leg of the round trip.
        inner.emit(CapturedEvent::Scroll { dx, dy: -dy, unit });
    }

    /// One key, as an evdev keycode.
    ///
    /// This is where the cross-layout guarantee is kept: the keycode is resolved
    /// against *this* machine's keymap before it goes anywhere, so `å` on a
    /// Norwegian keyboard crosses the wire as `å` rather than as the position a US
    /// desktop calls `[`.
    pub fn key(&self, keycode: u32, pressed: bool) {
        let mut inner = self.lock();
        if inner.activation.is_none() {
            return inner.ignored("a key");
        }
        let action = if pressed {
            KeyAction::Press
        } else {
            KeyAction::Release
        };
        let raw = inner.resolve_raw(keycode, action);
        for event in inner.resolver.resolve(&raw) {
            inner.emit(CapturedEvent::Key(event));
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // Poisoning means the driver thread panicked mid-event. Refusing to read
        // the state would turn that into a panic in the agent's tick loop, and the
        // worst this can do is deliver one more event than it should.
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Default for CaptureState {
    fn default() -> Self {
        Self::new()
    }
}

impl Inner {
    fn send(&self, command: Command) {
        match &self.control {
            Some(control) => control(command),
            // Before the session exists, or after it has gone. `attach` replays the
            // arming, and there is nothing to release from a session that is not
            // there.
            None => tracing::debug!(?command, "no input-capture session to send this to"),
        }
    }

    fn emit(&mut self, event: CapturedEvent) {
        if let Some(sink) = self.sink.as_mut() {
            sink(event);
        }
    }

    /// An event that arrived outside an activation.
    ///
    /// The compositor delivers a straggler or two either side of the `Activated`
    /// signal, because the D-Bus round trip is slower than the libei socket —
    /// measured at one motion event of about a pixel. Forwarding them would drive
    /// a peer with motion that also moved the local pointer, so they are dropped
    /// rather than attributed to an activation whose pinned position is not known
    /// yet.
    fn ignored(&self, what: &str) {
        tracing::trace!("{what} arrived outside an activation; not forwarding it");
    }

    fn track_button(&mut self, button: MouseButton, pressed: bool) {
        if pressed {
            if !self.buttons.contains(&button) {
                self.buttons.push(button);
            }
        } else {
            self.buttons.retain(|b| *b != button);
        }
    }

    /// Ask the compositor to hand the pointer back.
    fn release(&mut self) {
        let Some(activation) = self.activation.as_ref() else {
            return;
        };
        let command = Command::Release {
            activation_id: activation.id,
            // Where the pointer should reappear. The pinned point is on the
            // barrier, and asking for it there would put the cursor straight back
            // into the barrier it just left — so it is nudged clear of the edge it
            // came in through.
            position: Some(activation.released_at()),
        };
        self.end_activation();
        self.send(command);
    }

    /// End the current activation, releasing anything held.
    ///
    /// Both halves matter. GNOME does not send `Deactivated` for a release we
    /// asked for, so our own decision is what ends the activation locally; and a
    /// key or button still down when it ends has to go up, or it is stranded on
    /// whichever machine owned the cursor.
    fn end_activation(&mut self) {
        if self.activation.take().is_none() {
            return;
        }
        for button in std::mem::take(&mut self.buttons).into_iter().rev() {
            self.emit(CapturedEvent::Button {
                button,
                pressed: false,
            });
        }
        for event in self.resolver.release_held() {
            self.emit(CapturedEvent::Key(event));
        }
        if let Some(keyboard) = self.keyboard.as_mut() {
            keyboard.forget_held();
        }
    }

    /// Turn an evdev keycode into the [`RawKey`] the resolver wants.
    fn resolve_raw(&mut self, keycode: u32, action: KeyAction) -> RawKey {
        let Some(keyboard) = self.keyboard.as_mut() else {
            // No keymap: the session came up without a keyboard device, or its
            // keymap was unreadable. Forwarding the raw code beats dropping the
            // keystroke — some receivers honour it, and a key that silently
            // vanishes is indistinguishable from a network fault.
            return RawKey::unmapped(keycode, action, Modifiers::NONE);
        };
        keyboard.raw(keycode, action)
    }
}

impl Activation {
    /// Where to ask for the pointer back.
    ///
    /// Clear of the barrier it crossed. Handing it back exactly on the line the
    /// compositor pinned it to re-triggers the barrier on the next twitch, which
    /// reads as a pointer that will not leave the edge of the screen.
    fn released_at(&self) -> (f64, f64) {
        let step = RELEASE_CLEARANCE;
        let (dx, dy) = match self.edge {
            Some(BarrierEdge::Left) => (step, 0.0),
            Some(BarrierEdge::Right) => (-step, 0.0),
            Some(BarrierEdge::Top) => (0.0, step),
            Some(BarrierEdge::Bottom) => (0.0, -step),
            None => (0.0, 0.0),
        };
        (self.pinned.x + dx, self.pinned.y + dy)
    }
}

/// This machine's keyboard, as capture has to model it.
///
/// Two things live here that no single event carries: which modifiers are down,
/// and what the layout says each keycode means. Both are needed to answer the only
/// question that matters — what did the user just type — and both are this side's
/// responsibility because the compositor does not report modifier state to a
/// capturing client.
struct Keyboard {
    keymap: Keymap,
    /// Modifier keycodes currently down, in press order.
    held: Vec<u32>,
    /// Caps Lock and Num Lock, which are toggles rather than held keys.
    caps: bool,
    num: bool,
}

impl Keyboard {
    fn new(keymap: Keymap) -> Self {
        Self {
            keymap,
            held: Vec::new(),
            caps: false,
            num: false,
        }
    }

    /// Forget which modifiers are down.
    ///
    /// Called when an activation ends: the user goes on using their keyboard with
    /// capture off, and a Shift believed held from last time would shift the first
    /// character of the next crossing. The lock states are *not* cleared — those
    /// are the desktop's, and they do not change because we stopped watching.
    fn forget_held(&mut self) {
        self.held.clear();
    }

    /// What the layout says this keycode is, where what it says is a modifier.
    ///
    /// The one place the layout outranks the evdev table. `lv3:menu_switch` puts
    /// `ISO_Level3_Shift` on Menu and `ctrl:nocaps` puts `Control_L` on Caps Lock:
    /// both keycodes are in [`special_from_evdev`], so without this a peer is sent
    /// a context-menu key on every AltGr press, and a Caps Lock press that also
    /// flips the level every following letter resolves through.
    ///
    /// Asked at the base level, because a modifier's own meaning does not depend on
    /// which modifiers are already down — and because this is answered before the
    /// held set is updated.
    fn layout_modifier(&self, keycode: u32) -> Option<SpecialKey> {
        self.keymap
            .keysym(keycode, LevelMods::default(), false)
            .and_then(special_from_keysym)
    }

    fn raw(&mut self, keycode: u32, action: KeyAction) -> RawKey {
        let special = self
            .layout_modifier(keycode)
            .or_else(|| special_from_evdev(keycode));
        let pressed = action != KeyAction::Release;

        // State first, so a modifier's own event carries its bit — which is what
        // the router's held-key bookkeeping expects, and what every other backend
        // reports. Read off the *resolved* key, so a Caps Lock the layout has made
        // into a Control does not toggle a lock nobody pressed.
        match special {
            Some(SpecialKey::CapsLock) if pressed => self.caps = !self.caps,
            Some(SpecialKey::NumLock) if pressed => self.num = !self.num,
            _ if is_level_modifier(keycode)
                || is_chord_modifier(keycode)
                || self.keymap.is_level3(keycode) =>
            {
                self.hold(keycode, pressed);
            }
            _ => {}
        }

        let modifiers = self.modifiers();

        // A semantic key beats whatever *text* the layout would call it: a receiver
        // injecting `\t` as a character does not move focus, and `\r` does not
        // press a button. `RawKey` states the same precedence. The narrower rule
        // above is the exception, and only where the layout itself says the key is
        // a modifier rather than something to type.
        if let Some(key) = special {
            return RawKey::special(keycode, key, action, modifiers);
        }

        let level = LevelMods {
            shift: self.holding(KEY_LEFTSHIFT) || self.holding(SHIFT_RIGHT),
            level3: self.holding_level3(),
        };
        match self.keymap.keysym(keycode, level, self.caps) {
            Some(keysym) => {
                // A modifier the layout put somewhere unusual is still a modifier,
                // and the raw-keycode escape below is for content keys: sending
                // this position to a peer would press whatever its own keyboard has
                // there.
                if let Some(key) = special_from_keysym(keysym) {
                    return RawKey::special(keycode, key, action, modifiers);
                }
                if let Some(accent) = dead_accent(keysym) {
                    return RawKey::dead(keycode, accent, action, modifiers);
                }
                match char_for_keysym(keysym) {
                    Some(c) => RawKey::text(keycode, c.to_string(), action, modifiers),
                    // A keysym with no character: a function key the layout puts
                    // somewhere unusual, an XF86 key, a compose key. Nothing to
                    // resolve, so the raw code goes up and the receiver decides.
                    None => RawKey::unmapped(keycode, action, modifiers),
                }
            }
            None => RawKey::unmapped(keycode, action, modifiers),
        }
    }

    /// Take the compositor's own modifier state, where it offers one.
    ///
    /// Only the *level* modifiers and the locks are adopted, and only what this
    /// side cannot otherwise know: which Shift or AltGr key is down is tracked
    /// from the key stream, because the mask says a shift is held without saying
    /// which one, and a release has to match its press.
    fn observe_xkb(&mut self, depressed: u32, locked: u32) {
        self.caps = locked & XKB_LOCK != 0;
        self.num = locked & XKB_NUM != 0;
        // A shift the mask reports that no captured press accounts for was pressed
        // before capture started. Recording it under the left-hand keycode is the
        // same arbitrary-but-harmless choice the router makes for a synthesised
        // release: what matters is that the level comes out right.
        if depressed & XKB_SHIFT != 0 && !self.holding(KEY_LEFTSHIFT) && !self.holding(SHIFT_RIGHT)
        {
            self.hold(KEY_LEFTSHIFT, true);
        }
    }

    fn hold(&mut self, keycode: u32, pressed: bool) {
        if pressed {
            if !self.held.contains(&keycode) {
                self.held.push(keycode);
            }
        } else {
            self.held.retain(|k| *k != keycode);
        }
    }

    fn holding(&self, keycode: u32) -> bool {
        self.held.contains(&keycode)
    }

    /// Whether a key acting as this layout's level-3 shift is down.
    ///
    /// Asked of the keymap rather than assumed to be right Alt: a layout is free
    /// to put `ISO_Level3_Shift` elsewhere, and on a layout that has none, right
    /// Alt is a plain Alt and must not shift a level.
    fn holding_level3(&self) -> bool {
        self.held.iter().any(|k| self.keymap.is_level3(*k))
    }

    /// The modifier snapshot every captured key event carries.
    fn modifiers(&self) -> Modifiers {
        let mut mods = Modifiers::NONE;
        for keycode in &self.held {
            let bit = match *keycode {
                KEY_LEFTSHIFT | SHIFT_RIGHT => Modifiers::SHIFT,
                KEY_LEFTCTRL | CTRL_RIGHT => Modifiers::CTRL,
                KEY_LEFTALT => Modifiers::ALT,
                // Right Alt is both, on a layout that makes it AltGr. The router
                // relies on that pairing: releasing the key clears both bits, so
                // neither is released a second time.
                KEY_RIGHTALT if self.keymap.is_level3(KEY_RIGHTALT) => {
                    Modifiers::ALT.union(Modifiers::ALT_GR)
                }
                KEY_RIGHTALT => Modifiers::ALT,
                KEY_LEFTMETA | SUPER_RIGHT => Modifiers::SUPER,
                other if self.keymap.is_level3(other) => Modifiers::ALT_GR,
                _ => Modifiers::NONE,
            };
            mods = mods.union(bit);
        }
        if self.caps {
            mods = mods.union(Modifiers::CAPS_LOCK);
        }
        if self.num {
            mods = mods.union(Modifiers::NUM_LOCK);
        }
        mods
    }
}

/// The xkb real-modifier bits, in the order every keymap `libxkbcommon` compiles
/// puts them: `Shift`, `Lock`, `Control`, `Mod1`..`Mod5`.
///
/// Only the three that change what a key produces are named. `inject` reads
/// `XKB_LOCK` out of the same mask for the same reason.
const XKB_SHIFT: u32 = 1 << 0;
const XKB_LOCK: u32 = 1 << 1;
const XKB_NUM: u32 = 1 << 4;

/// `KEY_RIGHTSHIFT`, `KEY_RIGHTCTRL`, `KEY_RIGHTMETA`.
///
/// The left-hand codes are named in [`super::keys`] because injection presses
/// them; these three only ever arrive from capture, so they live here.
const SHIFT_RIGHT: u32 = 54;
const CTRL_RIGHT: u32 = 97;
const SUPER_RIGHT: u32 = 126;

/// Whether a keycode can change which level of a key is produced, whatever the
/// layout says.
///
/// Only Shift is really fixed; right Alt is here because it is where a level-3
/// shift usually sits, and holding it costs nothing on a layout that makes it a
/// plain Alt. Which key *is* this layout's level-3 shift is the keymap's answer,
/// and [`Keyboard::raw`] asks it directly — measured on the alpha target, mutter's
/// own US keymap puts `ISO_Level3_Shift` on keycode 84, not on right Alt, so a
/// hardcoded set alone leaves every level-3 character resolving to its base level.
fn is_level_modifier(keycode: u32) -> bool {
    matches!(keycode, KEY_LEFTSHIFT | SHIFT_RIGHT | KEY_RIGHTALT)
}

/// Whether a keycode contributes a modifier bit to the wire event.
fn is_chord_modifier(keycode: u32) -> bool {
    matches!(
        keycode,
        KEY_LEFTCTRL | CTRL_RIGHT | KEY_LEFTALT | KEY_LEFTMETA | SUPER_RIGHT
    )
}

/// The error a caller gets when there is no capture session to talk to.
pub fn no_session() -> PlatformError {
    PlatformError::Unsupported {
        operation: "the xdg-desktop-portal InputCapture session",
        backend: BACKEND,
    }
}

#[cfg(test)]
#[path = "capture_tests.rs"]
mod tests;
