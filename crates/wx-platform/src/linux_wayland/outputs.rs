//! The Wayland protocol half of display enumeration.
//!
//! Everything here talks to the compositor; the interpretation of what it says
//! lives in [`super::display`], which is testable without one.
//!
//! The shape is the standard registry dance. Bind every `wl_output` and the
//! `zxdg_output_manager_v1` global, roundtrip so both are known, ask the manager
//! for an `xdg_output` per output, and roundtrip again to collect the logical
//! geometry. The manager has to be bound before the `xdg_output`s can be asked
//! for, and the registry does not promise to advertise it before the outputs, so
//! the two phases cannot be collapsed into one.

use std::io::ErrorKind;
use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use wayland_client::backend::WaylandError;
use wayland_client::protocol::{wl_callback, wl_output, wl_registry};
use wayland_client::{Connection, Dispatch, DispatchError, EventQueue, QueueHandle, WEnum};
use wayland_protocols::xdg::xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1};

use super::display::{RawOutput, WL_OUTPUT_VERSION, XDG_OUTPUT_MANAGER_VERSION};
use crate::error::{PlatformError, Result};

/// How many roundtrips to spend waiting for logical geometry.
///
/// One is enough on every compositor tested: `get_xdg_output` replies in the same
/// burst. The second covers a compositor that defers the reply, and the bound
/// stops a compositor that never sends it from spinning here forever — a monitor
/// that stays missing is recoverable on the next housekeeping tick, a hung
/// enumeration is not.
const GEOMETRY_ROUNDTRIPS: usize = 2;

/// Wall-clock budget for one whole enumeration, roundtrips included.
///
/// This is a liveness bound, not a performance one. `monitors()` is called inline
/// from the agent's single engine loop, the same loop that forwards captured input
/// and answers peer heartbeats. A compositor that stops servicing its socket
/// without closing it — wedged under load, SIGSTOPped, mid-crash — would otherwise
/// block that loop forever and strand the cursor on whichever machine owns it.
/// Giving up instead costs one tick's monitor list, which the next tick recomputes.
///
/// Enumeration measures ~200us against a live compositor, so this is pure headroom.
const ENUMERATION_BUDGET: Duration = Duration::from_millis(250);

/// One connection's worth of enumeration state.
struct Enumerator {
    manager: Option<zxdg_output_manager_v1::ZxdgOutputManagerV1>,
    outputs: Vec<Entry>,
    /// Set when the `wl_display.sync` barrier of the roundtrip in flight replies.
    synced: bool,
}

struct Entry {
    proxy: wl_output::WlOutput,
    raw: RawOutput,
}

/// Read the current outputs from the compositor over a fresh connection.
///
/// See [`super::display::WaylandDisplays`] for why the connection is not kept.
pub(super) fn enumerate() -> Result<Vec<RawOutput>> {
    let deadline = Instant::now() + ENUMERATION_BUDGET;

    let conn = Connection::connect_to_env().map_err(|e| {
        // Not an error worth a warning: a headless agent, a CI runner and a
        // machine on a TTY all land here, and none of them is broken.
        tracing::debug!(error = %e, "no wayland connection");
        PlatformError::NoDisplayServer
    })?;

    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    // The registry proxy is kept alive by the connection for as long as the queue
    // lives, which is the whole of this function.
    let _registry = conn.display().get_registry(&qh, ());

    let mut state = Enumerator {
        manager: None,
        outputs: Vec::new(),
        synced: false,
    };

    // First pass: learn the globals, and bind every output as it is advertised.
    roundtrip(&conn, &mut queue, &qh, &mut state, deadline)?;

    if state.outputs.is_empty() {
        // A compositor with no outputs at all — a nested session being torn down,
        // or a headless mutter. Not a failure; there is simply nothing to publish.
        return Ok(Vec::new());
    }

    // Second pass: logical geometry, which is the only reason this backend exists.
    let manager = state.manager.clone().ok_or(PlatformError::Unsupported {
        operation: "display enumeration (compositor offers no xdg_output)",
        backend: super::BACKEND,
    })?;
    for (index, entry) in state.outputs.iter().enumerate() {
        manager.get_xdg_output(&entry.proxy, &qh, index);
    }

    for _ in 0..GEOMETRY_ROUNDTRIPS {
        roundtrip(&conn, &mut queue, &qh, &mut state, deadline)?;
        // The same completeness rule `to_monitors` drops on: an output missing
        // either half of its geometry is not published, so stopping early on the
        // size alone would silently lose a monitor whose position is still in
        // flight.
        if state
            .outputs
            .iter()
            .all(|e| e.raw.logical_pos.is_some() && e.raw.logical_size.is_some())
        {
            break;
        }
    }

    Ok(state.outputs.into_iter().map(|e| e.raw).collect())
}

