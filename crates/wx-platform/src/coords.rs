//! Pure coordinate and identity maths shared by the backends.
//!
//! Deliberately free of `cfg` gates and syscalls so that it is testable on any
//! host. Every backend that has to turn a [`wx_proto::NormPos`] into something an
//! OS accepts goes through here, which keeps the off-by-one arguments in one
//! place instead of once per platform.

use wx_proto::{Monitor, NormPos, Point, Rect};

/// Smallest rectangle containing all of `rects`; empty if there are none.
///
/// Accumulates in `i64` because `Rect::right()` is `i32 + u32` and a hostile or
/// corrupted monitor list could otherwise overflow into a rectangle that claims
/// the whole plane.
pub fn union_bounds(rects: impl IntoIterator<Item = Rect>) -> Rect {
    let mut acc: Option<(i64, i64, i64, i64)> = None;
    for r in rects {
        if r.is_empty() {
            continue;
        }
        let (l, t) = (r.x as i64, r.y as i64);
        let (right, bottom) = (l + r.w as i64, t + r.h as i64);
        acc = Some(match acc {
            None => (l, t, right, bottom),
            Some((al, at, ar, ab)) => (al.min(l), at.min(t), ar.max(right), ab.max(bottom)),
        });
    }
    match acc {
        None => Rect::new(0, 0, 0, 0),
        Some((l, t, r, b)) => Rect::new(
            l.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
            t.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
            (r - l).clamp(0, u32::MAX as i64) as u32,
            (b - t).clamp(0, u32::MAX as i64) as u32,
        ),
    }
}

/// Full extent of the 16-bit absolute pointer grid Windows uses.
const ABS_MAX: i64 = 65535;

/// Map a desktop pixel position onto the 16-bit absolute grid that
/// `SendInput`/`MOUSEEVENTF_ABSOLUTE` expects, relative to `virtual_bounds`.
///
/// The scale divisor is `extent - 1`, not `extent`. Dividing by the full extent
/// makes the maximum reachable value fall one pixel short of the right and bottom
/// edges — and those edges are precisely where edge crossing is detected, so the
/// cursor would be unable to leave the screen in two of four directions. The
/// symptom looks like "moving right works, moving left doesn't", which is a
/// miserable bug to chase.
///
/// Positions outside `virtual_bounds` clamp rather than wrap: a peer sending a
/// position for a monitor that has just been unplugged should park the cursor at
/// the edge, not at a negative coordinate the OS reinterprets as huge.
pub fn to_absolute_grid(p: Point, virtual_bounds: Rect) -> (i32, i32) {
    let map = |v: f64, origin: i32, extent: u32| -> i32 {
        // A single-pixel (or zero-width) desktop has no range to scale across;
        // everything is the origin of the grid.
        if extent <= 1 {
            return 0;
        }
        // NaN has no meaningful position and would survive `clamp` unchanged, so it
        // is pinned to the origin. Infinities need no special case: they clamp to
        // the grid's ends, which is the sensible reading of "infinitely far right".
        if v.is_nan() {
            return 0;
        }
        let offset = v - origin as f64;
        let scaled = (offset * ABS_MAX as f64 / (extent as f64 - 1.0)).round();
        scaled.clamp(0.0, ABS_MAX as f64) as i32
    };
    (
        map(p.x, virtual_bounds.x, virtual_bounds.w),
        map(p.y, virtual_bounds.y, virtual_bounds.h),
    )
}

/// Turn a wire position addressed to `monitor` into a local desktop pixel point.
///
/// Sanitizes first: a `NaN` normalized position off the wire would otherwise
/// propagate into the OS call and wedge the pointer somewhere unrecoverable.
pub fn local_point(monitor: &Monitor, pos: NormPos) -> Point {
    let bounds = monitor.local_bounds;
    bounds.clamp(bounds.denormalize(pos.sanitized()))
}

