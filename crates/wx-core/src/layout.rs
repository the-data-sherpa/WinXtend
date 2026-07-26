//! The global virtual desktop, and how the cursor crosses between monitors.
//!
//! Every monitor in the mesh is a rectangle in one shared coordinate space, so
//! "what is to the right of this screen?" is answered by geometry rather than by
//! configuration. That removes a whole class of setup mistakes — mutually
//! inconsistent neighbour lists — and makes split edges fall out for free.

use std::collections::HashMap;

use wx_proto::{Edge, GlobalMonitorId, Layout, Placement, Point, Rect};

/// Distance in global units used to place an entering cursor just inside the
/// destination monitor.
///
/// Without it, entry lands exactly on the boundary. Under the half-open
/// containment rule that is either outside the destination (for a left/top edge
/// it is fine, for right/bottom it is not) or close enough that the next motion
/// event immediately re-crosses back, which manifests as the cursor flickering
/// between two machines at the seam.
const ENTRY_INSET: f64 = 1.0;

/// A resolved edge crossing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Crossing {
    /// Monitor the cursor is entering.
    pub to: GlobalMonitorId,
    /// Entry position, in global coordinates, already inset inside `to`.
    pub entry: Point,
    /// Edge of the *destination* the cursor came in through.
    pub via: Edge,
}

/// A problem the layout editor should warn about.
#[derive(Debug, Clone, PartialEq)]
pub enum LayoutProblem {
    /// Two monitors occupy the same global space. The cursor can only ever be on
    /// one of them, so the other is partly unreachable.
    Overlap {
        a: GlobalMonitorId,
        b: GlobalMonitorId,
    },
    /// A monitor no other monitor borders or points at: the cursor can never
    /// arrive, so the screen is unreachable.
    Unreachable(GlobalMonitorId),
    /// Zero width or height, which cannot hold a cursor at all.
    Degenerate(GlobalMonitorId),
}

/// Monitor placements, indexed for lookup.
#[derive(Debug, Clone, Default)]
pub struct GlobalLayout {
    placements: HashMap<GlobalMonitorId, Placement>,
    /// Stable iteration order, so crossing resolution is deterministic when two
    /// candidates tie. A HashMap alone would pick arbitrarily between them and
    /// the cursor would jump to a different machine run to run.
    order: Vec<GlobalMonitorId>,
    revision: u64,
}

impl GlobalLayout {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_layout(layout: &Layout) -> Self {
        let mut me = Self {
            placements: HashMap::with_capacity(layout.placements.len()),
            order: Vec::with_capacity(layout.placements.len()),
            revision: layout.revision,
        };
        for p in &layout.placements {
            me.insert(*p);
        }
        me
    }

    pub fn to_layout(&self) -> Layout {
        Layout {
            placements: self
                .order
                .iter()
                .filter_map(|id| self.placements.get(id).copied())
                .collect(),
            revision: self.revision,
        }
    }

    pub fn insert(&mut self, placement: Placement) {
        if self
            .placements
            .insert(placement.monitor, placement)
            .is_none()
        {
            self.order.push(placement.monitor);
        }
    }

    pub fn remove(&mut self, id: GlobalMonitorId) {
        self.placements.remove(&id);
        self.order.retain(|m| *m != id);
    }

    pub fn placement(&self, id: GlobalMonitorId) -> Option<&Placement> {
        self.placements.get(&id)
    }

    pub fn rect(&self, id: GlobalMonitorId) -> Option<Rect> {
        self.placements.get(&id).map(|p| p.global_bounds)
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn set_revision(&mut self, revision: u64) {
        self.revision = revision;
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Placement> + '_ {
        self.order.iter().filter_map(|id| self.placements.get(id))
    }

    /// Monitors belonging to one node.
    pub fn monitors_of(&self, node: wx_proto::NodeId) -> impl Iterator<Item = &Placement> + '_ {
        self.iter().filter(move |p| p.monitor.node == node)
    }

