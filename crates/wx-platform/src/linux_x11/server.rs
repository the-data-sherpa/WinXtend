//! The X11 protocol half of this backend.
//!
//! Everything here talks to the server; the interpretation of what it says lives in
//! [`super::display`], [`super::keymap`] and [`super::inject`], which are testable
//! without one. That division is the same one [`crate::linux_wayland`] keeps and it
//! exists for the same reason: this crate has to compile and test on Windows and
//! macOS, where there is no X server to answer anything.
//!
//! # One connection, held open
//!
//! Unlike the Wayland backend, which opens a connection per enumeration, this one
//! keeps a single connection for the life of the process. It has to: injection is
//! on the latency path and a connect plus handshake per keystroke would be
//! indefensible, and `MappingNotify` — how the server says the keyboard layout
//! changed — is only delivered to a client that is still there to receive it.
//!
//! Nothing is cached behind that connection except the keymap, which the server
//! itself invalidates. Monitors are re-read on every call, so a screen that was
//! unplugged cannot linger in the layout — the failure
//! [`crate::traits::DisplayEnumerator`] warns about, and the one the Wayland
//! backend's connection-per-call design buys the same way.
//!
//! # Why the event queue must be drained
//!
//! This is the trap a long-lived X connection sets. `MappingNotify` is sent to
//! **every** client with no event mask to opt out of, so a client that only ever
//! writes will have events pile up in the socket until the server's write to it
//! blocks — and an X server blocked writing to one client stops servicing every
//! other, which looks to the person at the machine like the whole desktop freezing.
//! So [`Server::drain`] runs on every request this backend makes, and it is a
//! non-blocking read rather than anything that can wait.
//!
//! # What is selected, and what is deliberately not
//!
//! `RRScreenChangeNotify`, so a monitor being plugged in or a resolution changing
//! is visible in the log at the moment it happens rather than inferred later from a
//! layout that moved. It is **not** used to invalidate a cached monitor list,
//! because there is no cached monitor list.
//!
//! Nothing else. In particular no `XISelectEvents` and no `XIGrabDevice`: this
//! backend is a driven target only, and an exclusive grab that is not released on
//! every exit path leaves the local desktop apparently frozen. Capture is a
//! separate piece of work and is not started here by halves.

use std::sync::{Arc, Mutex};

use x11rb::connection::Connection as _;
use x11rb::errors::ReplyError;
use x11rb::protocol::randr::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

use super::display::RawOutput;
use super::inject::{Request, Sink};
use super::keymap::{Keymap, RawKeymap, MODIFIER_COUNT};
use super::BACKEND;
use crate::error::{PlatformError, Result};

/// The XTEST event types, which are the core protocol's own event codes.
const KEY_PRESS: u8 = xproto::KEY_PRESS_EVENT;
const KEY_RELEASE: u8 = xproto::KEY_RELEASE_EVENT;
const BUTTON_PRESS: u8 = xproto::BUTTON_PRESS_EVENT;
const BUTTON_RELEASE: u8 = xproto::BUTTON_RELEASE_EVENT;
const MOTION_NOTIFY: u8 = xproto::MOTION_NOTIFY_EVENT;

/// `xtest_fake_input`'s `detail` for a motion event: zero addresses a position on
/// the screen, non-zero makes the coordinates a delta.
const MOTION_ABSOLUTE: u8 = 0;
const MOTION_RELATIVE: u8 = 1;

/// `CurrentTime`, which asks the server to process the event now rather than
/// scheduling it.
const NOW: u32 = 0;

/// The core keyboard, which is what XTEST drives when no device is named.
const DEFAULT_DEVICE: u8 = 0;

/// A live connection to the X server.
pub(super) struct Server {
    conn: RustConnection,
    root: xproto::Window,
    /// Whether RandR answered its version query. Without it there is no way to ask
    /// about monitors at all, and saying so once beats failing per call.
    has_randr: bool,
    /// The keyboard layout, rebuilt when the server says it changed.
    ///
    /// The only thing cached behind this connection, and cached because the server
    /// itself invalidates it: `MappingNotify` is not a hint, it is the notification
    /// that every earlier answer is stale.
    keymap: Mutex<Option<Arc<Keymap>>>,
}

