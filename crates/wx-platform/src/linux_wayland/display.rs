//! Display enumeration on Wayland.
//!
//! There is no privileged "list all outputs" call for an ordinary Wayland client,
//! so the list is assembled from the registry: bind every `wl_output`, ask
//! `zxdg_output_manager_v1` for the matching `xdg_output`, and read the
//! `logical_position` / `logical_size` events. Those two give the compositor's own
//! layout in the same top-left-origin space [`Monitor`] uses, so no arithmetic is
//! needed to place a screen.
//!
//! # Why `xdg_output` is required rather than a nice-to-have
//!
//! `wl_output.scale` is an *integer*. On a 3840x2160 panel at 125% the compositor
//! reports a mode of 3840x2160 and a scale of 1, which multiplies out to a desktop
//! twice the size of the one the user can actually see. Fractional scaling is the
//! default on current Ubuntu, so deriving geometry from `wl_output` alone is wrong
//! on the common case, not the exotic one. `xdg_output`'s logical geometry is the
//! compositor's real answer, and a compositor that does not offer the manager is
//! reported as unsupported rather than guessed at.
//!
//! # Identity
//!
//! From `wl_output`'s `name` event (`DP-1`), hashed through
//! [`stable_monitor_id`]. The registry object id is per-connection: it renumbers
//! on every reconnect, so a saved layout would start addressing a different
//! physical screen with nothing to indicate why.
//!
//! # Connection lifetime
//!
//! [`DisplayEnumerator::monitors`] must be re-read rather than cached — the engine
//! calls it on a housekeeping tick to notice hotplug — and this backend answers
//! each call with a **short-lived connection**. See [`WaylandDisplays`].

use wx_proto::{Monitor, MonitorId, Rect};

use crate::coords::{deduplicate_ids, stable_monitor_id};
use crate::error::Result;
use crate::traits::DisplayEnumerator;

/// Highest `wl_output` version this client understands.
///
/// 4 is where `name` and `description` arrive. Lower is tolerated — the identity
/// falls back to `xdg_output`'s own `name` — but 4 is what every current
/// compositor offers and is the only version that makes identity cheap.
#[cfg(target_os = "linux")]
pub(super) const WL_OUTPUT_VERSION: u32 = 4;

/// Highest `zxdg_output_manager_v1` version this client understands.
#[cfg(target_os = "linux")]
pub(super) const XDG_OUTPUT_MANAGER_VERSION: u32 = 3;

/// One output as the protocol described it, before any interpretation.
///
/// Deliberately free of Wayland types so the awkward part — turning a mode, a
/// transform and a logical size into a scale factor — is testable on a machine
/// with no compositor at all, including the headless CI runner.
#[derive(Debug, Clone)]
pub(super) struct RawOutput {
    /// `wl_output.name`, e.g. `DP-1`. The identity source when present.
    pub name: Option<String>,
    /// `xdg_output.name`. The same connector name, from the interface that
    /// carried it before `wl_output` version 4 existed.
    pub xdg_name: Option<String>,
    /// Human-readable description, e.g. `LG Electronics 32"`.
    pub description: Option<String>,
    /// `wl_output.geometry` make and model, the last resort for identity.
    pub make: Option<String>,
    pub model: Option<String>,
    /// `xdg_output.logical_position`, in the compositor's layout space.
    pub logical_pos: Option<(i32, i32)>,
    /// `xdg_output.logical_size`, already rotated by the output transform.
    pub logical_size: Option<(i32, i32)>,
    /// The current `wl_output.mode`, in hardware pixels and *not* rotated.
    pub mode: Option<(i32, i32)>,
    /// Whether the output transform is a 90 or 270 degree rotation, which swaps
    /// the axes of `mode` but not of `logical_size`.
    pub transform_swaps_axes: bool,
    /// `wl_output.scale`: integer only, and therefore a fallback rather than the
    /// answer.
    pub int_scale: i32,
}

