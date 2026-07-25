//! Where each captured input event has to go.
//!
//! The router sits between a platform capture backend and the network. It owns
//! the authoritative [`VirtualCursor`], and for every event it produces an
//! *ordered* list of [`RouteAction`]s: inject here, send there, hand control
//! over, take it back. Ordering is part of the contract rather than an
//! implementation detail — a modifier release that overtakes the handoff it was
//! meant to precede leaves a key stuck down on a machine that is no longer
//! listening, and only a physical keypress on that machine clears it.
//!
//! Everything here is synchronous and allocation-light. There is no I/O, so the
//! whole handoff protocol is testable without two machines, and the caller keeps
//! full control over how actions reach the wire.

use std::collections::HashMap;

use wx_proto::{
    ControlMsg, Edge, GlobalMonitorId, InputEvent, InputFrame, KeyAction, KeyEvent, KeyPayload,
    Modifiers, MonitorId, MouseButton, NodeId, NormPos, PointerEvent, SpecialKey,
};

use crate::cursor::{CursorMotion, VirtualCursor, WarpError};
use crate::layout::GlobalLayout;

/// Synthetic modifier keys used to let go of a modifier that was only ever
/// observed as a bit in an event's snapshot.
///
/// The left-hand key is chosen arbitrarily: the receiver injects a modifier, it
/// does not care which physical key produced it.
const MODIFIER_KEYS: [(Modifiers, SpecialKey); 5] = [
    (Modifiers::SHIFT, SpecialKey::ShiftLeft),
    (Modifiers::CTRL, SpecialKey::CtrlLeft),
    (Modifiers::ALT, SpecialKey::AltLeft),
    (Modifiers::ALT_GR, SpecialKey::AltRight),
    (Modifiers::SUPER, SpecialKey::SuperLeft),
];

/// One thing the caller must do, in the order given.
#[derive(Debug, Clone, PartialEq)]
pub enum RouteAction {
    /// Inject on this machine.
    ///
    /// Carries its own target monitor rather than relying on the router's
    /// current cursor: by the time a caller walks the list the cursor may have
    /// moved on, and a multi-monitor local machine would inject on the wrong
    /// screen.
    Local {
        target: MonitorId,
        event: InputEvent,
    },
    /// Send to a peer over the input plane.
    Remote { node: NodeId, frame: InputFrame },
    /// Control is moving to `node`; everything needed for
    /// [`ControlMsg::TakeControl`]. Emitted before any frame addressed there,
    /// and the point at which the local pointer must be parked out of the way.
    Handoff {
        node: NodeId,
        target: MonitorId,
        entry: NormPos,
        via: Edge,
    },
    /// `node` no longer owns the cursor and should restore its own pointer.
    Yield { node: NodeId },
}

impl RouteAction {
    /// The control-plane message this action becomes on the wire, if any.
    ///
    /// Handoff and yield are latched state, so they travel on the reliable
    /// stream rather than as datagrams; losing one desynchronises ownership and
    /// the cursor appears on two machines at once.
    pub fn control_msg(&self) -> Option<ControlMsg> {
        match *self {
            RouteAction::Handoff {
                target, entry, via, ..
            } => Some(ControlMsg::TakeControl { target, entry, via }),
            RouteAction::Yield { .. } => Some(ControlMsg::YieldControl),
            _ => None,
        }
    }

    /// Peer this action is addressed to, or `None` for local injection.
    pub fn node(&self) -> Option<NodeId> {
        match *self {
            RouteAction::Local { .. } => None,
            RouteAction::Remote { node, .. }
            | RouteAction::Handoff { node, .. }
            | RouteAction::Yield { node } => Some(node),
        }
    }
}

/// Keys and buttons currently held down by whoever owns the cursor.
///
/// Tracked so that control handoff can release exactly those, rather than
/// blanket-releasing every modifier — which would fight with a peer's own
/// physical keyboard.
#[derive(Debug, Clone, Default)]
struct HeldInput {
    /// Press order, so releases can unwind last-in-first-out.
    keys: Vec<KeyPayload>,
    buttons: Vec<MouseButton>,
    /// Latest modifier snapshot, excluding the lock keys.
    mods: Modifiers,
}

impl HeldInput {
    fn observe_key(&mut self, ev: &KeyEvent) {
        // Every event carries the sender's full modifier snapshot, which is
        // authoritative and corrects any drift in this bookkeeping. Caps Lock and
        // Num Lock are toggles, not held keys: synthesising a release for them
        // would flip the receiver's lock state instead of letting go.
        self.mods = ev
            .modifiers
            .without(Modifiers::CAPS_LOCK)
            .without(Modifiers::NUM_LOCK);
        match ev.action {
            // A repeat means the key is still down; it must not be counted twice
            // or the release list would carry duplicates.
            KeyAction::Press | KeyAction::Repeat => {
                if !self.keys.contains(&ev.payload) {
                    self.keys.push(ev.payload.clone());
                }
            }
            KeyAction::Release => {
                self.keys.retain(|k| k != &ev.payload);
                // Some capture backends report a modifier bit as still set in the
                // very event that releases it.
                if let KeyPayload::Special(k) = &ev.payload {
                    if let Some(bit) = k.modifier_bit() {
                        self.mods = self.mods.without(bit);
                    }
                }
            }
        }
    }

