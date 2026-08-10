//! The barclean application, shared by both hosts.
//!
//! One `FluorApp` implementation runs on the desktop shell (winit + softbuffer) and the Android
//! shell (ANativeWindow + Choreographer). The host differs; this does not. Desktop exists so the
//! decoder can be worked on at compiler speed against the corpus, and the phone exists because a
//! barcode cleaner that has never met a real camera is a research project rather than a tool.

use fluor::coord::Coord;
use fluor::event::CursorIcon as FCursorIcon;
use fluor::event::Event as FEvent;
use fluor::host::app::{Context, FluorApp};
use fluor::host::event_response::EventResponse;
use fluor::pixel::{Blend, BlendMode};
use fluor::Viewport;

use std::sync::Arc;

use crate::camera::{LensPicker, LensSpec, PickerParams};
use crate::feed::{CameraFeed, Frame};
use crate::render::{ModuleVerdict, Reconstructed};
use crate::ui::{self, LensButtons, ResultButtons, colour};

/// What the last decode attempt concluded.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum DecodeState {
    #[default]
    NoFrames,
    /// No symbol located. A framing, focus or resolution problem, not a damage problem.
    Searching,
    /// Read without needing any help — a stock decoder would have managed this too.
    Decoded(String),
    /// Recovered from damage that defeats stock decoding. `rescued` of `total` blocks came back
    /// only because of the bootstrap loop.
    Recovered {
        payload: String,
        rescued: usize,
        total: usize,
    },
    /// Located and read, but too damaged to recover.
    TooDamaged { decoded: usize, total: usize },
    /// Cleaned, but re-encoded rather than rebuilt bit-exactly.
    ///
    /// Aztec, DataMatrix and PDF417 land here — see `clean::any` for why the grade differs.
    Reencoded { payload: String, format: String },
}

impl DecodeState {
    pub fn payload(&self) -> Option<&str> {
        match self {
            DecodeState::Decoded(p)
            | DecodeState::Recovered { payload: p, .. }
            | DecodeState::Reencoded { payload: p, .. } => Some(p),
            _ => None,
        }
    }
}

/// The frozen result of a successful scan.
///
/// Held rather than recomputed: at the moment of recovery we have the corrected codewords and the
/// matrix they came from, and the very next camera frame would overwrite both. A recovery that took
/// effort to get is not something to make the user earn twice.
pub struct ResultView {
    pub payload: String,
    /// Source was light-on-dark; the export preserves that.
    pub inverted: bool,
    pub symbology: crate::Symbology,
    pub fidelity: crate::clean::Fidelity,
    pub rebuilt: Reconstructed,
    pub verdicts: Vec<ModuleVerdict>,
    pub dimension: usize,
    pub height: usize,
    pub recovered: usize,
    pub blocks_rescued: usize,
    pub blocks_total: usize,
}

impl ResultView {
    fn headline(&self) -> String {
        let what = self.symbology.name();
        if self.blocks_rescued > 0 {
            format!(
                "{what} recovered — {} of {} blocks rescued, {} modules repaired",
                self.blocks_rescued, self.blocks_total, self.recovered
            )
        } else if self.recovered > 0 {
            format!("{what} cleaned — {} modules repaired", self.recovered)
        } else {
            format!("{what} — {}", self.fidelity.label())
        }
    }
}

/// Which screen the app is on.
pub enum Screen {
    Scanning,
    /// Frozen on a successful scan. The camera keeps streaming (so returning is instant) but frames
    /// no longer touch the decode state.
    Result(Box<ResultView>),
}

