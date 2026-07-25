// The layout editor: every screen in the mesh, in one global coordinate space.
//
// This is the screen the product is really about. Adjacency in WinXtend is geometry,
// not configuration — there are no neighbour lists anywhere in the protocol — so
// arranging rectangles here *is* deciding which edge leads to which machine. That is
// why it looks like an OS display-settings pane: the user already knows that idiom,
// and the mental model it teaches happens to be exactly the truth.
//
// Two rules the editor keeps to:
//
// * Edits are local until saved. The agent assigns the revision, so a half-finished
//   arrangement is never pushed to other machines one drag at a time.
// * An edit is never silently overwritten. If the agent's layout changes while there
//   are unsaved changes here — another UI, a new machine being auto-placed — the user
//   is told and chooses, because losing a careful arrangement to a background event
//   is unforgivable and completely invisible.

import { callOk, changed, log, refreshStatus, store, validateLayout } from "./agent.js";
import { h, replace } from "./dom.js";
import { nodeColor, nodeHue, problemText, shortId } from "./format.js";
import { boundsOf, fitTransform, nextFreeSpot, snap, SNAP_PIXELS } from "./snap.js";

/// Global units nudged by an arrow key, and by an arrow key with Shift held.
const NUDGE = 10;
const NUDGE_FAR = 100;

/// Widest coordinate the wire accepts.
///
/// A placement is an i32 on the wire, so a typed-in coordinate beyond that range
/// would be refused by the agent's deserialiser with a message about JSON types,
/// which tells the user nothing. Clamping in the field keeps the failure impossible
/// instead of merely reported.
const COORD_LIMIT = 2_000_000_000;

const clampCoord = (value) => Math.max(-COORD_LIMIT, Math.min(COORD_LIMIT, Math.round(value)));
const clampSize = (value) => Math.max(0, Math.min(COORD_LIMIT, Math.round(value)));

let root = null;
let canvasEl = null;
let problemsEl = null;

/// The working copy. Never a reference into `store.status`.
let draft = null;
/// JSON of the agent's placements when the draft was taken, for a dirty check that
/// cannot be fooled by a drag that ends where it started.
let baseJson = "[]";
let selectedKey = null;
let problems = [];
let validatedJson = null;
let guides = [];
let drag = null;
/// The agent's layout moved on while there were unsaved edits here.
let staleRemote = false;

const screenEls = new Map();

export function mount(element) {
  root = element;
  // Refit when the window changes size: the transform is derived from the canvas's
  // measured box, so without this the screens keep the scale of the old size.
  window.addEventListener("resize", () => {
    if (!root.hidden) paint();
  });
}

const key = (p) => `${p.node}:${p.monitor}`;

function dirty() {
  return draft ? JSON.stringify(draft.placements) !== baseJson : false;
}

function adopt(remote) {
  draft = {
    revision: remote?.revision ?? 0,
    placements: (remote?.placements || []).map((p) => ({ ...p })),
  };
  baseJson = JSON.stringify(draft.placements);
  staleRemote = false;
  if (selectedKey && !draft.placements.some((p) => key(p) === selectedKey)) {
    selectedKey = null;
  }
}

function syncDraft() {
  const remote = store.status?.layout;
  if (!draft) {
    adopt(remote);
    return;
  }
  if (!dirty()) {
    adopt(remote);
    return;
  }
  if (JSON.stringify(remote?.placements || []) !== baseJson) staleRemote = true;
}

/// Everything the UI knows about every screen in the mesh, placed or not.
///
/// Built from the status snapshot rather than from the layout, because a layout
/// placement carries only a node id and a monitor number — the name, the native
/// resolution and the owning machine's label all live in the snapshot.
function catalogue() {
  const status = store.status;
  const out = new Map();
  if (!status) return out;
  const add = (node, machine, local, monitor) => {
    out.set(`${node}:${monitor.id}`, {
      node,
      machine,
      local,
      monitor: monitor.id,
      name: monitor.name || `Display ${monitor.id}`,
      nativeW: monitor.w,
      nativeH: monitor.h,
      primary: monitor.primary,
      scale: monitor.scale,
    });
  };
  for (const m of status.monitors || []) add(status.nodeId, status.nodeName, true, m);
  for (const peer of status.peers || []) {
    for (const m of peer.monitors || []) add(peer.node, peer.name, false, m);
  }
  return out;
}

