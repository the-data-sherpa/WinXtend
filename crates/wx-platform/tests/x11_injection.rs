//! X11 injection, end to end against a real X server.
//!
//! # Why this is opt-in, and why the opt-in is a display name
//!
//! This test **synthesises keyboard and mouse input**. Run against a desktop
//! somebody is using, it types into whatever window has focus and clicks wherever
//! the pointer happens to be. That is not a risk to manage carefully, it is a thing
//! that must not be possible by accident — so the test does not read `DISPLAY` at
//! all. It runs only when `WINXTEND_X11_TEST_DISPLAY` names a server, and it uses
//! that name and nothing else. A plain `cargo test`, on a developer's machine or on
//! CI, skips.
//!
//! Point it at a throwaway server, never a session:
//!
//! ```sh
//! Xvfb :77 -screen 0 1920x1080x24 &
//! WINXTEND_X11_TEST_DISPLAY=:77 cargo test -p wx-platform --test x11_injection
//! ```
//!
//! `Xephyr :77 -screen 1920x1080` works the same way and lets you watch it happen.
//!
//! # How it knows the injection landed
//!
//! Without asking a toolkit for anything. X11 will report its own input state, so
//! the server itself is the oracle:
//!
//! * `QueryPointer` gives the pointer's position and which buttons are down.
//! * `QueryKeymap` gives a bit per keycode for every key physically down.
//!
//! So "the key went down" and — the one that matters most — "`release_all` really
//! let go of everything" are assertions rather than inferences. A backend that
//! leaves a modifier stuck is worse than one that does nothing, and this is where
//! that is checked against a real server rather than against a recording fake.

#![cfg(target_os = "linux")]

use wx_platform::linux_x11;
use wx_proto::{
    Capabilities, InputEvent, KeyAction, KeyEvent, Modifiers, Monitor, MouseButton, NormPos,
    PointerEvent, SpecialKey,
};

use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use x11rb::rust_connection::RustConnection;

/// The server this test is allowed to touch, or `None` to skip.
///
/// Deliberately not `DISPLAY`: the whole point is that pointing this at a live
/// session has to be a decision somebody made, not the default.
fn test_display() -> Option<String> {
    match std::env::var("WINXTEND_X11_TEST_DISPLAY") {
        Ok(display) if !display.is_empty() => Some(display),
        _ => {
            eprintln!(
                "skipped: set WINXTEND_X11_TEST_DISPLAY to a throwaway X server \
                 (e.g. `Xvfb :77 -screen 0 1920x1080x24 &`) to run this. \
                 It synthesises input and must never be pointed at a session."
            );
            None
        }
    }
}

/// Reads the server's own view of the input state.
struct Observer {
    conn: RustConnection,
    root: xproto::Window,
}

impl Observer {
    fn connect(display: &str) -> Self {
        let (conn, screen) =
            RustConnection::connect(Some(display)).expect("the test display must be reachable");
        let root = conn.setup().roots[screen].root;
        Self { conn, root }
    }

    fn pointer(&self) -> xproto::QueryPointerReply {
        self.conn
            .query_pointer(self.root)
            .expect("query_pointer")
            .reply()
            .expect("query_pointer reply")
    }

    /// Whether the server believes this keycode is physically down.
    fn key_down(&self, keycode: u8) -> bool {
        let keys = self
            .conn
            .query_keymap()
            .expect("query_keymap")
            .reply()
            .expect("query_keymap reply")
            .keys;
        keys[usize::from(keycode / 8)] & (1 << (keycode % 8)) != 0
    }

    /// Every keycode the server believes is down.
    fn keys_down(&self) -> Vec<u8> {
        let keys = self
            .conn
            .query_keymap()
            .expect("query_keymap")
            .reply()
            .expect("query_keymap reply")
            .keys;
        (0u8..=255)
            .filter(|k| keys[usize::from(k / 8)] & (1 << (k % 8)) != 0)
            .collect()
    }

    fn buttons_down(&self) -> Vec<u8> {
        let mask = self.pointer().mask;
        [
            (1, xproto::KeyButMask::BUTTON1),
            (2, xproto::KeyButMask::BUTTON2),
            (3, xproto::KeyButMask::BUTTON3),
            (4, xproto::KeyButMask::BUTTON4),
            (5, xproto::KeyButMask::BUTTON5),
        ]
        .into_iter()
        .filter(|(_, bit)| mask.contains(*bit))
        .map(|(n, _)| n)
        .collect()
    }

    /// The keycode this server's layout puts a character on, unshifted.
    fn keycode_for(&self, c: char) -> u8 {
        self.find_keycode(|row| row.first() == Some(&(c as u32)))
            .unwrap_or_else(|| panic!("this server's layout has no unshifted {c:?}"))
    }

