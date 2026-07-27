//! The portal session's state, shared between the driver thread and the traits.
//!
//! Deliberately free of D-Bus, libei and async, so that the rules that actually
//! matter — which transitions are legal, what each state reports to the agent, and
//! above all that a refusal is final — are testable on any machine with no session
//! at all.

use std::sync::Mutex;

use wx_proto::Capabilities;

use crate::error::PlatformError;
use crate::LiveCapabilities;

use super::BACKEND;

/// Where the portal session is in its life.
///
/// # Why refusal is terminal
///
/// There is no `Retrying` state and no way back out of [`SessionState::Denied`].
/// Every way of losing the session — the user dismisses the dialog, the compositor
/// revokes it, the screen locks — arrives here, and a daemon that answered any of
/// them by starting the sequence again would put the dialog back on screen, forever,
/// against a user who has already said no. Recovery is a new agent run, which the
/// restore token makes silent when the user did in fact consent before.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// Never started. What `cargo test` and any non-daemon caller sees.
    Idle,
    /// D-Bus calls are in flight; the consent dialog may be on screen.
    Starting,
    /// Live: devices granted and the libei transport connected.
    Active,
    /// Permission refused or withdrawn. Terminal.
    Denied,
    /// There is no portal to talk to — headless, CI, a desktop without a
    /// `RemoteDesktop` backend. Terminal.
    Unsupported,
    /// The portal is there but the sequence failed for a reason that is not about
    /// permission, such as D-Bus going away. Terminal, for the same reason
    /// [`SessionState::Denied`] is: this backend does not reconnect.
    Failed,
    /// Torn down deliberately, on agent shutdown. Terminal.
    Stopped,
}

impl SessionState {
    /// Whether nothing further will happen without a new agent run.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            SessionState::Denied
                | SessionState::Unsupported
                | SessionState::Failed
                | SessionState::Stopped
        )
    }
}

/// The session as the rest of the backend sees it.
///
/// One of these is shared by [`super::WaylandCapture`] and [`super::WaylandInjector`]:
/// `SelectDevices` asks for keyboard and pointer together and the portal grants one
/// session for both, so building a second would put a second dialog on screen and
/// leave two sessions to revoke.
#[derive(Debug)]
pub struct SharedSession {
    status: Mutex<Status>,
    live: LiveCapabilities,
    /// What the backend can do with no session at all — display enumeration, so
    /// far. Held here because this type *replaces* the published set on every
    /// transition, and a session going live or being revoked must not take
    /// `HAS_DISPLAYS` down with it: the screens are still there either way.
    base: Capabilities,
}

#[derive(Debug)]
struct Status {
    state: SessionState,
    /// What to tell the user. Empty until something goes wrong.
    detail: String,
}

/// The bits this session governs, and the only ones it may ever set or clear.
///
/// Both, from one session: the portal's `SelectDevices` grants keyboard and pointer
/// access that serves capture and injection alike. Fixed for the life of the type —
/// it is the mask [`SharedSession::publish`] applies, so it says what the session
/// *owns*, not what it currently grants. Anything outside it belongs to somebody
/// else and survives every transition untouched.
pub const SESSION_OWNED_CAPABILITIES: Capabilities =
    Capabilities::CAPTURE_INPUT.union(Capabilities::INJECT_INPUT);

/// What a live portal session contributes to the advertised set.
///
/// **Empty today, deliberately.** The portal session and its libei transport are
/// real, but `WaylandCapture::start`, `WaylandInjector::inject` and
/// `WaylandInjector::warp_cursor` still return `Unsupported`: translating
/// `InputEvent`s to and from `ei` events is a skeleton. Advertising
/// [`Capabilities::INJECT_INPUT`] here would tell every peer this machine can be
/// driven, and the cursor would cross onto a desktop where the keyboard goes dead
/// with nothing to say why — against the rule this backend is built on that it
/// advertises nothing it cannot do. It would also silence the one warning built to
/// surface exactly this state, `Engine::on_peer_ready`'s notice about a peer that
/// has screens but does not advertise injection.
///
/// This is the single switch: when the injection (#6) and capture (#7) slices land
/// their event translation, they set this to [`SESSION_OWNED_CAPABILITIES`] and the
/// whole lifecycle above — activation, revocation, re-advertisement — publishes it
/// with nothing else to change.
pub const SESSION_CAPABILITIES: Capabilities = Capabilities::NONE;

