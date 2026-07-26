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

## A Linux test count below the README's is expected

The README quotes a Windows-run total, so a `cargo test --workspace` on Linux
legitimately reports fewer. `wx-platform` is where the gap lives: a large block of
its tests is `cfg(windows)`-gated and never runs here, a handful are
`cfg(target_os = "linux")` and never run there, and one is
`#[cfg_attr(not(windows), ignore)]` so it counts as ignored off Windows. Whether a
given figure still holds is a question for the compiler, not this file:

```sh
cargo test --workspace                 # the total for this platform
cargo test -p wx-platform --lib        # the crate the gap comes from
grep -rn 'cfg(windows)\|cfg(target_os = "linux")\|cfg_attr(not(windows)' crates/wx-platform/src
```

A lower Linux number is not evidence the README is stale. Confirm with the above
before "fixing" it — and don't copy a fresh number back into this file.

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

- **Headless (CI)** — anything that opens a Wayland connection must skip, not
  fail. Reproduce with
  `env -u WAYLAND_DISPLAY -u DISPLAY -u XDG_SESSION_TYPE XDG_RUNTIME_DIR=$(mktemp -d) cargo test`.

- **End-to-end without the UI** — `wx-agent --config-dir <scratch> --status`
  prints the same `StatusSnapshot` the Tauri layout editor renders. Always pass
  `--config-dir`; the default writes an identity key into the user's real config
  directory. The UI honours `WINXTEND_CONFIG_DIR` and `WINXTEND_AGENT` for the
  same reason.

GNOME denies programmatic screenshots (`org.gnome.Shell.Screenshot`) to untrusted
callers, so visual confirmation of the UI needs a human or a portal prompt.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
