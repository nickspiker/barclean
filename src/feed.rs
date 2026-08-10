//! The camera feed: a hand-off between three threads that must not touch each other's memory.
//!
//! # The threads
//!
//! - **Camera** — Android's `ImageReader` callback. Delivers frames at 30 Hz and must return fast;
//!   anything slow here stalls capture and the frames back up.
//! - **Decode** — owned by this module. Takes the newest frame available and cleans it.
//! - **Render** — the Choreographer callback. Draws the preview and the chrome, every vsync.
//!
//! # Why this exists
//!
//! Before it, the camera thread reached straight into the app (`shell.app().on_camera_frame(..)`)
//! and mutated the luma buffer while the render thread was reading it for the preview. That is a
//! data race, and it shows up on screen as **tearing**: the top of the frame is one capture and the
//! bottom is the next. Decoding also ran inline on the camera callback, so a 40 ms clean at preview
//! resolution held up capture on a 33 ms frame budget.
//!
//! The fix is one owner per piece of state and immutable snapshots between them. Frames are
//! `Arc<Frame>`; a thread takes a clone of the pointer and reads it for as long as it likes, while
//! the camera thread swaps a new one in. Nothing is ever mutated in place, so nothing can be half
//! updated when someone else looks.
//!
//! # Always the newest frame, never a queue
//!
//! `pending` holds exactly one frame. A camera frame arriving while the decoder is busy *replaces*
//! the waiting one rather than queueing behind it. Queueing would be actively wrong here: the user
//! is aiming a phone, and a decode of a frame from two seconds ago answers a question they have
//! stopped asking. Dropped frames are the correct behaviour, not a compromise.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

/// One camera frame's luminance plane, immutable once published.
#[derive(Clone, Default)]
pub struct Frame {
    pub luma: Vec<u8>,
    pub width: usize,
    pub height: usize,
    /// Clockwise rotation to bring the sensor upright, from `SENSOR_ORIENTATION`.
    pub rotation: u32,
}

impl Frame {
    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0 || self.luma.is_empty()
    }

    /// Dimensions after rotation — swapped on the quarter turns.
    pub fn rotated_dims(&self) -> (usize, usize) {
        match self.rotation {
            90 | 270 => (self.height, self.width),
            _ => (self.width, self.height),
        }
    }

    /// Sample the upright image at `(x, y)`, mapping back through the rotation.
    ///
    /// Rotating at sample time rather than rotating the buffer keeps the decoder on the sensor's own
    /// pixels: a rotation would cost a full copy every frame and resample module edges, which are
    /// the entire signal.
    pub fn sample_upright(&self, x: usize, y: usize) -> u8 {
        let (ox, oy) = match self.rotation {
            90 => (y, self.height.saturating_sub(1).saturating_sub(x)),
            180 => (
                self.width.saturating_sub(1).saturating_sub(x),
                self.height.saturating_sub(1).saturating_sub(y),
            ),
            270 => (self.width.saturating_sub(1).saturating_sub(y), x),
            _ => (x, y),
        };
        if ox >= self.width || oy >= self.height {
            return 0;
        }
        self.luma[oy * self.width + ox]
    }
}

/// What a decode attempt concluded, with the frame it ran on.
pub struct Decoded {
    pub result: Result<crate::clean::Cleaned, crate::clean::CleanError>,
    /// Stock multi-format fallback, when the cleaner declined and something else read it.
    pub fallback: Option<(String, String)>,
    pub elapsed_ms: u32,
}

#[derive(Default)]
struct Slot {
    pending: Option<Arc<Frame>>,
    stopping: bool,
}

/// Shared state between the camera, decode and render threads.
pub struct CameraFeed {
    /// Newest frame, for the preview. Read by render, written by camera.
    latest: Mutex<Option<Arc<Frame>>>,
    /// The single frame waiting to be decoded, plus the shutdown flag.
    slot: Mutex<Slot>,
    wake: Condvar,
    /// Newest decode outcome, awaiting collection by the render thread.
    outcome: Mutex<Option<Decoded>>,
    /// Frames accepted since launch.
    frames: AtomicU64,
    /// While set, frames are still shown but not decoded — the app is holding a frozen result and
    /// a new decode would overwrite it.
    frozen: AtomicBool,
}

impl Default for CameraFeed {
    fn default() -> Self {
        Self::new()
    }
}

impl CameraFeed {
    pub fn new() -> Self {
        Self {
            latest: Mutex::new(None),
            slot: Mutex::new(Slot::default()),
            wake: Condvar::new(),
            outcome: Mutex::new(None),
            frames: AtomicU64::new(0),
            frozen: AtomicBool::new(false),
        }
    }

