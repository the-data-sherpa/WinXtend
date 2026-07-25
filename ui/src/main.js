// Application shell: the three tabs, the connection banner, and the timers.
//
// The banner is the important part. A software KVM that is not running looks
// identical to one that has found no other machines — an empty list either way — and
// a user staring at an empty Devices tab has no way to tell which. So the connection
// state is never implied by absence: it is stated, with the one action that fixes it.

import "./styles.css";

import {
  connect,
  connected,
  listenToAgent,
  log,
  onChange,
  refreshDaemon,
  refreshStatus,
  startAgent,
  store,
} from "./agent.js";
import { h, replace } from "./dom.js";
import * as devices from "./devices.js";
import * as layout from "./layout.js";
import * as status from "./status.js";

/// How often to look for an agent while not connected. Slow enough to be free,
/// fast enough that starting the daemon from a terminal is noticed while the user is
/// still looking at the window.
const DISCOVER_INTERVAL_MS = 2500;
/// How often to re-read the whole snapshot while the Status tab is open. Uptime and
/// round-trip time are not pushed as events — they change continuously and would
/// flood the channel — so they are polled while they are on screen and never
/// otherwise.
const STATUS_POLL_MS = 5000;

const views = {
  devices: { module: devices, section: null, tab: null },
  layout: { module: layout, section: null, tab: null },
  status: { module: status, section: null, tab: null },
};

let active = "devices";
let bannerEl = null;
let connectionEl = null;
let localNameEl = null;
/// Set while an automatic attempt is running, so the poll does not stack attempts.
let autoConnecting = false;

function renderChrome() {
  const daemon = store.daemon;
  const online = connected();
  replace(
    connectionEl,
    h("span", { class: `dot ${online ? "good" : "bad"}` }),
    h(
      "span",
      {},
      online
        ? `Connected${daemon?.endpoint ? ` · port ${daemon.endpoint.port}` : ""}`
        : "Not connected"
    )
  );
  localNameEl.textContent = store.status ? store.status.nodeName : "";
}

function renderBanner() {
  const daemon = store.daemon;
  const fault = store.fault;
  if (connected()) {
    bannerEl.hidden = true;
    return;
  }
  bannerEl.hidden = false;

  if (store.busy) {
    replace(bannerEl, h("span", {}, "Talking to the agent..."));
    bannerEl.className = "banner";
    return;
  }

  const startButton = h(
    "button",
    {
      type: "button",
      class: "primary",
      onClick: () =>
        startAgent().catch((error) => {
          log("error", `Starting the agent: ${error.message}`);
        }),
    },
    "Start the agent"
  );
  const retryButton = h(
    "button",
    {
      type: "button",
      onClick: () =>
        connect().catch((error) => {
          log("error", `Connecting: ${error.message}`);
        }),
    },
    "Try again"
  );

  const code = fault?.code;
  let headline;
  let detail;
  let actions;
  if (code === "notRunning" || !daemon?.endpoint) {
    headline = "The WinXtend agent is not running.";
    detail = daemon?.agentPath
      ? "Nothing is being shared. The agent is the process that captures the keyboard and mouse; this window only configures it."
      : "The agent program could not be found next to this application, so it cannot be started from here.";
    actions = daemon?.agentPath ? [startButton] : [];
  } else if (code === "stale") {
    headline = "An agent left its details behind but is not answering.";
    detail = `${fault.message}. It was probably killed rather than asked to stop. Starting it again is safe.`;
    actions = [startButton, retryButton];
  } else if (code === "disconnected" || code === "notConnected") {
    headline = "The connection to the agent was lost.";
    detail = `${fault.message}. Keyboard and mouse sharing carries on without this window.`;
    actions = [retryButton];
  } else if (fault) {
    headline = "The agent could not be reached.";
    detail = fault.message;
    actions = [retryButton];
  } else {
    headline = "Not connected to the agent.";
    detail = "";
    actions = [retryButton];
  }

  bannerEl.className = "banner warning";
  replace(
    bannerEl,
    h(
      "div",
      { class: "banner-text" },
      h("strong", {}, headline),
      detail ? h("span", {}, detail) : null,
      daemon?.agentPath === null && code === "notRunning"
        ? h(
            "span",
            { class: "mono small" },
            "Set WINXTEND_AGENT to the path of wx-agent to start it from here."
          )
        : null
    ),
    h("div", { class: "row" }, ...actions)
  );
}

function renderActive() {
  for (const [name, view] of Object.entries(views)) {
    const isActive = name === active;
    view.section.hidden = !isActive;
    view.tab.classList.toggle("active", isActive);
    view.tab.setAttribute("aria-selected", String(isActive));
  }
  views[active].module.render();
}

function renderAll() {
  renderChrome();
  renderBanner();
  renderActive();
}

function selectView(name) {
  if (!views[name]) return;
  active = name;
  renderAll();
}

/// Try to attach without making noise about it.
///
/// Used on startup and by the poll, where a missing agent is the expected case and a
/// thrown error every two seconds would fill the activity log with the same line.
async function autoConnect() {
  if (autoConnecting || connected()) return;
  autoConnecting = true;
  try {
    await refreshDaemon();
    if (store.daemon?.endpoint && !store.daemon.connected) {
      await connect();
    }
  } catch {
    // The banner already carries the reason; see store.fault.
  } finally {
    autoConnecting = false;
    renderAll();
  }
}

function wire() {
  bannerEl = document.getElementById("banner");
  connectionEl = document.getElementById("connection");
  localNameEl = document.getElementById("local-name");
  for (const [name, view] of Object.entries(views)) {
    view.section = document.getElementById(`view-${name}`);
    view.tab = document.querySelector(`#tabs button[data-view="${name}"]`);
    view.tab.addEventListener("click", () => selectView(name));
    view.module.mount(view.section);
  }
  onChange(renderAll);
}

async function main() {
  wire();
  renderAll();
  await listenToAgent();
  await autoConnect();

  setInterval(() => {
    if (!connected()) autoConnect();
  }, DISCOVER_INTERVAL_MS);

  setInterval(() => {
    if (active === "status" && connected()) {
      refreshStatus().catch(() => {
        /* the disconnect path reports this */
      });
    }
  }, STATUS_POLL_MS);

  // Coming back to the window is the moment stale numbers are most obvious.
  window.addEventListener("focus", () => {
    if (connected()) refreshStatus().catch(() => {});
    else autoConnect();
  });
}

main().catch((error) => {
  // Nothing else has rendered yet if this fails, so say so in the document itself
  // rather than only in a console the user will never open.
  document.body.appendChild(
    h(
      "pre",
      { class: "fatal" },
      `The interface failed to start: ${error?.message || String(error)}`
    )
  );
});