impl Default for RawOutput {
    fn default() -> Self {
        Self {
            name: None,
            xdg_name: None,
            description: None,
            make: None,
            model: None,
            logical_pos: None,
            logical_size: None,
            mode: None,
            transform_swaps_axes: false,
            // The protocol default when no `scale` event is sent, per wl_output.
            int_scale: 1,
        }
    }
}

/// Wayland display enumeration.
///
/// # Why a fresh connection per call rather than a long-lived one
///
/// A long-lived connection with a roundtrip per call is faster, but it has to own
/// an event queue behind a lock to stay `Send` under a `&self` method, re-bind
/// outputs on `global_remove`, and — the part that actually bites — notice that
/// the compositor restarted and rebuild itself. All of that is state whose only
/// job is to be invalidated, and getting it subtly wrong shows up as a monitor
/// that stays in the layout after it was unplugged, which is precisely the failure
/// [`DisplayEnumerator`] warns about.
///
/// A short-lived connection has none of that: every call starts from an empty
/// registry, so hotplug and compositor restarts are correct by construction, and
/// the "ids survive a reconnect" requirement is exercised on every single call
/// rather than only in a test. It costs a connect plus two roundtrips over a unix
/// socket — well under a millisecond, against a housekeeping tick measured in
/// seconds.
///
/// A unit struct also keeps `Send` trivially true.
///
/// # Liveness
///
/// Correctness is not the only thing at stake here: this method runs *inline on
/// the agent's engine loop*, the same single thread that forwards captured input
/// and answers peer heartbeats. Every socket wait therefore has a deadline
/// (`ENUMERATION_BUDGET` in `outputs`), because a compositor that stops servicing
/// its socket without closing it would otherwise block that loop indefinitely and
/// strand the cursor wherever it happens to be. On timeout enumeration fails, the
/// failure is logged and non-fatal, and the node publishes no screens until the
/// next tick finds a compositor that answers.
///
/// # Known limit: `WAYLAND_SOCKET`
///
/// `Connection::connect_to_env` *consumes* `WAYLAND_SOCKET`, unsetting it once it
/// has taken the fd. A process launched with `WAYLAND_SOCKET` set and
/// `WAYLAND_DISPLAY` unset therefore enumerates exactly once and reports
/// [`PlatformError::NoDisplayServer`] forever after, since each later call opens a
/// fresh connection and finds nothing to open it from. This is a known and
/// accepted consequence of the connection-per-call design, not an oversight: it is
/// unreachable from a normal desktop launch or a systemd user service, both of
/// which export `WAYLAND_DISPLAY`.
///
/// [`PlatformError::NoDisplayServer`]: crate::error::PlatformError::NoDisplayServer
pub struct WaylandDisplays;

impl WaylandDisplays {
    pub fn new() -> Self {
        Self
    }
}

impl Default for WaylandDisplays {
    fn default() -> Self {
        Self::new()
    }
}

impl DisplayEnumerator for WaylandDisplays {
    fn monitors(&self) -> Result<Vec<Monitor>> {
        Ok(to_monitors(enumerate_raw()?))
    }
}

#[cfg(target_os = "linux")]
fn enumerate_raw() -> Result<Vec<RawOutput>> {
    super::outputs::enumerate()
}

/// The backend modules compile on every target so the skeletons cannot rot, but
/// the protocol client only exists on Linux.
#[cfg(not(target_os = "linux"))]
fn enumerate_raw() -> Result<Vec<RawOutput>> {
    Err(crate::error::PlatformError::Unsupported {
        operation: "display enumeration",
        backend: super::BACKEND,
    })
}

