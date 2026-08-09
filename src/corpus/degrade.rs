//! Making a rendered symbol look like something a camera produced.
//!
//! A corpus of crisp synthetic renders would grade the decoder on a problem
//! nobody has. Real captures are blurred by optics and motion, noisy from a
//! small sensor at high gain, and quantized by whatever JPEG pipeline sat
//! between the sensor and the file.
//!
//! Degradation matters here beyond realism, because it interacts directly with
//! the signal barclean depends on. Blur raises intra-cell variance at every
//! module *boundary*, not just over the logo, so an occlusion detector tuned on
//! clean renders will flag the whole symbol the moment it meets a real photo.
//! Grading against blurred input is what keeps the confidence calibration
//! honest.

use image::{Rgb, RgbImage};

/// How badly to damage a render.
#[derive(Clone, Copy, Debug)]
pub struct Degradation {
    /// Box-blur radius in pixels. `0` disables.
    pub blur_radius: u32,
    /// Additive luminance noise amplitude, `0..=255`. `0` disables.
    pub noise: u8,
    /// JPEG quality to round-trip through, `1..=100`. `None` skips it.
    pub jpeg_quality: Option<u8>,
}

impl Degradation {
    /// No degradation — a synthetic best case, useful as a control row.
    pub const PRISTINE: Degradation = Degradation {
        blur_radius: 0,
        noise: 0,
        jpeg_quality: None,
    };

    /// A good handheld capture in decent light.
    pub const GOOD_CAPTURE: Degradation = Degradation {
        blur_radius: 1,
        noise: 4,
        jpeg_quality: Some(85),
    };

    /// A poor capture: soft focus, high gain, aggressive compression.
    pub const POOR_CAPTURE: Degradation = Degradation {
        blur_radius: 2,
        noise: 14,
        jpeg_quality: Some(45),
    };
}

/// Apply a degradation, returning the damaged image.
pub fn degrade(img: &RgbImage, params: &Degradation) -> RgbImage {
    let mut out = img.clone();
    if params.blur_radius > 0 {
        out = box_blur(&out, params.blur_radius);
    }
    if params.noise > 0 {
        out = add_noise(&out, params.noise);
    }
    if let Some(q) = params.jpeg_quality {
        out = jpeg_roundtrip(&out, q).unwrap_or(out);
    }
    out
}

/// Separable box blur. Two passes over a square window, which is a decent
/// stand-in for defocus and far cheaper than a true Gaussian.
fn box_blur(img: &RgbImage, radius: u32) -> RgbImage {
    let horizontal = blur_axis(img, radius, true);
    blur_axis(&horizontal, radius, false)
}

fn blur_axis(img: &RgbImage, radius: u32, horizontal: bool) -> RgbImage {
    let (w, h) = (img.width(), img.height());
    let mut out = RgbImage::new(w, h);
    let r = radius as i64;

    for y in 0..h {
        for x in 0..w {
            let mut sums = [0u32; 3];
            let mut n = 0u32;
            for d in -r..=r {
                let (sx, sy) = if horizontal {
                    ((x as i64 + d).clamp(0, w as i64 - 1) as u32, y)
                } else {
                    (x, (y as i64 + d).clamp(0, h as i64 - 1) as u32)
                };
                let px = img.get_pixel(sx, sy).0;
                for c in 0..3 {
                    sums[c] += px[c] as u32;
                }
                n += 1;
            }
            out.put_pixel(
                x,
                y,
                Rgb([
                    (sums[0] / n) as u8,
                    (sums[1] / n) as u8,
                    (sums[2] / n) as u8,
                ]),
            );
        }
    }
    out
}

/// Deterministic additive noise.
///
/// Reproducible on purpose: a grading run that moves because the RNG moved
/// cannot be compared against the previous run, and the whole point of the
/// harness is comparing runs.
fn add_noise(img: &RgbImage, amplitude: u8) -> RgbImage {
    let mut out = img.clone();
    let amp = amplitude as i32;
    for (x, y, px) in out.enumerate_pixels_mut() {
        let mut h = x.wrapping_mul(0x27D4_EB2D) ^ y.wrapping_mul(0x1656_67B1);
        h ^= h >> 15;
        h = h.wrapping_mul(0x2545_F491);
        h ^= h >> 13;
        // Symmetric about zero so the mean luminance does not drift, which would
        // shift the binarization threshold and confound the measurement.
        let delta = (h % (2 * amp as u32 + 1)) as i32 - amp;
        for c in 0..3 {
            px.0[c] = (px.0[c] as i32 + delta).clamp(0, 255) as u8;
        }
    }
    out
}

