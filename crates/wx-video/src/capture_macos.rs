//! macOS capture: not implemented.
//!
//! Present as a stub rather than absent so that the crate compiles for macOS and
//! the agent can report `VideoUnavailable` honestly instead of failing to build.
//! Screen streaming is optional; input sharing on macOS does not depend on this.
//!
//! # What a real backend must do
//!
//! * Use **ScreenCaptureKit** (`SCShareableContent` -> `SCDisplay` ->
//!   `SCStream` with an `SCStreamConfiguration`), not `CGDisplayStream` and not
//!   `CGWindowListCreateImage`. The older APIs are deprecated, and
//!   `CGDisplayStream` is gone from the supported surface going forward.
//! * Request `kCVPixelFormatType_32BGRA` so frames arrive in the same layout as
//!   every other backend. The `CVPixelBuffer` must be locked
//!   (`CVPixelBufferLockBaseAddress`) before reading, and its
//!   `CVPixelBufferGetBytesPerRow` is the stride — it is not width * 4, and
//!   assuming otherwise produces the diagonal shear described in
//!   [`crate::capture`].
//! * Handle the TCC screen-recording permission. The first capture attempt
//!   triggers a system prompt, and until the user approves it the stream
//!   silently yields black or empty frames rather than an error. The backend
//!   must probe (`CGPreflightScreenCaptureAccess`) and surface
//!   [`CaptureError::PermissionDenied`], because a black screen looks like a
//!   WinXtend bug and a permission prompt does not.
//! * Map `SCDisplay.displayID` to our [`wx_proto::MonitorId`] via the same
//!   `CGDirectDisplayID` ordering `wx-platform` uses. Two independent
//!   enumerations that disagree stream the wrong screen.
//! * Retain the frame's timing from `CMSampleBufferGetPresentationTimeStamp`
//!   rather than reading a clock on delivery; the sample is already timestamped
//!   at composition time.

use crate::capture::{CaptureConfig, CaptureError, ScreenCapture};

pub(crate) fn open(_config: CaptureConfig) -> Result<Box<dyn ScreenCapture>, CaptureError> {
    Err(CaptureError::Unsupported {
        detail: "macOS ScreenCaptureKit capture is not implemented yet",
    })
}
