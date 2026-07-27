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
  and say so before you run it.

GNOME denies programmatic screenshots (`org.gnome.Shell.Screenshot`) to untrusted
callers, so visual confirmation of the UI needs a human or a portal prompt.

The backend is split to keep that reachable from CI: the parts needing no desktop —
restore-token persistence in `token.rs`, the session state machine in `session.rs`, the
event translation in `capture.rs` — are deliberately separated from everything touching
D-Bus or libei (`driver.rs` and `capture_driver.rs`, behind `cfg(target_os = "linux")`
with a stub alongside each), so this crate compiles and tests on all three platforms
with no session. Keep that division.

## Wayland input needs two portals, and they are not interchangeable

`org.freedesktop.portal.RemoteDesktop` **drives** a desktop and cannot capture from
one: its session has no zones, barriers, `Enable`/`Release` or activation, and its
libei devices are client-owned emulation devices. Capture is
`org.freedesktop.portal.InputCapture`, which GNOME 50 is the first release to ship.
So the backend holds two sessions and the user answers two consent dialogs per
launch; only `RemoteDesktop` has a restore token at the versions the alpha target
has. `SharedSession` in `crates/wx-platform/src/linux_wayland/session.rs` is
parameterised by which capability bit it owns precisely so one being revoked cannot
unadvertise the other.

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

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
