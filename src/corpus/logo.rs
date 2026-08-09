//! Compositing occlusions over a rendered symbol.
//!
//! The corpus is only as honest as its occlusions. A flat grey square is the
//! easy case and would flatter the detector: it is uniform, so intra-cell
//! variance stays low and only the chroma and margin signals fire. Real logos
//! carry internal structure, which is the signal barclean leans on hardest, so
//! the generator has to produce content that genuinely varies inside a single
//! module cell.
//!
//! Deliberately included is the adversarial case: a **neutral, flat, high
//! contrast** occlusion, which defeats chroma and flatness both and leaves only
//! the function-pattern contradiction and the RS syndrome to notice anything is
//! wrong. Any honest grading run needs a case the confidence heuristic is
//! expected to struggle with, or the numbers only describe the easy half.

use image::{Rgb, RgbImage};

/// What kind of thing is sitting on the symbol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogoKind {
    /// Flat saturated colour. Chroma and flatness both fire — the easy case,
    /// and also the most common one in the wild.
    FlatColour,
    /// Deterministic pseudo-photographic texture: structure at the sub-module
    /// scale, which is the variance signal's whole reason for existing.
    Textured,
    /// Flat, neutral, high contrast. The adversarial case: it looks exactly like
    /// legitimate module content to every per-pixel statistic, and can only be
    /// caught structurally.
    NeutralFlat,
}

/// An occlusion to composite.
#[derive(Clone, Copy, Debug)]
pub struct Logo {
    pub kind: LogoKind,
    /// Share of total symbol area to cover, `0.0..1.0`. Applied as a centred
    /// square, which is where logos actually go.
    pub area_fraction: f32,
}

impl Logo {
    pub fn new(kind: LogoKind, area_fraction: f32) -> Self {
        Self {
            kind,
            area_fraction,
        }
    }
}

/// Paint the occlusion over the centre of `img`, in place.
///
/// Returns the covered rectangle in pixels, `(x0, y0, w, h)`, so the grader can
/// report occlusion area without re-deriving it.
pub fn composite(img: &mut RgbImage, logo: &Logo) -> (u32, u32, u32, u32) {
    let (w, h) = (img.width(), img.height());
    let side = ((w as f32 * h as f32 * logo.area_fraction.clamp(0.0, 1.0)).sqrt()) as u32;
    let side = side.min(w).min(h);
    if side == 0 {
        return (0, 0, 0, 0);
    }
    let x0 = (w - side) / 2;
    let y0 = (h - side) / 2;

    for y in y0..y0 + side {
        for x in x0..x0 + side {
            img.put_pixel(x, y, pixel_for(logo.kind, x, y));
        }
    }
    (x0, y0, side, side)
}

fn pixel_for(kind: LogoKind, x: u32, y: u32) -> Rgb<u8> {
    match kind {
        LogoKind::FlatColour => Rgb([201, 42, 58]),
        LogoKind::NeutralFlat => Rgb([28, 28, 28]),
        LogoKind::Textured => {
            // A cheap deterministic hash gives structure at the pixel scale
            // without a dependency or a seeded RNG, and stays reproducible
            // across runs so grading results are comparable.
            let h = hash2(x, y);
            let base = 60 + (h & 0x7F) as u8;
            let r = base.saturating_add(((h >> 8) & 0x3F) as u8);
            let g = base.saturating_sub(((h >> 16) & 0x3F) as u8);
            let b = base.saturating_add(((h >> 24) & 0x1F) as u8);
            Rgb([r, g, b])
        }
    }
}

/// Deterministic 2D integer hash. Not cryptographic; just well-mixed enough that
/// neighbouring pixels are uncorrelated, which is what makes the texture read as
/// photographic detail to a variance measurement.
fn hash2(x: u32, y: u32) -> u32 {
    let mut h = x.wrapping_mul(0x9E37_79B9) ^ y.wrapping_mul(0x85EB_CA6B);
    h ^= h >> 15;
    h = h.wrapping_mul(0xC2B2_AE35);
    h ^= h >> 13;
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    fn white(w: u32, h: u32) -> RgbImage {
        RgbImage::from_pixel(w, h, Rgb([255, 255, 255]))
    }

    #[test]
    fn covers_roughly_the_requested_area() {
        let mut img = white(200, 200);
        let (_, _, w, h) = composite(&mut img, &Logo::new(LogoKind::FlatColour, 0.25));
        let covered = (w * h) as f32 / (200.0 * 200.0);
        assert!(
            (covered - 0.25).abs() < 0.01,
            "covered {covered}, expected about 0.25"
        );
    }

    #[test]
    fn occlusion_is_centred() {
        let mut img = white(200, 200);
        let (x0, y0, w, h) = composite(&mut img, &Logo::new(LogoKind::FlatColour, 0.16));
        assert_eq!(x0 + w / 2, 100);
        assert_eq!(y0 + h / 2, 100);
        // Corners untouched.
        assert_eq!(*img.get_pixel(0, 0), Rgb([255, 255, 255]));
        assert_eq!(*img.get_pixel(199, 199), Rgb([255, 255, 255]));
    }

    #[test]
    fn textured_logo_actually_varies_within_a_module() {
        // The point of the Textured kind. Sample a 6x6 patch — roughly one
        // module at a typical render scale — and confirm it is not flat, or the
        // variance signal has nothing to detect.
        let mut img = white(200, 200);
        composite(&mut img, &Logo::new(LogoKind::Textured, 0.25));

        let mut seen = std::collections::HashSet::new();
        for y in 100..106 {
            for x in 100..106 {
                seen.insert(img.get_pixel(x, y).0);
            }
        }
        assert!(
            seen.len() > 20,
            "a 6x6 patch held only {} distinct colours; too flat to be photographic",
            seen.len()
        );
    }

    #[test]
    fn flat_kinds_are_actually_flat() {
        for kind in [LogoKind::FlatColour, LogoKind::NeutralFlat] {
            let mut img = white(200, 200);
            composite(&mut img, &Logo::new(kind, 0.25));
            let sample = *img.get_pixel(100, 100);
            for y in 100..106 {
                for x in 100..106 {
                    assert_eq!(*img.get_pixel(x, y), sample, "{kind:?} must be uniform");
                }
            }
        }
    }

    #[test]
    fn neutral_flat_is_the_adversarial_case() {
        // It must be genuinely neutral, or the chroma signal would catch it and
        // it would not be testing what it claims to test.
        let mut img = white(200, 200);
        composite(&mut img, &Logo::new(LogoKind::NeutralFlat, 0.25));
        let Rgb([r, g, b]) = *img.get_pixel(100, 100);
        assert_eq!(r, g);
        assert_eq!(g, b);
    }

    #[test]
    fn texture_is_reproducible_across_runs() {
        let mut a = white(100, 100);
        let mut b = white(100, 100);
        composite(&mut a, &Logo::new(LogoKind::Textured, 0.5));
        composite(&mut b, &Logo::new(LogoKind::Textured, 0.5));
        assert_eq!(a, b, "grading runs must be comparable");
    }

    #[test]
    fn zero_and_full_area_are_handled() {
        let mut img = white(100, 100);
        assert_eq!(composite(&mut img, &Logo::new(LogoKind::FlatColour, 0.0)).2, 0);
        assert_eq!(*img.get_pixel(50, 50), Rgb([255, 255, 255]));

        let (_, _, w, _) = composite(&mut img, &Logo::new(LogoKind::FlatColour, 1.0));
        assert_eq!(w, 100, "clamped to the image, not past it");
    }
}
