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

use crate::camera::{LensPicker, LensSpec, PickerParams};
use crate::render::{ModuleVerdict, Reconstructed};
use crate::ui::{self, LensButtons, ResultButtons, colour};

/// The most recent camera frame's luminance plane.
#[derive(Clone, Default)]
struct Frame {
    luma: Vec<u8>,
    width: usize,
    height: usize,
    /// Clockwise rotation needed to bring the sensor's output upright on this display, from
    /// `SENSOR_ORIENTATION`. Phone sensors are mounted landscape, so this is 90 on essentially
    /// every device held in portrait.
    rotation: u32,
}

impl Frame {
    fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0 || self.luma.is_empty()
    }

    /// Dimensions after rotation — swapped on the quarter turns.
    fn rotated_dims(&self) -> (usize, usize) {
        match self.rotation {
            90 | 270 => (self.height, self.width),
            _ => (self.width, self.height),
        }
    }

    /// Sample the upright image at `(x, y)`, mapping back through the rotation.
    ///
    /// Rotating at sample time rather than rotating the buffer keeps the decoder working on the
    /// sensor's own pixels: a rotation would either cost a full copy every frame or resample and
    /// blur module edges, and module edges are the entire signal here.
    fn sample_upright(&self, x: usize, y: usize) -> u8 {
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
    /// Read by the stock decoder in a format barclean does not clean yet.
    ///
    /// Aztec, PDF417 and DataMatrix currently fall here: they share the erasure-aware Reed-Solomon
    /// layer but not the QR-specific bootstrap path. Reporting them honestly beats showing nothing,
    /// and it separates "detection works, cleaning is not wired" from "nothing works at all" —
    /// which is otherwise indistinguishable from behind the viewfinder.
    Uncleaned { payload: String, format: String },
}