pub struct BarcleanApp {
    /// Shared with the camera and decode threads. The app only ever *reads* snapshots from it, so
    /// nothing here can be caught half-updated.
    feed: Arc<CameraFeed>,
    /// The frame currently being drawn, held for the duration of a render so the preview cannot
    /// change under the blit.
    frame: Option<Arc<Frame>>,
    state: DecodeState,
    /// A frame has arrived that has not been drawn yet.
    ///
    /// fluor only repaints when the window is marked dirty, and the default `tick` returns `false`,
    /// so nothing ever marks it. Input does — which is why, without this, the preview advances one
    /// frame per touch and looks frozen otherwise. A camera app is the case where new content
    /// arrives with no user input at all, so it has to raise its own hand.
    pending_frame: bool,
    /// Frame number at the last diagnostic log line.
    last_logged_frame: u64,
    /// Wall time of the last decode attempt. A full clean at preview resolution is not free, and if
    /// it exceeds the frame interval the pipeline is running behind rather than failing.
    last_decode_ms: u32,
    /// Symbol width in modules and its measured pixels-per-module, from the last detection.
    last_symbol: Option<(usize, f32)>,
    picker: LensPicker,
    /// Physical camera currently streaming, as reported by the shim.
    current_lens: String,
    /// Set when the user taps a lens; the shim polls it and reconfigures the capture session.
    ///
    /// A poll rather than a callback because the reconfigure has to happen on the camera thread,
    /// and reaching back across JNI from inside a render pass to get there would be a deadlock
    /// waiting to happen.
    lens_request: Option<String>,
    buttons: LensButtons,
    screen: Screen,
    result_buttons: ResultButtons,
    /// PNG bytes awaiting a write by the platform layer, polled each frame.
    save_request: Option<Vec<u8>>,
    viewport: Viewport,
}

impl BarcleanApp {
    pub fn new(feed: Arc<CameraFeed>) -> Self {
        Self {
            feed,
            frame: None,
            state: DecodeState::NoFrames,
            pending_frame: false,
            last_decode_ms: 0,
            last_logged_frame: 0,
            last_symbol: None,
            picker: LensPicker::new(Vec::new(), PickerParams::default()),
            current_lens: String::new(),
            lens_request: None,
            buttons: LensButtons::default(),
            screen: Screen::Scanning,
            result_buttons: ResultButtons::default(),
            save_request: None,
            viewport: Viewport::new(1, 1),
        }
    }

    pub fn state(&self) -> &DecodeState {
        &self.state
    }

    pub fn frames(&self) -> u64 {
        self.feed.frames()
    }

    pub fn feed(&self) -> &Arc<CameraFeed> {
        &self.feed
    }

    /// One-line diagnostic for logcat and, shortly, the on-screen status text.
    pub fn status_line(&self) -> String {
        match &self.state {
            DecodeState::NoFrames => "no frames".to_string(),
            DecodeState::Searching => format!(
                "searching  {}x{}  rot{}",
                self.frame.as_ref().map_or(0, |f| f.width),
                self.frame.as_ref().map_or(0, |f| f.height),
                self.frame.as_ref().map_or(0, |f| f.rotation)
            ),
            DecodeState::Decoded(p) => format!("decoded: {p}"),
            DecodeState::Recovered {
                payload,
                rescued,
                total,
            } => format!("RECOVERED {rescued}/{total} blocks: {payload}"),
            DecodeState::TooDamaged { decoded, total } => {
                format!("too damaged: {decoded}/{total} blocks")
            }
            DecodeState::Reencoded { payload, format } => {
                format!("{format} re-encoded: {payload}")
            }
        }
    }

    /// Milliseconds the last decode attempt took.
    pub fn last_decode_ms(&self) -> u32 {
        self.last_decode_ms
    }

    pub fn lenses(&self) -> &[LensSpec] {
        self.picker.lenses()
    }

    pub fn current_lens(&self) -> &str {
        &self.current_lens
    }

    /// Install the physical lenses the shim enumerated.
    ///
    /// Idempotent by lens id. The shim re-enumerates whenever the camera is opened, which happens
    /// on both `surfaceChanged` and `onResume`, so a naive append gave every lens a duplicate
    /// button after the first resume. Deduplicating here rather than in Kotlin keeps the invariant
    /// where it can be tested.
    pub fn set_lenses(&mut self, lenses: Vec<LensSpec>, current: &str) {
        let mut unique: Vec<LensSpec> = Vec::with_capacity(lenses.len());
        for lens in lenses {
            match unique.iter_mut().find(|u| u.id == lens.id) {
                Some(existing) => *existing = lens,
                None => unique.push(lens),
            }
        }
        self.picker = LensPicker::new(unique, PickerParams::default());
        self.current_lens = current.to_string();
        self.pending_frame = true;
    }

    /// Record which physical camera is now streaming.
    pub fn set_current_lens(&mut self, id: &str) {
        self.current_lens = id.to_string();
        self.picker.clear_focus_failures();
        self.pending_frame = true;
    }

