import { describe, expect, it } from "vitest";
import { capabilitiesText, capabilityLabels, displaysText, droppedText } from "./format.js";

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
    // A peer that has been discovered but never connected, so nothing it can do is
    // known either way.
    expect(capabilityLabels(0)).toEqual([]);
    expect(capabilitiesText(0)).toBe("Nothing reported yet");
    expect(capabilitiesText(undefined)).toBe("Nothing reported yet");
    expect(capabilitiesText(0, false)).toBe("Nothing reported yet");
  });

  it("separates a machine that has not answered from one that reports nothing", () => {
    // A connected peer advertising no bits has answered the question, and the
    // answer is "nothing" — which a macOS or headless peer in this build gives.
    // Showing it as though it had never reported hides a real, actionable fact.
    expect(capabilitiesText(0, true)).toBe("Reports it can do nothing");
    expect(capabilitiesText(undefined, true)).toBe("Reports it can do nothing");
    expect(capabilitiesText(0, true)).not.toBe(capabilitiesText(0, false));
  });

  it("names the bits the same way whether or not the machine is connected", () => {
    // Connection state only decides the wording of the empty case; it must not
    // change what a machine that did claim something is said to be able to do.
    expect(capabilitiesText(CAPTURE_INPUT | INJECT_INPUT, true)).toBe(
      capabilitiesText(CAPTURE_INPUT | INJECT_INPUT, false)
    );
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

describe("what a machine's screens are", () => {
  const screens = [
    { name: "DP-1", w: 3440, h: 1440, primary: true },
    { name: "HDMI-1", w: 1920, h: 1080, primary: false },
  ];

  it("lists the screens it was told about", () => {
    expect(displaysText(screens, null)).toBe("DP-1 3440x1440 (primary), HDMI-1 1920x1080");
  });

  it("says none only when the agent really found none", () => {
    expect(displaysText([], null)).toBe("none reported");
    expect(displaysText([], undefined)).toBe("none reported");
  });

  it("does not read a failed enumeration as a machine with no screens", () => {
    // The whole reason `displaysError` exists. Collapsing these two is what had a
    // desktop with a 3440x1440 monitor attached describing itself as headless, and
    // the row the user reads must not repeat the mistake the CLI stopped making.
    const failed = displaysText([], "no display server available");
    expect(failed).not.toBe(displaysText([], null));
    expect(failed).toContain("no display server available");
  });

  it("still shows the last screens it knew when a poll fails", () => {
    // The agent deliberately keeps the last list rather than dropping the machine
    // out of every layout over one bad poll, so the list is real — but it is the
    // last answer that worked, and saying so is what stops it being read as current.
    const stale = displaysText(screens, "asking randr for the current screen resources failed");
    expect(stale).toContain("DP-1 3440x1440 (primary)");
    expect(stale).toContain("last known");
    expect(stale).toContain("asking randr for the current screen resources failed");
  });

  it("reads an agent that predates the field exactly as it always did", () => {
    // `displaysError` is serde-default, so an older agent simply omits it.
    expect(displaysText(screens, undefined)).toBe(displaysText(screens, null));
    expect(displaysText(undefined, undefined)).toBe("none reported");
  });
});

describe("input a machine could not keep up with", () => {
  it("distinguishes loss happening now from loss that happened at some point", () => {
    // The reason the agent sends a window count beside the total at all. Someone
    // watching a live session needs "is this happening as I look at it", and a
    // monotonic counter answers only "did it ever happen".
    const live = droppedText({ total: 48, recent: 12, windowMs: 2000 });
    expect(live).toContain("12 in the last 2.0s");
    expect(live).toContain("48 this session");

    const over = droppedText({ total: 48, recent: 0, windowMs: 2000 });
    expect(over).toBe("48 this session");
    expect(over).not.toContain("last");
  });

  it("separates a peer with nothing dropped from a peer with no session", () => {
    // Zero drops on a live connection and no connection to have dropped anything
    // on are different facts, and the counter is absent in the second case.
    expect(droppedText({ total: 0, recent: 0, windowMs: 2000 })).toBe("none");
    expect(droppedText(undefined)).toBe("no session");
    expect(droppedText(null)).toBe("no session");
  });

  it("does not claim the network lost anything", () => {
    // This counts input that arrived and was thrown away because the local queue
    // was full; a datagram the wire dropped never reaches the counter. The two have
    // opposite causes and opposite fixes, so the wording must not merge them.
    const text = droppedText({ total: 48, recent: 12, windowMs: 2000 });
    expect(text).not.toContain("lost");
    expect(text).not.toContain("packet");
  });

  it("does not invent a rate when the agent did not send the window", () => {
    // An agent older than this UI omits the field entirely; showing the count
    // without a window beats showing a per-second figure that was never measured.
    const text = droppedText({ total: 5, recent: 5 });
    expect(text).toContain("5 just now");
    expect(text).not.toContain("in the last");
  });
});
