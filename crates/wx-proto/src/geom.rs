//! Geometry for the global virtual desktop.
//!
//! WinXtend departs from Synergy-lineage tools here. Rather than describing the
//! mesh as per-host neighbour lists ("laptop is to the right of desktop"), every
//! monitor in the mesh is placed as a rectangle in one shared coordinate space.
//! Adjacency is then a geometric fact rather than a thing to configure, which
//! buys three properties for free:
//!
//! * Split edges. A tall monitor bordering two stacked ones routes by cursor
//!   position without any "range" syntax, because the rectangles already say so.
//! * Gaps and diagonals are representable, and behave predictably.
//! * The config UI is a drag-and-drop canvas that matches what every OS already
//!   shows in its display settings, so there is nothing new to learn.

use serde::{Deserialize, Serialize};

/// A point in some desktop coordinate space.
///
/// Floating point because cursor motion accumulates sub-pixel remainders: with
/// per-monitor scaling applied, rounding every intermediate step would make slow
/// drags drift off-axis.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

/// Clamp a widened coordinate back into the range a [`Rect`] edge can hold.
const fn saturate(v: i64) -> i32 {
    if v > i32::MAX as i64 {
        i32::MAX
    } else if v < i32::MIN as i64 {
        i32::MIN
    } else {
        v as i32
    }
}

/// An axis-aligned rectangle, top-left origin.
///
/// Coordinates are signed: OS desktop spaces routinely place secondary monitors
/// at negative offsets, and so does the global space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }

    /// Right edge, saturating rather than overflowing.
    ///
    /// Widened before adding for two distinct reasons, both reachable from a
    /// peer's `LayoutUpdate` since nothing validates inbound coordinates:
    ///
    /// * `x + w` overflows `i32` for large-but-legal values, which panics in
    ///   debug builds and wraps in release. `right()` is on the cursor-routing
    ///   path through [`Rect::contains`], so that is a remotely triggerable
    ///   crash in the agent.
    /// * `w as i32` wraps *negative* above `i32::MAX`, which would put the right
    ///   edge behind `x` and invert every containment test.
    ///
    /// Saturating leaves such a rectangle degenerate but coherent, which routing
    /// already handles, instead of panicking or silently inverting.
    pub const fn right(&self) -> i32 {
        saturate(self.x as i64 + self.w as i64)
    }

    /// Bottom edge, saturating. See [`Rect::right`].
    pub const fn bottom(&self) -> i32 {
        saturate(self.y as i64 + self.h as i64)
    }

    pub fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }

    /// Half-open containment: the right and bottom edges belong to the
    /// neighbouring rectangle, so abutting monitors never both claim a point.
    pub fn contains(&self, p: Point) -> bool {
        p.x >= self.x as f64
            && p.x < self.right() as f64
            && p.y >= self.y as f64
            && p.y < self.bottom() as f64
    }

    /// Nearest point inside the rectangle. Used to park the cursor legally after
    /// a transition lands slightly outside the destination.
    pub fn clamp(&self, p: Point) -> Point {
        // Rectangles with zero extent have no interior to clamp into; the
        // caller gets the origin rather than a NaN-adjacent nudge below x.
        if self.is_empty() {
            return Point::new(self.x as f64, self.y as f64);
        }
        // `nudge` keeps the result strictly inside under half-open containment:
        // clamping to `right()` exactly would land in the next monitor.
        const NUDGE: f64 = 1e-3;
        Point::new(
            p.x.clamp(self.x as f64, self.right() as f64 - NUDGE),
            p.y.clamp(self.y as f64, self.bottom() as f64 - NUDGE),
        )
    }

    /// Position of `p` within the rectangle as fractions of its width/height.
    ///
    /// This is how positions cross the wire: a fraction survives any resolution
    /// or DPI difference between sender and receiver, where a pixel count does
    /// not.
    pub fn normalize(&self, p: Point) -> NormPos {
        if self.is_empty() {
            return NormPos { x: 0.0, y: 0.0 };
        }
        NormPos {
            x: (p.x - self.x as f64) / self.w as f64,
            y: (p.y - self.y as f64) / self.h as f64,
        }
    }

    /// Inverse of [`Rect::normalize`].
    pub fn denormalize(&self, n: NormPos) -> Point {
        Point::new(
            self.x as f64 + n.x * self.w as f64,
            self.y as f64 + n.y * self.h as f64,
        )
    }
}

/// A position expressed as a fraction of a monitor's extent, nominally `0.0..1.0`.
///
/// Resolution-independent by construction, which is what lets a 4K monitor drive
/// a 1080p one without the cursor landing in the wrong place.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct NormPos {
    pub x: f64,
    pub y: f64,
}

impl NormPos {
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    /// Clamp into range, and scrub non-finite values.
    ///
    /// Always applied to values arriving off the wire: a peer sending NaN would
    /// otherwise propagate into cursor arithmetic and wedge the pointer at an
    /// unrecoverable position.
    pub fn sanitized(self) -> Self {
        let fix = |v: f64| {
            if v.is_finite() {
                v.clamp(0.0, 1.0)
            } else {
                0.5
            }
        };
        Self {
            x: fix(self.x),
            y: fix(self.y),
        }
    }
}

/// Which side of a rectangle a crossing happened on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    pub fn opposite(self) -> Self {
        match self {
            Edge::Left => Edge::Right,
            Edge::Right => Edge::Left,
            Edge::Top => Edge::Bottom,
            Edge::Bottom => Edge::Top,
        }
    }
}

