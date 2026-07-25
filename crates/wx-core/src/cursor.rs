//! The authoritative cursor, in global virtual-desktop coordinates.
//!
//! Exactly one [`VirtualCursor`] in the mesh is authoritative: it lives on the
//! node that is currently capturing input, and every machine's physical pointer
//! is a rendering of it. Keeping the truth in one place is what makes crossings
//! deterministic — two machines each integrating their own deltas would disagree
//! about which side of a seam the cursor is on, and the pointer would oscillate.
//!
//! Position is kept in global units rather than as a normalized fraction because
//! motion arrives as deltas that must be integrated. Normalization happens only
//! at the moment a position is handed out, since that is the only place a
//! resolution-independent value is needed.

use wx_proto::{Edge, GlobalMonitorId, NormPos, Point, Rect};

use crate::layout::GlobalLayout;

/// What one motion step did.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CursorMotion {
    /// Still on the same monitor.
    Moved {
        monitor: GlobalMonitorId,
        pos: NormPos,
    },
    /// Crossed a seam. The cursor now belongs to `to`.
    Transitioned {
        from: GlobalMonitorId,
        to: GlobalMonitorId,
        /// Where the cursor landed, as a fraction of `to`'s extent.
        entry: NormPos,
        /// Edge of `to` it came in through, so the receiver can nudge clear of
        /// the seam instead of immediately crossing back.
        via: Edge,
    },
    /// Nothing lies that way; the cursor was clamped to the monitor it is on.
    Blocked {
        monitor: GlobalMonitorId,
        pos: NormPos,
    },
}

impl CursorMotion {
    /// The monitor holding the cursor *after* this motion.
    pub fn monitor(&self) -> GlobalMonitorId {
        match *self {
            CursorMotion::Moved { monitor, .. } | CursorMotion::Blocked { monitor, .. } => monitor,
            CursorMotion::Transitioned { to, .. } => to,
        }
    }

    /// Where the cursor ended up, normalized within [`CursorMotion::monitor`].
    pub fn pos(&self) -> NormPos {
        match *self {
            CursorMotion::Moved { pos, .. } | CursorMotion::Blocked { pos, .. } => pos,
            CursorMotion::Transitioned { entry, .. } => entry,
        }
    }

    pub fn is_transition(&self) -> bool {
        matches!(self, CursorMotion::Transitioned { .. })
    }
}

/// Why a cursor could not be placed on a monitor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum WarpError {
    /// Named monitor is not in the current layout. Happens when a peer sends a
    /// handoff for a display that has since been unplugged.
    #[error("monitor {0} is not in the layout")]
    UnknownMonitor(GlobalMonitorId),
    /// Zero width or height. Refused rather than clamped, because a cursor
    /// parked on a zero-extent rectangle can never leave it by motion: every
    /// position is simultaneously outside the rectangle and unnormalizable.
    #[error("monitor {0} has zero extent")]
    DegenerateMonitor(GlobalMonitorId),
}

/// The one true cursor.
#[derive(Debug, Clone)]
pub struct VirtualCursor {
    monitor: GlobalMonitorId,
    /// Position in global coordinates, always inside `monitor`'s rectangle.
    pos: Point,
    /// Sub-unit motion not yet spent.
    ///
    /// Cursor scale turns integral device deltas into fractional global ones. If
    /// each step were rounded independently, a slow diagonal drag under a scale
    /// of 0.3 would round every step to zero and the cursor would never move at
    /// all; under 1.4 the two axes would round differently and the drag would
    /// bend off-axis. Carrying the remainder makes the integration exact.
    remainder: Point,
    /// Last position we were able to express as a fraction.
    ///
    /// Kept so that motion attempted while the holding monitor is missing from
    /// the layout — a display unplugged mid-drag — still reports a usable
    /// position instead of a NaN derived from a zero-extent rectangle.
    last_norm: NormPos,
    locked: bool,
}

