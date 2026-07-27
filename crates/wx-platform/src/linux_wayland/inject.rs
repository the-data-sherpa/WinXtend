//! Input injection over the portal session's libei transport.
//!
//! # The one rule that has no error to report
//!
//! **Every event must be followed by `ei_device.frame`.** libei buffers requests
//! until a frame commits them, so a batch without one is not rejected, not
//! logged and not delivered — injection simply does nothing, on a connection that
//! looks perfectly healthy. Every send path here goes through [`Wired::commit`],
//! which frames and then flushes, so there is one place to get it right.
//!
//! # Where the devices come from
//!
//! Not from here. [`super::driver`] owns the `ei` event loop and hands the devices
//! the compositor offers to a [`Transport`], which is shared with the injector
//! behind a mutex. `reis` proxies are `Send`/`Sync` — the wire buffer is
//! mutex-protected inside the library — so requests can be written straight from
//! whichever thread the agent calls [`crate::traits::InputInjector`] on, with no
//! channel and no hop through the driver thread.
//!
//! Three devices arrive on the alpha target: a relative virtual pointer, a virtual
//! keyboard carrying an xkb keymap, and a shared virtual absolute pointer whose
//! regions match the monitors. Buttons and scrolling are offered by both pointers;
//! they are sent on the absolute one, because that is the device this backend
//! positions with and a click should be unambiguous about where it landed.
//!
//! # What is held, and why it is tracked rather than read back
//!
//! A dropped release strands Ctrl or a mouse button down on the receiving desktop,
//! and the user cannot clear it without physically pressing that key on that
//! machine. So [`Injector`] records exactly what it pressed and
//! [`Injector::release_all`] unwinds precisely that — never a blanket release,
//! which would fight the physical keyboard someone may be using at the same time.
//!
//! Bookkeeping is committed only *after* the flush that carries the event, for the
//! same reason the Windows backend does it (see `crate::windows::inject`): a
//! release that never reached the compositor must stay on the held list so a later
//! `release_all` tries again.

use std::sync::{Arc, Mutex, MutexGuard};

use reis::ei;
use reis::event::{Connection, Device, DeviceCapability};

use wx_proto::{
    InputEvent, KeyAction, KeyEvent, KeyPayload, Modifiers, Monitor, MouseButton, NormPos,
    PointerEvent, ScrollUnit,
};

use super::keymap::{Keymap, LevelMods, Stroke};
use super::keys::{
    self, char_keysyms, evdev_from_button, evdev_from_special, KEY_LEFTALT, KEY_LEFTCTRL,
    KEY_LEFTMETA, KEY_LEFTSHIFT, KEY_RIGHTALT,
};
use super::BACKEND;
use crate::coords::local_point;
use crate::error::{PlatformError, Result};

/// One notch of a notched wheel, in the units `ei_scroll.scroll_discrete` wants.
const SCROLL_DETENT: f64 = 120.0;

/// The xkb `Lock` modifier — Caps Lock — in the mask `ei_keyboard.modifiers`
/// reports.
const XKB_LOCK: u32 = 1 << 1;

/// The libei devices for one live session.
///
/// Empty until the portal session comes up, and empty again the moment it goes
/// away. Sharing one mutex across the whole set rather than one per device keeps
/// "the transport is live" a single observation: an injector must not find the
/// keyboard present and the pointer gone halfway through an event.
#[derive(Default)]
pub struct Transport {
    wired: Mutex<Option<Wired>>,
}

struct Wired {
    connection: Connection,
    keyboard: Option<VirtualKeyboard>,
    /// Relative motion.
    pointer: Option<Device>,
    /// Absolute motion, and the device buttons and scrolling are sent on.
    absolute: Option<Device>,
    /// Devices the compositor has paused.
    ///
    /// A pause is temporary and routine — mutter pauses the devices around
    /// session suspension — so a paused device keeps its slot and only loses the
    /// right to be sent on, which is what the accessors below enforce. Dropping
    /// it instead would be unrecoverable: the keymap arrives exactly once, on a
    /// file descriptor, and the layout group and locked modifiers reported since
    /// are state no later event repeats.
    paused: Vec<Device>,
    /// Incremented for each `start_emulating`, as the protocol asks.
    sequence: u32,
}

struct VirtualKeyboard {
    device: Device,
    keyboard: ei::Keyboard,
    /// Kept so a layout-group change can be re-resolved without waiting for a new
    /// device: switching input source is something a user does mid-session and it
    /// changes every answer this module gives.
    source: String,
    keymap: Keymap,
    group: u32,
    /// Modifiers the *desktop* has locked, which this injector did not set and
    /// must not clear. Only Caps Lock matters, and only because it moves letters
    /// to the other level.
    locked: u32,
}

impl VirtualKeyboard {
    /// Turn a keymap lookup into something this backend can actually press.
    ///
    /// Two adjustments, both about state that belongs to the desktop rather than
    /// to us. Caps Lock is a *locked* modifier this injector did not set and must
    /// not clear, so the level is worked around it: pressing `a` with Caps Lock on
    /// produces `A` unless the shift is inverted. And a level-3 character on a
    /// layout that defines no AltGr key is refused rather than attempted, because
    /// pressing right Alt on the chance it works turns the keystroke into an Alt
    /// chord and opens a menu.
    fn reachable(&self, stroke: Stroke) -> Result<ResolvedKey> {
        let mut level = stroke.mods;
        if stroke.alphabetic && self.locked & XKB_LOCK != 0 {
            level.shift = !level.shift;
        }
        if level.level3 && self.keymap.level3_keycode().is_none() {
            return Err(PlatformError::Unsupported {
                operation: "injecting a character whose layout level has no modifier key",
                backend: BACKEND,
            });
        }
        Ok(ResolvedKey {
            keycode: stroke.keycode,
            level,
        })
    }
}

impl Transport {
    pub fn new() -> Self {
        Self::default()
    }

    /// The `ei` connection is up; devices will follow.
    pub fn attach(&self, connection: Connection) {
        *self.lock() = Some(Wired {
            connection,
            keyboard: None,
            pointer: None,
            absolute: None,
            paused: Vec::new(),
            sequence: 0,
        });
    }

