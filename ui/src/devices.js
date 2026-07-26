// Devices: what the local network has, and what is paired.
//
// This is the first screen because discovery is the feature that has to work with no
// configuration: two agents on one network find each other, and the only thing asked
// of the user is to confirm a six-digit code. Everything else on this screen exists
// to explain why that has not happened yet.

import { call, callOk, log, refreshStatus, store, changed } from "./agent.js";
import { h, replace } from "./dom.js";
import {
  capabilitiesText,
  connectionState,
  nodeColor,
  pinGroups,
  platformLabel,
  shortId,
} from "./format.js";

let root = null;
/// Node id currently being renamed, so a redraw does not close the editor.
let renaming = null;

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

function pairingCard() {
  const pairing = store.pairing;
  if (!pairing) return null;

  const close = () => {
    store.pairing = null;
    changed();
  };

  if (pairing.finished) {
    return h(
      "div",
      { class: `pairing ${pairing.accepted ? "ok" : "bad"}` },
      h("h2", {}, pairing.accepted ? "Paired" : "Pairing failed"),
      h(
        "p",
        {},
        pairing.accepted
          ? `${pairing.name} is now paired. Its screens can be arranged in the Layout tab.`
          : pairing.error || "The other machine refused the code."
      ),
      h("div", { class: "row" }, h("button", { type: "button", onClick: close }, "Close"))
    );
  }

  if (pairing.direction === "outgoing") {
    // This side generated the code; the user reads it out and types it there.
    return h(
      "div",
      { class: "pairing" },
      h("h2", {}, `Pairing with ${pairing.name}`),
      h("p", {}, "Type this code on the other machine to confirm it is really this one."),
      h("div", { class: "pin", role: "status" }, pinGroups(pairing.pin)),
      h(
        "p",
        { class: "muted" },
        "The code is valid for a short time. Nothing is trusted until it is typed."
      ),
      h(
        "div",
        { class: "row" },
        h(
          "button",
          {
            type: "button",
            onClick: () =>
              attempt("cancelling the pairing", async () => {
                await call({ kind: "cancelPairing", node: pairing.node });
                store.pairing = null;
                changed();
              }),
          },
          "Cancel"
        )
      )
    );
  }

  // Incoming: the other machine is showing a code and waiting for it here.
  const input = h("input", {
    type: "text",
    inputMode: "numeric",
    autocomplete: "off",
    maxLength: 7,
    class: "pin-input",
    value: pairing.pin || "",
    placeholder: "000 000",
    onInput: (e) => {
      pairing.pin = e.target.value;
    },
    onKeyDown: (e) => {
      if (e.key === "Enter") submit();
    },
  });

  const submit = () =>
    attempt("confirming the pairing", async () => {
      const digits = String(pairing.pin || "").replace(/\D/g, "");
      if (digits.length !== 6) {
        pairing.error = "The code is six digits.";
        changed();
        return;
      }
      pairing.error = null;
      changed();
      await callOk({ kind: "confirmPairing", node: pairing.node, pin: digits });
      await refreshStatus();
    });

  const card = h(
    "div",
    { class: "pairing" },
    h("h2", {}, `${pairing.name} wants to pair`),
    h("p", {}, "Enter the six-digit code shown on that machine."),
    h("div", { class: "row" }, input),
    pairing.error ? h("p", { class: "error" }, pairing.error) : null,
    h(
      "div",
      { class: "row" },
      h("button", { type: "button", class: "primary", onClick: submit }, "Confirm"),
      h(
        "button",
        {
          type: "button",
          onClick: () =>
            attempt("refusing the pairing", async () => {
              await call({ kind: "cancelPairing", node: pairing.node });
              store.pairing = null;
              changed();
            }),
        },
        "Refuse"
      ),
      h(
        "button",
        {
          type: "button",
          class: "danger-quiet",
          onClick: () =>
            attempt("blocking the machine", async () => {
              await call({ kind: "blockPeer", node: pairing.node });
              store.pairing = null;
              await refreshStatus();
            }),
        },
        "Block this machine"
      )
    )
  );
  // Focus after the node is in the document, so typing can start immediately.
  queueMicrotask(() => input.isConnected && input.focus());
  return card;
}

