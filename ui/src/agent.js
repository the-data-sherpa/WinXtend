// The frontend's whole model: one status snapshot from the agent, patched by the
// events the agent pushes.
//
// There is no second source of truth here on purpose. Every screen renders from
// `store`, `store.status` is replaced wholesale by a `status` request, and events
// patch the parts they name. The alternative — each view holding its own copy of the
// peer list — is how two screens end up disagreeing about whether a machine is
// paired, and the user believes the one that is wrong.

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { shortId } from "./format.js";

/// Longest activity log kept in memory. The log is a tail, not an archive: the agent
/// already logs to its own output, and an unbounded array in a window that stays
/// open for days is a leak.
const JOURNAL_LIMIT = 300;

export const store = {
  /// Host-process view: is there an agent, are we attached, where would we start one.
  daemon: null,
  /// The agent's StatusSnapshot, or null when not connected.
  status: null,
  /// Last connection-level failure, as an AgentError-shaped object.
  fault: null,
  /// In-flight pairing, in either direction. See devices.js.
  pairing: null,
  /// Newest-first activity lines for the Status screen.
  journal: [],
  /// Set while a connect/start attempt is running, so buttons can be disabled.
  busy: false,
  /// Why the agent's events are not reaching this window, or null while they are.
  ///
  /// Separate from `fault` because the two are independent: the control channel
  /// can be perfectly healthy while the webview's own event bus is not, and that
  /// combination has to be describable. A window in that state shows correct
  /// numbers that quietly stop changing, which is worse than no numbers, so it is
  /// something the banner says out loud rather than something the store forgets.
  eventsProblem: null,
};

const listeners = new Set();

export function onChange(fn) {
  listeners.add(fn);
  return () => listeners.delete(fn);
}

export function changed() {
  for (const fn of listeners) {
    try {
      fn();
    } catch (e) {
      // One broken view must not stop the others from redrawing.
      console.error("view failed to render", e);
    }
  }
}

/// Anything that went wrong, from either side of the IPC channel.
///
/// `source` distinguishes the two, because they need different recovery: a `host`
/// fault means the connection is in question, an `agent` fault means the agent is
/// fine and refused this particular request.
export class AgentError extends Error {
  constructor(code, message, source) {
    super(message || String(code));
    this.name = "AgentError";
    this.code = code || "internal";
    this.source = source;
  }
}

function asAgentError(thrown) {
  if (thrown instanceof AgentError) return thrown;
  if (thrown && typeof thrown === "object" && typeof thrown.code === "string") {
    return new AgentError(thrown.code, thrown.message, "host");
  }
  return new AgentError("internal", thrown?.message || String(thrown), "host");
}

/// Send a request and return the raw Response, including `{kind:"error"}`.
export async function call(request) {
  try {
    return await invoke("agent_request", { request });
  } catch (thrown) {
    throw asAgentError(thrown);
  }
}

/// Send a request and throw unless the agent accepted it.
export async function callOk(request) {
  const response = await call(request);
  if (response && response.kind === "error") {
    throw new AgentError(response.code, response.message, "agent");
  }
  return response;
}

export async function validateLayout(layout) {
  try {
    return await invoke("validate_layout", { layout });
  } catch (thrown) {
    // Validation failing is a bug in this build, not something the user can act on;
    // an unvalidated layout still saves, so the editor stays usable.
    console.error("validating the layout", thrown);
    return [];
  }
}

export function log(level, text) {
  store.journal.unshift({ at: new Date(), level, text });
  if (store.journal.length > JOURNAL_LIMIT) store.journal.length = JOURNAL_LIMIT;
}

// ---------------------------------------------------------------------------
// Connection
// ---------------------------------------------------------------------------