    /// Take a pending lens switch, if the user tapped one. Polled by the shim each frame.
    pub fn take_lens_request(&mut self) -> Option<String> {
        self.lens_request.take()
    }

    /// Pixels per module measured on the current frame, if a symbol was located.
    ///
    /// Derived from the symbol's module count against its extent in the frame. Feeds the picker's
    /// per-lens predictions, which is the number that makes a lens choice informed rather than a
    /// guess.
    fn measured_px_per_module(&self) -> (f32, u32) {
        match self.last_symbol {
            Some((dimension, px_per_module)) => (px_per_module, dimension as u32),
            None => (0.0, 0),
        }
    }

    /// Collect whatever the decode worker has produced since the last frame.
    ///
    /// Called from the render thread, which is the only thread that mutates app state — the camera
    /// and decode threads communicate exclusively through [`CameraFeed`] snapshots.
    fn collect(&mut self) {
        self.frame = self.feed.latest();

        let Some(outcome) = self.feed.take_outcome() else {
            return;
        };
        self.last_decode_ms = outcome.elapsed_ms;

        self.state = match outcome.result {
            Ok(c) => {
                if c.px_per_module > 0.0 {
                    self.last_symbol = Some((c.rebuilt.dimension, c.px_per_module));
                }
                let (rescued, total) = (c.blocks_rescued, c.blocks_total);
                let payload = c.payload.clone();
                let exact = c.fidelity == crate::clean::Fidelity::Exact;
                let name = c.symbology.name().to_string();
                // Freeze on any successful decode, not only a rescued one: a logo-branded code that
                // reads fine still carries a logo the user wants gone, and the pristine rebuild is
                // what they came for either way.
                self.freeze(c);
                match (exact, rescued > 0) {
                    (_, true) => DecodeState::Recovered { payload, rescued, total },
                    (true, false) => DecodeState::Decoded(payload),
                    (false, false) => DecodeState::Reencoded { payload, format: name },
                }
            }
            Err(crate::clean::CleanError::Unrecoverable { blocks_total, blocks_decoded }) => {
                DecodeState::TooDamaged { decoded: blocks_decoded, total: blocks_total }
            }
            Err(_) => DecodeState::Searching,
        };
        self.pending_frame = true;

        // Roughly once a second, from the render thread where the state is consistent. Gated on a
        // watermark rather than a modulus: collect() runs per vsync while frames arrive per
        // capture, so a modulus fires twice whenever the two happen to line up.
        if self.frames() >= self.last_logged_frame + 30 {
            self.last_logged_frame = self.frames();
            #[cfg(target_os = "android")]
            log::info!(
                "frame {} ({} ms): {}",
                self.frames(),
                self.last_decode_ms,
                self.status_line()
            );
        }
    }

    /// Freeze a successful clean into the result screen.
    fn freeze(&mut self, cleaned: crate::clean::CleanedAny) {
        // A comparison only exists where the sampled matrix aligns with the rebuild, which is the
        // exact path. For a re-encode the symbol may not even be the same size, so the grid shows
        // the restoration plainly rather than inventing a diff against a different symbol.
        let verdicts = cleaned
            .sampled
            .as_ref()
            .and_then(|s| crate::render::compare(&cleaned.rebuilt, s));
        let recovered = verdicts
            .as_ref()
            .map(|v| v.iter().filter(|v| v.recovered()).count())
            .unwrap_or(0);
        let verdicts = verdicts.unwrap_or_else(|| {
            cleaned
                .rebuilt
                .modules()
                .iter()
                .map(|&dark| {
                    if dark {
                        crate::render::ModuleVerdict::MatchedDark
                    } else {
                        crate::render::ModuleVerdict::MatchedLight
                    }
                })
                .collect()
        });

        self.feed.set_frozen(true);
        self.screen = Screen::Result(Box::new(ResultView {
            inverted: cleaned.source_inverted,
            payload: cleaned.payload,
            symbology: cleaned.symbology,
            fidelity: cleaned.fidelity,
            dimension: cleaned.rebuilt.dimension,
            height: cleaned.rebuilt.height(),
            rebuilt: cleaned.rebuilt,
            verdicts,
            recovered,
            blocks_rescued: cleaned.blocks_rescued,
            blocks_total: cleaned.blocks_total,
        }));
        self.pending_frame = true;
    }