    fn observe_button(&mut self, button: MouseButton, pressed: bool) {
        if pressed {
            if !self.buttons.contains(&button) {
                self.buttons.push(button);
            }
        } else {
            self.buttons.retain(|b| *b != button);
        }
    }

    fn clear(&mut self) {
        self.keys.clear();
        self.buttons.clear();
        self.mods = Modifiers::NONE;
    }

    /// Test-only: production code never needs to ask, but a test does, because
    /// the latched modifier mask has no public accessor and leaking one would
    /// invite callers to make decisions on it.
    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.keys.is_empty() && self.buttons.is_empty() && self.mods.is_empty()
    }

    /// Events that put everything currently held back up.
    fn release_events(&self) -> Vec<InputEvent> {
        let mut out = Vec::new();

        // Buttons first. A drag cannot continue across a handoff, and an
        // application wants the button up before the keys that modified it.
        for button in self.buttons.iter().rev() {
            out.push(InputEvent::Pointer(PointerEvent::Button {
                button: *button,
                pressed: false,
            }));
        }

        for payload in self.keys.iter().rev() {
            out.push(InputEvent::Key(KeyEvent {
                payload: payload.clone(),
                action: KeyAction::Release,
                // Nothing is being modified by a release, and reporting a stale
                // snapshot here would invite the receiver to re-latch it.
                modifiers: Modifiers::NONE,
            }));
        }

        // Modifiers that never arrived as a discrete key press. `Ctrl+C`
        // captured as Text("c") with the CTRL bit set is the ordinary case: the
        // receiver physically held Ctrl to inject the chord and has been told
        // nothing that would make it let go.
        let covered = self
            .keys
            .iter()
            .fold(Modifiers::NONE, |acc, k| acc.union(covered_bits(k)));
        let stranded = self.mods.without(covered);
        for (bit, key) in MODIFIER_KEYS {
            if stranded.contains(bit) {
                out.push(InputEvent::Key(KeyEvent::special(
                    key,
                    KeyAction::Release,
                    Modifiers::NONE,
                )));
            }
        }

        out
    }
}

/// Modifier bits a held key already accounts for.
///
/// Right Alt reports as [`Modifiers::ALT`], but on European layouts the same
/// physical key is AltGr; releasing it clears both, so neither bit should be
/// released a second time.
fn covered_bits(payload: &KeyPayload) -> Modifiers {
    match payload {
        KeyPayload::Special(SpecialKey::AltRight) => Modifiers::ALT.union(Modifiers::ALT_GR),
        KeyPayload::Special(k) => k.modifier_bit().unwrap_or(Modifiers::NONE),
        _ => Modifiers::NONE,
    }
}

/// Routes captured input to whichever machine currently owns the cursor.
#[derive(Debug, Clone)]
pub struct InputRouter {
    /// This machine. Anything addressed here is injected rather than sent.
    local: NodeId,
    layout: GlobalLayout,
    cursor: VirtualCursor,
    /// Outbound sequence numbers, one counter per peer.
    ///
    /// Per peer rather than global because the receiving [`wx_proto::SequenceGate`]
    /// keeps a single high-water mark per connection: a shared counter would
    /// leave gaps, and a peer that had been idle would reject its first frames as
    /// stale after the counter advanced elsewhere.
    seq: HashMap<NodeId, u64>,
    held: HeldInput,
}

impl InputRouter {
    pub fn new(local: NodeId, layout: GlobalLayout, cursor: VirtualCursor) -> Self {
        Self {
            local,
            layout,
            cursor,
            seq: HashMap::new(),
            held: HeldInput::default(),
        }
    }

    pub fn local_node(&self) -> NodeId {
        self.local
    }

    /// Node whose machine the cursor is on right now.
    pub fn owner(&self) -> NodeId {
        self.cursor.monitor().node
    }

    pub fn owns_cursor(&self) -> bool {
        self.owner() == self.local
    }

    /// Whether the local pointer must be hidden and pinned out of the way.
    ///
    /// While a peer owns the cursor the local pointer would otherwise sit
    /// visibly on screen and, worse, drift under any input the capture backend
    /// fails to swallow.
    pub fn local_cursor_suppressed(&self) -> bool {
        !self.owns_cursor()
    }

    pub fn cursor(&self) -> &VirtualCursor {
        &self.cursor
    }

    pub fn layout(&self) -> &GlobalLayout {
        &self.layout
    }

    pub fn is_locked(&self) -> bool {
        self.cursor.is_locked()
    }

    pub fn set_locked(&mut self, locked: bool) {
        self.cursor.set_locked(locked);
    }

    /// Toggle the cursor lock, returning its new state. Bound to
    /// Ctrl+Alt+Super+L.
    pub fn toggle_lock(&mut self) -> bool {
        self.cursor.toggle_lock()
    }

    pub fn held_keys(&self) -> &[KeyPayload] {
        &self.held.keys
    }

    pub fn held_buttons(&self) -> &[MouseButton] {
        &self.held.buttons
    }