    /// Which monitor contains a global point, if any.
    pub fn monitor_at(&self, p: Point) -> Option<GlobalMonitorId> {
        self.iter()
            .find(|pl| pl.global_bounds.contains(p))
            .map(|pl| pl.monitor)
    }

    /// Bounding box of the whole mesh, for fitting the layout editor's canvas.
    pub fn bounds(&self) -> Option<Rect> {
        let mut it = self.iter().map(|p| p.global_bounds);
        let first = it.next()?;
        let (mut min_x, mut min_y) = (first.x, first.y);
        let (mut max_x, mut max_y) = (first.right(), first.bottom());
        for r in it {
            min_x = min_x.min(r.x);
            min_y = min_y.min(r.y);
            max_x = max_x.max(r.right());
            max_y = max_y.max(r.bottom());
        }
        Some(Rect::new(
            min_x,
            min_y,
            (max_x - min_x) as u32,
            (max_y - min_y) as u32,
        ))
    }

    /// Work out where the cursor goes when it leaves `from` heading for `to`.
    ///
    /// `to` is the unclamped destination, which may be well outside every
    /// monitor. Returns `None` when nothing lies that way, meaning the cursor is
    /// blocked and should be clamped to `from`.
    pub fn resolve_move(
        &self,
        from: GlobalMonitorId,
        origin: Point,
        target: Point,
    ) -> Option<Crossing> {
        let from_rect = self.rect(from)?;

        // Still on the same screen: nothing to resolve.
        if from_rect.contains(target) {
            return None;
        }

        let (edge, exit) = exit_edge(from_rect, origin, target)?;
        self.resolve_crossing(from, exit, edge)
    }

    /// Find the monitor reached by leaving `from` through `edge` at `exit`.
    ///
    /// Candidate selection mirrors how desktop environments handle multi-monitor
    /// arrangements, because that is the behaviour users already have intuitions
    /// about:
    ///
    /// 1. Only monitors genuinely beyond that edge are eligible.
    /// 2. Prefer those whose perpendicular span covers the exit point — the
    ///    cursor keeps its physical height when moving sideways.
    /// 3. Among those, the nearest wins, so gaps in the arrangement are crossed
    ///    rather than blocking.
    /// 4. With no covering candidate, fall back to the nearest by combined gap
    ///    and perpendicular offset, so a diagonal arrangement still connects.
    pub fn resolve_crossing(
        &self,
        from: GlobalMonitorId,
        exit: Point,
        edge: Edge,
    ) -> Option<Crossing> {
        let from_rect = self.rect(from)?;

        let mut covering: Option<(f64, GlobalMonitorId, Rect)> = None;
        let mut nearest: Option<(f64, GlobalMonitorId, Rect)> = None;

        for pl in self.iter() {
            if pl.monitor == from || pl.global_bounds.is_empty() {
                continue;
            }
            let r = pl.global_bounds;
            let Some(gap) = axis_gap(from_rect, r, edge) else {
                continue;
            };
            let offset = perpendicular_offset(r, exit, edge);

            if offset == 0.0 {
                let better = covering.is_none_or(|(g, _, _)| gap < g);
                if better {
                    covering = Some((gap, pl.monitor, r));
                }
            }
            // Ranked by gap plus offset so a screen straight ahead beats one the
            // same distance away but far off to the side.
            let score = gap + offset;
            let better = nearest.is_none_or(|(s, _, _)| score < s);
            if better {
                nearest = Some((score, pl.monitor, r));
            }
        }

        let (_, to, to_rect) = covering.or(nearest)?;
        let via = edge.opposite();
        Some(Crossing {
            to,
            entry: entry_point(to_rect, exit, via),
            via,
        })
    }

