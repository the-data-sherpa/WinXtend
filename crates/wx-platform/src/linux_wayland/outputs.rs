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

use wayland_client::protocol::{wl_output, wl_registry};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};
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

/// One connection's worth of enumeration state.
struct Enumerator {
    manager: Option<zxdg_output_manager_v1::ZxdgOutputManagerV1>,
    outputs: Vec<Entry>,
}

struct Entry {
    proxy: wl_output::WlOutput,
    raw: RawOutput,
}

/// Read the current outputs from the compositor over a fresh connection.
///
/// See [`super::display::WaylandDisplays`] for why the connection is not kept.
pub(super) fn enumerate() -> Result<Vec<RawOutput>> {
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
    };

    // First pass: learn the globals, and bind every output as it is advertised.
    queue.roundtrip(&mut state).map_err(roundtrip_failed)?;

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
        queue.roundtrip(&mut state).map_err(roundtrip_failed)?;
        if state.outputs.iter().all(|e| e.raw.logical_size.is_some()) {
            break;
        }
    }

    Ok(state.outputs.into_iter().map(|e| e.raw).collect())
}

/// A dead connection is reported rather than papered over: the compositor
/// restarting is exactly the moment the layout must stop trusting its rectangles.
/// The next call opens a new connection, so this is self-healing.
fn roundtrip_failed(e: wayland_client::DispatchError) -> PlatformError {
    PlatformError::Other(format!("wayland roundtrip failed: {e}"))
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

    /// Whether this machine has a compositor to talk to.
    ///
    /// CI is headless, and a test that fails there would make the whole suite
    /// unrunnable on the one platform that builds this code. Skipping is the only
    /// honest option: there is nothing to assert about a machine with no
    /// compositor.
    fn wayland_session() -> bool {
        Connection::connect_to_env().is_ok()
    }

    #[test]
    fn enumerating_this_session_yields_consistent_monitors() {
        if !wayland_session() {
            eprintln!("skipped: no wayland session");
            return;
        }
        let monitors = super::super::WaylandDisplays::new().monitors().unwrap();
        assert!(
            !monitors.is_empty(),
            "a wayland session with no outputs at all"
        );
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
        if !wayland_session() {
            eprintln!("skipped: no wayland session");
            return;
        }
        // Each call is a whole new connection, so the registry object ids are
        // renumbered between these two. Ids derived from them would differ here,
        // and every saved layout would address the wrong screen after a restart.
        let displays = super::super::WaylandDisplays::new();
        let first = displays.monitors().unwrap();
        let second = displays.monitors().unwrap();

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
        if !wayland_session() {
            eprintln!("skipped: no wayland session");
            return;
        }
        let displays = super::super::WaylandDisplays::new();
        let bounds = displays.virtual_bounds().unwrap();
        for m in displays.monitors().unwrap() {
            assert!(bounds.x <= m.local_bounds.x);
            assert!(bounds.y <= m.local_bounds.y);
            assert!(bounds.right() >= m.local_bounds.right());
            assert!(bounds.bottom() >= m.local_bounds.bottom());
        }
    }
}
