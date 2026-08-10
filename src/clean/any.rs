//! Cleaning any of the four supported symbologies.
//!
//! # Two grades of restoration, and why they differ
//!
//! **QR** is rebuilt from its corrected codewords — bit-identical to what the original encoder
//! emitted (see [`crate::render::exact`]). It also gets the bootstrap loop, because QR interleaves
//! its Reed-Solomon blocks: a contiguous occlusion is spread across every block, so surviving blocks
//! can localize the damage for the failed ones.
//!
//! **Aztec, DataMatrix and PDF417** are re-encoded from the recovered payload and verified. The
//! difference is not laziness, it is structural:
//!
//! - Aztec and PDF417 carry a **single** Reed-Solomon block. There are no survivors to bootstrap
//!   from — the symbol either decodes or it does not — so the loop has nothing to work with.
//! - Rebuilding them at codeword level needs each format's own placement algorithm written in
//!   reverse (Aztec's spiral, DataMatrix's ECC200 placement, PDF417's row indicators), which is
//!   real work per format and independent of the recovery itself.
//!
//! What all four *do* get is the thing this is for: photograph a damaged code — folded, smudged,
//! scuffed, printed badly — and get back a clean one. Ordinary Reed-Solomon already recovers the
//! data in those cases; the restoration is the deliverable.
//!
//! Every re-encode is verified by decoding it again and comparing payloads before it is offered for
//! saving. A restoration that does not scan, or scans to something else, is worse than no
//! restoration at all.

use crate::Symbology;
use crate::clean::CleanError;
use crate::render::Reconstructed;
use rxing::{BarcodeFormat, EncodeHints, MultiFormatWriter, Writer};

/// How faithful a restoration is to the original symbol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fidelity {
    /// Rebuilt from the corrected codewords: bit-identical to the original symbol.
    Exact,
    /// Re-encoded from the recovered payload and verified to decode identically. Same content,
    /// possibly different structure — an encoder's choice of segmentation, version and padding is
    /// its own.
    Reencoded,
}

impl Fidelity {
    pub fn label(self) -> &'static str {
        match self {
            Fidelity::Exact => "exact restoration",
            Fidelity::Reencoded => "re-encoded",
        }
    }
}

/// A cleaned symbol of any supported symbology.
pub struct CleanedAny {
    pub symbology: Symbology,
    pub payload: String,
    pub fidelity: Fidelity,
    pub rebuilt: Reconstructed,
    /// The symbol as sampled, when it can be aligned with the rebuild. Only meaningful for an
    /// exact restoration — a re-encode may not even be the same size, and a comparison against a
    /// differently-shaped symbol would be noise dressed up as evidence.
    pub sampled: Option<Vec<bool>>,
    /// Blocks the bootstrap loop rescued, for the formats that have more than one.
    pub blocks_rescued: usize,
    pub blocks_total: usize,
    pub source_inverted: bool,
    pub px_per_module: f32,
}

impl CleanedAny {
    pub fn needed_barclean(&self) -> bool {
        self.blocks_rescued > 0
    }
}

fn symbology_of(format: &BarcodeFormat) -> Option<Symbology> {
    match format {
        BarcodeFormat::QR_CODE => Some(Symbology::QrCode),
        BarcodeFormat::AZTEC => Some(Symbology::Aztec),
        BarcodeFormat::DATA_MATRIX => Some(Symbology::DataMatrix),
        BarcodeFormat::PDF_417 => Some(Symbology::Pdf417),
        _ => None,
    }
}

fn format_of(symbology: Symbology) -> BarcodeFormat {
    match symbology {
        Symbology::QrCode => BarcodeFormat::QR_CODE,
        Symbology::Aztec => BarcodeFormat::AZTEC,
        Symbology::DataMatrix => BarcodeFormat::DATA_MATRIX,
        Symbology::Pdf417 => BarcodeFormat::PDF_417,
    }
}

/// Detect, decode and restore a symbol of any supported symbology.
///
/// QR takes the full cleaning path. The rest decode conventionally and are re-encoded; see the
/// module documentation for why.
pub fn clean(luma: &[u8], width: u32, height: u32) -> Result<CleanedAny, CleanError> {
    // QR first, since it is the only one with a bit-exact path and the only one that benefits from
    // bootstrapping. If it is not a QR this simply fails and we fall through.
    if let Ok(qr) = crate::clean::clean_luma(luma, width, height) {
        let rebuilt = qr.reconstruct().map_err(|_| CleanError::Unreadable)?;
        return Ok(CleanedAny {
            symbology: Symbology::QrCode,
            payload: qr.payload,
            fidelity: Fidelity::Exact,
            sampled: Some(qr.sampled),
            rebuilt,
            blocks_rescued: qr.blocks_total - qr.blocks_decoded_initially,
            blocks_total: qr.blocks_total,
            source_inverted: qr.source_inverted,
            px_per_module: qr.px_per_module,
        });
    }

    // Everything else, upright then inverted — light-on-dark codes are common and the binarizer
    // marks the background as the dark modules, so they are simply invisible otherwise.
    let (result, inverted) = match rxing::helpers::detect_in_luma(luma.to_vec(), width, height, None)
    {
        Ok(r) => (r, false),
        Err(_) => {
            let flipped: Vec<u8> = luma.iter().map(|v| 255 - v).collect();
            match rxing::helpers::detect_in_luma(flipped, width, height, None) {
                Ok(r) => (r, true),
                Err(_) => return Err(CleanError::NotDetected),
            }
        }
    };

    let symbology = symbology_of(result.getBarcodeFormat()).ok_or(CleanError::NotDetected)?;
    let payload = result.getText().to_string();

    let rebuilt = reencode(&payload, symbology)?;

    Ok(CleanedAny {
        symbology,
        payload,
        fidelity: Fidelity::Reencoded,
        rebuilt,
        sampled: None,
        blocks_rescued: 0,
        blocks_total: 1,
        source_inverted: inverted,
        px_per_module: 0.0,
    })
}