    /// Problems worth surfacing in the layout editor.
    pub fn validate(&self) -> Vec<LayoutProblem> {
        let mut problems = Vec::new();

        for pl in self.iter() {
            if pl.global_bounds.is_empty() {
                problems.push(LayoutProblem::Degenerate(pl.monitor));
            }
        }

        let all: Vec<&Placement> = self.iter().collect();
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                if rects_overlap(a.global_bounds, b.global_bounds) {
                    problems.push(LayoutProblem::Overlap {
                        a: a.monitor,
                        b: b.monitor,
                    });
                }
            }
        }

        // A single-monitor mesh is trivially reachable; only flag isolation once
        // there is somewhere else to come from.
        if all.len() > 1 {
            for pl in &all {
                if pl.global_bounds.is_empty() {
                    continue;
                }
                let reachable = [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom]
                    .iter()
                    .any(|edge| {
                        let exit = edge_midpoint(pl.global_bounds, *edge);
                        self.resolve_crossing(pl.monitor, exit, *edge)
                            .is_some_and(|c| {
                                // Reachable if the trip is reversible: something
                                // over there can send the cursor back here.
                                self.resolve_crossing(c.to, exit, edge.opposite())
                                    .is_some_and(|back| back.to == pl.monitor)
                            })
                    });
                if !reachable {
                    problems.push(LayoutProblem::Unreachable(pl.monitor));
                }
            }
        }

        problems
    }
}

/// Which edge of `rect` a move from `from` to `to` leaves through, and where.
///
/// When a diagonal move crosses two edges, the one crossed *first* along the path
/// wins — that is the edge the cursor physically passes through.
fn exit_edge(rect: Rect, from: Point, to: Point) -> Option<(Edge, Point)> {
    let dx = to.x - from.x;
    let dy = to.y - from.y;
    if !dx.is_finite() || !dy.is_finite() || (dx == 0.0 && dy == 0.0) {
        return None;
    }

    let mut best: Option<(f64, Edge)> = None;
    let mut consider = |t: f64, edge: Edge| {
        // Reject crossings behind the start or beyond the destination: those
        // edges are not on this path.
        // NaN needs no separate check: `contains` is false for it, so a
        // non-finite `t` is rejected by the same test as an out-of-range one.
        if !(0.0..=1.0).contains(&t) {
            return;
        }
        if best.is_none_or(|(bt, _)| t < bt) {
            best = Some((t, edge));
        }
    };

    if dx > 0.0 {
        consider((rect.right() as f64 - from.x) / dx, Edge::Right);
    } else if dx < 0.0 {
        consider((rect.x as f64 - from.x) / dx, Edge::Left);
    }
    if dy > 0.0 {
        consider((rect.bottom() as f64 - from.y) / dy, Edge::Bottom);
    } else if dy < 0.0 {
        consider((rect.y as f64 - from.y) / dy, Edge::Top);
    }

    let (t, edge) = best?;
    Some((edge, Point::new(from.x + dx * t, from.y + dy * t)))
}

/// Distance from `from`'s `edge` to `candidate`, or `None` if the candidate is
/// not beyond that edge at all.
fn axis_gap(from: Rect, candidate: Rect, edge: Edge) -> Option<f64> {
    let gap = match edge {
        Edge::Right => candidate.x - from.right(),
        Edge::Left => from.x - candidate.right(),
        Edge::Bottom => candidate.y - from.bottom(),
        Edge::Top => from.y - candidate.bottom(),
    };
    // Zero is the abutting case and must be allowed; negative means the
    // candidate straddles or sits behind the edge, so it is not "that way".
    (gap >= 0).then_some(gap as f64)
}

