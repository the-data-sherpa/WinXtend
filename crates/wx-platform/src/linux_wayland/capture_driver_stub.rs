//! Stand-in for the `InputCapture` portal driver on targets that have no desktop
//! portal.
//!
//! The Wayland module is compiled on Windows and macOS on purpose — see the note
//! in [`crate::macos`] — so [`super::capture`] and [`super::session`] stay
//! type-checked and tested everywhere, and only the part that opens a D-Bus
//! connection is swapped out.
//!
//! It reports [`super::SessionState::Unsupported`] rather than pretending to try:
//! a session that sat in `Starting` forever would have the backend wait for a
//! grant no portal is ever going to give it.

use std::sync::Arc;

use super::capture::CaptureState;
use super::session::SharedSession;

pub struct Driver;

pub fn start(shared: Arc<SharedSession>, _capture: Arc<CaptureState>) -> Driver {
    shared.starting();
    shared.unsupported("there is no xdg-desktop-portal on this platform");
    Driver
}