    /// Replace the layout, rehoming the cursor if its monitor disappeared.
    ///
    /// Unplugging the display that holds the cursor, or losing the peer that owns
    /// it, would otherwise strand control on a monitor no motion can leave. The
    /// rescue prefers a local monitor: the user is sitting in front of this
    /// machine, so this is where the pointer needs to reappear.
    pub fn set_layout(&mut self, layout: GlobalLayout) -> Vec<RouteAction> {
        self.layout = layout;
        if let Some(rect) = self.layout.rect(self.cursor.monitor()) {
            if !rect.is_empty() {
                return Vec::new();
            }
        }
        let rescue = self
            .layout
            .monitors_of(self.local)
            .chain(self.layout.iter())
            .find(|p| !p.global_bounds.is_empty())
            .map(|p| p.monitor);
        match rescue {
            Some(monitor) => self
                .warp(monitor, NormPos::new(0.5, 0.5))
                .unwrap_or_default(),
            // Nowhere to go: an empty layout means no machine can be driven, and
            // motion stays blocked until one shows up.
            None => Vec::new(),
        }
    }

    /// Integrate a raw pointer delta from the local capture backend.
    pub fn motion(&mut self, dx: f64, dy: f64) -> Vec<RouteAction> {
        let before = self.cursor.global_position();
        let motion = self.cursor.move_by(&self.layout, dx, dy);
        let mut out = Vec::new();
        match motion {
            CursorMotion::Transitioned {
                from,
                to,
                entry,
                via,
            } => {
                self.transfer(from, to, entry, via, &mut out);
                self.emit(
                    to,
                    InputEvent::Pointer(PointerEvent::MoveTo { pos: entry }),
                    &mut out,
                );
            }
            CursorMotion::Moved { monitor, pos } | CursorMotion::Blocked { monitor, pos } => {
                // Motion too small to move the cursor a whole unit produces no
                // traffic. A high-rate mouse under a small cursor scale would
                // otherwise flood the link with identical positions, and the
                // datagram plane has no deduplication of its own.
                if self.cursor.global_position() != before {
                    self.emit(
                        monitor,
                        InputEvent::Pointer(PointerEvent::MoveTo { pos }),
                        &mut out,
                    );
                }
            }
        }
        out
    }

    /// Route a captured event that is not pointer motion.
    ///
    /// Relative motion is redirected through [`InputRouter::motion`] so the
    /// virtual cursor stays authoritative; letting a delta past it would leave
    /// the router's idea of the cursor position behind the peer's.
    pub fn route(&mut self, event: InputEvent) -> Vec<RouteAction> {
        if let InputEvent::Pointer(PointerEvent::MoveBy { dx, dy }) = event {
            return self.motion(dx, dy);
        }

        let owner = self.cursor.monitor();
        let mut out = Vec::new();
        match &event {
            InputEvent::Pointer(PointerEvent::Button { button, pressed }) => {
                self.held.observe_button(*button, *pressed);
            }
            InputEvent::Key(ev) => self.held.observe_key(ev),
            // ReleaseControl means the owner is letting go of everything, so
            // there is nothing left to release later.
            InputEvent::ReleaseControl => self.held.clear(),
            _ => {}
        }
        self.emit(owner, event, &mut out);
        out
    }

    /// Move control to `monitor` outright, as when accepting a peer's handoff or
    /// jumping the cursor from the UI.
    ///
    /// `via` is the edge the cursor should be treated as having entered through.
    pub fn warp_via(
        &mut self,
        monitor: GlobalMonitorId,
        entry: NormPos,
        via: Edge,
    ) -> Result<Vec<RouteAction>, WarpError> {
        let from = self.cursor.monitor();
        let landed = self.cursor.warp_to(&self.layout, monitor, entry)?;
        let mut out = Vec::new();
        self.transfer(from, monitor, landed, via, &mut out);
        self.emit(
            monitor,
            InputEvent::Pointer(PointerEvent::MoveTo { pos: landed }),
            &mut out,
        );
        Ok(out)
    }

    /// [`InputRouter::warp_via`] with the entry edge inferred from the position.
    pub fn warp(
        &mut self,
        monitor: GlobalMonitorId,
        entry: NormPos,
    ) -> Result<Vec<RouteAction>, WarpError> {
        self.warp_via(monitor, entry, nearest_edge(entry))
    }

    /// Release everything held, without moving control.
    ///
    /// For the cases where input stops but ownership does not change: the local
    /// screensaver engaging, the capture backend being torn down, or the agent
    /// shutting down cleanly.
    pub fn release_all(&mut self) -> Vec<RouteAction> {
        let owner = self.cursor.monitor();
        let releases = self.held.release_events();
        let mut out = Vec::new();
        for event in releases {
            self.emit(owner, event, &mut out);
        }
        self.held.clear();
        out
    }

    /// Ownership change bookkeeping, in the only order that is safe.
    fn transfer(
        &mut self,
        from: GlobalMonitorId,
        to: GlobalMonitorId,
        entry: NormPos,
        via: Edge,
        out: &mut Vec<RouteAction>,
    ) {
        if from.node == to.node {
            // Two monitors of one machine. Nothing changes hands, and yielding
            // here would make that machine restore its own pointer in the middle
            // of a gesture; the frame's target monitor carries the change.
            return;
        }

        // Release before yielding. After YieldControl the losing peer restores
        // its own pointer and stops treating us as the owner, so a release sent
        // afterwards may be discarded — and a discarded release is a modifier
        // stuck down until someone physically presses that key.
        let releases = self.held.release_events();
        for event in releases {
            self.emit(from, event, out);
        }
        self.held.clear();

        if from.node == self.local {
            // No control message to ourselves. ReleaseControl is the local
            // instruction to stop injecting and restore the physical pointer.
            self.emit(from, InputEvent::ReleaseControl, out);
        } else {
            out.push(RouteAction::Yield { node: from.node });
        }

        // Deliberately not re-pressing the released modifiers on the gaining
        // machine: every forwarded event carries its own modifier snapshot, so
        // chords still work, while a re-press that is followed by a disconnect
        // would strand the modifier on the new machine instead.
        if to.node != self.local {
            out.push(RouteAction::Handoff {
                node: to.node,
                target: to.monitor,
                entry,
                via,
            });
        }
    }