function beginPairing(peer) {
  return attempt("starting the pairing", async () => {
    const response = await callOk({ kind: "beginPairing", node: peer.node });
    if (response.kind !== "pairingStarted") {
      throw new Error("the agent did not return a pairing code");
    }
    store.pairing = {
      direction: "outgoing",
      node: response.node,
      name: peer.name,
      pin: response.pin,
      error: null,
    };
    changed();
  });
}

function nameCell(peer) {
  if (renaming === peer.node) {
    const input = h("input", {
      type: "text",
      class: "rename",
      value: peer.name,
      onKeyDown: (e) => {
        if (e.key === "Escape") {
          renaming = null;
          changed();
        }
        if (e.key === "Enter") commit(e.target.value);
      },
      onBlur: (e) => commit(e.target.value),
    });
    const commit = (value) => {
      const name = value.trim();
      renaming = null;
      if (!name || name === peer.name) {
        changed();
        return;
      }
      attempt("renaming the machine", async () => {
        await callOk({ kind: "setPeerName", node: peer.node, name });
        await refreshStatus();
      });
    };
    queueMicrotask(() => input.isConnected && input.select());
    return input;
  }

  const differs = peer.advertisedName && peer.advertisedName !== peer.name;
  return h(
    "div",
    { class: "name-cell" },
    h(
      "button",
      {
        type: "button",
        class: "name",
        title: "Rename this machine locally",
        onClick: () => {
          renaming = peer.node;
          changed();
        },
      },
      peer.name
    ),
    differs ? h("span", { class: "muted small" }, `advertises "${peer.advertisedName}"`) : null
  );
}

/// What a machine has told the mesh it can do.
///
/// Shown on every row, including this one, because the agent now refuses to attempt
/// a feature against a machine that has not advertised it — and a refusal the user
/// cannot see the reason for is no better than the silent failure it replaced. It is
/// also the cheapest guard against the software claiming something it cannot do: an
/// untrue line here is visible on the first screen of the app.
function capabilityRow(bits) {
  return h(
    "div",
    { class: "facts capabilities", title: "What this machine has told the others it can do" },
    capabilitiesText(bits)
  );
}

function peerRow(peer) {
  const state = connectionState(peer.status);
  const swatch = h("span", {
    class: "swatch",
    style: { background: nodeColor(peer.node) },
    title: `Colour used for this machine's screens in the Layout tab`,
  });

  const actions = [];
  if (peer.blocked) {
    actions.push(
      h(
        "button",
        {
          type: "button",
          onClick: () =>
            attempt("unblocking the machine", async () => {
              await callOk({ kind: "forgetPeer", node: peer.node });
              await refreshStatus();
            }),
        },
        "Unblock"
      )
    );
  } else if (peer.paired) {
    actions.push(
      h(
        "button",
        {
          type: "button",
          onClick: () =>
            attempt("changing whether the machine is enabled", async () => {
              await callOk({
                kind: "setPeerEnabled",
                node: peer.node,
                enabled: !peer.enabled,
              });
              await refreshStatus();
            }),
        },
        peer.enabled ? "Disable" : "Enable"
      ),
      h(
        "button",
        {
          type: "button",
          class: "danger-quiet",
          onClick: () =>
            attempt("forgetting the machine", async () => {
              await callOk({ kind: "forgetPeer", node: peer.node });
              await refreshStatus();
            }),
        },
        "Forget"
      )
    );
  } else {
    actions.push(
      h(
        "button",
        { type: "button", class: "primary", onClick: () => beginPairing(peer) },
        "Pair"
      ),
      h(
        "button",
        {
          type: "button",
          class: "danger-quiet",
          onClick: () =>
            attempt("blocking the machine", async () => {
              await callOk({ kind: "blockPeer", node: peer.node });
              await refreshStatus();
            }),
        },
        "Block"
      )
    );
  }

  const facts = [
    platformLabel(peer.platform),
    `id ${shortId(peer.node)}`,
    peer.agentVersion ? `agent ${peer.agentVersion}` : null,
    peer.monitors?.length ? `${peer.monitors.length} display(s)` : null,
    peer.addresses?.length ? peer.addresses[0] : null,
  ].filter(Boolean);

  return h(
    "li",
    { class: `device ${peer.paired ? "paired" : "unpaired"}${peer.blocked ? " blocked" : ""}` },
    swatch,
    h(
      "div",
      { class: "device-main" },
      nameCell(peer),
      h("div", { class: "facts" }, facts.join("  ·  ")),
      capabilityRow(peer.capabilities)
    ),
    h(
      "div",
      { class: "device-state" },
      h("span", { class: `pill ${state.tone}` }, state.label),
      h(
        "span",
        { class: "muted small" },
        peer.blocked
          ? "Blocked"
          : peer.paired
            ? peer.enabled
              ? "Paired"
              : "Paired, disabled"
            : "Not paired"
      )
    ),
    h("div", { class: "device-actions" }, actions)
  );
}

