//! Stand-in for [`super::inject`] on the platforms that have no libei.
//!
//! Exists for the same reason [`super::driver_stub`] does: the Wayland module is
//! declared unconditionally so a change to [`crate::traits`] breaks on every
//! machine rather than on the one nobody has. See the note in [`crate::macos`].

use std::sync::Arc;

use wx_proto::{InputEvent, Monitor, NormPos};

use super::BACKEND;
use crate::error::{PlatformError, Result};

#[derive(Default)]
pub struct Transport;

impl Transport {
    pub fn new() -> Self {
        Self
    }
}

pub struct Injector {
    _transport: Arc<Transport>,
}

impl Injector {
    pub fn new(transport: Arc<Transport>) -> Self {
        Self {
            _transport: transport,
        }
    }

    pub fn inject(&mut self, _monitor: &Monitor, _event: &InputEvent) -> Result<()> {
        Err(unsupported())
    }

    pub fn warp_cursor(&mut self, _monitor: &Monitor, _pos: NormPos) -> Result<()> {
        Err(unsupported())
    }

    /// Nothing was ever pressed here, so there is nothing to strand.
    pub fn release_all(&mut self) -> Result<()> {
        Ok(())
    }
}

fn unsupported() -> PlatformError {
    PlatformError::Unsupported {
        operation: "input injection",
        backend: BACKEND,
    }
}