/// Open the connection and check that the extensions this backend needs are there.
///
/// Fails rather than degrading when XTEST is missing: a backend that enumerated
/// screens and could not inject would put this machine in peers' layouts and then
/// swallow everything routed to it, which is worse than not appearing at all. RandR
/// is softer — a server without it has no monitors to report, and the node can
/// still be told about peers.
pub(super) fn connect() -> Result<Arc<Server>> {
    let (conn, screen_num) = RustConnection::connect(None).map_err(|e| {
        // Not a warning: an agent on a TTY, a CI runner and a machine whose
        // DISPLAY points at a server that has gone away all land here.
        tracing::debug!(error = %e, "no X11 connection");
        PlatformError::NoDisplayServer
    })?;

    let root = conn
        .setup()
        .roots
        .get(screen_num)
        .ok_or_else(|| {
            PlatformError::Other(format!(
                "the X server offered no screen {screen_num} to attach to"
            ))
        })?
        .root;

    // XTEST is the whole injection path. Its absence is rare — no current X server
    // ships without it — but it is not something to discover one keystroke at a
    // time.
    conn.xtest_get_version(2, 2)
        .map_err(ReplyError::from)
        .and_then(|c| c.reply())
        .map_err(|e| {
            PlatformError::Other(format!(
                "this X server has no XTEST extension, so nothing can be injected into it: {e}"
            ))
        })?;

    // 1.2 is where per-output CRTC geometry arrives, which is the only thing this
    // backend asks RandR for.
    let has_randr = match conn
        .randr_query_version(1, 2)
        .map_err(ReplyError::from)
        .and_then(|c| c.reply())
    {
        Ok(version) => {
            tracing::debug!(
                major = version.major_version,
                minor = version.minor_version,
                "randr is available"
            );
            true
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "this X server has no RandR extension; it will report no displays"
            );
            false
        }
    };

    let server = Arc::new(Server {
        conn,
        root,
        has_randr,
        keymap: Mutex::new(None),
    });

    if has_randr {
        // Selected so a reconfiguration is visible in the log at the moment it
        // happens. The monitor list itself is re-read per call and never cached, so
        // nothing depends on this arriving.
        if let Err(e) = server
            .conn
            .randr_select_input(root, randr::NotifyMask::SCREEN_CHANGE)
            .map_err(ReplyError::from)
            .and_then(|c| c.check())
        {
            tracing::debug!(error = %e, "could not subscribe to randr screen changes");
        }
    }

    Ok(server)
}

impl Server {
    /// The RandR outputs as the server describes them right now.
    ///
    /// Every output's info is asked for before any reply is awaited, so the whole
    /// enumeration costs one round trip rather than one per connector.
    ///
    /// Stops at the raw outputs rather than returning [`wx_proto::Monitor`]s,
    /// because deciding which of them counts is [`super::display`] — pure, and
    /// therefore tested on machines that have no X server to disagree with it.
    pub(super) fn outputs(&self) -> Result<Vec<RawOutput>> {
        self.drain();
        if !self.has_randr {
            return Err(PlatformError::Unsupported {
                operation: "display enumeration (this X server has no RandR extension)",
                backend: BACKEND,
            });
        }

        // `Current` rather than `GetScreenResources`: the latter asks the driver to
        // re-probe every connector, which takes tens of milliseconds and is a poll
        // this runs on the agent's engine loop.
        let resources = self
            .conn
            .randr_get_screen_resources_current(self.root)
            .map_err(ReplyError::from)
            .and_then(|c| c.reply())
            .map_err(|e| failed("asking randr for the current screen resources", e))?;

        // A server where `xrandr --primary` was never run answers zero, which is
        // `None` and not an output.
        let primary = self
            .conn
            .randr_get_output_primary(self.root)
            .map_err(ReplyError::from)
            .and_then(|c| c.reply())
            .map(|r| r.output)
            .unwrap_or(0);

        let timestamp = resources.config_timestamp;
        let cookies: Vec<_> = resources
            .outputs
            .iter()
            .map(|output| (*output, self.conn.randr_get_output_info(*output, timestamp)))
            .collect();

        let mut raw = Vec::with_capacity(cookies.len());
        for (output, cookie) in cookies {
            let info = match cookie.map_err(ReplyError::from).and_then(|c| c.reply()) {
                Ok(info) => info,
                // One connector that vanished between the resource list and this
                // reply is not a reason to report no screens at all.
                Err(e) => {
                    tracing::debug!(error = %e, output, "randr output info unavailable; skipping");
                    continue;
                }
            };
            let bounds = if info.crtc == 0 {
                None
            } else {
                match self
                    .conn
                    .randr_get_crtc_info(info.crtc, timestamp)
                    .map_err(ReplyError::from)
                    .and_then(|c| c.reply())
                {
                    Ok(crtc) => Some(wx_proto::Rect::new(
                        i32::from(crtc.x),
                        i32::from(crtc.y),
                        u32::from(crtc.width),
                        u32::from(crtc.height),
                    )),
                    Err(e) => {
                        tracing::debug!(error = %e, output, "randr crtc info unavailable; skipping");
                        None
                    }
                }
            };
            raw.push(RawOutput {
                // RandR names are bytes with no declared encoding. Connector names
                // are ASCII in practice, and a lossy read keeps a monitor with an
                // odd byte in its name rather than dropping it.
                name: String::from_utf8_lossy(&info.name).into_owned(),
                connected: info.connection == randr::Connection::CONNECTED,
                bounds,
                primary: output == primary && output != 0,
            });
        }

        Ok(raw)
    }