    /// Return to the camera, discarding the frozen result.
    fn resume_scanning(&mut self) {
        self.feed.set_frozen(false);
        self.screen = Screen::Scanning;
        self.state = DecodeState::Searching;
        self.pending_frame = true;
    }

    /// Take PNG bytes the user asked to save. Polled by the platform layer each frame.
    pub fn take_save_request(&mut self) -> Option<Vec<u8>> {
        self.save_request.take()
    }

    /// Whether the app is showing a frozen result.
    pub fn is_frozen(&self) -> bool {
        matches!(self.screen, Screen::Result(_))
    }

    /// Blit the camera preview, letterboxed to preserve aspect ratio.
    ///
    /// Nearest-neighbour on purpose. This is a diagnostic view of what the decoder is actually
    /// being fed, and a smoothing filter would hide precisely the undersampling that makes symbols
    /// fail — a preview that looks better than the data is worse than useless here.
    fn blit_preview(&self, target: &mut [u32], w: usize, h: usize) {
        let Some(frame) = self.frame.as_ref() else {
            return;
        };
        if frame.is_empty() || w == 0 || h == 0 {
            return;
        }
        let (fw, fh) = frame.rotated_dims();
        if fw == 0 || fh == 0 {
            return;
        }
        let scale = (w as f32 / fw as f32).min(h as f32 / fh as f32);
        let dw = ((fw as f32 * scale) as usize).max(1).min(w);
        let dh = ((fh as f32 * scale) as usize).max(1).min(h);
        let ox = (w - dw) / 2;
        let oy = (h - dh) / 2;

        for y in 0..dh {
            let sy = y * fh / dh;
            for x in 0..dw {
                let sx = x * fw / dw;
                let v = frame.sample_upright(sx, sy);
                let idx = (oy + y) * w + (ox + x);
                if idx < target.len() {
                    // Under, not assign: the chrome was already painted on top of this.
                    target[idx] =
                        target[idx].under(colour(v, v, v, 255), BlendMode::Normal);
                }
            }
        }
    }

    /// Accent colour, headline and payload for the status readout.
    fn status_parts(&self) -> (u32, String, Option<String>) {
        match &self.state {
            DecodeState::NoFrames => (
                colour(180, 40, 40, 255),
                "waiting for camera".into(),
                None,
            ),
            DecodeState::Searching => (
                colour(200, 150, 30, 255),
                "searching…".into(),
                Some(format!(
                    "{}x{}  {} ms/frame",
                    self.frame.as_ref().map_or(0, |f| f.width),
                    self.frame.as_ref().map_or(0, |f| f.height),
                    self.last_decode_ms
                )),
            ),
            DecodeState::Decoded(p) => (
                colour(40, 170, 80, 255),
                "decoded".into(),
                Some(p.clone()),
            ),
            DecodeState::Recovered {
                payload,
                rescued,
                total,
            } => (
                colour(80, 140, 230, 255),
                format!("RECOVERED — {rescued} of {total} blocks rescued"),
                Some(payload.clone()),
            ),
            DecodeState::TooDamaged { decoded, total } => (
                colour(190, 90, 30, 255),
                format!("too damaged — {decoded}/{total} blocks"),
                None,
            ),
            DecodeState::Reencoded { payload, format } => (
                colour(150, 90, 200, 255),
                format!("{format} — re-encoded"),
                Some(payload.clone()),
            ),
        }
    }

}

impl FluorApp for BarcleanApp {
    type UserEvent = ();

    fn title(&self) -> &str {
        "barclean"
    }

    fn init(&mut self, ctx: &mut Context) {
        self.viewport = ctx.viewport;
    }

    fn on_resize(&mut self, width: u32, height: u32, _ctx: &mut Context) {
        self.viewport = Viewport::new(width, height);
    }

