//! Screen capture: turning a monitor into a stream of BGRA frames.
//!
//! Frames are the one place in WinXtend where bulk memory is involved, so the
//! shape of [`RawFrame`] is deliberately defensive. Two properties matter:
//!
//! * **Stride is not width.** Every OS capture API hands back rows padded to
//!   some alignment (GDI to 4 bytes, DXGI to whatever the GPU chose, which is
//!   routinely 2560 bytes for a 1920-wide surface). Code that assumes
//!   `row_start == y * width * 4` produces a picture that shears progressively
//!   further right as it goes down the screen — the classic diagonal-smear bug.
//!   Stride is therefore carried explicitly and never inferred.
//! * **Geometry is validated at construction.** A frame can be built from bytes
//!   that arrived over the network, so `width * height * 4` is checked against
//!   the buffer length before anything indexes into it, and against a pixel
//!   budget before anything allocates.

use core::fmt;
use std::time::Duration;

use wx_proto::{MonitorId, Rect};

/// Bytes per pixel in every frame this crate handles.
///
/// BGRA8 throughout, because that is what both GDI (`biBitCount: 32`,
/// little-endian BGRX) and DXGI Desktop Duplication
/// (`DXGI_FORMAT_B8G8R8A8_UNORM`) natively produce, and converting to RGBA at
/// capture time would cost a full pass over every frame for nothing.
pub const BYTES_PER_PIXEL: usize = 4;

/// Largest frame area accepted, in pixels.
///
/// Bounds allocation when the geometry came from somewhere untrusted: a packet
/// header claiming 60000x60000 must be refused, not honoured with a 14GiB
/// `Vec`. Sized to admit an 8K panel with room to spare.
pub const MAX_FRAME_PIXELS: u64 = 8192 * 8192;

/// Frame rate ceiling.
///
/// Capture above this is pointless (no display shows it) and mostly serves to
/// pin a core, so a peer asking for 10000 fps gets clamped rather than obeyed.
pub const MAX_FPS: u16 = 240;

/// Why a frame's geometry was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FrameError {
    #[error("frame has zero extent ({width}x{height})")]
    Empty { width: u32, height: u32 },
    #[error("stride {stride} is shorter than a {width}px row ({needed} bytes)")]
    StrideTooSmall {
        stride: usize,
        width: u32,
        needed: usize,
    },
    #[error(
        "buffer of {got} bytes is short of the {needed} bytes {width}x{height}@{stride} needs"
    )]
    BufferTooSmall {
        got: usize,
        needed: usize,
        width: u32,
        height: u32,
        stride: usize,
    },
    #[error("{width}x{height} exceeds the {max} pixel limit")]
    TooLarge { width: u32, height: u32, max: u64 },
}

/// One captured frame: BGRA8, top-down, rows `stride` bytes apart.
///
/// Fields are private because the length-versus-stride-versus-height relation is
/// an invariant established once in [`RawFrame::new`]. If callers could mutate
/// `stride` afterwards, every `row()` would need re-checking.
#[derive(Clone, PartialEq, Eq)]
pub struct RawFrame {
    width: u32,
    height: u32,
    stride: usize,
    pixels: Vec<u8>,
    timestamp: Duration,
}

// The pixel buffer is megabytes; dumping it into a log line is never what anyone
// wanted from `{:?}`.
impl fmt::Debug for RawFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RawFrame")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("stride", &self.stride)
            .field("bytes", &self.pixels.len())
            .field("timestamp", &self.timestamp)
            .finish()
    }
}