/// How far `exit` sits outside `rect` along the axis parallel to `edge`.
///
/// Zero means the monitor's span covers the exit point, so a straight move lands
/// on it.
fn perpendicular_offset(rect: Rect, exit: Point, edge: Edge) -> f64 {
    match edge {
        Edge::Left | Edge::Right => {
            if exit.y < rect.y as f64 {
                rect.y as f64 - exit.y
            } else if exit.y >= rect.bottom() as f64 {
                exit.y - (rect.bottom() as f64 - 1.0)
            } else {
                0.0
            }
        }
        Edge::Top | Edge::Bottom => {
            if exit.x < rect.x as f64 {
                rect.x as f64 - exit.x
            } else if exit.x >= rect.right() as f64 {
                exit.x - (rect.right() as f64 - 1.0)
            } else {
                0.0
            }
        }
    }
}

/// Where the cursor lands when entering `rect` through `via`, preserving its
/// position along the seam.
///
/// The parallel coordinate carries over in *global* units rather than as a
/// fraction: for physically aligned monitors that keeps the cursor at the same
/// height, which is what makes a crossing feel like one continuous desktop.
fn entry_point(rect: Rect, exit: Point, via: Edge) -> Point {
    let p = match via {
        Edge::Left => Point::new(rect.x as f64 + ENTRY_INSET, exit.y),
        Edge::Right => Point::new(rect.right() as f64 - ENTRY_INSET, exit.y),
        Edge::Top => Point::new(exit.x, rect.y as f64 + ENTRY_INSET),
        Edge::Bottom => Point::new(exit.x, rect.bottom() as f64 - ENTRY_INSET),
    };
    rect.clamp(p)
}

fn edge_midpoint(rect: Rect, edge: Edge) -> Point {
    let cx = rect.x as f64 + rect.w as f64 / 2.0;
    let cy = rect.y as f64 + rect.h as f64 / 2.0;
    match edge {
        Edge::Left => Point::new(rect.x as f64, cy),
        Edge::Right => Point::new(rect.right() as f64, cy),
        Edge::Top => Point::new(cx, rect.y as f64),
        Edge::Bottom => Point::new(cx, rect.bottom() as f64),
    }
}

fn rects_overlap(a: Rect, b: Rect) -> bool {
    if a.is_empty() || b.is_empty() {
        return false;
    }
    a.x < b.right() && b.x < a.right() && a.y < b.bottom() && b.y < a.bottom()
}

#[cfg(test)]
mod tests {
    use super::*;
    use wx_proto::{MonitorId, NodeId};

    fn node(n: u8) -> NodeId {
        NodeId([n; 32])
    }

    fn gid(n: u8, m: u32) -> GlobalMonitorId {
        GlobalMonitorId::new(node(n), MonitorId(m))
    }

    fn place(id: GlobalMonitorId, x: i32, y: i32, w: u32, h: u32) -> Placement {
        Placement {
            monitor: id,
            global_bounds: Rect::new(x, y, w, h),
            cursor_scale: 1.0,
        }
    }

    /// Two 1080p screens side by side: the canonical setup.
    fn side_by_side() -> GlobalLayout {
        let mut l = GlobalLayout::new();
        l.insert(place(gid(1, 0), 0, 0, 1920, 1080));
        l.insert(place(gid(2, 0), 1920, 0, 1920, 1080));
        l
    }

    #[test]
    fn monitor_at_finds_the_containing_screen() {
        let l = side_by_side();
        assert_eq!(l.monitor_at(Point::new(10.0, 10.0)), Some(gid(1, 0)));
        assert_eq!(l.monitor_at(Point::new(2000.0, 10.0)), Some(gid(2, 0)));
        assert_eq!(l.monitor_at(Point::new(-5.0, 10.0)), None);
    }

    #[test]
    fn moving_right_crosses_to_the_right_screen() {
        let l = side_by_side();
        let c = l
            .resolve_move(
                gid(1, 0),
                Point::new(1900.0, 500.0),
                Point::new(1930.0, 500.0),
            )
            .expect("should cross");
        assert_eq!(c.to, gid(2, 0));
        assert_eq!(c.via, Edge::Left);
        // Physical height is preserved across the seam.
        assert!((c.entry.y - 500.0).abs() < 1.0, "entry {:?}", c.entry);
    }

