//! Stand-in for [`super::driver`] on targets that are not Linux.
//!
//! This module exists so that `linux_wayland` keeps compiling on Windows and macOS,
//! the way every other backend skeleton in this crate does: a change to
//! [`crate::traits`] should break here and now rather than months later on a machine
//! nobody has. Only the parts that talk to D-Bus and libei are behind `cfg`, not the
//! module — see the note in [`crate`].

use std::path::PathBuf;
use std::sync::Arc;

use super::inject_stub::Transport;
use super::session::SharedSession;

/// A driver that never runs. Dropping it does nothing, because there is nothing.
pub struct Driver;

/// Takes the transport it will never fill, so the signature stays the one
/// [`super::PortalSession::starting`] calls on every target.
pub fn start(
    shared: Arc<SharedSession>,
    _transport: Arc<Transport>,
    _config_dir: PathBuf,
) -> Driver {
    shared.unsupported("the xdg-desktop-portal RemoteDesktop session needs Linux");
    Driver
}
