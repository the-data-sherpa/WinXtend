//! Windows capture over GDI `BitBlt`.
//!
//! This is the deliberately boring backend: `CreateCompatibleDC` + `BitBlt` +
//! `GetDIBits` works on every Windows version WinXtend targets, needs no
//! Direct3D device, survives RDP sessions, and costs one small dependency. It is
//! good for perhaps 15-30 fps on a 4K panel, because every frame is a full CPU
//! copy of the whole desktop region whether or not anything changed.
//!
//! # The DXGI seam
//!
//! The correct long-term backend is DXGI Desktop Duplication
//! (`IDXGIOutputDuplication`), which is why [`ScreenCapture`] is a trait and
//! nothing outside this file knows GDI exists. What a DXGI backend has to add,
//! and why it is not here yet:
//!
//! * A D3D11 device, an `IDXGIOutput1::DuplicateOutput` call, and a staging
//!   texture to map for CPU access — or, better, an encoder that consumes the
//!   GPU texture directly and never touches system memory.
//! * `AcquireNextFrame` returns dirty rectangles and move rectangles. Honouring
//!   them is where the actual win is: a static desktop costs nothing, and a
//!   moving window costs the window, not the screen.
//! * `DXGI_ERROR_ACCESS_LOST` on every resolution change, UAC secure-desktop
//!   switch, GPU driver reset, and fullscreen-exclusive transition. The
//!   duplication object must be torn down and recreated, and a backend that
//!   treats this as fatal appears to work perfectly until the first UAC prompt.
//! * Output-to-monitor mapping: duplication is per-`IDXGIOutput`, which must be
//!   matched to the `HMONITOR` behind our [`MonitorId`], not assumed by index.
//!
//! # Known limits of this backend
//!
//! * The hardware cursor is not composited into a `BitBlt`. The viewer draws the
//!   remote cursor itself from the input plane, which is also lower latency than
//!   waiting for it to arrive inside a video frame.
//! * The agent process must be per-monitor DPI aware. Without that manifest
//!   setting, Windows lies to `GetDC(NULL)` about the desktop geometry and the
//!   captured region is a blurry, wrongly-offset rectangle on any scaled display.
//! * Secure desktops (the logon screen, UAC consent) cannot be captured from a
//!   normal user session at all; frames simply come back black.

use std::ffi::c_void;
use std::time::Instant;

use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits,
    ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CAPTUREBLT, DIB_RGB_COLORS,
    HBITMAP, HDC, HGDIOBJ, RGBQUAD, SRCCOPY,
};

use crate::capture::{
    CaptureConfig, CaptureError, CaptureTarget, RawFrame, ScreenCapture, BYTES_PER_PIXEL,
};

pub(crate) fn open(config: CaptureConfig) -> Result<Box<dyn ScreenCapture>, CaptureError> {
    Ok(Box::new(GdiCapture::new(config)?))
}

/// GDI resources for one monitor, created once and reused.
///
/// Recreating the DC and bitmap per frame is a common and expensive mistake:
/// `CreateCompatibleBitmap` for a 4K frame is not free, and doing it 60 times a
/// second leaks GDI handles the moment any error path forgets a cleanup.
struct GdiCapture {
    config: CaptureConfig,
    /// DC for the whole virtual desktop. Its origin is the primary monitor's
    /// top-left, so monitors placed left of or above it have negative source
    /// coordinates — which `BitBlt` handles, and which is why the target rect is
    /// carried as signed.
    screen_dc: HDC,
    mem_dc: HDC,
    bitmap: HBITMAP,
    /// Whatever `SelectObject` displaced, restored before the DC is deleted so
    /// the bitmap is deletable.
    previous_bitmap: HGDIOBJ,
    width: i32,
    height: i32,
    started: Instant,
}

// GDI handles are process-scoped and carry no thread affinity (unlike an HWND
// and its message queue), and the pipeline hands this struct to exactly one
// worker thread which then owns it outright. Concurrent use from two threads
// would not be safe, and `&mut self` on every method is what prevents it.
unsafe impl Send for GdiCapture {}