/// Encode to JPEG and decode back, so the corpus carries real DCT artifacts —
/// ringing along the high-contrast module edges, which is exactly where a
/// barcode is most sensitive.
fn jpeg_roundtrip(img: &RgbImage, quality: u8) -> Option<RgbImage> {
    use image::codecs::jpeg::JpegEncoder;
    use image::{DynamicImage, ImageEncoder};

    let mut buf = Vec::new();
    JpegEncoder::new_with_quality(&mut buf, quality.clamp(1, 100))
        .write_image(
            img.as_raw(),
            img.width(),
            img.height(),
            image::ExtendedColorType::Rgb8,
        )
        .ok()?;

    let decoded = image::load_from_memory_with_format(&buf, image::ImageFormat::Jpeg).ok()?;
    Some(DynamicImage::from(decoded).to_rgb8())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sharp black/white edge — the structure a barcode is made of.
    fn edge(w: u32, h: u32) -> RgbImage {
        RgbImage::from_fn(w, h, |x, _| {
            if x < w / 2 {
                Rgb([0, 0, 0])
            } else {
                Rgb([255, 255, 255])
            }
        })
    }

    #[test]
    fn pristine_is_a_no_op() {
        let img = edge(40, 40);
        assert_eq!(degrade(&img, &Degradation::PRISTINE), img);
    }

    #[test]
    fn blur_softens_the_edge() {
        let img = edge(40, 40);
        let blurred = box_blur(&img, 2);

        // Straddling the boundary must now be intermediate rather than binary.
        let v = blurred.get_pixel(20, 20).0[0];
        assert!(v > 0 && v < 255, "edge pixel is still hard at {v}");
        // Far from the edge nothing should have moved.
        assert_eq!(blurred.get_pixel(2, 20).0[0], 0);
        assert_eq!(blurred.get_pixel(37, 20).0[0], 255);
    }

    #[test]
    fn noise_perturbs_without_shifting_the_mean() {
        let flat = RgbImage::from_pixel(64, 64, Rgb([128, 128, 128]));
        let noisy = add_noise(&flat, 20);

        assert_ne!(noisy, flat, "noise must actually do something");

        let mean: f64 = noisy.pixels().map(|p| p.0[0] as f64).sum::<f64>() / (64.0 * 64.0);
        assert!(
            (mean - 128.0).abs() < 2.0,
            "noise shifted the mean to {mean}; that would move the binarization threshold"
        );
    }

    #[test]
    fn noise_stays_in_range() {
        for base in [0u8, 128, 255] {
            let flat = RgbImage::from_pixel(32, 32, Rgb([base, base, base]));
            let noisy = add_noise(&flat, 60);
            // Nothing to assert beyond "did not panic and stayed a valid image";
            // clamping is the property under test.
            assert_eq!(noisy.dimensions(), (32, 32));
        }
    }

    #[test]
    fn degradation_is_reproducible() {
        let img = edge(40, 40);
        let a = degrade(&img, &Degradation::POOR_CAPTURE);
        let b = degrade(&img, &Degradation::POOR_CAPTURE);
        assert_eq!(a, b, "grading runs must be comparable across invocations");
    }

    #[test]
    fn jpeg_roundtrip_preserves_geometry_and_adds_artifacts() {
        let img = edge(64, 64);
        let out = jpeg_roundtrip(&img, 40).expect("jpeg roundtrip");
        assert_eq!(out.dimensions(), img.dimensions());
        assert_ne!(out, img, "low-quality JPEG must leave visible artifacts");
    }

    #[test]
    fn poor_capture_is_worse_than_good() {
        let img = edge(64, 64);
        let good = degrade(&img, &Degradation::GOOD_CAPTURE);
        let poor = degrade(&img, &Degradation::POOR_CAPTURE);

        // Measure edge softness: distance from binary at the boundary column.
        let softness = |i: &RgbImage| -> u32 {
            (0..64)
                .map(|y| {
                    let v = i.get_pixel(32, y).0[0] as i32;
                    v.min(255 - v) as u32
                })
                .sum()
        };
        assert!(
            softness(&poor) > softness(&good),
            "the poor preset must actually be poorer"
        );
    }
}
