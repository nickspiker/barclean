//! On-screen chrome: the status readout and the lens picker.
//!
//! Split from [`crate::app`] so the drawing stays separable from the decode state machine. Layout
//! is expressed as fractions of the viewport, which is fluor's whole premise — no pixel constants,
//! so the same code is right on a phone and on a desktop window.

use fluor::canvas::Canvas;
use fluor::host::app::Context;
use fluor::paint::pack_argb;
use fluor::pixel::{Blend, BlendMode};
use fluor::text::TextStyle;

use crate::camera::{LensOption, LensStatus};

/// Pack a colour for the current host's framebuffer byte order.
///
/// fluor's `pack_argb` lays out ARGB — red in bits 16-23 — and its finalize pass emits that order
/// unconditionally. Android's surface is `RGBA_8888`, which as a little-endian `u32` puts red in the
/// low byte, so red and blue arrive swapped. Greyscale hides the bug completely (`r == g == b`),
/// which is exactly why it survived until something coloured got drawn.
#[inline]
pub fn colour(r: u8, g: u8, b: u8, a: u8) -> u32 {
    #[cfg(target_os = "android")]
    {
        pack_argb(b, g, r, a)
    }
    #[cfg(not(target_os = "android"))]
    {
        pack_argb(r, g, b, a)
    }
}

/// A tappable region, in viewport pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hit {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

impl Hit {
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x0 && x < self.x1 && y >= self.y0 && y < self.y1
    }
}

/// Where the lens buttons landed this frame, so touches can be matched to them.
///
/// Recomputed every render rather than cached, because the layout is a pure function of the
/// viewport and the lens list — caching it would just be a way to get it out of sync.
#[derive(Clone, Debug, Default)]
pub struct LensButtons {
    pub hits: Vec<(String, Hit)>,
}

impl LensButtons {
    /// Which lens id, if any, a touch at `(x, y)` selects.
    pub fn hit(&self, x: f32, y: f32) -> Option<&str> {
        self.hits
            .iter()
            .find(|(_, h)| h.contains(x, y))
            .map(|(id, _)| id.as_str())
    }
}

/// Composite a rectangle **underneath** whatever is already in the buffer.
///
/// fluor renders front to back: the topmost layer lands first, and a pixel that is already opaque
/// is finished. So a panel background is painted *after* its own text and slides in behind it,
/// filling only the gaps between glyphs. Plain assignment here would erase them — which is exactly
/// what it did, and why the lens buttons first rendered as empty rectangles.
fn fill_under(target: &mut [u32], w: usize, h: usize, hit: Hit, c: u32) {
    let x0 = hit.x0.max(0.0) as usize;
    let y0 = hit.y0.max(0.0) as usize;
    let x1 = (hit.x1.max(0.0) as usize).min(w);
    let y1 = (hit.y1.max(0.0) as usize).min(h);
    for y in y0..y1 {
        for x in x0..x1 {
            let idx = y * w + x;
            if idx < target.len() {
                target[idx] = target[idx].under(c, BlendMode::Normal);
            }
        }
    }
}

/// Draw the lens picker across the bottom and report where the buttons went.
///
/// Every lens is shown, including ones that would be a poor choice right now — a picker that hides
/// options is deciding for the user, which is exactly what this is not for. Poor choices are
/// labelled instead, with the predicted pixels-per-module that makes the reason legible.
pub fn draw_lens_picker(
    target: &mut [u32],
    ctx: &mut Context,
    options: &[LensOption],
    suggestion: Option<&str>,
) -> LensButtons {
    let w = ctx.viewport.width_px as usize;
    let h = ctx.viewport.height_px as usize;
    let mut buttons = LensButtons::default();
    if options.is_empty() || w == 0 || h == 0 {
        return buttons;
    }

    let span = ctx.viewport.effective_span();
    let bar_h = (h as f32 * 0.10).max(span * 0.06);
    let y0 = h as f32 - bar_h;
    let slot_w = w as f32 / options.len() as f32;

    for (i, option) in options.iter().enumerate() {
        let bx0 = i as f32 * slot_w;
        let hit = Hit {
            x0: bx0,
            y0,
            x1: bx0 + slot_w,
            y1: h as f32,
        };

        let (bg, fg) = if option.is_current {
            (colour(70, 130, 200, 255), colour(255, 255, 255, 255))
        } else {
            match option.status {
                LensStatus::Good => (colour(30, 34, 40, 255), colour(210, 235, 210, 255)),
                LensStatus::Marginal => (colour(30, 34, 40, 255), colour(225, 205, 140, 255)),
                LensStatus::TooCoarse | LensStatus::WouldCrop | LensStatus::CannotFocus => {
                    (colour(30, 30, 34, 255), colour(150, 130, 130, 255))
                }
                LensStatus::Unknown => (colour(30, 30, 34, 255), colour(180, 180, 185, 255)),
            }
        };

        let cx = bx0 + slot_w * 0.5;
        let label_size = span * 0.020;
        let detail_size = span * 0.014;

        let mut canvas = Canvas::new(target, w, h, ctx.damage);
        let marker = if suggestion == Some(option.id.as_str()) {
            "\u{2605} "
        } else {
            ""
        };
        ctx.text.draw_text_center(
            &mut canvas,
            &format!("{marker}{}", option.label),
            cx,
            y0 + bar_h * 0.35,
            &TextStyle::new(label_size, fg).weight(600),
            None,
            None,
        );

        let detail = match option.status {
            LensStatus::WouldCrop => "won't fit".to_string(),
            LensStatus::CannotFocus => "can't focus".to_string(),
            LensStatus::Unknown => format!("{:.1}mm", option.focal_length_mm),
            _ => format!("{:.1} px/mod", option.predicted_px_per_module),
        };
        let mut canvas = Canvas::new(target, w, h, ctx.damage);
        ctx.text.draw_text_center(
            &mut canvas,
            &detail,
            cx,
            y0 + bar_h * 0.70,
            &TextStyle::new(detail_size, fg),
            None,
            None,
        );

        // Button fill goes in AFTER its labels, sliding in behind them. Inset by a hairline so
        // adjacent buttons read as separate targets.
        let inset = slot_w * 0.02;
        fill_under(
            target,
            w,
            h,
            Hit {
                x0: hit.x0 + inset,
                y0: hit.y0 + inset,
                x1: hit.x1 - inset,
                y1: hit.y1 - inset,
            },
            bg,
        );

        buttons.hits.push((option.id.clone(), hit));
    }

    // The bar itself lands behind every button, filling the inset gutters between them.
    fill_under(
        target,
        w,
        h,
        Hit {
            x0: 0.0,
            y0,
            x1: w as f32,
            y1: h as f32,
        },
        colour(18, 18, 22, 255),
    );

    buttons
}

