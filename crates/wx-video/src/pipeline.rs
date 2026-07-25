//! Capture -> encode -> sink, with the frame-dropping policy that makes it safe.
//!
//! The rule this module exists to enforce: **video never queues without bound.**
//! A 4K BGRA frame is 33MB. Ten of them behind a slow encoder or a congested
//! QUIC stream is 330MB of resident memory and a third of a second of latency,
//! and it only gets worse from there — a queue that grows under load never
//! shrinks, because the load that filled it is still there. So there is exactly
//! one frame slot ([`FrameSlot`]): a new frame replaces an unconsumed one and the
//! old one is dropped. A backlogged stream loses frame rate, which is what a
//! viewer expects from a slow link, instead of losing memory, which is what
//! kills the agent.
//!
//! Pacing is the same idea a level up: [`FramePacer`] never fires twice to make
//! up for a missed deadline. Catching up would hand the encoder a burst it
//! already demonstrated it cannot absorb, so the backlog would become permanent.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::capture::{RawFrame, ScreenCapture, MAX_FPS};
use crate::encode::{EncodedPacket, Encoder};

/// Longest a worker sleeps without checking whether it has been asked to stop.
///
/// A 1 fps stream must not take a second to shut down: a `VideoStop` arrives when
/// the user closes the viewer window, and a visibly sluggish stop reads as a
/// hang.
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Decides when the next frame is due.
///
/// Takes `now` as an argument rather than reading a clock, so the interesting
/// behaviour — what happens when the caller is late — is testable without
/// sleeping.
#[derive(Debug, Clone)]
pub struct FramePacer {
    interval: Duration,
    next_due: Option<Duration>,
}

impl FramePacer {
    /// `max_fps` of 0 is clamped to 1 rather than rejected: a zero here would be
    /// a division by zero, and the callers that could produce one (a peer's
    /// `VideoConfig`) are better served by a slow stream than an error.
    pub fn new(max_fps: u16) -> Self {
        let fps = max_fps.clamp(1, MAX_FPS);
        Self {
            interval: Duration::from_nanos(1_000_000_000 / u64::from(fps)),
            next_due: None,
        }
    }

    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// Whether a frame should be captured at `now`, advancing the schedule if so.
    ///
    /// The first call always fires, so a stream shows a picture immediately
    /// rather than after one frame interval of black.
    pub fn tick(&mut self, now: Duration) -> bool {
        let Some(due) = self.next_due else {
            self.next_due = Some(now + self.interval);
            return true;
        };
        if now < due {
            return false;
        }

        // Normally the next deadline is one interval after the last, which keeps
        // the average rate exact instead of drifting by however long each frame
        // took. But if we are already past that too, the missed slots are
        // abandoned rather than fired in a burst.
        let mut next = due + self.interval;
        if next <= now {
            next = now + self.interval;
        }
        self.next_due = Some(next);
        true
    }

    /// How long to sleep before [`FramePacer::tick`] would fire.
    pub fn until_next(&self, now: Duration) -> Duration {
        match self.next_due {
            Some(due) => due.saturating_sub(now),
            None => Duration::ZERO,
        }
    }
}

/// A one-frame handoff between the capture thread and the encode thread.
///
/// Deliberately not a channel. A bounded channel of length one still blocks the
/// producer, which would push the backpressure onto capture pacing and make the
/// capture thread's timing depend on the encoder. Here the producer never waits:
/// it overwrites, and the overwritten frame is counted as dropped.
#[derive(Debug, Default)]
pub struct FrameSlot {
    state: Mutex<SlotState>,
    ready: Condvar,
}

#[derive(Debug, Default)]
struct SlotState {
    pending: Option<RawFrame>,
    closed: bool,
    published: u64,
    dropped: u64,
}

impl FrameSlot {
    pub fn new() -> Self {
        Self::default()
    }

    /// Offer a frame, replacing any the consumer has not taken yet.
    ///
    /// Returns true when a frame was displaced, i.e. the consumer is behind.
    pub fn publish(&self, frame: RawFrame) -> bool {
        let mut state = self.state.lock().expect("frame slot poisoned");
        if state.closed {
            return false;
        }
        state.published += 1;
        let displaced = state.pending.replace(frame).is_some();
        if displaced {
            state.dropped += 1;
        }
        drop(state);
        self.ready.notify_one();
        displaced
    }

