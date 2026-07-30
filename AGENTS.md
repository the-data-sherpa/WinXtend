# Project agent memory

This file is the project's committed home for project-intrinsic agent knowledge: build, test, release, architecture, and sharp-edge notes that should travel with the code.

- Add durable project-specific notes here as they are discovered through real work.

## There are two Cargo workspaces, not one

`ui/src-tauri` is its own workspace on purpose (the reasoning is in its `Cargo.toml`).
A root `cargo test --workspace` or `cargo clippy --workspace` **does not reach it** — it
needs its own invocations run from its own directory. `.github/workflows/ci.yml` is the
authoritative list of what "validated" means; run those commands, both jobs, not just the
root ones.

`ui/src-tauri/.cargo/config.toml` redirects `target-dir` to `../../target/ui`, so that
crate's artefacts never appear under `ui/src-tauri/target`.

## Platform backends compile everywhere on purpose

`crates/wx-platform/src/lib.rs` declares the macOS/Wayland/X11/evdev modules
unconditionally so that a change to `traits` breaks them immediately rather than on a
machine nobody has. Do not `cfg`-gate the module declarations to make a build pass; fix
the skeletons.

Related: clippy's `-D warnings` fires per-target because those `cfg` blocks differ, so a
lint can be clean on Linux and fail on Windows or macOS (or the reverse — see `f41c426`).
Fix the lint on the platform that reports it rather than dropping the flag.

## Constructing a platform backend must not prompt

`wx_platform::current_platform()` builds a backend and acquires nothing. Only
`current_platform_in(config_dir)` — which the daemon calls, and nothing else does — is
allowed to ask the OS for permission. The split exists because acquiring input
permission on Wayland means an `xdg-desktop-portal` consent dialog, and several tests
call `current_platform()`; a `cargo test` run that put a dialog on the developer's
screen would be indefensible. Any new backend with a permission step must keep that
line. See the doc comments on both functions.

`PlatformInfo::capabilities` is only the answer at startup. Where permission can be
granted or withdrawn while the process runs, `PlatformBackend::current_capabilities()`
is the authority, and `Engine::sync_capabilities` re-advertises to peers when it
changes.

A **grant is not a capability**, and on Wayland the two are apart for a window at the
start of every session: the portal grants, and the libei devices that carry an
injection arrive afterwards. Publish what can be served *now* and republish when that
changes — `serviceable` and `SharedSession::regrant` in
`crates/wx-platform/src/linux_wayland/` are that split. Any backend whose permission
and transport come up separately has the same window.

## A test total that differs by platform is expected