    #[test]
    fn entry_point_is_strictly_inside_the_destination() {
        // If entry landed on the boundary the next motion event would bounce the
        // cursor straight back, which users see as flicker at the seam.
        let l = side_by_side();
        let c = l
            .resolve_move(
                gid(1, 0),
                Point::new(1900.0, 500.0),
                Point::new(2000.0, 500.0),
            )
            .unwrap();
        let dest = l.rect(c.to).unwrap();
        assert!(
            dest.contains(c.entry),
            "entry {:?} outside {dest:?}",
            c.entry
        );
    }

    #[test]
    fn crossing_back_returns_to_the_original_screen() {
        let l = side_by_side();
        let there = l
            .resolve_move(
                gid(1, 0),
                Point::new(1900.0, 400.0),
                Point::new(1950.0, 400.0),
            )
            .unwrap();
        assert_eq!(there.to, gid(2, 0));

        let back = l
            .resolve_move(
                gid(2, 0),
                there.entry,
                Point::new(there.entry.x - 50.0, 400.0),
            )
            .unwrap();
        assert_eq!(back.to, gid(1, 0));
        assert_eq!(back.via, Edge::Right);
    }

    #[test]
    fn movement_within_one_screen_is_not_a_crossing() {
        let l = side_by_side();
        assert_eq!(
            l.resolve_move(
                gid(1, 0),
                Point::new(100.0, 100.0),
                Point::new(200.0, 200.0)
            ),
            None
        );
    }

    #[test]
    fn moving_off_the_far_edge_is_blocked() {
        // Nothing to the left of the leftmost screen: the cursor must stop.
        let l = side_by_side();
        assert_eq!(
            l.resolve_move(gid(1, 0), Point::new(10.0, 500.0), Point::new(-50.0, 500.0)),
            None
        );
    }

    #[test]
    fn split_edge_routes_by_cursor_height_without_configuration() {
        // A 1440-tall screen on the left, two 720-tall screens stacked on the
        // right. Geometry alone must route to the correct one.
        let mut l = GlobalLayout::new();
        l.insert(place(gid(1, 0), 0, 0, 2560, 1440));
        l.insert(place(gid(2, 0), 2560, 0, 1280, 720));
        l.insert(place(gid(3, 0), 2560, 720, 1280, 720));

        let top = l
            .resolve_move(
                gid(1, 0),
                Point::new(2550.0, 200.0),
                Point::new(2600.0, 200.0),
            )
            .unwrap();
        assert_eq!(top.to, gid(2, 0), "upper exit should reach the top screen");

        let bottom = l
            .resolve_move(
                gid(1, 0),
                Point::new(2550.0, 1200.0),
                Point::new(2600.0, 1200.0),
            )
            .unwrap();
        assert_eq!(
            bottom.to,
            gid(3, 0),
            "lower exit should reach the bottom screen"
        );
    }

    #[test]
    fn gaps_in_the_arrangement_are_crossed_not_blocked() {
        // Users leave gaps when arranging screens; the cursor should still pass.
        let mut l = GlobalLayout::new();
        l.insert(place(gid(1, 0), 0, 0, 1920, 1080));
        l.insert(place(gid(2, 0), 2400, 0, 1920, 1080));

        let c = l
            .resolve_move(
                gid(1, 0),
                Point::new(1900.0, 500.0),
                Point::new(1950.0, 500.0),
            )
            .expect("gap should not block");
        assert_eq!(c.to, gid(2, 0));
    }

    #[test]
    fn nearest_screen_wins_when_two_are_the_same_way() {
        let mut l = GlobalLayout::new();
        l.insert(place(gid(1, 0), 0, 0, 1920, 1080));
        l.insert(place(gid(2, 0), 1920, 0, 1920, 1080));
        l.insert(place(gid(3, 0), 5000, 0, 1920, 1080));

        let c = l
            .resolve_move(
                gid(1, 0),
                Point::new(1900.0, 500.0),
                Point::new(1950.0, 500.0),
            )
            .unwrap();
        assert_eq!(c.to, gid(2, 0), "should stop at the nearer screen");
    }