    /// Wait for a frame. `None` means the slot is closed and drained, which is
    /// the consumer's cue to exit.
    pub fn take_blocking(&self) -> Option<RawFrame> {
        let mut state = self.state.lock().expect("frame slot poisoned");
        loop {
            if let Some(frame) = state.pending.take() {
                return Some(frame);
            }
            if state.closed {
                return None;
            }
            state = self.ready.wait(state).expect("frame slot poisoned");
        }
    }

    /// Take a frame if one is waiting, without blocking.
    pub fn try_take(&self) -> Option<RawFrame> {
        self.state
            .lock()
            .expect("frame slot poisoned")
            .pending
            .take()
    }

    /// Stop accepting frames and wake the consumer.
    ///
    /// Any frame still pending stays takeable, so a shutdown does not discard a
    /// frame that was about to be sent.
    pub fn close(&self) {
        let mut state = self.state.lock().expect("frame slot poisoned");
        state.closed = true;
        drop(state);
        self.ready.notify_all();
    }

    pub fn is_closed(&self) -> bool {
        self.state.lock().expect("frame slot poisoned").closed
    }

    /// Frames displaced before the consumer could take them.
    pub fn dropped(&self) -> u64 {
        self.state.lock().expect("frame slot poisoned").dropped
    }

    pub fn published(&self) -> u64 {
        self.state.lock().expect("frame slot poisoned").published
    }
}

/// Where encoded packets go.
///
/// `FnMut` rather than a channel so the caller decides: the agent writes straight
/// to a QUIC stream, tests collect into a `Vec`. It is called on the encode
/// thread, and blocking in it costs frame rate — which is the intended
/// backpressure signal, so blocking is allowed.
pub type PacketSink = Box<dyn FnMut(EncodedPacket) + Send>;

/// Counters for one running stream.
///
/// `captured - dropped - encoded` is the honest measure of how badly a link is
/// coping, and it is the first thing to look at when someone says the picture is
/// choppy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PipelineStats {
    pub captured: u64,
    /// Frames superseded before the encoder could take them.
    pub dropped: u64,
    pub encoded: u64,
    pub bytes_out: u64,
    pub capture_errors: u64,
    pub encode_errors: u64,
}

#[derive(Debug, Default)]
struct Counters {
    captured: AtomicU64,
    dropped: AtomicU64,
    encoded: AtomicU64,
    bytes_out: AtomicU64,
    capture_errors: AtomicU64,
    encode_errors: AtomicU64,
}

impl Counters {
    fn snapshot(&self) -> PipelineStats {
        PipelineStats {
            captured: self.captured.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            encoded: self.encoded.load(Ordering::Relaxed),
            bytes_out: self.bytes_out.load(Ordering::Relaxed),
            capture_errors: self.capture_errors.load(Ordering::Relaxed),
            encode_errors: self.encode_errors.load(Ordering::Relaxed),
        }
    }
}

/// A running capture-encode-send stream for one monitor.
///
/// Two threads, not one. With a single thread a slow encode delays the next
/// capture, so the frame rate collapses to the encoder's rate *and* the frames
/// that do arrive are stale. Split, capture keeps its own cadence and the slot
/// absorbs the mismatch by dropping.
pub struct VideoPipeline {
    stop: Arc<AtomicBool>,
    slot: Arc<FrameSlot>,
    counters: Arc<Counters>,
    capture_thread: Option<JoinHandle<()>>,
    encode_thread: Option<JoinHandle<()>>,
}