    /// Take note of a device the compositor has offered.
    pub fn device_added(&self, device: &Device) {
        let mut guard = self.lock();
        let Some(wired) = guard.as_mut() else { return };
        wired.register(device);
    }

    /// The device is ready to emulate. Nothing may be sent before this.
    ///
    /// Not only at the start of a session: the compositor pauses and resumes
    /// devices over the life of one, so a resume has to put the device back in
    /// service as well as start it emulating. Leaving that to `DeviceAdded`
    /// alone would mean one routine suspend killed injection for the rest of the
    /// process while the session still advertised it to peers.
    pub fn device_resumed(&self, device: &Device) {
        let mut guard = self.lock();
        let Some(wired) = guard.as_mut() else { return };
        wired.paused.retain(|d| d != device);
        wired.register(device);
        wired.sequence = wired.sequence.wrapping_add(1);
        let serial = wired.connection.serial();
        device.device().start_emulating(serial, wired.sequence);
        if let Err(e) = wired.connection.flush() {
            tracing::warn!(error = %e, "could not start emulating on a libei device");
        }
    }

    /// The device is suspended. Nothing may be sent on it until it resumes.
    ///
    /// It keeps its slot, because the compositor may hand it straight back; see
    /// [`Wired::paused`].
    pub fn device_paused(&self, device: &Device) {
        let mut guard = self.lock();
        let Some(wired) = guard.as_mut() else { return };
        if !wired.is_paused(device) {
            wired.paused.push(device.clone());
        }
    }

    /// The device is gone for good and nothing may be sent on it again.
    pub fn device_lost(&self, device: &Device) {
        let mut guard = self.lock();
        let Some(wired) = guard.as_mut() else { return };
        if wired.keyboard.as_ref().is_some_and(|k| &k.device == device) {
            wired.keyboard = None;
        }
        if wired.pointer.as_ref() == Some(device) {
            wired.pointer = None;
        }
        if wired.absolute.as_ref() == Some(device) {
            wired.absolute = None;
        }
        wired.paused.retain(|d| d != device);
    }

    /// The compositor's view of the keyboard's modifier state.
    ///
    /// Two things here are load-bearing. The layout **group** decides which of a
    /// multi-layout keymap a keycode will be read through, so a user switching
    /// input source silently invalidates every lookup unless the keymap is
    /// re-resolved. And **Caps Lock** shifts letters to the other level, so
    /// injecting `a` while it is on produces `A` unless the shift is inverted.
    pub fn modifiers(&self, device: &Device, locked: u32, group: u32) {
        let mut guard = self.lock();
        let Some(keyboard) = guard.as_mut().and_then(|w| w.keyboard.as_mut()) else {
            return;
        };
        if &keyboard.device != device {
            return;
        }
        keyboard.locked = locked;
        if keyboard.group != group {
            tracing::debug!(
                group,
                "the keyboard layout group changed; re-reading the keymap"
            );
            keyboard.keymap = Keymap::parse(&keyboard.source, group as usize);
            keyboard.group = group;
        }
    }

    /// The session is over.
    ///
    /// `stop_emulating` is sent first so the compositor is not left believing this
    /// client might still be mid-gesture.
    pub fn detach(&self) {
        let mut guard = self.lock();
        let Some(wired) = guard.take() else { return };
        let serial = wired.connection.serial();
        for device in wired.devices() {
            device.device().stop_emulating(serial);
        }
        let _ = wired.connection.flush();
    }

    fn lock(&self) -> MutexGuard<'_, Option<Wired>> {
        // A panicking driver thread leaves the session unusable either way;
        // refusing to read the transport would turn that into a second panic in
        // the agent's input path.
        self.wired
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Whether a live transport exists at all.
    fn is_live(&self) -> bool {
        self.lock().is_some()
    }
}

impl Wired {
    /// Put a device into whichever slots its capabilities claim.
    ///
    /// Runs again on every resume, so it has to be idempotent in the one way that
    /// matters: a keyboard that is already registered keeps the instance it has
    /// rather than being rebuilt, or the layout group and locked modifiers
    /// reported since it arrived would silently reset to zero — and the keymap
    /// would be re-read from a file descriptor that has already been consumed.
    fn register(&mut self, device: &Device) {
        let known = self.keyboard.as_ref().is_some_and(|k| &k.device == device);
        if device.has_capability(DeviceCapability::Keyboard) && !known {
            if let Some(keyboard) = device.interface::<ei::Keyboard>() {
                let source = read_keymap(device);
                let keymap = Keymap::parse(&source, 0);
                if keymap.is_empty() {
                    // Not fatal — special keys and chords use fixed evdev codes and
                    // work regardless — but every text payload will be refused, so
                    // it needs to be attributable.
                    tracing::warn!(
                        device = ?device.name(),
                        "the libei keyboard offered no keymap this backend could read; text injection will be unavailable"
                    );
                }
                self.keyboard = Some(VirtualKeyboard {
                    device: device.clone(),
                    keyboard,
                    source,
                    keymap,
                    group: 0,
                    locked: 0,
                });
            }
        }
        // Two independent questions, not two branches of one. On the alpha target
        // the capabilities land on separate devices, but a compositor is free to
        // offer one device that does both — and an `else if` there would leave
        // relative motion with nowhere to go while an absolute pointer sat right
        // in front of it.
        if device.has_capability(DeviceCapability::PointerAbsolute) {
            self.absolute = Some(device.clone());
        }
        if device.has_capability(DeviceCapability::Pointer) {
            self.pointer = Some(device.clone());
        }
    }

    fn is_paused(&self, device: &Device) -> bool {
        self.paused.iter().any(|d| d == device)
    }

    /// The keyboard, if there is one and it may be sent on.
    fn keyboard(&self) -> Option<&VirtualKeyboard> {
        self.keyboard
            .as_ref()
            .filter(|k| !self.is_paused(&k.device))
    }

    fn pointer(&self) -> Option<&Device> {
        self.pointer.as_ref().filter(|d| !self.is_paused(d))
    }

    fn absolute(&self) -> Option<&Device> {
        self.absolute.as_ref().filter(|d| !self.is_paused(d))
    }

