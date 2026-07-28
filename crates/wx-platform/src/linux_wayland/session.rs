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

/// One portal session's state, as the rest of the backend sees it.
///
/// Two of these exist, because the desktop has two portals and they are not
/// interchangeable. [`super::WaylandInjector`] rides on
/// `org.freedesktop.portal.RemoteDesktop`, whose devices are *emulation* devices;
/// [`super::WaylandCapture`] rides on `org.freedesktop.portal.InputCapture`, which
/// is the only interface with zones, barriers and an activation to suppress local
/// input with. Measured on the alpha target: `RemoteDesktop` delivers no captured
/// events at all, in either handshake mode.
///
/// So this type is parameterised by what it owns rather than hard-coding it. Each
/// session sets and clears exactly its own capability bit and leaves the other
/// session's — and the backend's own `HAS_DISPLAYS` — untouched.
#[derive(Debug)]
pub struct SharedSession {
    status: Mutex<Status>,
    live: LiveCapabilities,
    /// What the backend can do with no session at all — display enumeration, so
    /// far. Held here because this type *replaces* the published set on every
    /// transition, and a session going live or being revoked must not take
    /// `HAS_DISPLAYS` down with it: the screens are still there either way.
    base: Capabilities,
    /// The bits this session governs, and the only ones it may ever set or clear.
    owned: Capabilities,
    /// The portal interface, for the messages the user reads.
    portal: &'static str,
}

#[derive(Debug)]
struct Status {
    state: SessionState,
    /// What to tell the user. Empty until something goes wrong.
    detail: String,
}

/// What the devices on a live `RemoteDesktop` session are worth to peers.
///
/// [`Capabilities::INJECT_INPUT`], and only that. The portal grants keyboard and
/// pointer access, but its devices are emulation devices — the seat offers
/// "WinXtend virtual keyboard" and friends — so what they buy is the ability to
/// *be driven*. Capture comes from the other portal entirely; see
/// [`INPUT_CAPTURE_CAPABILITIES`].
pub const INJECT_CAPABILITIES: Capabilities = Capabilities::INJECT_INPUT;

/// What clipboard access on a live `RemoteDesktop` session is worth to peers.
///
/// Both bits together, because the portal's clipboard is not per-format: what is
/// granted is access to the selection, and every format this backend maps rides
/// the same `SelectionRead`/`SetSelection` pair. Both were measured round-tripping
/// byte-exact on the alpha target with no focused surface, so claiming one and
/// withholding the other would be a false modesty peers would pay for.
///
/// Granted separately from [`INJECT_CAPABILITIES`]: the consent dialog carries
/// "Allow Clipboard Access" as its own toggle, and `Start` answers
/// `clipboard_enabled` separately from the device list.
pub const CLIPBOARD_CAPABILITIES: Capabilities =
    Capabilities::CLIPBOARD_TEXT.union(Capabilities::CLIPBOARD_IMAGE);

/// Everything a `RemoteDesktop` session may ever publish.
///
/// The `owned` mask, not a grant: one dialog answers for input and clipboard
/// independently, so the session has to be able to publish either half alone.
/// What it *actually* publishes is whatever [`super::driver`] hands
/// [`SharedSession::activate`] after reading `Start`'s results.
pub const REMOTE_DESKTOP_CAPABILITIES: Capabilities =
    INJECT_CAPABILITIES.union(CLIPBOARD_CAPABILITIES);

/// What a live `InputCapture` session is worth to peers.
///
/// [`Capabilities::CAPTURE_INPUT`]. Advertised only while the session is actually
/// live, and dropped the moment it is revoked, because a peer told this machine
/// can drive it will sit waiting for input that will never come.
pub const INPUT_CAPTURE_CAPABILITIES: Capabilities = Capabilities::CAPTURE_INPUT;

/// The portal interfaces the two sessions speak, for the messages a user reads.
pub const PORTAL_REMOTE_DESKTOP: &str = "RemoteDesktop";
pub const PORTAL_INPUT_CAPTURE: &str = "InputCapture";

