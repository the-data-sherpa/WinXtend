// Text and colour derived from agent data. No DOM, no state: the views call these
// so that the same node is described the same way on every screen.

/// Short form of a node id: the first four bytes, matching `NodeId::short` in the
/// engine and therefore the CLI's output. A user comparing the UI against
/// `wx-agent --status` must see the same string.
export function shortId(hex) {
  if (typeof hex !== "string") return "unknown";
  return hex.slice(0, 8) || "unknown";
}

/// A stable hue per machine, used to colour that machine's screens in the editor.
///
/// Derived from the node id rather than from the row's position, so a machine keeps
/// its colour when another one appears above it in the list — a colour that moves is
/// worse than no colour, because the user has already learned it.
export function nodeHue(hex) {
  const text = typeof hex === "string" ? hex : "";
  let hash = 0;
  for (let i = 0; i < text.length; i += 1) {
    hash = (hash * 31 + text.charCodeAt(i)) % 360;
  }
  // Spread by the golden angle so ids that share a prefix still land far apart.
  return (hash * 137.508) % 360;
}

export function nodeColor(hex, { alpha = 1, lightness = 52 } = {}) {
  const h = Math.round(nodeHue(hex));
  return alpha >= 1
    ? `hsl(${h} 58% ${lightness}%)`
    : `hsl(${h} 58% ${lightness}% / ${alpha})`;
}

export function platformLabel(platform) {
  switch (platform) {
    case "windows":
      return "Windows";
    case "macos":
      return "macOS";
    case "linux":
      return "Linux";
    case "other":
      return "Other";
    default:
      // An agent newer than this UI can report a platform it has never heard of;
      // showing the raw string beats showing nothing.
      return platform ? String(platform) : "Unknown platform";
  }
}

export function displayServerLabel(server) {
  switch (server) {
    case "windows":
      return "Windows";
    case "quartz":
      return "Quartz";
    case "x11":
      return "X11";
    case "wayland":
      return "Wayland";
    case "headless":
      return "Headless";
    default:
      return server ? String(server) : "unknown";
  }
}

/// Human form of a peer's connection state, plus the class the pill is styled with.
export function connectionState(status) {
  const state = status && typeof status === "object" ? status.state : undefined;
  switch (state) {
    case "connected":
      return { label: "Connected", tone: "good" };
    case "connecting":
      return { label: "Connecting", tone: "busy" };
    case "pairing":
      return { label: "Pairing", tone: "busy" };
    case "discovered":
      return { label: "Discovered", tone: "idle" };
    case "offline":
      return { label: "Offline", tone: "idle" };
    case "failed":
      return { label: `Failed: ${status.error || "unknown reason"}`, tone: "bad" };
    default:
      return { label: "Unknown", tone: "idle" };
  }
}

export function uptime(seconds) {
  const total = Number.isFinite(seconds) ? Math.max(0, Math.floor(seconds)) : 0;
  const days = Math.floor(total / 86400);
  const hours = Math.floor((total % 86400) / 3600);
  const minutes = Math.floor((total % 3600) / 60);
  const secs = total % 60;
  if (days > 0) return `${days}d ${hours}h ${minutes}m`;
  if (hours > 0) return `${hours}h ${minutes}m`;
  if (minutes > 0) return `${minutes}m ${String(secs).padStart(2, "0")}s`;
  return `${secs}s`;
}

export function latency(ms) {
  if (!Number.isFinite(ms)) return "not measured";
  return `${ms} ms`;
}

export function clockTime(date) {
  const d = date instanceof Date ? date : new Date();
  const pad = (n) => String(n).padStart(2, "0");
  return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
}

/// Group a six-digit pairing code so it can be read out loud without mistakes.
export function pinGroups(pin) {
  const digits = String(pin || "").replace(/\D/g, "");
  if (digits.length !== 6) return digits;
  return `${digits.slice(0, 3)} ${digits.slice(3)}`;
}

/// Wording for one wx-core layout problem.
///
/// `describe` turns a { node, monitor } reference into a label like
/// "DP-1 on workshop-mac"; it lives in the caller because only the caller knows the
/// names, and a warning naming hex node ids helps nobody.
export function problemText(problem, describe) {
  const names = (problem.refs || []).map(describe);
  switch (problem.kind) {
    case "overlap":
      return `${names[0]} and ${names[1]} overlap. The cursor can only be on one of them, so part of the other cannot be reached.`;
    case "unreachable":
      return `Nothing borders ${names[0]}, so the cursor can never move onto it. Drag it against another screen.`;
    case "degenerate":
      return `${names[0]} has no width or height, so it cannot hold the cursor.`;
    default:
      // A newer agent may report a problem kind this build does not know. Saying so
      // is better than dropping the warning silently.
      return `${names.join(", ") || "A screen"} has a problem this version cannot describe (${problem.kind}).`;
  }
}