function describe(ref, cat = catalogue()) {
  const known = cat.get(`${ref.node}:${ref.monitor}`);
  if (known) return `${known.name} on ${known.machine}`;
  const peer = store.status?.peers?.find((p) => p.node === ref.node);
  const machine =
    ref.node === store.status?.nodeId
      ? store.status.nodeName
      : peer?.name || shortId(ref.node);
  return `display ${ref.monitor} on ${machine}`;
}

function problemKeys() {
  const set = new Set();
  for (const problem of problems) {
    for (const ref of problem.refs || []) set.add(`${ref.node}:${ref.monitor}`);
  }
  return set;
}

// ---------------------------------------------------------------------------
// Painting
// ---------------------------------------------------------------------------

let view = { scale: 0.1, offsetX: 0, offsetY: 0 };

function paint() {
  if (!canvasEl || !draft) return;
  const rects = draft.placements.map((p) => ({ x: p.x, y: p.y, w: p.w, h: p.h }));
  view = fitTransform(boundsOf(rects), {
    w: canvasEl.clientWidth,
    h: canvasEl.clientHeight,
  });
  for (const placement of draft.placements) {
    const el = screenEls.get(key(placement));
    if (!el) continue;
    el.style.left = `${placement.x * view.scale + view.offsetX}px`;
    el.style.top = `${placement.y * view.scale + view.offsetY}px`;
    el.style.width = `${Math.max(2, placement.w * view.scale)}px`;
    el.style.height = `${Math.max(2, placement.h * view.scale)}px`;
  }
  paintGuides();
}

function paintGuides() {
  const existing = canvasEl.querySelectorAll(".guide");
  for (const node of existing) node.remove();
  for (const guide of guides) {
    const el = h("div", { class: `guide ${guide.axis}` });
    if (guide.axis === "x") {
      el.style.left = `${guide.at * view.scale + view.offsetX}px`;
    } else {
      el.style.top = `${guide.at * view.scale + view.offsetY}px`;
    }
    canvasEl.appendChild(el);
  }
}

function paintProblems() {
  const flagged = problemKeys();
  for (const [k, el] of screenEls) el.classList.toggle("warn", flagged.has(k));
  if (!problemsEl) return;
  const cat = catalogue();
  replace(
    problemsEl,
    problems.length === 0
      ? h("p", { class: "muted" }, "No problems: every screen can be reached and none overlap.")
      : h(
          "ul",
          { class: "problem-list" },
          ...problems.map((problem) =>
            h(
              "li",
              { class: `problem ${problem.kind}` },
              h("span", { class: "problem-kind" }, problem.kind),
              h("span", {}, problemText(problem, (ref) => describe(ref, cat)))
            )
          )
        )
  );
}

/// Re-run the engine's validator, unless it has already seen this exact layout.
///
/// Guarded by the last validated JSON so that painting the result cannot trigger
/// another validation and loop.
function ensureValidated() {
  if (!draft) return;
  const json = JSON.stringify(draft.placements);
  if (json === validatedJson) return;
  validatedJson = json;
  validateLayout(draft).then((list) => {
    problems = list;
    paintProblems();
  });
}

// ---------------------------------------------------------------------------
// Editing
// ---------------------------------------------------------------------------

function placementFor(k) {
  return draft?.placements.find((p) => key(p) === k) || null;
}

function others(k) {
  return draft.placements
    .filter((p) => key(p) !== k)
    .map((p) => ({ x: p.x, y: p.y, w: p.w, h: p.h }));
}

function beginDrag(event, placement) {
  if (event.button !== 0) return;
  const el = screenEls.get(key(placement));
  event.preventDefault();
  selectedKey = key(placement);
  drag = {
    key: selectedKey,
    startX: event.clientX,
    startY: event.clientY,
    originX: placement.x,
    originY: placement.y,
    moved: false,
  };
  el.setPointerCapture(event.pointerId);
  el.classList.add("dragging");
  renderInspector();
}