/// Turn the protocol's view of the outputs into the layout's view.
///
/// Outputs with no logical geometry are dropped rather than guessed at: without
/// `xdg_output` there is no honest rectangle to publish, and inventing one from
/// the integer scale would place the screen wrongly under fractional scaling. An
/// output that cannot be named at all is dropped for the same reason — it could
/// only be given an unstable id, and a layout entry that moves to a different
/// physical screen on the next reconnect is worse than a missing one.
fn to_monitors(raw: Vec<RawOutput>) -> Vec<Monitor> {
    let usable: Vec<(RawOutput, String)> = raw
        .into_iter()
        .filter_map(|o| {
            let Some(identity) = identity_name(&o) else {
                tracing::debug!("wayland output with no name, make or model; skipping");
                return None;
            };
            if o.logical_pos.is_none() || o.logical_size.is_none() {
                tracing::debug!(output = %identity, "no xdg_output geometry; skipping");
                return None;
            }
            Some((o, identity))
        })
        .collect();

    let mut ids: Vec<u32> = usable
        .iter()
        .map(|(_, identity)| stable_monitor_id(identity))
        .collect();
    deduplicate_ids(&mut ids);

    // Wayland has no primary output — it is a desktop-environment concept the
    // protocol deliberately does not expose. The output at the layout origin is
    // the closest thing available and is what every compositor puts the panel and
    // the login greeter on; if nothing sits at the origin the first output takes
    // it, so exactly one monitor is ever marked.
    let primary = usable
        .iter()
        .position(|(o, _)| o.logical_pos == Some((0, 0)))
        .unwrap_or(0);

    usable
        .into_iter()
        .zip(ids)
        .enumerate()
        .map(|(index, ((o, identity), id))| Monitor {
            id: MonitorId(id),
            name: display_name(&o, &identity),
            local_bounds: logical_rect(&o),
            scale: effective_scale(&o),
            primary: index == primary,
        })
        .collect()
}

/// The stable identity string for an output.
///
/// `wl_output.name` first (`DP-1`), then `xdg_output.name` for compositors older
/// than `wl_output` version 4, then make and model, which `wl_output.geometry`
/// has carried since version 1. Make and model are weaker — two identical panels
/// collide, and [`deduplicate_ids`] then has to break the tie by position in the
/// list — but a weak id beats dropping the monitor.
fn identity_name(o: &RawOutput) -> Option<String> {
    if let Some(n) = o.name.as_ref().or(o.xdg_name.as_ref()) {
        return Some(n.clone());
    }
    match (o.make.as_deref(), o.model.as_deref()) {
        (Some(make), Some(model)) => Some(format!("{make} {model}")),
        (Some(s), None) | (None, Some(s)) => Some(s.to_string()),
        (None, None) => None,
    }
}

/// What the layout editor shows. Identity stays with [`identity_name`].
///
/// The description is the recognisable one (`LG Electronics 32"`), and matching
/// the Windows backend, which shows the marketing name rather than
/// `\\.\DISPLAY1`. The connector is appended because two identical panels share a
/// description and would otherwise be indistinguishable in the editor — which is
/// exactly when a user needs to tell them apart.
fn display_name(o: &RawOutput, identity: &str) -> String {
    match o.description.as_deref() {
        Some(d) if d != identity => format!("{d} ({identity})"),
        _ => identity.to_string(),
    }
}

/// The output's rectangle in the compositor's layout space.
///
/// Negative extents cannot occur in a well-behaved compositor but are clamped
/// rather than cast, so a bad value becomes an empty monitor that
/// [`crate::coords::union_bounds`] skips instead of a rectangle claiming four
/// billion pixels.
fn logical_rect(o: &RawOutput) -> Rect {
    let (x, y) = o.logical_pos.unwrap_or((0, 0));
    let (w, h) = o.logical_size.unwrap_or((0, 0));
    Rect::new(x, y, w.max(0) as u32, h.max(0) as u32)
}