    /// Every device that is currently emulating, each one once.
    ///
    /// Deduplicated by identity because one device can fill two slots: a
    /// compositor is free to offer a single pointer carrying both `Pointer` and
    /// `PointerAbsolute`, and `stop_emulating` twice on it is a request the peer
    /// has every reason to read as a client bug.
    fn devices(&self) -> Vec<&Device> {
        let mut out: Vec<&Device> = Vec::new();
        for device in self
            .keyboard()
            .map(|k| &k.device)
            .into_iter()
            .chain(self.pointer())
            .chain(self.absolute())
        {
            if !out.contains(&device) {
                out.push(device);
            }
        }
        out
    }

    /// Frame the events just queued on `device` and put them on the wire.
    ///
    /// The frame is the entire reason this helper exists: without it the requests
    /// sit in libei's buffer forever and injection is a silent no-op.
    fn commit(&self, device: &Device) -> Result<()> {
        device.device().frame(self.connection.serial(), now_us());
        self.connection.flush().map_err(|e| PlatformError::Os {
            context: "flushing the libei transport",
            code: e.raw_os_error(),
        })
    }

    /// The device buttons and scrolling go on: the one this backend positions
    /// with, so a click is unambiguous about where it landed.
    fn buttons(&self) -> Option<&Device> {
        self.absolute()
            .filter(|d| d.has_capability(DeviceCapability::Button))
            .or(self.pointer())
    }

    fn scrolling(&self) -> Option<&Device> {
        self.absolute()
            .filter(|d| d.has_capability(DeviceCapability::Scroll))
            .or(self.pointer())
    }
}

/// Something this injector pressed and is therefore responsible for releasing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Held {
    Key(u32),
    Button(u32),
}

/// One modifier key a keystroke asks to change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ModifierStep {
    keycode: u32,
    release: bool,
    /// Whether the key itself has to move, or only the bookkeeping.
    ///
    /// False when another record is already holding the same physical key down:
    /// the record that stops wanting it forgets it, but must not pull it out
    /// from under the others.
    physical: bool,
}

/// The receiving half of the input path on Wayland.
pub struct Injector {
    transport: Arc<Transport>,
    /// Press-ordered, so `release_all` unwinds newest first.
    held: Vec<Held>,
    /// Modifiers this injector is holding because the sender asked for them.
    ///
    /// Tracked rather than queried, because the user may be holding the same
    /// modifier on the local keyboard and releasing theirs would be a visible
    /// malfunction on their own machine.
    chord: Modifiers,
    /// Shift and AltGr held to reach a keymap level. Kept apart from `chord`
    /// because they are consumed by layout resolution rather than requested by
    /// the sender — but they drive the same two physical keys, so neither set
    /// moves one without asking the other (see [`Injector::chord_plan`]).
    level: LevelMods,
}

impl Injector {
    pub fn new(transport: Arc<Transport>) -> Self {
        Self {
            transport,
            held: Vec::new(),
            chord: Modifiers::NONE,
            level: LevelMods::default(),
        }
    }

    pub fn inject(&mut self, monitor: &Monitor, event: &InputEvent) -> Result<()> {
        match event {
            InputEvent::Pointer(p) => self.inject_pointer(monitor, p),
            InputEvent::Key(k) => self.inject_key(k),
            // The wire-level handoff: everything held has to come up, or the peer
            // is left with a stuck modifier it cannot clear locally.
            InputEvent::ReleaseControl => self.release_all(),
        }
    }

    pub fn warp_cursor(&mut self, monitor: &Monitor, pos: NormPos) -> Result<()> {
        self.move_absolute(monitor, pos)
    }

    pub fn release_all(&mut self) -> Result<()> {
        if self.held.is_empty() && self.chord.is_empty() && self.level == LevelMods::default() {
            return Ok(());
        }
        // A session that has already gone away has taken its virtual devices with
        // it, so there is nothing left holding anything down and nothing to send
        // the releases over. Forgetting is then correct; keeping the list would
        // strand it for the life of the process.
        if !self.transport.is_live() {
            self.forget();
            return Ok(());
        }

        let guard = self.transport.lock();
        let wired = guard.as_ref().ok_or_else(transport_gone)?;

        for held in self.held.iter().rev() {
            match held {
                Held::Key(code) => {
                    let Some(keyboard) = wired.keyboard() else {
                        continue;
                    };
                    keyboard
                        .keyboard
                        .key(*code, ei::keyboard::KeyState::Released);
                    wired.commit(&keyboard.device)?;
                }
                Held::Button(code) => {
                    let Some(device) = wired.buttons() else {
                        continue;
                    };
                    let Some(button) = device.interface::<ei::Button>() else {
                        continue;
                    };
                    button.button(*code, ei::button::ButtonState::Released);
                    wired.commit(device)?;
                }
            }
        }
        // Modifiers last: letting Ctrl go first would turn the remaining releases
        // into bare keys mid-chord, which some applications act on.
        if let Some(keyboard) = wired.keyboard() {
            let level3 = keyboard.keymap.level3_keycode().unwrap_or(KEY_RIGHTALT);
            for code in self.modifier_keycodes(level3) {
                keyboard
                    .keyboard
                    .key(code, ei::keyboard::KeyState::Released);
                wired.commit(&keyboard.device)?;
            }
        }

        drop(guard);
        // Only after every release is on the wire. A flush that failed halfway
        // leaves the rest on the list for the next attempt rather than pretending
        // the desktop is clear.
        self.forget();
        Ok(())
    }

    /// Every modifier keycode currently believed held, in release order.
    fn modifier_keycodes(&self, level3: u32) -> Vec<u32> {
        let mut codes = Vec::new();
        for (held, code) in [
            (self.level.level3, level3),
            (self.level.shift, KEY_LEFTSHIFT),
            (self.chord.contains(Modifiers::SHIFT), KEY_LEFTSHIFT),
            (self.chord.contains(Modifiers::ALT_GR), level3),
            (self.chord.contains(Modifiers::SUPER), KEY_LEFTMETA),
            (self.chord.contains(Modifiers::ALT), KEY_LEFTALT),
            (self.chord.contains(Modifiers::CTRL), KEY_LEFTCTRL),
        ] {
            // Once each, and not at all for a key the loop above has already
            // released: the records share their keycodes with each other and with
            // a modifier the sender forwarded, but there is only ever one key
            // down to let go of.
            if held && !codes.contains(&code) && !self.sender_holds(code) {
                codes.push(code);
            }
        }
        codes
    }

