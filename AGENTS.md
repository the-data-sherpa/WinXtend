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

## The README's test count is right

The README says 614 tests; a Linux run reports 523 passed, 1 ignored. That reconciles:
`wx-platform` contributes 70 of its 160 on Linux (90 are Windows-gated), and
523 + 90 + 1 = 614. It is a Windows-run number, not a stale one. Don't "fix" it.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