impl SharedSession {
    /// `base` is what the backend advertises without a session, which this type
    /// preserves across every transition.
    pub fn new(live: LiveCapabilities, base: Capabilities) -> Self {
        Self {
            status: Mutex::new(Status {
                state: SessionState::Idle,
                detail: String::new(),
            }),
            live,
            base,
        }
    }

    pub fn state(&self) -> SessionState {
        self.lock().state
    }

    pub fn detail(&self) -> String {
        self.lock().detail.clone()
    }

    /// The session is being established; the dialog may be up.
    pub fn starting(&self) {
        self.enter(SessionState::Starting, String::new());
    }

    /// Permission granted and the transport is up.
    pub fn activate(&self, granted: Capabilities) {
        if self.enter(SessionState::Active, String::new()) {
            self.publish(granted);
        }
    }

    /// The user refused, or the portal took the session away.
    pub fn denied(&self, detail: impl Into<String>) {
        self.terminate(SessionState::Denied, detail.into());
    }

    /// There is no portal on this machine.
    pub fn unsupported(&self, detail: impl Into<String>) {
        self.terminate(SessionState::Unsupported, detail.into());
    }

    /// The sequence broke for a reason that is not the user's decision.
    pub fn failed(&self, detail: impl Into<String>) {
        self.terminate(SessionState::Failed, detail.into());
    }

    /// Torn down on purpose.
    pub fn stopped(&self) {
        self.terminate(SessionState::Stopped, String::new());
    }

    /// The error to hand a caller that wanted to capture or inject.
    ///
    /// `None` while the session is live. Everything else is an error, including
    /// [`SessionState::Starting`]: the answer to "can you capture yet" while a
    /// consent dialog is on screen is no.
    pub fn error(&self) -> Option<PlatformError> {
        let status = self.lock();
        let detail = || {
            if status.detail.is_empty() {
                "the xdg-desktop-portal RemoteDesktop session is not available".to_string()
            } else {
                status.detail.clone()
            }
        };
        match status.state {
            SessionState::Active => None,
            SessionState::Denied => Some(PlatformError::PermissionDenied(detail())),
            SessionState::Unsupported => Some(PlatformError::Unsupported {
                operation: "the xdg-desktop-portal RemoteDesktop session",
                backend: BACKEND,
            }),
            SessionState::Idle => Some(PlatformError::Other(
                "the xdg-desktop-portal RemoteDesktop session was never started".into(),
            )),
            SessionState::Starting => Some(PlatformError::Other(
                "waiting for the desktop portal consent dialog".into(),
            )),
            SessionState::Failed | SessionState::Stopped => Some(PlatformError::Other(detail())),
        }
    }

