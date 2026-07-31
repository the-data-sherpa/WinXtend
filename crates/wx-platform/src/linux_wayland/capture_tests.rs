//! What capture must get right, tested without a compositor.
//!
//! Every rule here was learned from the alpha target and then written down as an
//! assertion, so a change that breaks one fails on a CI runner with no desktop
//! rather than on somebody's screen.

use std::sync::{Arc, Mutex};

use wx_proto::{Edge, KeyPayload, MouseButton, Point, ScrollUnit, SpecialKey};

use super::*;
use crate::linux_wayland::keymap::tests::{NO, NO_NAMED, US};

/// A layout with a real dead key on it.
///
/// Neither shipped fixture has one — the Norwegian `¨` key carries the *spacing*
/// `diaeresis`, which is a character and not a composition — and a dead key is the
/// one thing capture has to hold back rather than forward.
const DEAD: &str = r#"
xkb_keymap {
xkb_keycodes "evdev" {
	<AE01> = 10;
	<AD01> = 24;
	<AC11> = 48;
};
xkb_types "complete" {
	type "ALPHABETIC" {
		modifiers= Shift+Lock;
		map[Shift]= 2;
		map[Lock]= 2;
	};
};
xkb_symbols "pc" {
	key <AD01> {
		type= "ALPHABETIC",
		symbols[1]= [ 0x65, 0x45 ]
	};
	key <AC11> {	[ 0xfe51, 0xfe50 ] };
};
};
"#;

/// A layout whose level-3 shift is *not* right Alt.
///
/// `lv3:lsgt_switch` puts `ISO_Level3_Shift` on the key between left Shift and Z,
/// which is evdev 86. Not a curiosity: measured on the alpha target, mutter's own
/// US keymap reports the level-3 shift on keycode 84 rather than on right Alt, so a
/// backend that only ever tracks right Alt resolves every level-3 character to its
/// base level.
const LV3_ELSEWHERE: &str = r#"
xkb_keymap {
xkb_keycodes "evdev" {
	<AE08> = 17;
	<LSGT> = 94;
};
xkb_types "complete" {
	type "FOUR_LEVEL" {
		modifiers= Shift+LevelThree;
		map[Shift]= 2;
		map[LevelThree]= 3;
		map[Shift+LevelThree]= 4;
	};
};
xkb_symbols "pc" {
	key <AE08> {
		type= "FOUR_LEVEL",
		symbols[1]= [ 7, slash, braceleft, NoSymbol ]
	};
	key <LSGT> { [ ISO_Level3_Shift ] };
};
};
"#;

/// `lv3:menu_switch`: the level-3 shift on a key the evdev table already names.
///
/// Menu is evdev 127, which [`special_from_evdev`] answers `SpecialKey::Menu` for.
/// The layout has the better answer and has to win, or every AltGr press opens a
/// context menu on the peer.
const LV3_ON_MENU: &str = r#"
xkb_keymap {
xkb_keycodes "evdev" {
	<AE08> = 17;
	<MENU> = 135;
};
xkb_types "complete" {
	type "FOUR_LEVEL" {
		modifiers= Shift+LevelThree;
		map[Shift]= 2;
		map[LevelThree]= 3;
		map[Shift+LevelThree]= 4;
	};
};
xkb_symbols "pc" {
	key <AE08> {
		type= "FOUR_LEVEL",
		symbols[1]= [ 7, slash, braceleft, NoSymbol ]
	};
	key <MENU> { [ ISO_Level3_Shift ] };
};
};
"#;

/// `ctrl:nocaps`: Control where Caps Lock sits, evdev 58.
const CTRL_ON_CAPS: &str = r#"
xkb_keymap {
xkb_keycodes "evdev" {
	<CAPS> = 66;
	<AD01> = 24;
};
xkb_types "complete" {
	type "ALPHABETIC" {
		modifiers= Shift+Lock;
		map[Shift]= 2;
		map[Lock]= 2;
	};
};
xkb_symbols "pc" {
	key <AD01> {
		type= "ALPHABETIC",
		symbols[1]= [ q, Q ]
	};
	key <CAPS> { [ Control_L ] };
};
};
"#;

/// `altwin:swap_alt_win`: Super where left Alt sits, evdev 56.
const SUPER_ON_ALT: &str = r#"
xkb_keymap {
xkb_keycodes "evdev" {
	<LALT> = 64;
	<AD01> = 24;
};
xkb_types "complete" {
	type "ALPHABETIC" {
		modifiers= Shift+Lock;
		map[Shift]= 2;
		map[Lock]= 2;
	};
};
xkb_symbols "pc" {
	key <AD01> {
		type= "ALPHABETIC",
		symbols[1]= [ q, Q ]
	};
	key <LALT> { [ Super_L ] };
};
};
"#;

/// The screen the harness pretends to be on: one 1920x1080 zone at the origin.
const SCREEN: Zone = Zone {
    x: 0,
    y: 0,
    w: 1920,
    h: 1080,
};

/// A capture session with somewhere to look at what came out of it.
struct Harness {
    state: CaptureState,
    events: Arc<Mutex<Vec<CapturedEvent>>>,
    commands: Arc<Mutex<Vec<Command>>>,
}

impl Harness {
    /// Started, wired to a driver, and holding the US layout.
    fn us() -> Self {
        Self::with_layout(US)
    }

    fn with_layout(keymap: &str) -> Self {
        let harness = Self::bare();
        harness.state.set_keymap(keymap.to_string());
        harness.start();
        harness
    }

