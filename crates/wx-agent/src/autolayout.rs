//! First-pass layout, so a freshly paired mesh works before anyone opens the
//! editor.
//!
//! The layout editor is the right way to arrange screens, but requiring it before
//! anything works would make pairing feel broken: two machines that have just
//! agreed to trust each other, with no placements, share a cursor that cannot
//! reach either of them. So the moment a peer's monitor list is known, its
//! screens are dropped into the global space to the right of everything already
//! there, vertically centred.
//!
//! # Rules, and why each one
//!
//! * **A monitor keeps its own pixel dimensions.** [`wx_proto::Placement`]
//!   permits rescaling, but a guess that also changes cursor speed is two
//!   surprises instead of one.
//! * **A node's internal arrangement is preserved.** The OS already told us how
//!   that machine's own screens sit relative to each other, and the user arranged
//!   them; re-deriving it would be worse than copying it. Only the whole block
//!   moves.
//! * **Blocks abut with no gap.** The layout engine crosses gaps happily, but an
//!   abutting seam is the one arrangement where the cursor's height is preserved
//!   exactly as it crosses.
//! * **Vertically centred.** Aligning tops would put a 1080p laptop's screen
//!   opposite the top third of a 1440p monitor, so the natural sideways move at
//!   eye level would miss it entirely and the cursor would appear to stick.
//! * **Degenerate monitors are dropped.** A zero-extent rectangle cannot hold a
//!   cursor, and placing one only creates a screen the cursor can enter and never
//!   leave.
//!
//! Everything here is pure: no I/O, no clock, no platform. It is the part of the
//! agent most likely to be wrong in a way that only shows up as "the cursor won't
//! come back", so it is also the part that is exhaustively unit-tested.

use wx_core::layout::GlobalLayout;
use wx_proto::{GlobalMonitorId, Monitor, NodeId, Placement, Rect};

/// Where a node's block of monitors is put when the mesh is otherwise empty.
///
/// The origin rather than the node's own local coordinates: a machine whose
/// primary display sits at a large negative offset would otherwise place the
/// whole mesh far from the origin, which makes every rectangle in the UI's canvas
/// harder to read for no gain.
const ORIGIN: (i32, i32) = (0, 0);

/// Placements for one node's monitors, to the right of everything else.
///
/// Pure: `layout` is not modified, and any placements it already holds for `node`
/// are ignored when measuring where "the right of everything" is — otherwise
/// re-running this after a monitor is plugged in would march the node further
/// right on every attempt.
///
/// Returns an empty vector when the node has no usable monitors, which is the
/// correct answer for a headless node: it participates by forwarding input and
/// owns no part of the global space.
pub fn place_node(layout: &GlobalLayout, node: NodeId, monitors: &[Monitor]) -> Vec<Placement> {
    let usable: Vec<&Monitor> = monitors
        .iter()
        .filter(|m| !m.local_bounds.is_empty())
        .collect();
    if usable.is_empty() {
        return Vec::new();
    }

    let block = union_of_or_origin(usable.iter().map(|m| m.local_bounds));
    let others = union_of(
        layout
            .iter()
            .filter(|p| p.monitor.node != node && !p.global_bounds.is_empty())
            .map(|p| p.global_bounds),
    );

    // Anchor: hard left of the mesh's right edge, centred on its vertical middle.
    // With no mesh yet, the origin.
    //
    // All of this arithmetic is in `i64`. A `Rect` is an `i32` origin plus a `u32`
    // extent, so `right()` alone can overflow, and after the clamp in `union_of` the
    // mesh box may legitimately be `u32::MAX` wide. In `i32` a debug build panics
    // here and takes the whole engine task with it; a release build wraps and
    // anchors the node at a garbage coordinate.
    let (anchor_x, anchor_y) = match others {
        Some(rect) => {
            let centre_y = rect.y as i64 + rect.h as i64 / 2;
            (rect.x as i64 + rect.w as i64, centre_y - block.h as i64 / 2)
        }
        None => (ORIGIN.0 as i64, ORIGIN.1 as i64),
    };

    // Translation from the node's own desktop space into the global one. Applied
    // per monitor so the relative arrangement survives verbatim.
    let dx = anchor_x - block.x as i64;
    let dy = anchor_y - block.y as i64;

    usable
        .iter()
        .map(|m| Placement {
            monitor: GlobalMonitorId::new(node, m.id),
            global_bounds: Rect::new(
                clamp_to_i32(m.local_bounds.x as i64 + dx),
                clamp_to_i32(m.local_bounds.y as i64 + dy),
                m.local_bounds.w,
                m.local_bounds.h,
            ),
            cursor_scale: 1.0,
        })
        .collect()
}