    #[test]
    fn vertical_stacking_works() {
        let mut l = GlobalLayout::new();
        l.insert(place(gid(1, 0), 0, 0, 1920, 1080));
        l.insert(place(gid(2, 0), 0, 1080, 1920, 1080));

        let down = l
            .resolve_move(
                gid(1, 0),
                Point::new(500.0, 1070.0),
                Point::new(500.0, 1120.0),
            )
            .unwrap();
        assert_eq!(down.to, gid(2, 0));
        assert_eq!(down.via, Edge::Top);
        assert!((down.entry.x - 500.0).abs() < 1.0);

        let up = l
            .resolve_move(
                gid(2, 0),
                Point::new(500.0, 1090.0),
                Point::new(500.0, 1040.0),
            )
            .unwrap();
        assert_eq!(up.to, gid(1, 0));
        assert_eq!(up.via, Edge::Bottom);
    }

    #[test]
    fn diagonal_move_uses_the_edge_crossed_first() {
        // Leaving from near the bottom-right corner, mostly downward: the bottom
        // edge is crossed before the right one, so the move goes down.
        let mut l = GlobalLayout::new();
        l.insert(place(gid(1, 0), 0, 0, 1000, 1000));
        l.insert(place(gid(2, 0), 1000, 0, 1000, 1000));
        l.insert(place(gid(3, 0), 0, 1000, 1000, 1000));

        let c = l
            .resolve_move(
                gid(1, 0),
                Point::new(900.0, 990.0),
                Point::new(920.0, 1100.0),
            )
            .unwrap();
        assert_eq!(c.to, gid(3, 0), "should exit through the bottom edge");
    }

    #[test]
    fn exit_edge_picks_the_first_boundary_on_the_path() {
        let rect = Rect::new(0, 0, 100, 100);
        // Straight right.
        let (edge, p) = exit_edge(rect, Point::new(50.0, 50.0), Point::new(150.0, 50.0)).unwrap();
        assert_eq!(edge, Edge::Right);
        assert!((p.x - 100.0).abs() < 1e-9 && (p.y - 50.0).abs() < 1e-9);

        // Mostly down, slightly right: bottom comes first.
        let (edge, _) = exit_edge(rect, Point::new(90.0, 90.0), Point::new(110.0, 200.0)).unwrap();
        assert_eq!(edge, Edge::Bottom);
    }

    #[test]
    fn exit_edge_ignores_zero_and_nonfinite_movement() {
        let rect = Rect::new(0, 0, 100, 100);
        assert!(exit_edge(rect, Point::new(50.0, 50.0), Point::new(50.0, 50.0)).is_none());
        assert!(exit_edge(rect, Point::new(50.0, 50.0), Point::new(f64::NAN, 50.0)).is_none());
    }

    #[test]
    fn multiple_monitors_on_one_node_are_addressable_separately() {
        let mut l = GlobalLayout::new();
        l.insert(place(gid(1, 0), 0, 0, 1920, 1080));
        l.insert(place(gid(1, 1), 1920, 0, 1920, 1080));
        l.insert(place(gid(2, 0), 3840, 0, 1920, 1080));

        assert_eq!(l.monitors_of(node(1)).count(), 2);
        assert_eq!(l.monitors_of(node(2)).count(), 1);

        // Crossing between two monitors of the same machine stays local.
        let c = l
            .resolve_move(
                gid(1, 0),
                Point::new(1900.0, 500.0),
                Point::new(1950.0, 500.0),
            )
            .unwrap();
        assert_eq!(c.to, gid(1, 1));
    }

