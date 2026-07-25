//! Linux capture: not implemented.
//!
//! A stub so the crate builds on Linux and the agent can answer a
//! `ControlMsg::VideoStart` with `VideoUnavailable` instead of not existing.
//!
//! # What a real backend must do
//!
//! * Go through the **XDG desktop portal**
//!   (`org.freedesktop.portal.ScreenCast`: `CreateSession`, `SelectSources`,
//!   `Start`, `OpenPipeWireRemote`), then consume the PipeWire stream on the
//!   returned fd. Under Wayland there is no other sanctioned path: no compositor
//!   will hand a client the screen without the portal's consent dialog.
//! * Treat the portal token as **persistent state**. Without a stored
//!   `restore_token`, every agent restart pops a "share your screen?" dialog,
//!   which on a headless box nobody is looking at means video never starts. That
//!   token is the difference between a usable feature and a demo.
//! * Accept PipeWire's negotiated format rather than demanding BGRA:
//!   `SPA_VIDEO_FORMAT_BGRx` is usual, but a compositor may offer only
//!   `RGBx`, and DMA-BUF buffers may arrive instead of memory buffers. The
//!   stride comes from `spa_data.chunk.stride` and is frequently not width * 4.
//! * Keep an **X11 fallback** for X sessions, where `XShmGetImage` against the
//!   root window is far simpler and much faster than the portal path. Detect the
//!   session type (`wx_proto::DisplayServer`) rather than probing and hoping.
//! * Recover from stream renegotiation: PipeWire changes format or size on
//!   monitor reconfiguration, and a backend that caches the first negotiated
//!   geometry will emit correctly sized buffers full of misaligned rows.

use crate::capture::{CaptureConfig, CaptureError, ScreenCapture};

pub(crate) fn open(_config: CaptureConfig) -> Result<Box<dyn ScreenCapture>, CaptureError> {
    Err(CaptureError::Unsupported {
        detail: "Linux PipeWire portal capture is not implemented yet",
    })
}
