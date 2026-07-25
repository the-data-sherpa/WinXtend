//! Display enumeration on Windows.

use std::sync::Once;

use windows::core::{BOOL, PCWSTR};
use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayDevicesW, EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO,
    MONITORINFOEXW,
};
use windows::Win32::UI::HiDpi::{
    GetDpiForMonitor, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    MDT_EFFECTIVE_DPI,
};
use windows::Win32::UI::WindowsAndMessaging::MONITORINFOF_PRIMARY;

use wx_proto::{Monitor, MonitorId, Rect};

use crate::coords::{deduplicate_ids, stable_monitor_id};
use crate::error::{PlatformError, Result};

/// Standard DPI on Windows. A scale factor is DPI divided by this.
const DEFAULT_DPI: f32 = 96.0;

static DPI_INIT: Once = Once::new();

/// Opt this process into per-monitor DPI awareness, once.
///
/// Without it Windows lies to us: an unaware process sees every monitor's
/// rectangle scaled into the primary display's DPI, so a 150%-scaled 4K panel
/// reports as if it were 2560x1440. The layout would then be built from
/// fabricated geometry and the cursor would cross edges in the wrong places.
///
/// Failure is deliberately ignored: the awareness may already have been set by a
/// manifest or by the host application (the Tauri UI does set it), and that is a
/// success for our purposes, not an error.
pub fn ensure_dpi_awareness() {
    DPI_INIT.call_once(|| {
        // SAFETY: no arguments beyond a constant context value, and the call is
        // idempotent from our point of view. The only documented failure is
        // "awareness already set", which is the outcome we want anyway.
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
    });
}

pub struct WindowsDisplays;

impl WindowsDisplays {
    pub fn new() -> Self {
        ensure_dpi_awareness();
        Self
    }
}

impl Default for WindowsDisplays {
    fn default() -> Self {
        Self::new()
    }
}

impl crate::traits::DisplayEnumerator for WindowsDisplays {
    fn monitors(&self) -> Result<Vec<Monitor>> {
        let handles = enumerate_handles()?;
        let mut raw: Vec<RawMonitor> = Vec::with_capacity(handles.len());
        for h in handles {
            if let Some(m) = describe(h) {
                raw.push(m);
            }
        }

        // Ids come from the device name, so a replugged monitor keeps its place in
        // a saved layout. Collisions are impossible in practice but would make a
        // display unaddressable, so they are forced apart rather than trusted.
        let mut ids: Vec<u32> = raw.iter().map(|m| stable_monitor_id(&m.device)).collect();
        deduplicate_ids(&mut ids);

        Ok(raw
            .into_iter()
            .zip(ids)
            .map(|(m, id)| Monitor {
                id: MonitorId(id),
                name: m.display_name,
                local_bounds: m.bounds,
                scale: m.scale,
                primary: m.primary,
            })
            .collect())
    }
}

struct RawMonitor {
    /// GDI device name, e.g. `\\.\DISPLAY2`. The identity source.
    device: String,
    /// Friendlier name for the layout editor.
    display_name: String,
    bounds: Rect,
    scale: f32,
    primary: bool,
}

fn enumerate_handles() -> Result<Vec<HMONITOR>> {
    let mut handles: Vec<HMONITOR> = Vec::new();

    // SAFETY: `handles` outlives the enumeration because EnumDisplayMonitors is
    // synchronous — it returns only after the last callback. The callback receives
    // the vector by raw pointer and does nothing else with it.
    unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(collect_handle),
            LPARAM(&mut handles as *mut Vec<HMONITOR> as isize),
        )
        .ok()
        .map_err(|e| PlatformError::Os {
            context: "EnumDisplayMonitors",
            code: e.code().0,
        })?;
    }

    Ok(handles)
}

/// SAFETY: `data` is the `&mut Vec<HMONITOR>` passed to `EnumDisplayMonitors`
/// above, and this callback only ever runs during that synchronous call.
unsafe extern "system" fn collect_handle(
    monitor: HMONITOR,
    _hdc: HDC,
    _clip: *mut RECT,
    data: LPARAM,
) -> BOOL {
    let handles = &mut *(data.0 as *mut Vec<HMONITOR>);
    handles.push(monitor);
    // Non-zero keeps the enumeration going.
    BOOL(1)
}

fn describe(monitor: HMONITOR) -> Option<RawMonitor> {
    let mut info = MONITORINFOEXW {
        monitorInfo: MONITORINFO {
            cbSize: core::mem::size_of::<MONITORINFOEXW>() as u32,
            ..Default::default()
        },
        ..Default::default()
    };

    // SAFETY: `cbSize` is set to the extended struct's size, which is how
    // GetMonitorInfoW is told to fill in `szDevice` as well. The pointer cast to
    // the base struct is the documented calling convention.
    let ok = unsafe { GetMonitorInfoW(monitor, &mut info as *mut _ as *mut MONITORINFO) };
    if !ok.as_bool() {
        // A monitor can vanish between enumeration and query; skipping it is
        // correct, and failing the whole call would make a hot-unplug look like a
        // fatal error.
        return None;
    }

    let r = info.monitorInfo.rcMonitor;
    let device = wide_to_string(&info.szDevice);

    Some(RawMonitor {
        display_name: friendly_name(&device).unwrap_or_else(|| device.clone()),
        bounds: rect_from_win32(r),
        scale: effective_scale(monitor),
        primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
        device,
    })
}