impl DecodeState {
    pub fn payload(&self) -> Option<&str> {
        match self {
            DecodeState::Decoded(p)
            | DecodeState::Recovered { payload: p, .. }
            | DecodeState::Uncleaned { payload: p, .. } => Some(p),
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
    pub rebuilt: Reconstructed,
    pub verdicts: Vec<ModuleVerdict>,
    pub dimension: usize,
    pub recovered: usize,
    pub blocks_rescued: usize,
    pub blocks_total: usize,
}

impl ResultView {
    fn headline(&self) -> String {
        if self.blocks_rescued > 0 {
            format!(
                "Recovered — {} of {} blocks rescued, {} modules repaired",
                self.blocks_rescued, self.blocks_total, self.recovered
            )
        } else if self.recovered > 0 {
            format!("Cleaned — {} modules repaired", self.recovered)
        } else {
            "Clean — nothing needed repairing".to_string()
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
    frame: Frame,
    state: DecodeState,
    /// Frames received since launch. The first thing to check when the screen is blank: if this is
    /// not climbing, the problem is the camera pipeline, not the renderer.
    frames: u64,
    /// A frame has arrived that has not been drawn yet.
    ///
    /// fluor only repaints when the window is marked dirty, and the default `tick` returns `false`,
    /// so nothing ever marks it. Input does — which is why, without this, the preview advances one
    /// frame per touch and looks frozen otherwise. A camera app is the case where new content
    /// arrives with no user input at all, so it has to raise its own hand.
    pending_frame: bool,
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

impl Default for BarcleanApp {
    fn default() -> Self {
        Self::new()
    }
}

impl BarcleanApp {
    pub fn new() -> Self {
        Self {
            frame: Frame::default(),
            state: DecodeState::NoFrames,
            frames: 0,
            pending_frame: false,
            last_decode_ms: 0,
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
        self.frames
    }

    /// One-line diagnostic for logcat and, shortly, the on-screen status text.
    pub fn status_line(&self) -> String {
        match &self.state {
            DecodeState::NoFrames => "no frames".to_string(),
            DecodeState::Searching => format!(
                "searching  {}x{}  rot{}",
                self.frame.width, self.frame.height, self.frame.rotation
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
            DecodeState::Uncleaned { payload, format } => {
                format!("{format} (uncleaned): {payload}")
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

    /// Accept one camera frame's luminance plane.
    ///
    /// `row_stride` is not necessarily `width` — Camera2 pads rows to a hardware alignment, and
    /// treating stride as width shears the image progressively down the frame. The rows are
    /// compacted here so everything downstream can assume a tight buffer.
    pub fn on_camera_frame(
        &mut self,
        luma: &[u8],
        width: usize,
        height: usize,
        row_stride: usize,
        rotation: u32,
    ) {
        if width == 0 || height == 0 {
            return;
        }
        let stride = if row_stride >= width { row_stride } else { width };

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

        self.frame = Frame {
            luma: tight,
            width,
            height,
            rotation: rotation % 360,
        };
        self.frames += 1;
        self.pending_frame = true;

        let started = std::time::Instant::now();
        self.decode();
        self.last_decode_ms = started.elapsed().as_millis() as u32;
    }

    /// Run the full cleaning path on the current frame.
    ///
    /// This is barclean's actual pipeline, not a stock decode: detection, provenance-recording
    /// codeword extraction, then the bootstrap loop. On an undamaged symbol it costs one extra
    /// Reed-Solomon pass over a plain decode, and on a damaged one it is the difference between a
    /// payload and nothing.
    ///
    /// Note it runs on the *unrotated* sensor buffer. The detector finds symbols at any orientation
    /// — that is what the perspective transform is for — so rotating first would cost a full copy
    /// per frame and resample module edges, which are the entire signal.
    fn decode(&mut self) {
        // Frozen on a result: frames keep arriving so the preview is warm when the user returns,
        // but nothing is allowed to overwrite the recovery they are looking at.
        if matches!(self.screen, Screen::Result(_)) {
            return;
        }
        if self.frame.is_empty() {
            self.state = DecodeState::NoFrames;
            return;
        }
        self.state = match crate::clean::clean_luma(
            &self.frame.luma,
            self.frame.width as u32,
            self.frame.height as u32,
        ) {
            Ok(c) => {
                self.last_symbol = Some((c.dimension, c.px_per_module));
                let (rescued, total) = (c.blocks_rescued(), c.blocks_total);
                let payload = c.payload.clone();
                        // Freeze on any successful decode, not only a rescued one: a logo-branded code that
                // reads fine still carries a logo the user wants gone, and the pristine rebuild is
                // the thing they came for either way.
                self.freeze(c);
                if rescued > 0 {
                    DecodeState::Recovered {
                        payload,
                        rescued,
                        total,
                    }
                } else {
                    DecodeState::Decoded(payload)
                }
            }
            Err(crate::clean::CleanError::Unrecoverable {
                blocks_total,
                blocks_decoded,
            }) => DecodeState::TooDamaged {
                decoded: blocks_decoded,
                total: blocks_total,
            },
            Err(_) => self.stock_fallback(),
        };
    }

    /// Freeze a successful clean into the result screen.
    fn freeze(&mut self, cleaned: crate::clean::Cleaned) {
        let Ok(rebuilt) = cleaned.reconstruct() else {
            return;
        };
        let Some(verdicts) = crate::render::compare(&rebuilt, &cleaned.sampled) else {
            return;
        };
        let recovered = verdicts.iter().filter(|v| v.recovered()).count();
        self.screen = Screen::Result(Box::new(ResultView {
            inverted: cleaned.source_inverted,
            payload: cleaned.payload,
            dimension: rebuilt.dimension,
            rebuilt,
            verdicts,
            recovered,
            blocks_rescued: cleaned.blocks_total - cleaned.blocks_decoded_initially,
            blocks_total: cleaned.blocks_total,
        }));
        self.pending_frame = true;
    }

    /// Return to the camera, discarding the frozen result.
    fn resume_scanning(&mut self) {
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

    /// Stock multi-format decode, tried when the QR cleaner finds nothing.
    ///
    /// Two jobs. It makes the app read the three symbologies whose cleaning is not wired yet, and
    /// it is the diagnostic that separates a detection failure from a cleaning failure — if this
    /// succeeds where the cleaner did not, the camera and the imaging path are fine and the problem
    /// is downstream.
    fn stock_fallback(&self) -> DecodeState {
        match rxing::helpers::detect_in_luma(
            self.frame.luma.clone(),
            self.frame.width as u32,
            self.frame.height as u32,
            None,
        ) {
            Ok(r) => DecodeState::Uncleaned {
                payload: r.getText().to_string(),
                format: format!("{:?}", r.getBarcodeFormat()),
            },
            Err(_) => DecodeState::Searching,
        }
    }

    /// Blit the camera preview, letterboxed to preserve aspect ratio.
    ///
    /// Nearest-neighbour on purpose. This is a diagnostic view of what the decoder is actually
    /// being fed, and a smoothing filter would hide precisely the undersampling that makes symbols
    /// fail — a preview that looks better than the data is worse than useless here.
    fn blit_preview(&self, target: &mut [u32], w: usize, h: usize) {
        if self.frame.is_empty() || w == 0 || h == 0 {
            return;
        }
        let (fw, fh) = self.frame.rotated_dims();
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
                let v = self.frame.sample_upright(sx, sy);
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
                    self.frame.width, self.frame.height, self.last_decode_ms
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
            DecodeState::Uncleaned { payload, format } => (
                colour(150, 90, 200, 255),
                format!("{format} (cleaning not wired)"),
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
                            crate::Symbology::QrCode,
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
        core::mem::take(&mut self.pending_frame)
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
            let (dimension, verdicts) = (view.dimension, view.verdicts.clone());
            self.result_buttons =
                ui::draw_result(target, ctx, dimension, &verdicts, &headline, &payload);
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

    #[test]
    fn stride_padding_is_compacted_not_sheared() {
        // Camera2 pads rows; treating stride as width shears the image progressively downward.
        // Build a 4x3 frame inside an 8-wide stride and confirm the padding is dropped.
        let width = 4;
        let height = 3;
        let stride = 8;
        let mut padded = vec![0u8; stride * height];
        for y in 0..height {
            for x in 0..width {
                padded[y * stride + x] = (y * 10 + x) as u8;
            }
        }

        let mut app = BarcleanApp::new();
        app.on_camera_frame(&padded, width, height, stride, 0);

        assert_eq!(app.frame.width, width);
        assert_eq!(app.frame.height, height);
        assert_eq!(app.frame.luma.len(), width * height);
        for y in 0..height {
            for x in 0..width {
                assert_eq!(
                    app.frame.luma[y * width + x],
                    (y * 10 + x) as u8,
                    "row {y} column {x} came from the wrong place"
                );
            }
        }
    }

    #[test]
    fn short_or_empty_frames_are_ignored() {
        let mut app = BarcleanApp::new();

        app.on_camera_frame(&[], 0, 0, 0, 0);
        assert_eq!(app.frames(), 0);

        // Buffer shorter than the declared geometry must not panic or half-fill.
        app.on_camera_frame(&[1, 2, 3], 10, 10, 10, 0);
        assert_eq!(app.frames(), 0);
        assert_eq!(*app.state(), DecodeState::NoFrames);
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
        let mut app = BarcleanApp::new();
        let three = vec![lens("3", 2.23), lens("2", 6.9), lens("4", 18.0)];

        app.set_lenses(three.clone(), "2");
        assert_eq!(app.lenses().len(), 3);

        let mut twice = three.clone();
        twice.extend(three);
        app.set_lenses(twice, "2");
        assert_eq!(app.lenses().len(), 3, "re-enumeration must not duplicate");
    }

    #[test]
    fn a_frame_without_a_symbol_reports_searching() {
        let mut app = BarcleanApp::new();
        app.on_camera_frame(&vec![128u8; 64 * 64], 64, 64, 64, 0);
        assert_eq!(app.frames(), 1);
        assert_eq!(*app.state(), DecodeState::Searching);
    }

    #[test]
    fn a_rendered_symbol_decodes_through_the_camera_entry_point() {
        // End-to-end through the same call the JNI shim makes: render a symbol to a luma buffer and
        // confirm the payload comes back. Guards the wiring between the camera path and the cleaner,
        // which no amount of algorithm testing would catch.
        use crate::Symbology;
        use crate::corpus::symbol;

        let payload = "barclean camera path";
        let spec = symbol::generate(Symbology::QrCode, payload, "M").unwrap();
        let img = symbol::render(&spec, 6, 6);
        let (w, h) = (img.width() as usize, img.height() as usize);
        let luma: Vec<u8> = img.pixels().map(|p| p.0[0]).collect();

        let mut app = BarcleanApp::new();
        app.on_camera_frame(&luma, w, h, w, 0);

        assert_eq!(
            app.state().payload(),
            Some(payload),
            "camera entry point did not decode a clean rendered symbol, state was {:?}",
            app.state()
        );
    }

    #[test]
    fn preview_blit_stays_in_bounds_for_any_aspect() {
        // Letterboxing must never write outside the target, whichever way the aspect mismatch runs.
        for (fw, fh) in [(64usize, 16usize), (16, 64), (33, 33)] {
            let mut app = BarcleanApp::new();
            app.on_camera_frame(&vec![200u8; fw * fh], fw, fh, fw, 0);

            let (w, h) = (40usize, 30usize);
            let mut target = vec![0u32; w * h];
            app.blit_preview(&mut target, w, h);
            assert!(
                target.iter().any(|&p| p != 0),
                "{fw}x{fh} frame produced no output at all"
            );
        }
    }
}
