//! WinXtend agent: the daemon that does the actual work.
//!
//! One of these runs on every machine in the mesh. It owns the platform input
//! hooks, the QUIC listener, the trust store, and the authoritative cursor; the UI
//! is a thin client that talks to it over a local socket and can be closed without
//! interrupting anything.
//!
//! Separating them is deliberate. Input capture has to survive the UI being quit,
//! and on Windows the hooks have to live in a process that runs for the whole
//! session — neither is possible if they live inside a GUI process the user closes
//! when they are done arranging screens.
//!
//! # Layering
//!
//! ```text
//!   wx-platform ──capture──> engine ──route──> wx-core ──actions──> engine ──> wx-net
//!                     ^                                                          |
//!                     └──────────────────── inject ──────────────────────────────┘
//! ```
//!
//! * [`config`] is the on-disk state, with defaults good enough that a fresh
//!   install needs no file at all.
//! * [`state`] is the live view of the mesh: who exists, who is reachable, where
//!   the cursor is.
//! * [`autolayout`] invents a layout so a freshly paired mesh works before anyone
//!   opens the editor.
//! * [`engine`] is the run loop.
//! * [`ipc`] is the contract the UI codes against.
//! * [`autostart`] registers the agent to start with the session.
//! * [`firewall`] names the one local cause of "nobody can see this machine"
//!   that the agent can detect and the user can fix.
//!
//! Exposed as a library as well as a binary so that the UI process can share the
//! IPC types rather than restating them, and so the pure logic — layout guessing,
//! hotkey matching, disconnect recovery — is testable without starting a daemon.

pub mod autolayout;
pub mod autostart;
pub mod config;
pub mod engine;
pub mod firewall;
pub mod ipc;
pub mod state;

pub use config::{Config, Hotkey, HotkeyAction, Hotkeys};
pub use engine::{run, AGENT_VERSION};
pub use ipc::{Event, IpcClient, Request, Response};
pub use state::{AgentState, ConnStatus, PeerState};
