//! What stands in for [`super::server`] where there is no X server to talk to.
//!
//! The backend modules compile on every target so the skeletons cannot rot (see the
//! note in [`crate::macos`]), and `x11rb` is a Linux-only dependency. Everything
//! above this — the display rules, the keymap reverse map, the injector's modifier
//! planning — is pure and is compiled and tested on Windows and macOS through this
//! stub, which is the point of the split.
//!
//! Nothing here can be reached at runtime: `current_platform` only chooses this
//! backend on Linux.

use std::sync::Arc;

use super::display::RawOutput;
use super::inject::{Request, Sink};
use super::keymap::Keymap;
use super::BACKEND;
use crate::error::{PlatformError, Result};

pub(super) struct Server;

fn unsupported<T>(operation: &'static str) -> Result<T> {
    Err(PlatformError::Unsupported {
        operation,
        backend: BACKEND,
    })
}

pub(super) fn connect() -> Result<Arc<Server>> {
    unsupported("connecting to an X server on a platform that has none")
}

impl Server {
    pub(super) fn outputs(&self) -> Result<Vec<RawOutput>> {
        unsupported("display enumeration")
    }

    /// Unreachable in both senses: [`connect`] never hands one of these out, so
    /// nothing can ask, and a stub has no server behind it to answer.
    pub(super) fn is_reachable(&self) -> bool {
        false
    }
}

impl Sink for Server {
    fn send(&self, _request: Request) -> Result<()> {
        unsupported("input injection")
    }

    fn keymap(&self) -> Result<Arc<Keymap>> {
        unsupported("reading the keyboard layout")
    }

    fn caps_locked(&self) -> Result<bool> {
        unsupported("reading the modifier state")
    }
}
