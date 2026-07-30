// What the window says about who has the keyboard and mouse.
//
// Asserted on the words, not on the shape: the whole point of this module is that
// a user glancing at either machine reads a true sentence, and a test that only
// checked a `tone` string would pass while the copy said the opposite. So each
// case pins the text that reaches the screen.

import { describe, expect, it } from "vitest";
import { cursorStateFor } from "./cursor.js";

const LOCAL = "aa".repeat(16);
const REMOTE = "bb".repeat(16);

function snapshot({ cursor, peers = [] }) {
  return {
    nodeId: LOCAL,
    nodeName: "cowen-workhorse",
    cursor,
    peers,
  };
}

function peer(state, { node = REMOTE, name = "cowen-ubuntu" } = {}) {
  return { node, name, status: { state } };
}

const HERE = { owner: LOCAL, ownerName: "cowen-workhorse", monitor: 0, locked: false, local: true };
const THERE = { owner: REMOTE, ownerName: "cowen-ubuntu", monitor: 1, locked: false, local: false };

describe("the machine that has the cursor", () => {
  it("says so about itself in the first person", () => {
    const state = cursorStateFor(snapshot({ cursor: HERE }));
    expect(state.badge).toBe("Cursor here");
    expect(state.headline).toBe("This machine has the keyboard and mouse.");
    expect(state.detail).toBe("The cursor is on display 0.");
    expect(state.tone).toBe("good");
    expect(state.certain).toBe(true);
  });

  it("names the other machine when the other machine has it", () => {
    const state = cursorStateFor(
      snapshot({ cursor: THERE, peers: [peer("connected")] })
    );
    expect(state.badge).toBe("Cursor on cowen-ubuntu");
    expect(state.headline).toBe("cowen-ubuntu has the keyboard and mouse.");
    expect(state.detail).toBe("The cursor is on its display 1.");
    expect(state.certain).toBe(true);
    // Coloured by the owner, not by whoever is looking.
    expect(state.node).toBe(REMOTE);
  });

  it("falls back to the short node id when the owner has no name yet", () => {
    const state = cursorStateFor(
      snapshot({
        cursor: { ...THERE, ownerName: "" },
        peers: [peer("connected")],
      })
    );
    expect(state.badge).toBe(`Cursor on ${REMOTE.slice(0, 8)}`);
  });

  // The machine the captain will look at first: `cowen-workhorse` advertises no
  // displays today, so the cursor is always on the other machine there, and the
  // sentence it reads has to be about the other machine without ever implying
  // that machine is in charge of it.
  it("never calls either machine a master or a slave", () => {
    const words = /\b(master|slave)\b/i;
    for (const state of [
      cursorStateFor(snapshot({ cursor: HERE })),
      cursorStateFor(snapshot({ cursor: { ...HERE, locked: true } })),
      cursorStateFor(snapshot({ cursor: THERE, peers: [peer("connected")] })),
      cursorStateFor(snapshot({ cursor: THERE, peers: [peer("offline")] })),
      cursorStateFor(snapshot({ cursor: THERE })),
      cursorStateFor(snapshot({ cursor: null })),
    ]) {
      expect(`${state.badge} ${state.headline} ${state.detail}`).not.toMatch(words);
    }
  });
});