    /// Publish a frame from the camera thread. Returns quickly by design.
    ///
    /// `row_stride` is not necessarily `width` — Camera2 pads rows to a hardware alignment, and
    /// treating stride as width shears the image progressively down the frame.
    pub fn submit(
        &self,
        luma: &[u8],
        width: usize,
        height: usize,
        row_stride: usize,
        rotation: u32,
    ) {
        if width == 0 || height == 0 {
            return;
        }
        let stride = row_stride.max(width);
        let mut tight = Vec::with_capacity(width * height);
        for y in 0..height {
            let start = y * stride;
            let end = start + width;
            if end > luma.len() {
                break;
            }
            tight.extend_from_slice(&luma[start..end]);
        }
        if tight.len() < width * height {
            return;
        }

        let frame = Arc::new(Frame {
            luma: tight,
            width,
            height,
            rotation: rotation % 360,
        });
        self.frames.fetch_add(1, Ordering::Relaxed);

        *self.latest.lock().unwrap() = Some(Arc::clone(&frame));

        if !self.frozen.load(Ordering::Relaxed) {
            let mut slot = self.slot.lock().unwrap();
            // Replace rather than queue: the freshest frame is the only one worth decoding.
            slot.pending = Some(frame);
            drop(slot);
            self.wake.notify_one();
        }
    }

    /// Newest frame for the preview. Cheap — clones an `Arc`, not the pixels.
    pub fn latest(&self) -> Option<Arc<Frame>> {
        self.latest.lock().unwrap().clone()
    }

    pub fn frames(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }

    /// Stop or resume decoding. Frames keep flowing to the preview either way, so returning from a
    /// frozen result is instant rather than waiting for the camera to spin back up.
    pub fn set_frozen(&self, frozen: bool) {
        self.frozen.store(frozen, Ordering::Relaxed);
        if frozen {
            // Drop anything already waiting so the worker does not publish a stale outcome over
            // the result the user is looking at.
            self.slot.lock().unwrap().pending = None;
            *self.outcome.lock().unwrap() = None;
        }
    }

    pub fn is_frozen(&self) -> bool {
        self.frozen.load(Ordering::Relaxed)
    }

    /// Take the newest decode outcome, if one has landed since the last call.
    pub fn take_outcome(&self) -> Option<Decoded> {
        self.outcome.lock().unwrap().take()
    }

    /// Ask the worker to finish. Idempotent.
    pub fn stop(&self) {
        self.slot.lock().unwrap().stopping = true;
        self.wake.notify_all();
    }

    /// Block until there is a frame to decode, or shutdown. `None` means stop.
    fn next_to_decode(&self) -> Option<Arc<Frame>> {
        let mut slot = self.slot.lock().unwrap();
        loop {
            if slot.stopping {
                return None;
            }
            if let Some(frame) = slot.pending.take() {
                return Some(frame);
            }
            slot = self.wake.wait(slot).unwrap();
        }
    }
}

