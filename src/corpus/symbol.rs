//! Encoding pristine symbols and keeping their ground truth.

use crate::Symbology;
use anyhow::{Context, Result, anyhow};
use image::{Rgb, RgbImage};
use rxing::qrcode::common::ErrorCorrectionLevel;

/// The pristine module matrix, before anything is done to it.
///
/// Ground truth for reconstruction identity. `true` is a dark module.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TruthMatrix {
    pub width: usize,
    pub height: usize,
    modules: Vec<bool>,
}

impl TruthMatrix {
    pub fn new(width: usize, height: usize, modules: Vec<bool>) -> Self {
        assert_eq!(modules.len(), width * height);
        Self {
            width,
            height,
            modules,
        }
    }

    pub fn get(&self, x: usize, y: usize) -> bool {
        self.modules[y * self.width + x]
    }

    pub fn modules(&self) -> &[bool] {
        &self.modules
    }

    /// Modules differing between two matrices of the same size.
    ///
    /// The reconstruction metric. Zero means bit-exact — the only acceptable
    /// result for the exact-reconstruction mode.
    pub fn differences(&self, other: &TruthMatrix) -> Option<usize> {
        if self.width != other.width || self.height != other.height {
            return None;
        }
        Some(
            self.modules
                .iter()
                .zip(&other.modules)
                .filter(|(a, b)| a != b)
                .count(),
        )
    }
}

/// A generated symbol plus everything needed to judge a reconstruction of it.
#[derive(Clone, Debug)]
pub struct Specimen {
    pub symbology: Symbology,
    pub payload: String,
    pub truth: TruthMatrix,
    /// Symbol version / size designator, where the symbology has one.
    pub version: u32,
    /// Data-mask pattern, where the symbology has one. `-1` if not applicable.
    pub mask: i32,
    /// Error-correction level as written by the encoder (`"L"`, `"M"`, `"Q"`,
    /// `"H"` for QR).
    pub ec_level: String,
}

impl Specimen {
    /// Modules across the symbol, excluding any quiet zone.
    pub fn modules_across(&self) -> usize {
        self.truth.width
    }
}

/// Encode a payload into a pristine symbol.
///
/// Only QR is wired so far — it is the format the erasure path is being brought
/// up against first, and the one where mask and version are both recoverable
/// from the symbol itself. The other three follow the same shape.
pub fn generate(symbology: Symbology, payload: &str, ec_level: &str) -> Result<Specimen> {
    match symbology {
        Symbology::QrCode => generate_qr(payload, ec_level),
        other => Err(anyhow!(
            "{} generation not yet wired (phase 3)",
            other.name()
        )),
    }
}

fn generate_qr(payload: &str, ec_level: &str) -> Result<Specimen> {
    let ec = match ec_level {
        "L" => ErrorCorrectionLevel::L,
        "M" => ErrorCorrectionLevel::M,
        "Q" => ErrorCorrectionLevel::Q,
        "H" => ErrorCorrectionLevel::H,
        other => return Err(anyhow!("unknown QR error-correction level {other:?}")),
    };

    let code = rxing::qrcode::encoder::qrcode_encoder::encode(payload, ec)
        .map_err(|e| anyhow!("QR encode failed: {e}"))?;

    let matrix = code
        .getMatrix()
        .as_ref()
        .context("encoder produced no matrix")?;

    let width = matrix.getWidth() as usize;
    let height = matrix.getHeight() as usize;
    let mut modules = Vec::with_capacity(width * height);
    for y in 0..height {
        for x in 0..width {
            // ByteMatrix carries 1 for a dark module, 0 for light.
            modules.push(matrix.get(x as u32, y as u32) == 1);
        }
    }

    let version = code
        .getVersion()
        .map(|v| v.getVersionNumber())
        .unwrap_or(0);

    Ok(Specimen {
        symbology: Symbology::QrCode,
        payload: payload.to_string(),
        truth: TruthMatrix::new(width, height, modules),
        version,
        mask: code.getMaskPattern(),
        ec_level: ec_level.to_string(),
    })
}