/// A roundtrip that gives up at `deadline` instead of blocking indefinitely.
///
/// [`EventQueue::roundtrip`] loops on `blocking_dispatch`, which polls the socket
/// with no timeout; this is the same dance with the wait bounded. See
/// [`ENUMERATION_BUDGET`] for why the bound has to exist at all.
fn roundtrip(
    conn: &Connection,
    queue: &mut EventQueue<Enumerator>,
    qh: &QueueHandle<Enumerator>,
    state: &mut Enumerator,
    deadline: Instant,
) -> Result<()> {
    state.synced = false;
    // `wl_display.sync` replies only once the server has processed everything sent
    // before it, which is what makes this a roundtrip rather than a poll.
    let _barrier = conn.display().sync(qh, ());

    loop {
        queue.dispatch_pending(state).map_err(dispatch_failed)?;
        if state.synced {
            return Ok(());
        }

        conn.flush()
            .map_err(|e| wayland_failed("flushing the connection", e))?;

        // `None` means the queue still holds events nobody has dispatched; going
        // round drains them rather than waiting for a socket that may stay quiet.
        let Some(guard) = conn.prepare_read() else {
            continue;
        };

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(timed_out());
        }

        let fd = conn.as_fd();
        let mut fds = [rustix::event::PollFd::new(
            &fd,
            rustix::event::PollFlags::IN | rustix::event::PollFlags::ERR,
        )];
        let timeout = rustix::event::Timespec {
            tv_sec: remaining.as_secs() as _,
            tv_nsec: remaining.subsec_nanos() as _,
        };
        match rustix::event::poll(&mut fds, Some(&timeout)) {
            Ok(0) => return Err(timed_out()),
            Ok(_) => {}
            // Re-derive the timeout from the deadline rather than restarting it,
            // so a stream of signals cannot extend the budget indefinitely.
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => {
                return Err(PlatformError::Other(format!(
                    "polling the wayland socket failed: {e}"
                )));
            }
        }

        match guard.read() {
            Ok(_) => {}
            // Another thread beat us to the socket, or the readiness was spurious.
            // Either way the deadline still bounds the retry.
            Err(WaylandError::Io(e)) if e.kind() == ErrorKind::WouldBlock => {}
            Err(e) => return Err(wayland_failed("reading from the socket", e)),
        }
    }
}

/// The one place the wording of a lost connection lives.
///
/// A dead connection is reported rather than papered over: the compositor
/// restarting is exactly the moment the layout must stop trusting its rectangles.
/// The next call opens a new one, so this heals itself, and
/// [`is_self_healing`] recognises it by this prefix.
const CONNECTION_LOST: &str = "wayland connection lost";

/// The one place the wording of a protocol failure lives.
///
/// Deliberately *not* self-healing. A message this client cannot decode, or a
/// server-side error raised against one of its objects, means this client asked
/// for something wrong — a version the compositor rejects, a request on a
/// destroyed object, an unknown opcode. Reconnecting repeats it.
const PROTOCOL_FAILED: &str = "wayland protocol error";

/// The same, for the [`ENUMERATION_BUDGET`] deadline.
const ENUMERATION_TIMED_OUT: &str = "wayland enumeration timed out";

/// Split a connection failure by what actually went wrong rather than by which
/// call reported it: the IO half is the compositor going away, the protocol half
/// is a bug on this side of the socket.
fn wayland_failed(during: &str, e: WaylandError) -> PlatformError {
    match e {
        WaylandError::Io(e) => {
            PlatformError::Other(format!("{CONNECTION_LOST} while {during}: {e}"))
        }
        WaylandError::Protocol(e) => {
            PlatformError::Other(format!("{PROTOCOL_FAILED} while {during}: {e}"))
        }
    }
}

/// `BadMessage` is an event this client failed to decode, which is the same class
/// of bug as a protocol error and is classified with it.
fn dispatch_failed(e: DispatchError) -> PlatformError {
    match e {
        DispatchError::Backend(e) => wayland_failed("dispatching events", e),
        DispatchError::BadMessage {
            sender_id,
            interface,
            opcode,
        } => PlatformError::Other(format!(
            "{PROTOCOL_FAILED} while dispatching events: undecodable {interface} event \
             (opcode {opcode}) from {sender_id}"
        )),
    }
}

/// A compositor that stopped answering is reported the same way as one that went
/// away, and for the same reason: enumeration failure is already non-fatal
/// upstream, so the node simply publishes no screens until it recovers.
fn timed_out() -> PlatformError {
    PlatformError::Other(format!(
        "{ENUMERATION_TIMED_OUT} after {}ms",
        ENUMERATION_BUDGET.as_millis()
    ))
}

