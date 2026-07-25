//! Manual smoke test for the real Windows capture backend.
//!
//! `#[ignore]`d because it needs an interactive desktop session: on a headless CI
//! agent, in a service context, or over a disconnected RDP session, `BitBlt`
//! against the desktop DC returns black or fails outright, and a test that
//! depends on a screen existing is a test that fails for reasons unrelated to the
//! code. Run it by hand after touching the GDI path:
//!
//! ```text
//! cargo test -p wx-video --test windows_capture_smoke -- --ignored --nocapture
//! ```

#![cfg(target_os = "windows")]

use std::time::Duration;

use wx_proto::{MonitorId, Rect};
use wx_video::{open_capture, CaptureConfig, Encoder, PassthroughDecoder, PassthroughEncoder};

/// A 64x64 region at the desktop origin. Inside the primary monitor on any real
/// display, so this needs no monitor enumeration.
fn origin_region() -> CaptureConfig {
    CaptureConfig::new(MonitorId(0), Rect::new(0, 0, 64, 64), 30)
}

#[test]
#[ignore = "requires an interactive desktop session"]
fn gdi_capture_produces_a_well_formed_frame() {
    let mut capture = open_capture(origin_region()).expect("open GDI capture");
    let frame = capture
        .next_frame()
        .expect("capture failed")
        .expect("GDI capture always returns a frame");

    assert_eq!(frame.width(), 64);
    assert_eq!(frame.height(), 64);
    assert_eq!(frame.stride(), 64 * 4);
    assert_eq!(frame.as_bytes().len(), 64 * 64 * 4);

    // Not asserted: that the pixels are non-black. A genuinely black corner of
    // the desktop is legal. Printed instead, because "all zeroes" is the
    // signature of the GetDIBits-while-selected mistake and is worth seeing.
    let non_zero = frame.as_bytes().iter().filter(|b| **b != 0).count();
    println!(
        "captured {}x{} stride {}, {non_zero} non-zero bytes, first pixel {:?}",
        frame.width(),
        frame.height(),
        frame.stride(),
        &frame.as_bytes()[..4]
    );
}

#[test]
#[ignore = "requires an interactive desktop session"]
fn repeated_capture_reuses_its_gdi_resources() {
    // Leaking a DC or bitmap per frame is the classic version of this code; 200
    // frames is enough that a leak shows up in Task Manager's GDI object count
    // while the test runs.
    let mut capture = open_capture(origin_region()).expect("open GDI capture");
    for i in 0..200 {
        capture
            .next_frame()
            .unwrap_or_else(|e| panic!("frame {i} failed: {e}"))
            .expect("a frame");
    }
}

#[test]
#[ignore = "requires an interactive desktop session"]
fn a_real_captured_frame_survives_the_passthrough_round_trip() {
    let mut capture = open_capture(origin_region()).expect("open GDI capture");
    let frame = capture.next_frame().unwrap().unwrap();

    let packet = PassthroughEncoder::raw().encode(&frame).unwrap().unwrap();
    let back = PassthroughDecoder::new()
        .decode_packet(&packet.data)
        .unwrap();

    assert_eq!(back.width(), frame.width());
    assert_eq!(back.height(), frame.height());
    assert_eq!(back.to_packed_bytes(), frame.to_packed_bytes());
    assert!(back.timestamp() < Duration::from_secs(60));
}