`cargo test --workspace` legitimately reports a different total on Linux, Windows and
macOS; that is the `cfg` gating above, not a broken checkout. The README's
[Why the test count differs by platform](README.md#why-the-test-count-differs-by-platform)
owns the explanation of which gates cause it. The rule that matters here: the command
is the only authority on the current figure, so no count is quoted in this file or in
the README — don't reintroduce one.

## Testing Linux/Wayland backends without a second machine

Wayland work (`crates/wx-platform/src/linux_wayland/`) can be exercised against a
real compositor on one box, with no extra packages:

- **Multi-monitor, mixed-DPI, hotplug** — run a private nested compositor on its
  own D-Bus session, then drive it through Mutter's `DisplayConfig`:

  ```sh
  eval "$(dbus-daemon --session --print-address --fork --print-pid \
    | { read a; read p; echo "export DBUS_SESSION_BUS_ADDRESS='$a'"; })"
  gnome-shell --headless --wayland --wayland-display=wxtest \
    --virtual-monitor 2560x1440 --virtual-monitor 1920x1080 &
  # then run the client with WAYLAND_DISPLAY=wxtest
  ```

  `ApplyMonitorsConfig` on that bus changes scale, rotation and position live, and
  applying a config that omits a monitor *is* an unplug as far as a client sees.
  Note current gnome-shell has no `--nested`, and Mutter rejects negative logical
  positions.

- **Fractional scaling on the real session** — `GetCurrentState` /
  `ApplyMonitorsConfig` on `org.gnome.Mutter.DisplayConfig` is the ground truth to
  compare against; supported scales are listed per mode.

- **Input injection, end to end, with no second machine** — drive the real
  `WaylandInjector` from a scratch binary and read the result back rather than
  eyeballing it. `zenity --entry --timeout=45` prints its contents to stdout when
  Enter is injected, which turns "did it type?" into an assertion; for pointer work
  a small GTK4 window with `Gtk.EventControllerMotion`/`GestureClick`/
  `EventControllerScroll` printing each event is the equivalent. Always inject into
  a window the test owns — the events go to whatever has focus.

  Two traps when testing across layouts. `gsettings set org.gnome.desktop.input-sources
  current` is **ignored** by GNOME Shell, so it does not switch the active layout;
  inject `<Super>space` (the real `switch-input-source` binding) instead, and confirm
  the switch happened by injecting a `KeyPayload::RawKeyCode` for a key whose meaning
  differs between the two layouts. Changing `input-sources sources` does work, so
  restore it from a shell `trap` rather than at the end of the script.

- **Headless (CI)** — anything that opens a Wayland connection must skip, not
  fail. Reproduce with
  `env -u WAYLAND_DISPLAY -u DISPLAY -u XDG_SESSION_TYPE XDG_RUNTIME_DIR=$(mktemp -d) cargo test`.

- **End-to-end without the UI** — `wx-agent --config-dir <scratch> --status`
  prints the same `StatusSnapshot` the Tauri layout editor renders. Always pass
  `--config-dir`; the default writes an identity key into the user's real config
  directory. The UI honours `WINXTEND_CONFIG_DIR` and `WINXTEND_AGENT` for the
  same reason.

  Two agents on one host bring up, pair and lay themselves out fine — the
  first-pass layout does **not** reliably put the second machine on the right, so
  read the placements rather than assuming a direction. Passing the *cursor*
  between them on one host is a different matter and is confounded, not merely
  fiddly: both agents arm pointer barriers on every edge of the same screen and
  the compositor allows only one active capture, and they then fight over one
  physical pointer, because the "remote" machine's injection warps the very
  pointer the "local" machine's capture has pinned. Budget for that before
  spending a captain's afternoon on it; a second machine is the honest test, which
  is what issue #11 is for.

  A capture session pins the pointer and swallows the keyboard, so a test that
  wedges leaves whoever is at the machine unable to recover. Never start one
  without a bounded lifetime and a way to kill the agents from another terminal,
  and say so before you run it. Denying the *Input Capture* consent dialog is a
  legitimate way to run a clipboard or layout test with no pointer hazard at all —
  the clipboard rides the `RemoteDesktop` session, so it is unaffected — and only
  `RemoteDesktop` has a restore token, so that is also the only dialog a relaunch
  skips.

  **The clipboard is confounded on one host in the same way the cursor is, and
  worse.** Two agents in one desktop session share one physical clipboard, so
  "machine A's clipboard" and "machine B's clipboard" are the same object. Every
  message still flows and every payload still round-trips byte-exact, so the
  transport, the format gating and the size limits are all genuinely testable this
  way — but the write-back suppression in `crates/wx-agent/src/clipboard.rs` looks
  broken and is not: the agent that *wrote* absorbs its own change correctly, and
  the other agent then sees that write as a change nobody told it about and offers
  it back. Read the two agents' logs separately — the writer must log zero
  "offering the clipboard" — rather than concluding from the pair that there is a
  loop. Between two real machines the second agent's clipboard never moves and the
  exchange terminates. Issue #11 is the honest test here too.

GNOME denies programmatic screenshots (`org.gnome.Shell.Screenshot`) to untrusted
callers, so visual confirmation of the UI needs a human or a portal prompt.
`org.gnome.Shell.Introspect.GetWindows` is refused the same way, so a nested shell
cannot be asked what it is showing either; instrument the process instead.

A second agent can be run on a live desktop without ever reaching the portal: with
`WAYLAND_DISPLAY`, `DISPLAY` and `XDG_SESSION_TYPE` all unset, `detect_display_server`
picks the evdev backend, which asks for nothing and reports "input capture
unavailable". That is the safe way to exercise the mesh, the control channel, or the
UI beside someone's working session — including from a throwaway systemd user unit,
where `UnsetEnvironment=` does it — because nothing can then take the keyboard.

The backend is split to keep that reachable from CI: the parts needing no desktop —
restore-token persistence in `token.rs`, the session state machine in `session.rs`, the
event translation in `capture.rs`, the clipboard content rules in `clipboard.rs` — are
deliberately separated from everything touching D-Bus or libei — `driver.rs`,
`capture_driver.rs` and `clipboard_portal.rs`, behind `cfg(target_os = "linux")`, the
first two with a stub alongside them because the backend is assembled from them on
every target — so this crate compiles and tests on all three platforms with no
session. Keep that division.

## Wayland input needs two portals, and they are not interchangeable

`org.freedesktop.portal.RemoteDesktop` **drives** a desktop and cannot capture from
one: its session has no zones, barriers, `Enable`/`Release` or activation, and its
libei devices are client-owned emulation devices. Capture is
`org.freedesktop.portal.InputCapture`, which GNOME 50 is the first release to ship.
So the backend holds two sessions and the user answers two consent dialogs per
launch; only `RemoteDesktop` has a restore token at the versions the alpha target
has. `SharedSession` in `crates/wx-platform/src/linux_wayland/session.rs` is
parameterised by which capability bits it owns precisely so one being revoked cannot
unadvertise the other.

The clipboard is a third interface and deliberately **not** a third session:
`org.freedesktop.portal.Clipboard` refuses `RequestClipboard` for anything that is
not a `RemoteDesktop` session, so it rides that one — asked for between
`SelectDevices` and `Start`, which is the only window the portal accepts it in.
One dialog, two independent toggles: `Start` answers `clipboard_enabled` separately
from the device list, so the session publishes `INJECT_CAPABILITIES` and
`CLIPBOARD_CAPABILITIES` independently and either can be withheld. Everything
measured about it — why no `wlr`/`ext` data-control route exists on this target,
and the portal calls that fail — is in the module docs of
`crates/wx-platform/src/linux_wayland/clipboard.rs` and `clipboard_portal.rs`.

Everything measured about the capture side — that suppression is real and
exclusive, that the compositor sends a capturing client no modifier state and no
absolute pointer motion, that it emits no `Deactivated` for a client-initiated
`Release`, and the barrier geometry it silently refuses — is in the module docs of
`crates/wx-platform/src/linux_wayland/capture.rs` and `capture_driver.rs`. Read
those before changing anything there; each line of them cost a run on real
hardware, and every one of those failures is silent.

## One keymap index serves both directions, and reads three spellings

`crates/wx-platform/src/linux_wayland/keymap.rs` answers both "which key produces
this character" (injection) and "what does this key produce" (capture) from one
parse, so the two cannot disagree. It is hand-written rather than bound to
`libxkbcommon`, and the reason is in its module docs — it applies to the X11
backend too, which should reuse it rather than link the C library.

Keymaps arrive in more than one legal spelling and the differences fail *silently*.
mutter writes `map[Shift]= 2` and numeric keysyms; `xkbcomp -xkb` writes
`map[Shift]= Level2` and names like `dead_diaeresis`. Missing either leaves shifted
levels or dead keys unreachable while everything still appears to work — `Å` simply
types as `å`. `keymap.rs` has fixtures for all three; add one rather than widening a
match if a fourth turns up.

## Wayland text injection has one route, and it has a hard limit

`RemoteDesktop` + `ConnectToEIS` is the only transport, and once `ConnectToEIS` has
been called the portal refuses the `Notify*` D-Bus methods outright — the two are
mutually exclusive, so the choice is final for the session. That leaves resolving
each character against the keymap the compositor hands over. The reasoning, the two
alternatives that do not work, and what consequently *cannot* be injected are in the
module docs of `crates/wx-platform/src/linux_wayland/keymap.rs`; read those before
proposing a fourth approach, because the obvious ones have been measured and ruled
out on the alpha target.

## Windows-only code can be checked from Linux

`cargo check --target x86_64-pc-windows-msvc` needs no MSVC toolchain (checking does not
link), but the root workspace still fails: `aws-lc-sys`/`ring` build scripts want `lib.exe`.
`-p wx-platform` alone does succeed, on both `x86_64-pc-windows-msvc` and
`aarch64-apple-darwin`, which covers every `cfg` block in the platform crate and is the
cheapest way to prove a backend stub still compiles. For a crate that does not build
that way, copy the module into a throwaway crate whose only dependency is `windows`, and
run clippy against that target — `--all-targets`
covers `cfg(test)` code too, so the Windows tests get compiled as well. This turns a
push-and-wait CI loop into a local one. It proves compilation and lints, not behaviour;
anything touching the real registry, clipboard, or desktop still needs a Windows run.

## Capability negotiation is enforced, not just advertised

- **Protocol enums and capability bits are append-only.** postcard encodes variants by
  index, so deleting one silently reinterprets every older peer's messages. Stop
  *advertising* a capability rather than removing it — see the note at the top of
  `crates/wx-proto/src/lib.rs` for the variants, and `Capabilities::FILE_TRANSFER` in
  `crates/wx-proto/src/caps.rs` for a bit that is deliberately defined and advertised
  by nothing.
- **Append-only does not mean older peers can skip what they do not know.** postcard is
  not self-describing: an unrecognised variant index is a hard decode error, and a
  control stream that fails to decode is torn down.
  `codec::tests::an_unknown_variant_is_a_hard_decode_failure` pins it. So appending is
  safe only *because* capability bits keep the new variant off the wire.
- **`PROTOCOL_VERSION` guards the wire format; capability bits gate features.** The two
  are not interchangeable, and the division decides what a change needs: a new feature
  over an existing encoding gets a capability bit and **no** bump; only a changed
  encoding of something already on the wire is a bump. A build speaks the whole range
  `MIN_COMPATIBLE_VERSION..=PROTOCOL_VERSION`, advertises the top of it, and both ends
  independently settle on `min` — so a *newer* peer negotiates down rather than being
  refused, while a below-floor one is still refused outright. `check_version` in
  `crates/wx-proto/src/caps.rs` carries the full argument, including why unilateral
  `min` cannot make the two ends disagree; read it before changing either constant.
- **Optional features are gated on what the peer advertised, before they send.**
  `Engine::peer_supports` / `send_optional` / `broadcast_optional` in
  `crates/wx-agent/src/engine.rs` are that seam, and a refusal is a `warn` naming the
  machine and the capability, never a silent drop. A peer's advertised set lives on
  `PeerState` in `crates/wx-agent/src/state.rs`. The Linux backends land one capability
  at a time, so a partially capable peer is the normal case during the alpha, not a bug.
- **This machine is asked the same question as a peer.** A backend that implements
  `lock_session` without also advertising `SCREENSAVER_SYNC` silently stops locking its
  own screen — implement and advertise together.
- **`ControlMsg::MonitorsChanged` carries monitors and not capabilities.** Both sides
  therefore derive `HAS_DISPLAYS` from the monitor list, through `with_displays` in
  `engine.rs`. Keep it as one rule in one place.
- **A peer's handshake `NodeInfo` can predate a portal grant, and routinely does.**
  The accept loop snapshots `local_info` at the top of each iteration — before it
  awaits the next connection — so what a peer learns at the handshake may be minutes
  old, and on Wayland the consent dialog is usually answered after the process
  starts and around when peers connect. `sync_capabilities` does not close that gap,
  because it only speaks on a transition and the transition already happened.
  `Engine::on_peer_ready` therefore sends `CapabilitiesChanged` to every peer that
  can decode it, **unconditionally** — there is no sound local operand to compare
  against, because the snapshot the peer was actually given is a clone nobody kept,
  and comparing against the peer's own advertised set instead silently matches on
  two identically configured machines and skips the correction exactly when it is
  needed.

The rule that follows for anything new: **do not gate a feature on a peer's
handshake capability set**, and do not gate one on this machine's either. Ask at
the moment the feature is used, when the answer is current. The clipboard's QUIC
stream is built that way — opened on demand rather than during session setup — for
this reason as much as for compatibility with a build that predates it.

## The UI workspace does not build until the agent sidecar exists

`ui/src-tauri/tauri.conf.json` declares `binaries/wx-agent` as an `externalBin`, and
`tauri-build` fails its build script when a declared one is missing — so `cargo check`,
`clippy` and `test` in that workspace all fail on a fresh checkout until
`cd ui && npm run bundle:agent` has been run once. That is deliberate: it makes an
installer with no daemon in it a build failure rather than a user's discovery. The same
step runs from `beforeBuildCommand`, so `npm run package:deb` needs nothing first.

Both of its products — `ui/src-tauri/binaries/wx-agent-<triple>` and
`packaging/winxtend.service` — are generated and `.gitignore`d. `tauri build` also
rewrites `ui/src-tauri/Cargo.toml`, normalising `tauri` and `tauri-build` to
`features = []`; it is a semantic no-op, so `git checkout` it rather than committing it.
Adding a runtime dependency or a packaged file means editing `bundle.linux.deb` in
`tauri.conf.json`; `libwebkit2gtk-4.1-0` and `libgtk-3-0` are added by Tauri's own
bundler and must not be listed there as well, or they appear twice in `Depends`.
`.github/workflows/package.yml` asserts the built `.deb`'s contents and dependency list,
which is the only check that the package is complete.

## The window runs under a capability ACL, and only half of it is gated

Tauri refuses every `plugin:*` command the webview sends unless a file in
`ui/src-tauri/capabilities/` grants it, while the commands this crate registers with
`invoke_handler` are allowed because they were registered. Nothing announces the
difference: with no capability file the window still opens, still draws, and still
starts an agent when the button is pressed — only `listen` is refused, so only the
paths that need agent events break, and they break silently. That is how an agent
running healthily under `systemctl --user` came to be reported as "not running": the
frontend awaited its event subscription before it looked for a daemon, and the
rejection took the discovery and its retry timer with it.

Two rules follow. A new use of any Tauri plugin API needs a matching permission, and
`tauri_build` will not tell you — it validates the capability files it finds and is
content to find none; `the_window_may_subscribe_to_the_events_this_process_emits` in
`ui/src-tauri/src/lib.rs` is the guard instead. And frontend startup must not let one
failed step cancel the rest: `listenToAgent` reports through `store.eventsProblem`
rather than throwing, and `ui/src/banner.js` owns what the window is entitled to
claim — "the agent is not running" is a statement about the daemon and may only be
made after the endpoint file has actually been looked for and found missing.

## Anything a window must show has to be in the snapshot, not only in an event

`ipc::Event` is a moment and there is no replay. A window is routinely not
listening at that moment: the agent under `systemctl --user` is up long before
anyone opens the UI, it emits `PairingRequested` microseconds after the session
carrying it comes up, and a webview reload throws away everything the previous
window heard. So any condition the user has to act on must also be a field on
`ipc::StatusSnapshot`, which every window asks for on connecting — `firewall` and
`pairings` are both there for this reason and say so in their doc comments.

The mirror of it on the frontend: state the store latches on an event needs an
event that ends it. `store.pairing` in `ui/src/agent.js` had none, so the first
pairing to end left a card standing that suppressed every later prompt. Every
removal of a pending pairing therefore goes through `end_pending_pairing` in
`engine.rs`, so that the announcement cannot be forgotten; its doc comment owns
that rule and names the only two removals exempt from it. The snapshot is the
other half of the same rule: once a card's node has appeared in `pairings`, a
later snapshot that omits it ends the card, whichever side raised it — see
`adoptPendingPairing`, which is what a window whose event subscription was
refused has instead of `pairingFinished`.

## Cursor ownership is reported, but never retracted

`CursorSnapshot::owner` is the last thing the engine settled on, and nothing
revises it when the owning peer drops: no event says "the machine holding the
cursor went away". So anything that *presents* ownership has to check the owner
against its `PeerSnapshot` — a peer that is missing or not `connected` means the
field is the last thing known and not where the cursor is. `ui/src/cursor.js`
is where that judgement lives and carries the argument; it derives everything
from the existing snapshot, which is why nothing was added to `StatusSnapshot`
for it.

`CursorSnapshot::locked` is **this machine's own router flag** and does not
cross the wire (`Router::is_locked`, `crates/wx-core/src/router.rs`; the
`cursor.rs` one is `VirtualCursor::is_locked`). What it pins is the virtual
cursor's *current* monitor, whosever that monitor is: `VirtualCursor::move_by`
consults `locked` before resolving a crossing and refuses it wherever the cursor
happens to be, and `Request::SetCursorLock` sets the flag regardless of who owns
the cursor. So locking while a peer holds the cursor keeps input flowing to that
peer instead of clawing control back — see
`locking_keeps_input_flowing_to_the_peer_that_already_owns_it` in
`crates/wx-core/src/router.rs`, which pins exactly that. Two things follow, and
they are easy to confuse. The lock a machine sets is in effect immediately, even
when the cursor is elsewhere, and copy must not describe it as dormant or
promise the cursor will come back. But the flag still says nothing about the
*peer's* own lock, so a machine that does not have the cursor must not report
the peer's cursor as locked — that would be a claim about somewhere else.

What the lock stops is the *crossing*, and only the crossing.
`VirtualCursor::warp_to` never consults `locked` and says in its own doc comment
why: the lock is there to stop accidental edge crossings, and an explicit
handoff is not accidental. So `Request::WarpCursor` moves the cursor while
locked — which the Layout tab offers as "Move the cursor here" — and copy must
not tell anyone that unlocking is the only way to move it. That is not a
loophole to be closed; it is the reason someone locked out of their own cursor
is not stranded.

`Request::LockAll` is a different thing entirely: it is screensaver sync, not
the cursor lock.

There is no machine in charge in this mesh. Do not describe one as a master or a
slave in code, copy, or comments; ownership moves to whichever machine the user
is driving from, and the words have to say who *has* the cursor.

## A transport error string is never user-facing copy

`wx_net::TransportError` renders things like "reading from a stream: connection
lost", which belong in the log and in a peer's state and nowhere a person reads;
copy shown to someone must be written for them at a site that knows what actually
happened. `Engine::on_peer_gone` in `crates/wx-agent/src/engine.rs` carries the
two as separate arguments for exactly this reason — see its doc comment.

## A plain `cargo build` of the UI produces a dev-mode binary

`generate_context!` picks `devUrl` over `frontendDist` whenever `tauri/custom-protocol`
is off, and that feature is turned on by the CLI's manifest rewrite at package time,
not by anything in `Cargo.toml`. So `cargo run` in `ui/src-tauri` opens a window
pointed at `http://localhost:1420`, which shows nothing whatever unless `npm run dev`
is up — no error, just a blank window that reads exactly like a broken frontend. To
exercise the binary the `.deb` ships, build it the way the package does:
`npm run tauri build`, or `cargo build --release --features tauri/custom-protocol` for
a quick one. `cargo test`, `cargo clippy` and `cargo fmt` are unaffected.

## Autostart is per-user on every platform, and the reasoning is load-bearing

`crates/wx-agent/src/autostart.rs` registers an `HKCU\…\Run` entry on Windows and a
systemd **user** unit on Linux. Neither is an implementation shortcut: a Windows
session-0 service and a Linux system unit would both start, report healthy, and capture
nothing, because neither has the interactive desktop — on Linux, the Wayland display,
the session bus, and `xdg-desktop-portal`. The module doc and
`packaging/winxtend.service.in` carry the full argument; read them before proposing a
system service again.

Three properties are easy to break and are tested: registration rewrites the recorded
path every time, a registration whose target is gone reads as *not* registered, and the
rendered `ExecStart` is one systemd will accept — quoted for a space, `%%` for a percent,
and refused outright for a path systemd cannot name at all (`quoted` and `representable`
in that module carry the reasoning). All three are the same failure and it is the one
this module is written against: a registration reported as successful that then starts
nothing, at the next login, on the user's machine. The Linux path is testable end to end
because `install_in`/`uninstall_in`/`is_registered_in` take a config root, so tests
redirect it instead of touching `~/.config`; `systemctl daemon-reload` sits outside that
split and is best-effort, because CI has no user systemd instance.

`packaging/winxtend.service.in` is the single unit text: `autostart.rs` `include_str!`s
it and `ui/scripts/bundle-agent.mjs` substitutes the same file for the packaged copy.
Its `After=xdg-desktop-portal.service` exists to narrow a real startup race — an
autostarted agent that reaches the portal before it is answerable gets `Unsupported` and
stays without input capability for the whole run. Do not drop the ordering to tidy the
unit. It cannot close the race on its own, because ordering says the portal's *unit* was
reached and not that the portal answers; the agent-side bounded retry
(`STARTUP_RETRY_DELAYS` in `crates/wx-platform/src/linux_wayland/driver.rs`) is what
does. Read the rules on `retry_after` before touching either: a retry that could raise a
consent dialog is a worse bug than the one both exist to fix.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