/// Per-monitor scale factor.
///
/// Falls back to 1.0 rather than failing: the scale is reported for display
/// purposes only — wire positions are normalized, so routing never depends on it
/// — and a mixed-DPI desk should not lose a monitor because one query failed.
fn effective_scale(monitor: HMONITOR) -> f32 {
    let mut dpi_x = 0u32;
    let mut dpi_y = 0u32;
    // SAFETY: both out-pointers are to live stack locals.
    let ok = unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) };
    if ok.is_err() || dpi_x == 0 {
        return 1.0;
    }
    dpi_x as f32 / DEFAULT_DPI
}

/// The monitor's marketing name, e.g. `Dell U2720Q`, via a second-level device
/// query. Used only for display; identity stays with the GDI device name.
fn friendly_name(device: &str) -> Option<String> {
    let mut wide: Vec<u16> = device.encode_utf16().collect();
    wide.push(0);

    let mut dd = windows::Win32::Graphics::Gdi::DISPLAY_DEVICEW {
        cb: core::mem::size_of::<windows::Win32::Graphics::Gdi::DISPLAY_DEVICEW>() as u32,
        ..Default::default()
    };

    // SAFETY: `wide` is NUL-terminated and outlives the call; `dd.cb` is set so
    // the OS knows how much of the struct it may write.
    let ok = unsafe { EnumDisplayDevicesW(PCWSTR(wide.as_ptr()), 0, &mut dd, 0) };
    if !ok.as_bool() {
        return None;
    }
    let name = wide_to_string(&dd.DeviceString);
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

fn wide_to_string(buf: &[u16]) -> String {
    let len = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// Convert a Win32 `RECT` (left/top/right/bottom) into the protocol's
/// origin-plus-extent form.
///
/// Saturating rather than wrapping: a corrupted or inverted rectangle should
/// become an empty monitor that the union skips, not a rectangle claiming four
/// billion pixels.
pub fn rect_from_win32(r: RECT) -> Rect {
    let w = (r.right as i64 - r.left as i64).clamp(0, u32::MAX as i64) as u32;
    let h = (r.bottom as i64 - r.top as i64).clamp(0, u32::MAX as i64) as u32;
    Rect::new(r.left, r.top, w, h)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::DisplayEnumerator;

    #[test]
    fn win32_rect_becomes_origin_and_extent() {
        assert_eq!(
            rect_from_win32(RECT {
                left: -1920,
                top: 120,
                right: 0,
                bottom: 1200,
            }),
            Rect::new(-1920, 120, 1920, 1080)
        );
    }

    #[test]
    fn an_inverted_rect_becomes_empty_rather_than_enormous() {
        let r = rect_from_win32(RECT {
            left: 100,
            top: 100,
            right: 0,
            bottom: 0,
        });
        assert!(r.is_empty());
    }

    #[test]
    fn wide_strings_stop_at_the_first_nul() {
        let mut buf = [0u16; 32];
        for (i, c) in "DISPLAY1".encode_utf16().enumerate() {
            buf[i] = c;
        }
        assert_eq!(wide_to_string(&buf), "DISPLAY1");
    }

    #[test]
    fn wide_strings_without_a_nul_use_the_whole_buffer() {
        let buf: Vec<u16> = "abc".encode_utf16().collect();
        assert_eq!(wide_to_string(&buf), "abc");
    }

    /// Runs against the real desktop. Asserts only invariants that must hold on
    /// any machine, including CI agents with a single virtual display.
    #[test]
    fn enumerating_this_machine_yields_consistent_monitors() {
        let monitors = WindowsDisplays::new().monitors().unwrap();
        assert!(
            !monitors.is_empty(),
            "Windows always has at least one display"
        );
        assert!(monitors.iter().filter(|m| m.primary).count() <= 1);
        for m in &monitors {
            assert!(!m.local_bounds.is_empty(), "{m:?} has no extent");
            assert!(m.scale > 0.0, "{m:?} has a nonsensical scale");
        }
        let mut ids: Vec<_> = monitors.iter().map(|m| m.id).collect();
        ids.sort();
        let count = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), count, "monitor ids must be unique");
    }

    #[test]
    fn enumeration_is_stable_across_calls() {
        // Ids derived from enumeration order would fail this the moment the OS
        // returned monitors in a different order.
        let d = WindowsDisplays::new();
        let first = d.monitors().unwrap();
        let second = d.monitors().unwrap();
        assert_eq!(
            first.iter().map(|m| m.id).collect::<Vec<_>>(),
            second.iter().map(|m| m.id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn virtual_bounds_contain_every_monitor() {
        let d = WindowsDisplays::new();
        let bounds = d.virtual_bounds().unwrap();
        for m in d.monitors().unwrap() {
            assert!(bounds.x <= m.local_bounds.x);
            assert!(bounds.right() >= m.local_bounds.right());
        }
    }
}
