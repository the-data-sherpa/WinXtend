import { beforeEach, describe, expect, it, vi } from "vitest";

// Both halves of the Tauri API are replaced, because the point of these tests is
// what the frontend does when one of them refuses. `vi.hoisted` because `vi.mock`
// factories run before the imports below.
const tauri = vi.hoisted(() => ({ invoke: vi.fn(), listen: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: tauri.invoke }));
vi.mock("@tauri-apps/api/event", () => ({ listen: tauri.listen }));

import { attachToRunningAgent, connected, listenToAgent, store } from "./agent.js";

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

  it("does not stack attempts while one is already running", async () => {
    hostWith(FOUND);

    await Promise.all([attachToRunningAgent(), attachToRunningAgent(), attachToRunningAgent()]);

    const attaches = tauri.invoke.mock.calls.filter(([c]) => c === "connect_agent");
    expect(attaches).toHaveLength(1);
  });
});
