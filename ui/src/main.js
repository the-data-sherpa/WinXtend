// Application shell: the three tabs, the connection banner, and the timers.
//
// The banner is the important part. A software KVM that is not running looks
// identical to one that has found no other machines — an empty list either way — and
// a user staring at an empty Devices tab has no way to tell which. So the connection
// state is never implied by absence: it is stated, with the one action that fixes it.

import "./styles.css";

import {
  attachToRunningAgent,
  connect,
  connected,
  listenToAgent,
  log,
  onChange,
  refreshStatus,
  startAgent,
  store,
} from "./agent.js";
import { bannerFor } from "./banner.js";
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
  const banner = bannerFor({
    connected: connected(),
    busy: store.busy,
    daemon: store.daemon,
    fault: store.fault,
    eventsProblem: store.eventsProblem,
  });
  if (!banner) {
    bannerEl.hidden = true;
    return;
  }
  bannerEl.hidden = false;

  const buttons = {
    start: () =>
      h(
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
      ),
    retry: () =>
      h(
        "button",
        {
          type: "button",
          onClick: () =>
            connect().catch((error) => {
              log("error", `Connecting: ${error.message}`);
            }),
        },
        "Try again"
      ),
  };

  bannerEl.className = banner.tone === "info" ? "banner" : "banner warning";
  replace(
    bannerEl,
    h(
      "div",
      { class: "banner-text" },
      h("strong", {}, banner.headline),
      banner.detail ? h("span", {}, banner.detail) : null,
      // Plain rather than `mono`: a hint is now a sentence about what is missing,
      // which may or may not have a variable name in it.
      banner.hint ? h("span", { class: "small" }, banner.hint) : null
    ),
    h("div", { class: "row" }, ...banner.actions.map((name) => buttons[name]()))
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
  // Subscribing first so that nothing the agent publishes between attaching and
  // listening is missed — but the result is not awaited for permission to carry
  // on. Discovery does not need events, and a startup that stops here is a window
  // that never looks for the daemon at all: no attach, and no timer below to try
  // again. `listenToAgent` therefore reports through `store.eventsProblem`, which
  // the banner names, instead of throwing.
  await listenToAgent();
  await attachToRunningAgent();

  setInterval(() => {
    if (!connected()) attachToRunningAgent();
  }, DISCOVER_INTERVAL_MS);

  setInterval(() => {
    if (!connected()) return;
    // Devices draws the pairing card, and a window whose event subscription was
    // refused learns about an incoming request only by reading the status — the
    // banner says as much. Polling it only in that state keeps the promise true
    // where it is made, without redrawing a healthy window's device list on a
    // timer it does not need.
    // Skipped rather than redrawn over: this poll's own redraw would take away the
    // rename editor and everything typed into it. The next tick picks it up.
    if (active === "devices" && devices.editing()) return;
    const pollingHere = active === "status" || (store.eventsProblem && active === "devices");
    if (pollingHere) {
      refreshStatus().catch(() => {
        /* the disconnect path reports this */
      });
    }
  }, STATUS_POLL_MS);

  // Coming back to the window is the moment stale numbers are most obvious.
  window.addEventListener("focus", () => {
    if (connected()) refreshStatus().catch(() => {});
    else attachToRunningAgent();
  });
}

main().catch((error) => {
  // Only a fault in this file reaches here now: everything that talks to the agent
  // reports through the store so that the banner can describe it. A message in the
  // document rather than only in a console the user will never open, and below the
  // banner rather than instead of it, because a half-started window still has to
  // say what it does know.
  document.body.appendChild(
    h(
      "pre",
      { class: "fatal" },
      `The interface failed to start: ${error?.message || String(error)}`
    )
  );
});