    /// Republish with the session's own bits set to `granted` and nothing else
    /// touched.
    ///
    /// Written as a read-modify-write over the published set rather than as
    /// `base | granted`, because this type owns [`SESSION_OWNED_CAPABILITIES`] and
    /// nothing more. `base` is the floor it can never publish less than; a bit
    /// somebody else put on the live set — one that describes the build rather
    /// than the portal — is not this type's to clear, and replacing the whole set
    /// on every transition would silently drop it the moment the session came up
    /// or went away. Only the session's own bits are set here, and `granted` is
    /// masked so a caller cannot smuggle a foreign bit in through it either.
    ///
    /// Masked against what the session *owns* rather than against what it currently
    /// grants, so that the clear on the way out stays real while
    /// [`SESSION_CAPABILITIES`] is empty.
    fn publish(&self, granted: Capabilities) {
        let held = self.live.get().union(self.base);
        self.live.set(Capabilities(
            (held.0 & !SESSION_OWNED_CAPABILITIES.0) | (granted.0 & SESSION_OWNED_CAPABILITIES.0),
        ));
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Status> {
        // Poisoning would mean the driver thread panicked mid-transition. The
        // session is unusable either way, and refusing to read the state would turn
        // that into a panic in the agent's tick loop.
        self.status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Move to a non-terminal state, unless a terminal one already won.
    fn enter(&self, state: SessionState, detail: String) -> bool {
        let mut status = self.lock();
        if status.state.is_terminal() {
            return false;
        }
        status.state = state;
        status.detail = detail;
        true
    }

    /// Move to a terminal state and drop every capability that depended on the
    /// session.
    ///
    /// The first terminal state wins: a revocation that arrives while shutdown is
    /// running should not overwrite "stopped on purpose" with "the user refused",
    /// and neither should be reported twice.
    fn terminate(&self, state: SessionState, detail: String) {
        let mut status = self.lock();
        if status.state.is_terminal() {
            return;
        }
        status.state = state;
        status.detail = detail;
        drop(status);
        // Dropped under no lock, but this is the only writer, so there is no order
        // in which a stale capability can be published after a terminal state.
        //
        // Only the bits this session owns are dropped. Losing input permission does
        // not unplug the monitors, and clearing the whole set would tell peers to
        // drop this node out of the layout entirely.
        self.publish(Capabilities::NONE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> (SharedSession, LiveCapabilities) {
        with_base(Capabilities::NONE)
    }

    /// A session on a backend that already advertises `base` without any portal.
    fn with_base(base: Capabilities) -> (SharedSession, LiveCapabilities) {
        let live = LiveCapabilities::fixed(base);
        (SharedSession::new(live.clone(), base), live)
    }

    #[test]
    fn a_session_that_was_never_started_advertises_nothing() {
        let (s, live) = session();
        assert_eq!(s.state(), SessionState::Idle);
        assert!(live.get().is_empty());
        assert!(s.error().is_some());
    }

    #[test]
    fn a_live_session_publishes_exactly_what_it_was_granted() {
        // Drives the mechanism with an explicit non-empty grant rather than with
        // whatever `SESSION_CAPABILITIES` happens to be today, so the publish path
        // stays proven while the shipped contribution is empty — and so the slices
        // that repopulate it inherit a test that already covers them.
        let (s, live) = session();
        s.starting();
        assert!(
            live.get().is_empty(),
            "nothing is granted until Start returns"
        );
        s.activate(SESSION_OWNED_CAPABILITIES);
        assert_eq!(s.state(), SessionState::Active);
        assert!(live.get().contains(Capabilities::CAPTURE_INPUT));
        assert!(live.get().contains(Capabilities::INJECT_INPUT));
        assert!(s.error().is_none());
    }

    #[test]
    fn a_live_session_contributes_nothing_while_event_translation_is_a_skeleton() {
        // Today's truth, and the reason it is true: capture and injection still
        // return `Unsupported`, so a granted session must not tell peers this
        // machine can be driven. The session is `Active` all the same — the portal
        // really did grant it — which is what the lifecycle above is for.
        assert!(SESSION_CAPABILITIES.is_empty());

        let (s, live) = with_base(Capabilities::HAS_DISPLAYS);
        s.starting();
        s.activate(SESSION_CAPABILITIES);
        assert_eq!(s.state(), SessionState::Active);
        assert!(!live.get().contains(Capabilities::CAPTURE_INPUT));
        assert!(!live.get().contains(Capabilities::INJECT_INPUT));
        assert!(
            live.get().contains(Capabilities::HAS_DISPLAYS),
            "the screens are real whatever the portal answered"
        );
    }

    #[test]
    fn revocation_drops_the_capabilities_and_reports_permission_denied() {
        // The product requirement in one test: the capability goes away so peers
        // stop being told this machine can drive them, and the error is the one the
        // UI turns into an actionable prompt.
        let (s, live) = session();
        s.starting();
        s.activate(SESSION_OWNED_CAPABILITIES);
        s.denied("the desktop portal revoked the session");

        assert_eq!(s.state(), SessionState::Denied);
        assert!(live.get().is_empty());
        assert!(matches!(
            s.error(),
            Some(PlatformError::PermissionDenied(msg)) if msg.contains("revoked")
        ));
    }

    #[test]
    fn a_refusal_cannot_be_undone_by_a_later_start() {
        // The whole point of the terminal states: nothing may put the consent
        // dialog back in front of a user who already said no.
        let (s, live) = session();
        s.starting();
        s.denied("dialog dismissed");

        s.starting();
        s.activate(SESSION_OWNED_CAPABILITIES);

        assert_eq!(s.state(), SessionState::Denied);
        assert!(
            live.get().is_empty(),
            "a denied session must advertise nothing"
        );
    }

    #[test]
    fn the_first_terminal_reason_is_the_one_reported() {
        // A revocation racing an agent shutdown must not be reported to the user as
        // a permission problem they need to act on.
        let (s, _) = session();
        s.starting();
        s.activate(SESSION_OWNED_CAPABILITIES);
        s.stopped();
        s.denied("revoked");
        assert_eq!(s.state(), SessionState::Stopped);
        assert!(!matches!(
            s.error(),
            Some(PlatformError::PermissionDenied(_))
        ));
    }

    #[test]
    fn no_portal_is_unsupported_rather_than_denied() {
        // Headless and CI have no portal at all. Reporting that as a permission
        // problem would send the user looking for a dialog that never existed.
        let (s, _) = session();
        s.starting();
        s.unsupported("no D-Bus session bus");
        assert_eq!(s.state(), SessionState::Unsupported);
        assert!(matches!(
            s.error(),
            Some(PlatformError::Unsupported { backend, .. }) if backend == BACKEND
        ));
    }

    #[test]
    fn a_transport_failure_is_not_reported_as_the_user_refusing() {
        // D-Bus going away is not a decision anybody made; telling the user their
        // permission was denied would send them to the wrong settings panel.
        let (s, live) = session();
        s.starting();
        s.failed("D-Bus connection closed");
        assert_eq!(s.state(), SessionState::Failed);
        assert!(live.get().is_empty());
        assert!(!matches!(
            s.error(),
            Some(PlatformError::PermissionDenied(_))
        ));
    }

    #[test]
    fn while_the_dialog_is_up_nothing_is_advertised_but_it_is_not_a_refusal() {
        let (s, live) = session();
        s.starting();
        assert!(live.get().is_empty());
        assert!(s.error().is_some());
        assert!(
            !s.state().is_terminal(),
            "a pending dialog can still succeed"
        );
    }

    #[test]
    fn the_sessions_transitions_leave_display_capability_alone() {
        // The session owns capture and injection and nothing else. Publishing its
        // grant used to replace the whole set, which silently unadvertised the
        // screens found by display enumeration the moment the portal answered —
        // and again when it was revoked, taking the node out of peers' layouts
        // over a permission it never needed for that.
        let (s, live) = with_base(Capabilities::HAS_DISPLAYS);
        assert!(live.get().contains(Capabilities::HAS_DISPLAYS));

        s.starting();
        assert!(live.get().contains(Capabilities::HAS_DISPLAYS));

        s.activate(SESSION_OWNED_CAPABILITIES);
        assert!(live.get().contains(Capabilities::HAS_DISPLAYS));
        assert!(live.get().contains(SESSION_OWNED_CAPABILITIES));

        s.denied("the desktop portal revoked the session");
        assert!(
            live.get().contains(Capabilities::HAS_DISPLAYS),
            "losing input permission does not unplug the monitors"
        );
        assert!(!live.get().contains(Capabilities::CAPTURE_INPUT));
        assert!(!live.get().contains(Capabilities::INJECT_INPUT));
    }

    #[test]
    fn a_bit_this_session_does_not_own_survives_every_transition() {
        // `CAPABILITY_UPDATES` says what this build's wire implementation
        // understands, so it is true on a machine whose portal never answered and
        // must not blink off when one does. The session owns capture and injection
        // and nothing else; anything else on the published set is somebody's to
        // keep, not this type's to overwrite.
        let (s, live) = with_base(Capabilities::HAS_DISPLAYS);
        live.set(Capabilities::HAS_DISPLAYS.union(Capabilities::CAPABILITY_UPDATES));

        s.starting();
        s.activate(SESSION_OWNED_CAPABILITIES);
        assert!(live.get().contains(Capabilities::CAPABILITY_UPDATES));
        assert!(live.get().contains(SESSION_OWNED_CAPABILITIES));

        s.denied("the desktop portal revoked the session");
        assert!(live.get().contains(Capabilities::CAPABILITY_UPDATES));
        assert!(live.get().contains(Capabilities::HAS_DISPLAYS));
        assert!(!live.get().contains(Capabilities::CAPTURE_INPUT));
        assert!(!live.get().contains(Capabilities::INJECT_INPUT));
    }

    #[test]
    fn every_terminal_state_reports_itself_as_terminal() {
        for state in [
            SessionState::Denied,
            SessionState::Unsupported,
            SessionState::Failed,
            SessionState::Stopped,
        ] {
            assert!(state.is_terminal(), "{state:?}");
        }
        for state in [
            SessionState::Idle,
            SessionState::Starting,
            SessionState::Active,
        ] {
            assert!(!state.is_terminal(), "{state:?}");
        }
    }
}