    /// Whether this connection still has a server on the other end of it.
    ///
    /// A round trip rather than a reading of the last error, because x11rb reports
    /// "the server raised a protocol error against that one request" and "the socket
    /// has closed" through the same `Result`, and only the first of those leaves a
    /// connection that can still inject. `GetInputFocus` is the request to ask with:
    /// it takes no arguments, cannot fail on its own merits, and is what X clients
    /// have always used to force a synchronisation.
    ///
    /// Only called after something else has already failed, so its cost is paid on a
    /// path that is not working anyway.
    pub(super) fn is_reachable(&self) -> bool {
        self.conn
            .get_input_focus()
            .map_err(ReplyError::from)
            .and_then(|c| c.reply())
            .is_ok()
    }

    /// Read everything the server has sent us, without waiting for more.
    ///
    /// See the module docs: an X connection that is never read from eventually
    /// blocks the server for every other client on the desktop. This is also where
    /// a keyboard remap is noticed, which is the only thing behind this connection
    /// that is cached.
    fn drain(&self) {
        loop {
            match self.conn.poll_for_event() {
                Ok(Some(Event::MappingNotify(event))) => {
                    // `Modifier` and `Keyboard` both change what a keycode produces;
                    // `Pointer` is a button remap and leaves the layout alone.
                    if event.request != xproto::Mapping::POINTER {
                        tracing::debug!("the keyboard mapping changed; re-reading the layout");
                        *self.keymap.lock().unwrap_or_else(poisoned) = None;
                    }
                }
                Ok(Some(Event::RandrScreenChangeNotify(event))) => {
                    tracing::debug!(
                        width = event.width,
                        height = event.height,
                        "the screen configuration changed"
                    );
                }
                Ok(Some(_)) => {}
                Ok(None) => return,
                Err(e) => {
                    // Not fatal here. Whatever the caller was about to do will fail
                    // with the same error and can report it properly.
                    tracing::debug!(error = %e, "reading from the X connection failed");
                    return;
                }
            }
        }
    }

    /// Read the keyboard layout out of the two core requests.
    fn read_keymap(&self) -> Result<Keymap> {
        let setup = self.conn.setup();
        let min_keycode = setup.min_keycode;
        let count = setup
            .max_keycode
            .saturating_sub(min_keycode)
            .saturating_add(1);

        let mapping = self
            .conn
            .get_keyboard_mapping(min_keycode, count)
            .map_err(ReplyError::from)
            .and_then(|c| c.reply())
            .map_err(|e| failed("asking the X server for the keyboard mapping", e))?;
        let modmap = self
            .conn
            .get_modifier_mapping()
            .map_err(ReplyError::from)
            .and_then(|c| c.reply())
            .map_err(|e| failed("asking the X server for the modifier mapping", e))?;

        // The reply is eight rows of equal length, so the width is not stated
        // separately. A server that sent nothing leaves every row empty rather than
        // dividing by zero.
        let per_modifier = modmap.keycodes.len() / MODIFIER_COUNT;
        let mut modifiers: [Vec<u8>; MODIFIER_COUNT] = Default::default();
        for (index, row) in modifiers.iter_mut().enumerate() {
            let start = index * per_modifier;
            // Zero means the slot is unbound, and a keycode of zero would otherwise
            // read as a real key.
            *row = modmap.keycodes[start..start + per_modifier]
                .iter()
                .copied()
                .filter(|code| *code != 0)
                .collect();
        }

        Ok(Keymap::from_raw(&RawKeymap {
            min_keycode,
            keysyms_per_keycode: mapping.keysyms_per_keycode,
            keysyms: mapping.keysyms,
            modifiers,
        }))
    }
}