    fn forget(&mut self) {
        self.held.clear();
        self.chord = Modifiers::NONE;
        self.level = LevelMods::default();
    }

    fn inject_pointer(&mut self, monitor: &Monitor, event: &PointerEvent) -> Result<()> {
        match *event {
            PointerEvent::MoveTo { pos } => self.move_absolute(monitor, pos),
            PointerEvent::MoveBy { dx, dy } => {
                // Non-finite deltas off the wire would reach the compositor as NaN
                // and park the cursor somewhere unrecoverable.
                if !dx.is_finite() || !dy.is_finite() || (dx == 0.0 && dy == 0.0) {
                    return Ok(());
                }
                let guard = self.transport.lock();
                let wired = guard.as_ref().ok_or_else(transport_gone)?;
                let device = wired.pointer().ok_or_else(|| no_device("a pointer"))?;
                let pointer = device
                    .interface::<ei::Pointer>()
                    .ok_or_else(|| no_device("relative pointer motion"))?;
                pointer.motion_relative(dx as f32, dy as f32);
                wired.commit(device)
            }
            PointerEvent::Button { button, pressed } => self.inject_button(button, pressed),
            PointerEvent::Scroll { dx, dy, unit } => self.scroll(dx, dy, unit),
        }
    }

    fn move_absolute(&mut self, monitor: &Monitor, pos: NormPos) -> Result<()> {
        let point = local_point(monitor, pos);
        let guard = self.transport.lock();
        let wired = guard.as_ref().ok_or_else(transport_gone)?;
        let device = wired
            .absolute()
            .ok_or_else(|| no_device("an absolute pointer"))?;
        let absolute = device
            .interface::<ei::PointerAbsolute>()
            .ok_or_else(|| no_device("absolute pointer motion"))?;

        // A position outside every region is discarded by the compositor without
        // a word, so it is caught here instead: it means the monitor this event
        // was addressed to is not one the portal session covers, which is a real
        // fault rather than a stray pixel.
        if !device.regions().is_empty()
            && !device.regions().iter().any(|r| {
                let (x, y) = (point.x, point.y);
                x >= f64::from(r.x)
                    && y >= f64::from(r.y)
                    && x < f64::from(r.x) + f64::from(r.width)
                    && y < f64::from(r.y) + f64::from(r.height)
            })
        {
            return Err(PlatformError::Other(format!(
                "the portal session covers no screen containing ({:.0}, {:.0}); \
                 the monitor this position was addressed to is not shared",
                point.x, point.y
            )));
        }

        absolute.motion_absolute(point.x as f32, point.y as f32);
        wired.commit(device)
    }

    fn inject_button(&mut self, button: MouseButton, pressed: bool) -> Result<()> {
        let code = evdev_from_button(button).ok_or(PlatformError::Unsupported {
            operation: "this mouse button",
            backend: BACKEND,
        })?;
        {
            let guard = self.transport.lock();
            let wired = guard.as_ref().ok_or_else(transport_gone)?;
            let device = wired.buttons().ok_or_else(|| no_device("a mouse button"))?;
            let interface = device
                .interface::<ei::Button>()
                .ok_or_else(|| no_device("mouse buttons"))?;
            interface.button(
                code,
                if pressed {
                    ei::button::ButtonState::Press
                } else {
                    ei::button::ButtonState::Released
                },
            );
            wired.commit(device)?;
        }
        // Recorded only once the compositor has it. A button-up that never left
        // this process must stay on the list, because a mouse button left down
        // drags whatever is under it.
        self.track(Held::Button(code), !pressed);
        Ok(())
    }

    fn scroll(&mut self, dx: f64, dy: f64, unit: ScrollUnit) -> Result<()> {
        let clean = |v: f64| if v.is_finite() { v } else { 0.0 };
        // Wire scroll follows the Windows sense — positive is away from the user —
        // and libei follows Wayland's, where positive is towards it. Sharing the
        // sign would invert the wheel on every Linux receiver, which reads as
        // "scrolling is backwards on that machine" and is easy to blame on a
        // setting.
        let (x, y) = (clean(dx), -clean(dy));
        // Decided before the transport is consulted, so a scroll that amounts to
        // nothing is not reported as a session failure.
        if x == 0.0 && y == 0.0 {
            return Ok(());
        }

        let guard = self.transport.lock();
        let wired = guard.as_ref().ok_or_else(transport_gone)?;
        let device = wired.scrolling().ok_or_else(|| no_device("scrolling"))?;
        let scroll = device
            .interface::<ei::Scroll>()
            .ok_or_else(|| no_device("scrolling"))?;

        match unit {
            // A notched wheel is discrete, and only the discrete request tells an
            // application that a detent happened — which is what paging and
            // ctrl-zoom act on.
            //
            // Deliberately *not* accompanied by a smooth `scroll` for the same
            // motion, even though a physical wheel reports both. Measured on the
            // alpha target: mutter turns the two requests into two separate scroll
            // events, so the application scrolls once for the detent and again for
            // the pixels — one notch moving two and a half lines. The compositor
            // already synthesises the smooth value from the detent.
            ScrollUnit::Lines => scroll.scroll_discrete(
                (x * SCROLL_DETENT).round() as i32,
                (y * SCROLL_DETENT).round() as i32,
            ),
            ScrollUnit::Pixels => scroll.scroll(x as f32, y as f32),
        }
        wired.commit(device)
    }

