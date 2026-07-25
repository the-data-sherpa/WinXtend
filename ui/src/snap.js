// Geometry for the layout editor: fitting the global desktop into the canvas, and
// snapping a dragged screen to its neighbours' edges.
//
// Pure functions with no DOM access, because this is the part of the editor that is
// easy to get subtly wrong — a snap that fires at the wrong distance, a fit that
// divides by zero when the mesh has one zero-width screen in it — and the only way
// to pin that down is to test it directly.
//
// Coordinates here are the global virtual desktop's, the same integers the agent
// stores in a Placement. Screen pixels only appear at the transform boundary.

/// Smallest scale worth rendering at, in screen pixels per global pixel.
//
// A mesh with one screen 40000 units away (a mistake, but a possible one) would
// otherwise shrink every real screen to a sub-pixel sliver and the user would have
// nothing to grab. Clamping instead means the far screen is off-canvas and the
// warning about it is readable, which is the recoverable state.
export const MIN_SCALE = 0.01;
export const MAX_SCALE = 0.5;

/// How close, in screen pixels, an edge has to be before it snaps.
export const SNAP_PIXELS = 10;

function finite(...values) {
  return values.every((v) => typeof v === "number" && Number.isFinite(v));
}

/// True for a rectangle that can be drawn and dragged.
export function usable(rect) {
  return Boolean(rect) && finite(rect.x, rect.y, rect.w, rect.h);
}

/// Bounding box of every usable rectangle, or null when there is nothing to draw.
export function boundsOf(rects) {
  let minX = Infinity;
  let minY = Infinity;
  let maxX = -Infinity;
  let maxY = -Infinity;
  for (const r of rects || []) {
    if (!usable(r)) continue;
    minX = Math.min(minX, r.x);
    minY = Math.min(minY, r.y);
    maxX = Math.max(maxX, r.x + r.w);
    maxY = Math.max(maxY, r.y + r.h);
  }
  if (!finite(minX, minY, maxX, maxY)) return null;
  return { x: minX, y: minY, w: maxX - minX, h: maxY - minY };
}

/// Transform from global coordinates to canvas pixels.
///
/// `pad` is in screen pixels and is subtracted from the viewport before fitting, so
/// a screen flush against the bounding box still has room for its border and label.
export function fitTransform(bounds, viewport, pad = 24) {
  const vw = Math.max(1, (viewport?.w || 0) - pad * 2);
  const vh = Math.max(1, (viewport?.h || 0) - pad * 2);
  if (!bounds || !finite(bounds.w, bounds.h)) {
    return { scale: 0.1, offsetX: pad, offsetY: pad };
  }
  // A zero-size bounding box happens with a single degenerate screen; treat the
  // missing dimension as one unit rather than dividing by it.
  const w = bounds.w > 0 ? bounds.w : 1;
  const h = bounds.h > 0 ? bounds.h : 1;
  const scale = Math.min(MAX_SCALE, Math.max(MIN_SCALE, Math.min(vw / w, vh / h)));
  // Centre whatever is left over, so a wide arrangement is not pinned to the top.
  const offsetX = pad + (vw - bounds.w * scale) / 2 - bounds.x * scale;
  const offsetY = pad + (vh - bounds.h * scale) / 2 - bounds.y * scale;
  return { scale, offsetX, offsetY };
}

export function toScreen(view, gx, gy) {
  return { x: gx * view.scale + view.offsetX, y: gy * view.scale + view.offsetY };
}

/// One axis of snapping.
///
/// Returns the snapped start coordinate and the edge the snap lined up with, or
/// null when nothing was near enough. Split out per axis because the axes are
/// independent: a screen dragged to sit under its neighbour should snap vertically
/// to the neighbour's bottom edge while keeping whatever horizontal offset the user
/// chose, and a combined "nearest position" search would fight that.
function snapAxis(start, size, others, tolerance) {
  let best = null;
  const consider = (candidate, edge) => {
    if (!finite(candidate)) return;
    const delta = Math.abs(candidate - start);
    if (delta > tolerance) return;
    // Strictly nearer, so that when two candidates tie the first one wins and the
    // result does not depend on the iteration order of the layout.
    if (best === null || delta < best.delta) best = { value: candidate, edge, delta };
  };
  for (const o of others) {
    consider(o.end, o.end); // butt this rectangle's start against their end
    consider(o.start - size, o.start); // butt its end against their start
    consider(o.start, o.start); // align starts
    consider(o.end - size, o.end); // align ends
  }
  return best;
}

/// Snap a dragged rectangle to the edges of the others.
///
/// `tolerance` is in global units — the caller divides its pixel threshold by the
/// current scale, so snapping feels the same however far the view is zoomed out.
/// Returns the position to use plus the guide lines that were matched.
export function snap(moving, others, tolerance) {
  if (!usable(moving)) return { x: moving?.x, y: moving?.y, guides: [] };
  const limit = finite(tolerance) && tolerance > 0 ? tolerance : 0;
  const usableOthers = (others || []).filter(usable);

  const x = snapAxis(
    moving.x,
    moving.w,
    usableOthers.map((o) => ({ start: o.x, end: o.x + o.w })),
    limit
  );
  const y = snapAxis(
    moving.y,
    moving.h,
    usableOthers.map((o) => ({ start: o.y, end: o.y + o.h })),
    limit
  );

  const guides = [];
  if (x) guides.push({ axis: "x", at: x.edge });
  if (y) guides.push({ axis: "y", at: y.edge });
  return {
    x: x ? x.value : moving.x,
    y: y ? y.value : moving.y,
    guides,
  };
}

/// Where an unplaced screen goes: to the right of everything, vertically centred.
///
/// The same rule the agent's autolayout uses, so dropping a new screen in by hand
/// and letting the agent place it produce the same arrangement rather than two
/// different ones the user has to reconcile.
export function nextFreeSpot(existing, size) {
  const bounds = boundsOf(existing);
  if (!bounds) return { x: 0, y: 0 };
  const h = finite(size?.h) ? size.h : 0;
  return {
    x: bounds.x + bounds.w,
    y: Math.round(bounds.y + (bounds.h - h) / 2),
  };
}