impl Sink for Server {
    fn send(&self, request: Request) -> Result<()> {
        self.drain();
        let (type_, detail, x, y) = match request {
            Request::Motion { x, y, relative } => (
                MOTION_NOTIFY,
                if relative {
                    MOTION_RELATIVE
                } else {
                    MOTION_ABSOLUTE
                },
                x,
                y,
            ),
            Request::Button { button, press } => (
                if press { BUTTON_PRESS } else { BUTTON_RELEASE },
                button,
                0,
                0,
            ),
            Request::Key { keycode, press } => {
                (if press { KEY_PRESS } else { KEY_RELEASE }, keycode, 0, 0)
            }
        };

        self.conn
            .xtest_fake_input(type_, detail, NOW, self.root, x, y, DEFAULT_DEVICE)
            .map_err(|e| failed("sending an XTEST event", e))?;
        // Flushed rather than left in the buffer: input latency is the product, and
        // a keystroke that goes out with the next one is a keystroke that arrived
        // late.
        self.conn
            .flush()
            .map_err(|e| failed("flushing the X connection", e))
    }

    fn keymap(&self) -> Result<Arc<Keymap>> {
        self.drain();
        let mut cached = self.keymap.lock().unwrap_or_else(poisoned);
        if let Some(keymap) = cached.as_ref() {
            return Ok(Arc::clone(keymap));
        }
        let keymap = Arc::new(self.read_keymap()?);
        if keymap.is_empty() {
            // Not fatal — special keys and chords resolve through fixed positions
            // and go on working — but every text payload will be refused, so it has
            // to be attributable.
            tracing::warn!(
                "the X server described no keyboard layout; text injection will be unavailable"
            );
        }
        *cached = Some(Arc::clone(&keymap));
        Ok(keymap)
    }

    fn caps_locked(&self) -> Result<bool> {
        // `QueryPointer` carries the keyboard's modifier state, which is the
        // cheapest way to ask: one round trip and no extension.
        let reply = self
            .conn
            .query_pointer(self.root)
            .map_err(ReplyError::from)
            .and_then(|c| c.reply())
            .map_err(|e| failed("asking the X server for the modifier state", e))?;
        Ok(reply.mask.contains(xproto::KeyButMask::LOCK))
    }
}

/// The one place an X11 failure becomes a [`PlatformError`].
///
/// Everything x11rb reports — a closed socket, a protocol error the server raised
/// against one of our requests — is the same answer to the caller: this connection
/// can no longer be used. The text keeps which call it was, because that is the
/// part a bug report needs.
fn failed<E: std::fmt::Display>(during: &str, e: E) -> PlatformError {
    PlatformError::Other(format!("{during} failed: {e}"))
}