    fn on_event(&mut self, event: &FEvent, _ctx: &mut Context) -> EventResponse {
        // Selection happens on press rather than release: a lens change is cheap, reversible, and
        // the user is holding a phone one-handed at something. Waiting for a clean press-release on
        // the same target is the right rule for destructive actions, not for this.
        // Android's back button arrives as KEYCODE_ESCAPE via the shell, so leaving the result
        // screen with Back works without a separate hook — and matches the desktop key.
        if let FEvent::KeyboardInput { event: key, .. } = event {
            if matches!(key.state, fluor::event::ElementState::Pressed)
                && key.logical_key == fluor::event::Key::Named(fluor::event::NamedKey::Escape)
                && self.is_frozen()
            {
                self.resume_scanning();
                return EventResponse::Handled;
            }
        }

        if let FEvent::MouseInput { state, .. } = event {
            if matches!(state, fluor::event::ElementState::Pressed) {
                if let Screen::Result(view) = &self.screen {
                    let (x, y) = (_ctx.cursor_x, _ctx.cursor_y);
                    if self.result_buttons.save.is_some_and(|hit| hit.contains(x, y)) {
                        // Encode here, on the UI thread, because it is a few milliseconds for a
                        // symbol-sized image and the alternative is threading a result back.
                        match crate::render::to_png(
                            &view.rebuilt,
                            view.symbology,
                            12,
                            view.inverted,
                        ) {
                            Ok(png) => self.save_request = Some(png),
                            Err(e) => eprintln!("PNG encode failed: {e}"),
                        }
                        self.resume_scanning();
                        return EventResponse::Handled;
                    }
                    if self.result_buttons.cancel.is_some_and(|hit| hit.contains(x, y)) {
                        self.resume_scanning();
                        return EventResponse::Handled;
                    }
                    return EventResponse::Handled;
                }
                if let Some(id) = self.buttons.hit(_ctx.cursor_x, _ctx.cursor_y) {
                    if id != self.current_lens {
                        self.lens_request = Some(id.to_string());
                    }
                    self.pending_frame = true;
                    return EventResponse::Handled;
                }
            }
        }
        EventResponse::Pass
    }

    fn cursor_for(&self, _x: Coord, _y: Coord, _ctx: &Context) -> FCursorIcon {
        FCursorIcon::Default
    }

    /// Report a new camera frame as a reason to repaint.
    ///
    /// The host calls this once per Choreographer callback and marks the window dirty when it
    /// returns `true`. Returning `false` unconditionally — the trait default — leaves a camera app
    /// repainting only on touch, since input is otherwise the only thing that dirties the window.
    fn tick(&mut self, _ctx: &mut Context) -> bool {
        // The render thread owns app state, so this is where worker results are taken up. Doing it
        // here rather than in the camera callback is what removed the data race that produced
        // tearing.
        let had_frame = self.frame.is_some();
        self.collect();
        core::mem::take(&mut self.pending_frame) || (!had_frame && self.frame.is_some())
    }