/// Render a specimen to an image.
///
/// `scale` is pixels per module and `quiet_zone` is measured in modules. The
/// quiet zone is not decoration — detectors need it to find the symbol at all,
/// and rendering without one produces a corpus that fails for reasons having
/// nothing to do with occlusion.
pub fn render(specimen: &Specimen, scale: u32, quiet_zone: u32) -> RgbImage {
    let modules_w = specimen.truth.width as u32;
    let modules_h = specimen.truth.height as u32;
    let px_w = (modules_w + 2 * quiet_zone) * scale;
    let px_h = (modules_h + 2 * quiet_zone) * scale;

    let mut img = RgbImage::from_pixel(px_w, px_h, Rgb([255, 255, 255]));
    for my in 0..modules_h {
        for mx in 0..modules_w {
            if !specimen.truth.get(mx as usize, my as usize) {
                continue;
            }
            let x0 = (mx + quiet_zone) * scale;
            let y0 = (my + quiet_zone) * scale;
            for y in y0..y0 + scale {
                for x in x0..x0 + scale {
                    img.put_pixel(x, y, Rgb([0, 0, 0]));
                }
            }
        }
    }
    img
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_a_qr_with_plausible_geometry() {
        let spec = generate(Symbology::QrCode, "barclean test payload", "M").unwrap();

        assert_eq!(spec.truth.width, spec.truth.height, "QR symbols are square");
        // Version v has 4v + 17 modules per side.
        assert_eq!(spec.truth.width, 4 * spec.version as usize + 17);
        assert!((0..=7).contains(&spec.mask), "mask pattern is 0..=7");
        assert_eq!(spec.ec_level, "M");
    }

    #[test]
    fn finder_patterns_are_where_the_spec_says() {
        let spec = generate(Symbology::QrCode, "finder check", "M").unwrap();
        let t = &spec.truth;
        let n = t.width;

        // Each finder is a 7x7 dark ring with a 3x3 dark core. Checking all
        // three corners also confirms the row-major order is not transposed,
        // which a square symbol would otherwise hide.
        for &(ox, oy) in &[(0, 0), (n - 7, 0), (0, n - 7)] {
            assert!(t.get(ox, oy), "finder outer corner at ({ox},{oy})");
            assert!(t.get(ox + 3, oy + 3), "finder core centre");
            assert!(!t.get(ox + 1, oy + 1), "finder inner light ring");
            assert!(!t.get(ox + 5, oy + 1), "finder inner light ring");
        }
        // The bottom-right corner has no finder.
        assert!(!t.get(n - 1, n - 1) || !t.get(n - 4, n - 4));
    }

    #[test]
    fn timing_patterns_alternate() {
        let spec = generate(Symbology::QrCode, "timing check", "M").unwrap();
        let t = &spec.truth;
        // Row 6 between the finders alternates dark/light, starting dark at x=8.
        for x in 8..t.width - 8 {
            assert_eq!(
                t.get(x, 6),
                x % 2 == 0,
                "horizontal timing module at x={x} broke the alternation"
            );
        }
        for y in 8..t.height - 8 {
            assert_eq!(t.get(6, y), y % 2 == 0, "vertical timing at y={y}");
        }
    }

    #[test]
    fn differences_counts_module_mismatches() {
        let a = generate(Symbology::QrCode, "same", "M").unwrap().truth;
        let b = generate(Symbology::QrCode, "same", "M").unwrap().truth;
        assert_eq!(a.differences(&b), Some(0), "encoding is deterministic");

        let mut modules = b.modules().to_vec();
        modules[100] = !modules[100];
        modules[200] = !modules[200];
        let c = TruthMatrix::new(b.width, b.height, modules);
        assert_eq!(a.differences(&c), Some(2));
    }

    #[test]
    fn differences_rejects_mismatched_sizes() {
        let a = generate(Symbology::QrCode, "short", "M").unwrap().truth;
        let b = generate(Symbology::QrCode, &"x".repeat(400), "M").unwrap().truth;
        assert_ne!(a.width, b.width);
        assert_eq!(a.differences(&b), None);
    }

    #[test]
    fn higher_ec_level_needs_a_bigger_symbol() {
        let payload = "the same payload at two correction levels";
        let l = generate(Symbology::QrCode, payload, "L").unwrap();
        let h = generate(Symbology::QrCode, payload, "H").unwrap();
        assert!(
            h.truth.width > l.truth.width,
            "H spends far more of the symbol on parity, so it needs more modules"
        );
    }

    #[test]
    fn render_geometry_and_quiet_zone() {
        let spec = generate(Symbology::QrCode, "render check", "M").unwrap();
        let img = render(&spec, 4, 4);

        let expect = (spec.truth.width as u32 + 8) * 4;
        assert_eq!(img.width(), expect);
        assert_eq!(img.height(), expect);

        // Quiet zone is white all the way round.
        assert_eq!(*img.get_pixel(0, 0), Rgb([255, 255, 255]));
        assert_eq!(*img.get_pixel(expect - 1, expect - 1), Rgb([255, 255, 255]));
        // The top-left finder's outermost module is dark.
        assert_eq!(*img.get_pixel(4 * 4, 4 * 4), Rgb([0, 0, 0]));
    }

    #[test]
    fn unwired_symbologies_report_rather_than_panic() {
        for s in [Symbology::Aztec, Symbology::Pdf417, Symbology::DataMatrix] {
            assert!(generate(s, "x", "M").is_err(), "{} should report", s.name());
        }
    }

    #[test]
    fn bad_ec_level_is_rejected() {
        assert!(generate(Symbology::QrCode, "x", "Z").is_err());
    }
}