/// One physical display attached to a node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Monitor {
    pub id: crate::ids::MonitorId,
    /// OS-reported name, e.g. `DP-1` or `Built-in Retina Display`. Shown in the
    /// layout editor so a monitor is recognisable without trial and error.
    pub name: String,
    /// Bounds in the node's own desktop space, as the OS reports them.
    pub local_bounds: Rect,
    /// OS DPI scale factor (1.0, 2.0, 1.5, ...). Reported for display purposes;
    /// wire positions are normalized so this never enters routing maths.
    pub scale: f32,
    pub primary: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::MonitorId;

    fn r() -> Rect {
        Rect::new(0, 0, 1920, 1080)
    }

    #[test]
    fn extreme_rect_does_not_overflow() {
        // A peer's LayoutUpdate carries arbitrary i32/u32 coordinates, and
        // `right()` sits on the cursor-routing path via `contains`. Unchecked
        // `x + w` panics in debug and wraps in release, so a hostile or merely
        // corrupted layout could crash the agent or silently invert a rectangle.
        let huge = Rect::new(2_000_000_000, 2_000_000_000, 2_000_000_000, 2_000_000_000);
        assert_eq!(huge.right(), i32::MAX);
        assert_eq!(huge.bottom(), i32::MAX);
        assert!(huge.right() >= huge.x, "right() must never fall behind x");
    }

    #[test]
    fn oversized_width_does_not_invert_the_rect() {
        // `w as i32` wraps negative above i32::MAX, which would make right() < x
        // and turn every containment test inside out.
        let wide = Rect::new(0, 0, u32::MAX, u32::MAX);
        assert_eq!(wide.right(), i32::MAX);
        assert_eq!(wide.bottom(), i32::MAX);
        assert!(wide.contains(Point::new(1000.0, 1000.0)));
    }

    #[test]
    fn extreme_rect_containment_and_clamp_stay_sane() {
        let huge = Rect::new(i32::MIN, i32::MIN, u32::MAX, u32::MAX);
        // Must not panic, and must not report a point outside itself.
        let c = huge.clamp(Point::new(0.0, 0.0));
        assert!(c.x.is_finite() && c.y.is_finite());
        let n = huge.normalize(Point::new(0.0, 0.0));
        assert!(n.x.is_finite() && n.y.is_finite());
    }

    #[test]
    fn contains_is_half_open() {
        assert!(r().contains(Point::new(0.0, 0.0)));
        assert!(r().contains(Point::new(1919.9, 1079.9)));
        // Right/bottom edges belong to the next monitor over.
        assert!(!r().contains(Point::new(1920.0, 500.0)));
        assert!(!r().contains(Point::new(500.0, 1080.0)));
        assert!(!r().contains(Point::new(-0.1, 500.0)));
    }

    #[test]
    fn abutting_rects_never_both_claim_a_point() {
        let left = Rect::new(0, 0, 1920, 1080);
        let right = Rect::new(1920, 0, 1920, 1080);
        let seam = Point::new(1920.0, 400.0);
        assert!(!left.contains(seam));
        assert!(right.contains(seam));
    }

    #[test]
    fn normalize_denormalize_round_trips() {
        let rect = Rect::new(-100, 50, 800, 600);
        let p = Point::new(140.0, 320.0);
        let back = rect.denormalize(rect.normalize(p));
        assert!((back.x - p.x).abs() < 1e-9, "x drifted: {back:?}");
        assert!((back.y - p.y).abs() < 1e-9, "y drifted: {back:?}");
    }

    #[test]
    fn normalize_is_resolution_independent() {
        let uhd = Rect::new(0, 0, 3840, 2160);
        let hd = Rect::new(0, 0, 1920, 1080);
        // Dead centre of one monitor is dead centre of the other.
        let centre = uhd.normalize(Point::new(1920.0, 1080.0));
        let mapped = hd.denormalize(centre);
        assert!((mapped.x - 960.0).abs() < 1e-9);
        assert!((mapped.y - 540.0).abs() < 1e-9);
    }

    #[test]
    fn clamp_lands_strictly_inside() {
        let rect = Rect::new(0, 0, 100, 100);
        let c = rect.clamp(Point::new(500.0, -20.0));
        assert!(rect.contains(c), "clamped point escaped: {c:?}");
    }

    #[test]
    fn clamp_handles_degenerate_rect() {
        let rect = Rect::new(10, 20, 0, 0);
        assert_eq!(rect.clamp(Point::new(99.0, 99.0)), Point::new(10.0, 20.0));
    }

    #[test]
    fn sanitize_rejects_hostile_values() {
        assert_eq!(
            NormPos::new(f64::NAN, 2.0).sanitized(),
            NormPos::new(0.5, 1.0)
        );
        assert_eq!(
            NormPos::new(f64::INFINITY, -5.0).sanitized(),
            NormPos::new(0.5, 0.0)
        );
    }

    #[test]
    fn normalize_of_degenerate_rect_is_not_nan() {
        let rect = Rect::new(0, 0, 0, 0);
        let n = rect.normalize(Point::new(5.0, 5.0));
        assert!(n.x.is_finite() && n.y.is_finite());
    }

    #[test]
    fn edge_opposite_is_an_involution() {
        for e in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
            assert_eq!(e.opposite().opposite(), e);
        }
    }

    #[test]
    fn monitor_serde_round_trips() {
        let m = Monitor {
            id: MonitorId(3),
            name: "DP-1".into(),
            local_bounds: Rect::new(0, 0, 2560, 1440),
            scale: 1.25,
            primary: true,
        };
        let bytes = postcard::to_allocvec(&m).unwrap();
        assert_eq!(postcard::from_bytes::<Monitor>(&bytes).unwrap(), m);
    }
}