function moveDrag(event) {
  if (!drag) return;
  const placement = placementFor(drag.key);
  if (!placement) return;
  const dx = (event.clientX - drag.startX) / view.scale;
  const dy = (event.clientY - drag.startY) / view.scale;
  if (Math.abs(event.clientX - drag.startX) > 2 || Math.abs(event.clientY - drag.startY) > 2) {
    drag.moved = true;
  }
  const raw = {
    x: drag.originX + dx,
    y: drag.originY + dy,
    w: placement.w,
    h: placement.h,
  };
  // The threshold is converted from screen pixels to global units so that snapping
  // feels the same whatever the current fit scale is.
  const snapped = snap(raw, others(drag.key), SNAP_PIXELS / view.scale);
  placement.x = clampCoord(snapped.x);
  placement.y = clampCoord(snapped.y);
  guides = snapped.guides;
  paint();
}

function endDrag(event) {
  if (!drag) return;
  const el = screenEls.get(drag.key);
  if (el) {
    el.classList.remove("dragging");
    if (el.hasPointerCapture?.(event.pointerId)) el.releasePointerCapture(event.pointerId);
  }
  drag = null;
  guides = [];
  paintGuides();
  ensureValidated();
  renderToolbar();
  renderInspector();
}

function nudge(placement, dx, dy) {
  placement.x = clampCoord(placement.x + dx);
  placement.y = clampCoord(placement.y + dy);
  paint();
  ensureValidated();
  renderToolbar();
  renderInspector();
}

function edit(placement, patch) {
  Object.assign(placement, patch);
  paint();
  ensureValidated();
  renderToolbar();
  renderInspector();
}

async function save() {
  try {
    // The revision is deliberately whatever the draft carries: the agent overwrites
    // it with its own next revision, which is what stops two open windows from both
    // claiming the same one.
    await callOk({ kind: "setLayout", layout: draft });
    await refreshStatus();
    adopt(store.status?.layout);
    log("info", "Saved the layout");
    render();
  } catch (error) {
    log("error", `Saving the layout: ${error.message}`);
    changed();
  }
}

async function autoArrange(nodeOnly) {
  try {
    const response = await callOk(
      nodeOnly ? { kind: "autoLayout", node: nodeOnly } : { kind: "autoLayout" }
    );
    if (response.kind === "layout") {
      adopt(response.layout);
      if (store.status) store.status.layout = response.layout;
    }
    log("info", "Rearranged the screens automatically");
    render();
  } catch (error) {
    log("error", `Rearranging: ${error.message}`);
    changed();
  }
}

function placeScreen(entry) {
  const spot = nextFreeSpot(
    draft.placements.map((p) => ({ x: p.x, y: p.y, w: p.w, h: p.h })),
    { w: entry.nativeW, h: entry.nativeH }
  );
  draft.placements.push({
    node: entry.node,
    monitor: entry.monitor,
    x: spot.x,
    y: spot.y,
    w: entry.nativeW,
    h: entry.nativeH,
    cursor_scale: 1,
  });
  selectedKey = `${entry.node}:${entry.monitor}`;
  render();
}