impl RawFrame {
    /// Build a frame, validating the geometry against the buffer.
    ///
    /// The final row is allowed to be short of a full `stride`: a decoder hands
    /// over exactly `height` packed rows, and demanding trailing padding it
    /// never had would reject a perfectly good frame.
    pub fn new(
        width: u32,
        height: u32,
        stride: usize,
        pixels: Vec<u8>,
        timestamp: Duration,
    ) -> Result<Self, FrameError> {
        // Zero extent is not a hostile value, it is what a monitor reports while
        // it is being reconfigured or has just been unplugged. Reject it here so
        // it cannot become a division by zero further down.
        if width == 0 || height == 0 {
            return Err(FrameError::Empty { width, height });
        }
        if u64::from(width) * u64::from(height) > MAX_FRAME_PIXELS {
            return Err(FrameError::TooLarge {
                width,
                height,
                max: MAX_FRAME_PIXELS,
            });
        }

        let row_bytes = width as usize * BYTES_PER_PIXEL;
        if stride < row_bytes {
            return Err(FrameError::StrideTooSmall {
                stride,
                width,
                needed: row_bytes,
            });
        }

        // Checked throughout: on a 32-bit target `stride * height` for a large
        // frame overflows, and an overflowed length would pass the comparison
        // below while every row read ran off the end of the buffer.
        let needed = stride
            .checked_mul(height as usize - 1)
            .and_then(|n| n.checked_add(row_bytes))
            .ok_or(FrameError::TooLarge {
                width,
                height,
                max: MAX_FRAME_PIXELS,
            })?;
        if pixels.len() < needed {
            return Err(FrameError::BufferTooSmall {
                got: pixels.len(),
                needed,
                width,
                height,
                stride,
            });
        }

        Ok(Self {
            width,
            height,
            stride,
            pixels,
            timestamp,
        })
    }

    /// Build a frame whose rows have no padding.
    pub fn packed(
        width: u32,
        height: u32,
        pixels: Vec<u8>,
        timestamp: Duration,
    ) -> Result<Self, FrameError> {
        let stride = width as usize * BYTES_PER_PIXEL;
        Self::new(width, height, stride, pixels, timestamp)
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// Bytes between the start of one row and the start of the next.
    pub fn stride(&self) -> usize {
        self.stride
    }

    /// Monotonic offset from the start of the capture session.
    ///
    /// Not a wall clock: `SystemTime` steps when NTP corrects it or the user
    /// changes the timezone, and a video timeline that steps backwards makes
    /// every downstream frame-pacing decision nonsense.
    pub fn timestamp(&self) -> Duration {
        self.timestamp
    }

    /// The whole buffer, padding included.
    pub fn as_bytes(&self) -> &[u8] {
        &self.pixels
    }

    /// One row of exactly `width * 4` bytes, with any padding excluded.
    pub fn row(&self, y: u32) -> Option<&[u8]> {
        if y >= self.height {
            return None;
        }
        let start = self.stride * y as usize;
        let row_bytes = self.width as usize * BYTES_PER_PIXEL;
        Some(&self.pixels[start..start + row_bytes])
    }

    /// Rows top to bottom, padding excluded.
    pub fn rows(&self) -> impl Iterator<Item = &[u8]> + '_ {
        (0..self.height).map(move |y| self.row(y).expect("y < height"))
    }

    /// Size this frame occupies with no row padding — its size on the wire.
    pub fn packed_len(&self) -> usize {
        self.width as usize * BYTES_PER_PIXEL * self.height as usize
    }

    /// Whether the buffer is already padding-free and exactly sized, in which
    /// case [`RawFrame::to_packed_bytes`] is a plain copy.
    pub fn is_packed(&self) -> bool {
        self.stride == self.width as usize * BYTES_PER_PIXEL
            && self.pixels.len() == self.packed_len()
    }

    /// Copy out the pixels with row padding removed.
    ///
    /// Padding must never reach the wire: it is uninitialised memory from
    /// another process's point of view, it inflates every frame, and the
    /// receiver's stride is its own business anyway.
    pub fn to_packed_bytes(&self) -> Vec<u8> {
        if self.is_packed() {
            return self.pixels.clone();
        }
        let mut out = Vec::with_capacity(self.packed_len());
        for row in self.rows() {
            out.extend_from_slice(row);
        }
        out
    }

    /// Consume the frame, yielding a padding-free buffer without copying when
    /// the frame already had no padding.
    pub fn into_packed_bytes(mut self) -> Vec<u8> {
        if self.is_packed() {
            return core::mem::take(&mut self.pixels);
        }
        self.to_packed_bytes()
    }
}