    fn emit(&mut self, target: GlobalMonitorId, event: InputEvent, out: &mut Vec<RouteAction>) {
        if target.node == self.local {
            out.push(RouteAction::Local {
                target: target.monitor,
                event,
            });
        } else {
            let seq = self.next_seq(target.node);
            out.push(RouteAction::Remote {
                node: target.node,
                frame: InputFrame::new(seq, target.monitor, event),
            });
        }
    }

    /// Next outbound sequence number for a peer.
    ///
    /// Starts at 1 so that 0 can mean "nothing sent yet" in logs and tests.
    fn next_seq(&mut self, node: NodeId) -> u64 {
        let counter = self.seq.entry(node).or_insert(0);
        *counter += 1;
        *counter
    }
}

/// Edge of a monitor that `entry` sits closest to.
///
/// A warp crosses no seam, but [`ControlMsg::TakeControl`] needs an edge for the
/// receiver to nudge away from. Any answer is harmless there, so the choice is
/// deterministic rather than optional: making callers unwrap an `Option<Edge>`
/// for a value nothing depends on buys nothing.
fn nearest_edge(entry: NormPos) -> Edge {
    let e = entry.sanitized();
    let mut best = (e.x, Edge::Left);
    for candidate in [
        (1.0 - e.x, Edge::Right),
        (e.y, Edge::Top),
        (1.0 - e.y, Edge::Bottom),
    ] {
        if candidate.0 < best.0 {
            best = candidate;
        }
    }
    best.1
}

#[cfg(test)]
mod tests {
    use super::*;
    use wx_proto::{Placement, Rect, ScrollUnit};

    fn node(n: u8) -> NodeId {
        NodeId([n; 32])
    }

    fn gid(n: u8, m: u32) -> GlobalMonitorId {
        GlobalMonitorId::new(node(n), MonitorId(m))
    }

    fn place(id: GlobalMonitorId, x: i32, w: u32) -> Placement {
        Placement {
            monitor: id,
            global_bounds: Rect::new(x, 0, w, 1080),
            cursor_scale: 1.0,
        }
    }

    /// This machine on the left, one peer on the right.
    fn two_machines() -> GlobalLayout {
        let mut l = GlobalLayout::new();
        l.insert(place(gid(1, 0), 0, 1920));
        l.insert(place(gid(2, 0), 1920, 1920));
        l
    }

    fn router_with(layout: GlobalLayout, start: GlobalMonitorId) -> InputRouter {
        let cursor = VirtualCursor::at(&layout, start, NormPos::new(0.5, 0.5)).unwrap();
        InputRouter::new(node(1), layout, cursor)
    }

    fn router() -> InputRouter {
        router_with(two_machines(), gid(1, 0))
    }

    /// Enough motion to leave the 1920-wide screen the cursor starts centred on.
    fn cross_right(r: &mut InputRouter) -> Vec<RouteAction> {
        r.motion(2000.0, 0.0)
    }

    fn key(payload: KeyPayload, action: KeyAction, mods: Modifiers) -> InputEvent {
        InputEvent::Key(KeyEvent {
            payload,
            action,
            modifiers: mods,
        })
    }

    fn special(k: SpecialKey, action: KeyAction, mods: Modifiers) -> InputEvent {
        key(KeyPayload::Special(k), action, mods)
    }

    fn index_of(actions: &[RouteAction], pred: impl Fn(&RouteAction) -> bool) -> usize {
        actions
            .iter()
            .position(pred)
            .unwrap_or_else(|| panic!("no matching action in {actions:#?}"))
    }

    #[test]
    fn events_stay_local_while_this_machine_owns_the_cursor() {
        let mut r = router();
        assert!(r.owns_cursor());
        let moved = r.motion(10.0, 10.0);
        assert!(
            matches!(moved.as_slice(), [RouteAction::Local { target, .. }] if *target == MonitorId(0)),
            "{moved:#?}"
        );

        let clicked = r.route(InputEvent::Pointer(PointerEvent::Button {
            button: MouseButton::Left,
            pressed: true,
        }));
        assert!(
            matches!(clicked.as_slice(), [RouteAction::Local { .. }]),
            "{clicked:#?}"
        );
    }

    #[test]
    fn crossing_to_a_peer_releases_then_yields_then_takes_control_before_any_frame() {
        let mut r = router();
        let actions = cross_right(&mut r);

        let release = index_of(&actions, |a| {
            matches!(
                a,
                RouteAction::Local {
                    event: InputEvent::ReleaseControl,
                    ..
                }
            )
        });
        let handoff = index_of(&actions, |a| matches!(a, RouteAction::Handoff { .. }));
        let frame = index_of(&actions, |a| matches!(a, RouteAction::Remote { .. }));
        assert!(release < handoff, "{actions:#?}");
        assert!(
            handoff < frame,
            "an input frame overtook the handoff: {actions:#?}"
        );
        assert_eq!(r.owner(), node(2));
    }