async function hostCall(command) {
  store.busy = true;
  changed();
  try {
    store.daemon = await invoke(command);
    store.fault = null;
    return store.daemon;
  } catch (thrown) {
    const error = asAgentError(thrown);
    store.fault = { code: error.code, message: error.message };
    // Refresh what is knowable even after a failure, so the banner can still say
    // whether an endpoint file exists.
    try {
      store.daemon = await invoke("agent_state");
    } catch {
      /* leave the previous view in place */
    }
    throw error;
  } finally {
    store.busy = false;
    changed();
  }
}

export async function refreshDaemon() {
  try {
    store.daemon = await invoke("agent_state");
  } catch (thrown) {
    store.fault = asAgentError(thrown);
  }
  changed();
  return store.daemon;
}

/// Attach to a running agent and load the first snapshot.
export async function connect() {
  await hostCall("connect_agent");
  await refreshStatus();
  log("info", "Connected to the agent");
  return store.daemon;
}

/// Start an agent and attach to it.
export async function startAgent() {
  log("info", "Starting the agent");
  await hostCall("start_agent");
  await refreshStatus();
  log("info", "The agent is running");
  return store.daemon;
}

export async function disconnect() {
  await hostCall("disconnect_agent");
  store.status = null;
  log("info", "Detached from the agent. It keeps running.");
  changed();
}

/// Replace the whole snapshot. Cheap enough to do on any change we cannot patch.
export async function refreshStatus() {
  const response = await callOk({ kind: "status" });
  store.status = response.status;
  changed();
  return store.status;
}

export function connected() {
  return Boolean(store.daemon?.connected && store.status);
}

/// Set while an attach attempt is running, so a poll does not stack attempts.
let attaching = false;

/// Attach to an agent that is already running, without making noise about it.
///
/// This is the only path that finds a daemon nobody asked this window to start —
/// the one from the previous session, the one a systemd user unit brought up with
/// the desktop, the one started from a terminal. It runs at startup and on a
/// timer, where a missing agent is the expected case, so a failure updates
/// `store.fault` for the banner and is otherwise swallowed: a thrown error every
/// two seconds would fill the activity log with the same line.
///
/// It deliberately does not depend on the event subscription. Events keep an
/// attached window fresh; they have nothing to do with finding an agent, and
/// making discovery wait on them is how a window that could have attached ends up
/// telling the user nothing is running.
export async function attachToRunningAgent() {
  if (attaching || connected()) return connected();
  attaching = true;
  try {
    await refreshDaemon();
    if (store.daemon?.endpoint && !store.daemon.connected) {
      await connect();
    }
  } catch {
    // The banner already carries the reason; see store.fault.
  } finally {
    attaching = false;
    changed();
  }
  return connected();
}

// ---------------------------------------------------------------------------
// Event handling
// ---------------------------------------------------------------------------

function peerName(node) {
  if (!node) return "unknown machine";
  if (store.status && node === store.status.nodeId) return store.status.nodeName;
  const peer = store.status?.peers?.find((p) => p.node === node);
  return peer?.name || shortId(node);
}

function upsertPeer(peer) {
  const status = store.status;
  if (!status) return;
  const peers = status.peers.filter((p) => p.node !== peer.node);
  peers.push(peer);
  // The agent sorts its own list by name; keeping the same order here means an
  // event-driven update does not reshuffle rows the user is reading.
  peers.sort((a, b) => a.name.localeCompare(b.name) || a.node.localeCompare(b.node));
  status.peers = peers;
}