    /// The keycode carrying a keysym at any level.
    fn keycode_for_keysym(&self, keysym: u32) -> u8 {
        self.find_keycode(|row| row.contains(&keysym))
            .unwrap_or_else(|| panic!("this server's layout has no keysym {keysym:#x}"))
    }

    /// Read straight from `GetKeyboardMapping`, independently of the backend under
    /// test, so an assertion about "the `a` key" cannot pass by agreeing with a bug
    /// in the resolution it is checking.
    fn find_keycode(&self, matches: impl Fn(&[u32]) -> bool) -> Option<u8> {
        let setup = self.conn.setup();
        let count = setup.max_keycode - setup.min_keycode + 1;
        let mapping = self
            .conn
            .get_keyboard_mapping(setup.min_keycode, count)
            .expect("get_keyboard_mapping")
            .reply()
            .expect("get_keyboard_mapping reply");
        let per = usize::from(mapping.keysyms_per_keycode);
        mapping
            .keysyms
            .chunks(per)
            .position(matches)
            .map(|index| setup.min_keycode + index as u8)
    }
}

/// Wait until everything already injected has actually been executed.
///
/// Two connections are in play — the backend's and this test's — and X promises
/// nothing about the order it services them in. A `QueryKeymap` written second can
/// be answered before the XTEST request written first, so reading the input state
/// straight after injecting is a race that passes about half the time. This makes
/// the *injecting* connection do a round trip, which the server can only answer
/// once it has executed everything queued ahead of it on that connection; the
/// observation that follows is then sent strictly afterwards.
///
/// Any request with a reply would do. Enumerating is the one the public backend
/// offers.
fn settle(backend: &wx_platform::PlatformBackend) {
    let _ = backend.displays.monitors();
}