    fn inject_key(&mut self, event: &KeyEvent) -> Result<()> {
        let release = event.action == KeyAction::Release;
        match &event.payload {
            KeyPayload::Special(key) => {
                let code = evdev_from_special(*key).ok_or(PlatformError::Unsupported {
                    operation: "injecting this key",
                    backend: BACKEND,
                })?;
                // Bare modifier events pass through untouched: an application can
                // legitimately observe "Ctrl went down" with no other key
                // involved, and re-deriving it from the modifier bits would lose
                // the left/right distinction the sender took care to keep.
                if !key.is_modifier() {
                    self.sync_chord(event.injection_modifiers())?;
                    // A Shift or AltGr still held to reach some earlier
                    // character's level is not part of this key, and leaving it
                    // down turns Enter into AltGr+Enter.
                    self.sync_level(LevelMods::default())?;
                }
                self.send_key(code, release)
            }
            KeyPayload::Text(text) => self.inject_text(text, event, release),
            // The sender could not resolve this key through its own layout, so
            // what arrives is a keycode from a platform whose numbering may not be
            // ours. Honouring it is still better than dropping the keystroke —
            // the same call the Windows backend makes — but it is worth a line in
            // the log when a key turns out to do something unexpected.
            KeyPayload::RawKeyCode(code) => {
                tracing::debug!(
                    code,
                    "injecting an unresolved raw keycode; the sender's numbering may not be evdev"
                );
                self.sync_chord(event.injection_modifiers())?;
                self.sync_level(LevelMods::default())?;
                self.send_key(*code, release)
            }
        }
    }

    /// Type text by finding it on the receiver's own layout.
    ///
    /// This is where the cross-layout promise is kept and where its Wayland limit
    /// shows: see the module docs on [`super::keymap`] for why there is no way to
    /// produce a character the loaded layout does not have.
    fn inject_text(&mut self, text: &str, event: &KeyEvent, release: bool) -> Result<()> {
        let mut chars = text.chars();
        let (Some(first), rest) = (chars.next(), chars.as_str()) else {
            // An empty payload is not a keystroke. Nothing to press, nothing to
            // fail about.
            return Ok(());
        };

        if rest.is_empty() {
            let strokes = self.resolve(first)?;
            self.sync_chord(event.injection_modifiers())?;
            // One key means the press and release can be paired the way the sender
            // sent them, so a held letter repeats on the receiver as it would
            // locally.
            if let [only] = strokes[..] {
                self.sync_level(only.level)?;
                self.send_key(only.keycode, release)?;
                // The level modifiers belong to this keystroke and nothing after
                // it. Held past the key-up they reach the next event — the
                // pointer click or the Enter that follows a level-3 character —
                // and the peer sees a modifier it never sent. A Shift the sender
                // is holding itself is a different record and outlives this: the
                // plan leaves that key alone.
                if release {
                    self.sync_level(LevelMods::default())?;
                }
                return Ok(());
            }
            // A dead-key sequence has no single key to hold: the accent has to be
            // pressed and let go before the base letter means anything. So the
            // whole thing is typed on the press and the release has nothing left
            // to do.
            if release {
                return Ok(());
            }
            return self.tap(&strokes);
        }

        // Several codepoints in one event: IME output, an emoji with a modifier,
        // or a composed sequence the sender flushed as one payload. Same reasoning
        // as above — nothing here is a single held key.
        if release {
            return Ok(());
        }
        self.sync_chord(event.injection_modifiers())?;
        for c in text.chars() {
            let strokes = self.resolve(c)?;
            self.tap(&strokes)?;
        }
        Ok(())
    }

    /// Press and release each key in turn, holding each one's level only for it.
    fn tap(&mut self, strokes: &[ResolvedKey]) -> Result<()> {
        for stroke in strokes {
            self.sync_level(stroke.level)?;
            self.send_key(stroke.keycode, false)?;
            self.send_key(stroke.keycode, true)?;
        }
        self.sync_level(LevelMods::default())
    }

    /// The keys that produce `c` on the loaded layout, in the order to press them.
    ///
    /// Usually one. Two when the layout has no single key for the character but
    /// does have the dead accent that composes it — `é` on a Norwegian keyboard is
    /// the dead acute followed by `e`, and typing it that way is what somebody
    /// sitting at that machine would do.
    fn resolve(&self, c: char) -> Result<Vec<ResolvedKey>> {
        let guard = self.transport.lock();
        let wired = guard.as_ref().ok_or_else(transport_gone)?;
        let keyboard = wired.keyboard().ok_or_else(|| no_device("a keyboard"))?;

        if let Some(stroke) = keyboard.keymap.stroke_for_any(char_keysyms(c)) {
            return Ok(vec![keyboard.reachable(stroke)?]);
        }

        // Nothing direct. If the sender composed this character, the receiver's
        // layout very likely composes it the same way.
        if let Some((base, mark)) = crate::keyres::decompose(c) {
            let dead = keys::dead_keysym(mark).and_then(|k| keyboard.keymap.stroke(k));
            let base = keyboard.keymap.stroke_for_any(char_keysyms(base));
            if let (Some(dead), Some(base)) = (dead, base) {
                tracing::debug!(
                    character = %c,
                    "composing through the layout's dead key; there is no single key for it"
                );
                return Ok(vec![keyboard.reachable(dead)?, keyboard.reachable(base)?]);
            }
        }

        // Named rather than swallowed, because this is the one failure the whole
        // text-over-the-wire design exists to avoid and the user needs to be able
        // to attribute it. Pressing something else instead would type a character
        // nobody asked for.
        tracing::warn!(
            codepoint = format!("U+{:04X}", c as u32),
            character = %c,
            "the receiving desktop's keyboard layout cannot produce this character, so it was not injected"
        );
        Err(PlatformError::Unsupported {
            operation: "injecting a character the loaded keyboard layout cannot produce",
            backend: BACKEND,
        })
    }

    /// Bring the chord modifiers in line with what this event asks for.
    ///
    /// Lock keys are deliberately absent: Caps Lock and Num Lock are toggles, not
    /// held keys, and pressing them to "match" the sender would flip the
    /// receiver's state permanently.
    ///
    /// Shift and AltGr are here despite also being level modifiers, because
    /// [`KeyEvent::injection_modifiers`] is what keeps them from being applied
    /// twice: it strips both from a text payload — the sender already spent them
    /// producing the character, and [`Injector::sync_level`] re-applies them
    /// through the keymap — and preserves them on the payloads that have no text
    /// form and genuinely need them, which is how Shift+Tab and Shift+Home reach
    /// the receiver as chords at all.
    fn sync_chord(&mut self, required: Modifiers) -> Result<()> {
        let wanted = required
            .without(Modifiers::CAPS_LOCK)
            .without(Modifiers::NUM_LOCK);
        // Answered without touching the transport, so a keystroke that needs no
        // modifier does not depend on a level-3 keycode being resolvable.
        if wanted == self.chord {
            return Ok(());
        }
        let level3 = self.level3_keycode()?;
        for (bit, step) in self.chord_plan(wanted, level3) {
            if step.physical {
                self.send_modifier(step.keycode, step.release)?;
            }
            self.chord = if step.release {
                self.chord.without(bit)
            } else {
                self.chord.union(bit)
            };
        }
        Ok(())
    }