/// Place a node's monitors, replacing whatever it had before.
///
/// Returns `true` if the layout changed, so the caller can decide whether to bump
/// the revision, persist, and push the result to peers. Reporting no change
/// matters: a peer re-announcing an unchanged monitor list on every reconnect
/// would otherwise generate a layout revision each time, and a mesh of three
/// machines would push layouts at each other indefinitely.
pub fn apply(layout: &mut GlobalLayout, node: NodeId, monitors: &[Monitor]) -> bool {
    let wanted = place_node(layout, node, monitors);
    let existing: Vec<Placement> = layout.monitors_of(node).copied().collect();

    if same_placements(&existing, &wanted) {
        return false;
    }

    for old in &existing {
        layout.remove(old.monitor);
    }
    for p in &wanted {
        layout.insert(*p);
    }
    layout.set_revision(layout.revision() + 1);
    true
}

/// Drop every placement belonging to a node.
///
/// Used when a peer is unpaired or blocked: leaving its screens in the layout
/// would let the cursor walk onto a machine that will refuse every frame, and the
/// user would have to guess why the pointer vanished.
pub fn forget_node(layout: &mut GlobalLayout, node: NodeId) -> bool {
    let doomed: Vec<GlobalMonitorId> = layout.monitors_of(node).map(|p| p.monitor).collect();
    if doomed.is_empty() {
        return false;
    }
    for id in doomed {
        layout.remove(id);
    }
    layout.set_revision(layout.revision() + 1);
    true
}

/// The starting layout for a machine that has never been configured: its own
/// screens, in their own arrangement, at the origin.
pub fn bootstrap(node: NodeId, monitors: &[Monitor]) -> GlobalLayout {
    let mut layout = GlobalLayout::new();
    apply(&mut layout, node, monitors);
    layout
}

/// Whether two placement sets describe the same arrangement.
///
/// Order-insensitive because neither the OS's monitor enumeration order nor the
/// layout's insertion order is stable across a reconnect, and treating a
/// reordering as a change would produce endless revisions.
fn same_placements(a: &[Placement], b: &[Placement]) -> bool {
    a.len() == b.len()
        && a.iter().all(|x| {
            b.iter().any(|y| {
                x.monitor == y.monitor
                    && x.global_bounds == y.global_bounds
                    && x.cursor_scale.to_bits() == y.cursor_scale.to_bits()
            })
        })
}

/// Smallest rectangle containing all of `rects`, or `None` when there are none.
///
/// Accumulated in `i64` and clamped on the way out, exactly as
/// `wx_platform::coords::union_bounds` does for the same shape of data. The failure
/// mode when this was `i32`: a layout is not a trusted input. A paired peer can send
/// `ControlMsg::LayoutUpdate` with placements at x = -2_000_000_000 and
/// x = +2_000_000_000, and the layout editor's own clamp permits typed coordinates
/// over the same range, so the span between two screens can exceed `i32::MAX` with
/// no hostile peer involved at all. `max_x - min_x` then panicked with "attempt to
/// subtract with overflow" (overflow checks are on in the dev and test profiles),
/// killing the engine task from inside `on_control` — so a single bad layout update
/// took the agent's whole run loop down. In release it silently wrapped to a
/// nonsense width and anchored the next node somewhere unreachable.
///
/// Clamping rather than rejecting: a mesh that wide is already unusable, but the
/// layout validator can say so, whereas a dead engine cannot say anything.
fn union_of(rects: impl IntoIterator<Item = Rect>) -> Option<Rect> {
    let mut acc: Option<(i64, i64, i64, i64)> = None;
    for r in rects.into_iter().filter(|r| !r.is_empty()) {
        let (left, top) = (r.x as i64, r.y as i64);
        let (right, bottom) = (left + r.w as i64, top + r.h as i64);
        acc = Some(match acc {
            None => (left, top, right, bottom),
            Some((l, t, rr, bb)) => (l.min(left), t.min(top), rr.max(right), bb.max(bottom)),
        });
    }
    let (left, top, right, bottom) = acc?;
    Some(Rect::new(
        clamp_to_i32(left),
        clamp_to_i32(top),
        (right - left).clamp(0, u32::MAX as i64) as u32,
        (bottom - top).clamp(0, u32::MAX as i64) as u32,
    ))
}

/// Saturate a widened coordinate back into the range a [`Rect`] can hold.
fn clamp_to_i32(v: i64) -> i32 {
    v.clamp(i32::MIN as i64, i32::MAX as i64) as i32
}

