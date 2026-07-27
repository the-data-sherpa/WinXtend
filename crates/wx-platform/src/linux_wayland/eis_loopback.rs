//! Driving the whole injection path against a real libei server, with no
//! compositor and no consent dialog.
//!
//! # Why this exists
//!
//! Everything else that tests this backend stops at the transport: [`Injector`]'s
//! own tests run with no devices and assert about bookkeeping, and the granted
//! branch of [`super`] needs a human on the *Share* button. Between the two sits
//! the part the product is actually judged on — that "the peer sent `[`" comes out
//! of the wire as the keycode and modifiers that produce `[` *on the layout the
//! receiver has loaded*. That is protocol serialisation, device selection, keymap
//! resolution and frame discipline together, and none of it is exercised by a test
//! that never opens a socket.
//!
//! So this module stands up the other end. `reis` implements EIS as well as EI, so
//! a `socketpair` with a server on one side is a complete libei session: the real
//! handshake, the real `ei_seat`/`ei_device` objects, the real keymap file
//! descriptor, and the real requests arriving on the far side to be recorded. The
//! client half is not a mock at all — it is [`super::on_ei_event`] and
//! [`Transport`], the same code the portal path runs.
//!
//! What is still not covered: the portal, consent, and the compositor's own
//! interpretation of what it receives. Those are verified by hand (see `AGENTS.md`).
//!
//! # The transcript
//!
//! Each test prints the requests the server received, prefixed `WIRE`. That is
//! deliberate: fed back through `libxkbcommon` — the same library the compositor
//! resolves keycodes with — the keyboard lines spell the string that was sent,
//! which is the end-user claim this backend makes and not something an assertion
//! about keycodes can show on its own.

use std::fs::File;
use std::io::Write as _;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use reis::ei;
use reis::eis;
use reis::handshake::EisHandshaker;
use reis::request::{DeviceCapability, EisRequest, EisRequestConverter};
use reis::PendingRequestResult;

use wx_proto::{
    InputEvent, KeyAction, KeyEvent, KeyPayload, Modifiers, Monitor, MonitorId, MouseButton,
    NormPos, PointerEvent, Rect, ScrollUnit, SpecialKey,
};

use super::super::inject::{Injector, Transport};

/// Real keymaps, not sketches. `US` is the 36 KB text dumped straight off the alpha
/// target's compositor — the same bytes `ei_keyboard.keymap` carries. `US_NO` is
/// the two-group keymap a user with a second input source gets, compiled by
/// `libxkbcommon` the way the compositor compiles its own; the layout-group
/// handling exists for exactly that case.
const US_KEYMAP: &str = include_str!("../../tests/data/keymap-us.xkb");
const US_NO_KEYMAP: &str = include_str!("../../tests/data/keymap-us-no.xkb");

/// How long a test will wait for the server thread to do something before giving
/// up. Generous: the work is microseconds, and a slow CI box must not flake.
const DEADLINE: Duration = Duration::from_secs(10);

/// One request the server received, in the form the assertions care about.
#[derive(Debug, Clone, PartialEq)]
enum Wire {
    StartEmulating { device: String },
    StopEmulating { device: String },
    Key { code: u32, down: bool },
    Button { code: u32, down: bool },
    MotionAbsolute { x: f32, y: f32 },
    MotionRelative { dx: f32, dy: f32 },
    Scroll { dx: f32, dy: f32 },
    ScrollDiscrete { dx: i32, dy: i32 },
    Frame { device: String },
}

impl Wire {
    /// The line a test prints for this request. Parsed by the replay script that
    /// turns the keyboard lines back into text; see the module docs.
    fn line(&self) -> String {
        match self {
            Self::StartEmulating { device } => format!("start_emulating {device}"),
            Self::StopEmulating { device } => format!("stop_emulating {device}"),
            Self::Key { code, down } => {
                format!("key {code} {}", if *down { "press" } else { "release" })
            }
            Self::Button { code, down } => {
                format!("button {code} {}", if *down { "press" } else { "release" })
            }
            Self::MotionAbsolute { x, y } => format!("motion_absolute {x} {y}"),
            Self::MotionRelative { dx, dy } => format!("motion_relative {dx} {dy}"),
            Self::Scroll { dx, dy } => format!("scroll {dx} {dy}"),
            Self::ScrollDiscrete { dx, dy } => format!("scroll_discrete {dx} {dy}"),
            Self::Frame { device } => format!("frame {device}"),
        }
    }
}