/// Re-encode a payload and verify the result decodes back to it.
///
/// The verification is not ceremony. A restoration that does not scan is useless, and one that
/// scans to *something else* is actively dangerous — a payment or a URL silently altered is a far
/// worse outcome than a failed clean. Nothing leaves here unverified.
fn reencode(payload: &str, symbology: Symbology) -> Result<Reconstructed, CleanError> {
    let matrix = MultiFormatWriter
        .encode_with_hints(payload, &format_of(symbology), 0, 0, &EncodeHints::default())
        .map_err(|_| CleanError::Unreadable)?;

    let (w, h) = (matrix.getWidth() as usize, matrix.getHeight() as usize);
    let mut modules = Vec::with_capacity(w * h);
    for y in 0..h {
        for x in 0..w {
            modules.push(matrix.get(x as u32, y as u32));
        }
    }
    let rebuilt = Reconstructed::from_modules(w, h, modules).ok_or(CleanError::Unreadable)?;

    let (luma, lw, lh) = rebuilt.to_luma(6, crate::render::quiet_zone_modules(symbology));
    let check = rxing::helpers::detect_in_luma(luma, lw as u32, lh as u32, None)
        .map_err(|_| CleanError::Unreadable)?;
    if check.getText() != payload {
        return Err(CleanError::Unreadable);
    }

    Ok(rebuilt)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_luma(symbology: Symbology, payload: &str, scale: u32) -> (Vec<u8>, u32, u32) {
        let matrix = MultiFormatWriter
            .encode_with_hints(
                payload,
                &format_of(symbology),
                0,
                0,
                &EncodeHints::default(),
            )
            .unwrap();
        let (mw, mh) = (matrix.getWidth(), matrix.getHeight());
        let quiet = 4;
        let w = (mw + 2 * quiet) * scale;
        let h = (mh + 2 * quiet) * scale;
        let mut luma = vec![255u8; (w * h) as usize];
        for my in 0..mh {
            for mx in 0..mw {
                if !matrix.get(mx, my) {
                    continue;
                }
                for dy in 0..scale {
                    for dx in 0..scale {
                        let x = (mx + quiet) * scale + dx;
                        let y = (my + quiet) * scale + dy;
                        luma[(y * w + x) as usize] = 0;
                    }
                }
            }
        }
        (luma, w, h)
    }

    const PAYLOAD: &str = "barclean all formats 0123456789";

    #[test]
    fn every_symbology_cleans_and_restores() {
        for symbology in Symbology::ALL {
            let (luma, w, h) = render_luma(symbology, PAYLOAD, 8);
            let cleaned = clean(&luma, w, h)
                .unwrap_or_else(|e| panic!("{:?} did not clean: {e}", symbology.name()));

            assert_eq!(cleaned.symbology, symbology, "wrong symbology reported");
            assert_eq!(cleaned.payload, PAYLOAD, "{}: payload mismatch", symbology.name());
            assert!(!cleaned.source_inverted);

            let expected = if symbology == Symbology::QrCode {
                Fidelity::Exact
            } else {
                Fidelity::Reencoded
            };
            assert_eq!(cleaned.fidelity, expected, "{}", symbology.name());
        }
    }

    #[test]
    fn every_restoration_scans_back_to_the_original_payload() {
        // The only property a user actually depends on.
        for symbology in Symbology::ALL {
            let (luma, w, h) = render_luma(symbology, PAYLOAD, 8);
            let cleaned = clean(&luma, w, h).unwrap();

            let png = crate::render::to_png(&cleaned.rebuilt, symbology, 8, false).unwrap();
            let img = image::load_from_memory(&png).unwrap().to_luma8();
            let (iw, ih) = img.dimensions();
            let scanned = rxing::helpers::detect_in_luma(img.into_raw(), iw, ih, None)
                .unwrap_or_else(|e| panic!("{} restoration does not scan: {e}", symbology.name()));
            assert_eq!(scanned.getText(), PAYLOAD, "{}", symbology.name());
        }
    }

    #[test]
    fn inverted_sources_are_read_for_every_symbology() {
        for symbology in Symbology::ALL {
            let (mut luma, w, h) = render_luma(symbology, PAYLOAD, 8);
            for p in luma.iter_mut() {
                *p = 255 - *p;
            }
            let cleaned = clean(&luma, w, h)
                .unwrap_or_else(|e| panic!("inverted {} did not clean: {e}", symbology.name()));
            assert_eq!(cleaned.payload, PAYLOAD);
            assert!(
                cleaned.source_inverted,
                "{} read via the inverted path but was not marked",
                symbology.name()
            );
        }
    }

    #[test]
    fn only_qr_carries_a_comparable_sampled_matrix() {
        // A re-encode may not even be the same size as the original, so offering a module-by-module
        // comparison for it would be noise presented as evidence.
        for symbology in Symbology::ALL {
            let (luma, w, h) = render_luma(symbology, PAYLOAD, 8);
            let cleaned = clean(&luma, w, h).unwrap();
            if symbology == Symbology::QrCode {
                let sampled = cleaned.sampled.as_ref().expect("QR must carry its sampled matrix");
                assert_eq!(sampled.len(), cleaned.rebuilt.modules().len());
            } else {
                assert!(cleaned.sampled.is_none(), "{}", symbology.name());
            }
        }
    }

    #[test]
    fn a_blank_image_is_not_detected() {
        let luma = vec![255u8; 200 * 200];
        assert!(matches!(clean(&luma, 200, 200), Err(CleanError::NotDetected)));
    }
}