    #[test]
    fn bounds_covers_every_monitor_including_negative_offsets() {
        let mut l = GlobalLayout::new();
        l.insert(place(gid(1, 0), 0, 0, 1920, 1080));
        l.insert(place(gid(2, 0), -1280, -200, 1280, 1024));
        assert_eq!(l.bounds(), Some(Rect::new(-1280, -200, 3200, 1280)));
    }

    #[test]
    fn bounds_of_empty_layout_is_none() {
        assert_eq!(GlobalLayout::new().bounds(), None);
    }

    #[test]
    fn validate_accepts_a_sane_layout() {
        assert!(side_by_side().validate().is_empty());
    }

    #[test]
    fn validate_reports_overlapping_monitors() {
        let mut l = GlobalLayout::new();
        l.insert(place(gid(1, 0), 0, 0, 1920, 1080));
        l.insert(place(gid(2, 0), 1000, 0, 1920, 1080));
        let problems = l.validate();
        assert!(
            problems
                .iter()
                .any(|p| matches!(p, LayoutProblem::Overlap { .. })),
            "{problems:?}"
        );
    }

    #[test]
    fn abutting_monitors_do_not_count_as_overlapping() {
        assert!(!rects_overlap(
            Rect::new(0, 0, 100, 100),
            Rect::new(100, 0, 100, 100)
        ));
        assert!(rects_overlap(
            Rect::new(0, 0, 100, 100),
            Rect::new(99, 0, 100, 100)
        ));
    }

    #[test]
    fn validate_flags_a_degenerate_monitor() {
        let mut l = side_by_side();
        l.insert(place(gid(3, 0), 5000, 0, 0, 0));
        let problems = l.validate();
        assert!(
            problems
                .iter()
                .any(|p| matches!(p, LayoutProblem::Degenerate(m) if *m == gid(3, 0))),
            "{problems:?}"
        );
    }

    #[test]
    fn single_monitor_layout_is_not_flagged_unreachable() {
        let mut l = GlobalLayout::new();
        l.insert(place(gid(1, 0), 0, 0, 1920, 1080));
        assert!(l.validate().is_empty());
    }

    #[test]
    fn layout_round_trips_through_the_wire_type() {
        let l = side_by_side();
        let wire = l.to_layout();
        let back = GlobalLayout::from_layout(&wire);
        assert_eq!(back.len(), l.len());
        assert_eq!(back.rect(gid(2, 0)), l.rect(gid(2, 0)));
    }

    #[test]
    fn insert_replaces_rather_than_duplicating() {
        let mut l = GlobalLayout::new();
        l.insert(place(gid(1, 0), 0, 0, 1920, 1080));
        l.insert(place(gid(1, 0), 0, 0, 2560, 1440));
        assert_eq!(l.len(), 1);
        assert_eq!(l.rect(gid(1, 0)), Some(Rect::new(0, 0, 2560, 1440)));
    }

    #[test]
    fn remove_takes_the_monitor_out_of_routing() {
        let mut l = side_by_side();
        l.remove(gid(2, 0));
        assert_eq!(l.len(), 1);
        assert_eq!(
            l.resolve_move(
                gid(1, 0),
                Point::new(1900.0, 500.0),
                Point::new(1950.0, 500.0)
            ),
            None
        );
    }

    #[test]
    fn unknown_source_monitor_resolves_to_nothing() {
        let l = side_by_side();
        assert_eq!(
            l.resolve_move(gid(9, 9), Point::new(0.0, 0.0), Point::new(100.0, 0.0)),
            None
        );
    }

    #[test]
    fn iteration_order_is_stable_across_rebuilds() {
        // Crossing resolution must not depend on HashMap iteration order, or the
        // cursor would go to a different machine between runs.
        let l = side_by_side();
        let first: Vec<_> = l.iter().map(|p| p.monitor).collect();
        for _ in 0..10 {
            let rebuilt = GlobalLayout::from_layout(&l.to_layout());
            assert_eq!(rebuilt.iter().map(|p| p.monitor).collect::<Vec<_>>(), first);
        }
    }
}