/// A [`wx_proto::MonitorId`] derived from the OS's own name for a display.
///
/// Not the enumeration index. Indices renumber when a monitor is unplugged, so a
/// saved layout would silently start addressing a different physical screen —
/// input arriving on the wrong monitor with no error anywhere. Hashing the OS
/// device name keeps the id stable across replugs and reboots as long as the OS
/// name is stable.
///
/// FNV-1a: not cryptographic, but this only needs to spread a handful of short
/// strings, and it is stable across builds and architectures, which a
/// `DefaultHasher` explicitly is not.
pub fn stable_monitor_id(os_name: &str) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in os_name.as_bytes() {
        hash ^= *byte as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// Force distinct ids onto monitors whose names collided.
///
/// Two displays sharing an id would make the layout unaddressable, so a collision
/// is nudged rather than tolerated. Called by every display backend after hashing.
pub fn deduplicate_ids(ids: &mut [u32]) {
    for i in 0..ids.len() {
        // Bounded probe: with a handful of monitors this terminates immediately,
        // and the bound stops a pathological set from spinning.
        for _ in 0..ids.len() {
            if ids[..i].contains(&ids[i]) {
                ids[i] = ids[i].wrapping_add(1);
            } else {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wx_proto::MonitorId;

    fn mon(bounds: Rect) -> Monitor {
        Monitor {
            id: MonitorId(0),
            name: "test".into(),
            local_bounds: bounds,
            scale: 1.0,
            primary: true,
        }
    }

    #[test]
    fn union_of_no_monitors_is_empty() {
        assert!(union_bounds(std::iter::empty()).is_empty());
    }

    #[test]
    fn union_spans_monitors_left_and_above_the_origin() {
        let u = union_bounds([
            Rect::new(0, 0, 1920, 1080),
            Rect::new(-1280, -720, 1280, 720),
        ]);
        assert_eq!(u, Rect::new(-1280, -720, 3200, 1800));
    }

    #[test]
    fn union_ignores_degenerate_monitors() {
        // A display being torn down can momentarily report a zero-size rect;
        // letting it into the union would anchor the desktop at its origin.
        let u = union_bounds([Rect::new(100, 100, 800, 600), Rect::new(-9999, -9999, 0, 0)]);
        assert_eq!(u, Rect::new(100, 100, 800, 600));
    }

    #[test]
    fn union_does_not_overflow_on_extreme_rects() {
        let u = union_bounds([
            Rect::new(i32::MIN, i32::MIN, u32::MAX, u32::MAX),
            Rect::new(i32::MAX - 1, i32::MAX - 1, 2, 2),
        ]);
        // The exact numbers do not matter; not panicking and staying inside the
        // representable range does.
        assert!(u.w > 0 && u.h > 0);
    }

    #[test]
    fn absolute_grid_reaches_both_extremes() {
        let v = Rect::new(0, 0, 1920, 1080);
        assert_eq!(to_absolute_grid(Point::new(0.0, 0.0), v), (0, 0));
        // The last pixel must map to the very end of the grid, or the cursor can
        // never touch the right or bottom edge and crossing breaks in two
        // directions.
        assert_eq!(
            to_absolute_grid(Point::new(1919.0, 1079.0), v),
            (65535, 65535)
        );
    }

    #[test]
    fn absolute_grid_is_relative_to_the_virtual_desktop_origin() {
        // Secondary monitor to the left of primary: the desktop origin is negative.
        let v = Rect::new(-1920, 0, 3840, 1080);
        let (x, _) = to_absolute_grid(Point::new(-1920.0, 0.0), v);
        assert_eq!(x, 0);
        let (mid, _) = to_absolute_grid(Point::new(0.0, 0.0), v);
        assert!((mid as i64 - 32767).abs() <= 20, "midpoint mapped to {mid}");
    }

    #[test]
    fn absolute_grid_clamps_positions_outside_the_desktop() {
        let v = Rect::new(0, 0, 1920, 1080);
        assert_eq!(to_absolute_grid(Point::new(-500.0, 99999.0), v), (0, 65535));
    }

    #[test]
    fn absolute_grid_survives_a_degenerate_desktop() {
        // Zero-size desktop happens between a last-monitor unplug and shutdown.
        assert_eq!(
            to_absolute_grid(Point::new(5.0, 5.0), Rect::new(0, 0, 0, 0)),
            (0, 0)
        );
        assert_eq!(
            to_absolute_grid(Point::new(5.0, 5.0), Rect::new(0, 0, 1, 1)),
            (0, 0)
        );
    }

    #[test]
    fn absolute_grid_rejects_non_finite_input() {
        let v = Rect::new(0, 0, 1920, 1080);
        // NaN is pinned to the origin; an infinity is simply very far away and
        // clamps to the edge. Neither may escape the grid.
        assert_eq!(
            to_absolute_grid(Point::new(f64::NAN, f64::INFINITY), v),
            (0, 65535)
        );
        assert_eq!(
            to_absolute_grid(Point::new(f64::NEG_INFINITY, f64::NAN), v),
            (0, 0)
        );
    }

    #[test]
    fn local_point_lands_inside_the_target_monitor() {
        let m = mon(Rect::new(1920, -200, 2560, 1440));
        for n in [
            NormPos::new(0.0, 0.0),
            NormPos::new(1.0, 1.0),
            NormPos::new(0.5, 0.5),
        ] {
            let p = local_point(&m, n);
            assert!(m.local_bounds.contains(p), "{n:?} landed at {p:?}");
        }
    }

    #[test]
    fn local_point_scrubs_hostile_wire_values() {
        let m = mon(Rect::new(0, 0, 1920, 1080));
        let p = local_point(&m, NormPos::new(f64::NAN, 12.0));
        assert!(p.x.is_finite() && p.y.is_finite());
        assert!(m.local_bounds.contains(p));
    }

    #[test]
    fn monitor_ids_are_stable_across_runs() {
        // Hard-coded expectation on purpose: if the hash changes, every saved
        // layout in the wild starts addressing the wrong screen.
        assert_eq!(stable_monitor_id(""), 0x811c_9dc5);
        assert_eq!(
            stable_monitor_id("\\\\.\\DISPLAY1"),
            stable_monitor_id("\\\\.\\DISPLAY1")
        );
    }

    #[test]
    fn different_displays_get_different_ids() {
        assert_ne!(
            stable_monitor_id("\\\\.\\DISPLAY1"),
            stable_monitor_id("\\\\.\\DISPLAY2")
        );
    }

    #[test]
    fn colliding_ids_are_forced_apart() {
        let mut ids = [7, 7, 7];
        deduplicate_ids(&mut ids);
        assert_eq!(ids, [7, 8, 9]);
    }

    #[test]
    fn deduplicate_leaves_distinct_ids_alone() {
        let mut ids = [3, 100, 42];
        deduplicate_ids(&mut ids);
        assert_eq!(ids, [3, 100, 42]);
    }
}