/// Spawn the decode worker.
///
/// Owns the whole cleaning path. Nothing it touches is shared mutably: it reads an `Arc<Frame>`
/// snapshot and publishes an owned `Decoded`.
pub fn spawn_worker(feed: Arc<CameraFeed>) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("barclean-decode".into())
        .spawn(move || {
            while let Some(frame) = feed.next_to_decode() {
                if frame.is_empty() || feed.is_frozen() {
                    continue;
                }
                let started = std::time::Instant::now();
                let result =
                    crate::clean::clean_luma(&frame.luma, frame.width as u32, frame.height as u32);
                let fallback = if result.is_err() {
                    rxing::helpers::detect_in_luma(
                        frame.luma.clone(),
                        frame.width as u32,
                        frame.height as u32,
                        None,
                    )
                    .ok()
                    .map(|r| (r.getText().to_string(), format!("{:?}", r.getBarcodeFormat())))
                } else {
                    None
                };
                let elapsed_ms = started.elapsed().as_millis() as u32;

                // Drop the result if the app froze while this was running — the user is looking at
                // a recovery and must not have it replaced from underneath.
                if feed.is_frozen() {
                    continue;
                }
                *feed.outcome.lock().unwrap() = Some(Decoded {
                    result,
                    fallback,
                    elapsed_ms,
                });
            }
        })
        .expect("spawn decode worker")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_with_frame() -> Arc<CameraFeed> {
        let feed = Arc::new(CameraFeed::new());
        feed.submit(&vec![128u8; 16 * 16], 16, 16, 16, 0);
        feed
    }

    #[test]
    fn stride_padding_is_removed() {
        // Camera2 pads rows; treating stride as width shears the image down the frame.
        let (w, h, stride) = (4usize, 3usize, 7usize);
        let mut padded = vec![0u8; stride * h];
        for y in 0..h {
            for x in 0..w {
                padded[y * stride + x] = (y * w + x) as u8;
            }
        }
        let feed = CameraFeed::new();
        feed.submit(&padded, w, h, stride, 0);

        let frame = feed.latest().expect("frame published");
        assert_eq!(frame.luma.len(), w * h);
        assert_eq!(frame.luma, (0..(w * h) as u8).collect::<Vec<_>>());
    }

    #[test]
    fn a_short_buffer_is_rejected_rather_than_padded() {
        let feed = CameraFeed::new();
        feed.submit(&[1, 2, 3], 10, 10, 10, 0);
        assert!(feed.latest().is_none(), "a truncated frame must not be published");
        assert_eq!(
            feed.frames(),
            0,
            "a rejected frame must not be counted — the counter is the diagnostic for whether real \
             frames are arriving, and inflating it would hide a broken capture path"
        );
    }

    #[test]
    fn newest_frame_replaces_the_waiting_one() {
        // The decoder must always get the freshest frame. Queueing would answer a question the
        // user stopped asking two seconds ago.
        let feed = CameraFeed::new();
        for v in 1..=5u8 {
            feed.submit(&vec![v; 4 * 4], 4, 4, 4, 0);
        }
        let next = feed.next_to_decode().expect("something to decode");
        assert_eq!(next.luma[0], 5, "should decode the newest, not the oldest");
        assert_eq!(feed.frames(), 5);
    }

    #[test]
    fn freezing_stops_decoding_but_not_the_preview() {
        let feed = feed_with_frame();
        feed.set_frozen(true);

        feed.submit(&vec![77u8; 16 * 16], 16, 16, 16, 0);
        assert_eq!(
            feed.latest().unwrap().luma[0],
            77,
            "preview must keep updating so returning is instant"
        );
        assert!(
            feed.slot.lock().unwrap().pending.is_none(),
            "nothing should be queued for decode while frozen"
        );

        feed.set_frozen(false);
        feed.submit(&vec![99u8; 16 * 16], 16, 16, 16, 0);
        assert!(feed.slot.lock().unwrap().pending.is_some(), "decoding resumes");
    }

    #[test]
    fn freezing_discards_an_outcome_in_flight() {
        let feed = feed_with_frame();
        *feed.outcome.lock().unwrap() = Some(Decoded {
            result: Err(crate::clean::CleanError::NotDetected),
            fallback: None,
            elapsed_ms: 1,
        });
        feed.set_frozen(true);
        assert!(
            feed.take_outcome().is_none(),
            "a result must never replace what the user is looking at"
        );
    }

    #[test]
    fn stop_releases_a_waiting_worker() {
        let feed = Arc::new(CameraFeed::new());
        let handle = {
            let feed = Arc::clone(&feed);
            std::thread::spawn(move || feed.next_to_decode().is_none())
        };
        // Give the worker a moment to park on the condvar, then release it.
        std::thread::sleep(std::time::Duration::from_millis(50));
        feed.stop();
        assert!(handle.join().unwrap(), "stop must wake the worker with None");
    }

    #[test]
    fn worker_decodes_a_real_symbol_off_the_thread() {
        use crate::Symbology;
        use crate::corpus::symbol;

        let spec = symbol::generate(Symbology::QrCode, "threaded decode", "M").unwrap();
        let img = symbol::render(&spec, 6, 6);
        let (w, h) = (img.width() as usize, img.height() as usize);
        let luma: Vec<u8> = img.pixels().map(|p| p.0[0]).collect();

        let feed = Arc::new(CameraFeed::new());
        let worker = spawn_worker(Arc::clone(&feed));
        feed.submit(&luma, w, h, w, 0);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let payload = loop {
            if let Some(outcome) = feed.take_outcome() {
                break outcome.result.map(|c| c.payload).ok();
            }
            assert!(std::time::Instant::now() < deadline, "worker never produced an outcome");
            std::thread::sleep(std::time::Duration::from_millis(10));
        };

        feed.stop();
        worker.join().unwrap();
        assert_eq!(payload.as_deref(), Some("threaded decode"));
    }
}