impl VirtualCursor {
    /// Place the cursor on `monitor` at `entry`.
    pub fn at(
        layout: &GlobalLayout,
        monitor: GlobalMonitorId,
        entry: NormPos,
    ) -> Result<Self, WarpError> {
        let rect = usable_rect(layout, monitor)?;
        let pos = rect.clamp(rect.denormalize(entry.sanitized()));
        Ok(Self {
            monitor,
            pos,
            remainder: Point::new(0.0, 0.0),
            last_norm: rect.normalize(pos).sanitized(),
            locked: false,
        })
    }

    /// Park the cursor on the first monitor of the layout.
    ///
    /// Startup convenience: an agent with no persisted cursor state has to put
    /// it somewhere, and layout iteration order is stable.
    pub fn anywhere(layout: &GlobalLayout) -> Option<Self> {
        layout
            .iter()
            .find(|p| !p.global_bounds.is_empty())
            .map(|p| p.monitor)
            .and_then(|m| Self::at(layout, m, NormPos::new(0.5, 0.5)).ok())
    }

    pub fn monitor(&self) -> GlobalMonitorId {
        self.monitor
    }

    /// Position in global coordinates. For diagnostics and the layout editor;
    /// anything crossing the wire wants [`VirtualCursor::norm_position`].
    pub fn global_position(&self) -> Point {
        self.pos
    }

    /// Position as a fraction of the holding monitor's extent.
    pub fn norm_position(&self, layout: &GlobalLayout) -> NormPos {
        match layout.rect(self.monitor) {
            Some(rect) if !rect.is_empty() => rect.normalize(self.pos).sanitized(),
            _ => self.last_norm,
        }
    }

    pub fn is_locked(&self) -> bool {
        self.locked
    }

    /// Pin the cursor to the monitor it is on, or release it.
    ///
    /// Bound to a hotkey (Ctrl+Alt+Super+L) because full-screen applications —
    /// games, VMs, remote-desktop clients — are exactly the case where sliding
    /// off the edge onto another machine is never what the user meant.
    pub fn set_locked(&mut self, locked: bool) {
        self.locked = locked;
    }

    pub fn toggle_lock(&mut self) -> bool {
        self.locked = !self.locked;
        self.locked
    }

    /// Integrate a raw pointer delta in the capturing device's units.
    ///
    /// A single delta crosses at most one seam. A very large delta — a flick, or
    /// a stall that batched several reports together — therefore stops at the
    /// first screen it reaches instead of skipping over a machine entirely,
    /// which would make that machine unreachable at speed.
    pub fn move_by(&mut self, layout: &GlobalLayout, dx: f64, dy: f64) -> CursorMotion {
        let Some(placement) = layout.placement(self.monitor).copied() else {
            // The monitor holding the cursor is gone from the layout. Refuse the
            // motion rather than guessing a new home: rehoming is a policy the
            // caller owns, and picking one here would teleport the user's
            // pointer to an arbitrary machine mid-gesture.
            return CursorMotion::Blocked {
                monitor: self.monitor,
                pos: self.last_norm,
            };
        };
        let rect = placement.global_bounds;
        if rect.is_empty() {
            return CursorMotion::Blocked {
                monitor: self.monitor,
                pos: self.last_norm,
            };
        }

        let scale = effective_scale(placement.cursor_scale);
        let step = self.accumulate(sanitize_delta(dx) * scale, sanitize_delta(dy) * scale);
        let target = Point::new(self.pos.x + step.x, self.pos.y + step.y);

        if rect.contains(target) {
            self.pos = target;
            return CursorMotion::Moved {
                monitor: self.monitor,
                pos: self.renormalize(rect),
            };
        }

        if self.locked {
            self.pos = rect.clamp(target);
            return CursorMotion::Blocked {
                monitor: self.monitor,
                pos: self.renormalize(rect),
            };
        }

        match layout.resolve_move(self.monitor, self.pos, target) {
            Some(crossing) => {
                let from = self.monitor;
                let to_rect = layout.rect(crossing.to).unwrap_or(rect);
                self.monitor = crossing.to;
                self.pos = crossing.entry;
                // The remainder was accumulated under the old monitor's scale;
                // spending it on the new one would smear the first step after a
                // crossing by up to a pixel in the wrong proportion.
                self.remainder = Point::new(0.0, 0.0);
                CursorMotion::Transitioned {
                    from,
                    to: crossing.to,
                    entry: self.renormalize(to_rect),
                    via: crossing.via,
                }
            }
            None => {
                self.pos = rect.clamp(target);
                CursorMotion::Blocked {
                    monitor: self.monitor,
                    pos: self.renormalize(rect),
                }
            }
        }
    }

