// A three-function element builder.
//
// Everything on these screens is built with `h` and text nodes rather than with
// template strings assigned to innerHTML, and that is a security decision, not a
// style one: peer names, monitor names and error strings all arrive from other
// machines over the network. A peer that advertises itself as
// `<img src=x onerror=...>` must end up displayed as those characters, and
// `textContent` guarantees that where an HTML template does not.

/// Build an element. `props` sets properties (not attributes) except for `class`,
/// `dataset`, `style` and `on*` handlers, which are special-cased.
export function h(tag, props = {}, ...children) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(props || {})) {
    if (value === undefined || value === null || value === false) continue;
    if (key === "class") node.className = value;
    else if (key === "dataset") Object.assign(node.dataset, value);
    else if (key === "style") Object.assign(node.style, value);
    else if (key.startsWith("on") && typeof value === "function") {
      node.addEventListener(key.slice(2).toLowerCase(), value);
    } else if (key === "text") node.textContent = String(value);
    else node[key] = value;
  }
  append(node, children);
  return node;
}

function append(node, children) {
  for (const child of children) {
    if (child === undefined || child === null || child === false) continue;
    if (Array.isArray(child)) append(node, child);
    else if (child instanceof Node) node.appendChild(child);
    else node.appendChild(document.createTextNode(String(child)));
  }
}

export function clear(node) {
  while (node.firstChild) node.removeChild(node.firstChild);
  return node;
}

export function replace(node, ...children) {
  clear(node);
  append(node, children);
  return node;
}
