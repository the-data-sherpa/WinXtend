//! Display enumeration on X11.
//!
//! RandR is the only source: the X screen's own `width_in_pixels` is the bounding
//! box of every monitor glued together, which says nothing about where one screen
//! ends and the next begins. `GetScreenResourcesCurrent` plus a `GetCrtcInfo` per
//! output gives each screen's rectangle in the X screen's coordinate space, which
//! is already the top-left-origin space [`Monitor`] wants.
//!
//! Everything the server said is captured in [`RawOutput`] first, so the
//! interpretation below — which outputs count, which one is primary, how ids are
//! derived — is testable on a machine with no X server at all, including the
//! headless CI runner. The protocol half is [`super::server`].
//!
//! # Identity
//!
//! The output *name* (`DP-2`, `eDP-1`), hashed through [`stable_monitor_id`].
//! RandR's own output ids are per-session and renumber on replug, so a saved
//! layout built on them would silently start addressing a different physical
//! screen.
//!
//! This is the same rule the Wayland backend follows, and the two agree: a hash of
//! `DP-1` is `DP-1` whichever protocol read the name, so one machine's screen keeps
//! its identity across a change of session type.
//!
//! # Scale is 1.0, and that is the honest answer
//!
//! X11 has no per-monitor DPI. `Xft.dpi` is one number for the whole display and
//! toolkits apply it however they like, so there is nothing to divide a *monitor's*
//! rectangle by. Reporting a guessed scale would be worse than reporting none:
//! wire positions are normalized against `local_bounds`, so routing never consults
//! the scale, and the only thing a wrong value changes is the size the layout
//! editor draws the screen at.
//!
//! The visible consequence, worth knowing before it is filed as a bug: the same
//! panel that a Wayland session reports as `3072x1728 scale=2.0` appears here as
//! `6144x3456 scale=1.0`. Both describe the same screen; only X11 declines to say
//! how much of it is chrome.

use wx_proto::{Monitor, MonitorId, Rect};

use crate::coords::{deduplicate_ids, stable_monitor_id};

/// One RandR output as the server described it, before any interpretation.
///
/// Deliberately free of X11 types so the rules below can be tested without a
/// display.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct RawOutput {
    /// `GetOutputInfo`'s name, e.g. `DP-2`. Empty when the server gave none.
    pub name: String,
    /// Whether RandR reports something plugged into this connector.
    ///
    /// `RR_Connection` has three values and only `Connected` is a yes;
    /// `Disconnected` and `Unknown` both arrive here as `false`. See
    /// [`to_monitors`] for why `Unknown` is not treated as a maybe.
    pub connected: bool,
    /// The CRTC's rectangle, or `None` when the output is not driving one.
    ///
    /// An output with no CRTC is connected but switched off — a screen the user
    /// disabled in display settings, or the inactive half of a mirrored pair
    /// mid-reconfiguration. It has no rectangle, so it has no place in the layout.
    pub bounds: Option<Rect>,
    /// Whether `GetOutputPrimary` named this output.
    pub primary: bool,
}