/// A poisoned keymap lock means a panic elsewhere left it half-written.
///
/// Taking it anyway is correct here: the worst case is one stale or one rebuilt
/// keymap, and refusing to read it would turn somebody else's panic into a machine
/// that cannot be typed on.
fn poisoned<T>(e: std::sync::PoisonError<T>) -> T {
    e.into_inner()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::DisplayEnumerator;
    use std::collections::HashSet;
    use wx_proto::Monitor;

    /// The screens of this session, read the way the backend reads them.
    fn read(server: &Server) -> Result<Vec<Monitor>> {
        Ok(super::super::display::to_monitors(server.outputs()?))
    }

    /// The monitors of this session, or `None` when there is nothing to assert
    /// against.
    ///
    /// CI is headless and every developer machine in this project runs Wayland, so
    /// a test that failed without an X server would make the suite unrunnable
    /// almost everywhere. The skip covers exactly what [`connect`] itself calls
    /// acceptable — no server, and a server with no RandR — and nothing else: an
    /// enumeration that fails against a server that *is* there still panics,
    /// because that is the only place the bug is visible.
    ///
    /// Note that on a Wayland desktop this reaches Xwayland, which is a real X
    /// server answering real RandR. That is a useful test target and not a
    /// substitute for the Xorg the captain's machine runs.
    fn session_monitors() -> Option<Vec<Monitor>> {
        let server = match connect() {
            Ok(server) => server,
            Err(e) => {
                eprintln!("skipped: {e}");
                return None;
            }
        };
        match read(&server) {
            Ok(monitors) if monitors.is_empty() => {
                eprintln!("skipped: an X server with no usable output");
                None
            }
            Ok(monitors) => Some(monitors),
            Err(e @ PlatformError::Unsupported { .. }) => {
                eprintln!("skipped: {e}");
                None
            }
            Err(e) => panic!("enumeration failed against a live X server: {e}"),
        }
    }

    #[test]
    fn enumerating_this_session_yields_consistent_monitors() {
        let Some(monitors) = session_monitors() else {
            return;
        };
        assert_eq!(monitors.iter().filter(|m| m.primary).count(), 1);

        for m in &monitors {
            assert!(!m.local_bounds.is_empty(), "{m:?} has no extent");
            assert!(m.scale > 0.0, "{m:?} has a nonsensical scale");
            assert!(!m.name.is_empty(), "{m:?} has no name");
        }

        let ids: HashSet<_> = monitors.iter().map(|m| m.id).collect();
        assert_eq!(ids.len(), monitors.len(), "monitor ids must be unique");
    }

    #[test]
    fn ids_and_geometry_do_not_move_between_two_reads() {
        // Nothing is cached, so these are two independent enumerations. Ids that
        // differed between them would mean every saved layout started addressing a
        // different screen on the next tick.
        let Some(first) = session_monitors() else {
            return;
        };
        let second = connect()
            .and_then(|s| read(&s))
            .expect("a second read of a server that just answered");

        assert_eq!(
            first.iter().map(|m| m.id).collect::<Vec<_>>(),
            second.iter().map(|m| m.id).collect::<Vec<_>>()
        );
        assert_eq!(
            first.iter().map(|m| m.local_bounds).collect::<Vec<_>>(),
            second.iter().map(|m| m.local_bounds).collect::<Vec<_>>()
        );
    }

    #[test]
    fn virtual_bounds_contain_every_monitor() {
        let Some(monitors) = session_monitors() else {
            return;
        };
        let bounds = super::super::X11Displays::new(
            super::super::Session::connect(),
            crate::LiveCapabilities::fixed(wx_proto::Capabilities::NONE),
        )
        .virtual_bounds()
        .expect("a server that just enumerated");
        for m in monitors {
            assert!(bounds.x <= m.local_bounds.x);
            assert!(bounds.y <= m.local_bounds.y);
            assert!(bounds.right() >= m.local_bounds.right());
            assert!(bounds.bottom() >= m.local_bounds.bottom());
        }
    }

    #[test]
    fn the_layout_this_session_reports_is_one_characters_can_be_resolved_against() {
        // Reads the real `GetKeyboardMapping` and `GetModifierMapping` — both
        // queries, and the only part of injection that can be exercised against a
        // developer's own session without typing into their windows.
        let Ok(server) = connect() else {
            eprintln!("skipped: no X server");
            return;
        };
        let Ok(keymap) = server.keymap() else {
            eprintln!("skipped: an X server that described no keyboard");
            return;
        };
        if keymap.is_empty() {
            eprintln!("skipped: an X server with an empty keyboard mapping");
            return;
        }

        // ASCII is what this backend ships being able to type, and a layout that
        // could not produce it would make the whole thing useless.
        for c in ['a', 'A', 'z', '0', ' ', '.'] {
            assert!(
                keymap
                    .stroke_for_any(crate::linux_wayland::keys::char_keysyms(c))
                    .is_some(),
                "this session's layout cannot type {c:?}"
            );
        }
        // Shift has to be findable or every capital is unreachable.
        assert!(keymap.shift_keycode().is_some());
    }

    #[test]
    fn a_second_read_of_the_layout_comes_from_the_cache() {
        // The keymap is the one thing held behind the connection, and it is held
        // because `MappingNotify` is what invalidates it. Re-reading it per
        // keystroke would be two round trips on the latency path.
        let Ok(server) = connect() else {
            eprintln!("skipped: no X server");
            return;
        };
        let Ok(first) = server.keymap() else {
            eprintln!("skipped: an X server that described no keyboard");
            return;
        };
        let second = server.keymap().expect("a cached keymap cannot fail");
        assert!(Arc::ptr_eq(&first, &second));
    }
}