    #[test]
    fn handoff_carries_the_entry_seam_of_the_destination() {
        let mut r = router();
        let actions = cross_right(&mut r);
        match &actions[index_of(&actions, |a| matches!(a, RouteAction::Handoff { .. }))] {
            RouteAction::Handoff {
                node: n,
                target,
                entry,
                via,
            } => {
                assert_eq!(*n, node(2));
                assert_eq!(*target, MonitorId(0));
                assert_eq!(*via, Edge::Left);
                assert!(entry.x < 0.01, "entered away from the seam: {entry:?}");
                assert!(
                    (entry.y - 0.5).abs() < 0.01,
                    "height not preserved: {entry:?}"
                );
            }
            other => panic!("{other:#?}"),
        }
    }

    #[test]
    fn control_actions_become_the_right_wire_messages() {
        let mut r = router();
        let actions = cross_right(&mut r);
        let handoff = &actions[index_of(&actions, |a| matches!(a, RouteAction::Handoff { .. }))];
        assert!(matches!(
            handoff.control_msg(),
            Some(ControlMsg::TakeControl { .. })
        ));
        assert_eq!(
            RouteAction::Yield { node: node(2) }.control_msg(),
            Some(ControlMsg::YieldControl)
        );
        // Input frames belong on the input plane, not the control stream.
        let frame = &actions[index_of(&actions, |a| matches!(a, RouteAction::Remote { .. }))];
        assert_eq!(frame.control_msg(), None);
    }

    #[test]
    fn a_held_modifier_is_released_on_the_machine_losing_control() {
        let mut r = router();
        r.route(special(
            SpecialKey::CtrlLeft,
            KeyAction::Press,
            Modifiers::CTRL,
        ));
        let actions = cross_right(&mut r);

        let released = index_of(&actions, |a| {
            matches!(
                a,
                RouteAction::Local {
                    event: InputEvent::Key(KeyEvent {
                        payload: KeyPayload::Special(SpecialKey::CtrlLeft),
                        action: KeyAction::Release,
                        ..
                    }),
                    ..
                }
            )
        });
        let give_up = index_of(&actions, |a| {
            matches!(
                a,
                RouteAction::Local {
                    event: InputEvent::ReleaseControl,
                    ..
                }
            )
        });
        assert!(
            released < give_up,
            "release must precede the handover: {actions:#?}"
        );
        // Nothing about the held Ctrl is pressed on the machine gaining control.
        assert!(
            !actions.iter().any(|a| matches!(
                a,
                RouteAction::Remote {
                    frame: InputFrame {
                        event: InputEvent::Key(_),
                        ..
                    },
                    ..
                }
            )),
            "{actions:#?}"
        );
        assert!(r.held_keys().is_empty());
    }

    #[test]
    fn a_modifier_seen_only_in_a_chord_snapshot_is_still_released() {
        // Ctrl+C typically arrives as Text("c") with the CTRL bit set, and no
        // discrete Ctrl press ever appears. Without this the machine losing
        // control is left holding Ctrl.
        let mut r = router();
        r.route(key(
            KeyPayload::Text("c".into()),
            KeyAction::Press,
            Modifiers::CTRL,
        ));
        r.route(key(
            KeyPayload::Text("c".into()),
            KeyAction::Release,
            Modifiers::CTRL,
        ));
        let actions = cross_right(&mut r);

        assert!(
            actions.iter().any(|a| matches!(
                a,
                RouteAction::Local {
                    event: InputEvent::Key(KeyEvent {
                        payload: KeyPayload::Special(SpecialKey::CtrlLeft),
                        action: KeyAction::Release,
                        ..
                    }),
                    ..
                }
            )),
            "stranded Ctrl was never released: {actions:#?}"
        );
    }

    #[test]
    fn lock_keys_are_never_synthetically_released() {
        // Releasing Caps Lock as if it were held would toggle the receiver's
        // lock state, which is worse than leaving it alone.
        let mut r = router();
        r.route(key(
            KeyPayload::Text("a".into()),
            KeyAction::Press,
            Modifiers::CAPS_LOCK | Modifiers::NUM_LOCK,
        ));
        r.route(key(
            KeyPayload::Text("a".into()),
            KeyAction::Release,
            Modifiers::CAPS_LOCK | Modifiers::NUM_LOCK,
        ));
        let actions = cross_right(&mut r);
        assert!(
            !actions.iter().any(|a| matches!(
                a,
                RouteAction::Local {
                    event: InputEvent::Key(_),
                    ..
                }
            )),
            "{actions:#?}"
        );
    }