/// Whether an [`enumerate`] failure is one the next call is expected to recover
/// from on its own.
///
/// Positive recognition only: a losable connection and the deadline, matched
/// against the constants their constructors format from rather than against a
/// repeated literal. Everything else — protocol errors, undecodable messages,
/// a `poll` that failed — is unrecognised and therefore not self-healing. This is
/// not [`PlatformError::is_transient`], which drives the clipboard poller's retry
/// loop for every backend and is not widened for this.
#[cfg(test)]
fn is_self_healing(e: &PlatformError) -> bool {
    match e {
        PlatformError::Other(msg) => {
            msg.starts_with(CONNECTION_LOST) || msg.starts_with(ENUMERATION_TIMED_OUT)
        }
        _ => false,
    }
}

/// Wayland strings are never null but are routinely empty; an empty name is no
/// name, and treating it as one would produce a monitor called "".
fn non_empty(s: String) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for Enumerator {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        // `global_remove` is ignored on purpose: this connection lives for one
        // enumeration, so an output that vanishes mid-pass simply arrives without
        // geometry and is dropped by `to_monitors`.
        let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        else {
            return;
        };

        match interface.as_str() {
            "wl_output" => {
                // The udata is the index into `outputs`, which is how a later
                // event finds the entry it belongs to. It is stable because
                // entries are only ever appended.
                let index = state.outputs.len();
                let proxy = registry.bind::<wl_output::WlOutput, _, _>(
                    name,
                    version.min(WL_OUTPUT_VERSION),
                    qh,
                    index,
                );
                state.outputs.push(Entry {
                    proxy,
                    raw: RawOutput::default(),
                });
            }
            "zxdg_output_manager_v1" => {
                state.manager = Some(
                    registry.bind::<zxdg_output_manager_v1::ZxdgOutputManagerV1, _, _>(
                        name,
                        version.min(XDG_OUTPUT_MANAGER_VERSION),
                        qh,
                        (),
                    ),
                );
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, usize> for Enumerator {
    fn event(
        state: &mut Self,
        _proxy: &wl_output::WlOutput,
        event: wl_output::Event,
        index: &usize,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let Some(entry) = state.outputs.get_mut(*index) else {
            return;
        };

        match event {
            wl_output::Event::Geometry {
                make,
                model,
                transform,
                ..
            } => {
                entry.raw.make = non_empty(make);
                entry.raw.model = non_empty(model);
                entry.raw.transform_swaps_axes = matches!(
                    transform,
                    WEnum::Value(
                        wl_output::Transform::_90
                            | wl_output::Transform::_270
                            | wl_output::Transform::Flipped90
                            | wl_output::Transform::Flipped270
                    )
                );
            }
            wl_output::Event::Mode {
                flags,
                width,
                height,
                ..
            } => {
                // Every supported mode is advertised, not just the active one, so
                // taking the last would report whatever the panel can do rather
                // than what it is doing.
                if matches!(flags, WEnum::Value(f) if f.contains(wl_output::Mode::Current)) {
                    entry.raw.mode = Some((width, height));
                }
            }
            wl_output::Event::Scale { factor } => entry.raw.int_scale = factor,
            wl_output::Event::Name { name } => entry.raw.name = non_empty(name),
            wl_output::Event::Description { description } => {
                entry.raw.description = non_empty(description)
            }
            _ => {}
        }
    }
}

impl Dispatch<zxdg_output_v1::ZxdgOutputV1, usize> for Enumerator {
    fn event(
        state: &mut Self,
        _proxy: &zxdg_output_v1::ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        index: &usize,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let Some(entry) = state.outputs.get_mut(*index) else {
            return;
        };

        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => entry.raw.logical_pos = Some((x, y)),
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                entry.raw.logical_size = Some((width, height))
            }
            // Both deprecated in favour of the wl_output events of the same name
            // from version 4 onwards, so they only fill a gap.
            zxdg_output_v1::Event::Name { name } => entry.raw.xdg_name = non_empty(name),
            // The guard, not an `if` inside the arm: on a version 4 compositor the
            // wl_output description has already arrived and is the one to keep.
            zxdg_output_v1::Event::Description { description }
                if entry.raw.description.is_none() =>
            {
                entry.raw.description = non_empty(description);
            }
            _ => {}
        }
    }
}

/// The `wl_display.sync` barrier that ends a [`roundtrip`].
impl Dispatch<wl_callback::WlCallback, ()> for Enumerator {
    fn event(
        state: &mut Self,
        _proxy: &wl_callback::WlCallback,
        event: wl_callback::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if matches!(event, wl_callback::Event::Done { .. }) {
            state.synced = true;
        }
    }
}

/// The manager is a factory with no events of its own.
impl Dispatch<zxdg_output_manager_v1::ZxdgOutputManagerV1, ()> for Enumerator {
    fn event(
        _state: &mut Self,
        _proxy: &zxdg_output_manager_v1::ZxdgOutputManagerV1,
        _event: zxdg_output_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::DisplayEnumerator;
    use std::collections::HashSet;
    use wayland_client::backend::protocol::ProtocolError;
    use wayland_client::backend::ObjectId;
    use wx_proto::Monitor;

    #[test]
    fn a_lost_connection_is_self_healing_and_a_client_bug_is_not() {
        assert!(is_self_healing(&timed_out()));
        assert!(is_self_healing(&wayland_failed(
            "flushing the connection",
            WaylandError::Io(std::io::Error::from(ErrorKind::BrokenPipe)),
        )));

        // The distinction the skip in `session_monitors` rests on: these mean this
        // client asked for something wrong, and a live compositor is the only
        // place that shows up. Skipping them would let the assertions below pass
        // against a backend that enumerates nothing.
        let protocol = wayland_failed(
            "dispatching events",
            WaylandError::Protocol(ProtocolError {
                code: 0,
                object_id: 7,
                object_interface: "wl_output".into(),
                message: "invalid version".into(),
            }),
        );
        assert!(!is_self_healing(&protocol), "{protocol}");

        let bad_message = dispatch_failed(DispatchError::BadMessage {
            sender_id: ObjectId::null(),
            interface: "zxdg_output_v1",
            opcode: 3,
        });
        assert!(!is_self_healing(&bad_message), "{bad_message}");
    }

    /// The monitors of this session, or `None` when there is nothing to assert
    /// against.
    ///
    /// CI is headless, and a test that fails there would make the whole suite
    /// unrunnable on the one platform that builds this code. The skip has to cover
    /// everything [`enumerate`] itself calls acceptable, not just an absent
    /// compositor: a session with no outputs and a compositor with no
    /// `zxdg_output_manager_v1` are both documented non-failures, and a bare
    /// `gnome-shell --headless` with no `--virtual-monitor` — the harness AGENTS.md
    /// recommends — is exactly the first of them.
    ///
    /// The [`is_self_healing`] failures skip as well. The deadline exists to stop
    /// a wedged compositor freezing the engine loop; a compositor that hiccups
    /// past it under a parallel `cargo test` would otherwise turn that robustness
    /// bound into a flaky suite. Anything else — a protocol error above all, which
    /// would mean this client is the broken one — still panics, because a live
    /// compositor is the only place that bug is visible and skipping would let
    /// every assertion below pass on an empty vector.
    fn session_monitors() -> Option<Vec<Monitor>> {
        if Connection::connect_to_env().is_err() {
            eprintln!("skipped: no wayland session");
            return None;
        }
        match super::super::WaylandDisplays::new().monitors() {
            Ok(monitors) if monitors.is_empty() => {
                eprintln!("skipped: wayland session with no outputs");
                None
            }
            Ok(monitors) => Some(monitors),
            Err(e @ PlatformError::Unsupported { .. }) => {
                eprintln!("skipped: {e}");
                None
            }
            Err(e) if is_self_healing(&e) => {
                eprintln!("skipped: {e}");
                None
            }
            Err(e) => panic!("enumeration failed against a live compositor: {e}"),
        }
    }

    /// A further reading of a session [`session_monitors`] has already proved is
    /// there, with the same skip rule.
    ///
    /// Unwrapping instead would reintroduce the flake this skip exists to prevent:
    /// a compositor is no less free to hiccup on the second call than on the first.
    fn resample<T>(result: Result<T>) -> Option<T> {
        match result {
            Ok(v) => Some(v),
            Err(e) if is_self_healing(&e) => {
                eprintln!("skipped: {e}");
                None
            }
            Err(e) => panic!("enumeration failed against a live compositor: {e}"),
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
    fn ids_survive_a_reconnect() {
        // Each call is a whole new connection, so the registry object ids are
        // renumbered between these two. Ids derived from them would differ here,
        // and every saved layout would address the wrong screen after a restart.
        let Some(first) = session_monitors() else {
            return;
        };
        let Some(second) = resample(super::super::WaylandDisplays::new().monitors()) else {
            return;
        };

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
        let Some(bounds) = resample(super::super::WaylandDisplays::new().virtual_bounds()) else {
            return;
        };
        for m in monitors {
            assert!(bounds.x <= m.local_bounds.x);
            assert!(bounds.y <= m.local_bounds.y);
            assert!(bounds.right() >= m.local_bounds.right());
            assert!(bounds.bottom() >= m.local_bounds.bottom());
        }
    }
}