/// The output's real scale factor, fractional included.
///
/// Hardware pixels divided by logical pixels. This is the whole reason
/// `xdg_output` is mandatory: at 125% the compositor reports `wl_output.scale`
/// of 1 and a 3840-wide mode, and only the 3072-wide logical size reveals the
/// 1.25. The mode is *not* rotated by the output transform while the logical size
/// is, so a portrait monitor needs its mode axes swapped first or the ratio comes
/// out as the aspect ratio instead of the scale.
///
/// Falls back to the integer scale rather than failing: the value is reported for
/// display only — wire positions are normalized, so routing never depends on it —
/// and a mixed-DPI desk should not lose a screen over it.
fn effective_scale(o: &RawOutput) -> f32 {
    let fallback = if o.int_scale >= 1 {
        o.int_scale as f32
    } else {
        1.0
    };

    let (Some((mode_w, mode_h)), Some((logical_w, _))) = (o.mode, o.logical_size) else {
        return fallback;
    };
    let hardware_w = if o.transform_swaps_axes {
        mode_h
    } else {
        mode_w
    };
    if hardware_w <= 0 || logical_w <= 0 {
        return fallback;
    }

    // Rounded to three places so a 1920/1536 that lands on 1.2499999 is shown as
    // 125% rather than 124.99999%.
    let scale = (hardware_w as f32 / logical_w as f32 * 1000.0).round() / 1000.0;
    if scale > 0.0 {
        scale
    } else {
        fallback
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(name: &str, pos: (i32, i32), logical: (i32, i32), mode: (i32, i32)) -> RawOutput {
        RawOutput {
            name: Some(name.to_string()),
            logical_pos: Some(pos),
            logical_size: Some(logical),
            mode: Some(mode),
            ..Default::default()
        }
    }

    #[test]
    fn fractional_scaling_comes_from_logical_size_not_the_integer_scale() {
        // 3840x2160 at 125% is the case that motivates the whole xdg_output
        // dependency: wl_output reports an integer scale of 1 here, and believing
        // it would publish a desktop 768 pixels wider than the user has.
        let mut o = output("DP-1", (0, 0), (3072, 1728), (3840, 2160));
        o.int_scale = 1;
        assert_eq!(effective_scale(&o), 1.25);
        assert_eq!(logical_rect(&o), Rect::new(0, 0, 3072, 1728));

        // 150% on the same panel.
        let mut o = output("DP-1", (0, 0), (2560, 1440), (3840, 2160));
        o.int_scale = 1;
        assert_eq!(effective_scale(&o), 1.5);
    }

    #[test]
    fn an_integer_scaled_output_still_reports_its_scale() {
        let mut o = output("eDP-1", (0, 0), (1920, 1080), (3840, 2160));
        o.int_scale = 2;
        assert_eq!(effective_scale(&o), 2.0);
    }

    #[test]
    fn a_rotated_output_reports_scale_rather_than_aspect_ratio() {
        // Portrait: the mode stays landscape, the logical size is rotated. Without
        // swapping the mode axes this would come out as 3840/1080 = 3.55.
        let mut o = output("DP-2", (0, 0), (1080, 1920), (3840, 2160));
        o.transform_swaps_axes = true;
        assert_eq!(effective_scale(&o), 2.0);
    }

    #[test]
    fn scale_falls_back_to_the_integer_when_no_mode_arrived() {
        let mut o = output("DP-1", (0, 0), (1920, 1080), (1920, 1080));
        o.mode = None;
        o.int_scale = 2;
        assert_eq!(effective_scale(&o), 2.0);

        // A compositor that never sent `scale` leaves the protocol default of 1.
        let mut o = output("DP-1", (0, 0), (1920, 1080), (1920, 1080));
        o.mode = None;
        assert_eq!(effective_scale(&o), 1.0);
    }

    #[test]
    fn a_nonsensical_mode_does_not_produce_a_nonsensical_scale() {
        let mut o = output("DP-1", (0, 0), (1920, 1080), (0, 0));
        o.int_scale = 1;
        assert_eq!(effective_scale(&o), 1.0);
    }

    #[test]
    fn identity_is_the_connector_name_not_the_enumeration_order() {
        let a = output("DP-1", (0, 0), (1920, 1080), (1920, 1080));
        let b = output("HDMI-A-1", (1920, 0), (1920, 1080), (1920, 1080));

        let forwards = to_monitors(vec![a.clone(), b.clone()]);
        let backwards = to_monitors(vec![b, a]);

        // Same physical screens, opposite registry order: the ids must not move.
        assert_eq!(forwards[0].id, backwards[1].id);
        assert_eq!(forwards[1].id, backwards[0].id);
    }

    #[test]
    fn identity_falls_back_through_xdg_name_then_make_and_model() {
        let mut o = RawOutput::default();
        assert_eq!(identity_name(&o), None);

        o.make = Some("GSM".into());
        o.model = Some("LG HDR 4K".into());
        assert_eq!(identity_name(&o).as_deref(), Some("GSM LG HDR 4K"));

        // A pre-version-4 compositor carries the connector on xdg_output instead.
        o.xdg_name = Some("DP-1".into());
        assert_eq!(identity_name(&o).as_deref(), Some("DP-1"));

        o.name = Some("DP-2".into());
        assert_eq!(identity_name(&o).as_deref(), Some("DP-2"));
    }

    #[test]
    fn an_output_without_logical_geometry_is_dropped_rather_than_guessed() {
        // Publishing a rectangle derived from the integer scale would put the
        // screen in the wrong place under fractional scaling, and the cursor would
        // cross an edge that is not there.
        let mut o = output("DP-1", (0, 0), (3072, 1728), (3840, 2160));
        o.logical_size = None;
        assert!(to_monitors(vec![o]).is_empty());
    }

    #[test]
    fn an_unnameable_output_is_dropped_rather_than_given_an_unstable_id() {
        let o = RawOutput {
            logical_pos: Some((0, 0)),
            logical_size: Some((1920, 1080)),
            ..Default::default()
        };
        assert!(to_monitors(vec![o]).is_empty());
    }

    #[test]
    fn two_identical_panels_still_get_distinct_ids() {
        // Same make and model, no connector name: the hash collides and the layout
        // would have one unaddressable screen if the collision were tolerated.
        let panel = RawOutput {
            make: Some("GSM".into()),
            model: Some("LG HDR 4K".into()),
            logical_pos: Some((0, 0)),
            logical_size: Some((1920, 1080)),
            ..Default::default()
        };
        let mut second = panel.clone();
        second.logical_pos = Some((1920, 0));

        let monitors = to_monitors(vec![panel, second]);
        assert_eq!(monitors.len(), 2);
        assert_ne!(monitors[0].id, monitors[1].id);
    }

    #[test]
    fn exactly_one_monitor_is_primary_and_it_is_the_one_at_the_origin() {
        let left = output("HDMI-A-1", (-1920, 0), (1920, 1080), (1920, 1080));
        let origin = output("DP-1", (0, 0), (2560, 1440), (2560, 1440));
        let monitors = to_monitors(vec![left, origin]);

        assert_eq!(monitors.iter().filter(|m| m.primary).count(), 1);
        assert!(monitors[1].primary, "the output at the origin is primary");
    }

    #[test]
    fn a_layout_with_nothing_at_the_origin_still_marks_one_primary() {
        // Possible while the compositor is mid-reconfiguration; leaving every
        // monitor non-primary would make `primary()` return None and the UI show
        // the machine with no anchor screen.
        let a = output("DP-1", (100, 100), (1920, 1080), (1920, 1080));
        let b = output("DP-2", (2020, 100), (1920, 1080), (1920, 1080));
        let monitors = to_monitors(vec![a, b]);
        assert_eq!(monitors.iter().filter(|m| m.primary).count(), 1);
    }

    #[test]
    fn no_outputs_is_an_empty_list_not_a_panic() {
        assert!(to_monitors(Vec::new()).is_empty());
    }

    #[test]
    fn the_display_name_disambiguates_identical_descriptions() {
        let mut o = output("DP-1", (0, 0), (1920, 1080), (1920, 1080));
        o.description = Some("LG Electronics 32\"".into());
        assert_eq!(display_name(&o, "DP-1"), "LG Electronics 32\" (DP-1)");

        // A compositor whose description is just the connector should not repeat
        // itself.
        o.description = Some("DP-1".into());
        assert_eq!(display_name(&o, "DP-1"), "DP-1");

        o.description = None;
        assert_eq!(display_name(&o, "DP-1"), "DP-1");
    }

    #[test]
    fn negative_extents_become_an_empty_rect_rather_than_an_enormous_one() {
        let mut o = output("DP-1", (0, 0), (1920, 1080), (1920, 1080));
        o.logical_size = Some((-1, -1));
        assert!(logical_rect(&o).is_empty());
    }
}
