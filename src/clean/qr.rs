//! The full QR cleaning path: image in, payload out, damage map alongside.
//!
//! Chains detection, provenance-recording codeword extraction, and the bootstrap loop into one
//! call, then parses the recovered codewords into a payload. This is what the camera path runs and
//! what the grading harness measures.

use crate::clean::bootstrap::{BootstrapOutcome, BootstrapParams, bootstrap};
use crate::clean::erasure::BlockLayout;
use rxing::common::reedsolomon::{PredefinedGenericGF, ReedSolomonDecoder};
use rxing::common::{BitMatrix, DetectorRXingResult, HybridBinarizer};
use rxing::qrcode::decoder::{BitMatrixParser, DataBlock, build_block_map, decoded_bit_stream_parser};
use rxing::qrcode::detector::Detector;
use rxing::{BinaryBitmap, DecodeHints, Luma8LuminanceSource};

/// What cleaning a symbol produced.
#[derive(Clone, Debug)]
pub struct Cleaned {
    pub payload: String,
    /// Symbol width in modules.
    pub dimension: usize,
    pub blocks_total: usize,
    /// Blocks that decoded before any bootstrapping — what a stock decoder would have managed.
    pub blocks_decoded_initially: usize,
    /// Refinement rounds the loop took.
    pub rounds: usize,
    /// Modules belonging to codewords proven damaged — the inspect overlay's damage layer, and
    /// later the reconstruction mask. See `BootstrapOutcome::damaged_codeword_modules` for why this
    /// is codeword-granular rather than module-granular.
    pub damaged_modules: Vec<usize>,
}

impl Cleaned {
    /// Blocks recovered purely by bootstrapping — what barclean added over a stock decode.
    pub fn blocks_rescued(&self) -> usize {
        self.blocks_total
            .saturating_sub(self.blocks_decoded_initially)
    }

    /// Whether a stock decoder would have failed on this symbol.
    pub fn needed_barclean(&self) -> bool {
        self.blocks_decoded_initially < self.blocks_total
    }
}

#[derive(Debug)]
pub enum CleanError {
    /// No symbol found. Detection is upstream of everything here, so this is a framing, focus or
    /// resolution problem rather than a damage problem.
    NotDetected,
    /// Located, but its structure could not be read — version or format information unrecoverable.
    Unreadable,
    /// Read, but too damaged to recover even with bootstrapping. Carries how far it got, which is
    /// the difference between "nearly there, hold steadier" and "hopeless".
    Unrecoverable {
        blocks_total: usize,
        blocks_decoded: usize,
    },
}

impl core::fmt::Display for CleanError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            CleanError::NotDetected => write!(f, "no symbol found"),
            CleanError::Unreadable => write!(f, "symbol found but unreadable"),
            CleanError::Unrecoverable {
                blocks_total,
                blocks_decoded,
            } => write!(f, "recovered {blocks_decoded}/{blocks_total} blocks"),
        }
    }
}

/// Detect and clean a QR symbol in a luminance image.
pub fn clean_luma(luma: &[u8], width: u32, height: u32) -> Result<Cleaned, CleanError> {
    let source = Luma8LuminanceSource::new(luma.to_vec(), width, height)
        .map_err(|_| CleanError::NotDetected)?;
    let mut bitmap = BinaryBitmap::new(HybridBinarizer::new(source));
    let hints = DecodeHints::default();

    let detected = Detector::new(bitmap.get_black_matrix())
        .detect_with_hints(&hints)
        .map_err(|_| CleanError::NotDetected)?;

    clean_bitmatrix(detected.getBits().clone())
}

