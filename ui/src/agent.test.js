import { beforeEach, describe, expect, it, vi } from "vitest";

// Both halves of the Tauri API are replaced, because the point of these tests is
// what the frontend does when one of them refuses. `vi.hoisted` because `vi.mock`
// factories run before the imports below.
const tauri = vi.hoisted(() => ({ invoke: vi.fn(), listen: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: tauri.invoke }));
vi.mock("@tauri-apps/api/event", () => ({ listen: tauri.listen }));

import {
  applyEvent,
  attachToRunningAgent,
  connected,
  listenToAgent,
  store,
} from "./agent.js";

/// A `DaemonView` for an agent this window did not start: an endpoint file is on
/// disk, and nothing is attached to it yet.
const FOUND = {
  connected: false,
  configDir: "/home/someone/.config/winxtend",
  endpoint: { port: 43555, pid: 7706, agentVersion: "0.1.0" },
  agentPath: "/usr/bin/wx-agent",
  uiVersion: "0.1.0",
  message: null,
};

const NOTHING_RUNNING = { ...FOUND, endpoint: null };

const SNAPSHOT = {
  nodeId: "903ebcf9",
  nodeName: "cowen-ubuntu",
  agentVersion: "0.1.0",
  peers: [],
  monitors: [],
  cursor: { owner: "903ebcf9", ownerName: "cowen-ubuntu", monitor: 0, locked: false, local: true },
  layout: { revision: 1, placements: [] },
};

/// Stand in for the host process: `agent_state` reports what is on disk,
/// `connect_agent` attaches, and `agent_request` answers a status.
function hostWith(initial) {
  let daemon = initial;
  tauri.invoke.mockImplementation((command) => {
    switch (command) {
      case "agent_state":
        return Promise.resolve(daemon);
      case "connect_agent":
        if (!daemon.endpoint) return Promise.reject({ code: "notRunning", message: "no agent" });
        daemon = { ...daemon, connected: true };
        return Promise.resolve(daemon);
      case "agent_request":
        return Promise.resolve({ kind: "status", status: SNAPSHOT });
      default:
        return Promise.reject(new Error(`unexpected command ${command}`));
    }
  });
}

beforeEach(() => {
  tauri.invoke.mockReset();
  tauri.listen.mockReset();
  tauri.listen.mockResolvedValue(() => {});
  store.daemon = null;
  store.status = null;
  store.fault = null;
  store.pairing = null;
  store.journal = [];
  store.busy = false;
  store.eventsProblem = null;
});

describe("attaching to an agent this window did not start", () => {
  it("connects and loads a snapshot from an endpoint file it did not write", async () => {
    hostWith(FOUND);

    expect(await listenToAgent()).toBe(true);
    await attachToRunningAgent();

    expect(connected()).toBe(true);
    expect(store.status.nodeName).toBe("cowen-ubuntu");
    expect(store.daemon.endpoint.pid).toBe(7706);
    expect(store.eventsProblem).toBeNull();
  });

  // The reported failure, in full. On an installed machine a systemd user unit
  // brings the agent up and the window is opened afterwards, so this attach is
  // the *only* thing that ever finds the daemon — there is no click in it. It
  // used to be skipped entirely whenever the event subscription was refused,
  // because startup awaited the subscription first and let its rejection escape:
  // Tauri denies `plugin:event|listen` unless `src-tauri/capabilities` grants it,
  // this crate's own commands are not gated the same way, and the result was a
  // window that could talk to the agent perfectly well and never asked.
  it("still attaches when Tauri refuses the event subscription", async () => {
    hostWith(FOUND);
    tauri.listen.mockRejectedValue("Command plugin:event|listen not allowed by ACL");

    expect(await listenToAgent()).toBe(false);
    await attachToRunningAgent();

    expect(connected()).toBe(true);
    expect(store.status.nodeName).toBe("cowen-ubuntu");
    // And the window knows which specific thing it lost, so the banner can say so
    // rather than blaming the agent.
    expect(store.eventsProblem).toContain("plugin:event|listen");
  });

  it("says nothing is running only when the host reports no endpoint", async () => {
    hostWith(NOTHING_RUNNING);

    await attachToRunningAgent();

    expect(connected()).toBe(false);
    expect(store.daemon.endpoint).toBeNull();
    // No attempt was made, so there is no fault to report: the absence of an
    // endpoint file is the whole answer.
    expect(store.fault).toBeNull();
    expect(tauri.invoke).not.toHaveBeenCalledWith("connect_agent");
  });

  it("recovers on its own when the host is attached but this window has no snapshot", async () => {
    // The host holding a link and this window holding a snapshot are two different
    // things, and they come apart: `connect_agent` succeeds, the status request
    // that follows it is refused, and the window is left attached with nothing to
    // draw. Keying rediscovery on the host's `connected` flag made that permanent —
    // every later tick saw `connected: true`, did nothing, and the banner sat on
    // "Not connected to the agent" until someone clicked, which is the one thing
    // this path exists to avoid.
    let statuses = 0;
    const daemon = { ...FOUND, connected: true };
    tauri.invoke.mockImplementation((command) => {
      switch (command) {
        case "agent_state":
          return Promise.resolve(daemon);
        case "connect_agent":
          // Idempotent host-side: the link is already there and is handed back.
          return Promise.resolve(daemon);
        case "agent_request":
          return statuses++ === 0
            ? Promise.resolve({ kind: "error", code: "internal", message: "not ready" })
            : Promise.resolve({ kind: "status", status: SNAPSHOT });
        default:
          return Promise.reject(new Error(`unexpected command ${command}`));
      }
    });

    expect(await attachToRunningAgent()).toBe(false);
    expect(store.daemon.connected).toBe(true);
    expect(store.status).toBeNull();

    // No click on "Try again": the next turn of the rediscovery timer is enough.
    expect(await attachToRunningAgent()).toBe(true);
    expect(store.status.nodeName).toBe("cowen-ubuntu");
  });

  it("does not stack attempts while one is already running", async () => {
    hostWith(FOUND);

    await Promise.all([attachToRunningAgent(), attachToRunningAgent(), attachToRunningAgent()]);

    const attaches = tauri.invoke.mock.calls.filter(([c]) => c === "connect_agent");
    expect(attaches).toHaveLength(1);
  });
});