/// Which monitor to capture, and where it lives in the local desktop space.
///
/// Bounds are supplied by the caller rather than looked up here on purpose.
/// Monitor enumeration belongs to `wx-platform`; a second enumeration in this
/// crate could disagree with it about which display [`MonitorId`] refers to, and
/// the failure mode is silently streaming the wrong screen — which looks like a
/// working feature until someone notices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureTarget {
    pub monitor: MonitorId,
    /// The monitor's rectangle in the node's own desktop coordinate space, as
    /// the OS reports it (`wx_proto::Monitor::local_bounds`). Negative origins
    /// are normal for displays left of or above the primary.
    pub bounds: Rect,
}

/// Capture parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureConfig {
    pub target: CaptureTarget,
    /// Frames per second to aim for. Advisory: the pacing is enforced by
    /// [`crate::pipeline`], and a backend that cannot keep up simply delivers
    /// fewer frames rather than falling behind in a queue.
    pub max_fps: u16,
}

impl CaptureConfig {
    pub fn new(monitor: MonitorId, bounds: Rect, max_fps: u16) -> Self {
        Self {
            target: CaptureTarget { monitor, bounds },
            max_fps: max_fps.clamp(1, MAX_FPS),
        }
    }
}

/// Why capture could not start or continue.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CaptureError {
    /// No backend is compiled in for this platform.
    #[error("screen capture is not implemented on this platform: {detail}")]
    Unsupported { detail: &'static str },
    /// The OS refused: macOS TCC screen-recording consent, or a Wayland portal
    /// dialog the user dismissed. Recoverable only by user action, so callers
    /// should surface it rather than retry in a loop.
    #[error("screen capture was denied by the OS: {detail}")]
    PermissionDenied { detail: String },
    /// The monitor was unplugged or reconfigured mid-stream. The caller should
    /// re-resolve the layout and start a new capture instead of retrying this
    /// one.
    #[error("capture target is gone or its geometry changed")]
    TargetLost,
    #[error("capture target has no area: {0:?}")]
    EmptyTarget(Rect),
    #[error("frame geometry: {0}")]
    Geometry(#[from] FrameError),
    /// An OS call failed. Carries the platform message because these are
    /// undiagnosable without it.
    #[error("{api} failed: {detail}")]
    Os { api: &'static str, detail: String },
}

/// A source of frames for one monitor.
///
/// Pull-based rather than callback-based: the caller owns the timing, which is
/// what lets [`crate::pipeline`] drop frames under load. A backend that pushed
/// on its own schedule would need its own queue, and a queue is exactly the
/// thing we are trying not to have.
pub trait ScreenCapture: Send {
    fn target(&self) -> CaptureTarget;

    /// Grab the current screen contents.
    ///
    /// `Ok(None)` means "nothing new right now" — a duplication-style backend
    /// reports no change when the screen is static, and a static screen is the
    /// common case for a headless box sitting at a desktop. It is emphatically
    /// not end-of-stream; callers must keep asking.
    fn next_frame(&mut self) -> Result<Option<RawFrame>, CaptureError>;
}

/// Open the platform's capture backend.
pub fn open(config: CaptureConfig) -> Result<Box<dyn ScreenCapture>, CaptureError> {
    if config.target.bounds.is_empty() {
        return Err(CaptureError::EmptyTarget(config.target.bounds));
    }
    platform_open(config)
}

#[cfg(target_os = "windows")]
fn platform_open(config: CaptureConfig) -> Result<Box<dyn ScreenCapture>, CaptureError> {
    crate::capture_windows::open(config)
}

#[cfg(target_os = "macos")]
fn platform_open(config: CaptureConfig) -> Result<Box<dyn ScreenCapture>, CaptureError> {
    crate::capture_macos::open(config)
}

#[cfg(target_os = "linux")]
fn platform_open(config: CaptureConfig) -> Result<Box<dyn ScreenCapture>, CaptureError> {
    crate::capture_linux::open(config)
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
fn platform_open(_config: CaptureConfig) -> Result<Box<dyn ScreenCapture>, CaptureError> {
    Err(CaptureError::Unsupported {
        detail: "no capture backend for this target",
    })
}

/// A capture source that replays frames handed to it.
///
/// Lives outside `#[cfg(test)]` so that the agent can exercise the encode and
/// transport path — and the viewer UI — on a machine where real capture is
/// unavailable, which is most of the CI matrix.
#[derive(Debug, Default)]
pub struct StaticCapture {
    target: CaptureTarget,
    frames: Vec<RawFrame>,
    next: usize,
    /// When true, frames repeat forever; otherwise the source reports "nothing
    /// new" once exhausted, which is what a real backend does on a static
    /// screen.
    looping: bool,
}

impl StaticCapture {
    pub fn new(target: CaptureTarget, frames: Vec<RawFrame>) -> Self {
        Self {
            target,
            frames,
            next: 0,
            looping: false,
        }
    }

    pub fn looping(mut self) -> Self {
        self.looping = true;
        self
    }
}

impl Default for CaptureTarget {
    fn default() -> Self {
        Self {
            monitor: MonitorId(0),
            bounds: Rect::new(0, 0, 1, 1),
        }
    }
}

impl ScreenCapture for StaticCapture {
    fn target(&self) -> CaptureTarget {
        self.target
    }

    fn next_frame(&mut self) -> Result<Option<RawFrame>, CaptureError> {
        if self.frames.is_empty() {
            return Ok(None);
        }
        if self.next >= self.frames.len() {
            if !self.looping {
                return Ok(None);
            }
            self.next = 0;
        }
        let frame = self.frames[self.next].clone();
        self.next += 1;
        Ok(Some(frame))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `width * height` BGRA pixels where each byte encodes its own offset, so
    /// a stride mistake shows up as wrong values rather than a wrong length.
    fn padded_buffer(width: u32, height: u32, stride: usize) -> Vec<u8> {
        let mut buf = vec![0u8; stride * height as usize];
        for y in 0..height as usize {
            for x in 0..width as usize * BYTES_PER_PIXEL {
                buf[y * stride + x] = (y * 7 + x) as u8;
            }
            // Poison the padding: any code that reads it will produce 0xAA.
            for p in (width as usize * BYTES_PER_PIXEL)..stride {
                buf[y * stride + p] = 0xAA;
            }
        }
        buf
    }

    #[test]
    fn rows_exclude_stride_padding() {
        let (w, h, stride) = (3u32, 4u32, 32usize);
        let frame =
            RawFrame::new(w, h, stride, padded_buffer(w, h, stride), Duration::ZERO).unwrap();
        for (y, row) in frame.rows().enumerate() {
            assert_eq!(row.len(), w as usize * BYTES_PER_PIXEL);
            assert!(!row.contains(&0xAA), "padding leaked into row {y}");
            assert_eq!(row[0], (y * 7) as u8, "row {y} started at the wrong offset");
        }
    }

    #[test]
    fn packing_a_padded_frame_drops_exactly_the_padding() {
        let (w, h, stride) = (5u32, 3u32, 64usize);
        let frame =
            RawFrame::new(w, h, stride, padded_buffer(w, h, stride), Duration::ZERO).unwrap();
        let packed = frame.to_packed_bytes();
        assert_eq!(packed.len(), frame.packed_len());
        assert_eq!(packed.len(), 5 * 4 * 3);
        assert!(!packed.contains(&0xAA));
    }

    #[test]
    fn packed_frame_survives_a_pack_unpack_round_trip() {
        let (w, h) = (7u32, 6u32);
        let stride = w as usize * BYTES_PER_PIXEL;
        let bytes = padded_buffer(w, h, stride);
        let frame = RawFrame::packed(w, h, bytes.clone(), Duration::from_millis(5)).unwrap();
        assert!(frame.is_packed());
        assert_eq!(frame.to_packed_bytes(), bytes);
        assert_eq!(frame.timestamp(), Duration::from_millis(5));
    }

    #[test]
    fn last_row_may_lack_trailing_padding() {
        // A decoder produces height packed rows and nothing more. Requiring the
        // pad bytes after the final row would reject a valid frame.
        let (w, h, stride) = (4u32, 3u32, 24usize);
        let len = stride * (h as usize - 1) + w as usize * BYTES_PER_PIXEL;
        let frame = RawFrame::new(w, h, stride, vec![1u8; len], Duration::ZERO).unwrap();
        assert_eq!(
            frame.row(h - 1).unwrap().len(),
            w as usize * BYTES_PER_PIXEL
        );
    }

    #[test]
    fn buffer_one_byte_short_is_rejected() {
        let (w, h, stride) = (4u32, 3u32, 24usize);
        let len = stride * (h as usize - 1) + w as usize * BYTES_PER_PIXEL;
        let err = RawFrame::new(w, h, stride, vec![0u8; len - 1], Duration::ZERO).unwrap_err();
        assert!(matches!(err, FrameError::BufferTooSmall { .. }), "{err:?}");
    }

    #[test]
    fn stride_narrower_than_a_row_is_rejected() {
        let err = RawFrame::new(10, 2, 39, vec![0u8; 1000], Duration::ZERO).unwrap_err();
        assert!(matches!(err, FrameError::StrideTooSmall { .. }), "{err:?}");
    }

    #[test]
    fn zero_extent_frames_are_rejected() {
        for (w, h) in [(0u32, 10u32), (10, 0), (0, 0)] {
            let err = RawFrame::new(w, h, 0, Vec::new(), Duration::ZERO).unwrap_err();
            assert!(matches!(err, FrameError::Empty { .. }), "{w}x{h}: {err:?}");
        }
    }

    #[test]
    fn absurd_geometry_is_refused_before_allocating() {
        // The numbers a hostile or corrupted packet header supplies. The check
        // must come from the declared geometry, never from the buffer we would
        // have had to allocate first.
        let err = RawFrame::new(60_000, 60_000, 240_000, Vec::new(), Duration::ZERO).unwrap_err();
        assert!(matches!(err, FrameError::TooLarge { .. }), "{err:?}");
    }

    #[test]
    fn single_pixel_frame_is_valid() {
        let frame = RawFrame::packed(1, 1, vec![1, 2, 3, 4], Duration::ZERO).unwrap();
        assert_eq!(frame.packed_len(), 4);
        assert_eq!(frame.row(0).unwrap(), &[1, 2, 3, 4]);
        assert!(frame.row(1).is_none());
    }

    #[test]
    fn row_past_the_bottom_is_none_rather_than_a_panic() {
        let frame = RawFrame::packed(2, 2, vec![0u8; 16], Duration::ZERO).unwrap();
        assert!(frame.row(2).is_none());
        assert!(frame.row(u32::MAX).is_none());
    }

    #[test]
    fn oversized_buffer_is_accepted_and_extra_bytes_ignored() {
        // Backends hand over a buffer sized for the largest frame seen so far;
        // that is not an error, and the surplus must not appear in the output.
        let frame = RawFrame::new(2, 2, 8, vec![9u8; 4096], Duration::ZERO).unwrap();
        assert!(!frame.is_packed());
        assert_eq!(frame.to_packed_bytes().len(), 16);
    }

    #[test]
    fn fps_is_clamped_to_something_a_display_could_show() {
        assert_eq!(
            CaptureConfig::new(MonitorId(0), Rect::new(0, 0, 8, 8), 0).max_fps,
            1
        );
        assert_eq!(
            CaptureConfig::new(MonitorId(0), Rect::new(0, 0, 8, 8), u16::MAX).max_fps,
            MAX_FPS
        );
    }

    #[test]
    fn opening_a_zero_area_monitor_fails_before_touching_the_os() {
        let config = CaptureConfig::new(MonitorId(3), Rect::new(100, 100, 0, 1080), 60);
        let err = open(config)
            .err()
            .expect("a zero-area target must not open");
        assert!(matches!(err, CaptureError::EmptyTarget(_)), "{err:?}");
    }

    #[test]
    fn exhausted_static_capture_reports_no_new_frame_not_an_error() {
        let frame = RawFrame::packed(1, 1, vec![0u8; 4], Duration::ZERO).unwrap();
        let mut cap = StaticCapture::new(CaptureTarget::default(), vec![frame]);
        assert!(cap.next_frame().unwrap().is_some());
        assert!(cap.next_frame().unwrap().is_none());
        assert!(cap.next_frame().unwrap().is_none());
    }

    #[test]
    fn looping_static_capture_never_runs_dry() {
        let frame = RawFrame::packed(1, 1, vec![0u8; 4], Duration::ZERO).unwrap();
        let mut cap = StaticCapture::new(CaptureTarget::default(), vec![frame]).looping();
        for _ in 0..5 {
            assert!(cap.next_frame().unwrap().is_some());
        }
    }
}