/// Everything in one test function on purpose.
///
/// It sets `DISPLAY` for the process — which is how the backend under test is
/// pointed at the throwaway server, since `linux_x11::backend()` takes no argument
/// — and a second test running concurrently in the same binary would race that.
/// One function, run in order, has no such race.
#[test]
fn a_driven_target_enumerates_its_screen_and_accepts_what_is_injected() {
    let Some(display) = test_display() else {
        return;
    };
    // Scoped to this process. The safety property is upstream of it: the value
    // comes from a variable nobody sets by accident.
    std::env::set_var("DISPLAY", &display);
    // Left over from a shell that exported it. `detect_display_server` prefers
    // Wayland when both are set, and this test is about the X11 backend.
    std::env::remove_var("WAYLAND_DISPLAY");

    let observer = Observer::connect(&display);
    let mut backend = linux_x11::backend().expect("the X11 backend is never fatal");

    // -- what the node advertises ------------------------------------------

    let caps = backend.current_capabilities();
    assert!(
        caps.contains(Capabilities::INJECT_INPUT),
        "a reachable X server with XTEST must advertise injection, or nothing routed \
         here can ever be delivered; advertised {caps:?}"
    );

    let monitors = backend
        .displays
        .monitors()
        .expect("RandR must answer on a server that has it");
    assert!(
        !monitors.is_empty(),
        "the test server has a screen; enumerating none would keep this node out of \
         the layout entirely"
    );
    assert!(
        caps.contains(Capabilities::HAS_DISPLAYS),
        "screens were enumerated but not advertised"
    );
    let screen: Monitor = monitors[0].clone();
    assert!(!screen.local_bounds.is_empty());
    assert_eq!(
        monitors.iter().filter(|m| m.primary).count(),
        1,
        "exactly one screen has to be primary or the layout has no anchor"
    );

    // -- the pointer -------------------------------------------------------

    backend
        .injector
        .warp_cursor(&screen, NormPos::new(0.25, 0.75))
        .expect("warping the cursor");
    settle(&backend);
    let at = observer.pointer();
    let expected_x = screen.local_bounds.x + (screen.local_bounds.w as f64 * 0.25) as i32;
    let expected_y = screen.local_bounds.y + (screen.local_bounds.h as f64 * 0.75) as i32;
    assert!(
        (i32::from(at.root_x) - expected_x).abs() <= 1
            && (i32::from(at.root_y) - expected_y).abs() <= 1,
        "warp landed at ({}, {}) rather than ({expected_x}, {expected_y})",
        at.root_x,
        at.root_y
    );

    // Relative motion, which is the other half of the pointer path and the one a
    // pointer-locked application gets.
    let before = observer.pointer();
    backend
        .injector
        .inject(
            &screen,
            &InputEvent::Pointer(PointerEvent::MoveBy { dx: 10.0, dy: -5.0 }),
        )
        .expect("relative motion");
    settle(&backend);
    let after = observer.pointer();
    assert_eq!(
        (after.root_x - before.root_x, after.root_y - before.root_y),
        (10, -5),
        "relative motion did not move the pointer by the delta it was given"
    );

    // -- a mouse button ----------------------------------------------------

    backend
        .injector
        .inject(
            &screen,
            &InputEvent::Pointer(PointerEvent::Button {
                button: MouseButton::Left,
                pressed: true,
            }),
        )
        .expect("button press");
    settle(&backend);
    assert_eq!(
        observer.buttons_down(),
        vec![1],
        "the left button did not go down on the server"
    );
    backend
        .injector
        .inject(
            &screen,
            &InputEvent::Pointer(PointerEvent::Button {
                button: MouseButton::Left,
                pressed: false,
            }),
        )
        .expect("button release");
    settle(&backend);
    assert!(observer.buttons_down().is_empty());

    // -- a character, resolved through the server's own layout -------------

    let a = observer.keycode_for('a');
    backend
        .injector
        .inject(
            &screen,
            &InputEvent::Key(KeyEvent::text("a", KeyAction::Press, Modifiers::NONE)),
        )
        .expect("typing a letter");
    settle(&backend);
    assert!(
        observer.key_down(a),
        "`a` was resolved to some other key: down are {:?}, expected {a}",
        observer.keys_down()
    );
    backend
        .injector
        .inject(
            &screen,
            &InputEvent::Key(KeyEvent::text("a", KeyAction::Release, Modifiers::NONE)),
        )
        .expect("releasing a letter");
    settle(&backend);
    assert!(!observer.key_down(a));
    assert!(
        observer.keys_down().is_empty(),
        "a plain letter left something else down: {:?}",
        observer.keys_down()
    );

    // A capital needs Shift held, and it has to come back up with the keystroke —
    // held past the key-up it reaches the next event and a following Home arrives
    // as Shift+Home.
    backend
        .injector
        .inject(
            &screen,
            &InputEvent::Key(KeyEvent::text("A", KeyAction::Press, Modifiers::SHIFT)),
        )
        .expect("typing a capital");
    settle(&backend);
    let held = observer.keys_down();
    assert!(held.contains(&a), "the letter key is not down: {held:?}");
    assert_eq!(held.len(), 2, "expected the letter and one Shift: {held:?}");
    backend
        .injector
        .inject(
            &screen,
            &InputEvent::Key(KeyEvent::text("A", KeyAction::Release, Modifiers::SHIFT)),
        )
        .expect("releasing a capital");
    settle(&backend);
    assert!(
        observer.keys_down().is_empty(),
        "a capital left a key down: {:?}",
        observer.keys_down()
    );

    // -- a special key, which resolves by keysym rather than by character ---

    backend
        .injector
        .inject(
            &screen,
            &InputEvent::Key(KeyEvent::special(
                SpecialKey::F5,
                KeyAction::Press,
                Modifiers::NONE,
            )),
        )
        .expect("pressing F5");
    settle(&backend);
    assert_eq!(
        observer.keys_down(),
        vec![observer.keycode_for_keysym(0xffc2)],
        "F5 pressed something other than the key the layout calls F5"
    );
    backend
        .injector
        .inject(
            &screen,
            &InputEvent::Key(KeyEvent::special(
                SpecialKey::F5,
                KeyAction::Release,
                Modifiers::NONE,
            )),
        )
        .expect("releasing F5");
    settle(&backend);

    // -- release_all, which is the one that must never be wrong -------------

    backend
        .injector
        .inject(
            &screen,
            &InputEvent::Pointer(PointerEvent::Button {
                button: MouseButton::Left,
                pressed: true,
            }),
        )
        .expect("button press");
    backend
        .injector
        .inject(
            &screen,
            &InputEvent::Key(KeyEvent::special(
                SpecialKey::ShiftLeft,
                KeyAction::Press,
                Modifiers::NONE,
            )),
        )
        .expect("a forwarded bare modifier");
    backend
        .injector
        .inject(
            &screen,
            &InputEvent::Key(KeyEvent::text("c", KeyAction::Press, Modifiers::CTRL)),
        )
        .expect("a chord");
    settle(&backend);
    assert!(
        observer.keys_down().len() >= 3,
        "the setup for the release did not press enough: {:?}",
        observer.keys_down()
    );

    backend.injector.release_all().expect("release_all");
    settle(&backend);
    assert!(
        observer.keys_down().is_empty(),
        "release_all left keys down on the server: {:?} — a stuck modifier cannot be \
         cleared without walking to that machine",
        observer.keys_down()
    );
    assert!(
        observer.buttons_down().is_empty(),
        "release_all left a mouse button down: {:?} — which drags whatever is under it",
        observer.buttons_down()
    );

    // Idempotent, because it runs on every disconnect and at shutdown.
    backend
        .injector
        .release_all()
        .expect("a second release_all");
    settle(&backend);
    assert!(observer.keys_down().is_empty());

    // -- what it refuses ---------------------------------------------------

    // Capture and suppression are deliberately not implemented, and must stay
    // refusals rather than quiet successes.
    assert!(backend.capture.set_suppress_local(true).is_err());
    assert!(!backend.capture.suppresses_local());
}