/// Apply one agent event to the store. Exported for the event listener only.
export function applyEvent(event) {
  const status = store.status;
  switch (event.kind) {
    case "peerChanged": {
      const known = status?.peers?.find((p) => p.node === event.peer.node);
      upsertPeer(event.peer);
      if (!known) {
        log("info", `Found ${event.peer.name} (${shortId(event.peer.node)})`);
      } else if (known.status?.state !== event.peer.status?.state) {
        log("info", `${event.peer.name} is now ${event.peer.status?.state || "unknown"}`);
      } else if (known.paired !== event.peer.paired) {
        log("info", `${event.peer.name} is ${event.peer.paired ? "paired" : "no longer paired"}`);
      }
      break;
    }
    case "peerRemoved": {
      log("info", `${peerName(event.node)} was removed`);
      if (status) status.peers = status.peers.filter((p) => p.node !== event.node);
      break;
    }
    case "pairingRequested": {
      // The other machine started this and is showing a code. Only one prompt at a
      // time: a second request while one is open would replace the code the user is
      // halfway through typing.
      if (!store.pairing) {
        store.pairing = {
          direction: "incoming",
          node: event.node,
          name: event.name,
          pin: "",
          error: null,
        };
      }
      log("info", `${event.name} wants to pair`);
      break;
    }
    case "pairingFinished": {
      if (store.pairing && store.pairing.node === event.node) {
        store.pairing = {
          ...store.pairing,
          finished: true,
          accepted: event.accepted,
          error: event.accepted ? null : event.message || "pairing was refused",
        };
      }
      log(
        event.accepted ? "info" : "warning",
        `Pairing with ${peerName(event.node)} ${event.accepted ? "succeeded" : `failed: ${event.message || "refused"}`}`
      );
      break;
    }
    case "layoutChanged": {
      if (status) status.layout = event.layout;
      log("info", `The layout changed (revision ${event.layout?.revision ?? "?"})`);
      break;
    }
    case "cursorOwnerChanged": {
      if (status) {
        status.cursor = {
          ...status.cursor,
          owner: event.node,
          ownerName: peerName(event.node),
          monitor: event.monitor,
          local: event.local,
        };
      }
      log("info", `The cursor moved to ${peerName(event.node)}`);
      break;
    }
    case "cursorLockChanged": {
      if (status) status.cursor = { ...status.cursor, locked: event.locked };
      log("info", event.locked ? "The cursor is locked to this machine" : "The cursor is unlocked");
      break;
    }
    case "monitorsChanged": {
      if (status) status.monitors = event.monitors;
      log("info", `This machine now has ${event.monitors.length} display(s)`);
      break;
    }
    case "notice": {
      log(event.level || "info", event.message);
      break;
    }
    default:
      // Rule 4 of the IPC contract: an unknown kind is ignored, not an error. It is
      // still logged, because a UI that silently drops news from a newer agent is
      // impossible to debug from a screenshot.
      log("info", `Unrecognised event from the agent: ${event.kind}`);
      break;
  }
  changed();
}

/// Subscribe to what the host process republishes. Called once at startup.
///
/// Reports failure by returning `false` and setting `store.eventsProblem` rather
/// than throwing, because every caller is a startup sequence and a throw here used
/// to take the rest of that sequence with it — including the attach that would
/// have found the running agent, and the timer that would have retried. Tauri's
/// capability system is the way this fails in practice: `listen` is a built-in
/// plugin command, so it is refused by the ACL unless `src-tauri/capabilities`
/// grants it, while this crate's own commands keep working. The window is then
/// perfectly able to talk to the agent and never tries.
///
/// A window without events is degraded, not broken: everything on screen still
/// arrives through `refreshStatus`, so the honest answer is to carry on and say
/// what is missing.
export async function listenToAgent() {
  try {
    await listen("agent-event", (message) => applyEvent(message.payload));
    await listen("agent-status", async (message) => {
      const note = message.payload;
      if (note.connected) return; // the connect path already logged and refreshed
      const wasConnected = Boolean(store.daemon?.connected);
      store.status = null;
      store.pairing = null;
      await refreshDaemon();
      if (wasConnected) {
        log("error", `Lost the connection to the agent: ${note.message}`);
        store.fault = { code: "disconnected", message: note.message };
      }
      changed();
    });
    store.eventsProblem = null;
    return true;
  } catch (thrown) {
    const error = asAgentError(thrown);
    store.eventsProblem = error.message;
    log("error", `This window cannot receive agent events: ${error.message}`);
    changed();
    return false;
  }
}

