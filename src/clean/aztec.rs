//! Exact Aztec restoration.
//!
//! # Rebuilding without re-encoding
//!
//! Re-encoding an Aztec payload produces a *different symbol*: the writer picks its own layer count
//! and its own sequence of mode latches, so the result says the same thing while looking nothing
//! like the code that was photographed. For a tool whose job is "clean up **this** code", that is
//! the wrong artefact.
//!
//! So the rebuild goes the other way. Read the bits along with the module each came from, correct
//! them with Reed-Solomon, and write the corrected values back into the modules they came from. The
//! result is the symbol that was scanned, with its damage removed — the same principle as the QR
//! path, reached differently because Aztec has no codeword-to-matrix placement worth reimplementing.
//!
//! # Why there is no bootstrap loop here
//!
//! Aztec carries a **single** Reed-Solomon block. QR's bootstrap works because interleaving spreads
//! a contiguous occlusion across every block, so blocks that survive can localize the damage for
//! the ones that failed. With one block there are no survivors: the symbol decodes or it does not.
//!
//! What Aztec does get is everything that follows a successful decode — exact restoration, the
//! module-level comparison, and export. Which covers the damage people actually bring it: folds,
//! smudges, scuffs and bad printing, where ordinary error correction already recovers the data and
//! the clean copy is the deliverable.
//!
//! # What is not restored
//!
//! Function patterns — the bullseye, the reference grid, the mode message — are not part of the
//! data stream and are left as they were sampled. In practice they arrive intact, because a symbol
//! whose bullseye was badly damaged would not have been detected at all.

use crate::clean::CleanError;
use crate::render::Reconstructed;
use rxing::aztec::decoder::{correct_codewords, extract_bits_with_provenance};
use rxing::aztec::detector::Detector;
use rxing::common::{BitMatrix, DetectorRXingResult, HybridBinarizer};
use rxing::{BinaryBitmap, Luma8LuminanceSource};

/// An exactly restored Aztec symbol.
pub struct CleanedAztec {
    pub payload: String,
    pub dimension: usize,
    /// The symbol as sampled, row-major, `true` = dark.
    pub sampled: Vec<bool>,
    /// The symbol with every corrected bit written back into its own module.
    pub rebuilt: Reconstructed,
    /// Codewords Reed-Solomon proved wrong.
    pub damaged_codewords: usize,
}

/// Detect and exactly restore an Aztec symbol in a luminance image.
pub fn clean_luma(luma: &[u8], width: u32, height: u32) -> Result<CleanedAztec, CleanError> {
    let source =
        Luma8LuminanceSource::new(luma.to_vec(), width, height).map_err(|_| CleanError::NotDetected)?;
    let mut bitmap = BinaryBitmap::new(HybridBinarizer::new(source));
    let black = bitmap.get_black_matrix().clone();

    // Mirrored retry, as the stock reader does: a symbol photographed through glass, or simply
    // resolved the other way round, is otherwise invisible.
    let detected = Detector::new(&black)
        .detect(false)
        .or_else(|_| Detector::new(&black).detect(true))
        .map_err(|_| CleanError::NotDetected)?;

    let matrix = detected.getBits().clone();
    let dimension = matrix.getHeight() as usize;
    if matrix.getWidth() as usize != dimension {
        return Err(CleanError::Unreadable);
    }

    let (rawbits, origin) = extract_bits_with_provenance(&detected, &matrix);
    let corrected = correct_codewords(&detected, &rawbits).map_err(|_| {
        // A single block means all-or-nothing; there is no partial progress to report.
        CleanError::Unrecoverable {
            blocks_total: 1,
            blocks_decoded: 0,
        }
    })?;

    // The payload comes from the stock decode of the same detection, so the text and the rebuild
    // are guaranteed to describe the same symbol.
    let payload = rxing::aztec::decoder::decode(&detected)
        .map_err(|_| CleanError::Unreadable)?
        .getText()
        .to_string();

    let sampled = matrix_modules(&matrix, dimension);

    // Write the corrected bits back where they came from. Everything not covered by the data
    // stream — bullseye, reference grid, mode message — keeps its sampled value.
    let fixed_bits = corrected.to_rawbits(rawbits.len());
    let mut modules = sampled.clone();
    for (i, &(x, y)) in origin.iter().enumerate() {
        let (x, y) = (x as usize, y as usize);
        if x < dimension && y < dimension {
            modules[y * dimension + x] = fixed_bits[i];
        }
    }

    let rebuilt =
        Reconstructed::from_modules(dimension, dimension, modules).ok_or(CleanError::Unreadable)?;

    Ok(CleanedAztec {
        payload,
        dimension,
        sampled,
        rebuilt,
        damaged_codewords: corrected.damaged.len(),
    })
}