    /// Teleport the cursor, as when a peer hands control over.
    ///
    /// Allowed while locked: the lock exists to stop *accidental* edge crossings,
    /// and an explicit handoff is not accidental. Refusing it here would strand
    /// control on a machine that has already yielded.
    pub fn warp_to(
        &mut self,
        layout: &GlobalLayout,
        monitor: GlobalMonitorId,
        entry: NormPos,
    ) -> Result<NormPos, WarpError> {
        let rect = usable_rect(layout, monitor)?;
        self.monitor = monitor;
        self.pos = rect.clamp(rect.denormalize(entry.sanitized()));
        // A teleport is discontinuous, so any banked sub-unit motion belongs to
        // a gesture that has ended.
        self.remainder = Point::new(0.0, 0.0);
        Ok(self.renormalize(rect))
    }

    /// Spend as much accumulated motion as amounts to whole global units.
    fn accumulate(&mut self, sx: f64, sy: f64) -> Point {
        let wanted = Point::new(self.remainder.x + sx, self.remainder.y + sy);
        // Truncation toward zero, so the accumulator behaves identically for
        // leftward and rightward motion. Rounding half-up would bias one way.
        let step = Point::new(wanted.x.trunc(), wanted.y.trunc());
        let keep = |w: f64, s: f64| {
            let r = w - s;
            // A delta large enough to lose precision leaves nothing meaningful
            // to carry; keeping a non-finite remainder would poison every later
            // step.
            if r.is_finite() {
                r
            } else {
                0.0
            }
        };
        self.remainder = Point::new(keep(wanted.x, step.x), keep(wanted.y, step.y));
        Point::new(
            if step.x.is_finite() { step.x } else { 0.0 },
            if step.y.is_finite() { step.y } else { 0.0 },
        )
    }

    fn renormalize(&mut self, rect: Rect) -> NormPos {
        self.last_norm = rect.normalize(self.pos).sanitized();
        self.last_norm
    }
}

fn usable_rect(layout: &GlobalLayout, monitor: GlobalMonitorId) -> Result<Rect, WarpError> {
    let rect = layout
        .rect(monitor)
        .ok_or(WarpError::UnknownMonitor(monitor))?;
    if rect.is_empty() {
        return Err(WarpError::DegenerateMonitor(monitor));
    }
    Ok(rect)
}

/// Cursor speed multiplier, with hostile values neutralised.
///
/// A layout arrives off the wire, so `cursor_scale` can be zero (the cursor
/// would freeze and the user could never leave the screen), negative (motion
/// inverted), or NaN (position destroyed permanently). All three degrade to
/// unscaled rather than being rejected, because a bad scale is a cosmetic
/// mistake and refusing to move the cursor at all is not recoverable by mouse.
fn effective_scale(scale: f32) -> f64 {
    let s = scale as f64;
    if s.is_finite() && s > 0.0 {
        s
    } else {
        1.0
    }
}