    fn render(&mut self, target: &mut [u32], ctx: &mut Context) {
        let w = ctx.viewport.width_px as usize;
        let h = ctx.viewport.height_px as usize;

        // FRONT TO BACK. fluor paints the topmost layer first and stops at a pixel once it is
        // opaque, so this reads in the opposite order to a painter's-algorithm renderer: chrome,
        // then the preview beneath it, then the ground behind everything. Start from an empty
        // buffer — 0x00000000 is "nothing here yet", not black.
        for px in target.iter_mut().take(w * h) {
            *px = 0;
        }

        if let Screen::Result(view) = &self.screen {
            let (headline, payload) = (view.headline(), view.payload.clone());
            let (dimension, height) = (view.dimension, view.height);
            let verdicts = view.verdicts.clone();
            self.result_buttons =
                ui::draw_result(target, ctx, dimension, height, &verdicts, &headline, &payload);
            let ground = colour(10, 10, 12, 255);
            for px in target.iter_mut().take(w * h) {
                *px = (*px).under(ground, BlendMode::Normal);
            }
            return;
        }

        let (px_per_module, modules) = self.measured_px_per_module();
        let options = self
            .picker
            .options(&self.current_lens, px_per_module, modules);
        let suggestion = self.picker.suggestion(&options);

        let (accent, headline, detail) = self.status_parts();
        let bar_h = if options.is_empty() {
            0.0
        } else {
            (h as f32 * 0.10).max(ctx.viewport.effective_span() * 0.06)
        };

        self.buttons = ui::draw_lens_picker(target, ctx, &options, suggestion.as_deref());
        ui::draw_status(target, ctx, accent, &headline, detail.as_deref(), bar_h);

        self.blit_preview(target, w, h);

        // Ground last, behind everything, filling the letterbox bars.
        let ground = colour(10, 10, 12, 255);
        for px in target.iter_mut().take(w * h) {
            *px = (*px).under(ground, BlendMode::Normal);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> BarcleanApp {
        BarcleanApp::new(Arc::new(CameraFeed::new()))
    }

    fn lens(id: &str, focal: f32) -> LensSpec {
        LensSpec {
            id: id.into(),
            label: format!("{focal}mm"),
            focal_length_mm: focal,
            sensor_width_mm: 8.0,
            pixel_width: 1280,
            min_focus_distance_m: 0.1,
        }
    }

    #[test]
    fn re_enumerating_lenses_does_not_duplicate_buttons() {
        // The shim re-enumerates on every camera open, which happens on surfaceChanged AND on
        // onResume. Without deduplication every lens gained a second button after the first resume.
        let mut app = app();
        let three = vec![lens("3", 2.23), lens("2", 6.9), lens("4", 18.0)];

        app.set_lenses(three.clone(), "2");
        assert_eq!(app.lenses().len(), 3);

        let mut twice = three.clone();
        twice.extend(three);
        app.set_lenses(twice, "2");
        assert_eq!(app.lenses().len(), 3, "re-enumeration must not duplicate");
    }

    #[test]
    fn starts_with_no_frames_and_no_result() {
        let app = app();
        assert_eq!(*app.state(), DecodeState::NoFrames);
        assert_eq!(app.frames(), 0);
        assert!(!app.is_frozen());
    }

    #[test]
    fn collecting_a_decode_freezes_and_stops_the_worker() {
        use crate::Symbology;
        use crate::corpus::symbol;

        let spec = symbol::generate(Symbology::QrCode, "freeze me", "M").unwrap();
        let img = symbol::render(&spec, 6, 6);
        let (w, h) = (img.width() as usize, img.height() as usize);
        let luma: Vec<u8> = img.pixels().map(|p| p.0[0]).collect();

        let feed = Arc::new(CameraFeed::new());
        let worker = crate::feed::spawn_worker(Arc::clone(&feed));
        let mut app = BarcleanApp::new(Arc::clone(&feed));
        feed.submit(&luma, w, h, w, 0);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !app.is_frozen() {
            app.collect();
            assert!(std::time::Instant::now() < deadline, "never froze on a clean symbol");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        assert_eq!(app.state().payload(), Some("freeze me"));
        assert!(
            feed.is_frozen(),
            "the feed must stop decoding so a later frame cannot overwrite the result"
        );

        // Returning to the camera resumes decoding.
        app.resume_scanning();
        assert!(!app.is_frozen());
        assert!(!feed.is_frozen());

        feed.stop();
        worker.join().unwrap();
    }

    #[test]
    fn status_line_distinguishes_every_state() {
        let mut app = app();
        let mut seen: Vec<String> = Vec::new();
        for state in [
            DecodeState::NoFrames,
            DecodeState::Searching,
            DecodeState::Decoded("x".into()),
            DecodeState::Recovered { payload: "x".into(), rescued: 2, total: 8 },
            DecodeState::TooDamaged { decoded: 1, total: 8 },
            DecodeState::Reencoded { payload: "x".into(), format: "Aztec".into() },
        ] {
            app.state = state;
            let line = app.status_line();
            assert!(!line.is_empty());
            assert!(!seen.contains(&line), "two states share the status line {line:?}");
            seen.push(line);
        }
    }

    #[test]
    fn payload_is_exposed_for_every_state_that_has_one() {
        assert_eq!(DecodeState::Decoded("a".into()).payload(), Some("a"));
        assert_eq!(
            DecodeState::Recovered { payload: "b".into(), rescued: 1, total: 2 }.payload(),
            Some("b")
        );
        assert_eq!(
            DecodeState::Reencoded { payload: "c".into(), format: "Aztec".into() }.payload(),
            Some("c")
        );
        assert_eq!(DecodeState::Searching.payload(), None);
        assert_eq!(DecodeState::TooDamaged { decoded: 0, total: 1 }.payload(), None);
    }
}