/// Clean an already-detected, grid-sampled symbol.
///
/// Split out from [`clean_luma`] so the grading harness can drive the algebra directly, without an
/// imaging pipeline in the measurement.
pub fn clean_bitmatrix(bits: BitMatrix) -> Result<Cleaned, CleanError> {
    let dimension = bits.getHeight() as usize;
    let mut parser = BitMatrixParser::new(bits).map_err(|_| CleanError::Unreadable)?;
    let version = parser.readVersion().map_err(|_| CleanError::Unreadable)?;
    let ec_level = parser
        .readFormatInformation()
        .map_err(|_| CleanError::Unreadable)?
        .getErrorCorrectionLevel();

    let (raw, provenance) = parser
        .read_codewords_with_provenance()
        .map_err(|_| CleanError::Unreadable)?;

    let data_blocks =
        DataBlock::getDataBlocks(&raw, version, ec_level).map_err(|_| CleanError::Unreadable)?;
    let layouts: Vec<BlockLayout> = data_blocks
        .iter()
        .map(|b| BlockLayout::new(b.getCodewords().len(), b.getNumDataCodewords() as usize))
        .collect();

    let map = build_block_map(version, ec_level).map_err(|_| CleanError::Unreadable)?;
    let block_globals: Vec<Vec<usize>> = (0..map.len()).map(|b| map.block(b).to_vec()).collect();
    let block_codewords: Vec<Vec<u8>> = block_globals
        .iter()
        .map(|globals| globals.iter().map(|&g| raw[g]).collect())
        .collect();

    let decoder = ReedSolomonDecoder::new(PredefinedGenericGF::QrCodeField256.into());
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

    let outcome: BootstrapOutcome = bootstrap(
        &layouts,
        &to_global,
        &provenance,
        dimension,
        &decode_block,
        &BootstrapParams::default(),
    );

    let Some(data) = outcome.data.clone() else {
        return Err(CleanError::Unrecoverable {
            blocks_total: outcome.blocks_total,
            blocks_decoded: outcome.blocks_decoded_finally,
        });
    };

    let parsed = decoded_bit_stream_parser::decode(&data, version, ec_level, &DecodeHints::default())
        .map_err(|_| CleanError::Unreadable)?;

    Ok(Cleaned {
        payload: parsed.getText().to_string(),
        dimension,
        blocks_total: outcome.blocks_total,
        blocks_decoded_initially: outcome.blocks_decoded_initially,
        rounds: outcome.rounds,
        damaged_modules: outcome.damaged_codeword_modules,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Symbology;
    use crate::corpus::symbol;

    const PAYLOAD: &str = "https://example.com/barclean/end-to-end?id=0123456789abcdef&\
        session=fedcba9876543210&token=aaaabbbbccccddddeeeeffff";

    /// Encode, then overwrite a centred square of modules the way an opaque logo does.
    fn defaced(ec: &str, area: f32) -> BitMatrix {
        let spec = symbol::generate(Symbology::QrCode, PAYLOAD, ec).unwrap();
        let n = spec.truth.width;
        let side = ((n * n) as f32 * area).sqrt() as usize;
        let x0 = n.saturating_sub(side) / 2;
        let y0 = n.saturating_sub(side) / 2;

        let mut m = BitMatrix::new(n as u32, n as u32).unwrap();
        for y in 0..n {
            for x in 0..n {
                let covered = x >= x0 && x < x0 + side && y >= y0 && y < y0 + side;
                if if covered { true } else { spec.truth.get(x, y) } {
                    m.set(x as u32, y as u32);
                }
            }
        }
        m
    }

    #[test]
    fn clean_symbol_round_trips() {
        let cleaned = clean_bitmatrix(defaced("H", 0.0)).expect("clean symbol must decode");
        assert_eq!(cleaned.payload, PAYLOAD);
        assert!(!cleaned.needed_barclean(), "nothing to rescue");
        assert_eq!(cleaned.blocks_rescued(), 0);
        assert!(cleaned.damaged_modules.is_empty());
    }

    #[test]
    fn recovers_symbols_stock_decoding_cannot_read() {
        // Swept rather than asserted at one percentage, because the exact boundary depends on the
        // payload's version and block count — pinning a number would make this brittle without
        // making it more informative. The claim under test is payload-independent: there exist
        // occlusion levels that defeat stock decoding and that barclean recovers, payload intact.
        let mut recovered: Vec<(String, u32, usize, usize)> = Vec::new();

        for ec in ["L", "M", "Q", "H"] {
            for step in 1..=45u32 {
                let area = step as f32 * 0.01;
                if let Ok(c) = clean_bitmatrix(defaced(ec, area)) {
                    if c.needed_barclean() {
                        assert_eq!(
                            c.payload, PAYLOAD,
                            "ECC-{ec} at {step}%: recovered the wrong payload, which is far worse \
                             than failing"
                        );
                        recovered.push((
                            ec.to_string(),
                            step,
                            c.blocks_decoded_initially,
                            c.blocks_total,
                        ));
                    }
                }
            }
        }

        println!("\nrecovered where stock decoding could not:");
        for (ec, step, initial, total) in &recovered {
            println!("  ECC-{ec} {step:>3}%  stock {initial}/{total} -> barclean {total}/{total}");
        }

        assert!(
            !recovered.is_empty(),
            "no occlusion level at any ECC level was both fatal to stock decoding and recoverable \
             here — the end-to-end path is not delivering what the bootstrap loop measured"
        );
    }

    #[test]
    fn damage_map_concentrates_inside_the_occlusion() {
        // The map is codeword-granular: a damaged codeword contributes all eight of its modules, and
        // the placement walk scatters those across the symbol, so some necessarily land outside the
        // logo. What the blob fit relies on is that the set is *dense* inside it and sparse outside.
        let area = 0.20f32;
        let cleaned = clean_bitmatrix(defaced("H", area)).expect("recover");
        let n = cleaned.dimension;
        let side = ((n * n) as f32 * area).sqrt() as usize;
        let x0 = n.saturating_sub(side) / 2;
        let y0 = n.saturating_sub(side) / 2;

        assert!(!cleaned.damaged_modules.is_empty());
        let inside = cleaned
            .damaged_modules
            .iter()
            .filter(|&&m| {
                let (x, y) = (m % n, m / n);
                x >= x0 && x < x0 + side && y >= y0 && y < y0 + side
            })
            .count();
        let fraction = inside as f32 / cleaned.damaged_modules.len() as f32;

        // The occlusion is 20% of the symbol, so a set unrelated to it would land ~20% inside.
        println!(
            "{inside}/{} damaged-codeword modules inside a {:.0}% occlusion ({:.0}%)",
            cleaned.damaged_modules.len(),
            area * 100.0,
            fraction * 100.0
        );
        assert!(
            fraction > 0.5,
            "only {:.0}% of the damage map fell inside the occlusion; the blob fit would be \
             chasing noise",
            fraction * 100.0
        );
    }

    #[test]
    fn hopeless_damage_reports_how_far_it_got() {
        // Distinguishing "nearly there" from "hopeless" is what lets the UI say something useful
        // instead of just failing.
        match clean_bitmatrix(defaced("L", 0.45)) {
            Err(CleanError::Unrecoverable {
                blocks_total,
                blocks_decoded,
            }) => {
                assert!(blocks_total > 0);
                assert!(blocks_decoded < blocks_total);
            }
            Err(other) => panic!("expected Unrecoverable, got {other:?}"),
            Ok(c) => panic!("45% at ECC-L should not recover, got {:?}", c.payload),
        }
    }
}