/// Non-finite deltas are dropped, not propagated.
///
/// A platform capture backend that reports NaN once would otherwise put the
/// cursor at a position no clamp can recover, and the pointer would be stuck
/// until the agent restarts.
fn sanitize_delta(d: f64) -> f64 {
    if d.is_finite() {
        d
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wx_proto::{MonitorId, NodeId, Placement};

    fn gid(n: u8, m: u32) -> GlobalMonitorId {
        GlobalMonitorId::new(NodeId([n; 32]), MonitorId(m))
    }

    fn place(id: GlobalMonitorId, x: i32, y: i32, w: u32, h: u32, scale: f32) -> Placement {
        Placement {
            monitor: id,
            global_bounds: Rect::new(x, y, w, h),
            cursor_scale: scale,
        }
    }

    /// Two 1080p screens side by side, unscaled.
    fn side_by_side() -> GlobalLayout {
        let mut l = GlobalLayout::new();
        l.insert(place(gid(1, 0), 0, 0, 1920, 1080, 1.0));
        l.insert(place(gid(2, 0), 1920, 0, 1920, 1080, 1.0));
        l
    }

    fn cursor_on(layout: &GlobalLayout, id: GlobalMonitorId) -> VirtualCursor {
        VirtualCursor::at(layout, id, NormPos::new(0.5, 0.5)).expect("monitor exists")
    }

    #[test]
    fn motion_within_a_screen_reports_a_normalized_position() {
        let l = side_by_side();
        let mut c = cursor_on(&l, gid(1, 0));
        let m = c.move_by(&l, 96.0, 0.0);
        match m {
            CursorMotion::Moved { monitor, pos } => {
                assert_eq!(monitor, gid(1, 0));
                assert!((pos.x - 0.55).abs() < 1e-9, "{pos:?}");
                assert!((pos.y - 0.5).abs() < 1e-9, "{pos:?}");
            }
            other => panic!("expected a plain move, got {other:?}"),
        }
    }

    #[test]
    fn leaving_the_right_edge_transitions_to_the_next_screen() {
        let l = side_by_side();
        let mut c = cursor_on(&l, gid(1, 0));
        // Up to the edge but not over it.
        assert!(!c.move_by(&l, 900.0, 0.0).is_transition());
        let m = c.move_by(&l, 1000.0, 0.0);
        match m {
            CursorMotion::Transitioned {
                from,
                to,
                entry,
                via,
            } => {
                assert_eq!(from, gid(1, 0));
                assert_eq!(to, gid(2, 0));
                assert_eq!(via, Edge::Left);
                // Entered at the left seam, at the same height.
                assert!(entry.x < 0.01, "{entry:?}");
                assert!((entry.y - 0.5).abs() < 0.01, "{entry:?}");
            }
            other => panic!("expected a transition, got {other:?}"),
        }
        assert_eq!(c.monitor(), gid(2, 0));
    }

    #[test]
    fn a_single_huge_delta_crosses_at_most_one_seam() {
        // A flick or a batched report must not skip a machine: three screens in
        // a row, one delta wide enough to reach the third.
        let mut l = side_by_side();
        l.insert(place(gid(3, 0), 3840, 0, 1920, 1080, 1.0));
        let mut c = cursor_on(&l, gid(1, 0));
        let m = c.move_by(&l, 100_000.0, 0.0);
        assert!(m.is_transition());
        assert_eq!(c.monitor(), gid(2, 0), "should stop at the adjacent screen");
    }

    #[test]
    fn nothing_beyond_the_edge_blocks_and_clamps_inside() {
        let l = side_by_side();
        let mut c = cursor_on(&l, gid(1, 0));
        let m = c.move_by(&l, -5000.0, 0.0);
        match m {
            CursorMotion::Blocked { monitor, pos } => {
                assert_eq!(monitor, gid(1, 0));
                assert_eq!(pos.x, 0.0);
            }
            other => panic!("expected blocked, got {other:?}"),
        }
        // Still legally on the screen, so the next move works normally.
        assert!(l.rect(gid(1, 0)).unwrap().contains(c.global_position()));
    }

    #[test]
    fn blocked_axis_does_not_freeze_the_free_axis() {
        // Sliding along the far left edge must still move vertically, or the
        // cursor sticks in a corner.
        let l = side_by_side();
        let mut c = cursor_on(&l, gid(1, 0));
        c.move_by(&l, -5000.0, 0.0);
        let m = c.move_by(&l, -50.0, 200.0);
        assert!((m.pos().y - (540.0 + 200.0) / 1080.0).abs() < 1e-6, "{m:?}");
    }

    #[test]
    fn cursor_scale_multiplies_the_delta() {
        let mut l = GlobalLayout::new();
        l.insert(place(gid(1, 0), 0, 0, 1000, 1000, 2.0));
        let mut c = cursor_on(&l, gid(1, 0));
        let before = c.global_position().x;
        c.move_by(&l, 100.0, 0.0);
        assert!(
            (c.global_position().x - before - 200.0).abs() < 1e-9,
            "scale 2.0 should double the distance, moved {}",
            c.global_position().x - before
        );
    }

    #[test]
    fn hostile_cursor_scale_falls_back_to_unscaled() {
        // Zero would freeze the pointer with no way to recover by mouse; NaN and
        // a negative scale are worse.
        for bad in [0.0f32, -1.5, f32::NAN, f32::INFINITY] {
            let mut l = GlobalLayout::new();
            l.insert(place(gid(1, 0), 0, 0, 1000, 1000, bad));
            let mut c = cursor_on(&l, gid(1, 0));
            let before = c.global_position().x;
            c.move_by(&l, 100.0, 0.0);
            assert!(
                (c.global_position().x - before - 100.0).abs() < 1e-9,
                "scale {bad} did not degrade to 1.0"
            );
        }
    }

    #[test]
    fn slow_diagonal_drag_does_not_drift_off_axis() {
        // With a fractional scale, per-step rounding would either lose the
        // motion entirely or advance the axes at different times, bending a
        // 45-degree drag. The accumulator must keep both axes exact.
        let mut l = GlobalLayout::new();
        l.insert(place(gid(1, 0), 0, 0, 4000, 4000, 0.3));
        let mut c = cursor_on(&l, gid(1, 0));
        let start = c.global_position();
        for _ in 0..100 {
            c.move_by(&l, 1.0, 1.0);
        }
        let dx = c.global_position().x - start.x;
        let dy = c.global_position().y - start.y;
        assert_eq!(dx, dy, "diagonal drag bent off-axis: {dx} vs {dy}");
        assert!(
            (dx - 30.0).abs() < 1.0,
            "100 deltas at scale 0.3 should travel ~30 units, travelled {dx}"
        );
    }

    #[test]
    fn asymmetric_scale_still_tracks_both_axes_exactly() {
        let mut l = GlobalLayout::new();
        l.insert(place(gid(1, 0), 0, 0, 4000, 4000, 1.4));
        let mut c = cursor_on(&l, gid(1, 0));
        let start = c.global_position();
        for _ in 0..50 {
            c.move_by(&l, 3.0, 1.0);
        }
        let dx = c.global_position().x - start.x;
        let dy = c.global_position().y - start.y;
        // At most the one unit still banked in the accumulator may be missing.
        assert!((dx - 210.0).abs() <= 1.0, "x travelled {dx}, wanted ~210");
        assert!((dy - 70.0).abs() <= 1.0, "y travelled {dy}, wanted ~70");
    }

    #[test]
    fn sub_unit_deltas_accumulate_instead_of_vanishing() {
        let mut l = GlobalLayout::new();
        l.insert(place(gid(1, 0), 0, 0, 1000, 1000, 1.0));
        let mut c = cursor_on(&l, gid(1, 0));
        let start = c.global_position().x;
        for _ in 0..3 {
            // Each step alone truncates to zero.
            c.move_by(&l, 0.3, 0.0);
            assert_eq!(c.global_position().x, start);
        }
        c.move_by(&l, 0.3, 0.0);
        assert_eq!(c.global_position().x - start, 1.0, "remainder never spent");
    }

    #[test]
    fn remainder_does_not_survive_a_transition() {
        // Banked fractions belong to the monitor whose scale produced them;
        // carrying them across would jerk the first step on the new screen.
        let l = side_by_side();
        let mut c = cursor_on(&l, gid(1, 0));
        c.move_by(&l, 0.0, 0.9); // banked, no whole unit spent
        let crossing = c.move_by(&l, 2000.0, 0.0);
        assert!(crossing.is_transition());
        let entry_y = c.global_position().y;
        c.move_by(&l, 0.0, 0.2);
        assert_eq!(
            c.global_position().y,
            entry_y,
            "stale remainder leaked across the seam"
        );
    }

    #[test]
    fn nan_and_infinite_deltas_leave_the_position_finite() {
        let l = side_by_side();
        let mut c = cursor_on(&l, gid(1, 0));
        let before = c.global_position();
        for (dx, dy) in [
            (f64::NAN, 0.0),
            (0.0, f64::NAN),
            (f64::INFINITY, f64::NEG_INFINITY),
        ] {
            c.move_by(&l, dx, dy);
            assert!(
                c.global_position().x.is_finite() && c.global_position().y.is_finite(),
                "hostile delta ({dx}, {dy}) escaped sanitizing"
            );
        }
        assert_eq!(
            c.global_position(),
            before,
            "hostile delta moved the cursor"
        );
        // And the accumulator is still usable afterwards.
        c.move_by(&l, 10.0, 0.0);
        assert_eq!(c.global_position().x - before.x, 10.0);
    }

    #[test]
    fn a_hostile_delta_does_not_poison_later_motion() {
        let mut l = GlobalLayout::new();
        l.insert(place(gid(1, 0), 0, 0, 1000, 1000, 1.0));
        let mut c = cursor_on(&l, gid(1, 0));
        c.move_by(&l, f64::MAX, f64::MAX);
        let after_clamp = c.global_position();
        assert!(after_clamp.x.is_finite() && after_clamp.y.is_finite());
        c.move_by(&l, -100.0, 0.0);
        assert!((after_clamp.x - c.global_position().x - 100.0).abs() < 1e-9);
    }

    #[test]
    fn lock_refuses_transitions_and_reports_blocked() {
        let l = side_by_side();
        let mut c = cursor_on(&l, gid(1, 0));
        c.set_locked(true);
        let m = c.move_by(&l, 5000.0, 0.0);
        assert!(matches!(m, CursorMotion::Blocked { .. }), "{m:?}");
        assert_eq!(c.monitor(), gid(1, 0), "lock let the cursor escape");
        assert!(m.pos().x <= 1.0 && m.pos().x > 0.99, "{m:?}");
    }

    #[test]
    fn unlocking_restores_crossing() {
        let l = side_by_side();
        let mut c = cursor_on(&l, gid(1, 0));
        c.set_locked(true);
        c.move_by(&l, 5000.0, 0.0);
        assert!(!c.toggle_lock(), "toggle should have released the lock");
        assert!(c.move_by(&l, 50.0, 0.0).is_transition());
        assert_eq!(c.monitor(), gid(2, 0));
    }

    #[test]
    fn lock_still_allows_movement_inside_the_monitor() {
        let l = side_by_side();
        let mut c = cursor_on(&l, gid(1, 0));
        c.set_locked(true);
        let m = c.move_by(&l, 100.0, 100.0);
        assert!(matches!(m, CursorMotion::Moved { .. }), "{m:?}");
    }

    #[test]
    fn warp_lands_on_the_named_monitor() {
        let l = side_by_side();
        let mut c = cursor_on(&l, gid(1, 0));
        let landed = c
            .warp_to(&l, gid(2, 0), NormPos::new(0.25, 0.75))
            .expect("monitor exists");
        assert_eq!(c.monitor(), gid(2, 0));
        assert!((landed.x - 0.25).abs() < 1e-6, "{landed:?}");
        assert!((landed.y - 0.75).abs() < 1e-6, "{landed:?}");
    }

    #[test]
    fn warp_survives_hostile_entry_coordinates() {
        // Entry arrives off the wire from a peer.
        let l = side_by_side();
        let mut c = cursor_on(&l, gid(1, 0));
        c.warp_to(&l, gid(2, 0), NormPos::new(f64::NAN, 99.0))
            .unwrap();
        let rect = l.rect(gid(2, 0)).unwrap();
        assert!(
            rect.contains(c.global_position()),
            "cursor escaped the destination: {:?}",
            c.global_position()
        );
    }

    #[test]
    fn warp_is_allowed_while_locked_and_keeps_the_lock() {
        // An inbound handoff is explicit, unlike sliding off an edge by accident.
        let l = side_by_side();
        let mut c = cursor_on(&l, gid(1, 0));
        c.set_locked(true);
        c.warp_to(&l, gid(2, 0), NormPos::new(0.5, 0.5)).unwrap();
        assert_eq!(c.monitor(), gid(2, 0));
        assert!(c.is_locked(), "the lock should outlive a handoff");
    }

    #[test]
    fn warp_to_an_unknown_or_degenerate_monitor_is_refused() {
        let mut l = side_by_side();
        l.insert(place(gid(3, 0), 5000, 0, 0, 0, 1.0));
        let mut c = cursor_on(&l, gid(1, 0));
        assert_eq!(
            c.warp_to(&l, gid(9, 9), NormPos::new(0.5, 0.5)),
            Err(WarpError::UnknownMonitor(gid(9, 9)))
        );
        assert_eq!(
            c.warp_to(&l, gid(3, 0), NormPos::new(0.5, 0.5)),
            Err(WarpError::DegenerateMonitor(gid(3, 0)))
        );
        assert_eq!(c.monitor(), gid(1, 0), "a refused warp still moved it");
    }

    #[test]
    fn motion_on_an_unplugged_monitor_reports_the_last_known_position() {
        // A display can disappear between two motion events.
        let mut l = side_by_side();
        let mut c = cursor_on(&l, gid(1, 0));
        let known = c.move_by(&l, 100.0, 0.0).pos();
        l.remove(gid(1, 0));
        let m = c.move_by(&l, 100.0, 0.0);
        match m {
            CursorMotion::Blocked { monitor, pos } => {
                assert_eq!(monitor, gid(1, 0));
                assert_eq!(pos, known);
            }
            other => panic!("expected blocked, got {other:?}"),
        }
        assert_eq!(c.norm_position(&l), known);
    }

    #[test]
    fn a_zero_extent_monitor_never_holds_a_moving_cursor() {
        let mut l = side_by_side();
        let mut c = cursor_on(&l, gid(1, 0));
        // Resize the holding monitor out of existence under the cursor.
        l.insert(place(gid(1, 0), 0, 0, 0, 0, 1.0));
        let m = c.move_by(&l, 10.0, 10.0);
        assert!(matches!(m, CursorMotion::Blocked { .. }), "{m:?}");
        assert!(m.pos().x.is_finite() && m.pos().y.is_finite());
    }

    #[test]
    fn transitions_are_reversible_at_the_same_height() {
        // Crossing out and straight back must return to the original screen,
        // otherwise the seam behaves like a one-way door.
        let l = side_by_side();
        let mut c = cursor_on(&l, gid(1, 0));
        c.move_by(&l, 2000.0, 0.0);
        assert_eq!(c.monitor(), gid(2, 0));
        let back = c.move_by(&l, -50.0, 0.0);
        assert!(back.is_transition(), "{back:?}");
        assert_eq!(c.monitor(), gid(1, 0));
        assert!((c.norm_position(&l).y - 0.5).abs() < 0.01);
    }

    #[test]
    fn split_edges_route_by_height_with_no_extra_state() {
        let mut l = GlobalLayout::new();
        l.insert(place(gid(1, 0), 0, 0, 2560, 1440, 1.0));
        l.insert(place(gid(2, 0), 2560, 0, 1280, 720, 1.0));
        l.insert(place(gid(3, 0), 2560, 720, 1280, 720, 1.0));

        let mut high = VirtualCursor::at(&l, gid(1, 0), NormPos::new(0.9, 0.1)).unwrap();
        high.move_by(&l, 5000.0, 0.0);
        assert_eq!(high.monitor(), gid(2, 0));

        let mut low = VirtualCursor::at(&l, gid(1, 0), NormPos::new(0.9, 0.9)).unwrap();
        low.move_by(&l, 5000.0, 0.0);
        assert_eq!(low.monitor(), gid(3, 0));
    }

    #[test]
    fn zero_delta_is_a_move_that_changes_nothing() {
        let l = side_by_side();
        let mut c = cursor_on(&l, gid(1, 0));
        let before = c.global_position();
        let m = c.move_by(&l, 0.0, 0.0);
        assert!(matches!(m, CursorMotion::Moved { .. }), "{m:?}");
        assert_eq!(c.global_position(), before);
    }

    #[test]
    fn anywhere_skips_degenerate_monitors() {
        let mut l = GlobalLayout::new();
        l.insert(place(gid(1, 0), 0, 0, 0, 0, 1.0));
        l.insert(place(gid(2, 0), 0, 0, 800, 600, 1.0));
        assert_eq!(
            VirtualCursor::anywhere(&l).map(|c| c.monitor()),
            Some(gid(2, 0))
        );
        assert!(VirtualCursor::anywhere(&GlobalLayout::new()).is_none());
    }
}