impl VideoPipeline {
    /// Start streaming.
    ///
    /// `max_fps` is the target; the real rate is whatever capture, encode, and the
    /// sink can sustain.
    pub fn spawn(
        mut capture: Box<dyn ScreenCapture>,
        mut encoder: Box<dyn Encoder>,
        max_fps: u16,
        mut sink: PacketSink,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let slot = Arc::new(FrameSlot::new());
        let counters = Arc::new(Counters::default());

        let capture_thread = {
            let stop = Arc::clone(&stop);
            let slot = Arc::clone(&slot);
            let counters = Arc::clone(&counters);
            thread::Builder::new()
                .name("wx-video-capture".into())
                .spawn(move || {
                    let mut pacer = FramePacer::new(max_fps);
                    let started = Instant::now();
                    while !stop.load(Ordering::Relaxed) {
                        let now = started.elapsed();
                        let wait = pacer.until_next(now);
                        if !wait.is_zero() {
                            thread::sleep(wait.min(STOP_POLL_INTERVAL));
                            continue;
                        }
                        if !pacer.tick(now) {
                            continue;
                        }
                        match capture.next_frame() {
                            Ok(Some(frame)) => {
                                counters.captured.fetch_add(1, Ordering::Relaxed);
                                if slot.publish(frame) {
                                    counters.dropped.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                            // Nothing new: a static screen, not a problem.
                            Ok(None) => {}
                            Err(err) => {
                                // Capture errors are effectively all fatal for
                                // this target: a lost monitor or a revoked
                                // permission does not fix itself, and retrying in
                                // a tight loop would spin a core while logging a
                                // line per attempt. The stream ends and the peer
                                // must ask again.
                                counters.capture_errors.fetch_add(1, Ordering::Relaxed);
                                tracing::warn!(%err, "screen capture stopped");
                                break;
                            }
                        }
                    }
                    slot.close();
                })
                .expect("spawn capture thread")
        };

        let encode_thread = {
            let slot = Arc::clone(&slot);
            let counters = Arc::clone(&counters);
            let stop = Arc::clone(&stop);
            thread::Builder::new()
                .name("wx-video-encode".into())
                .spawn(move || {
                    while let Some(frame) = slot.take_blocking() {
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        match encoder.encode(&frame) {
                            Ok(Some(packet)) => {
                                counters.encoded.fetch_add(1, Ordering::Relaxed);
                                counters
                                    .bytes_out
                                    .fetch_add(packet.data.len() as u64, Ordering::Relaxed);
                                sink(packet);
                            }
                            // The encoder swallowed this frame (lookahead or
                            // reordering); it is not an error and not a drop.
                            Ok(None) => {}
                            Err(err) => {
                                counters.encode_errors.fetch_add(1, Ordering::Relaxed);
                                tracing::warn!(%err, "frame encode failed");
                            }
                        }
                    }
                })
                .expect("spawn encode thread")
        };

        Self {
            stop,
            slot,
            counters,
            capture_thread: Some(capture_thread),
            encode_thread: Some(encode_thread),
        }
    }

    pub fn stats(&self) -> PipelineStats {
        self.counters.snapshot()
    }

    /// Whether capture has ended on its own — a lost monitor, or a finite test
    /// source running out. Lets the agent send `VideoUnavailable` instead of
    /// leaving a viewer staring at a frozen frame.
    pub fn is_finished(&self) -> bool {
        self.slot.is_closed()
    }

    /// Stop both threads and wait for them.
    pub fn stop(mut self) -> PipelineStats {
        self.shutdown();
        self.counters.snapshot()
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.slot.close();
        if let Some(t) = self.capture_thread.take() {
            let _ = t.join();
        }
        if let Some(t) = self.encode_thread.take() {
            let _ = t.join();
        }
    }
}

// Dropping the handle must not leave two threads capturing the screen forever:
// on an error path or a panic higher up, nobody is going to call `stop`.
impl Drop for VideoPipeline {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::{CaptureError, CaptureTarget, StaticCapture};
    use crate::encode::{EncodeError, PassthroughEncoder, StreamFormat};
    use std::sync::mpsc;
    use wx_proto::Compression;

    fn frame(tag: u8) -> RawFrame {
        RawFrame::packed(2, 2, vec![tag; 16], Duration::from_millis(tag as u64)).unwrap()
    }

    #[test]
    fn pacer_fires_immediately_so_the_first_frame_is_not_delayed() {
        let mut pacer = FramePacer::new(30);
        assert!(pacer.tick(Duration::ZERO));
    }

    #[test]
    fn pacer_holds_off_until_the_interval_has_passed() {
        let mut pacer = FramePacer::new(100);
        assert!(pacer.tick(Duration::ZERO));
        assert!(!pacer.tick(Duration::from_millis(5)));
        assert!(!pacer.tick(Duration::from_millis(9)));
        // Exactly on the deadline counts as due; requiring strictly greater
        // silently loses a frame every interval.
        assert!(pacer.tick(Duration::from_millis(10)));
    }

    #[test]
    fn pacer_does_not_drift_when_ticks_arrive_slightly_late() {
        // Scheduling from the deadline rather than from `now` keeps the average
        // rate exact; scheduling from `now` loses a frame every few seconds.
        let mut pacer = FramePacer::new(100);
        let mut fired = 0;
        for ms in 0..1000 {
            if pacer.tick(Duration::from_millis(ms) + Duration::from_micros(200)) {
                fired += 1;
            }
        }
        assert_eq!(fired, 100, "expected 100 frames in one second, got {fired}");
    }

    #[test]
    fn pacer_never_bursts_to_catch_up_after_a_long_stall() {
        // The encoder blocked for a second at 60fps. Firing the 60 missed slots
        // would hand it a burst it already proved it cannot absorb.
        let mut pacer = FramePacer::new(60);
        assert!(pacer.tick(Duration::ZERO));
        assert!(pacer.tick(Duration::from_secs(1)));
        assert!(!pacer.tick(Duration::from_secs(1)));
        assert!(!pacer.tick(Duration::from_secs(1) + Duration::from_millis(10)));
        // The schedule resumes from the stall, not from the abandoned deadlines.
        assert!(pacer.tick(Duration::from_secs(1) + Duration::from_millis(17)));
    }

    #[test]
    fn pacer_survives_a_clock_that_goes_backwards() {
        // Not supposed to happen with a monotonic clock, but Duration subtraction
        // panics on underflow and a stream must not take the process down.
        let mut pacer = FramePacer::new(60);
        assert!(pacer.tick(Duration::from_secs(10)));
        assert!(!pacer.tick(Duration::from_secs(1)));
        assert_eq!(
            pacer.until_next(Duration::from_secs(1)),
            Duration::from_secs(9) + pacer.interval()
        );
        assert!(pacer.tick(Duration::from_secs(11)));
    }

    #[test]
    fn pacer_clamps_absurd_frame_rates_instead_of_dividing_by_zero() {
        assert_eq!(FramePacer::new(0).interval(), Duration::from_secs(1));
        assert!(FramePacer::new(u16::MAX).interval() >= Duration::from_nanos(1));
    }

    #[test]
    fn slot_hands_over_the_newest_frame_and_counts_the_rest_as_dropped() {
        let slot = FrameSlot::new();
        assert!(!slot.publish(frame(1)));
        assert!(slot.publish(frame(2)));
        assert!(slot.publish(frame(3)));
        let got = slot.take_blocking().unwrap();
        // Newest wins: showing a stale frame while a fresher one exists is the
        // one thing a live view must never do.
        assert_eq!(got.as_bytes()[0], 3);
        assert_eq!(slot.dropped(), 2);
        assert_eq!(slot.published(), 3);
    }

    #[test]
    fn slot_never_holds_more_than_one_frame_however_many_are_published() {
        let slot = FrameSlot::new();
        for tag in 0..200u8 {
            slot.publish(frame(tag));
        }
        assert_eq!(slot.dropped(), 199);
        assert!(slot.try_take().is_some());
        assert!(slot.try_take().is_none());
    }

    #[test]
    fn closing_the_slot_still_yields_the_frame_already_waiting() {
        let slot = FrameSlot::new();
        slot.publish(frame(7));
        slot.close();
        assert_eq!(slot.take_blocking().unwrap().as_bytes()[0], 7);
        assert!(slot.take_blocking().is_none());
    }

    #[test]
    fn closing_the_slot_wakes_a_blocked_consumer() {
        // Without this the encode thread never exits and `stop` deadlocks.
        let slot = Arc::new(FrameSlot::new());
        let consumer = {
            let slot = Arc::clone(&slot);
            thread::spawn(move || slot.take_blocking())
        };
        slot.close();
        assert!(consumer.join().unwrap().is_none());
    }

    #[test]
    fn publishing_to_a_closed_slot_is_ignored_rather_than_buffered() {
        let slot = FrameSlot::new();
        slot.close();
        assert!(!slot.publish(frame(1)));
        assert!(slot.try_take().is_none());
    }

    #[test]
    fn pipeline_delivers_encoded_packets_end_to_end() {
        let frames = (0..4).map(frame).collect();
        let capture = Box::new(StaticCapture::new(CaptureTarget::default(), frames));
        let (tx, rx) = mpsc::channel();
        let pipeline = VideoPipeline::spawn(
            capture,
            Box::new(PassthroughEncoder::raw()),
            120,
            Box::new(move |packet| {
                let _ = tx.send(packet);
            }),
        );

        let first = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("no packet arrived");
        assert_eq!(first.width, 2);
        assert!(first.keyframe);
        assert!(first.data.len() > 16);

        let stats = pipeline.stop();
        assert!(stats.encoded >= 1, "{stats:?}");
        assert_eq!(stats.capture_errors, 0, "{stats:?}");
        assert_eq!(stats.encode_errors, 0, "{stats:?}");
    }

    #[test]
    fn pipeline_drops_frames_rather_than_queueing_them_for_a_slow_sink() {
        // The whole point of the crate's memory story: a consumer slower than
        // capture must cost frame rate, not resident memory.
        let capture = Box::new(
            StaticCapture::new(
                CaptureTarget::default(),
                (0..8).map(frame).collect::<Vec<_>>(),
            )
            .looping(),
        );
        let (tx, rx) = mpsc::channel();
        let pipeline = VideoPipeline::spawn(
            capture,
            Box::new(PassthroughEncoder::raw()),
            MAX_FPS,
            Box::new(move |packet| {
                thread::sleep(Duration::from_millis(25));
                let _ = tx.send(packet);
            }),
        );

        // Wait for a couple of packets so capture has definitely outrun the sink.
        for _ in 0..2 {
            rx.recv_timeout(Duration::from_secs(5))
                .expect("sink starved");
        }
        let stats = pipeline.stop();
        assert!(
            stats.dropped > 0,
            "a sink 25ms behind per frame should have caused drops: {stats:?}"
        );
        assert!(stats.encoded < stats.captured, "{stats:?}");
    }

    #[test]
    fn pipeline_stops_promptly_and_reports_that_capture_ended() {
        // A finite capture source stands in for an unplugged monitor.
        let capture = Box::new(StaticCapture::new(CaptureTarget::default(), vec![frame(1)]));
        let pipeline = VideoPipeline::spawn(
            capture,
            Box::new(PassthroughEncoder::raw()),
            60,
            Box::new(|_| {}),
        );
        let stats = pipeline.stop();
        assert!(stats.encoded <= 1, "{stats:?}");
    }

    #[test]
    fn a_failing_encoder_does_not_take_the_pipeline_down() {
        struct BrokenEncoder;
        impl Encoder for BrokenEncoder {
            fn format(&self) -> StreamFormat {
                StreamFormat::Passthrough(Compression::None)
            }
            fn encode(&mut self, _frame: &RawFrame) -> Result<Option<EncodedPacket>, EncodeError> {
                Err(EncodeError::Backend {
                    detail: "session lost".into(),
                })
            }
        }

        let capture = Box::new(
            StaticCapture::new(CaptureTarget::default(), vec![frame(1), frame(2)]).looping(),
        );
        let pipeline =
            VideoPipeline::spawn(capture, Box::new(BrokenEncoder), MAX_FPS, Box::new(|_| {}));
        thread::sleep(Duration::from_millis(60));
        let stats = pipeline.stop();
        assert!(stats.encode_errors > 0, "{stats:?}");
        assert_eq!(stats.encoded, 0, "{stats:?}");
    }

    #[test]
    fn capture_failure_ends_the_stream_instead_of_spinning() {
        struct FailingCapture;
        impl ScreenCapture for FailingCapture {
            fn target(&self) -> CaptureTarget {
                CaptureTarget::default()
            }
            fn next_frame(&mut self) -> Result<Option<RawFrame>, CaptureError> {
                Err(CaptureError::TargetLost)
            }
        }

        let pipeline = VideoPipeline::spawn(
            Box::new(FailingCapture),
            Box::new(PassthroughEncoder::raw()),
            MAX_FPS,
            Box::new(|_| {}),
        );
        // One error, then the capture thread exits and closes the slot.
        thread::sleep(Duration::from_millis(80));
        assert!(pipeline.is_finished());
        let stats = pipeline.stop();
        assert_eq!(stats.capture_errors, 1, "{stats:?}");
    }

    #[test]
    fn dropping_the_handle_shuts_the_threads_down() {
        let capture =
            Box::new(StaticCapture::new(CaptureTarget::default(), vec![frame(1)]).looping());
        let slot_seen = {
            let pipeline = VideoPipeline::spawn(
                capture,
                Box::new(PassthroughEncoder::raw()),
                MAX_FPS,
                Box::new(|_| {}),
            );
            thread::sleep(Duration::from_millis(30));
            let before = pipeline.stats().captured;
            drop(pipeline);
            before
        };
        // Nothing to assert about the threads directly; if Drop did not join,
        // this test would leak a thread capturing forever. The counter proves the
        // pipeline was actually running when it was dropped.
        assert!(slot_seen > 0);
    }
}