function localRow() {
  const status = store.status;
  return h(
    "li",
    { class: "device local" },
    h("span", { class: "swatch", style: { background: nodeColor(status.nodeId) } }),
    h(
      "div",
      { class: "device-main" },
      h("div", { class: "name-cell" }, h("span", { class: "name static" }, status.nodeName)),
      h(
        "div",
        { class: "facts" },
        [
          platformLabel(status.platform),
          `id ${shortId(status.nodeId)}`,
          `agent ${status.agentVersion}`,
          `${status.monitors.length} display(s)`,
          `port ${status.port}`,
        ].join("  ·  ")
      ),
      capabilityRow(status.capabilities)
    ),
    h(
      "div",
      { class: "device-state" },
      h("span", { class: "pill good" }, "This machine"),
      h(
        "span",
        { class: "muted small" },
        status.discovery ? "Announcing on the network" : "Discovery is off"
      )
    ),
    h("div", { class: "device-actions" })
  );
}

function emptyState() {
  const status = store.status;
  if (!status.discovery) {
    return h(
      "div",
      { class: "empty" },
      h("h2", {}, "Discovery is switched off"),
      h(
        "p",
        {},
        "This agent is not announcing itself or looking for other machines, so nothing " +
          "will appear here. Turn discovery back on in the agent's configuration, or add " +
          "a machine by address."
      )
    );
  }
  return h(
    "div",
    { class: "empty" },
    h("h2", {}, "Looking for machines"),
    h(
      "p",
      {},
      "No other WinXtend agent has answered on this network yet. Start the agent on the " +
        "other machine; it should appear here within a few seconds."
    ),
    h(
      "ul",
      { class: "hints" },
      h("li", {}, "Both machines have to be on the same subnet, with multicast allowed."),
      h("li", {}, `This machine is listening on UDP port ${status.port}.`),
      h("li", {}, "A firewall prompt that was dismissed will keep the agent invisible.")
    )
  );
}

export function render() {
  if (!root || root.hidden) return;
  const status = store.status;
  if (!status) {
    // The banner above already explains the connection state; repeating it here
    // would be two competing explanations of the same thing.
    replace(root, h("div", { class: "empty" }, h("p", {}, "Not connected to an agent.")));
    return;
  }

  const peers = status.peers || [];
  const unpaired = peers.filter((p) => !p.paired && !p.blocked);
  const paired = peers.filter((p) => p.paired);
  const blocked = peers.filter((p) => p.blocked && !p.paired);

  replace(
    root,
    pairingCard(),
    h(
      "section",
      { class: "panel" },
      h(
        "header",
        {},
        h("h1", {}, "Devices"),
        h(
          "span",
          { class: "muted" },
          `${paired.length} paired, ${unpaired.length} discovered`
        )
      ),
      h("ul", { class: "devices" }, localRow(), ...paired.map(peerRow), ...unpaired.map(peerRow)),
      peers.length === 0 ? emptyState() : null,
      blocked.length
        ? h(
            "div",
            { class: "subsection" },
            h("h2", {}, "Blocked"),
            h("ul", { class: "devices" }, ...blocked.map(peerRow))
          )
        : null
    )
  );
}
