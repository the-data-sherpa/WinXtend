// Status: what the agent is doing right now, and what it just did.
//
// The log tail matters more than it looks. When a handoff does not happen the user
// has no other way to see why — the input path is invisible by design — so the
// stream of notices, peer state changes and cursor handoffs the agent pushes is the
// only account of events they can read. It is a tail of the control channel, not a
// copy of the agent's own log file, and it says so.

import { callOk, changed, disconnect, log, refreshStatus, store } from "./agent.js";
import { h, replace } from "./dom.js";
import {
  capabilitiesText,
  clockTime,
  connectionState,
  displayServerLabel,
  latency,
  nodeColor,
  platformLabel,
  shortId,
  uptime,
} from "./format.js";

let root = null;
let renamingLocal = false;
let confirmingStop = false;

export function mount(element) {
  root = element;
}

async function attempt(what, action) {
  try {
    await action();
  } catch (error) {
    log("error", `${what}: ${error.message}`);
    changed();
  }
}

function localPanel() {
  const status = store.status;
  const facts = [
    ["Node id", status.nodeId],
    ["Platform", `${platformLabel(status.platform)} (${displayServerLabel(status.displayServer)})`],
    ["Agent", `${status.agentVersion}, protocol ${status.protocol}`],
    ["Interface", store.daemon?.uiVersion ? `${store.daemon.uiVersion}` : "unknown"],
    ["Listening on", `UDP port ${status.port}`],
    ["Running for", uptime(status.uptimeSecs)],
    ["Discovery", status.discovery ? "on" : "off"],
    [
      "Starts with the session",
      // Read-only on purpose: registering an autostart entry is the agent's own
      // `--install` mode, and there is no request for it on the control channel.
      // Saying so beats a switch that silently does nothing.
      status.autostart ? "yes" : "no, run wx-agent --install to change that",
    ],
    ["Can do", capabilitiesText(status.capabilities, true)],
    [
      "Displays",
      status.monitors.length
        ? status.monitors
            .map((m) => `${m.name} ${m.w}x${m.h}${m.primary ? " (primary)" : ""}`)
            .join(", ")
        : "none reported",
    ],
  ];

  const nameNode = renamingLocal
    ? h("input", {
        type: "text",
        class: "rename",
        value: status.nodeName,
        onKeyDown: (e) => {
          if (e.key === "Escape") {
            renamingLocal = false;
            changed();
          }
          if (e.key === "Enter") commit(e.target.value);
        },
        onBlur: (e) => commit(e.target.value),
      })
    : h(
        "button",
        {
          type: "button",
          class: "name",
          title: "Rename this machine",
          onClick: () => {
            renamingLocal = true;
            changed();
          },
        },
        status.nodeName
      );

  const commit = (value) => {
    const name = value.trim();
    renamingLocal = false;
    if (!name || name === status.nodeName) {
      changed();
      return;
    }
    attempt("renaming this machine", async () => {
      await callOk({ kind: "setNodeName", name });
      await refreshStatus();
    });
  };

  if (renamingLocal) queueMicrotask(() => nameNode.isConnected && nameNode.select());

  return h(
    "section",
    { class: "panel" },
    h(
      "header",
      {},
      h("h1", {}, "This machine"),
      h("span", { class: "swatch", style: { background: nodeColor(status.nodeId) } })
    ),
    h("div", { class: "titled" }, nameNode),
    h(
      "dl",
      { class: "facts-grid" },
      ...facts.flatMap(([label, value]) => [h("dt", {}, label), h("dd", {}, value)])
    )
  );
}

function cursorPanel() {
  const status = store.status;
  const cursor = status.cursor;
  const where = Number.isInteger(cursor.monitor)
    ? `display ${cursor.monitor}`
    : "no display yet";
  return h(
    "section",
    { class: "panel" },
    h("header", {}, h("h1", {}, "Cursor")),
    h(
      "p",
      { class: "cursor-owner" },
      h("strong", {}, cursor.ownerName || shortId(cursor.owner)),
      ` has the keyboard and mouse (${where})`,
      cursor.local ? " — this machine" : ""
    ),
    h(
      "div",
      { class: "row" },
      h(
        "button",
        {
          type: "button",
          onClick: () =>
            attempt("changing the cursor lock", async () => {
              await callOk({ kind: "setCursorLock" });
              await refreshStatus();
            }),
        },
        cursor.locked ? "Unlock the cursor" : "Lock the cursor here"
      ),
      h(
        "button",
        {
          type: "button",
          onClick: () =>
            attempt("locking every machine", async () => {
              await callOk({ kind: "lockAll" });
              await refreshStatus();
            }),
        },
        "Lock every machine"
      )
    ),
    h(
      "p",
      { class: "muted small" },
      cursor.locked
        ? "The cursor is pinned to this machine: it will not cross an edge until it is unlocked."
        : "Locking pins the cursor to one machine, which is what a full-screen game or a virtual machine needs."
    )
  );
}