describe("the lock", () => {
  it("is distinguishable from ordinary ownership, and says why the cursor will not move", () => {
    const state = cursorStateFor(snapshot({ cursor: { ...HERE, locked: true } }));
    expect(state.badge).toBe("Cursor locked here");
    expect(state.tone).toBe("busy");
    expect(state.tone).not.toBe(cursorStateFor(snapshot({ cursor: HERE })).tone);
    expect(state.detail).toContain("locked to this machine");
    expect(state.detail).toContain("will not cross an edge");
  });

  // `locked` is this machine's own router flag and does not cross the wire, so
  // the sentence stays a statement about this machine's flag — but the flag is
  // in effect now, and it is what is keeping the cursor on the other machine:
  // `locking_keeps_input_flowing_to_the_peer_that_already_owns_it` in
  // `crates/wx-core/src/router.rs` pins that. Promising the cursor would come
  // back was the exact opposite of what the engine does.
  it("says the lock is what is keeping the cursor on the other machine", () => {
    const state = cursorStateFor(
      snapshot({ cursor: { ...THERE, locked: true }, peers: [peer("connected")] })
    );
    expect(state.headline).toBe("cowen-ubuntu has the keyboard and mouse.");
    expect(state.badge).toBe("Cursor on cowen-ubuntu");
    expect(state.detail).toContain("has the cursor locked");
    expect(state.detail).toContain("keeping it there");
    expect(state.detail).toContain("will not come back until you unlock it");
    // The old copy said the lock was dormant and the cursor could still arrive.
    expect(state.detail).not.toMatch(/arrives back|comes back here|once it returns/);
  });
});

describe("an answer that is not current fact", () => {
  it("hedges when the owning machine can no longer be reached", () => {
    for (const [state, why] of [
      ["offline", "offline"],
      ["connecting", "still connecting"],
      ["discovered", "visible but not connected"],
      ["pairing", "still pairing"],
      ["failed", "not answering"],
    ]) {
      const shown = cursorStateFor(
        snapshot({ cursor: THERE, peers: [peer(state)] })
      );
      expect(shown.certain).toBe(false);
      expect(shown.tone).toBe("bad");
      expect(shown.badge).toBe("Cursor was on cowen-ubuntu");
      expect(shown.headline).toContain("last had the keyboard and mouse");
      expect(shown.headline).toContain("can no longer reach it");
      expect(shown.detail).toContain(`That machine is ${why}`);
      // The present tense is what would make this a lie.
      expect(shown.headline).not.toContain("has the keyboard and mouse.");
    }
  });

  it("does not put a transport error in the sentence a person reads", () => {
    const broken = peer("failed");
    broken.status.error = "reading from a stream: connection lost";
    const state = cursorStateFor(snapshot({ cursor: THERE, peers: [broken] }));
    expect(`${state.badge} ${state.headline} ${state.detail}`).not.toContain("stream");
  });

  it("hedges when the owning machine is not a machine this one knows at all", () => {
    const state = cursorStateFor(snapshot({ cursor: THERE, peers: [] }));
    expect(state.certain).toBe(false);
    expect(state.tone).toBe("bad");
    expect(state.headline).toContain("last had the keyboard and mouse");
    expect(state.detail).toBe("Where the cursor is now cannot be told from here.");
  });

  it("says ownership is unknown rather than guessing at one", () => {
    for (const cursor of [null, undefined, { owner: "", ownerName: "" }, {}]) {
      const state = cursorStateFor(snapshot({ cursor }));
      expect(state.certain).toBe(false);
      expect(state.node).toBeNull();
      expect(state.badge).toBe("Cursor: not known");
      expect(state.headline).toBe(
        "Which machine has the keyboard and mouse is not known yet."
      );
    }
  });

  // Not the same as unknown: with no layout the cursor is unambiguously on the
  // machine you are looking at, and hedging there would teach the user to
  // distrust the badge in the one state it is certainly right about.
  it("still states plainly that the cursor is here when no layout has placed it", () => {
    const state = cursorStateFor(
      snapshot({ cursor: { ...HERE, monitor: null } })
    );
    expect(state.certain).toBe(true);
    expect(state.tone).toBe("good");
    expect(state.headline).toBe("This machine has the keyboard and mouse.");
    expect(state.detail).toBe(
      "No shared layout places it yet, so there is nowhere else for it to go."
    );
  });

  it("says nothing at all when there is no snapshot to say it from", () => {
    // A detached window knows nothing about the cursor; the banner is already
    // saying why. A badge left behind would name the last machine it saw.
    expect(cursorStateFor(null)).toBeNull();
    expect(cursorStateFor(undefined)).toBeNull();
  });
});