    /// Hold exactly the Shift and AltGr a keymap level needs.
    fn sync_level(&mut self, required: LevelMods) -> Result<()> {
        if required == self.level {
            return Ok(());
        }
        let level3 = self.level3_keycode()?;
        let [shift, third] = self.level_plan(required, level3);
        if let Some(step) = shift {
            if step.physical {
                self.send_modifier(step.keycode, step.release)?;
            }
            self.level.shift = required.shift;
        }
        if let Some(step) = third {
            if step.physical {
                self.send_modifier(step.keycode, step.release)?;
            }
            self.level.level3 = required.level3;
        }
        Ok(())
    }

    /// The chord modifiers `required` changes, and whether each one's key has to
    /// move.
    ///
    /// Pure, because the difficulty here is not the sending: three independent
    /// records drive the same physical Shift key — a bare `SpecialKey::ShiftLeft`
    /// the sender forwarded, which is injected as an ordinary key and lands on
    /// the held list; this chord set; and the keymap level — and each has to
    /// leave the key alone while another still wants it down.
    fn chord_plan(&self, required: Modifiers, level3: u32) -> Vec<(Modifiers, ModifierStep)> {
        let mut steps = Vec::new();
        for (bit, code) in [
            (Modifiers::CTRL, KEY_LEFTCTRL),
            (Modifiers::ALT, KEY_LEFTALT),
            (Modifiers::SUPER, KEY_LEFTMETA),
            (Modifiers::SHIFT, KEY_LEFTSHIFT),
            // Resolved against the keymap rather than assumed to be right Alt,
            // for the same reason the level path resolves it.
            (Modifiers::ALT_GR, level3),
        ] {
            let want = required.contains(bit);
            if want == self.chord.contains(bit) {
                continue;
            }
            let pinned = self.sender_holds(code)
                || (bit == Modifiers::SHIFT && self.level.shift)
                || (bit == Modifiers::ALT_GR && self.level.level3);
            steps.push((
                bit,
                ModifierStep {
                    keycode: code,
                    release: !want,
                    physical: !pinned,
                },
            ));
        }
        steps
    }

    /// The two level modifiers `required` changes, Shift first.
    ///
    /// Pure for the same reason [`Injector::chord_plan`] is, and the mirror of
    /// it: the level path must not release a Shift the sender is holding or one
    /// this injector is holding for a chord, and it must not press a second time
    /// a key either of them already has down.
    fn level_plan(&self, required: LevelMods, level3: u32) -> [Option<ModifierStep>; 2] {
        let shift = (required.shift != self.level.shift).then(|| ModifierStep {
            keycode: KEY_LEFTSHIFT,
            release: !required.shift,
            physical: !(self.sender_holds(KEY_LEFTSHIFT) || self.chord.contains(Modifiers::SHIFT)),
        });
        let third = (required.level3 != self.level.level3).then(|| ModifierStep {
            keycode: level3,
            release: !required.level3,
            physical: !(self.sender_holds(level3) || self.chord.contains(Modifiers::ALT_GR)),
        });
        [shift, third]
    }

    /// Whether the sender is holding this key itself.
    ///
    /// A forwarded bare modifier press is injected as an ordinary key, so it is
    /// invisible to `chord` and `level` — and it is the user's own Shift, which
    /// neither of them may release.
    fn sender_holds(&self, code: u32) -> bool {
        self.held.contains(&Held::Key(code))
    }

    fn level3_keycode(&self) -> Result<u32> {
        let guard = self.transport.lock();
        let wired = guard.as_ref().ok_or_else(transport_gone)?;
        Ok(wired
            .keyboard()
            .and_then(|k| k.keymap.level3_keycode())
            .unwrap_or(KEY_RIGHTALT))
    }

    /// A modifier press or release that is not tracked as a held key.
    ///
    /// Modifier state lives in `chord` and `level` instead, so `release_all` can
    /// let them go last; putting them on the held list too would release them
    /// twice.
    fn send_modifier(&self, code: u32, release: bool) -> Result<()> {
        let guard = self.transport.lock();
        let wired = guard.as_ref().ok_or_else(transport_gone)?;
        let keyboard = wired.keyboard().ok_or_else(|| no_device("a keyboard"))?;
        keyboard.keyboard.key(code, key_state(release));
        wired.commit(&keyboard.device)
    }

    fn send_key(&mut self, code: u32, release: bool) -> Result<()> {
        {
            let guard = self.transport.lock();
            let wired = guard.as_ref().ok_or_else(transport_gone)?;
            let keyboard = wired.keyboard().ok_or_else(|| no_device("a keyboard"))?;
            keyboard.keyboard.key(code, key_state(release));
            wired.commit(&keyboard.device)?;
        }
        self.track(Held::Key(code), release);
        Ok(())
    }

    fn track(&mut self, item: Held, release: bool) {
        if release {
            self.held.retain(|h| *h != item);
        } else if !self.held.contains(&item) {
            self.held.push(item);
        }
    }
}

/// A character's place on the loaded layout.
#[derive(Debug, Clone, Copy)]
struct ResolvedKey {
    keycode: u32,
    level: LevelMods,
}

fn key_state(release: bool) -> ei::keyboard::KeyState {
    if release {
        ei::keyboard::KeyState::Released
    } else {
        ei::keyboard::KeyState::Press
    }
}

fn transport_gone() -> PlatformError {
    PlatformError::Other("the libei transport is not connected".into())
}

fn no_device(what: &str) -> PlatformError {
    PlatformError::Other(format!("the portal session offered no device for {what}"))
}