function peersPanel() {
  const peers = store.status.peers || [];
  if (peers.length === 0) {
    return h(
      "section",
      { class: "panel" },
      h("header", {}, h("h1", {}, "Machines")),
      h("p", { class: "muted" }, "No other machine is known yet. See the Devices tab.")
    );
  }
  return h(
    "section",
    { class: "panel" },
    h("header", {}, h("h1", {}, "Machines")),
    h(
      "table",
      { class: "peers" },
      h(
        "thead",
        {},
        h(
          "tr",
          {},
          h("th", {}, "Machine"),
          h("th", {}, "State"),
          h("th", {}, "Round trip"),
          h("th", {}, "Displays"),
          h("th", {}, "Address")
        )
      ),
      h(
        "tbody",
        {},
        ...peers.map((peer) => {
          const state = connectionState(peer.status);
          return h(
            "tr",
            {},
            h(
              "td",
              {},
              h("span", { class: "swatch small", style: { background: nodeColor(peer.node) } }),
              peer.name,
              h("span", { class: "muted small" }, ` ${shortId(peer.node)}`)
            ),
            h("td", {}, h("span", { class: `pill ${state.tone}` }, state.label)),
            h("td", {}, latency(peer.rttMs)),
            h("td", {}, String(peer.monitors?.length ?? 0)),
            h("td", { class: "mono small" }, peer.addresses?.[0] || "not known")
          );
        })
      )
    )
  );
}

function agentPanel() {
  const daemon = store.daemon;
  return h(
    "section",
    { class: "panel" },
    h("header", {}, h("h1", {}, "Agent process")),
    h(
      "dl",
      { class: "facts-grid" },
      h("dt", {}, "Control channel"),
      h(
        "dd",
        { class: "mono small" },
        daemon?.endpoint
          ? `127.0.0.1:${daemon.endpoint.port}, pid ${daemon.endpoint.pid}`
          : "not published"
      ),
      h("dt", {}, "Configuration"),
      h("dd", { class: "mono small" }, daemon?.configDir || "unknown"),
      h("dt", {}, "Binary"),
      h("dd", { class: "mono small" }, daemon?.agentPath || "not found beside this application")
    ),
    h(
      "div",
      { class: "row" },
      h(
        "button",
        { type: "button", onClick: () => attempt("detaching", () => disconnect()) },
        "Detach (leave it running)"
      ),
      confirmingStop
        ? h(
            "span",
            { class: "row" },
            h(
              "button",
              {
                type: "button",
                class: "danger",
                onClick: () =>
                  attempt("stopping the agent", async () => {
                    confirmingStop = false;
                    await callOk({ kind: "shutdown" });
                    log("warning", "Asked the agent to stop");
                  }),
              },
              "Yes, stop sharing"
            ),
            h(
              "button",
              {
                type: "button",
                onClick: () => {
                  confirmingStop = false;
                  changed();
                },
              },
              "Keep it running"
            )
          )
        : h(
            "button",
            {
              type: "button",
              class: "danger-quiet",
              onClick: () => {
                confirmingStop = true;
                changed();
              },
            },
            "Stop the agent"
          )
    ),
    h(
      "p",
      { class: "muted small" },
      "Closing this window does not stop the agent. Stopping it ends keyboard and mouse " +
        "sharing on every machine until it is started again."
    )
  );
}

function logPanel() {
  return h(
    "section",
    { class: "panel" },
    h(
      "header",
      {},
      h("h1", {}, "Activity"),
      h(
        "button",
        {
          type: "button",
          onClick: () => {
            store.journal.length = 0;
            changed();
          },
        },
        "Clear"
      )
    ),
    h(
      "p",
      { class: "muted small" },
      "Everything the agent has pushed to this window since it opened. Newest first."
    ),
    store.journal.length === 0
      ? h("p", { class: "muted" }, "Nothing yet.")
      : h(
          "ol",
          { class: "journal" },
          ...store.journal.map((entry) =>
            h(
              "li",
              { class: `journal-${entry.level}` },
              h("span", { class: "mono small when" }, clockTime(entry.at)),
              h("span", {}, entry.text)
            )
          )
        )
  );
}

export function render() {
  if (!root || root.hidden) return;
  if (!store.status) {
    replace(
      root,
      h(
        "div",
        { class: "empty" },
        h("p", {}, "Not connected to an agent, so there is nothing to report."),
        logPanel()
      )
    );
    return;
  }
  replace(
    root,
    h(
      "div",
      { class: "columns" },
      h("div", { class: "column" }, localPanel(), cursorPanel(), peersPanel()),
      h("div", { class: "column" }, agentPanel(), logPanel())
    )
  );
}