impl GdiCapture {
    fn new(config: CaptureConfig) -> Result<Self, CaptureError> {
        let bounds = config.target.bounds;
        let width = bounds.w as i32;
        let height = bounds.h as i32;
        if width <= 0 || height <= 0 {
            return Err(CaptureError::EmptyTarget(bounds));
        }

        unsafe {
            let screen_dc = GetDC(None);
            if screen_dc.is_invalid() {
                return Err(CaptureError::Os {
                    api: "GetDC",
                    detail: "no device context for the desktop".into(),
                });
            }

            let mem_dc = CreateCompatibleDC(Some(screen_dc));
            if mem_dc.is_invalid() {
                ReleaseDC(None, screen_dc);
                return Err(CaptureError::Os {
                    api: "CreateCompatibleDC",
                    detail: "out of GDI resources".into(),
                });
            }

            let bitmap = CreateCompatibleBitmap(screen_dc, width, height);
            if bitmap.is_invalid() {
                let _ = DeleteDC(mem_dc);
                ReleaseDC(None, screen_dc);
                return Err(CaptureError::Os {
                    api: "CreateCompatibleBitmap",
                    detail: format!("could not allocate a {width}x{height} bitmap"),
                });
            }

            Ok(Self {
                config,
                screen_dc,
                mem_dc,
                bitmap,
                previous_bitmap: HGDIOBJ::default(),
                width,
                height,
                started: Instant::now(),
            })
        }
    }
}

impl Drop for GdiCapture {
    fn drop(&mut self) {
        unsafe {
            // Order matters: a bitmap still selected into a DC cannot be
            // deleted, and skipping the restore leaks it for the process
            // lifetime.
            if !self.previous_bitmap.is_invalid() {
                SelectObject(self.mem_dc, self.previous_bitmap);
            }
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.mem_dc);
            ReleaseDC(None, self.screen_dc);
        }
    }
}

impl ScreenCapture for GdiCapture {
    fn target(&self) -> CaptureTarget {
        self.config.target
    }

    fn next_frame(&mut self) -> Result<Option<RawFrame>, CaptureError> {
        let bounds = self.config.target.bounds;
        let stride = self.width as usize * BYTES_PER_PIXEL;
        // GDI writes into a buffer we then hand off, so this allocation is not
        // avoidable without an extra copy out of a reused scratch buffer. The
        // memcpy inside GetDIBits dominates either way.
        let mut pixels = vec![0u8; stride * self.height as usize];

        unsafe {
            let previous = SelectObject(self.mem_dc, self.bitmap.into());
            if previous.is_invalid() {
                return Err(CaptureError::Os {
                    api: "SelectObject",
                    detail: "could not select the capture bitmap".into(),
                });
            }
            self.previous_bitmap = previous;

            // CAPTUREBLT includes layered (transparent) windows, without which
            // tooltips, some menus, and many overlays are missing from the
            // stream. It costs an extra redraw of the affected windows; DXGI has
            // neither the cost nor the omission.
            let blit = BitBlt(
                self.mem_dc,
                0,
                0,
                self.width,
                self.height,
                Some(self.screen_dc),
                bounds.x,
                bounds.y,
                SRCCOPY | CAPTUREBLT,
            );

            // GetDIBits refuses to read a bitmap that is currently selected into
            // a DC, so the selection has to be undone first. Getting this wrong
            // yields a silent zero return and an all-black stream.
            SelectObject(self.mem_dc, self.previous_bitmap);
            self.previous_bitmap = HGDIOBJ::default();

            blit.map_err(|e| CaptureError::Os {
                api: "BitBlt",
                detail: e.message(),
            })?;

            let mut info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: core::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: self.width,
                    // Negative height asks for top-down rows. DIBs are
                    // bottom-up by default, and a positive value here delivers a
                    // vertically mirrored picture that looks like a decoder bug.
                    biHeight: -self.height,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    biSizeImage: 0,
                    biXPelsPerMeter: 0,
                    biYPelsPerMeter: 0,
                    biClrUsed: 0,
                    biClrImportant: 0,
                },
                bmiColors: [RGBQUAD::default(); 1],
            };

            let scanlines = GetDIBits(
                self.mem_dc,
                self.bitmap,
                0,
                self.height as u32,
                Some(pixels.as_mut_ptr() as *mut c_void),
                &mut info,
                DIB_RGB_COLORS,
            );
            if scanlines == 0 {
                return Err(CaptureError::Os {
                    api: "GetDIBits",
                    detail: "no scanlines copied".into(),
                });
            }
            if scanlines != self.height {
                // A short copy means the geometry we were told no longer matches
                // reality, which in practice means the display was reconfigured.
                return Err(CaptureError::TargetLost);
            }
        }

        let frame = RawFrame::new(
            self.width as u32,
            self.height as u32,
            stride,
            pixels,
            self.started.elapsed(),
        )?;
        Ok(Some(frame))
    }
}