/// Read the keymap the compositor attached to a keyboard device.
///
/// A failure here is not fatal and is not an error to the caller: it costs text
/// injection, which the caller reports per keystroke, and leaves special keys and
/// chords working on their fixed evdev codes.
fn read_keymap(device: &Device) -> String {
    use std::io::Read;

    let Some(keymap) = device.keymap() else {
        return String::new();
    };
    // MAP_PRIVATE is what the protocol requires of a mapping, but a plain read is
    // equivalent here and needs no unsafe: the fd is ours and the data is a few
    // tens of kilobytes.
    let Ok(fd) = keymap.fd.try_clone() else {
        return String::new();
    };
    let mut text = String::new();
    // Bounded by what the compositor said it wrote, so a compositor handing over a
    // pipe that never ends cannot hang the session thread.
    if let Err(e) = std::fs::File::from(fd)
        .take(u64::from(keymap.size))
        .read_to_string(&mut text)
    {
        tracing::warn!(error = %e, "could not read the libei keymap");
        return String::new();
    }
    // The keymap arrives NUL-terminated; the trailing byte would otherwise be the
    // last token the parser sees.
    text.trim_end_matches('\0').to_string()
}

/// Monotonic microseconds, which is the clock libei timestamps are in.
///
/// Not wall-clock: a timestamp that jumps backwards over an NTP correction would
/// have the compositor reordering or discarding events mid-gesture.
fn now_us() -> u64 {
    let now = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    (now.tv_sec as u64)
        .wrapping_mul(1_000_000)
        .wrapping_add(now.tv_nsec as u64 / 1_000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wx_proto::{MonitorId, Rect, SpecialKey};

    fn monitor() -> Monitor {
        Monitor {
            id: MonitorId(0),
            name: "test".into(),
            local_bounds: Rect::new(0, 0, 1920, 1080),
            scale: 1.0,
            primary: true,
        }
    }

    fn injector() -> Injector {
        Injector::new(Arc::new(Transport::new()))
    }

    #[test]
    fn every_event_is_refused_while_there_is_no_transport() {
        // The whole point of returning a real error: a peer driving a machine
        // whose portal session never came up must be told, not quietly ignored.
        let mut inj = injector();
        for event in [
            InputEvent::Pointer(PointerEvent::MoveTo {
                pos: NormPos::new(0.5, 0.5),
            }),
            InputEvent::Pointer(PointerEvent::MoveBy { dx: 3.0, dy: 4.0 }),
            InputEvent::Pointer(PointerEvent::Button {
                button: MouseButton::Left,
                pressed: true,
            }),
            InputEvent::Pointer(PointerEvent::Scroll {
                dx: 0.0,
                dy: 1.0,
                unit: ScrollUnit::Lines,
            }),
            InputEvent::Key(KeyEvent::text("a", KeyAction::Press, Modifiers::NONE)),
            InputEvent::Key(KeyEvent::special(
                SpecialKey::Enter,
                KeyAction::Press,
                Modifiers::NONE,
            )),
        ] {
            assert!(
                inj.inject(&monitor(), &event).is_err(),
                "{event:?} reported success with no transport"
            );
        }
        assert!(inj.warp_cursor(&monitor(), NormPos::new(0.5, 0.5)).is_err());
    }

    #[test]
    fn release_all_on_a_fresh_injector_does_nothing_and_succeeds() {
        // Called on every disconnect, including the ones where nothing was ever
        // pressed and no session was ever granted.
        let mut inj = injector();
        assert!(inj.release_all().is_ok());
        assert!(inj.held.is_empty());
    }

    #[test]
    fn a_transport_that_went_away_does_not_strand_what_it_was_holding() {
        // A revoked session takes its virtual devices with it, so nothing is left
        // held on that desktop. Keeping the record would make every later
        // `release_all` try to send into a dead connection forever.
        let mut inj = injector();
        inj.held.push(Held::Key(KEY_LEFTCTRL));
        inj.chord = Modifiers::CTRL;
        assert!(inj.release_all().is_ok());
        assert!(inj.held.is_empty());
        assert_eq!(inj.chord, Modifiers::NONE);
    }

    #[test]
    fn nothing_is_recorded_as_held_when_the_press_could_not_be_sent() {
        // The other direction of the same rule: a press that never reached the
        // compositor must not produce a release later, or the desktop sees a
        // key-up it never saw a key-down for.
        let mut inj = injector();
        assert!(inj
            .inject(
                &monitor(),
                &InputEvent::Pointer(PointerEvent::Button {
                    button: MouseButton::Left,
                    pressed: true,
                }),
            )
            .is_err());
        assert!(inj.held.is_empty());
    }

    #[test]
    fn a_button_with_no_linux_code_is_refused_before_the_transport_is_even_asked() {
        let mut inj = injector();
        let err = inj
            .inject(
                &monitor(),
                &InputEvent::Pointer(PointerEvent::Button {
                    button: MouseButton::Extra(200),
                    pressed: true,
                }),
            )
            .unwrap_err();
        assert!(matches!(err, PlatformError::Unsupported { .. }));
    }

    #[test]
    fn sub_pixel_and_non_finite_relative_motion_never_reaches_the_compositor() {
        // libei takes floats, so there is no accumulator to keep — but a NaN off
        // the wire would park the cursor somewhere unrecoverable, and a zero move
        // is pure wakeup for every pointer listener on the desktop. Both are
        // dropped before the transport is consulted, which is why these succeed
        // with no session at all.
        let mut inj = injector();
        for (dx, dy) in [(0.0, 0.0), (f64::NAN, 1.0), (1.0, f64::INFINITY)] {
            assert!(inj
                .inject(
                    &monitor(),
                    &InputEvent::Pointer(PointerEvent::MoveBy { dx, dy }),
                )
                .is_ok());
        }
    }

    #[test]
    fn a_scroll_that_rounds_to_nothing_is_not_sent() {
        let mut inj = injector();
        for (dx, dy) in [(0.0, 0.0), (f64::NAN, f64::NAN)] {
            assert!(inj
                .inject(
                    &monitor(),
                    &InputEvent::Pointer(PointerEvent::Scroll {
                        dx,
                        dy,
                        unit: ScrollUnit::Pixels
                    }),
                )
                .is_ok());
        }
    }

    #[test]
    fn an_empty_text_payload_is_not_a_keystroke() {
        // Nothing to press and nothing to fail about, so this must not be reported
        // as a transport error the agent would log on every such event.
        let mut inj = injector();
        assert!(inj
            .inject(
                &monitor(),
                &InputEvent::Key(KeyEvent::text("", KeyAction::Press, Modifiers::NONE)),
            )
            .is_ok());
    }

    #[test]
    fn release_control_clears_the_record_even_with_no_session() {
        let mut inj = injector();
        inj.held.push(Held::Button(0x110));
        assert!(inj.inject(&monitor(), &InputEvent::ReleaseControl).is_ok());
        assert!(inj.held.is_empty());
    }

    #[test]
    fn the_held_list_does_not_grow_on_auto_repeat() {
        // A held key repeating would otherwise queue one release per repeat, and
        // `release_all` would send a burst of key-ups for a key that came up once.
        let mut inj = injector();
        inj.track(Held::Key(30), false);
        inj.track(Held::Key(30), false);
        assert_eq!(inj.held.len(), 1);
        inj.track(Held::Key(30), true);
        assert!(inj.held.is_empty());
    }

    #[test]
    fn a_special_key_that_needs_shift_actually_holds_it() {
        // Shift+Tab has no text form, so `injection_modifiers` keeps the Shift and
        // this backend has to press it. Ignoring the bit sends a bare Tab, and the
        // reverse-tabbing the user asked for silently does not happen.
        let inj = injector();
        let event = KeyEvent::special(SpecialKey::Tab, KeyAction::Press, Modifiers::SHIFT);
        assert_eq!(
            inj.chord_plan(event.injection_modifiers(), KEY_RIGHTALT),
            vec![(
                Modifiers::SHIFT,
                ModifierStep {
                    keycode: KEY_LEFTSHIFT,
                    release: false,
                    physical: true,
                }
            )]
        );
    }

    #[test]
    fn a_shifted_character_is_not_shifted_twice() {
        // The other direction: the sender already spent Shift producing the `A`,
        // so `injection_modifiers` strips it and the chord path must ask for
        // nothing. The keymap level presses Shift once, on its own.
        let mut inj = injector();
        let event = KeyEvent::text("A", KeyAction::Press, Modifiers::SHIFT);
        assert!(inj
            .chord_plan(event.injection_modifiers(), KEY_RIGHTALT)
            .is_empty());

        let [shift, third] = inj.level_plan(
            LevelMods {
                shift: true,
                level3: false,
            },
            KEY_RIGHTALT,
        );
        assert_eq!(
            shift,
            Some(ModifierStep {
                keycode: KEY_LEFTSHIFT,
                release: false,
                physical: true,
            })
        );
        assert_eq!(third, None);

        // And it comes back up with the keystroke rather than reaching the next
        // event — the stuck-Shift defect that turned a following Home into
        // Shift+Home.
        inj.level.shift = true;
        let [shift, _] = inj.level_plan(LevelMods::default(), KEY_RIGHTALT);
        assert_eq!(
            shift,
            Some(ModifierStep {
                keycode: KEY_LEFTSHIFT,
                release: true,
                physical: true,
            })
        );
    }

    #[test]
    fn a_shift_the_sender_is_holding_is_never_released_by_a_keystroke() {
        // A forwarded `SpecialKey::ShiftLeft` is injected as an ordinary key, so
        // it is on the held list and invisible to both modifier sets. Letting the
        // level path release it after a `!` loses the Shift from the Shift+Home
        // that follows.
        let mut inj = injector();
        inj.held.push(Held::Key(KEY_LEFTSHIFT));
        inj.level.shift = true;

        let [shift, _] = inj.level_plan(LevelMods::default(), KEY_RIGHTALT);
        let step = shift.expect("the level record still has to let go of its Shift");
        assert!(step.release);
        assert!(!step.physical, "the sender's own Shift key must stay down");

        // And the level path must not press a key that is already down either.
        inj.level.shift = false;
        let [shift, _] = inj.level_plan(
            LevelMods {
                shift: true,
                level3: false,
            },
            KEY_RIGHTALT,
        );
        assert!(!shift.unwrap().physical);
    }

    #[test]
    fn the_chord_and_the_level_do_not_fight_over_the_same_key() {
        // Both drive KEY_LEFTSHIFT. Whichever stops wanting it while the other
        // still does must forget it without touching the key.
        let mut inj = injector();
        inj.chord = Modifiers::SHIFT;
        inj.level.shift = true;

        let [shift, _] = inj.level_plan(LevelMods::default(), KEY_RIGHTALT);
        assert!(!shift.unwrap().physical);

        assert_eq!(
            inj.chord_plan(Modifiers::NONE, KEY_RIGHTALT),
            vec![(
                Modifiers::SHIFT,
                ModifierStep {
                    keycode: KEY_LEFTSHIFT,
                    release: true,
                    physical: false,
                }
            )]
        );
    }

    #[test]
    fn release_all_lets_a_shared_modifier_key_go_exactly_once() {
        // Three records can name KEY_LEFTSHIFT at the same time, and the held list
        // is unwound first. A second key-up for a key that is already up is a
        // report the receiving desktop has no way to make sense of.
        let mut inj = injector();
        inj.held.push(Held::Key(KEY_LEFTSHIFT));
        inj.chord = Modifiers::SHIFT | Modifiers::CTRL;
        inj.level.shift = true;

        assert_eq!(inj.modifier_keycodes(KEY_RIGHTALT), vec![KEY_LEFTCTRL]);

        inj.held.clear();
        assert_eq!(
            inj.modifier_keycodes(KEY_RIGHTALT),
            vec![KEY_LEFTSHIFT, KEY_LEFTCTRL]
        );
    }

    #[test]
    fn the_monotonic_clock_moves_forward() {
        // The timestamp on every frame. A clock that went backwards would have the
        // compositor discarding events mid-gesture.
        let first = now_us();
        let second = now_us();
        assert!(second >= first);
        assert!(first > 0);
    }
}
