// Which machine has the keyboard and mouse: the decision as data, plus the one
// badge that renders it, kept together so the two screens that show it cannot
// drift apart in either the words or the markup.
//
// There is no machine in charge here and this file must not invent one: ownership
// belongs to whichever machine the user is currently driving from, and it moves.
// The words it produces say who *has* the cursor, never who commands whom.
//
// Everything below is derived from the snapshot the agent already sends —
// `status.cursor` (`owner`, `ownerName`, `monitor`, `local`, `locked`) and
// `status.peers[].status.state`. Nothing was added to `ipc::StatusSnapshot` for
// this, because nothing needed to be: the owner is already named and the owning
// peer's reachability is already in the peer list beside it.
//
// The rule that shapes the rest, and the reason this is a tested pure function
// rather than a branch inside a render: **an owner this machine cannot reach is
// not a report of where the cursor is.** `cursor.owner` is the last thing the
// engine settled on, and nothing revises it when a peer drops — no event says
// "the machine holding the cursor went away". So a window that renders the field
// as-is goes on stating, with a confident full stop, that a machine which has been
// off the network for an hour has the mouse. Every state below therefore carries
// `certain`, and the ones that are not certain say so in words as well.
//
// The second rule: `locked` is *this* machine's own router flag and does not
// cross the wire. It pins the virtual cursor to whatever monitor that cursor is
// on at the time, a peer's included — locking while another machine holds the
// cursor keeps it there rather than calling it back. What the flag cannot do is
// describe the peer: a machine that does not have the cursor knows only its own
// flag, so "that machine has the cursor locked" is a claim it cannot make.

import { h } from "./dom.js";
import { nodeColor, shortId } from "./format.js";

/// Why a peer that is not `connected` cannot be treated as holding the cursor.
///
/// Deliberately not `PeerStatusSpec::Failed`'s error string: that renders a
/// `wx_net::TransportError` — "reading from a stream: connection lost" — which
/// belongs in the log and not in a sentence someone reads.
const UNREACHABLE = {
  offline: "offline",
  connecting: "still connecting",
  discovered: "visible but not connected",
  pairing: "still pairing",
  failed: "not answering",
};

/// Describe cursor ownership from the whole snapshot.
///
/// Returns `null` when there is no snapshot — a window that is not attached knows
/// nothing about the cursor, and the connection banner is already saying so — or
/// an object with:
///
///   * `tone` — one of the pill tones the stylesheet already defines: `"good"`
///     when this machine has it, `"idle"` when another reachable machine does,
///     `"busy"` while it is locked here, `"bad"` when the answer is stale.
///   * `node` — the owning node id, for colouring its swatch, or `null` when
///     there is nobody to name.
///   * `badge` — the chrome's line. Short: it sits in the window header on every
///     tab, beside the machine's own name.
///   * `headline`, `detail` — the sentences the Status screen shows. `detail` may
///     be empty.
///   * `certain` — whether ownership is being asserted as current fact. False
///     means the copy is hedged and the caller must not present it as current.
export function cursorStateFor(status) {
  if (!status) return null;

  const cursor = status.cursor;
  if (!cursor || typeof cursor.owner !== "string" || !cursor.owner) {
    // A snapshot from an agent that has not settled ownership, or one from a
    // build that does not report it. Nothing is known, so nothing is claimed.
    return {
      tone: "idle",
      node: null,
      badge: "Cursor: not known",
      headline: "Which machine has the keyboard and mouse is not known yet.",
      detail: "The agent has not reported an owner. This will fill in as soon as it does.",
      certain: false,
    };
  }

  const name = cursor.ownerName || shortId(cursor.owner);
  const here = Boolean(cursor.local) || cursor.owner === status.nodeId;
  const where = Number.isInteger(cursor.monitor) ? `display ${cursor.monitor}` : null;

  if (here) {
    if (cursor.locked) {
      return {
        tone: "busy",
        node: cursor.owner,
        badge: "Cursor locked here",
        headline: "This machine has the keyboard and mouse, and is holding on to it.",
        detail: join(
          where ? `The cursor is on ${where}.` : null,
          "It is locked to this machine and will not cross an edge to another one until it is unlocked."
        ),
        certain: true,
      };
    }
    return {
      tone: "good",
      node: cursor.owner,
      badge: "Cursor here",
      headline: "This machine has the keyboard and mouse.",
      // No monitor means no shared layout has placed the cursor yet, which is
      // the ordinary state of a machine that has not been laid out with another
      // — not a fault, and not a reason to hedge: with nowhere to go, the cursor
      // is unambiguously here.
      detail: where
        ? `The cursor is on ${where}.`
        : "No shared layout places it yet, so there is nowhere else for it to go.",
      certain: true,
    };
  }

  const peer = (status.peers || []).find((p) => p.node === cursor.owner);

  if (!peer) {
    return {
      tone: "bad",
      node: cursor.owner,
      badge: `Cursor was on ${name}`,
      headline: `${name} last had the keyboard and mouse, and this machine no longer knows that machine.`,
      detail: "Where the cursor is now cannot be told from here.",
      certain: false,
    };
  }

  const state = peer.status?.state;
  if (state !== "connected") {
    return {
      tone: "bad",
      node: cursor.owner,
      badge: `Cursor was on ${name}`,
      headline: `${name} last had the keyboard and mouse, but this machine can no longer reach it.`,
      detail: `That machine is ${UNREACHABLE[state] || "not connected"}, so this is the last thing known rather than where the cursor is now.`,
      certain: false,
    };
  }

  return {
    tone: "idle",
    node: cursor.owner,
    badge: `Cursor on ${name}`,
    headline: `${name} has the keyboard and mouse.`,
    detail: join(
      where ? `The cursor is on its ${where}.` : null,
      // The lock is in effect right now, and it is what is holding the cursor
      // over there: `VirtualCursor::move_by` refuses the crossing on whichever
      // monitor the cursor occupies, so locking from here pins it to the peer
      // instead of reclaiming it — pinned by
      // `locking_keeps_input_flowing_to_the_peer_that_already_owns_it` in
      // `crates/wx-core/src/router.rs`. What it stops is the crossing, and only
      // the crossing: `VirtualCursor::warp_to` never consults `locked`, by
      // design and by its own doc comment — the lock is there to stop
      // *accidental* edge crossings, and an explicit handoff is not accidental.
      // So both ways out are named, and named as the two deliberate acts they
      // are rather than as a rule and a way around it.
      cursor.locked
        ? "This machine has the cursor locked, which is what is keeping it there: it will not cross back over an edge on its own. Unlock it on the Status screen, or hand it over directly with “Move the cursor here” on the Layout tab."
        : null
    ),
    certain: true,
  };
}

/// The badge element itself, so the window header and the Status panel cannot
/// draw the same decision two different ways.
///
/// The label is its own element rather than a bare text node so that the header,
/// where the pill is squeezed by an arbitrarily long peer name, can clip it with
/// an ellipsis: `text-overflow` needs a block container, which an anonymous flex
/// item is not.
export function cursorBadge(state, { title } = {}) {
  return h(
    "span",
    { class: `pill ${state.tone}`, title },
    state.node
      ? h("span", { class: "swatch small", style: { background: nodeColor(state.node) } })
      : null,
    h("span", { class: "label" }, state.badge)
  );
}

function join(...parts) {
  return parts.filter(Boolean).join(" ");
}
