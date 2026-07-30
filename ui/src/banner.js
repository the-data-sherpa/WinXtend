// What the connection banner says, decided as data rather than as DOM.
//
// The banner is the only place the interface admits that nothing is working, so
// every wrong sentence here sends someone to look in the wrong place. Two rules
// come out of that, and they are why this is a pure function with tests rather
// than a branch inside a render:
//
//   * **Never claim more than was observed.** "The agent is not running" is a
//     statement about the daemon, and it may only be made after the window has
//     actually looked and found no endpoint. A window that failed to ask its own
//     host process, or that has not asked yet, knows nothing about the daemon and
//     has to say so — a user who can see `wx-agent` in `systemctl --user status`
//     stops believing anything the window says after being told it is not there.
//   * **Name the missing thing.** When something specific is broken — the event
//     bus, the endpoint file, the binary — the banner says which, because that is
//     the difference between a bug report and a fixable problem.

/// Decide the banner from everything the frontend knows.
///
/// Returns `null` when there is nothing worth saying, or an object with:
///
///   * `tone` — `"info"` while something is in progress, `"warning"` otherwise.
///   * `headline`, `detail`, `hint` — text; `detail` and `hint` may be empty.
///   * `actions` — which buttons to offer, from `"start"` and `"retry"`.
export function bannerFor({ connected, busy, daemon, fault, eventsProblem }) {
  if (busy) {
    return { tone: "info", headline: "Talking to the agent...", detail: "", hint: "", actions: [] };
  }

  // Attached and being kept up to date: the screen speaks for itself.
  if (connected && !eventsProblem) return null;

  // Attached, but nothing will arrive unasked. Worth saying: the numbers on
  // screen are right now and silently will not be in a minute.
  if (connected) {
    return {
      tone: "warning",
      headline: "Connected, but this window is not receiving live updates.",
      detail: `${eventsProblem}. What is on screen is a snapshot and will quietly stop matching the agent.`,
      hint: "Pairing needs this: nothing arrives as it happens — a request from another machine, and the answer to one started here, both surface when this window next reads the agent's status, such as when you bring it to the front or while the Devices or Status tab is open.",
      actions: [],
    };
  }

  const code = fault?.code;
  // Carried on every disconnected banner rather than replacing it: a window can
  // be both unable to find the agent and unable to receive its events, and the
  // second is the one that explains why the first never fixes itself.
  const events = eventsProblem ? `This window also cannot receive agent events: ${eventsProblem}.` : "";

  // Nothing has been established about the daemon yet, one way or the other.
  if (!daemon) {
    if (!fault) {
      return {
        tone: "info",
        headline: "Looking for the WinXtend agent...",
        detail: "",
        hint: events,
        actions: [],
      };
    }
    return {
      tone: "warning",
      headline: "This window could not check whether the agent is running.",
      detail: `${fault.message}. The fault is in the window, not in the agent: sharing may well be running, and this window cannot see it.`,
      hint: events,
      actions: ["retry"],
    };
  }

  // The host looked and could not tell. `endpoint` is null here for the same
  // reason it is null when no agent exists, so the reason it carries is the only
  // thing separating "there is no daemon" from "this window cannot see the daemon
  // that is running" — and saying the first when the second is true is what sends
  // a user to restart a service that never stopped.
  if (!daemon.endpoint && daemon.message) {
    return {
      tone: "warning",
      headline: "This window could not read the agent's details.",
      detail: `${daemon.message}. Whether an agent is running cannot be told from here: sharing may well be going on.`,
      hint: events,
      actions: ["retry"],
    };
  }

  if (code === "notRunning" || !daemon.endpoint) {
    return {
      tone: "warning",
      headline: "The WinXtend agent is not running.",
      detail: daemon.agentPath
        ? "Nothing is being shared. The agent is the process that captures the keyboard and mouse; this window only configures it."
        : "The agent program could not be found next to this application, so it cannot be started from here.",
      hint: daemon.agentPath
        ? events
        : join("Set WINXTEND_AGENT to the path of wx-agent to start it from here.", events),
      actions: daemon.agentPath ? ["start"] : [],
    };
  }

  if (code === "stale") {
    return {
      tone: "warning",
      headline: "An agent left its details behind but is not answering.",
      detail: `${fault.message}. It was probably killed rather than asked to stop. Starting it again is safe.`,
      hint: events,
      actions: ["start", "retry"],
    };
  }

  if (code === "disconnected" || code === "notConnected") {
    return {
      tone: "warning",
      headline: "The connection to the agent was lost.",
      detail: `${fault.message}. Keyboard and mouse sharing carries on without this window.`,
      hint: events,
      actions: ["retry"],
    };
  }

  if (fault) {
    return {
      tone: "warning",
      headline: "The agent could not be reached.",
      detail: fault.message,
      hint: events,
      actions: ["retry"],
    };
  }

  // An endpoint is there and nothing has failed: the attach is simply not made
  // yet, which is what the window looks like for the moment after it is reopened.
  return {
    tone: "warning",
    headline: "Not connected to the agent.",
    detail: `An agent is listening on port ${daemon.endpoint.port}.`,
    hint: events,
    actions: ["retry"],
  };
}

function join(...parts) {
  return parts.filter(Boolean).join(" ");
}
