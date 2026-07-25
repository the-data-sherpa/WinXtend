import { describe, expect, it } from "vitest";
import { boundsOf, fitTransform, nextFreeSpot, snap, toScreen } from "./snap.js";

const rect = (x, y, w, h) => ({ x, y, w, h });

describe("snapping a dragged screen", () => {
  it("butts a screen against the right edge of its neighbour", () => {
    // Dropped four units short of touching: the seam has to close, because a gap of
    // four global units is a gap the cursor falls into and never crosses.
    const result = snap(rect(1916, 0, 1920, 1080), [rect(0, 0, 1920, 1080)], 40);
    expect(result.x).toBe(1920);
    expect(result.y).toBe(0);
  });

  it("snaps each axis independently so a deliberate offset survives", () => {
    // Placed below and to the right: the vertical seam should close while the chosen
    // horizontal offset is left exactly as dragged.
    const result = snap(rect(700, 1075, 1280, 800), [rect(0, 0, 1920, 1080)], 20);
    expect(result.y).toBe(1080);
    expect(result.x).toBe(700);
  });

  it("aligns edges as well as closing seams", () => {
    // Tops line up: the dragged screen's y should land on the neighbour's y.
    const result = snap(rect(1920, 6, 1280, 800), [rect(0, 0, 1920, 1080)], 20);
    expect(result.y).toBe(0);
  });

  it("leaves a screen alone when nothing is close enough", () => {
    const moving = rect(5000, 5000, 1920, 1080);
    const result = snap(moving, [rect(0, 0, 1920, 1080)], 20);
    expect(result).toMatchObject({ x: 5000, y: 5000 });
    expect(result.guides).toHaveLength(0);
  });

  it("takes the nearest candidate when two are in range", () => {
    // Two neighbours whose edges are 8 and 3 away. Picking the far one would feel
    // like the editor fighting the drag.
    const result = snap(rect(1000, 0, 100, 100), [rect(0, 0, 992, 100), rect(1003, 0, 100, 100)], 20);
    expect(result.x).toBe(1003 - 100 + 100); // aligned with the near neighbour's start
  });

  it("reports the guide it lined up with so the seam can be drawn", () => {
    const result = snap(rect(1918, 0, 1920, 1080), [rect(0, 0, 1920, 1080)], 40);
    expect(result.guides).toEqual([
      { axis: "x", at: 1920 },
      { axis: "y", at: 0 },
    ]);
  });

  it("never produces a NaN position from hostile input", () => {
    // Placements arrive from the agent, which reads them from a config file a human
    // may have edited. One bad number must not poison the whole canvas.
    const result = snap(rect(10, 10, 100, 100), [rect(Number.NaN, 0, 100, 100)], 20);
    expect(result.x).toBe(10);
    expect(result.y).toBe(10);

    const broken = snap({ x: undefined, y: 0, w: 100, h: 100 }, [rect(0, 0, 10, 10)], 20);
    expect(Number.isNaN(broken.x)).toBe(false);
  });

  it("does not snap at all when the tolerance is zero or nonsense", () => {
    for (const tolerance of [0, -5, Number.NaN, undefined]) {
      expect(snap(rect(1919, 0, 100, 100), [rect(0, 0, 1920, 100)], tolerance).x).toBe(1919);
    }
  });

  it("snaps a zero-width screen rather than ignoring it", () => {
    // A screen dragged to zero width is already a warning, but it still has to be
    // movable: refusing to snap it would strand it where the user cannot line it up.
    const result = snap(rect(1918, 0, 0, 1080), [rect(0, 0, 1920, 1080)], 20);
    expect(result.x).toBe(1920);
  });
});

describe("fitting the global desktop into the canvas", () => {
  it("covers every screen in the arrangement", () => {
    const rects = [rect(0, 0, 1920, 1080), rect(1920, -200, 2560, 1440)];
    const bounds = boundsOf(rects);
    // Top edge comes from the second screen at -200, bottom from its own 1240.
    expect(bounds).toEqual({ x: 0, y: -200, w: 4480, h: 1440 });

    const view = fitTransform(bounds, { w: 800, h: 460 }, 24);
    for (const r of rects) {
      const topLeft = toScreen(view, r.x, r.y);
      const bottomRight = toScreen(view, r.x + r.w, r.y + r.h);
      expect(topLeft.x).toBeGreaterThanOrEqual(0);
      expect(topLeft.y).toBeGreaterThanOrEqual(0);
      expect(bottomRight.x).toBeLessThanOrEqual(800);
      expect(bottomRight.y).toBeLessThanOrEqual(460);
    }
  });

  it("survives a bounding box with no area instead of dividing by zero", () => {
    // One screen dragged to zero width and height, which the editor allows and warns
    // about; the canvas still has to render.
    const view = fitTransform(boundsOf([rect(100, 100, 0, 0)]), { w: 600, h: 400 }, 24);
    expect(Number.isFinite(view.scale)).toBe(true);
    expect(view.scale).toBeGreaterThan(0);
    expect(Number.isFinite(view.offsetX)).toBe(true);
  });

  it("survives an empty layout and a zero-size canvas", () => {
    expect(boundsOf([])).toBeNull();
    const view = fitTransform(null, { w: 0, h: 0 }, 24);
    expect(Number.isFinite(view.scale)).toBe(true);
    expect(view.scale).toBeGreaterThan(0);
  });

  it("clamps the scale so one distant screen cannot shrink the rest to nothing", () => {
    // A misconfigured layout with a screen 40000 units away used to make every real
    // screen a sub-pixel sliver with nothing left to grab.
    const view = fitTransform(
      boundsOf([rect(0, 0, 1920, 1080), rect(400000, 0, 1920, 1080)]),
      { w: 800, h: 460 },
      24
    );
    const width = 1920 * view.scale;
    expect(width).toBeGreaterThan(1);
  });

  it("ignores unusable rectangles when measuring", () => {
    const bounds = boundsOf([rect(0, 0, 100, 100), { x: "left", y: 0, w: 10, h: 10 }, null]);
    expect(bounds).toEqual({ x: 0, y: 0, w: 100, h: 100 });
  });
});

describe("placing a screen that has never been placed", () => {
  it("puts it to the right of everything, vertically centred", () => {
    // The same rule the agent's autolayout uses, so doing it by hand and letting the
    // agent do it do not produce two different arrangements.
    const spot = nextFreeSpot([rect(0, 0, 1920, 1080)], { w: 1280, h: 800 });
    expect(spot.x).toBe(1920);
    expect(spot.y).toBe(140);
  });

  it("starts at the origin when there is nothing to sit beside", () => {
    expect(nextFreeSpot([], { w: 1920, h: 1080 })).toEqual({ x: 0, y: 0 });
  });
});