/// What the server thread recorded, shared with the test thread.
#[derive(Default)]
struct Recorder {
    wire: Mutex<Vec<Wire>>,
}

impl Recorder {
    fn push(&self, item: Wire) {
        self.wire.lock().unwrap().push(item);
    }

    fn all(&self) -> Vec<Wire> {
        self.wire.lock().unwrap().clone()
    }

    /// Everything recorded since `from` that the injector chose to send.
    ///
    /// Frames are dropped because they are asserted about separately: every event
    /// needs one, and repeating them in each expectation would drown the thing
    /// being tested. Emulation start and stop are dropped because they are the
    /// client answering the compositor rather than injecting anything, and
    /// [`Harness::sync`] provokes them deliberately.
    fn since(&self, from: usize) -> Vec<Wire> {
        self.wire.lock().unwrap()[from..]
            .iter()
            .filter(|w| {
                !matches!(
                    w,
                    Wire::Frame { .. } | Wire::StartEmulating { .. } | Wire::StopEmulating { .. }
                )
            })
            .cloned()
            .collect()
    }

    fn len(&self) -> usize {
        self.wire.lock().unwrap().len()
    }

    fn count(&self, pred: impl Fn(&Wire) -> bool) -> usize {
        self.wire.lock().unwrap().iter().filter(|w| pred(w)).count()
    }
}

/// Something the test asks the server to do to the client, the way a compositor
/// would.
enum Command {
    /// `ei_keyboard.modifiers`: Caps Lock, and the active layout group.
    Modifiers {
        locked: u32,
        group: u32,
    },
    /// `ei_device.paused` on every device, then `ei_device.resumed`.
    PauseAndResume,
    Stop,
}

/// The devices the server offers, mirroring what the alpha target offers: a
/// relative pointer, a keyboard carrying a keymap, and an absolute pointer whose
/// regions are the monitors.
struct Devices {
    keyboard: reis::request::Device,
    pointer: reis::request::Device,
    absolute: reis::request::Device,
    /// Held open only so the keymap file descriptor outlives the `done()` that
    /// tells the client to read it.
    _keymap: File,
}

impl Devices {
    fn each(&self) -> [&reis::request::Device; 3] {
        [&self.keyboard, &self.pointer, &self.absolute]
    }
}

