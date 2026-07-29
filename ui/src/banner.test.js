import { describe, expect, it } from "vitest";
import { bannerFor } from "./banner.js";

const FOUND = {
  connected: false,
  configDir: "/home/someone/.config/winxtend",
  endpoint: { port: 43555, pid: 7706, agentVersion: "0.1.0" },
  agentPath: "/usr/bin/wx-agent",
  uiVersion: "0.1.0",
  message: null,
};

const ACL = "Command plugin:event|listen not allowed by ACL";

/// Every sentence the banner can produce, in one string, for "must not say" checks.
function words(banner) {
  return [banner.headline, banner.detail, banner.hint].join(" ");
}

describe("what the banner is allowed to claim", () => {
  it("does not call the agent stopped when the window never managed to look", () => {
    const banner = bannerFor({
      connected: false,
      busy: false,
      daemon: null,
      fault: { code: "internal", message: ACL },
      eventsProblem: null,
    });
    // The reported bug rendered exactly this state as "The WinXtend agent is not
    // running", while `systemctl --user status winxtend` said otherwise. A window
    // that could not ask knows nothing about the daemon.
    expect(words(banner)).not.toMatch(/agent is not running/i);
    expect(banner.detail).toContain(ACL);
  });

  it("names the missing thing rather than blaming the agent", () => {
    const banner = bannerFor({
      connected: false,
      busy: false,
      daemon: null,
      fault: null,
      eventsProblem: ACL,
    });
    expect(banner.hint).toContain("plugin:event|listen");
  });

  it("says it is still looking before the first answer comes back", () => {
    const banner = bannerFor({
      connected: false,
      busy: false,
      daemon: null,
      fault: null,
      eventsProblem: null,
    });
    expect(banner.tone).toBe("info");
    expect(words(banner)).not.toMatch(/agent is not running/i);
    // Nothing to retry or start: nothing has failed yet.
    expect(banner.actions).toEqual([]);
  });

  it("does not call the agent stopped when the endpoint file could not be read", () => {
    // Same claim, host-side path: `endpoint` is null here because `ipc.json` would
    // not parse, not because it was absent. A daemon under `systemctl --user` can
    // be running behind an unreadable file, so the window has to name the file it
    // could not read instead of announcing a stopped agent.
    const banner = bannerFor({
      connected: false,
      busy: false,
      daemon: {
        ...FOUND,
        endpoint: null,
        message: "the agent's details at /home/someone/.config/winxtend/ipc.json could not be read",
      },
      fault: null,
      eventsProblem: null,
    });
    expect(words(banner)).not.toMatch(/agent is not running/i);
    expect(banner.detail).toContain("ipc.json");
    expect(banner.actions).toEqual(["retry"]);
  });

  it("says the agent is not running once the host has looked and found nothing", () => {
    const banner = bannerFor({
      connected: false,
      busy: false,
      daemon: { ...FOUND, endpoint: null },
      fault: null,
      eventsProblem: null,
    });
    expect(banner.headline).toMatch(/agent is not running/i);
    expect(banner.actions).toEqual(["start"]);
  });

  it("offers no start button when the agent binary cannot be found", () => {
    const banner = bannerFor({
      connected: false,
      busy: false,
      daemon: { ...FOUND, endpoint: null, agentPath: null },
      fault: null,
      eventsProblem: null,
    });
    expect(banner.actions).toEqual([]);
    expect(banner.hint).toContain("WINXTEND_AGENT");
  });

  it("distinguishes an endpoint file nothing answers from a missing one", () => {
    const banner = bannerFor({
      connected: false,
      busy: false,
      daemon: FOUND,
      fault: { code: "stale", message: "nothing is listening on port 43555" },
      eventsProblem: null,
    });
    expect(banner.headline).toMatch(/left its details behind/i);
    expect(banner.actions).toEqual(["start", "retry"]);
  });

  it("keeps quiet once attached and receiving events", () => {
    expect(
      bannerFor({
        connected: true,
        busy: false,
        daemon: { ...FOUND, connected: true },
        fault: null,
        eventsProblem: null,
      })
    ).toBeNull();
  });

  it("admits when an attached window has stopped being told anything", () => {
    const banner = bannerFor({
      connected: true,
      busy: false,
      daemon: { ...FOUND, connected: true },
      fault: null,
      eventsProblem: ACL,
    });
    // Connected is not the same as current, and a screen that silently freezes is
    // the one failure a user cannot see.
    expect(banner.headline).toMatch(/not receiving live updates/i);
    expect(banner.detail).toContain(ACL);
    // And does not wave it away. Pairing is driven entirely by these events, so a
    // window without them cannot show an incoming request or finish an outgoing
    // one; "sharing is unaffected" would send the user to try pairing and watch it
    // hang. Nor does it promise a refresh it offers no button for.
    expect(words(banner)).toMatch(/pairing/i);
    expect(words(banner)).not.toMatch(/unaffected/i);
    expect(words(banner)).not.toMatch(/refresh/i);
    expect(banner.actions).toEqual([]);
  });
});