/// Draw the status readout above the lens picker.
///
/// `accent` colours the band by decode state; `headline` is the one-line verdict and `detail` the
/// payload, truncated because a URL with a session token in it will happily run off any screen.
pub fn draw_status(
    target: &mut [u32],
    ctx: &mut Context,
    accent: u32,
    headline: &str,
    detail: Option<&str>,
    bottom_offset: f32,
) {
    let w = ctx.viewport.width_px as usize;
    let h = ctx.viewport.height_px as usize;
    if w == 0 || h == 0 {
        return;
    }

    let span = ctx.viewport.effective_span();
    let band_h = span * 0.085;
    let y1 = h as f32 - bottom_offset;
    let y0 = (y1 - band_h).max(0.0);

    let cx = w as f32 * 0.5;
    let mut canvas = Canvas::new(target, w, h, ctx.damage);
    ctx.text.draw_text_center(
        &mut canvas,
        headline,
        cx,
        y0 + band_h * 0.38,
        &TextStyle::new(span * 0.021, colour(240, 240, 245, 255)).weight(600),
        None,
        None,
    );

    if let Some(detail) = detail {
        let shown: String = if detail.chars().count() > 52 {
            detail.chars().take(49).collect::<String>() + "…"
        } else {
            detail.to_string()
        };
        let mut canvas = Canvas::new(target, w, h, ctx.damage);
        ctx.text.draw_text_center(
            &mut canvas,
            &shown,
            cx,
            y0 + band_h * 0.72,
            &TextStyle::new(span * 0.016, colour(185, 195, 205, 255)),
            None,
            None,
        );
    }

    // Accent stripe, then the band behind it — both after the text, front to back.
    fill_under(
        target,
        w,
        h,
        Hit {
            x0: 0.0,
            y0,
            x1: w as f32,
            y1: y0 + span * 0.006,
        },
        accent,
    );
    fill_under(
        target,
        w,
        h,
        Hit {
            x0: 0.0,
            y0,
            x1: w as f32,
            y1,
        },
        colour(14, 14, 18, 255),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hit_contains_is_half_open() {
        let h = Hit {
            x0: 10.0,
            y0: 20.0,
            x1: 30.0,
            y1: 40.0,
        };
        assert!(h.contains(10.0, 20.0), "inclusive at the top-left");
        assert!(h.contains(29.9, 39.9));
        assert!(!h.contains(30.0, 30.0), "exclusive at the right edge");
        assert!(!h.contains(20.0, 40.0), "exclusive at the bottom edge");
        assert!(!h.contains(9.0, 30.0));
    }

    #[test]
    fn buttons_resolve_touches_to_lens_ids() {
        let buttons = LensButtons {
            hits: vec![
                (
                    "ultrawide".into(),
                    Hit { x0: 0.0, y0: 900.0, x1: 100.0, y1: 1000.0 },
                ),
                (
                    "main".into(),
                    Hit { x0: 100.0, y0: 900.0, x1: 200.0, y1: 1000.0 },
                ),
            ],
        };

        assert_eq!(buttons.hit(50.0, 950.0), Some("ultrawide"));
        assert_eq!(buttons.hit(150.0, 950.0), Some("main"));
        assert_eq!(buttons.hit(150.0, 500.0), None, "above the bar selects nothing");
        assert_eq!(buttons.hit(500.0, 950.0), None, "past the last button");
    }

    #[test]
    fn empty_picker_reports_no_buttons() {
        let buttons = LensButtons::default();
        assert_eq!(buttons.hit(10.0, 10.0), None);
    }
}

/// Where the result screen's buttons landed.
#[derive(Clone, Copy, Debug, Default)]
pub struct ResultButtons {
    pub save: Option<Hit>,
    pub cancel: Option<Hit>,
}

/// Draw the frozen result: the rebuilt symbol as a four-colour comparison, plus Save and Cancel.
///
/// The grid is the evidence. Blue and green are modules the camera already had right; red and
/// yellow are ones barclean recovered. A logo's footprint shows up as a solid red-and-yellow patch,
/// so the claim "this was reconstructed" is something you can see rather than something you have to
/// take on faith.
pub fn draw_result(
    target: &mut [u32],
    ctx: &mut Context,
    dimension: usize,
    verdicts: &[crate::render::ModuleVerdict],
    headline: &str,
    payload: &str,
) -> ResultButtons {
    let w = ctx.viewport.width_px as usize;
    let h = ctx.viewport.height_px as usize;
    if w == 0 || h == 0 || dimension == 0 || verdicts.len() != dimension * dimension {
        return ResultButtons::default();
    }
    let span = ctx.viewport.effective_span();

    // Buttons across the bottom, then the grid above them, then the header text.
    let bar_h = (h as f32 * 0.11).max(span * 0.07);
    let bar_y = h as f32 - bar_h;
    let save = Hit { x0: 0.0, y0: bar_y, x1: w as f32 * 0.5, y1: h as f32 };
    let cancel = Hit { x0: w as f32 * 0.5, y0: bar_y, x1: w as f32, y1: h as f32 };

    for (hit, label, fill, ink) in [
        (save, "Save PNG", colour(40, 150, 80, 255), colour(255, 255, 255, 255)),
        (cancel, "Cancel", colour(45, 45, 52, 255), colour(225, 225, 230, 255)),
    ] {
        let mut canvas = Canvas::new(target, w, h, ctx.damage);
        ctx.text.draw_text_center(
            &mut canvas,
            label,
            (hit.x0 + hit.x1) * 0.5,
            bar_y + bar_h * 0.5,
            &TextStyle::new(span * 0.024, ink).weight(600),
            None,
            None,
        );
        let inset = span * 0.004;
        fill_under(
            target,
            w,
            h,
            Hit { x0: hit.x0 + inset, y0: hit.y0 + inset, x1: hit.x1 - inset, y1: hit.y1 - inset },
            fill,
        );
    }

    // Header.
    let head_y = span * 0.06;
    let mut canvas = Canvas::new(target, w, h, ctx.damage);
    ctx.text.draw_text_center(
        &mut canvas,
        headline,
        w as f32 * 0.5,
        head_y,
        &TextStyle::new(span * 0.026, colour(245, 245, 250, 255)).weight(600),
        None,
        None,
    );
    let shown: String = if payload.chars().count() > 46 {
        payload.chars().take(43).collect::<String>() + "…"
    } else {
        payload.to_string()
    };
    let mut canvas = Canvas::new(target, w, h, ctx.damage);
    ctx.text.draw_text_center(
        &mut canvas,
        &shown,
        w as f32 * 0.5,
        head_y + span * 0.035,
        &TextStyle::new(span * 0.017, colour(180, 190, 205, 255)),
        None,
        None,
    );

    // The grid, centred in the space between header and buttons. Integer module size so every
    // module is the same number of pixels — an uneven grid reads as a rendering fault in exactly
    // the kind of image whose whole purpose is to look precise.
    let top = head_y + span * 0.07;
    let avail_h = (bar_y - top).max(0.0);
    let avail_w = w as f32 * 0.92;
    let cell = (avail_w.min(avail_h) / dimension as f32).floor().max(1.0) as usize;
    let grid_px = cell * dimension;
    let gx = (w.saturating_sub(grid_px)) / 2;
    let gy = top as usize + ((avail_h as usize).saturating_sub(grid_px)) / 2;

    for my in 0..dimension {
        for mx in 0..dimension {
            let (r, g, b) = verdicts[my * dimension + mx].rgb();
            let c = colour(r, g, b, 255);
            fill_under(
                target,
                w,
                h,
                Hit {
                    x0: (gx + mx * cell) as f32,
                    y0: (gy + my * cell) as f32,
                    x1: (gx + (mx + 1) * cell) as f32,
                    y1: (gy + (my + 1) * cell) as f32,
                },
                c,
            );
        }
    }

    ResultButtons { save: Some(save), cancel: Some(cancel) }
}