    /// No sink and no driver: for the rules about the trait's own contract.
    ///
    /// Laid out with a peer to the right and nothing on the other three sides,
    /// which is what [`Harness::activate`] crosses into. Declared before the
    /// control hook goes in so that arming it queues no command for a test to
    /// have to step over.
    fn bare() -> Self {
        let state = CaptureState::new();
        state
            .set_exits(vec![exits(SCREEN, &[Edge::Right])])
            .expect("declaring where the peers are cannot fail");
        Self {
            state,
            events: Arc::new(Mutex::new(Vec::new())),
            commands: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn start(&self) {
        let events = Arc::clone(&self.events);
        let commands = Arc::clone(&self.commands);
        self.state
            .attach(Box::new(move |c| commands.lock().unwrap().push(c)));
        self.state
            .start(Box::new(move |e| events.lock().unwrap().push(e)))
            .unwrap();
    }

    /// Activated at the right-hand edge, which is where a peer to the right sits.
    ///
    /// Both queues are emptied afterwards, so a test asserts on what its own
    /// actions produced rather than on the `Enable` that arming left behind.
    fn activate(&self) {
        self.state.activated(
            Some(1),
            Point::new(1920.0, 540.0),
            Some(BarrierEdge::Right),
            Some(SCREEN),
        );
        self.drain();
        self.commands();
    }

    fn drain(&self) -> Vec<CapturedEvent> {
        std::mem::take(&mut *self.events.lock().unwrap())
    }

    fn commands(&self) -> Vec<Command> {
        std::mem::take(&mut *self.commands.lock().unwrap())
    }

    /// Type one key down and up, and return everything that reached the wire.
    fn tap(&self, keycode: u32) -> Vec<CapturedEvent> {
        self.state.key(keycode, true);
        self.state.key(keycode, false);
        self.drain()
    }

    /// Activate, then type one key and return the text it produced.
    fn activated_texts(&self, keycode: u32) -> Vec<String> {
        self.activate();
        self.texts(keycode)
    }

    fn texts(&self, keycode: u32) -> Vec<String> {
        self.tap(keycode)
            .into_iter()
            .filter_map(|e| match e {
                CapturedEvent::Key(k) if k.action == KeyAction::Press => match k.payload {
                    KeyPayload::Text(t) => Some(t),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }
}

// -- barriers ----------------------------------------------------------

/// The screen to the right of [`SCREEN`], sharing its whole right-hand edge.
const SCREEN_RIGHT: Zone = Zone {
    x: 1920,
    y: 0,
    w: 1920,
    h: 1080,
};

/// What the agent says about one screen.
fn exits(zone: Zone, edges: &[Edge]) -> ScreenExits {
    ScreenExits {
        bounds: wx_proto::Rect::new(zone.x, zone.y, zone.w as u32, zone.h as u32),
        edges: edges.to_vec(),
    }
}

/// A machine with something beyond every edge of every screen.
///
/// What the old `barriers_for` assumed unconditionally, and now has to be told.
/// Used by the tests about barrier *geometry*, which is a separate question from
/// which edges are live.
fn surrounded(zones: &[Zone]) -> Vec<ScreenExits> {
    zones
        .iter()
        .map(|z| exits(*z, &[Edge::Left, Edge::Right, Edge::Top, Edge::Bottom]))
        .collect()
}

/// `cowen-ubuntu` as the portal reports it, in its own desktop space.
///
/// The screen the bug was reported on. In the global layout it sits at
/// (6512, 5) with `cowen-workhorse` flush against its left-hand edge and nothing
/// on the other three sides; locally it is a screen at the origin like any other,
/// which is the whole reason this layer cannot answer the question itself.
const UBUNTU: Zone = Zone {
    x: 0,
    y: 0,
    w: 3072,
    h: 1728,
};

#[test]
fn an_edge_with_no_machine_beyond_it_is_not_armed() {
    // The reported bug, at the layer it is decided. One peer, on the left. The
    // other three edges must be ordinary screen edges: a barrier on any of them is
    // a request to be handed the pointer, and there is nowhere to send it.
    let plans = barriers_for(&[UBUNTU], &[exits(UBUNTU, &[Edge::Left])]);
    assert_eq!(
        plans.iter().map(|p| p.edge).collect::<Vec<_>>(),
        vec![BarrierEdge::Left],
        "only the edge with a machine beyond it may be armed"
    );
    // And still in the right place, so the crossing that should work still does.
    assert_eq!(plans[0].position, (0, 0, 0, 1727));
    assert_eq!(plans[0].zone, UBUNTU);
    assert_eq!(
        plans[0].id, 1,
        "ids stay 1-based and gap-free after filtering"
    );
}

#[test]
fn a_machine_with_no_neighbours_anywhere_arms_nothing() {
    // A lone machine, or every peer removed from the layout. Not an error and not
    // a degraded mode: the pointer stops on all four sides, exactly as it does
    // with nothing running.
    assert!(barriers_for(&[UBUNTU], &[exits(UBUNTU, &[])]).is_empty());
}

#[test]
fn a_screen_the_layout_does_not_place_arms_nothing() {
    // A monitor absent from the layout is one the router refuses to resolve any
    // crossing from, so a barrier on it could only ever pin the pointer. Failing
    // closed also covers the moment before the agent has said anything at all.
    assert!(barriers_for(&[UBUNTU], &[]).is_empty());
    // An answer that describes some other screen entirely is not an answer about
    // this one, and must not be borrowed for it.
    let elsewhere = Zone {
        x: 9000,
        y: 9000,
        w: 1920,
        h: 1080,
    };
    assert!(barriers_for(&[UBUNTU], &[exits(elsewhere, &[Edge::Left])]).is_empty());
}

#[test]
fn a_screen_is_recognised_without_the_two_rectangles_being_identical() {
    // The portal's regions and the monitors `display` enumerates were measured to
    // agree exactly, but a *safety* decision must not rest on that: a one-pixel
    // disagreement would leave the screen unmatched and unarmed, which is a
    // machine that silently stops driving anything.
    let reported = Zone {
        x: 0,
        y: 0,
        w: 3071,
        h: 1727,
    };
    let plans = barriers_for(&[reported], &[exits(UBUNTU, &[Edge::Left])]);
    assert_eq!(
        plans.iter().map(|p| p.edge).collect::<Vec<_>>(),
        vec![BarrierEdge::Left]
    );
}

#[test]
fn each_screen_is_armed_from_its_own_answer() {
    // Two local screens, a peer beyond the right-hand one only. The left screen's
    // outer edges are live nowhere, and the shared seam is skipped on both sides —
    // so exactly one barrier survives on a desktop with eight outer edges.
    let zones = [SCREEN, SCREEN_RIGHT];
    let plans = barriers_for(
        &zones,
        &[exits(SCREEN, &[]), exits(SCREEN_RIGHT, &[Edge::Right])],
    );
    assert_eq!(
        plans.iter().map(|p| (p.zone, p.edge)).collect::<Vec<_>>(),
        vec![(SCREEN_RIGHT, BarrierEdge::Right)]
    );
}

#[test]
fn a_seam_between_two_local_screens_stays_unarmed_however_the_layout_answers() {
    // The layout has no way to say "not across your own seam" — it sees the two
    // monitors as adjacent and will happily report a neighbour there. The physical
    // check has to win, or moving between the user's own screens activates capture.
    let zones = [SCREEN, SCREEN_RIGHT];
    let plans = barriers_for(&zones, &surrounded(&zones));
    let armed: Vec<(Zone, BarrierEdge)> = plans.iter().map(|p| (p.zone, p.edge)).collect();
    assert!(!armed.contains(&(SCREEN, BarrierEdge::Right)));
    assert!(!armed.contains(&(SCREEN_RIGHT, BarrierEdge::Left)));
}

#[test]
fn a_barrier_line_sits_on_the_boundary_and_its_extent_stops_one_short() {
    // Measured against the live compositor, and asymmetric in a way that is not
    // guessable: the line is at `offset + size`, the extent along it stops at
    // `offset + size - 1`. Both wrong forms are refused *silently*, losing that
    // edge and with it the only way the cursor leaves by it.
    let plans = barriers_for(&[SCREEN], &surrounded(&[SCREEN]));
    assert_eq!(
        plans.iter().map(|p| p.id).collect::<Vec<_>>(),
        vec![1, 2, 3, 4],
        "the ids must stay 1-based and gap-free; the driver maps them back to edges"
    );
    let at = |edge| plans.iter().find(|p| p.edge == edge).unwrap().position;
    assert_eq!(at(BarrierEdge::Top), (0, 0, 1919, 0));
    assert_eq!(at(BarrierEdge::Bottom), (0, 1080, 1919, 1080));
    assert_eq!(at(BarrierEdge::Left), (0, 0, 0, 1079));
    assert_eq!(at(BarrierEdge::Right), (1920, 0, 1920, 1079));
    assert!(plans.iter().all(|p| p.zone == SCREEN));
}

#[test]
fn the_seam_between_two_of_the_users_own_screens_is_left_unarmed() {
    // Both zones would otherwise put a barrier on the same line, and moving between
    // the user's own two monitors would activate capture: the pointer pins at the
    // seam, local windows go quiet, and nothing claims the cursor — so the pull-back
    // guard, which measures the axis in the crossing direction, never fires for a
    // user carrying on across their own desktop.
    let zones = [SCREEN, SCREEN_RIGHT];
    let plans = barriers_for(&zones, &surrounded(&zones));
    let armed: Vec<(Zone, BarrierEdge)> = plans.iter().map(|p| (p.zone, p.edge)).collect();
    assert!(!armed.contains(&(SCREEN, BarrierEdge::Right)));
    assert!(!armed.contains(&(SCREEN_RIGHT, BarrierEdge::Left)));

    // Everything on the outer boundary still is: the desktop as a whole is still
    // enclosed, which is what lets the cursor reach a peer at all.
    for edge in [BarrierEdge::Top, BarrierEdge::Bottom, BarrierEdge::Left] {
        assert!(armed.contains(&(SCREEN, edge)), "{edge:?}");
    }
    for edge in [BarrierEdge::Top, BarrierEdge::Bottom, BarrierEdge::Right] {
        assert!(armed.contains(&(SCREEN_RIGHT, edge)), "{edge:?}");
    }
    assert_eq!(
        plans.iter().map(|p| p.id).collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5, 6]
    );
}

#[test]
fn two_screens_that_only_share_a_line_are_both_still_enclosed() {
    // Flush along the same x, but nowhere near each other: nothing crosses between
    // them, so neither edge is internal and both stay armed.
    let below = Zone {
        x: 1920,
        y: 4000,
        w: 1920,
        h: 1080,
    };
    let zones = [SCREEN, below];
    let plans = barriers_for(&zones, &surrounded(&zones));
    let armed: Vec<(Zone, BarrierEdge)> = plans.iter().map(|p| (p.zone, p.edge)).collect();
    assert!(armed.contains(&(SCREEN, BarrierEdge::Right)));
    assert!(armed.contains(&(below, BarrierEdge::Left)));
    assert_eq!(plans.len(), 8);
}

#[test]
fn an_activation_the_compositor_did_not_attribute_still_lands_on_a_screen() {
    // The clamp cannot depend on the barrier being recognised: an unattributed
    // activation carries the same boundary coordinate, which belongs to no
    // half-open monitor rectangle and is silently dropped by whatever was going to
    // resynchronise against it.
    let zones = [SCREEN, SCREEN_RIGHT];
    // A seam point is on the screen it arrived at, not on the one it left.
    assert_eq!(
        zone_at(&zones, Point::new(1920.0, 540.0)),
        Some(SCREEN_RIGHT)
    );
    // The outer boundary line belongs to the screen it bounds.
    assert_eq!(
        zone_at(&zones, Point::new(3840.0, 540.0)),
        Some(SCREEN_RIGHT)
    );
    assert_eq!(zone_at(&zones, Point::new(600.0, 1080.0)), Some(SCREEN));
    // Nowhere near any of them: no honest answer, so none is given.
    assert_eq!(zone_at(&zones, Point::new(-5.0, 540.0)), None);

    let h = Harness::us();
    h.state.activated(
        Some(1),
        Point::new(3840.0, 540.0),
        None,
        zone_at(&zones, Point::new(3840.0, 540.0)),
    );
    assert_eq!(
        h.drain(),
        vec![CapturedEvent::PointerMotion {
            dx: 0.0,
            dy: 0.0,
            position: Point::new(3839.0, 540.0),
        }]
    );
}

// -- the trait contract ------------------------------------------------

#[test]
fn two_capture_sessions_would_double_every_keystroke() {
    let h = Harness::bare();
    h.start();
    assert!(matches!(
        h.state.start(Box::new(|_| {})),
        Err(PlatformError::AlreadyCapturing)
    ));
}

#[test]
fn stopping_something_that_never_started_says_so() {
    let h = Harness::bare();
    assert!(matches!(h.state.stop(), Err(PlatformError::NotCapturing)));
    assert!(!h.state.is_capturing());
}

#[test]
fn starting_arms_the_barriers_and_stopping_disarms_them() {
    // The portal only activates on a barrier crossing, so `Enable` is what makes
    // capture possible at all — and `Disable` is what stops the agent grabbing the
    // pointer after it has been told to stop.
    let h = Harness::bare();
    h.start();
    assert_eq!(h.commands(), vec![Command::Enable]);
    h.state.stop().unwrap();
    assert_eq!(h.commands(), vec![Command::Disable]);
    assert!(!h.state.is_capturing());
}

#[test]
fn a_session_that_comes_up_after_start_is_still_armed() {
    // The portal sequence takes as long as the user takes to answer the consent
    // dialog, and the agent starts capture at boot. Without this the barriers are
    // never armed and capture silently never happens.
    let h = Harness::bare();
    let events = Arc::clone(&h.events);
    h.state
        .start(Box::new(move |e| events.lock().unwrap().push(e)))
        .unwrap();
    let commands = Arc::clone(&h.commands);
    h.state
        .attach(Box::new(move |c| commands.lock().unwrap().push(c)));
    assert_eq!(h.commands(), vec![Command::Enable]);
}

// -- suppression -------------------------------------------------------

#[test]
fn suppression_is_reported_from_what_the_compositor_is_actually_doing() {
    // The measured truth on the alpha target: while the session is activated a
    // focused local window receives nothing. So this is not a flag the backend
    // keeps, it is a fact about the compositor.
    let h = Harness::us();
    assert!(!h.state.suppresses_local());
    h.activate();
    assert!(h.state.suppresses_local());
    h.state.deactivated(Some(1));
    assert!(!h.state.suppresses_local());
}

#[test]
fn suppression_is_never_claimed_when_it_is_not_in_force() {
    // The one answer this backend must not give. A cursor that reached a peer
    // through the layout editor rather than through a barrier leaves local input
    // going to local windows, and a silent success here is exactly the "my
    // keystrokes went to both computers" failure the trait warns about.
    let h = Harness::us();
    let err = h.state.set_suppress_local(true).unwrap_err();
    assert!(!matches!(err, PlatformError::Unsupported { .. }));
    assert!(!h.state.suppresses_local());

    // And with an activation it is honoured, with nothing to ask the compositor
    // for: it is already doing it.
    h.activate();
    assert!(h.state.set_suppress_local(true).is_ok());
    assert!(h.state.suppresses_local());
    assert!(h.commands().is_empty());
}

#[test]
fn giving_the_cursor_back_hands_the_pointer_back() {
    // The other half: the engine says the cursor is local again, and the pinned
    // pointer has to be returned or the user is stuck at the edge of their screen.
    let h = Harness::us();
    h.activate();
    h.state.set_suppress_local(true).unwrap();
    h.state.set_suppress_local(false).unwrap();

    assert!(!h.state.suppresses_local());
    let released = h.commands();
    assert!(
        matches!(
            released.as_slice(),
            [Command::Release {
                activation_id: Some(1),
                position: Some(_)
            }]
        ),
        "{released:?}"
    );
}

#[test]
fn the_pointer_is_handed_back_clear_of_the_barrier_it_crossed() {
    // Returning it exactly where the compositor pinned it puts it back on the
    // barrier line, which re-triggers on the next twitch: the pointer appears
    // stuck to the edge of the screen.
    let h = Harness::us();
    h.activate();
    h.state.set_suppress_local(false).unwrap();
    let Some(Command::Release {
        position: Some((x, y)),
        ..
    }) = h.commands().pop()
    else {
        panic!("no release");
    };
    assert!(
        x < 1919.0,
        "the pointer came back on the right-hand barrier"
    );
    assert_eq!(y, 540.0);
}

#[test]
fn asking_for_the_cursor_back_twice_is_not_an_error() {
    // `Engine::sync_suppression` only calls on a change, but a peer disconnecting
    // and a hotkey reclaiming can both drive it to the same answer.
    let h = Harness::us();
    h.activate();
    h.state.set_suppress_local(false).unwrap();
    assert!(h.state.set_suppress_local(false).is_ok());
}

#[test]
fn suppression_cannot_be_changed_on_a_capture_that_is_not_running() {
    let h = Harness::bare();
    assert!(matches!(
        h.state.set_suppress_local(true),
        Err(PlatformError::NotCapturing)
    ));
    assert!(matches!(
        h.state.set_suppress_local(false),
        Err(PlatformError::NotCapturing)
    ));
}

// -- the stranded pointer ----------------------------------------------

#[test]
fn capture_on_an_edge_with_nothing_beyond_it_is_handed_straight_back() {
    // The reported bug's symptom, and the last line of defence against it. Barriers
    // are no longer armed on such an edge at all, but an activation can still
    // arrive on one — a re-arm the compositor refused, or a layout that changed
    // while the pointer was already travelling — and the user must not pay a
    // quarter-screen drag for it.
    let h = Harness::us();
    // The `Enable` that starting left behind is not this test's subject.
    h.commands();
    h.state.activated(
        Some(1),
        Point::new(960.0, 0.0),
        Some(BarrierEdge::Top),
        Some(SCREEN),
    );
    assert!(
        matches!(h.commands().as_slice(), [Command::Release { .. }]),
        "an edge the layout has nothing beyond kept the pointer"
    );
    assert!(!h.state.suppresses_local());
}

#[test]
fn capture_on_an_edge_that_does_lead_somewhere_is_kept() {
    // The proven path, pinned alongside it: the edge with a machine beyond it must
    // still capture, and still report where the pointer is, or the fix above would
    // have stopped every crossing rather than the three bad ones.
    let h = Harness::us();
    // The `Enable` that starting left behind is not this test's subject.
    h.commands();
    h.state.activated(
        Some(1),
        Point::new(1920.0, 540.0),
        Some(BarrierEdge::Right),
        Some(SCREEN),
    );
    assert!(h.commands().is_empty(), "a real crossing was refused");
    assert!(h.state.suppresses_local());
    assert_eq!(
        h.drain(),
        vec![CapturedEvent::PointerMotion {
            dx: 0.0,
            dy: 0.0,
            // Clamped onto the last pixel of the screen it left, which is what the
            // engine resynchronises the virtual cursor against.
            position: Point::new(1919.0, 540.0),
        }]
    );
}

#[test]
fn a_pointer_pinned_at_an_edge_nobody_claimed_is_given_back() {
    // What the pull-back guard is for now that barriers are only armed where the
    // layout has somewhere to go: a crossing into an edge that *does* lead
    // somewhere, which nothing then claims — the peer is offline, or the engine is
    // still deciding. The pointer is pinned and only the user can end it.
    let h = Harness::us();
    h.activate();
    // Pushing further into the barrier changes nothing.
    for _ in 0..40 {
        h.state.motion(4.0, 0.0);
    }
    assert!(h.commands().is_empty());
    assert!(h.state.suppresses_local());

    // Pulling away from it gives up — but only after a quarter of the screen,
    // which is a deliberate movement rather than the settle at the end of a fast
    // approach.
    for _ in 0..30 {
        h.state.motion(-4.0, 0.0);
    }
    assert!(
        h.commands().is_empty(),
        "released on the settle at the end of an approach"
    );
    for _ in 0..120 {
        h.state.motion(-4.0, 0.0);
    }
    assert!(
        matches!(h.commands().as_slice(), [Command::Release { .. }]),
        "the pointer was never handed back"
    );
    assert!(!h.state.suppresses_local());
}

#[test]
fn a_peer_holding_the_cursor_keeps_it_however_far_the_mouse_is_pulled_back() {
    // Pulling back *is* how a user brings the cursor home from a peer, so the
    // guard above must not fire while somebody owns it — that would snatch the
    // pointer back mid-gesture and leave the peer's cursor stranded.
    let h = Harness::us();
    h.activate();
    h.state.set_suppress_local(true).unwrap();
    for _ in 0..400 {
        h.state.motion(-8.0, 0.0);
    }
    assert!(h.commands().is_empty());
    assert!(h.state.suppresses_local());
}

#[test]
fn a_claim_that_was_refused_does_not_disable_the_guard_afterwards() {
    // The engine asks for suppression whenever the cursor leaves, including when it
    // left by a route that started no activation. Recording that refused claim as
    // "a peer holds the cursor" would switch the pull-back guard off, and the next
    // crossing would pin the pointer with nothing left to hand it back.
    let h = Harness::us();
    assert!(h.state.set_suppress_local(true).is_err());

    h.activate();
    for _ in 0..150 {
        h.state.motion(-4.0, 0.0);
    }
    assert!(
        matches!(h.commands().as_slice(), [Command::Release { .. }]),
        "the pointer was stranded after a refused claim"
    );
}

// -- keeping up with the layout ----------------------------------------

#[test]
fn placing_a_machine_beside_this_one_arms_that_edge_without_a_restart() {
    // A restart is not an option to fall back on: this portal has no restore token
    // at the version the alpha target ships, so it would mean another consent
    // dialog. Adding, moving or removing a machine has to reach the barriers on
    // the running session.
    let h = Harness::us();
    h.commands();

    h.state
        .set_exits(vec![exits(SCREEN, &[Edge::Right, Edge::Bottom])])
        .unwrap();
    let Some(Command::Rearm(asked)) = h.commands().pop() else {
        panic!("a new machine below did not reach the barriers");
    };
    assert_eq!(asked, vec![exits(SCREEN, &[Edge::Right, Edge::Bottom])]);

    // And the removal of one disarms its edge again — the same push, so a machine
    // taken out of the layout cannot leave a barrier behind it.
    h.state.set_exits(vec![exits(SCREEN, &[])]).unwrap();
    assert!(matches!(h.commands().as_slice(), [Command::Rearm(e)] if e[0].edges.is_empty()));
    // Which the activation guard then honours, even before the compositor has
    // acted on it.
    h.state.activated(
        Some(1),
        Point::new(1920.0, 540.0),
        Some(BarrierEdge::Right),
        Some(SCREEN),
    );
    assert!(matches!(h.commands().as_slice(), [Command::Release { .. }]));
}

#[test]
fn an_unchanged_layout_costs_no_round_trip() {
    // The agent pushes the whole set whenever anything might have moved, which on a
    // running mesh is often. Obeying an unchanged one means disabling the session
    // and re-enabling it, so every heartbeat would open a window with no barriers
    // placed at all.
    let h = Harness::us();
    h.commands();
    h.state
        .set_exits(vec![exits(SCREEN, &[Edge::Right])])
        .unwrap();
    assert!(h.commands().is_empty(), "an unchanged answer was obeyed");
}

#[test]
fn what_the_driver_arms_from_is_what_it_was_last_told() {
    // The session comes up long after the agent has an opinion — the consent dialog
    // is answered by a human — so the driver reads this when the portal finally
    // grants, rather than being handed a copy that was current at start-up.
    let h = Harness::bare();
    h.state
        .set_exits(vec![exits(SCREEN, &[Edge::Left])])
        .unwrap();
    assert_eq!(h.state.exits(), vec![exits(SCREEN, &[Edge::Left])]);
}

#[test]
fn jitter_at_the_barrier_does_not_count_as_giving_up() {
    // A user pushing at the edge does not hold the mouse perfectly still. Summing
    // the raw motion rather than the net pull-back would release under them.
    let h = Harness::us();
    h.activate();
    for _ in 0..500 {
        h.state.motion(-3.0, 0.0);
        h.state.motion(3.0, 0.0);
    }
    assert!(h.commands().is_empty(), "released on jitter");
}

// -- what reaches the sink ---------------------------------------------

#[test]
fn the_settle_at_the_end_of_a_fast_approach_is_not_a_pull_back() {
    // Measured on the alpha target: arriving at an edge at speed and stopping
    // produced well over a hundred pixels of inward travel inside forty
    // milliseconds. A threshold that a hand naturally crosses takes the pointer
    // back from under the user in the very window the engine is still deciding
    // whether a peer wants it.
    let h = Harness::us();
    h.activate();
    let mut travelled = 0.0;
    while travelled < 200.0 {
        h.state.motion(-4.0, 0.0);
        travelled += 4.0;
    }
    assert!(h.commands().is_empty(), "released on the approach settle");
}

#[test]
fn a_crossing_the_compositor_did_not_attribute_to_a_barrier_is_never_auto_released() {
    // Without a barrier there is no axis to measure a pull-back along, and
    // guessing one would hand the pointer back in the middle of a gesture. The
    // engine's own release still works; only the guard is off.
    let h = Harness::us();
    h.state
        .activated(Some(1), Point::new(960.0, 540.0), None, Some(SCREEN));
    h.drain();
    h.commands();
    for _ in 0..500 {
        h.state.motion(-8.0, -8.0);
    }
    assert!(h.commands().is_empty());
    assert!(h.state.suppresses_local());
    h.state.set_suppress_local(false).unwrap();
    assert!(!h.state.suppresses_local());
}

#[test]
fn nothing_is_forwarded_outside_an_activation() {
    // The compositor delivers a straggler either side of the `Activated` signal,
    // because D-Bus is slower than the libei socket. Forwarding those would drive
    // a peer with motion that also moved the local pointer.
    let h = Harness::us();
    h.state.motion(5.0, 0.0);
    h.state.key(30, true);
    h.state.button(0x110, true);
    h.state.scroll_discrete(0, 120);
    assert!(h.drain().is_empty());
}

#[test]
fn an_activation_reports_where_the_real_pointer_is() {
    // The resynchronisation event, and the whole reason `CapturedEvent` carries a
    // position. Nothing is captured between activations, so this is the only
    // moment the engine can learn that the real cursor moved.
    let h = Harness::us();
    h.state.activated(
        Some(7),
        Point::new(1920.0, 300.0),
        Some(BarrierEdge::Right),
        Some(SCREEN),
    );
    // Reported one pixel inside the screen, not on the boundary line the portal
    // names: everything above this treats a monitor rectangle as half-open, so an
    // unclamped x=1920 belongs to no screen and is silently dropped by whatever was
    // going to resynchronise against it.
    assert_eq!(
        h.drain(),
        vec![CapturedEvent::PointerMotion {
            dx: 0.0,
            dy: 0.0,
            position: Point::new(1919.0, 300.0),
        }]
    );
}

#[test]
fn motion_reports_the_real_delta_and_the_real_position() {
    // Both, truthfully, and neither computed from the other. The delta is what
    // libei sent; the position is where the compositor pinned the pointer, which
    // does not move for as long as capture is active.
    let h = Harness::us();
    h.activate();
    h.state.motion(3.5, -2.0);
    h.state.motion(1.0, 0.0);
    assert_eq!(
        h.drain(),
        vec![
            CapturedEvent::PointerMotion {
                dx: 3.5,
                dy: -2.0,
                position: Point::new(1919.0, 540.0),
            },
            CapturedEvent::PointerMotion {
                dx: 1.0,
                dy: 0.0,
                position: Point::new(1919.0, 540.0),
            },
        ]
    );
}

#[test]
fn an_activation_on_the_boundary_line_lands_on_a_screen_that_exists() {
    // The portal names the boundary — a right-hand crossing of a 1920-wide screen
    // comes back at x=1920 — and a monitor rectangle is half-open everywhere above
    // this, so an unclamped position is on no monitor at all and the whole
    // resynchronisation is silently skipped. Measured: the alpha target really does
    // report x=3072 on a 3072-wide screen.
    let h = Harness::us();
    for (raw, expected) in [
        (Point::new(1920.0, 540.0), Point::new(1919.0, 540.0)),
        (Point::new(600.0, -0.0), Point::new(600.0, 0.0)),
        (Point::new(600.0, 1080.0), Point::new(600.0, 1079.0)),
        (Point::new(-2.0, 540.0), Point::new(0.0, 540.0)),
    ] {
        h.state
            .activated(Some(1), raw, Some(BarrierEdge::Right), Some(SCREEN));
        assert_eq!(
            h.drain(),
            vec![CapturedEvent::PointerMotion {
                dx: 0.0,
                dy: 0.0,
                position: expected,
            }],
            "{raw:?}"
        );
        h.state.deactivated(Some(1));
        h.drain();
    }
}

#[test]
fn buttons_arrive_as_the_button_that_was_pressed() {
    let h = Harness::us();
    h.activate();
    h.state.button(0x110, true);
    h.state.button(0x110, false);
    assert_eq!(
        h.drain(),
        vec![
            CapturedEvent::Button {
                button: MouseButton::Left,
                pressed: true
            },
            CapturedEvent::Button {
                button: MouseButton::Left,
                pressed: false
            },
        ]
    );
}

#[test]
fn a_button_the_wire_has_no_name_for_is_dropped_rather_than_guessed() {
    // A stylus tip is not a left click.
    let h = Harness::us();
    h.activate();
    h.state.button(0x140, true);
    assert!(h.drain().is_empty());
}

#[test]
fn a_notched_wheel_arrives_in_lines_and_a_trackpad_in_pixels() {
    // libei counts a detent as 120; the wire counts it as 1. And the sign flips,
    // because the wire follows the Windows sense and libei follows Wayland's —
    // sharing it would scroll backwards on exactly one leg of the round trip, the
    // same inversion `inject` undoes on the way out.
    let h = Harness::us();
    h.activate();
    h.state.scroll_discrete(0, 120);
    h.state.scroll(0.0, 15.0);
    assert_eq!(
        h.drain(),
        vec![
            CapturedEvent::Scroll {
                dx: 0.0,
                dy: -1.0,
                unit: ScrollUnit::Lines
            },
            CapturedEvent::Scroll {
                dx: 0.0,
                dy: -15.0,
                unit: ScrollUnit::Pixels
            },
        ]
    );
}

// -- keys --------------------------------------------------------------

#[test]
fn a_key_crosses_the_wire_as_the_text_this_machines_layout_gives_it() {
    // The central guarantee. Keycode 26 is `[` on a US keyboard and `å` on a
    // Norwegian one, and the answer has to come from the layout the user is
    // actually typing on — not from the number, which is identical either way.
    assert_eq!(Harness::us().activated_texts(26), vec!["[".to_string()]);
    assert_eq!(
        Harness::with_layout(NO).activated_texts(26),
        vec!["å".to_string()]
    );
}

#[test]
fn the_same_key_shifted_crosses_as_this_layouts_capital() {
    // `Å` on a Norwegian keyboard, from the keycode a US desktop calls `{`. Driven
    // through the keymap spelling `xkbcomp` produces, because that is the one whose
    // shifted levels used to resolve to the base character and silently type `å`.
    let h = Harness::with_layout(NO_NAMED);
    h.activate();
    assert_eq!(h.texts(26), vec!["å".to_string()]);
    h.state.key(42, true); // KEY_LEFTSHIFT
    h.drain();
    assert_eq!(h.texts(26), vec!["Å".to_string()]);
}

#[test]
fn a_dead_key_written_by_name_still_composes_rather_than_typing_nothing() {
    // The Norwegian `¨` key. Its keysym is spelled `dead_diaeresis` in a keymap
    // serialised with names, and a capture that did not recognise it reported the
    // key as unmapped — so the accent vanished and the base letter arrived alone.
    let h = Harness::with_layout(NO_NAMED);
    h.activate();
    h.state.key(27, true);
    h.state.key(27, false);
    assert!(h.drain().is_empty(), "the accent was forwarded on its own");
    assert_eq!(h.texts(16), vec!["q\u{308}".to_string()]);
}

#[test]
fn no_raw_keycode_escapes_upward_for_a_key_the_layout_maps() {
    // The rule the whole design rests on: a backend that forwards scancodes has
    // moved the cross-layout problem to the receiving machine.
    let h = Harness::us();
    h.activate();
    for keycode in [30, 2, 26, 16] {
        for event in h.tap(keycode) {
            if let CapturedEvent::Key(k) = event {
                assert!(
                    !matches!(k.payload, KeyPayload::RawKeyCode(_)),
                    "keycode {keycode} escaped as a raw code"
                );
            }
        }
    }

    // A modifier the layout put somewhere unusual is the case this rule was
    // written for and the one it kept missing: keycode 86 resolves to
    // `ISO_Level3_Shift`, which is no character, so it fell straight through to
    // the raw-keycode escape meant for content keys.
    let lv3 = Harness::with_layout(LV3_ELSEWHERE);
    lv3.activate();
    for event in lv3.tap(86) {
        if let CapturedEvent::Key(k) = event {
            assert!(
                !matches!(k.payload, KeyPayload::RawKeyCode(_)),
                "the level-3 shift escaped as a raw code"
            );
        }
    }
}

#[test]
fn a_level_three_shift_crosses_as_a_modifier_rather_than_a_position() {
    // The peer needs to know AltGr is down, not which key this keyboard keeps it
    // under: pressing position 86 on the receiver types whatever sits there, which
    // on an ordinary layout is `<`. `AltRight` is the honest target — the text has
    // already been resolved on this side, so no receiver needs a level-3 shift to
    // produce a character, and `wx_core`'s router pairs that key with `ALT|ALT_GR`
    // so releasing it clears both bits exactly once.
    let h = Harness::with_layout(LV3_ELSEWHERE);
    h.activate();
    let payloads: Vec<KeyPayload> = h
        .tap(86)
        .into_iter()
        .filter_map(|e| match e {
            CapturedEvent::Key(k) => Some(k.payload),
            _ => None,
        })
        .collect();
    assert_eq!(
        payloads,
        vec![
            KeyPayload::Special(SpecialKey::AltRight),
            KeyPayload::Special(SpecialKey::AltRight),
        ]
    );
}

#[test]
fn holding_shift_changes_what_the_key_produces() {
    let h = Harness::us();
    h.activate();
    h.state.key(42, true); // KEY_LEFTSHIFT
    assert_eq!(h.texts(30), vec!["A".to_string()]);
    h.state.key(42, false);
    assert_eq!(h.texts(30), vec!["a".to_string()]);
}

#[test]
fn altgr_reaches_the_level_it_is_the_shift_for() {
    // And reports itself as AltGr rather than a plain Alt, because the layout says
    // right Alt is `ISO_Level3_Shift`. Conflating the two turns `@` into an Alt
    // chord on the receiver.
    let h = Harness::us();
    h.activate();
    h.state.key(100, true); // KEY_RIGHTALT
    let pressed = h.drain();
    let CapturedEvent::Key(altgr) = &pressed[0] else {
        panic!("{pressed:?}");
    };
    assert_eq!(altgr.payload, KeyPayload::Special(SpecialKey::AltRight));
    assert!(altgr.modifiers.contains(Modifiers::ALT_GR));
    assert_eq!(h.texts(30), vec!["á".to_string()]);
}

#[test]
fn the_level_three_shift_is_followed_wherever_the_layout_puts_it() {
    // The keymap decides which key is `ISO_Level3_Shift`, so which key is *held*
    // has to be decided the same way. A hardcoded right-Alt-only set never records
    // this key as down, `holding_level3()` stays false, and every level-3 character
    // on the layout captures as its base level — `{` typing as `7`.
    let h = Harness::with_layout(LV3_ELSEWHERE);
    h.activate();
    h.state.key(86, true); // KEY_102ND, this layout's AltGr
    let pressed = h.drain();
    let CapturedEvent::Key(lv3) = &pressed[0] else {
        panic!("{pressed:?}");
    };
    assert!(lv3.modifiers.contains(Modifiers::ALT_GR));
    assert!(!lv3.modifiers.contains(Modifiers::ALT));
    assert_eq!(h.texts(9), vec!["{".to_string()]);
    h.state.key(86, false);
    assert_eq!(h.texts(9), vec!["7".to_string()]);
}

#[test]
fn a_modifier_the_layout_moved_onto_a_named_key_is_still_that_modifier() {
    // `lv3:menu_switch`. The evdev table says keycode 127 is Menu, and it is right
    // about the hardware and wrong about this layout: sending `Menu` would open a
    // context menu on the peer every time the user reached for AltGr.
    let h = Harness::with_layout(LV3_ON_MENU);
    h.activate();
    let payloads: Vec<KeyPayload> = h
        .tap(127)
        .into_iter()
        .filter_map(|e| match e {
            CapturedEvent::Key(k) => Some(k.payload),
            _ => None,
        })
        .collect();
    assert!(
        !payloads.contains(&KeyPayload::Special(SpecialKey::Menu)),
        "{payloads:?}"
    );
    assert_eq!(
        payloads,
        vec![
            KeyPayload::Special(SpecialKey::AltRight),
            KeyPayload::Special(SpecialKey::AltRight),
        ]
    );

    // And it still shifts the level it is the shift for.
    h.state.key(127, true);
    h.drain();
    assert_eq!(h.texts(9), vec!["{".to_string()]);
}

#[test]
fn a_caps_lock_the_layout_made_into_a_control_toggles_no_lock() {
    // `ctrl:nocaps`. Flipping the lock on a key the layout says is Control would
    // resolve every following letter through the swapped levels, so the user's
    // text crosses the wire in capitals they never typed.
    let h = Harness::with_layout(CTRL_ON_CAPS);
    h.activate();
    h.state.key(58, true);
    let pressed = h.drain();
    let CapturedEvent::Key(k) = &pressed[0] else {
        panic!("{pressed:?}");
    };
    assert_eq!(k.payload, KeyPayload::Special(SpecialKey::CtrlLeft));
    assert!(k.modifiers.contains(Modifiers::CTRL));
    assert!(!k.modifiers.contains(Modifiers::CAPS_LOCK));

    // And the chord typed while it is held carries the bit, or `Ctrl+C` crosses as
    // a bare `c` and does nothing at all on the peer.
    h.state.key(16, true);
    let chord = h.drain();
    let CapturedEvent::Key(k) = &chord[0] else {
        panic!("{chord:?}");
    };
    assert_eq!(k.payload, KeyPayload::Text("q".into()));
    assert!(k.modifiers.contains(Modifiers::CTRL));
    h.state.key(16, false);
    h.drain();

    h.state.key(58, false);
    h.drain();
    assert_eq!(h.texts(16), vec!["q".to_string()]);
}

#[test]
fn a_super_the_layout_put_on_the_alt_key_reports_super_rather_than_alt() {
    // `altwin:swap_alt_win`. Reading the position rather than the layout tells the
    // peer Alt is down while sending it a Super key: the router covers SUPER from
    // the payload, leaving ALT stranded and releasing an Alt nobody pressed, and an
    // agent hotkey on Super never matches.
    let h = Harness::with_layout(SUPER_ON_ALT);
    h.activate();
    h.state.key(56, true);
    let pressed = h.drain();
    let CapturedEvent::Key(k) = &pressed[0] else {
        panic!("{pressed:?}");
    };
    assert_eq!(k.payload, KeyPayload::Special(SpecialKey::SuperLeft));
    assert!(k.modifiers.contains(Modifiers::SUPER));
    assert!(!k.modifiers.contains(Modifiers::ALT));
}

#[test]
fn a_key_the_layout_says_nothing_special_about_keeps_its_evdev_meaning() {
    // The narrower rule must not cost the wider one: Tab and Enter still beat the
    // control characters the layout would give them, because a receiver injecting
    // `\t` as text does not move focus.
    let h = Harness::us();
    h.activate();
    for (keycode, expected) in [(15, SpecialKey::Tab), (28, SpecialKey::Enter)] {
        h.state.key(keycode, true);
        let pressed = h.drain();
        let CapturedEvent::Key(k) = &pressed[0] else {
            panic!("{pressed:?}");
        };
        assert_eq!(k.payload, KeyPayload::Special(expected));
        h.state.key(keycode, false);
        h.drain();
    }
}

#[test]
fn a_modifier_the_layout_does_not_make_a_level_shift_does_not_shift_a_level() {
    // Ctrl+C must cross as the character `c` with a Ctrl bit, not as a control
    // code and not as some other level of the key. `RawKey::text` says why: no
    // receiver can inject `0x03`.
    let h = Harness::us();
    h.activate();
    h.state.key(29, true); // KEY_LEFTCTRL
    h.drain();
    h.state.key(30, true);
    let CapturedEvent::Key(k) = &h.drain()[0] else {
        panic!()
    };
    assert_eq!(k.payload, KeyPayload::Text("a".into()));
    assert!(k.modifiers.contains(Modifiers::CTRL));
}

#[test]
fn a_key_with_a_meaning_of_its_own_beats_whatever_the_layout_calls_it() {
    // Tab resolves to `\t` through the keymap, and a receiver injecting `\t` as a
    // character does not move focus.
    let h = Harness::us();
    h.activate();
    h.state.key(1, true); // KEY_ESC
    let CapturedEvent::Key(k) = &h.drain()[0] else {
        panic!()
    };
    assert_eq!(k.payload, KeyPayload::Special(SpecialKey::Escape));
}

#[test]
fn a_release_carries_what_its_press_carried() {
    // Letting go of Shift before the letter must not turn `press A` into
    // `release a`, which leaves `A` held down on the receiver forever.
    let h = Harness::us();
    h.activate();
    h.state.key(42, true);
    h.state.key(30, true);
    h.drain();
    h.state.key(42, false);
    h.state.key(30, false);
    let released: Vec<_> = h
        .drain()
        .into_iter()
        .filter_map(|e| match e {
            CapturedEvent::Key(k) if k.action == KeyAction::Release => Some(k.payload),
            _ => None,
        })
        .collect();
    assert!(
        released.contains(&KeyPayload::Text("A".into())),
        "{released:?}"
    );
}

#[test]
fn caps_lock_is_followed_rather_than_ignored() {
    // Its own key press is the only signal there is: no compositor event reports
    // the lock state to a capturing client.
    let h = Harness::us();
    h.activate();
    assert_eq!(h.texts(16), vec!["q".to_string()]); // <AD01>, ALPHABETIC
    h.state.key(58, true); // KEY_CAPSLOCK
    h.state.key(58, false);
    h.drain();
    assert_eq!(h.texts(16), vec!["Q".to_string()]);
    // And it does not reach a key the layout did not mark alphabetic.
    assert_eq!(h.texts(2), vec!["1".to_string()]);
}

#[test]
fn a_dead_key_composes_on_this_side_and_crosses_as_one_character() {
    // The composition rule from `keyres`: while the cursor is on a peer, local
    // keystrokes are suppressed, so no window ever sees them and the desktop's own
    // composition machinery never runs. It has to happen here, and the peer must
    // receive one finished `é` rather than an accent and a letter.
    let h = Harness::with_layout(DEAD);
    h.activate();
    h.state.key(40, true); // <AC11>, `dead_acute`
    h.state.key(40, false);
    assert!(
        h.drain().is_empty(),
        "the dead key was forwarded instead of being held for the next one"
    );
    assert_eq!(h.texts(16), vec!["é".to_string()]); // <AD01>, `e`
}

#[test]
fn a_keycode_the_layout_says_nothing_about_still_reaches_the_peer() {
    // Forwarding the raw code beats dropping the keystroke: some receivers honour
    // it, and a key that silently vanishes is indistinguishable from a network
    // fault.
    let h = Harness::us();
    h.activate();
    h.state.key(200, true);
    let CapturedEvent::Key(k) = &h.drain()[0] else {
        panic!()
    };
    assert_eq!(k.payload, KeyPayload::RawKeyCode(200));
}

#[test]
fn a_session_with_no_keymap_forwards_rather_than_swallows() {
    let h = Harness::bare();
    h.start();
    h.activate();
    h.state.key(30, true);
    let CapturedEvent::Key(k) = &h.drain()[0] else {
        panic!()
    };
    assert_eq!(k.payload, KeyPayload::RawKeyCode(30));
}

// -- teardown ----------------------------------------------------------

#[test]
fn stopping_releases_everything_that_was_held() {
    // The rule `release_held` exists for. A Ctrl left down on the machine that
    // owned the cursor cannot be cleared by anything the user does here.
    let h = Harness::us();
    h.activate();
    h.state.key(29, true); // Ctrl
    h.state.key(30, true); // a
    h.state.button(0x110, true);
    h.drain();

    h.state.stop().unwrap();
    let released = h.drain();
    assert!(released.contains(&CapturedEvent::Button {
        button: MouseButton::Left,
        pressed: false
    }));
    let keys: Vec<_> = released
        .iter()
        .filter_map(|e| match e {
            CapturedEvent::Key(k) => Some((k.payload.clone(), k.action)),
            _ => None,
        })
        .collect();
    assert!(
        keys.contains(&(KeyPayload::Text("a".into()), KeyAction::Release)),
        "{keys:?}"
    );
    assert!(
        keys.contains(&(
            KeyPayload::Special(SpecialKey::CtrlLeft),
            KeyAction::Release
        )),
        "{keys:?}"
    );
}

#[test]
fn the_compositor_ending_an_activation_releases_everything_too() {
    // The lock-screen case, which is deliberately not tested on hardware: a screen
    // that locks mid-crossing must not leave a modifier down on the peer. Whether
    // it arrives as `Deactivated` or as the session closing, both paths land here.
    let h = Harness::us();
    h.activate();
    h.state.key(42, true); // Shift
    h.drain();
    h.state.deactivated(Some(1));
    let keys: Vec<_> = h
        .drain()
        .into_iter()
        .filter_map(|e| match e {
            CapturedEvent::Key(k) => Some(k.payload),
            _ => None,
        })
        .collect();
    assert_eq!(keys, vec![KeyPayload::Special(SpecialKey::ShiftLeft)]);
}

#[test]
fn losing_the_session_releases_everything_too() {
    // `Disabled`, or the libei transport going away. Same requirement, different
    // signal — which is exactly why both are handled.
    let h = Harness::us();
    h.activate();
    h.state.key(42, true);
    h.drain();
    h.state.detach();
    assert!(!h.drain().is_empty(), "a held Shift was stranded");
    assert!(!h.state.suppresses_local());
}

#[test]
fn a_modifier_believed_held_does_not_survive_into_the_next_crossing() {
    // Capture stops seeing the keyboard between activations, so a Shift released
    // while the cursor was local would still look held. The next crossing would
    // then shift its first character.
    let h = Harness::us();
    h.activate();
    h.state.key(42, true);
    h.drain();
    h.state.deactivated(Some(1));
    h.drain();

    h.activate();
    assert_eq!(h.texts(30), vec!["a".to_string()]);
}