describe("pairing prompts", () => {
  /// The event the agent pushes when another machine is showing a code and is
  /// waiting for it to be typed here.
  const requested = (node, name) => ({ kind: "pairingRequested", node, name });

  it("does not replace a code the user is halfway through typing", () => {
    applyEvent(requested("aa", "workhorse"));
    store.pairing.pin = "123";
    applyEvent(requested("bb", "laptop"));

    expect(store.pairing.node).toBe("aa");
    expect(store.pairing.pin).toBe("123");
  });

  // The reported failure, and the reason retrying never helped the captain: the
  // guard above had no expiry. `store.pairing` was set by the first request and
  // cleared by nothing, so every later request — from any machine, for the rest
  // of the window's life — was dropped in silence. The agent now says when a
  // pairing is over, including the case that started this, a session that died
  // underneath one; the card that remains is a receipt and must stand aside.
  it("prompts again once the pairing that was on screen has ended", () => {
    applyEvent(requested("aa", "workhorse"));
    applyEvent({
      kind: "pairingFinished",
      node: "aa",
      accepted: false,
      message: "the connection was lost",
    });

    // The user is told why, without having to dismiss anything for the next
    // attempt to work.
    expect(store.pairing.finished).toBe(true);
    expect(store.pairing.error).toBe("the connection was lost");

    applyEvent(requested("aa", "workhorse"));

    expect(store.pairing.finished).toBeUndefined();
    expect(store.pairing.direction).toBe("incoming");
    expect(store.pairing.node).toBe("aa");
  });

  // The other half of why nothing appeared on the second machine: the agent
  // emits `pairingRequested` microseconds after the session comes up, which is
  // long before a window started from the desktop has attached, and a reload
  // throws away everything the previous window heard. Without the snapshot
  // there is no replay and the prompt is simply lost.
  it("shows a pairing that was already under way when the window attached", async () => {
    tauri.invoke.mockImplementation((command) => {
      switch (command) {
        case "agent_state":
        case "connect_agent":
          return Promise.resolve({ ...FOUND, connected: true });
        case "agent_request":
          return Promise.resolve({
            kind: "status",
            status: {
              ...SNAPSHOT,
              pairings: [
                { node: "aa", name: "workhorse", initiatedLocally: false, pin: null },
              ],
            },
          });
        default:
          return Promise.reject(new Error(`unexpected command ${command}`));
      }
    });

    await attachToRunningAgent();

    expect(store.pairing).toMatchObject({
      direction: "incoming",
      node: "aa",
      name: "workhorse",
      pin: "",
    });
  });

  it("shows the code again to the machine that generated it", async () => {
    // The initiator's window, reloaded while it was showing the PIN. It cannot
    // ask the agent to make another one — that would restart the exchange and
    // invalidate the digits the user is already typing on the other machine.
    tauri.invoke.mockImplementation((command) => {
      switch (command) {
        case "agent_state":
        case "connect_agent":
          return Promise.resolve({ ...FOUND, connected: true });
        case "agent_request":
          return Promise.resolve({
            kind: "status",
            status: {
              ...SNAPSHOT,
              pairings: [
                { node: "bb", name: "laptop", initiatedLocally: true, pin: "123456" },
              ],
            },
          });
        default:
          return Promise.reject(new Error(`unexpected command ${command}`));
      }
    });

    await attachToRunningAgent();

    expect(store.pairing).toMatchObject({ direction: "outgoing", node: "bb", pin: "123456" });
  });

  it("does not overwrite a prompt the user is answering with the snapshot's copy", async () => {
    applyEvent(requested("aa", "workhorse"));
    store.pairing.pin = "1234";
    tauri.invoke.mockImplementation((command) => {
      switch (command) {
        case "agent_state":
        case "connect_agent":
          return Promise.resolve({ ...FOUND, connected: true });
        case "agent_request":
          return Promise.resolve({
            kind: "status",
            status: {
              ...SNAPSHOT,
              pairings: [
                { node: "aa", name: "workhorse", initiatedLocally: false, pin: null },
              ],
            },
          });
        default:
          return Promise.reject(new Error(`unexpected command ${command}`));
      }
    });

    await attachToRunningAgent();

    expect(store.pairing.pin).toBe("1234");
  });
});