    #[test]
    fn held_keys_unwind_in_reverse_press_order() {
        let mut r = router();
        r.route(special(
            SpecialKey::CtrlLeft,
            KeyAction::Press,
            Modifiers::CTRL,
        ));
        r.route(special(
            SpecialKey::ShiftLeft,
            KeyAction::Press,
            Modifiers::CTRL | Modifiers::SHIFT,
        ));
        let actions = cross_right(&mut r);

        let order: Vec<SpecialKey> = actions
            .iter()
            .filter_map(|a| match a {
                RouteAction::Local {
                    event:
                        InputEvent::Key(KeyEvent {
                            payload: KeyPayload::Special(k),
                            action: KeyAction::Release,
                            ..
                        }),
                    ..
                } => Some(*k),
                _ => None,
            })
            .collect();
        assert_eq!(
            order,
            vec![SpecialKey::ShiftLeft, SpecialKey::CtrlLeft],
            "chords must unwind last-in-first-out"
        );
    }

    #[test]
    fn a_properly_released_key_is_not_released_again_at_handoff() {
        let mut r = router();
        r.route(key(
            KeyPayload::Text("x".into()),
            KeyAction::Press,
            Modifiers::NONE,
        ));
        r.route(key(
            KeyPayload::Text("x".into()),
            KeyAction::Release,
            Modifiers::NONE,
        ));
        assert!(r.held_keys().is_empty());
        let actions = cross_right(&mut r);
        assert!(
            !actions.iter().any(|a| matches!(
                a,
                RouteAction::Local {
                    event: InputEvent::Key(_),
                    ..
                }
            )),
            "{actions:#?}"
        );
    }

    #[test]
    fn auto_repeat_does_not_duplicate_held_state() {
        let mut r = router();
        for _ in 0..5 {
            r.route(special(
                SpecialKey::Down,
                KeyAction::Repeat,
                Modifiers::NONE,
            ));
        }
        assert_eq!(r.held_keys().len(), 1);
    }

    #[test]
    fn a_held_button_goes_up_before_control_leaves() {
        let mut r = router();
        r.route(InputEvent::Pointer(PointerEvent::Button {
            button: MouseButton::Left,
            pressed: true,
        }));
        assert_eq!(r.held_buttons(), &[MouseButton::Left]);
        let actions = cross_right(&mut r);

        let up = index_of(&actions, |a| {
            matches!(
                a,
                RouteAction::Local {
                    event: InputEvent::Pointer(PointerEvent::Button { pressed: false, .. }),
                    ..
                }
            )
        });
        let give_up = index_of(&actions, |a| {
            matches!(
                a,
                RouteAction::Local {
                    event: InputEvent::ReleaseControl,
                    ..
                }
            )
        });
        assert!(up < give_up, "{actions:#?}");
        assert!(r.held_buttons().is_empty());
    }

    #[test]
    fn sequence_numbers_are_monotonic_per_peer() {
        let mut r = router();
        cross_right(&mut r);
        let mut last = 0;
        for _ in 0..20 {
            for action in r.motion(3.0, 1.0) {
                if let RouteAction::Remote { frame, .. } = action {
                    assert!(
                        frame.seq > last,
                        "seq went backwards: {} after {last}",
                        frame.seq
                    );
                    last = frame.seq;
                }
            }
        }
        assert!(last > 0, "no frames were sent");
    }

    #[test]
    fn each_peer_gets_its_own_counter() {
        // A shared counter would leave gaps, and a peer's SequenceGate would
        // reject the first frames of a fresh visit as stale.
        let mut l = two_machines();
        l.insert(place(gid(3, 0), 3840, 1920));
        let mut r = router_with(l, gid(1, 0));

        cross_right(&mut r);
        for _ in 0..5 {
            r.motion(10.0, 0.0);
        }
        let to_three = r.motion(2000.0, 0.0);
        let first = to_three
            .iter()
            .find_map(|a| match a {
                RouteAction::Remote { node: n, frame } if *n == node(3) => Some(frame.seq),
                _ => None,
            })
            .expect("should have sent a frame to the third machine");
        assert_eq!(first, 1, "third machine's counter did not start fresh");
    }

    #[test]
    fn sequence_numbers_keep_advancing_across_a_round_trip() {
        let mut r = router();
        let first = cross_right(&mut r)
            .iter()
            .find_map(|a| match a {
                RouteAction::Remote { frame, .. } => Some(frame.seq),
                _ => None,
            })
            .unwrap();
        // Back home and out again: the peer's gate never rewinds, so neither may
        // the counter.
        r.motion(-2000.0, 0.0);
        let again = cross_right(&mut r)
            .iter()
            .find_map(|a| match a {
                RouteAction::Remote { frame, .. } => Some(frame.seq),
                _ => None,
            })
            .unwrap();
        assert!(again > first, "{again} should exceed {first}");
    }