/// Bounding box of a set of monitors, or a degenerate rectangle at the origin.
///
/// Only reached with a non-empty, non-degenerate set, but returning a rectangle
/// rather than an `Option` keeps [`place_node`] free of a second unwrap on a
/// case it has already excluded.
fn union_of_or_origin(rects: impl IntoIterator<Item = Rect>) -> Rect {
    union_of(rects).unwrap_or(Rect::new(0, 0, 0, 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wx_core::layout::LayoutProblem;
    use wx_proto::MonitorId;

    fn node(n: u8) -> NodeId {
        NodeId([n; 32])
    }

    fn mon(id: u32, x: i32, y: i32, w: u32, h: u32) -> Monitor {
        Monitor {
            id: MonitorId(id),
            name: format!("DP-{id}"),
            local_bounds: Rect::new(x, y, w, h),
            scale: 1.0,
            primary: id == 0,
        }
    }

    fn hd(id: u32) -> Monitor {
        mon(id, 0, 0, 1920, 1080)
    }

    fn rect_of(layout: &GlobalLayout, n: u8, m: u32) -> Rect {
        layout
            .rect(GlobalMonitorId::new(node(n), MonitorId(m)))
            .expect("monitor is not in the layout")
    }

    #[test]
    fn the_first_node_lands_at_the_origin() {
        let layout = bootstrap(node(1), &[hd(0)]);
        assert_eq!(rect_of(&layout, 1, 0), Rect::new(0, 0, 1920, 1080));
    }

    #[test]
    fn a_first_node_at_a_negative_local_offset_is_normalised_to_the_origin() {
        // Windows puts a secondary display to the left of the primary at a
        // negative x. Copying that verbatim would place the whole mesh off in
        // negative space for no benefit.
        let layout = bootstrap(node(1), &[mon(0, -1920, -300, 1920, 1080)]);
        assert_eq!(rect_of(&layout, 1, 0), Rect::new(0, 0, 1920, 1080));
    }

    #[test]
    fn a_new_peer_lands_to_the_right_of_everything_existing() {
        let mut layout = bootstrap(node(1), &[hd(0)]);
        apply(&mut layout, node(2), &[hd(0)]);
        assert_eq!(rect_of(&layout, 2, 0), Rect::new(1920, 0, 1920, 1080));
    }

    #[test]
    fn a_third_peer_goes_beyond_the_second() {
        let mut layout = bootstrap(node(1), &[hd(0)]);
        apply(&mut layout, node(2), &[hd(0)]);
        apply(&mut layout, node(3), &[hd(0)]);
        assert_eq!(rect_of(&layout, 3, 0), Rect::new(3840, 0, 1920, 1080));
    }

    #[test]
    fn a_shorter_screen_is_centred_against_a_taller_one() {
        // Aligning tops would leave a sideways move at eye level missing the
        // laptop screen entirely, which reads as the cursor refusing to cross.
        let mut layout = bootstrap(node(1), &[mon(0, 0, 0, 2560, 1440)]);
        apply(&mut layout, node(2), &[mon(0, 0, 0, 1920, 1080)]);
        let placed = rect_of(&layout, 2, 0);
        assert_eq!(placed, Rect::new(2560, 180, 1920, 1080));
        // Both vertical centres coincide.
        assert_eq!(placed.y + placed.h as i32 / 2, 720);
    }

    #[test]
    fn a_taller_new_screen_is_allowed_to_sit_above_the_existing_mesh() {
        // Centring a 1440p panel against a 1080p one puts it at a negative y.
        // The global space is signed precisely so this needs no special case.
        let mut layout = bootstrap(node(1), &[mon(0, 0, 0, 1920, 1080)]);
        apply(&mut layout, node(2), &[mon(0, 0, 0, 2560, 1440)]);
        assert_eq!(rect_of(&layout, 2, 0), Rect::new(1920, -180, 2560, 1440));
    }

    #[test]
    fn a_multi_monitor_peer_keeps_its_own_arrangement() {
        // The peer's OS already knows how its screens sit; re-deriving that would
        // be strictly worse than copying it.
        let mut layout = bootstrap(node(1), &[hd(0)]);
        apply(
            &mut layout,
            node(2),
            &[mon(0, 0, 0, 1920, 1080), mon(1, -1920, 200, 1920, 1080)],
        );
        let a = rect_of(&layout, 2, 0);
        let b = rect_of(&layout, 2, 1);
        // Relative offsets are preserved exactly.
        assert_eq!(a.x - b.x, 1920);
        assert_eq!(a.y - b.y, -200);
        // And the block as a whole starts at the seam.
        assert_eq!(a.x.min(b.x), 1920);
    }

    #[test]
    fn replacing_a_nodes_monitors_does_not_march_it_further_right() {
        // A peer re-announcing its monitors on every reconnect must not drift.
        let mut layout = bootstrap(node(1), &[hd(0)]);
        apply(&mut layout, node(2), &[hd(0)]);
        let first = rect_of(&layout, 2, 0);
        apply(&mut layout, node(2), &[hd(0)]);
        assert_eq!(rect_of(&layout, 2, 0), first);
    }

    #[test]
    fn an_unchanged_monitor_list_reports_no_change() {
        // Otherwise every reconnect bumps the revision and three machines push
        // layout updates at each other forever.
        let mut layout = bootstrap(node(1), &[hd(0)]);
        assert!(apply(&mut layout, node(2), &[hd(0)]));
        let revision = layout.revision();
        assert!(!apply(&mut layout, node(2), &[hd(0)]));
        assert_eq!(layout.revision(), revision);
    }

    #[test]
    fn monitor_order_alone_is_not_a_change() {
        let mut layout = GlobalLayout::new();
        apply(&mut layout, node(1), &[hd(0), mon(1, 1920, 0, 1920, 1080)]);
        let revision = layout.revision();
        assert!(!apply(
            &mut layout,
            node(1),
            &[mon(1, 1920, 0, 1920, 1080), hd(0)]
        ));
        assert_eq!(layout.revision(), revision);
    }

    #[test]
    fn plugging_in_a_second_screen_is_a_change_and_keeps_the_block_anchored() {
        let mut layout = bootstrap(node(1), &[hd(0)]);
        apply(&mut layout, node(2), &[hd(0)]);
        assert!(apply(
            &mut layout,
            node(2),
            &[hd(0), mon(1, 1920, 0, 1920, 1080)]
        ));
        assert_eq!(rect_of(&layout, 2, 0), Rect::new(1920, 0, 1920, 1080));
        assert_eq!(rect_of(&layout, 2, 1), Rect::new(3840, 0, 1920, 1080));
    }

    #[test]
    fn a_headless_node_takes_up_no_space() {
        let mut layout = bootstrap(node(1), &[hd(0)]);
        assert!(!apply(&mut layout, node(2), &[]));
        assert_eq!(layout.len(), 1);
    }

    #[test]
    fn zero_extent_monitors_are_never_placed() {
        // A cursor parked on a zero-extent rectangle can never leave it.
        let layout = bootstrap(node(1), &[mon(0, 0, 0, 0, 0), hd(1)]);
        assert_eq!(layout.len(), 1);
        assert_eq!(rect_of(&layout, 1, 1), Rect::new(0, 0, 1920, 1080));
    }

    #[test]
    fn a_node_of_only_degenerate_monitors_is_treated_as_headless() {
        let layout = bootstrap(node(1), &[mon(0, 0, 0, 1920, 0)]);
        assert!(layout.is_empty());
    }

    #[test]
    fn an_auto_layout_of_three_machines_has_no_problems() {
        // The real acceptance test: the layout engine's own validator must find
        // no overlaps and no unreachable screens, or the mesh is broken before
        // the user ever sees the editor.
        let mut layout = bootstrap(
            node(1),
            &[mon(0, 0, 0, 2560, 1440), mon(1, 2560, 300, 1920, 1080)],
        );
        apply(&mut layout, node(2), &[mon(0, 0, 0, 1920, 1080)]);
        apply(&mut layout, node(3), &[mon(0, 0, 0, 3840, 2160)]);
        assert_eq!(layout.validate(), Vec::<LayoutProblem>::new());
        assert_eq!(layout.len(), 4);
    }

    #[test]
    fn every_placed_screen_can_be_reached_from_the_first_one() {
        // Stronger than validate(): walk right from the leftmost screen and
        // confirm each block is entered in turn.
        let mut layout = bootstrap(node(1), &[mon(0, 0, 0, 1920, 1080)]);
        apply(&mut layout, node(2), &[mon(0, 0, 0, 2560, 1440)]);
        apply(&mut layout, node(3), &[mon(0, 0, 0, 1280, 800)]);

        let mut cursor = wx_core::VirtualCursor::at(
            &layout,
            GlobalMonitorId::new(node(1), MonitorId(0)),
            wx_proto::NormPos::new(0.5, 0.5),
        )
        .unwrap();
        let mut visited = vec![cursor.monitor().node];
        // Far more than enough steps to cross 1920 + 2560 pixels.
        for _ in 0..600 {
            if let wx_core::CursorMotion::Transitioned { to, .. } =
                cursor.move_by(&layout, 20.0, 0.0)
            {
                visited.push(to.node);
            }
        }
        assert_eq!(visited, vec![node(1), node(2), node(3)]);
    }

    #[test]
    fn forgetting_a_node_removes_exactly_its_screens() {
        let mut layout = bootstrap(node(1), &[hd(0)]);
        apply(&mut layout, node(2), &[hd(0), hd(1)]);
        assert!(forget_node(&mut layout, node(2)));
        assert_eq!(layout.len(), 1);
        assert!(!forget_node(&mut layout, node(2)));
    }

    #[test]
    fn forgetting_leaves_room_for_the_next_peer_where_it_was() {
        // The freed space is reused rather than skipped, so unpairing and
        // re-pairing does not leave a growing hole in the layout.
        let mut layout = bootstrap(node(1), &[hd(0)]);
        apply(&mut layout, node(2), &[hd(0)]);
        forget_node(&mut layout, node(2));
        apply(&mut layout, node(3), &[hd(0)]);
        assert_eq!(rect_of(&layout, 3, 0), Rect::new(1920, 0, 1920, 1080));
    }

    #[test]
    fn union_of_nothing_is_a_degenerate_rectangle_not_a_panic() {
        assert!(union_of_or_origin(Vec::<Rect>::new()).is_empty());
    }

    #[test]
    fn a_mesh_wider_than_i32_max_is_measured_rather_than_overflowing() {
        // `Rect` is an i32 origin plus a u32 extent, so the span between two
        // placements can exceed i32::MAX. Measured in i32 this subtraction panicked
        // and killed the engine task.
        let wide = union_of([
            Rect::new(-2_000_000_000, 0, 1920, 1080),
            Rect::new(2_000_000_000, 0, 1920, 1080),
        ])
        .expect("a non-empty set has a bounding box");
        assert_eq!(wide.x, -2_000_000_000);
        assert_eq!(wide.w, 4_000_001_920);
    }

    #[test]
    fn a_hostile_layout_spanning_the_whole_plane_places_a_peer_instead_of_panicking() {
        // The real path: a paired peer sends a LayoutUpdate at a higher revision
        // with placements at both ends of the coordinate space, the local machine's
        // own screens are absent, so `apply` runs over it. This used to abort the
        // engine's run loop from inside `on_control` with "attempt to subtract with
        // overflow", which loses the cursor, the sessions and the IPC server at once.
        let mut layout = GlobalLayout::new();
        layout.insert(Placement {
            monitor: GlobalMonitorId::new(node(9), MonitorId(0)),
            global_bounds: Rect::new(-2_000_000_000, -2_000_000_000, 1920, 1080),
            cursor_scale: 1.0,
        });
        layout.insert(Placement {
            monitor: GlobalMonitorId::new(node(9), MonitorId(1)),
            global_bounds: Rect::new(2_000_000_000, 2_000_000_000, 1920, 1080),
            cursor_scale: 1.0,
        });

        let placed = place_node(&layout, node(1), &[hd(0)]);
        assert_eq!(placed.len(), 1);
        // Saturated rather than wrapped: the screen is somewhere absurd but it is a
        // real rectangle, and `validate()` can report it.
        assert_eq!(placed[0].global_bounds.w, 1920);
        assert!(apply(&mut layout, node(1), &[hd(0)]));
    }

    #[test]
    fn a_monitor_whose_own_extent_saturates_the_plane_is_still_placeable() {
        // Degenerate but representable: `right()` on this rectangle overflows i32 on
        // its own, so nothing here may compute an edge in i32.
        let mut layout = GlobalLayout::new();
        layout.insert(Placement {
            monitor: GlobalMonitorId::new(node(9), MonitorId(0)),
            global_bounds: Rect::new(2_000_000_000, 0, u32::MAX, 1080),
            cursor_scale: 1.0,
        });
        let placed = place_node(&layout, node(1), &[hd(0)]);
        assert_eq!(placed.len(), 1);
        assert_eq!(placed[0].global_bounds.x, i32::MAX);
    }

    #[test]
    fn ordinary_placement_is_unaffected_by_the_widened_arithmetic() {
        // The clamp must be invisible for every layout anyone will ever really have.
        let mut layout = bootstrap(node(1), &[mon(0, 0, 0, 2560, 1440)]);
        apply(&mut layout, node(2), &[mon(0, -1920, 200, 1920, 1080)]);
        assert_eq!(rect_of(&layout, 2, 0), Rect::new(2560, 180, 1920, 1080));
    }
}
