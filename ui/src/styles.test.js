// Whether the banner is actually off the screen when the code hides it.
//
// `renderBanner` says `bannerEl.hidden = true`, which is only half the story: an
// author rule that sets `display` outranks the user-agent `[hidden]` rule and
// leaves the element on screen with its last sentence still in it. That is how a
// window that had just attached went on showing "Not connected to the agent."
// above a header reading "Connected". Asserting `.hidden` would not have caught
// it, so this asserts the display the cascade computes.
//
// vitest runs in the node environment here, so the cascade is worked out from
// the stylesheet directly: author rules beat the user-agent sheet whatever their
// specificity, then higher specificity wins, then later source order. Only the
// selector forms this sheet uses are understood; anything else is treated as not
// matching, which is why the second test checks the visible case too — a helper
// that matched nothing would answer "none" for everything.

import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { bannerFor } from "./banner.js";

const CSS = readFileSync(new URL("./styles.css", import.meta.url), "utf8");
const HTML = readFileSync(new URL("../index.html", import.meta.url), "utf8");

/// The banner as `index.html` actually writes it, in one of its two states.
function bannerElement({ hidden }) {
  const tag = HTML.match(/<([a-z]+)([^>]*\bid="banner"[^>]*)>/i);
  const classes = tag[2].match(/class="([^"]*)"/)?.[1].split(/\s+/).filter(Boolean) ?? [];
  return { tag: tag[1], id: "banner", classes, hidden };
}

/// Every rule in the sheet, innermost first bodies only, so rules inside a
/// `@media` block are read as if the query matched: a `display` that hides the
/// banner has to hold at every width.
const RULES = [...CSS.replace(/\/\*[\s\S]*?\*\//g, "").matchAll(/([^{}]+)\{([^{}]*)\}/g)].map(
  ([, selector, body]) => ({
    selectors: selector.split(",").map((one) => one.trim()),
    display: [...body.matchAll(/(?:^|;)\s*display\s*:\s*([^;!]+)/g)].pop()?.[1].trim(),
  })
);

function matches(compound, element) {
  const parts = compound.match(/\*|[.#]?[\w-]+|\[[^\]]*\]|::?[\w-]+(?:\([^)]*\))?/g) ?? [];
  return parts.every((part) => {
    if (part === "*") return true;
    if (part.startsWith("#")) return part.slice(1) === element.id;
    if (part.startsWith(".")) return element.classes.includes(part.slice(1));
    if (part === "[hidden]") return element.hidden;
    if (part.startsWith("[") || part.startsWith(":")) return false;
    return part === element.tag;
  });
}

function specificity(selector) {
  const parts = selector.match(/\*|[.#]?[\w-]+|\[[^\]]*\]|::?[\w-]+(?:\([^)]*\))?/g) ?? [];
  const count = (test) => parts.filter(test).length;
  return [
    count((p) => p.startsWith("#")),
    count((p) => p.startsWith(".") || p.startsWith("[") || /^:[^:]/.test(p)),
    count((p) => /^[a-z]/i.test(p)),
  ];
}

/// Specificity first, then source order: is `a` the one that wins?
function outranks(a, b) {
  const first = a.rank.findIndex((value, index) => value !== b.rank[index]);
  return first === -1 ? false : a.rank[first] > b.rank[first];
}

/// The `display` the browser would compute for `element` from `styles.css`.
function computedDisplay(element) {
  let winner = null;
  RULES.forEach((rule, order) => {
    if (!rule.display) return;
    for (const selector of rule.selectors) {
      // The key selector decides what the rule is about; an ancestor part that
      // does not hold would only ever remove a rule from the running.
      const key = selector.split(/[\s>+~]+/).filter(Boolean).pop();
      if (!key || !matches(key, element)) continue;
      const candidate = { rank: [...specificity(selector), order], value: rule.display };
      if (!winner || outranks(candidate, winner)) winner = candidate;
    }
  });
  // No author rule spoke, so the user-agent sheet does.
  return winner?.value ?? (element.hidden ? "none" : "block");
}

describe("the banner on screen", () => {
  it("is gone once the window has attached", () => {
    const attached = bannerFor({
      connected: true,
      busy: false,
      daemon: { connected: true, endpoint: { port: 43555, pid: 7706, agentVersion: "0.1.0" } },
      fault: null,
      eventsProblem: null,
    });
    // Nothing to say, so `renderBanner` hides the element — and hiding it has to
    // take the sentence it was last showing off the screen with it.
    expect(attached).toBeNull();
    expect(computedDisplay(bannerElement({ hidden: true }))).toBe("none");
  });

  it("is still laid out as a row when it has something to say", () => {
    expect(computedDisplay(bannerElement({ hidden: false }))).toBe("flex");
  });
});