    #[test]
    fn crossing_between_two_monitors_of_one_peer_does_not_hand_over_control() {
        // Yielding here would make the peer restore its own pointer mid-gesture.
        let mut l = two_machines();
        l.insert(place(gid(2, 1), 3840, 1920));
        let mut r = router_with(l, gid(2, 0));

        let actions = r.motion(2000.0, 0.0);
        assert_eq!(r.owner(), node(2));
        assert_eq!(r.cursor().monitor(), gid(2, 1));
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, RouteAction::Yield { .. } | RouteAction::Handoff { .. })),
            "{actions:#?}"
        );
        assert!(
            matches!(
                actions.as_slice(),
                [RouteAction::Remote { frame, .. }] if frame.target == MonitorId(1)
            ),
            "the new target monitor must ride on the frame: {actions:#?}"
        );
    }

    #[test]
    fn leaving_a_peer_yields_to_it_and_not_to_ourselves() {
        let mut r = router();
        cross_right(&mut r);
        let home = r.motion(-2000.0, 0.0);
        assert_eq!(r.owner(), node(1));
        assert!(
            home.iter()
                .any(|a| *a == RouteAction::Yield { node: node(2) }),
            "{home:#?}"
        );
        // Coming home needs no TakeControl: there is no peer to tell.
        assert!(
            !home
                .iter()
                .any(|a| matches!(a, RouteAction::Handoff { .. })),
            "{home:#?}"
        );
        assert!(
            home.iter().any(|a| matches!(
                a,
                RouteAction::Local {
                    event: InputEvent::Pointer(PointerEvent::MoveTo { .. }),
                    ..
                }
            )),
            "the local pointer was never repositioned: {home:#?}"
        );
    }

    #[test]
    fn a_modifier_held_on_a_peer_is_released_there_before_control_comes_home() {
        let mut r = router();
        cross_right(&mut r);
        r.route(special(
            SpecialKey::AltLeft,
            KeyAction::Press,
            Modifiers::ALT,
        ));
        let home = r.motion(-2000.0, 0.0);

        let released = index_of(&home, |a| {
            matches!(
                a,
                RouteAction::Remote {
                    frame: InputFrame {
                        event: InputEvent::Key(KeyEvent {
                            action: KeyAction::Release,
                            ..
                        }),
                        ..
                    },
                    ..
                }
            )
        });
        let yielded = index_of(&home, |a| matches!(a, RouteAction::Yield { .. }));
        assert!(
            released < yielded,
            "a release after YieldControl may be discarded: {home:#?}"
        );
    }

    #[test]
    fn locking_keeps_the_cursor_on_this_machine() {
        let mut r = router();
        assert!(r.toggle_lock());
        let actions = cross_right(&mut r);
        assert_eq!(r.owner(), node(1));
        assert!(
            actions
                .iter()
                .all(|a| matches!(a, RouteAction::Local { .. })),
            "{actions:#?}"
        );
    }

    #[test]
    fn locking_keeps_input_flowing_to_the_peer_that_already_owns_it() {
        // The lock pins the cursor where it is; it does not claw control back.
        let mut r = router();
        cross_right(&mut r);
        r.set_locked(true);
        let actions = r.motion(-2000.0, 0.0);
        assert_eq!(r.owner(), node(2));
        assert!(
            actions
                .iter()
                .all(|a| matches!(a, RouteAction::Remote { .. })),
            "{actions:#?}"
        );
    }

    #[test]
    fn the_local_pointer_is_suppressed_only_while_a_peer_owns_the_cursor() {
        let mut r = router();
        assert!(!r.local_cursor_suppressed());
        cross_right(&mut r);
        assert!(r.local_cursor_suppressed());
        r.motion(-2000.0, 0.0);
        assert!(!r.local_cursor_suppressed());
    }

    #[test]
    fn sub_unit_motion_produces_no_traffic() {
        let mut l = GlobalLayout::new();
        l.insert(Placement {
            monitor: gid(1, 0),
            global_bounds: Rect::new(0, 0, 1920, 1080),
            cursor_scale: 0.2,
        });
        let mut r = router_with(l, gid(1, 0));
        assert!(r.motion(1.0, 0.0).is_empty(), "a fifth of a pixel was sent");
        assert!(r.motion(1.0, 0.0).is_empty());
        // The fifth report finally amounts to a whole unit.
        r.motion(1.0, 0.0);
        r.motion(1.0, 0.0);
        assert_eq!(r.motion(1.0, 0.0).len(), 1);
    }

    #[test]
    fn hostile_deltas_produce_no_traffic_and_no_handoff() {
        let mut r = router();
        for (dx, dy) in [(f64::NAN, f64::NAN), (f64::INFINITY, 0.0)] {
            assert!(r.motion(dx, dy).is_empty(), "({dx}, {dy}) reached the wire");
        }
        assert_eq!(r.owner(), node(1));
    }

    #[test]
    fn scroll_reaches_the_peer_holding_the_cursor() {
        let mut r = router();
        cross_right(&mut r);
        let actions = r.route(InputEvent::Pointer(PointerEvent::Scroll {
            dx: 0.0,
            dy: -3.0,
            unit: ScrollUnit::Lines,
        }));
        assert!(
            matches!(
                actions.as_slice(),
                [RouteAction::Remote { node: n, .. }] if *n == node(2)
            ),
            "{actions:#?}"
        );
    }

    #[test]
    fn relative_motion_events_go_through_the_virtual_cursor() {
        // Forwarding a delta verbatim would leave the router's cursor behind the
        // peer's, and the next crossing would resolve from a stale position.
        let mut r = router();
        r.route(InputEvent::Pointer(PointerEvent::MoveBy {
            dx: 2000.0,
            dy: 0.0,
        }));
        assert_eq!(r.owner(), node(2));
    }

    #[test]
    fn warping_hands_control_over_with_releases_first() {
        let mut r = router();
        r.route(special(
            SpecialKey::SuperLeft,
            KeyAction::Press,
            Modifiers::SUPER,
        ));
        let actions = r.warp(gid(2, 0), NormPos::new(0.25, 0.25)).unwrap();

        let released = index_of(&actions, |a| {
            matches!(
                a,
                RouteAction::Local {
                    event: InputEvent::Key(_),
                    ..
                }
            )
        });
        let handoff = index_of(&actions, |a| matches!(a, RouteAction::Handoff { .. }));
        assert!(released < handoff, "{actions:#?}");
        assert_eq!(r.owner(), node(2));
        assert!(r.local_cursor_suppressed());
    }

    #[test]
    fn warping_within_one_monitor_only_moves_the_pointer() {
        let mut r = router();
        let actions = r.warp(gid(1, 0), NormPos::new(0.1, 0.9)).unwrap();
        assert!(
            matches!(
                actions.as_slice(),
                [RouteAction::Local {
                    event: InputEvent::Pointer(PointerEvent::MoveTo { .. }),
                    ..
                }]
            ),
            "{actions:#?}"
        );
    }

    #[test]
    fn warping_to_a_monitor_that_is_gone_changes_nothing() {
        let mut r = router();
        assert_eq!(
            r.warp(gid(9, 9), NormPos::new(0.5, 0.5)),
            Err(WarpError::UnknownMonitor(gid(9, 9)))
        );
        assert_eq!(r.owner(), node(1));
    }

    #[test]
    fn unplugging_the_monitor_holding_the_cursor_brings_control_home() {
        // The peer's only display disappears while it owns the cursor. Control
        // must come back to the machine the user is sitting at, or the pointer is
        // stranded on a screen that no longer exists.
        let mut r = router();
        cross_right(&mut r);
        assert_eq!(r.owner(), node(2));

        let mut reduced = GlobalLayout::new();
        reduced.insert(place(gid(1, 0), 0, 1920));
        let actions = r.set_layout(reduced);

        assert_eq!(r.owner(), node(1));
        assert!(
            actions
                .iter()
                .any(|a| *a == RouteAction::Yield { node: node(2) }),
            "{actions:#?}"
        );
        assert!(!r.local_cursor_suppressed());
    }

    #[test]
    fn a_layout_change_that_keeps_the_cursor_home_is_silent() {
        let mut r = router();
        let mut wider = two_machines();
        wider.insert(place(gid(3, 0), 3840, 1920));
        assert!(r.set_layout(wider).is_empty());
        assert_eq!(r.cursor().monitor(), gid(1, 0));
    }

    #[test]
    fn an_empty_layout_leaves_ownership_alone_rather_than_guessing() {
        let mut r = router();
        assert!(r.set_layout(GlobalLayout::new()).is_empty());
        assert_eq!(r.owner(), node(1));
        // And motion is inert rather than panicking or drifting.
        assert!(r.motion(50.0, 50.0).is_empty());
    }

    #[test]
    fn release_all_lets_go_without_moving_control() {
        let mut r = router();
        cross_right(&mut r);
        r.route(special(
            SpecialKey::CtrlLeft,
            KeyAction::Press,
            Modifiers::CTRL,
        ));
        r.route(InputEvent::Pointer(PointerEvent::Button {
            button: MouseButton::Right,
            pressed: true,
        }));

        let actions = r.release_all();
        assert_eq!(r.owner(), node(2), "release_all must not move control");
        assert!(
            actions
                .iter()
                .all(|a| a.node() == Some(node(2)) && !matches!(a, RouteAction::Yield { .. })),
            "{actions:#?}"
        );
        assert_eq!(actions.len(), 2, "{actions:#?}");
        assert!(r.held_keys().is_empty() && r.held_buttons().is_empty());
        assert!(r.release_all().is_empty(), "nothing left to release");
    }

    #[test]
    fn release_control_from_the_capture_backend_clears_held_state() {
        let mut r = router();
        r.route(special(
            SpecialKey::CtrlLeft,
            KeyAction::Press,
            Modifiers::CTRL,
        ));
        r.route(InputEvent::ReleaseControl);
        assert!(r.held_keys().is_empty());
        let actions = cross_right(&mut r);
        assert!(
            !actions.iter().any(|a| matches!(
                a,
                RouteAction::Local {
                    event: InputEvent::Key(_),
                    ..
                }
            )),
            "{actions:#?}"
        );
    }

    #[test]
    fn held_state_starts_and_ends_empty_around_a_full_gesture() {
        let mut r = router();
        assert!(r.held.is_empty());
        r.route(special(
            SpecialKey::CtrlLeft,
            KeyAction::Press,
            Modifiers::CTRL,
        ));
        assert!(!r.held.is_empty());
        cross_right(&mut r);
        assert!(r.held.is_empty(), "handoff left state behind");
    }

    #[test]
    fn nearest_edge_is_deterministic_even_for_hostile_input() {
        assert_eq!(nearest_edge(NormPos::new(0.01, 0.5)), Edge::Left);
        assert_eq!(nearest_edge(NormPos::new(0.99, 0.5)), Edge::Right);
        assert_eq!(nearest_edge(NormPos::new(0.5, 0.02)), Edge::Top);
        assert_eq!(nearest_edge(NormPos::new(0.5, 0.98)), Edge::Bottom);
        // A tie, and a value that never came from a real monitor.
        assert_eq!(nearest_edge(NormPos::new(0.5, 0.5)), Edge::Left);
        assert_eq!(nearest_edge(NormPos::new(f64::NAN, f64::NAN)), Edge::Left);
    }
}