/// Turn the outputs RandR described into the screens the layout can use.
///
/// Three filters, and each one is about not publishing a rectangle the cursor
/// could be routed into and vanish:
///
/// * **Not connected.** `RR_Connection` also has `Unknown`, which VGA and some
///   DVI connectors report because they cannot sense a monitor. That is deliberately
///   *not* treated as connected: an unknown output that is genuinely in use has a
///   CRTC and a rectangle, and one that is not has neither, so the CRTC test below
///   already admits the case that matters without letting an empty connector into
///   the layout.
/// * **No CRTC.** A connected but disabled screen. RandR reports it, the user
///   cannot see it, and it has no coordinates.
/// * **No name.** The id would have to come from the per-session output number,
///   and a layout entry that moves to a different physical screen on the next
///   replug is worse than a missing one. Same rule as the Wayland backend.
///
/// A degenerate rectangle is dropped too: a CRTC caught mid-reconfiguration can
/// report zero width, and a monitor with no extent is one the cursor can enter and
/// never leave.
pub(super) fn to_monitors(raw: Vec<RawOutput>) -> Vec<Monitor> {
    let usable: Vec<RawOutput> = raw
        .into_iter()
        .filter(|o| {
            if o.name.is_empty() {
                tracing::debug!("randr output with no name; skipping");
                return false;
            }
            if !o.connected {
                tracing::debug!(output = %o.name, "randr output is not connected; skipping");
                return false;
            }
            match o.bounds {
                None => {
                    tracing::debug!(output = %o.name, "randr output drives no crtc; skipping");
                    false
                }
                Some(b) if b.is_empty() => {
                    tracing::debug!(output = %o.name, "randr output has no extent; skipping");
                    false
                }
                Some(_) => true,
            }
        })
        .collect();

    let mut ids: Vec<u32> = usable.iter().map(|o| stable_monitor_id(&o.name)).collect();
    deduplicate_ids(&mut ids);

    // Exactly one monitor is marked, whatever the server said. RandR's primary is
    // the answer when it names an output that survived the filters above, but it is
    // free to name a disabled one — or none at all, which is the default until
    // something sets it. Falling through to the screen at the origin and then to
    // the first keeps `DisplayEnumerator::primary` from returning `None` on a
    // machine that plainly has a screen, which would leave the layout editor with
    // no anchor to arrange around.
    let primary = usable
        .iter()
        .position(|o| o.primary)
        .or_else(|| {
            usable
                .iter()
                .position(|o| o.bounds.is_some_and(|b| b.x == 0 && b.y == 0))
        })
        .unwrap_or(0);

    usable
        .into_iter()
        .zip(ids)
        .enumerate()
        .map(|(index, (o, id))| Monitor {
            id: MonitorId(id),
            name: o.name,
            local_bounds: o.bounds.unwrap_or(Rect::new(0, 0, 0, 0)),
            // See the module docs: X11 has no per-monitor DPI, and a guess would
            // only ever be wrong in the layout editor.
            scale: 1.0,
            primary: index == primary,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(name: &str, bounds: Rect) -> RawOutput {
        RawOutput {
            name: name.to_string(),
            connected: true,
            bounds: Some(bounds),
            primary: false,
        }
    }

    #[test]
    fn a_connected_output_driving_a_crtc_becomes_a_monitor() {
        // The captain's workhorse, as `xrandr --query` reports it.
        let mut o = output("DP-2", Rect::new(0, 0, 3440, 1440));
        o.primary = true;
        let monitors = to_monitors(vec![o]);

        assert_eq!(monitors.len(), 1);
        assert_eq!(monitors[0].name, "DP-2");
        assert_eq!(monitors[0].local_bounds, Rect::new(0, 0, 3440, 1440));
        assert!(monitors[0].primary);
        assert_eq!(monitors[0].scale, 1.0);
    }

    #[test]
    fn a_disconnected_connector_is_not_a_screen() {
        // Every desktop has more connectors than monitors, and each spare one
        // would otherwise be a rectangle at the origin the cursor could be routed
        // into.
        let mut o = output("HDMI-1", Rect::new(0, 0, 1920, 1080));
        o.connected = false;
        assert!(to_monitors(vec![o]).is_empty());
    }

    #[test]
    fn a_connected_output_with_no_crtc_is_switched_off_and_has_no_place() {
        // What a screen the user disabled in display settings looks like: RandR
        // still reports the panel, it simply is not being driven.
        let mut o = output("DP-1", Rect::new(0, 0, 1920, 1080));
        o.bounds = None;
        assert!(to_monitors(vec![o]).is_empty());
    }

    #[test]
    fn a_crtc_caught_mid_reconfiguration_is_dropped_rather_than_published() {
        // A zero-extent monitor is one the cursor can enter and never leave.
        let o = output("DP-1", Rect::new(0, 0, 0, 0));
        assert!(to_monitors(vec![o]).is_empty());
    }

    #[test]
    fn an_unnameable_output_is_dropped_rather_than_given_an_unstable_id() {
        let mut o = output("", Rect::new(0, 0, 1920, 1080));
        o.primary = true;
        assert!(to_monitors(vec![o]).is_empty());
    }

    #[test]
    fn identity_is_the_connector_name_not_the_enumeration_order() {
        let a = output("DP-1", Rect::new(0, 0, 1920, 1080));
        let b = output("HDMI-A-1", Rect::new(1920, 0, 1920, 1080));

        let forwards = to_monitors(vec![a.clone(), b.clone()]);
        let backwards = to_monitors(vec![b, a]);

        assert_eq!(forwards[0].id, backwards[1].id);
        assert_eq!(forwards[1].id, backwards[0].id);
    }

    #[test]
    fn the_id_matches_what_the_wayland_backend_derives_for_the_same_screen() {
        // Both backends hash the connector name, so one machine keeps its screens'
        // identity across a change of session type — and a peer that has seen it on
        // Wayland recognises it on X11. Pinned against `stable_monitor_id` directly
        // rather than against the other backend, which needs a compositor to run.
        let monitors = to_monitors(vec![output("DP-1", Rect::new(0, 0, 1920, 1080))]);
        assert_eq!(monitors[0].id, MonitorId(stable_monitor_id("DP-1")));
    }

    #[test]
    fn exactly_one_monitor_is_primary_and_it_is_the_one_randr_named() {
        let left = output("HDMI-1", Rect::new(-1920, 0, 1920, 1080));
        let mut right = output("DP-1", Rect::new(0, 0, 2560, 1440));
        right.primary = true;

        let monitors = to_monitors(vec![left, right]);
        assert_eq!(monitors.iter().filter(|m| m.primary).count(), 1);
        assert!(monitors[1].primary);
    }

    #[test]
    fn a_primary_the_server_named_but_disabled_does_not_leave_the_layout_anchorless() {
        // RandR will happily keep naming an output that has since been switched
        // off. Marking nothing then makes `primary()` return `None` on a machine
        // with a screen right in front of it.
        let mut disabled = output("DP-9", Rect::new(0, 0, 1920, 1080));
        disabled.primary = true;
        disabled.bounds = None;
        let live = output("DP-1", Rect::new(0, 0, 2560, 1440));

        let monitors = to_monitors(vec![disabled, live]);
        assert_eq!(monitors.len(), 1);
        assert!(monitors[0].primary);
    }

    #[test]
    fn with_no_primary_set_the_screen_at_the_origin_takes_it() {
        // `xrandr --primary` has never been run on this machine, which is the
        // default state of a fresh X session.
        let left = output("HDMI-1", Rect::new(-1920, 0, 1920, 1080));
        let origin = output("DP-1", Rect::new(0, 0, 2560, 1440));

        let monitors = to_monitors(vec![left, origin]);
        assert_eq!(monitors.iter().filter(|m| m.primary).count(), 1);
        assert!(monitors[1].primary);
    }

    #[test]
    fn a_layout_with_nothing_at_the_origin_still_marks_one_primary() {
        let a = output("DP-1", Rect::new(100, 100, 1920, 1080));
        let b = output("DP-2", Rect::new(2020, 100, 1920, 1080));
        let monitors = to_monitors(vec![a, b]);
        assert_eq!(monitors.iter().filter(|m| m.primary).count(), 1);
    }

    #[test]
    fn two_outputs_mirroring_one_crtc_still_get_distinct_ids() {
        // `xrandr --output DP-1 --same-as HDMI-1` puts two connectors on one
        // rectangle. Both are real screens someone is looking at, so both are
        // published — but two monitors sharing an id would make one of them
        // unaddressable.
        let a = output("DP-1", Rect::new(0, 0, 1920, 1080));
        let b = output("HDMI-1", Rect::new(0, 0, 1920, 1080));

        let monitors = to_monitors(vec![a, b]);
        assert_eq!(monitors.len(), 2);
        assert_ne!(monitors[0].id, monitors[1].id);
        assert_eq!(monitors.iter().filter(|m| m.primary).count(), 1);
    }

    #[test]
    fn no_outputs_is_an_empty_list_not_a_panic() {
        assert!(to_monitors(Vec::new()).is_empty());
    }

    #[test]
    fn monitors_left_of_the_origin_keep_their_negative_coordinates() {
        // X11 screens can sit at negative offsets and the layout has to see them
        // where they are, not clamped to the origin.
        let monitors = to_monitors(vec![output("HDMI-1", Rect::new(-1920, -200, 1920, 1080))]);
        assert_eq!(monitors[0].local_bounds, Rect::new(-1920, -200, 1920, 1080));
    }
}
