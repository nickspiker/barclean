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
use fluor::paint::pack_argb;
use fluor::Viewport;

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

/// Pack a colour for the current host's framebuffer byte order.
///
/// fluor's `pack_argb` always lays out ARGB — red in bits 16-23 — and its finalize pass emits that
/// order unconditionally, with no platform branch. That matches desktop softbuffer. Android's
/// surface is `RGBA_8888`, which as a little-endian `u32` puts **red in the low byte**, so red and
/// blue arrive swapped.
///
/// The bug hides in exactly the place you would test first: greyscale has `r == g == b`, so a
/// preview looks perfect while every authored colour comes out as its channel-swapped twin (amber
/// renders cyan). Anything with an opinion about colour has to go through here.
#[inline]
fn colour(r: u8, g: u8, b: u8, a: u8) -> u32 {
    #[cfg(target_os = "android")]
    {
        pack_argb(b, g, r, a)
    }
    #[cfg(not(target_os = "android"))]
    {
        pack_argb(r, g, b, a)
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
}

impl DecodeState {
    pub fn payload(&self) -> Option<&str> {
        match self {
            DecodeState::Decoded(p) | DecodeState::Recovered { payload: p, .. } => Some(p),
            _ => None,
        }
    }
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
            viewport: Viewport::new(1, 1),
        }
    }

    pub fn state(&self) -> &DecodeState {
        &self.state
    }

    pub fn frames(&self) -> u64 {
        self.frames
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
        self.decode();
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
        if self.frame.is_empty() {
            self.state = DecodeState::NoFrames;
            return;
        }
        self.state = match crate::clean::clean_luma(
            &self.frame.luma,
            self.frame.width as u32,
            self.frame.height as u32,
        ) {
            Ok(c) if c.needed_barclean() => {
                let (rescued, total) = (c.blocks_rescued(), c.blocks_total);
                DecodeState::Recovered {
                    payload: c.payload,
                    rescued,
                    total,
                }
            }
            Ok(c) => DecodeState::Decoded(c.payload),
            Err(crate::clean::CleanError::Unrecoverable {
                blocks_total,
                blocks_decoded,
            }) => DecodeState::TooDamaged {
                decoded: blocks_decoded,
                total: blocks_total,
            },
            Err(_) => DecodeState::Searching,
        };
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
                    target[idx] = colour(v, v, v, 255);
                }
            }
        }
    }

    /// A status band across the bottom, colour-coded by decode state.
    ///
    /// Colour rather than text for the moment: it is legible at a glance while pointing a phone at
    /// something, which is exactly the posture this gets used in.
    fn draw_status(&self, target: &mut [u32], w: usize, h: usize) {
        let colour = match &self.state {
            DecodeState::NoFrames => colour(180, 40, 40, 255),
            DecodeState::Searching => colour(200, 150, 30, 255),
            DecodeState::Decoded(_) => colour(40, 170, 80, 255),
            // Distinct from a plain decode on purpose: this is the case barclean exists for, and
            // it should be visible that the symbol needed rescuing rather than merely reading.
            DecodeState::Recovered { .. } => colour(80, 140, 230, 255),
            DecodeState::TooDamaged { .. } => colour(190, 90, 30, 255),
        };
        let band = (h / 12).max(8);
        let y0 = h.saturating_sub(band);
        for y in y0..h {
            for x in 0..w {
                let idx = y * w + x;
                if idx < target.len() {
                    target[idx] = colour;
                }
            }
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

    fn on_event(&mut self, _event: &FEvent, _ctx: &mut Context) -> EventResponse {
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

        // Opaque near-black ground. fluor stores α + *darkness* — the top byte is opacity, the low
        // three are the complement of visible RGB — but `pack_argb` takes ordinary RGB and does the
        // inversion, so this reads as it looks.
        let ground = colour(10, 10, 12, 255);
        for px in target.iter_mut().take(w * h) {
            *px = ground;
        }

        self.blit_preview(target, w, h);
        self.draw_status(target, w, h);
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
            app.draw_status(&mut target, w, h);
            assert!(
                target.iter().any(|&p| p != 0),
                "{fw}x{fh} frame produced no output at all"
            );
        }
    }
}