fn matrix_modules(bits: &BitMatrix, dimension: usize) -> Vec<bool> {
    let mut out = Vec::with_capacity(dimension * dimension);
    for y in 0..dimension {
        for x in 0..dimension {
            out.push(bits.get(x as u32, y as u32));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rxing::{BarcodeFormat, EncodeHints, MultiFormatWriter, Writer};

    fn render(payload: &str, scale: u32) -> (Vec<u8>, u32, u32, BitMatrix) {
        let matrix = MultiFormatWriter
            .encode_with_hints(payload, &BarcodeFormat::AZTEC, 0, 0, &EncodeHints::default())
            .unwrap();
        let quiet = 4u32;
        let w = (matrix.getWidth() + 2 * quiet) * scale;
        let h = (matrix.getHeight() + 2 * quiet) * scale;
        let mut luma = vec![255u8; (w * h) as usize];
        for my in 0..matrix.getHeight() {
            for mx in 0..matrix.getWidth() {
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
        (luma, w, h, matrix)
    }

    const PAYLOAD: &str = "barclean aztec exact restoration 0123456789";

    #[test]
    fn an_undamaged_symbol_rebuilds_to_itself() {
        // The restoration must be the symbol that was scanned, not a re-encoding of its payload.
        let (luma, w, h, _) = render(PAYLOAD, 6);
        let cleaned = clean_luma(&luma, w, h).expect("clean Aztec must restore");

        assert_eq!(cleaned.payload, PAYLOAD);
        assert_eq!(cleaned.damaged_codewords, 0, "clean symbol reported damage");
        assert_eq!(
            cleaned.rebuilt.differences(&cleaned.sampled),
            Some(0),
            "an undamaged symbol must rebuild to exactly what was sampled"
        );
    }

    #[test]
    fn damage_is_repaired_and_the_rest_is_left_alone() {
        // Blot a patch, as a fold or a smudge would, and confirm the rebuild differs from the
        // damaged scan ONLY inside that patch — a rebuild that rewrote untouched modules would be
        // manufacturing a symbol rather than restoring one.
        let (mut luma, w, h, _) = render(PAYLOAD, 6);

        // Off-centre on purpose. The centre of an Aztec is the bullseye finder, and blotting it
        // kills DETECTION rather than exercising correction — the symbol is never located, so there
        // is nothing to restore. Real folds and smudges land on the data rings, which is what this
        // damages.
        let (bx, by, side) = (w / 6, h / 6, 4 * 6u32);
        for y in by..(by + side).min(h) {
            for x in bx..(bx + side).min(w) {
                luma[(y * w + x) as usize] = 0;
            }
        }

        let cleaned = clean_luma(&luma, w, h).expect("damaged Aztec must still recover");
        assert_eq!(cleaned.payload, PAYLOAD, "payload must survive the damage");
        assert!(
            cleaned.damaged_codewords > 0,
            "the blot should have damaged codewords"
        );

        let differences = cleaned
            .rebuilt
            .differences(&cleaned.sampled)
            .expect("same size");
        assert!(differences > 0, "the rebuild should differ from the damaged scan");
        assert!(
            differences < cleaned.dimension * cleaned.dimension / 4,
            "rebuild changed {differences} modules, far more than the blot covered"
        );
    }

    #[test]
    fn the_restoration_still_scans() {
        let (luma, w, h, _) = render(PAYLOAD, 6);
        let cleaned = clean_luma(&luma, w, h).unwrap();

        let (out, ow, oh) = cleaned.rebuilt.to_luma(6, 4);
        let scanned = rxing::helpers::detect_in_luma(out, ow as u32, oh as u32, None)
            .expect("restored Aztec must scan");
        assert_eq!(scanned.getText(), PAYLOAD);
    }

    #[test]
    fn a_blank_image_is_not_detected() {
        let luma = vec![255u8; 200 * 200];
        assert!(matches!(
            clean_luma(&luma, 200, 200),
            Err(CleanError::NotDetected)
        ));
    }
}
