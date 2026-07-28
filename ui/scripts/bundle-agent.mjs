// Put the agent, and the systemd unit that starts it, where the Tauri bundler
// will find them.
//
// Without this the installer ships the UI and nothing else: `tauri.conf.json`
// names `binaries/wx-agent` as an `externalBin`, but nothing builds it and the
// bundler only copies what is already on disk. `ui/src-tauri/src/spawn.rs` then
// searches beside the UI executable first — the only location guaranteed to hold
// an agent that matches this UI's protocol version — and on an installed machine
// that is the only one of its three candidates that can ever exist.
//
// Two products, from one input each:
//
//   * `src-tauri/binaries/wx-agent-<target-triple>` — the sidecar. The triple
//     suffix is not decoration: it is how Tauri picks the right binary when a
//     bundle is built for a target that is not the host, and a file without it
//     is invisible to the bundler. It is stripped again inside the bundle, so
//     what lands beside the UI is a plain `wx-agent`.
//   * `packaging/winxtend.service` — the systemd user unit, from the template
//     in the same directory, with `ExecStart` pointing at where the `.deb` puts
//     the agent. Generated rather than checked in so that there is exactly one
//     unit text in the tree; `crates/wx-agent/src/autostart.rs` `include_str!`s
//     the same template for the copy `--install` writes.
//
// Run through `npm run tauri:build`, and by `beforeBuildCommand`, so that
// `tauri build` alone cannot produce a bundle with no daemon in it.

import { execFileSync } from "node:child_process";
import { chmodSync, copyFileSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repo = resolve(here, "..", "..");
const tauriDir = join(repo, "ui", "src-tauri");

// Where the `.deb` puts the agent. Tauri's Debian bundler copies every binary it
// knows about — the main one and each `externalBin` — into `/usr/bin`, so this
// is the path the packaged unit has to name. It is also why the agent ends up on
// `PATH`, which is what makes `wx-agent --status` work after an install.
const INSTALLED_AGENT = "/usr/bin/wx-agent";

function run(command, args, options = {}) {
  return execFileSync(command, args, { encoding: "utf8", stdio: ["ignore", "pipe", "inherit"], ...options });
}

/// The host target triple, as rustc reports it. Overridable so that a
/// cross-build can name the target it is actually bundling for.
function targetTriple() {
  const explicit = process.env.WINXTEND_TARGET_TRIPLE;
  if (explicit) return explicit.trim();
  const line = run("rustc", ["-vV"])
    .split("\n")
    .find((l) => l.startsWith("host:"));
  if (!line) throw new Error("rustc -vV did not report a host triple");
  return line.slice("host:".length).trim();
}

function buildAgent() {
  // Release, always: this is the binary a user runs, and the workspace's dev
  // profile exists to keep input latency tolerable while iterating, not to be
  // shipped.
  console.log("building wx-agent (release)");
  execFileSync("cargo", ["build", "--release", "--package", "wx-agent"], {
    cwd: repo,
    stdio: "inherit",
  });
  return join(repo, "target", "release", process.platform === "win32" ? "wx-agent.exe" : "wx-agent");
}

function placeSidecar(agent, triple) {
  const suffix = process.platform === "win32" ? ".exe" : "";
  const dir = join(tauriDir, "binaries");
  mkdirSync(dir, { recursive: true });
  const target = join(dir, `wx-agent-${triple}${suffix}`);
  copyFileSync(agent, target);
  // `copyFileSync` preserves the mode on Linux, but not reliably across every
  // filesystem and umask combination, and a sidecar the bundler copies without
  // the execute bit produces an installer whose agent cannot be started.
  chmodSync(target, 0o755);
  console.log(`sidecar  ${target}`);
  return target;
}

function generateUnit() {
  const template = readFileSync(join(repo, "packaging", "winxtend.service.in"), "utf8");
  const unit = template.replaceAll("@EXEC_START@", INSTALLED_AGENT);
  if (unit.includes("@EXEC_START@")) throw new Error("the unit template still has a placeholder in it");
  const target = join(repo, "packaging", "winxtend.service");
  writeFileSync(target, unit);
  console.log(`unit     ${target}`);
  return target;
}

const triple = targetTriple();
placeSidecar(buildAgent(), triple);
generateUnit();