impl SharedSession {
    /// `base` is what the backend advertises without a session, which this type
    /// preserves across every transition. `owned` is the bit this session governs
    /// and the only one it may set or clear; `portal` names the interface in the
    /// errors a user reads.
    pub fn new(
        live: LiveCapabilities,
        base: Capabilities,
        owned: Capabilities,
        portal: &'static str,
    ) -> Self {
        Self {
            status: Mutex::new(Status {
                state: SessionState::Idle,
                detail: String::new(),
            }),
            live,
            base,
            owned,
            portal,
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
                format!(
                    "the xdg-desktop-portal {} session is not available",
                    self.portal
                )
            } else {
                status.detail.clone()
            }
        };
        match status.state {
            SessionState::Active => None,
            SessionState::Denied => Some(PlatformError::PermissionDenied(detail())),
            // The operation string has to be `'static`, so the two portals get one
            // wording rather than a leaked per-session string; the detail above
            // names which one wherever it matters.
            SessionState::Unsupported => Some(PlatformError::Unsupported {
                operation: "the xdg-desktop-portal input session",
                backend: BACKEND,
            }),
            SessionState::Idle => Some(PlatformError::Other(format!(
                "the xdg-desktop-portal {} session was never started",
                self.portal
            ))),
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
    /// `base | granted`, because this type owns [`REMOTE_DESKTOP_CAPABILITIES`] and
    /// nothing more. `base` is the floor it can never publish less than; a bit
    /// somebody else put on the live set — one that describes the build rather
    /// than the portal — is not this type's to clear, and replacing the whole set
    /// on every transition would silently drop it the moment the session came up
    /// or went away. Only the session's own bits are set here, and `granted` is
    /// masked so a caller cannot smuggle a foreign bit in through it either.
    ///
    /// Masked against what the session *owns* rather than against what it
    /// currently grants, so the clear on the way out stays real however small the
    /// grant was — and so the two sessions cannot overwrite each other's bit.
    fn publish(&self, granted: Capabilities) {
        let held = self.live.get().union(self.base);
        self.live.set(Capabilities(
            (held.0 & !self.owned.0) | (granted.0 & self.owned.0),
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

    /// The `RemoteDesktop` session on a backend that already advertises `base`
    /// without any portal.
    fn with_base(base: Capabilities) -> (SharedSession, LiveCapabilities) {
        let live = LiveCapabilities::fixed(base);
        (
            SharedSession::new(
                live.clone(),
                base,
                REMOTE_DESKTOP_CAPABILITIES,
                PORTAL_REMOTE_DESKTOP,
            ),
            live,
        )
    }

    /// The two sessions sharing one published set, which is what the backend
    /// really has.
    fn both(base: Capabilities) -> (SharedSession, SharedSession, LiveCapabilities) {
        let live = LiveCapabilities::fixed(base);
        (
            SharedSession::new(
                live.clone(),
                base,
                REMOTE_DESKTOP_CAPABILITIES,
                PORTAL_REMOTE_DESKTOP,
            ),
            SharedSession::new(
                live.clone(),
                base,
                INPUT_CAPTURE_CAPABILITIES,
                PORTAL_INPUT_CAPTURE,
            ),
            live,
        )
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
        let (s, live) = session();
        s.starting();
        assert!(
            live.get().is_empty(),
            "nothing is granted until Start returns"
        );
        s.activate(INJECT_CAPABILITIES);
        assert_eq!(s.state(), SessionState::Active);
        assert!(live.get().contains(Capabilities::INJECT_INPUT));
        assert!(
            !live.get().contains(Capabilities::CLIPBOARD_TEXT),
            "one dialog, two answers: a granted device list says nothing about the clipboard"
        );
        assert!(s.error().is_none());
    }

    #[test]
    fn one_session_publishes_input_and_clipboard_independently() {
        // The trap the `owned` mask exists for. The consent dialog carries "Allow
        // Clipboard Access" as a toggle of its own, so all four answers are
        // reachable and each has to be sayable — a user who granted the clipboard
        // and refused remote interaction has a machine that syncs copy and paste
        // and cannot be driven, and peers must be told exactly that.
        for (granted, input, clipboard) in [
            (
                INJECT_CAPABILITIES.union(CLIPBOARD_CAPABILITIES),
                true,
                true,
            ),
            (INJECT_CAPABILITIES, true, false),
            (CLIPBOARD_CAPABILITIES, false, true),
            (Capabilities::NONE, false, false),
        ] {
            let (s, live) = with_base(Capabilities::HAS_DISPLAYS);
            s.starting();
            s.activate(granted);
            assert_eq!(
                live.get().contains(Capabilities::INJECT_INPUT),
                input,
                "{granted:?}"
            );
            assert_eq!(
                live.get().contains(Capabilities::CLIPBOARD_TEXT),
                clipboard,
                "{granted:?}"
            );
            assert_eq!(
                live.get().contains(Capabilities::CLIPBOARD_IMAGE),
                clipboard,
                "{granted:?}"
            );
            assert!(live.get().contains(Capabilities::HAS_DISPLAYS));
        }
    }

    #[test]
    fn revoking_the_session_takes_the_clipboard_with_it() {
        // The clipboard rides the injection session, so it dies with it. A peer
        // still told this machine can paste would offer content to a backend whose
        // portal has gone.
        let (s, live) = with_base(Capabilities::HAS_DISPLAYS);
        s.starting();
        s.activate(REMOTE_DESKTOP_CAPABILITIES);
        assert!(live.get().contains(CLIPBOARD_CAPABILITIES));
        s.denied("the desktop portal revoked the session");
        assert!(!live.get().contains(Capabilities::CLIPBOARD_TEXT));
        assert!(!live.get().contains(Capabilities::CLIPBOARD_IMAGE));
        assert!(live.get().contains(Capabilities::HAS_DISPLAYS));
    }

    #[test]
    fn the_capture_session_can_never_publish_a_clipboard_bit() {
        // The two sessions share one published set and each owns its own bits.
        // `InputCapture` has no clipboard of its own — `RequestClipboard` is
        // refused for it on the alpha target — so a grant that tried to claim one
        // through it is masked away rather than believed.
        let (_, capture, live) = both(Capabilities::NONE);
        capture.starting();
        capture.activate(INPUT_CAPTURE_CAPABILITIES.union(CLIPBOARD_CAPABILITIES));
        assert!(live.get().contains(Capabilities::CAPTURE_INPUT));
        assert!(!live.get().contains(Capabilities::CLIPBOARD_TEXT));
    }

    #[test]
    fn each_portal_advertises_its_own_half_and_only_that() {
        // The two sessions are separate grants on separate portals, with separate
        // consent dialogs, and either can be refused on its own. A machine that
        // may be driven but cannot drive is the ordinary headless case; a machine
        // that can drive but not be driven is what a user who said no to one
        // dialog has. Both have to be sayable.
        assert!(REMOTE_DESKTOP_CAPABILITIES.contains(Capabilities::INJECT_INPUT));
        assert!(!REMOTE_DESKTOP_CAPABILITIES.contains(Capabilities::CAPTURE_INPUT));
        assert!(INPUT_CAPTURE_CAPABILITIES.contains(Capabilities::CAPTURE_INPUT));
        assert!(!INPUT_CAPTURE_CAPABILITIES.contains(Capabilities::INJECT_INPUT));

        let (inject, capture, live) = both(Capabilities::HAS_DISPLAYS);
        inject.starting();
        inject.activate(REMOTE_DESKTOP_CAPABILITIES);
        assert!(live.get().contains(Capabilities::INJECT_INPUT));
        assert!(!live.get().contains(Capabilities::CAPTURE_INPUT));

        capture.starting();
        capture.activate(INPUT_CAPTURE_CAPABILITIES);
        assert!(live.get().contains(Capabilities::INJECT_INPUT));
        assert!(live.get().contains(Capabilities::CAPTURE_INPUT));
        assert!(
            live.get().contains(Capabilities::HAS_DISPLAYS),
            "the screens are real whatever either portal answered"
        );
    }

    #[test]
    fn losing_one_portal_does_not_unadvertise_the_other() {
        // The failure this parameterisation exists to prevent. Both sessions
        // publish into one set, and a shared mask would have the capture session's
        // revocation clear injection — leaving a machine that can still perfectly
        // well be driven telling its peers it cannot.
        let (inject, capture, live) = both(Capabilities::HAS_DISPLAYS);
        inject.starting();
        inject.activate(REMOTE_DESKTOP_CAPABILITIES);
        capture.starting();
        capture.activate(INPUT_CAPTURE_CAPABILITIES);

        capture.denied("the user closed the sharing indicator");
        assert!(!live.get().contains(Capabilities::CAPTURE_INPUT));
        assert!(
            live.get().contains(Capabilities::INJECT_INPUT),
            "capture being revoked stopped this machine being driven"
        );
        assert!(live.get().contains(Capabilities::HAS_DISPLAYS));
    }

    #[test]
    fn losing_the_session_takes_injection_away_again() {
        // The half of the lifecycle that matters most now that the bit is really
        // published: a peer must stop being told this machine accepts input the
        // moment the portal takes the session back.
        let (s, live) = with_base(Capabilities::HAS_DISPLAYS);
        s.starting();
        s.activate(REMOTE_DESKTOP_CAPABILITIES);
        assert!(live.get().contains(Capabilities::INJECT_INPUT));
        s.denied("the desktop portal revoked the session");
        assert!(!live.get().contains(Capabilities::INJECT_INPUT));
        assert!(live.get().contains(Capabilities::HAS_DISPLAYS));
    }

    #[test]
    fn revocation_drops_the_capabilities_and_reports_permission_denied() {
        // The product requirement in one test: the capability goes away so peers
        // stop being told this machine can drive them, and the error is the one the
        // UI turns into an actionable prompt.
        let (s, live) = session();
        s.starting();
        s.activate(REMOTE_DESKTOP_CAPABILITIES);
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
        s.activate(REMOTE_DESKTOP_CAPABILITIES);

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
        s.activate(REMOTE_DESKTOP_CAPABILITIES);
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
        // A session owns its own input bit and nothing else. Publishing its
        // grant used to replace the whole set, which silently unadvertised the
        // screens found by display enumeration the moment the portal answered —
        // and again when it was revoked, taking the node out of peers' layouts
        // over a permission it never needed for that.
        let (s, live) = with_base(Capabilities::HAS_DISPLAYS);
        assert!(live.get().contains(Capabilities::HAS_DISPLAYS));

        s.starting();
        assert!(live.get().contains(Capabilities::HAS_DISPLAYS));

        s.activate(REMOTE_DESKTOP_CAPABILITIES);
        assert!(live.get().contains(Capabilities::HAS_DISPLAYS));
        assert!(live.get().contains(REMOTE_DESKTOP_CAPABILITIES));

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
        // must not blink off when one does. A session owns its own input bit and
        // nothing else; anything else on the published set is somebody's to keep,
        // not this type's to overwrite.
        let (s, live) = with_base(Capabilities::HAS_DISPLAYS);
        live.set(Capabilities::HAS_DISPLAYS.union(Capabilities::CAPABILITY_UPDATES));

        s.starting();
        s.activate(REMOTE_DESKTOP_CAPABILITIES);
        assert!(live.get().contains(Capabilities::CAPABILITY_UPDATES));
        assert!(live.get().contains(REMOTE_DESKTOP_CAPABILITIES));

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
