import { describe, expect, it } from "vitest";
import { capabilitiesText, capabilityLabels } from "./format.js";

// Mirrors `Capabilities` in crates/wx-proto/src/caps.rs. Written out here rather
// than imported so that a bit renumbered on the Rust side fails this test instead
// of silently relabelling every machine in the list.
const CAPTURE_INPUT = 1 << 0;
const INJECT_INPUT = 1 << 1;
const HAS_DISPLAYS = 1 << 2;
const CLIPBOARD_TEXT = 1 << 3;
const FILE_TRANSFER = 1 << 5;
const SCREENSAVER_SYNC = 1 << 8;

describe("what a machine says it can do", () => {
  it("names every bit a machine claims", () => {
    const bits = CAPTURE_INPUT | INJECT_INPUT | HAS_DISPLAYS | CLIPBOARD_TEXT | SCREENSAVER_SYNC;
    expect(capabilityLabels(bits)).toEqual([
      "Captures input",
      "Accepts input",
      "Has displays",
      "Clipboard: text",
      "Locks with you",
    ]);
  });

  it("names nothing a machine has not claimed", () => {
    // The point of the row: a machine that cannot take a file must not look as
    // though it can, because the agent will refuse the attempt either way.
    expect(capabilityLabels(CAPTURE_INPUT | HAS_DISPLAYS)).not.toContain("File transfer");
    expect(capabilityLabels(FILE_TRANSFER)).toEqual(["File transfer"]);
  });

  it("says so when a machine has claimed nothing yet", () => {
    // A peer that has been discovered but never connected, which is not the same
    // as a machine that reports it can do nothing.
    expect(capabilityLabels(0)).toEqual([]);
    expect(capabilitiesText(0)).toBe("Nothing reported yet");
    expect(capabilitiesText(undefined)).toBe("Nothing reported yet");
  });

  it("reports a bit it has never heard of rather than hiding it", () => {
    // An agent newer than this UI claims something new. Dropping it would make the
    // machine look less capable than it is.
    expect(capabilityLabels(INJECT_INPUT | (1 << 20))).toEqual(["Accepts input", "bit 20"]);
  });

  it("survives the top bit, which is negative under a signed mask", () => {
    expect(capabilityLabels(2 ** 31)).toEqual(["bit 31"]);
    expect(capabilityLabels(0xffffffff)).toContain("Captures input");
    expect(capabilityLabels(0xffffffff)).toContain("bit 31");
  });

  it("joins the labels into one readable line", () => {
    expect(capabilitiesText(CAPTURE_INPUT | INJECT_INPUT)).toBe(
      "Captures input  ·  Accepts input"
    );
  });
});
