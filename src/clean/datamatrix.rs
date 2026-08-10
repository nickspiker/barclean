//! Exact DataMatrix restoration, with bootstrapping on interleaved symbols.
//!
//! DataMatrix is the one other symbology that earns the full treatment. Above 24×24 it interleaves
//! its Reed-Solomon blocks the way QR does, so a contiguous patch of damage is spread across every
//! block and surviving blocks can localize it for the ones that failed — the bootstrap loop applies
//! unchanged. Smaller symbols carry a single block and behave like Aztec: they decode or they do
//! not.
//!
//! Restoration is exact either way. Codewords are read with the modules that produced them,
//! corrected, and written back into those same modules, so the output is the symbol that was
//! photographed rather than a re-encoding of its payload.
//!
//! Function patterns — the L-shaped finder and the alternating timing edges — are not part of the
//! data stream and keep their sampled values, as with Aztec. A symbol whose finder was badly
//! damaged would not have been detected in the first place.

use crate::clean::bootstrap::{BootstrapParams, bootstrap};
use crate::clean::erasure::BlockLayout;
use crate::clean::CleanError;
use crate::render::Reconstructed;
use rxing::common::reedsolomon::{PredefinedGenericGF, ReedSolomonDecoder};
use rxing::common::{BitMatrix, DetectorRXingResult, HybridBinarizer};
use rxing::datamatrix::decoder::{BitMatrixParser, DataBlock, build_block_map};
use rxing::datamatrix::detector::{Detector, zxing_cpp_detector};
use rxing::{BinaryBitmap, Luma8LuminanceSource};

/// An exactly restored DataMatrix symbol.
pub struct CleanedDataMatrix {
    pub payload: String,
    pub width: usize,
    pub height: usize,
    /// The symbol as sampled, row-major, `true` = dark.
    pub sampled: Vec<bool>,
    pub rebuilt: Reconstructed,
    pub blocks_total: usize,
    /// Blocks that decoded before any bootstrapping.
    pub blocks_decoded_initially: usize,
}

impl CleanedDataMatrix {
    pub fn blocks_rescued(&self) -> usize {
        self.blocks_total
            .saturating_sub(self.blocks_decoded_initially)
    }
}

/// Detect and exactly restore a DataMatrix symbol in a luminance image.
pub fn clean_luma(luma: &[u8], width: u32, height: u32) -> Result<CleanedDataMatrix, CleanError> {
    let source = Luma8LuminanceSource::new(luma.to_vec(), width, height)
        .map_err(|_| CleanError::NotDetected)?;
    let mut bitmap = BinaryBitmap::new(HybridBinarizer::new(source));
    let black = bitmap.get_black_matrix();

    // The zxing-cpp-derived detector first, as the stock reader does — it copes with real
    // photography far better than the legacy one, which stays as the fallback.
    let matrix = zxing_cpp_detector::detect(black, true, true)
        .ok()
        .and_then(|found| found.into_iter().next().map(|s| s.getBits().clone()))
        .or_else(|| {
            Detector::new(black)
                .ok()
                .and_then(|mut d| d.detect().ok())
                .map(|d| d.getBits().clone())
        })
        .ok_or(CleanError::NotDetected)?;

    clean_matrix(matrix)
}