/// A keymap on a file descriptor, the way `ei_keyboard.keymap` carries one.
///
/// Unlinked immediately, so a test that panics leaves nothing behind.
fn keymap_file(text: &str) -> (File, u32) {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let path = std::env::temp_dir().join(format!(
        "wx-eis-keymap-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = File::create(&path).expect("creating the keymap file");
    file.write_all(text.as_bytes()).expect("writing the keymap");
    // The compositor's keymap is NUL-terminated and its `size` counts the NUL.
    file.write_all(&[0]).expect("terminating the keymap");
    let read = File::open(&path).expect("reopening the keymap");
    std::fs::remove_file(&path).expect("unlinking the keymap");
    let size = text.len() as u32 + 1;
    (read, size)
}

/// Wait for the socket to have something on it, or for the timeout to pass.
fn poll_readable<T: AsFd>(fd: &T, timeout: Duration) {
    let spec = rustix::time::Timespec {
        tv_sec: timeout.as_secs() as _,
        tv_nsec: timeout.subsec_nanos() as _,
    };
    let _ = rustix::event::poll(
        &mut [rustix::event::PollFd::new(fd, rustix::event::PollFlags::IN)],
        Some(&spec),
    );
}

/// The EIS half: a compositor's side of one libei session.
fn serve(
    stream: UnixStream,
    keymap: &'static str,
    recorder: Arc<Recorder>,
    cmds: Receiver<Command>,
) {
    let context = eis::Context::new(stream).expect("creating the EIS context");
    let mut handshaker = EisHandshaker::new(&context, 1);
    let mut converter: Option<EisRequestConverter> = None;
    let mut seat: Option<reis::request::Seat> = None;
    let mut devices: Option<Devices> = None;

    loop {
        // Short timeout rather than a blocking wait: the command channel has to be
        // serviced between reads, and a test that wedges must end by deadline
        // rather than by hanging the whole run.
        poll_readable(&context, Duration::from_millis(20));

        while let Ok(cmd) = cmds.try_recv() {
            match cmd {
                Command::Stop => return,
                Command::Modifiers { locked, group } => {
                    if let Some(devices) = &devices {
                        let keyboard = devices
                            .keyboard
                            .interface::<eis::Keyboard>()
                            .expect("the keyboard device has an ei_keyboard");
                        keyboard.modifiers(1, 0, locked, 0, group);
                    }
                }
                Command::PauseAndResume => {
                    if let Some(devices) = &devices {
                        for device in devices.each() {
                            device.paused();
                        }
                        for device in devices.each() {
                            device.resumed();
                        }
                    }
                }
            }
            if let Some(c) = &converter {
                let _ = c.handle().flush();
            }
        }

        match context.read() {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(_) => return,
        }

        while let Some(pending) = context.pending_request() {
            let PendingRequestResult::Request(request) = pending else {
                continue;
            };
            match &mut converter {
                None => {
                    let resp = handshaker
                        .handle_request(request)
                        .expect("the client's handshake");
                    if let Some(resp) = resp {
                        let c = EisRequestConverter::new(&context, resp, 1);
                        let handle = c.handle().clone();
                        seat = Some(handle.add_seat(
                            Some("winxtend-loopback"),
                            DeviceCapability::Pointer
                                | DeviceCapability::PointerAbsolute
                                | DeviceCapability::Keyboard
                                | DeviceCapability::Button
                                | DeviceCapability::Scroll,
                        ));
                        let _ = handle.flush();
                        converter = Some(c);
                    }
                }
                Some(c) => {
                    if c.handle_request(request).is_err() {
                        return;
                    }
                    while let Some(request) = c.next_request() {
                        if record(&request, &recorder) {
                            return;
                        }
                        if let EisRequest::Bind(bind) = &request {
                            if devices.is_none()
                                && bind.capabilities.contains(DeviceCapability::Keyboard)
                            {
                                devices =
                                    Some(offer_devices(seat.as_ref().expect("a seat"), keymap));
                            }
                        }
                    }
                    let _ = c.handle().flush();
                }
            }
        }
    }
}

/// The three devices, offered and resumed.
fn offer_devices(seat: &reis::request::Seat, keymap: &str) -> Devices {
    let (file, size) = keymap_file(keymap);
    let keyboard = seat.add_device(
        Some("winxtend-keyboard"),
        eis::device::DeviceType::Virtual,
        DeviceCapability::Keyboard.into(),
        |device| {
            device
                .interface::<eis::Keyboard>()
                .expect("the keyboard device has an ei_keyboard")
                .keymap(eis::keyboard::KeymapType::Xkb, size, file.as_fd());
        },
    );
    let pointer = seat.add_device(
        Some("winxtend-pointer"),
        eis::device::DeviceType::Virtual,
        DeviceCapability::Pointer | DeviceCapability::Button | DeviceCapability::Scroll,
        |_| {},
    );
    // The regions are the monitors: a position outside every one of them is what
    // the injector refuses rather than letting the compositor drop it silently.
    let absolute = seat.add_device(
        Some("winxtend-absolute"),
        eis::device::DeviceType::Virtual,
        DeviceCapability::PointerAbsolute | DeviceCapability::Button | DeviceCapability::Scroll,
        |device| {
            device.device().region(0, 0, 1920, 1080, 1.0);
        },
    );
    for device in [&keyboard, &pointer, &absolute] {
        device.resumed();
    }
    Devices {
        keyboard,
        pointer,
        absolute,
        _keymap: file,
    }
}

/// Record one request. Returns true when the client has disconnected.
fn record(request: &EisRequest, recorder: &Recorder) -> bool {
    let name = |d: &reis::request::Device| d.name().unwrap_or("?").to_string();
    match request {
        EisRequest::Disconnect => return true,
        EisRequest::DeviceStartEmulating(e) => recorder.push(Wire::StartEmulating {
            device: name(&e.device),
        }),
        EisRequest::DeviceStopEmulating(e) => recorder.push(Wire::StopEmulating {
            device: name(&e.device),
        }),
        EisRequest::KeyboardKey(e) => recorder.push(Wire::Key {
            code: e.key,
            down: e.state == eis::keyboard::KeyState::Press,
        }),
        EisRequest::Button(e) => recorder.push(Wire::Button {
            code: e.button,
            down: e.state == eis::button::ButtonState::Press,
        }),
        EisRequest::PointerMotionAbsolute(e) => recorder.push(Wire::MotionAbsolute {
            x: e.dx_absolute,
            y: e.dy_absolute,
        }),
        EisRequest::PointerMotion(e) => recorder.push(Wire::MotionRelative { dx: e.dx, dy: e.dy }),
        EisRequest::ScrollDelta(e) => recorder.push(Wire::Scroll { dx: e.dx, dy: e.dy }),
        EisRequest::ScrollDiscrete(e) => recorder.push(Wire::ScrollDiscrete {
            dx: e.discrete_dx,
            dy: e.discrete_dy,
        }),
        EisRequest::Frame(e) => recorder.push(Wire::Frame {
            device: name(&e.device),
        }),
        _ => {}
    }
    false
}

/// One live loopback session: server thread, client pump thread, and an injector
/// wired to the same [`Transport`] the driver would give it.
struct Harness {
    injector: Injector,
    recorder: Arc<Recorder>,
    commands: Sender<Command>,
    monitor: Monitor,
}

impl Harness {
    fn start(keymap: &'static str) -> Self {
        let (client, server) = UnixStream::pair().expect("a socketpair");
        let recorder = Arc::new(Recorder::default());
        let (commands, rx) = mpsc::channel();

        let served = Arc::clone(&recorder);
        std::thread::spawn(move || serve(server, keymap, served, rx));

        let transport = Arc::new(Transport::new());

        // The client half runs on its own thread because `reis`'s converter is not
        // `Send` — in the product that thread is the driver's, with a tokio runtime
        // on it. The `Connection` that comes out *is* shared, which is the whole
        // reason the injector can write from whichever thread the agent calls on.
        //
        // Deliberately not `ei::Context::handshake_blocking`: its iterator polls
        // the socket before handing back an event it has already decoded, so a
        // server that says nothing more until the client answers deadlocks. The
        // loop below is what the driver's async stream does — drain first, then
        // wait.
        let pumped = Arc::clone(&transport);
        let (ready_tx, ready_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let context = ei::Context::new(client).expect("creating the ei context");
            let resp = reis::handshake::ei_handshake_blocking(
                &context,
                "winxtend-loopback",
                ei::handshake::ContextType::Sender,
            )
            .expect("the libei handshake");
            let mut converter = reis::event::EiEventConverter::new(&context, resp);
            let connection = converter.connection().clone();
            pumped.attach(connection.clone());
            ready_tx.send(()).expect("the test is still waiting");

            loop {
                // The same handler the driver calls: this is what turns
                // `DeviceAdded`/`DeviceResumed` into a usable transport, reads the
                // keymap off the file descriptor, and starts the devices emulating.
                while let Some(event) = converter.next_event() {
                    if super::on_ei_event(&connection, &pumped, event).is_some() {
                        return;
                    }
                }
                poll_readable(&context, Duration::from_millis(20));
                match context.read() {
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                    Err(_) => return,
                }
                while let Some(pending) = context.pending_event() {
                    let PendingRequestResult::Request(event) = pending else {
                        continue;
                    };
                    if converter.handle_event(event).is_err() {
                        return;
                    }
                }
            }
        });
        ready_rx
            .recv_timeout(DEADLINE)
            .expect("the libei handshake completed");

        let harness = Self {
            injector: Injector::new(transport),
            recorder,
            commands,
            monitor: Monitor {
                id: MonitorId(0),
                name: "loopback".into(),
                local_bounds: Rect::new(0, 0, 1920, 1080),
                scale: 1.0,
                primary: true,
            },
        };
        // The devices are usable once the client has answered `resumed` with
        // `start_emulating` for all three — the point before which everything sent
        // is discarded.
        harness.wait_until(
            |r| r.count(|w| matches!(w, Wire::StartEmulating { .. })) >= 3,
            "the devices to start emulating",
        );
        harness
    }

    fn wait_until(&self, done: impl Fn(&Recorder) -> bool, what: &str) {
        let deadline = Instant::now() + DEADLINE;
        while Instant::now() < deadline {
            if done(&self.recorder) {
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        panic!(
            "timed out waiting for {what}; recorded {:?}",
            self.recorder.all()
        );
    }

    /// Where the recording is now, so a test can talk about what its own step did.
    fn mark(&self) -> usize {
        self.recorder.len()
    }

    /// A barrier in both directions, so no assertion here is a race.
    ///
    /// A pause-and-resume is the cheapest thing a compositor can do that the
    /// client visibly answers: seeing the three `start_emulating` requests come
    /// back means the client has processed every event sent before them — a
    /// modifier change, say — *and* that everything it sent earlier has already
    /// been read on this side, because a socket keeps its order.
    fn sync(&self) {
        let before = self
            .recorder
            .count(|w| matches!(w, Wire::StartEmulating { .. }));
        self.tell(Command::PauseAndResume);
        self.wait_until(
            |r| r.count(|w| matches!(w, Wire::StartEmulating { .. })) >= before + 3,
            "the round trip that proves both sides are caught up",
        );
    }

    /// Everything the injector sent since `mark`, once all of it has arrived.
    fn drain(&self, mark: usize) -> Vec<Wire> {
        self.sync();
        self.recorder.since(mark)
    }

    fn type_text(&mut self, text: &str) {
        for c in text.chars() {
            let press = KeyEvent::text(c.to_string(), KeyAction::Press, Modifiers::NONE);
            self.inject(&InputEvent::Key(press));
            let release = KeyEvent::text(c.to_string(), KeyAction::Release, Modifiers::NONE);
            self.inject(&InputEvent::Key(release));
        }
    }

    fn inject(&mut self, event: &InputEvent) {
        let monitor = self.monitor.clone();
        self.injector
            .inject(&monitor, event)
            .unwrap_or_else(|e| panic!("injecting {event:?}: {e}"));
    }

    fn tell(&self, command: Command) {
        self.commands.send(command).expect("the server is running");
    }

    /// Print the transcript so the run itself shows what reached the wire.
    fn transcript(&self, title: &str, mark: usize) {
        println!("WIRE-BEGIN {title}");
        for item in self.recorder.wire.lock().unwrap()[mark..].iter() {
            println!("WIRE {}", item.line());
        }
        println!("WIRE-END {title}");
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Stop);
    }
}

/// Keycodes this test names directly, so an expectation reads as the key rather
/// than as a number. From `linux/input-event-codes.h`.
const KEY_LEFTSHIFT: u32 = 42;
const KEY_LEFTCTRL: u32 = 29;
const KEY_RIGHTALT: u32 = 100;
const KEY_A: u32 = 30;
const KEY_HOME: u32 = 102;
const KEY_1: u32 = 2;
const BTN_LEFT: u32 = 0x110;

#[test]
fn a_string_reaches_the_wire_as_the_keys_that_type_it() {
    // The product claim, at the level the compositor sees it. The transcript this
    // prints is replayed through libxkbcommon to show it spells the string back;
    // the assertions here pin the two things a replay would not notice.
    let mut h = Harness::start(US_KEYMAP);
    let mark = h.mark();
    h.type_text("Hi!");
    // 'H' shift down, H down, H up, shift up; 'i' down, up; '!' shift down, 1
    // down, 1 up, shift up.
    let wire = h.drain(mark);
    h.transcript("us-hi", mark);

    assert_eq!(
        wire,
        vec![
            Wire::Key {
                code: KEY_LEFTSHIFT,
                down: true
            },
            Wire::Key {
                code: 35,
                down: true
            },
            Wire::Key {
                code: 35,
                down: false
            },
            Wire::Key {
                code: KEY_LEFTSHIFT,
                down: false
            },
            Wire::Key {
                code: 23,
                down: true
            },
            Wire::Key {
                code: 23,
                down: false
            },
            Wire::Key {
                code: KEY_LEFTSHIFT,
                down: true
            },
            Wire::Key {
                code: KEY_1,
                down: true
            },
            Wire::Key {
                code: KEY_1,
                down: false
            },
            Wire::Key {
                code: KEY_LEFTSHIFT,
                down: false
            },
        ],
        "the keys that produce `Hi!` on a US layout"
    );

    // Every event framed, or the compositor would have seen none of it.
    assert_eq!(
        h.recorder.count(|w| matches!(w, Wire::Frame { .. })),
        h.recorder.count(|w| matches!(w, Wire::Key { .. })),
        "every key needs its own frame or nothing is delivered"
    );
}

/// Everything the peer might reasonably send as text in one go, on the layout the
/// alpha target actually loads. Long enough that a replay spelling it back is
/// evidence rather than a coincidence.
const SENTENCE: &str = "Hello, WinXtend! 100% ~ (a+b)/c = {d}; \"e\" & 'f' <g> #3 \\_/";

#[test]
fn a_whole_sentence_survives_the_round_trip_on_the_us_layout() {
    // The transcript this prints is replayed through `libxkbcommon` to show it
    // spells `SENTENCE` back. What is asserted here is what a replay cannot see:
    // that nothing is left held down afterwards.
    let mut h = Harness::start(US_KEYMAP);
    let mark = h.mark();
    h.type_text(SENTENCE);
    let wire = h.drain(mark);
    h.transcript("us-sentence", mark);

    let mut down: Vec<u32> = Vec::new();
    for item in &wire {
        if let Wire::Key {
            code,
            down: is_down,
        } = item
        {
            if *is_down {
                assert!(!down.contains(code), "key {code} pressed twice: {wire:?}");
                down.push(*code);
            } else {
                let at = down
                    .iter()
                    .position(|c| c == code)
                    .unwrap_or_else(|| panic!("key {code} released without a press"));
                down.remove(at);
            }
        }
    }
    assert!(
        down.is_empty(),
        "typing a sentence must leave nothing held down: {down:?}"
    );
}

#[test]
fn a_whole_sentence_survives_the_round_trip_on_a_norwegian_layout() {
    // The cross-layout promise over a whole string rather than one character: none
    // of these are where a US layout puts them, and `é` is not on the layout at all.
    const NORWEGIAN: &str = "Blåbærsyltetøy! [x] {y} @ € é";
    let mut h = Harness::start(US_NO_KEYMAP);
    h.tell(Command::Modifiers {
        locked: 0,
        group: 1,
    });
    h.sync();

    let mark = h.mark();
    h.type_text(NORWEGIAN);
    let wire = h.drain(mark);
    h.transcript("no-sentence", mark);

    assert!(
        wire.iter().any(|w| matches!(
            w,
            Wire::Key {
                code: KEY_RIGHTALT,
                ..
            }
        )),
        "this layout needs AltGr for several of these, which a US-shaped answer would not"
    );
}

#[test]
fn a_shift_held_to_reach_a_level_does_not_reach_the_next_key() {
    // The stuck-Shift defect, at the wire: a `!` left Shift down and turned the
    // Home that followed into Shift+Home, which selects the line.
    let mut h = Harness::start(US_KEYMAP);
    let mark = h.mark();
    h.type_text("!");
    h.inject(&InputEvent::Key(KeyEvent::special(
        SpecialKey::Home,
        KeyAction::Press,
        Modifiers::NONE,
    )));
    let wire = h.drain(mark);
    h.transcript("us-shift-release", mark);

    assert_eq!(
        wire,
        vec![
            Wire::Key {
                code: KEY_LEFTSHIFT,
                down: true
            },
            Wire::Key {
                code: KEY_1,
                down: true
            },
            Wire::Key {
                code: KEY_1,
                down: false
            },
            Wire::Key {
                code: KEY_LEFTSHIFT,
                down: false
            },
            Wire::Key {
                code: KEY_HOME,
                down: true
            },
        ],
        "Shift must come back up with the `!` it was held for"
    );
}

#[test]
fn a_chord_holds_its_modifier_across_the_key_and_release_all_clears_it() {
    // Ctrl+A as a real chord, and then the handoff: everything this injector
    // pressed comes back up, newest first with the modifier last.
    let mut h = Harness::start(US_KEYMAP);
    let mark = h.mark();
    h.inject(&InputEvent::Key(KeyEvent::text(
        "a",
        KeyAction::Press,
        Modifiers::CTRL,
    )));
    let wire = h.drain(mark);
    assert_eq!(
        wire,
        vec![
            Wire::Key {
                code: KEY_LEFTCTRL,
                down: true
            },
            Wire::Key {
                code: KEY_A,
                down: true
            },
        ],
        "Ctrl is held down around the key rather than baked into it"
    );

    // The handoff, with the `a` still down: everything this injector pressed comes
    // back up, the key first and the modifier last.
    let mark = h.mark();
    h.inject(&InputEvent::ReleaseControl);
    let wire = h.drain(mark);
    h.transcript("us-ctrl-a-release-all", mark);
    assert_eq!(
        wire,
        vec![
            Wire::Key {
                code: KEY_A,
                down: false
            },
            Wire::Key {
                code: KEY_LEFTCTRL,
                down: false
            },
        ],
        "release_all lets go of exactly what was still held, modifiers last"
    );
}

#[test]
fn caps_lock_on_the_receiving_desktop_inverts_the_shift() {
    // Caps Lock is locked by the *user*, not by this injector, and must not be
    // cleared. So `a` with it on has to be typed as Shift+a to come out as `a`.
    let mut h = Harness::start(US_KEYMAP);
    h.tell(Command::Modifiers {
        locked: 1 << 1,
        group: 0,
    });
    // The modifier event has to reach the client before the keystroke is resolved.
    h.sync();

    let mark = h.mark();
    h.type_text("a");
    let wire = h.drain(mark);
    h.transcript("us-caps-lock", mark);
    assert_eq!(
        wire,
        vec![
            Wire::Key {
                code: KEY_LEFTSHIFT,
                down: true
            },
            Wire::Key {
                code: KEY_A,
                down: true
            },
            Wire::Key {
                code: KEY_A,
                down: false
            },
            Wire::Key {
                code: KEY_LEFTSHIFT,
                down: false
            },
        ],
        "with Caps Lock on, typing `a` means pressing Shift"
    );
}

#[test]
fn a_layout_switch_mid_session_is_re_read_and_changes_the_answer() {
    // The cross-layout promise. `[` is on the same physical key as `å` on a
    // Norwegian layout; a receiver that switched input source mid-session must
    // still type `[` when the peer sends `[`, which means resolving against
    // group 1 rather than group 0.
    let mut h = Harness::start(US_NO_KEYMAP);

    let mark = h.mark();
    h.type_text("[");
    let us = h.drain(mark);
    h.transcript("group0-bracket", mark);
    assert_eq!(
        us,
        vec![
            Wire::Key {
                code: 26,
                down: true
            },
            Wire::Key {
                code: 26,
                down: false
            },
        ],
        "on the US group `[` is the unshifted key next to P"
    );

    // What GNOME's own Super+Space does, as far as this client can tell.
    h.tell(Command::Modifiers {
        locked: 0,
        group: 1,
    });
    h.sync();

    let mark = h.mark();
    h.type_text("[å");
    let no = h.drain(mark);
    h.transcript("group1-bracket-aring", mark);
    assert_eq!(
        no,
        vec![
            // `[` on the Norwegian group is AltGr+8 — not the key that carries it
            // on US, which now types `å`.
            Wire::Key {
                code: KEY_RIGHTALT,
                down: true
            },
            Wire::Key {
                code: 9,
                down: true
            },
            Wire::Key {
                code: 9,
                down: false
            },
            Wire::Key {
                code: KEY_RIGHTALT,
                down: false
            },
            // ...and `å` is the key `[` sits on over on the US group.
            Wire::Key {
                code: 26,
                down: true
            },
            Wire::Key {
                code: 26,
                down: false
            },
        ],
        "after the group change `[` must still type `[`, not the key it shares"
    );
}

#[test]
fn a_character_the_layout_cannot_produce_is_refused_rather_than_mistyped() {
    // The documented limit, and the shape of it that matters: an error the peer
    // is told about, and not some other key pressed on the user's desktop.
    let mut h = Harness::start(US_KEYMAP);
    let mark = h.mark();
    let event = InputEvent::Key(KeyEvent::text("漢", KeyAction::Press, Modifiers::NONE));
    let err = h
        .injector
        .inject(&h.monitor.clone(), &event)
        .expect_err("a US layout cannot produce this character");
    println!("WIRE-BEGIN us-unproducible");
    println!("WIRE error {err}");
    println!("WIRE-END us-unproducible");

    h.sync();
    assert!(
        h.recorder.since(mark).is_empty(),
        "nothing may be pressed for a character that cannot be typed"
    );
}

#[test]
fn pointer_motion_buttons_and_scroll_arrive_as_the_compositor_expects() {
    let mut h = Harness::start(US_KEYMAP);
    let mark = h.mark();

    // Absolute: the middle of a 1920x1080 monitor is the middle of the region.
    h.inject(&InputEvent::Pointer(PointerEvent::MoveTo {
        pos: NormPos::new(0.5, 0.25),
    }));
    // Relative motion goes on the relative device, untouched.
    h.inject(&InputEvent::Pointer(PointerEvent::MoveBy {
        dx: 7.0,
        dy: -3.0,
    }));
    h.inject(&InputEvent::Pointer(PointerEvent::Button {
        button: MouseButton::Left,
        pressed: true,
    }));
    h.inject(&InputEvent::Pointer(PointerEvent::Button {
        button: MouseButton::Left,
        pressed: false,
    }));
    // One notch of a notched wheel, away from the user.
    h.inject(&InputEvent::Pointer(PointerEvent::Scroll {
        dx: 0.0,
        dy: 1.0,
        unit: ScrollUnit::Lines,
    }));
    let wire = h.drain(mark);
    h.transcript("pointer", mark);

    assert_eq!(
        wire,
        vec![
            Wire::MotionAbsolute { x: 960.0, y: 270.0 },
            Wire::MotionRelative { dx: 7.0, dy: -3.0 },
            Wire::Button {
                code: BTN_LEFT,
                down: true
            },
            Wire::Button {
                code: BTN_LEFT,
                down: false
            },
            // Exactly one request per detent, and negated: the wire follows the
            // Windows sense, libei follows Wayland's.
            Wire::ScrollDiscrete { dx: 0, dy: -120 },
        ],
    );

    // The measured defect this guards: sending the smooth scroll as well made one
    // notch move two and a half lines on the alpha target.
    assert_eq!(
        h.recorder.count(|w| matches!(w, Wire::Scroll { .. })),
        0,
        "a line scroll must not also send the smooth value"
    );

    // And a position on a monitor the session does not cover is an error rather
    // than something the compositor drops in silence.
    let off_session = Monitor {
        id: MonitorId(1),
        name: "not-shared".into(),
        local_bounds: Rect::new(3000, 0, 1920, 1080),
        scale: 1.0,
        primary: false,
    };
    let err = h
        .injector
        .inject(
            &off_session,
            &InputEvent::Pointer(PointerEvent::MoveTo {
                pos: NormPos::new(0.5, 0.5),
            }),
        )
        .expect_err("the portal session covers no such screen");
    println!("WIRE-BEGIN pointer-off-session");
    println!("WIRE error {err}");
    println!("WIRE-END pointer-off-session");
}

#[test]
fn a_device_the_compositor_paused_and_resumed_still_injects() {
    // Mutter pauses the devices around session suspension. Before the fix a
    // resume left the transport unusable for the rest of the process while the
    // session went on advertising injection.
    let mut h = Harness::start(US_KEYMAP);
    h.tell(Command::PauseAndResume);
    h.wait_until(
        |r| r.count(|w| matches!(w, Wire::StartEmulating { .. })) >= 6,
        "the devices to start emulating again",
    );

    let mark = h.mark();
    h.type_text("a");
    let wire = h.drain(mark);
    h.transcript("resume", mark);
    assert_eq!(
        wire,
        vec![
            Wire::Key {
                code: KEY_A,
                down: true
            },
            Wire::Key {
                code: KEY_A,
                down: false
            },
        ],
        "a resumed keyboard still types, and still has its keymap"
    );
}

#[test]
fn a_dead_key_composes_a_character_the_layout_has_no_key_for() {
    // `é` is not on a Norwegian keyboard as a key; it is the dead acute and then
    // `e`, which is what somebody sitting at that machine would type.
    let mut h = Harness::start(US_NO_KEYMAP);
    h.tell(Command::Modifiers {
        locked: 0,
        group: 1,
    });
    h.sync();

    let mark = h.mark();
    h.type_text("é");
    let wire = h.drain(mark);
    h.transcript("group1-eacute", mark);

    assert_eq!(
        wire,
        vec![
            // AltGr and the key left of Backspace is `dead_acute` on this layout...
            Wire::Key {
                code: KEY_RIGHTALT,
                down: true
            },
            Wire::Key {
                code: 13,
                down: true
            },
            Wire::Key {
                code: 13,
                down: false
            },
            Wire::Key {
                code: KEY_RIGHTALT,
                down: false
            },
            // ...and the accent means nothing until the base letter follows it.
            Wire::Key {
                code: 18,
                down: true
            },
            Wire::Key {
                code: 18,
                down: false
            },
        ],
        "`é` is composed the way somebody at that keyboard would type it"
    );
}

#[test]
fn a_special_key_and_a_raw_keycode_both_reach_the_wire() {
    let mut h = Harness::start(US_KEYMAP);
    let mark = h.mark();
    for key in [SpecialKey::Enter, SpecialKey::Left, SpecialKey::Escape] {
        h.inject(&InputEvent::Key(KeyEvent::special(
            key,
            KeyAction::Press,
            Modifiers::NONE,
        )));
        h.inject(&InputEvent::Key(KeyEvent::special(
            key,
            KeyAction::Release,
            Modifiers::NONE,
        )));
    }
    h.inject(&InputEvent::Key(KeyEvent {
        payload: KeyPayload::RawKeyCode(KEY_A),
        action: KeyAction::Press,
        modifiers: Modifiers::NONE,
    }));
    let wire = h.drain(mark);
    h.transcript("special-keys", mark);
    assert_eq!(
        wire,
        vec![
            Wire::Key {
                code: 28,
                down: true
            },
            Wire::Key {
                code: 28,
                down: false
            },
            Wire::Key {
                code: 105,
                down: true
            },
            Wire::Key {
                code: 105,
                down: false
            },
            Wire::Key {
                code: 1,
                down: true
            },
            Wire::Key {
                code: 1,
                down: false
            },
            Wire::Key {
                code: KEY_A,
                down: true
            },
        ],
    );
}
