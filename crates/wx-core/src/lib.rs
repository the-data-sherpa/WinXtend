//! WinXtend engine.
//!
//! Platform-agnostic and I/O-free: the whole cursor-routing model is pure
//! functions over data, so it is testable without a display server, a network,
//! or a second machine.
//!
//! Three layers, each usable on its own:
//!
//! * [`layout`] places every monitor in the mesh in one global coordinate space
//!   and answers "what is that way?" geometrically.
//! * [`cursor`] integrates raw pointer deltas into an authoritative position and
//!   decides when a seam has been crossed.
//! * [`router`] turns each captured event into an ordered list of things to do —
//!   inject locally, forward to a peer, or transfer control.

pub mod cursor;
pub mod layout;
pub mod router;

pub use cursor::{CursorMotion, VirtualCursor, WarpError};
pub use layout::{Crossing, GlobalLayout, LayoutProblem};
pub use router::{InputRouter, RouteAction};