/// Clean an already-detected, grid-sampled DataMatrix.
pub fn clean_matrix(matrix: BitMatrix) -> Result<CleanedDataMatrix, CleanError> {
    let (w, h) = (matrix.getWidth() as usize, matrix.getHeight() as usize);
    let sampled = matrix_modules(&matrix, w, h);

    let mut parser = BitMatrixParser::new(&matrix).map_err(|_| CleanError::Unreadable)?;
    // Resolve the version from the symbol's dimensions rather than borrowing it out of the parser,
    // which the provenance read needs mutably.
    let version = rxing::datamatrix::decoder::Version::getVersionForDimensions(
        matrix.getHeight(),
        matrix.getWidth(),
    )
    .map_err(|_| CleanError::Unreadable)?;
    let (raw, provenance) = parser
        .read_codewords_with_provenance()
        .map_err(|_| CleanError::Unreadable)?;

    let data_blocks =
        DataBlock::getDataBlocks(&raw, version, false).map_err(|_| CleanError::Unreadable)?;
    let layouts: Vec<BlockLayout> = data_blocks
        .iter()
        .map(|b| BlockLayout::new(b.getCodewords().len(), b.getNumDataCodewords() as usize))
        .collect();

    let map = build_block_map(version, false).map_err(|_| CleanError::Unreadable)?;
    let block_globals: Vec<Vec<usize>> = (0..map.len()).map(|b| map.block(b).to_vec()).collect();
    let block_codewords: Vec<Vec<u8>> = block_globals
        .iter()
        .map(|globals| globals.iter().map(|&g| raw[g]).collect())
        .collect();

    // DataMatrix ECC200 uses the same GF(256) field as QR but with generator base 1, which is
    // exactly the branch a base-0-only Forney implementation gets wrong while passing every QR test.
    let decoder = ReedSolomonDecoder::new(PredefinedGenericGF::DataMatrixField256.into());
    let decode_block = |b: usize, erasures: &[usize]| -> Option<(Vec<u8>, Vec<usize>)> {
        let mut codewords = block_codewords[b].clone();
        let two_s = (layouts[b].total - layouts[b].data) as i32;
        let result = if erasures.is_empty() {
            decoder.decode_reporting(&mut codewords, two_s)
        } else {
            decoder.decode_with_erasures_reporting(&mut codewords, two_s, erasures)
        };
        result.ok().map(|damaged| (codewords, damaged))
    };
    let to_global = |b: usize, local: usize| block_globals.get(b)?.get(local).copied();

    // Module provenance is per-codeword, so the blob fit works in the same terms as QR's.
    let module_provenance: Vec<Vec<usize>> = provenance
        .iter()
        .map(|modules| modules.iter().map(|&(x, y)| y as usize * w + x as usize).collect())
        .collect();

    let outcome = bootstrap(
        &layouts,
        &to_global,
        &module_provenance,
        w.max(h),
        &decode_block,
        &BootstrapParams::default(),
    );

    let Some(data) = outcome.data.clone() else {
        return Err(CleanError::Unrecoverable {
            blocks_total: outcome.blocks_total,
            blocks_decoded: outcome.blocks_decoded_finally,
        });
    };

    let payload = rxing::datamatrix::decoder::decoded_bit_stream_parser::decode(&data, false)
        .map_err(|_| CleanError::Unreadable)?
        .getText()
        .to_string();

    // Reassemble the corrected interleaved stream, then write every corrected codeword back into
    // the modules it was read from.
    let mut corrected = raw.clone();
    for state in &outcome.blocks {
        if let Some(block) = &state.codewords {
            for (local, &cw) in block.iter().enumerate() {
                if let Some(g) = to_global(state.index, local) {
                    corrected[g] = cw;
                }
            }
        }
    }

    let mut modules = sampled.clone();
    for (cw_index, cells) in provenance.iter().enumerate() {
        let byte = corrected[cw_index];
        // Modules were recorded in read order, which is most-significant bit first.
        let bits = cells.len();
        for (i, &(x, y)) in cells.iter().enumerate() {
            let shift = bits.saturating_sub(1 + i);
            let bit = (byte >> shift) & 1 == 1;
            let (x, y) = (x as usize, y as usize);
            if x < w && y < h {
                modules[y * w + x] = bit;
            }
        }
    }

    let rebuilt = Reconstructed::from_modules(w, h, modules).ok_or(CleanError::Unreadable)?;

    Ok(CleanedDataMatrix {
        payload,
        width: w,
        height: h,
        sampled,
        rebuilt,
        blocks_total: outcome.blocks_total,
        blocks_decoded_initially: outcome.blocks_decoded_initially,
    })
}

fn matrix_modules(bits: &BitMatrix, w: usize, h: usize) -> Vec<bool> {
    let mut out = Vec::with_capacity(w * h);
    for y in 0..h {
        for x in 0..w {
            out.push(bits.get(x as u32, y as u32));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rxing::{BarcodeFormat, EncodeHints, MultiFormatWriter, Writer};

    fn render(payload: &str, scale: u32) -> (Vec<u8>, u32, u32) {
        let matrix = MultiFormatWriter
            .encode_with_hints(
                payload,
                &BarcodeFormat::DATA_MATRIX,
                0,
                0,
                &EncodeHints::default(),
            )
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
        (luma, w, h)
    }

    const PAYLOAD: &str = "barclean datamatrix exact restoration 0123456789 ABCDEFGH";

    #[test]
    fn an_undamaged_symbol_rebuilds_to_itself() {
        let (luma, w, h) = render(PAYLOAD, 8);
        let cleaned = clean_luma(&luma, w, h).expect("clean DataMatrix must restore");

        assert_eq!(cleaned.payload, PAYLOAD);
        assert_eq!(
            cleaned.rebuilt.differences(&cleaned.sampled),
            Some(0),
            "an undamaged symbol must rebuild to exactly what was sampled"
        );
        assert_eq!(cleaned.blocks_rescued(), 0, "nothing to rescue on a clean symbol");
    }

    #[test]
    fn damage_is_repaired_and_confined() {
        let (mut luma, w, h) = render(PAYLOAD, 8);
        // Off the L-finder, which lives on the left and bottom edges: damaging it kills detection
        // rather than exercising correction.
        let (bx, by, side) = (w / 3, h / 4, 4 * 8u32);
        for y in by..(by + side).min(h) {
            for x in bx..(bx + side).min(w) {
                luma[(y * w + x) as usize] = 0;
            }
        }

        let cleaned = clean_luma(&luma, w, h).expect("damaged DataMatrix must recover");
        assert_eq!(cleaned.payload, PAYLOAD, "payload must survive the damage");

        let differences = cleaned.rebuilt.differences(&cleaned.sampled).expect("same size");
        assert!(differences > 0, "the rebuild should differ from the damaged scan");
        assert!(
            differences < cleaned.width * cleaned.height / 4,
            "rebuild changed {differences} modules, far more than the blot covered"
        );
    }

    #[test]
    fn the_restoration_still_scans() {
        let (luma, w, h) = render(PAYLOAD, 8);
        let cleaned = clean_luma(&luma, w, h).unwrap();

        let (out, ow, oh) = cleaned.rebuilt.to_luma(8, 4);
        let scanned = rxing::helpers::detect_in_luma(out, ow as u32, oh as u32, None)
            .expect("restored DataMatrix must scan");
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