function unplace(k) {
  draft.placements = draft.placements.filter((p) => key(p) !== k);
  if (selectedKey === k) selectedKey = null;
  render();
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

let toolbarEl = null;
let inspectorEl = null;

function renderToolbar() {
  if (!toolbarEl) return;
  const isDirty = dirty();
  replace(
    toolbarEl,
    h(
      "span",
      { class: "muted" },
      `Revision ${draft?.revision ?? 0}${isDirty ? " · unsaved changes" : ""}`
    ),
    h(
      "div",
      { class: "row" },
      h(
        "button",
        { type: "button", onClick: () => autoArrange(null) },
        "Arrange automatically"
      ),
      h(
        "button",
        {
          type: "button",
          disabled: !isDirty,
          onClick: () => {
            adopt(store.status?.layout);
            render();
          },
        },
        "Discard changes"
      ),
      h(
        "button",
        { type: "button", class: "primary", disabled: !isDirty, onClick: save },
        "Save layout"
      )
    )
  );
}

function screenElement(placement, entry, cursorKey) {
  const k = key(placement);
  const node = placement.node;
  const label = entry?.name || `Display ${placement.monitor}`;
  const machine =
    entry?.machine ||
    (node === store.status?.nodeId ? store.status.nodeName : shortId(node));
  const el = h(
    "div",
    {
      class: `screen${selectedKey === k ? " selected" : ""}${k === cursorKey ? " has-cursor" : ""}`,
      tabIndex: 0,
      dataset: { key: k },
      style: {
        borderColor: nodeColor(node),
        background: `hsl(${Math.round(nodeHue(node))} 45% 50% / 0.18)`,
      },
      title: `${label} on ${machine}`,
      onPointerDown: (e) => beginDrag(e, placement),
      onPointerMove: moveDrag,
      onPointerUp: endDrag,
      onPointerCancel: endDrag,
      onDblClick: () => warpTo(placement),
      onFocus: () => {
        selectedKey = k;
        renderInspector();
      },
      onKeyDown: (e) => {
        const step = e.shiftKey ? NUDGE_FAR : NUDGE;
        const moves = {
          ArrowLeft: [-step, 0],
          ArrowRight: [step, 0],
          ArrowUp: [0, -step],
          ArrowDown: [0, step],
        };
        const move = moves[e.key];
        if (!move) return;
        e.preventDefault();
        nudge(placement, move[0], move[1]);
      },
    },
    h("span", { class: "screen-machine" }, machine),
    h("span", { class: "screen-name" }, label),
    h("span", { class: "screen-size" }, `${placement.w} x ${placement.h}`)
  );
  screenEls.set(k, el);
  return el;
}

async function warpTo(placement) {
  try {
    await callOk({
      kind: "warpCursor",
      node: placement.node,
      monitor: placement.monitor,
      x: 0.5,
      y: 0.5,
    });
  } catch (error) {
    log("error", `Moving the cursor there: ${error.message}`);
    changed();
  }
}

function renderInspector() {
  if (!inspectorEl) return;
  const cat = catalogue();
  const placed = new Set(draft.placements.map(key));
  const unplaced = [...cat.values()].filter((e) => !placed.has(`${e.node}:${e.monitor}`));
  const placement = selectedKey ? placementFor(selectedKey) : null;
  const entry = placement ? cat.get(selectedKey) : null;

  // Reflect the selection in the canvas without a rebuild, so a click does not
  // interrupt a drag that is still in flight.
  for (const [k, el] of screenEls) el.classList.toggle("selected", k === selectedKey);

  const numberField = (label, value, onChange, min) =>
    h(
      "label",
      { class: "field" },
      h("span", {}, label),
      h("input", {
        type: "number",
        value: String(value),
        min: min === undefined ? undefined : String(min),
        step: "1",
        onChange: (e) => {
          const next = Number.parseInt(e.target.value, 10);
          if (Number.isFinite(next)) onChange(next);
        },
      })
    );

  replace(
    inspectorEl,
    placement
      ? h(
          "div",
          { class: "inspect" },
          h("h2", {}, entry?.name || `Display ${placement.monitor}`),
          h(
            "p",
            { class: "muted small" },
            `${entry?.machine || shortId(placement.node)} · id ${shortId(placement.node)} · display ${placement.monitor}`
          ),
          entry
            ? h(
                "p",
                { class: "muted small" },
                `Native ${entry.nativeW} x ${entry.nativeH} at scale ${entry.scale}${entry.primary ? ", primary" : ""}`
              )
            : h(
                "p",
                { class: "warning-text" },
                "This screen is in the layout but its machine is not reporting it right now."
              ),
          h(
            "div",
            { class: "fields" },
            numberField("X", placement.x, (v) => edit(placement, { x: clampCoord(v) })),
            numberField("Y", placement.y, (v) => edit(placement, { y: clampCoord(v) })),
            numberField("Width", placement.w, (v) => edit(placement, { w: clampSize(v) }), 0),
            numberField("Height", placement.h, (v) => edit(placement, { h: clampSize(v) }), 0)
          ),
          entry && (entry.nativeW !== placement.w || entry.nativeH !== placement.h)
            ? h(
                "button",
                {
                  type: "button",
                  onClick: () => edit(placement, { w: entry.nativeW, h: entry.nativeH }),
                },
                "Match native resolution"
              )
            : null,
          h(
            "label",
            { class: "field slider" },
            h(
              "span",
              {},
              `Cursor speed on this screen: ${Number(placement.cursor_scale ?? 1).toFixed(2)}x`
            ),
            h("input", {
              type: "range",
              min: "0.25",
              max: "3",
              step: "0.05",
              value: String(placement.cursor_scale ?? 1),
              onInput: (e) => edit(placement, { cursor_scale: Number(e.target.value) }),
            })
          ),
          h(
            "p",
            { class: "muted small" },
            "A high-resolution panel placed at its native size can feel slow next to a " +
              "smaller one. This multiplies how far the pointer travels while it is here."
          ),
          h(
            "div",
            { class: "row" },
            h(
              "button",
              { type: "button", onClick: () => warpTo(placement) },
              "Move the cursor here"
            ),
            h(
              "button",
              {
                type: "button",
                class: "danger-quiet",
                onClick: () => unplace(selectedKey),
              },
              "Remove from layout"
            )
          )
        )
      : h(
          "div",
          { class: "inspect" },
          h("h2", {}, "No screen selected"),
          h(
            "p",
            { class: "muted" },
            "Click a screen to change its position, size or cursor speed. Drag to " +
              "rearrange; screens snap to their neighbours' edges. Double-click to send " +
              "the cursor there."
          )
        ),
    unplaced.length
      ? h(
          "div",
          { class: "inspect" },
          h("h2", {}, "Not in the layout"),
          h(
            "p",
            { class: "muted small" },
            "These screens exist but have no place in the shared desktop, so the cursor " +
              "cannot reach them."
          ),
          h(
            "ul",
            { class: "unplaced" },
            ...unplaced.map((entry) =>
              h(
                "li",
                {},
                h("span", { class: "swatch small", style: { background: nodeColor(entry.node) } }),
                h("span", {}, `${entry.name} on ${entry.machine}`),
                h(
                  "button",
                  { type: "button", onClick: () => placeScreen(entry) },
                  "Add"
                )
              )
            )
          )
        )
      : null
  );
}

export function render() {
  if (!root || root.hidden) return;
  // A rebuild mid-drag would replace the element holding the pointer capture.
  if (drag) return;
  if (!store.status) {
    replace(root, h("div", { class: "empty" }, h("p", {}, "Not connected to an agent.")));
    return;
  }
  syncDraft();
  screenEls.clear();

  const cat = catalogue();
  const cursor = store.status.cursor;
  const cursorKey =
    cursor && Number.isInteger(cursor.monitor) ? `${cursor.owner}:${cursor.monitor}` : null;

  toolbarEl = h("div", { class: "toolbar" });
  canvasEl = h(
    "div",
    {
      class: "canvas",
      onPointerDown: (e) => {
        // A click on the background clears the selection, the same as a file manager.
        if (e.target === canvasEl) {
          selectedKey = null;
          renderInspector();
        }
      },
    },
    ...draft.placements.map((p) => screenElement(p, cat.get(key(p)), cursorKey))
  );
  inspectorEl = h("aside", { class: "inspector" });
  problemsEl = h("div", { class: "problems" });

  replace(
    root,
    h(
      "section",
      { class: "panel layout-panel" },
      h(
        "header",
        {},
        h("h1", {}, "Layout"),
        h(
          "span",
          { class: "muted" },
          "One shared desktop. An edge that touches another screen is a way onto it."
        )
      ),
      toolbarEl,
      staleRemote
        ? h(
            "div",
            { class: "notice warning" },
            h(
              "span",
              {},
              "The agent's layout changed while you were editing. Your changes are still here and have not been sent."
            ),
            h(
              "button",
              {
                type: "button",
                onClick: () => {
                  adopt(store.status?.layout);
                  render();
                },
              },
              "Discard mine and reload"
            )
          )
        : null,
      draft.placements.length === 0
        ? h(
            "div",
            { class: "empty" },
            h("h2", {}, "Nothing is placed yet"),
            h(
              "p",
              {},
              "Add this machine's screens from the panel on the right, or let the agent " +
                "arrange everything it knows about."
            )
          )
        : null,
      h("div", { class: "editor" }, canvasEl, inspectorEl),
      h(
        "div",
        { class: "problems-shell" },
        h("h2", {}, "Warnings"),
        problemsEl
      )
    )
  );

  renderToolbar();
  renderInspector();
  paintProblems();
  requestAnimationFrame(paint);
  ensureValidated();
}
