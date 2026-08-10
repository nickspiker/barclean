//! The full QR cleaning path: image in, payload out, damage map alongside.
//!
//! Chains detection, provenance-recording codeword extraction, and the bootstrap loop into one
//! call, then parses the recovered codewords into a payload. This is what the camera path runs and
//! what the grading harness measures.

use crate::clean::bootstrap::{BootstrapOutcome, BootstrapParams, bootstrap};
use crate::clean::erasure::BlockLayout;
use rxing::common::reedsolomon::{PredefinedGenericGF, ReedSolomonDecoder};
use rxing::common::{BitMatrix, DetectorRXingResult, HybridBinarizer};
use rxing::qrcode::common::ErrorCorrectionLevel;
use rxing::qrcode::decoder::{BitMatrixParser, DataBlock, build_block_map, decoded_bit_stream_parser};
use rxing::qrcode::detector::Detector;
use rxing::{BinaryBitmap, DecodeHints, Luma8LuminanceSource};

/// What cleaning a symbol produced.
#[derive(Clone, Debug)]
pub struct Cleaned {
    pub payload: String,
    /// Symbol width in modules.
    pub dimension: usize,
    /// Pixels per module in the source image, measured from the detector's finder-pattern centres.
    ///
    /// `0.0` when the symbol did not come from a real detection (the grading harness feeds bit
    /// matrices directly). This is the number the lens picker's predictions are built on, so it has
    /// to be measured rather than estimated from the frame size — the symbol occupies whatever
    /// fraction of the frame it occupies, and guessing that it fills the frame makes every other
    /// lens look like it would crop.
    pub px_per_module: f32,
    pub blocks_total: usize,
    /// Blocks that decoded before any bootstrapping — what a stock decoder would have managed.
    pub blocks_decoded_initially: usize,
    /// Refinement rounds the loop took.
    pub rounds: usize,
    /// Modules belonging to codewords proven damaged — the inspect overlay's damage layer, and
    /// later the reconstruction mask. See `BootstrapOutcome::damaged_codeword_modules` for why this
    /// is codeword-granular rather than module-granular.
    pub damaged_modules: Vec<usize>,
    /// The corrected codeword stream — data and error correction, both repaired.
    ///
    /// This, not the payload, is what a pristine re-render is built from. See
    /// [`crate::render::exact`] for why re-encoding the decoded text would produce a different
    /// symbol.
    pub codewords: Vec<u8>,
    pub version: u32,
    pub ec_level: String,
    pub mask: i32,
    /// Whether the source was light-on-dark and had to be inverted to be read.
    ///
    /// Carried through so the export can be written back in the polarity it was found in — a
    /// white-on-black sign should be restored white-on-black.
    pub source_inverted: bool,
    /// The symbol exactly as it was sampled off the camera, row-major, `true` = dark.
    ///
    /// Kept so the result screen can show *what changed*: comparing this against the rebuild marks
    /// every module barclean had to recover. Orientation-aligned with the reconstruction, including
    /// through the mirrored retry — otherwise the whole grid would read as "changed".
    pub sampled: Vec<bool>,
}

impl Cleaned {
    /// Rebuild the pristine symbol this scan came from.
    pub fn reconstruct(&self) -> Result<crate::render::Reconstructed, String> {
        let version = rxing::qrcode::common::Version::getVersionForNumber(self.version)
            .map_err(|e| e.to_string())?;
        let ec = match self.ec_level.as_str() {
            "L" => ErrorCorrectionLevel::L,
            "M" => ErrorCorrectionLevel::M,
            "Q" => ErrorCorrectionLevel::Q,
            "H" => ErrorCorrectionLevel::H,
            other => return Err(format!("unknown error-correction level {other:?}")),
        };
        crate::render::from_codewords(&self.codewords, version, ec, self.mask)
    }

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
///
/// Tries the image as-is, then inverted. Plenty of real codes are printed light-on-dark — signage,
/// dark packaging, screens in dark mode — and the binarizer marks the *background* as the dark
/// modules there, so detection simply fails. rxing exposes an `AlsoInverted` hint but only on the
/// multi-format reader, not on the bare detector this path uses, so the retry is done here.
pub fn clean_luma(luma: &[u8], width: u32, height: u32) -> Result<Cleaned, CleanError> {
    match clean_luma_oriented(luma, width, height) {
        Ok(mut c) => {
            c.source_inverted = false;
            Ok(c)
        }
        Err(first) => {
            let flipped: Vec<u8> = luma.iter().map(|v| 255 - v).collect();
            match clean_luma_oriented(&flipped, width, height) {
                Ok(mut c) => {
                    c.source_inverted = true;
                    Ok(c)
                }
                // Report the upright attempt's verdict: an inverted read of a non-inverted symbol
                // fails in uninformative ways, and "how close was that" should describe the image
                // as it actually is.
                Err(_) => Err(first),
            }
        }
    }
}

fn clean_luma_oriented(luma: &[u8], width: u32, height: u32) -> Result<Cleaned, CleanError> {
    let source = Luma8LuminanceSource::new(luma.to_vec(), width, height)
        .map_err(|_| CleanError::NotDetected)?;
    let bitmap = BinaryBitmap::new(HybridBinarizer::new(source));
    let hints = DecodeHints::default();

    let detected = Detector::new(bitmap.get_black_matrix())
        .detect_with_hints(&hints)
        .map_err(|_| CleanError::NotDetected)?;

    // Finder-pattern centres sit at module (3.5, 3.5) from each of three corners, so the two
    // adjacent pairs are (dimension - 7) modules apart and the third pair is that times root two.
    // Taking the minimum pairwise distance picks an adjacent pair regardless of the order the
    // detector returns them in.
    let points = detected.getPoints();
    let span_px = (0..points.len())
        .flat_map(|i| ((i + 1)..points.len()).map(move |j| (i, j)))
        .map(|(i, j)| {
            let (dx, dy) = (points[i].x - points[j].x, points[i].y - points[j].y);
            (dx * dx + dy * dy).sqrt()
        })
        .fold(f32::INFINITY, f32::min);

    let bits = detected.getBits().clone();
    let dimension = bits.getHeight() as usize;
    let mut cleaned = clean_bitmatrix(bits)?;
    if span_px.is_finite() && dimension > 7 {
        cleaned.px_per_module = span_px / (dimension - 7) as f32;
    }
    Ok(cleaned)
}

/// Clean an already-detected, grid-sampled symbol.
///
/// Split out from [`clean_luma`] so the grading harness can drive the algebra directly, without an
/// imaging pipeline in the measurement.
///
/// Tries a normal read, then a **mirrored** one. That second attempt is not optional: the detector
/// resolves a symbol's orientation from its three finder patterns, which are symmetric about the
/// diagonal, so a perfectly good symbol is routinely sampled transposed. The stock decoder retries
/// mirrored for exactly this reason, and without it a large share of real-world scans arrive here
/// as unreadable structure or codewords that decode to nothing — which on-device looked like
/// "0 of 2 blocks recovered" on codes a stock reader handled without complaint.
pub fn clean_bitmatrix(bits: BitMatrix) -> Result<Cleaned, CleanError> {
    let dimension = bits.getHeight() as usize;
    let sampled = matrix_modules(&bits, dimension);

    let mut parser = BitMatrixParser::new(bits).map_err(|_| CleanError::Unreadable)?;
    let first = clean_with_parser(&mut parser, dimension);
    if let Ok(mut cleaned) = first {
        cleaned.sampled = sampled;
        return Ok(cleaned);
    }

    // Mirrored retry, mirroring the stock decoder's own recovery path.
    if parser.remask().is_err() {
        return first;
    }
    parser.setMirror(true);
    if parser.readVersion().is_err() || parser.readFormatInformation().is_err() {
        return first;
    }
    parser.mirror();

    match clean_with_parser(&mut parser, dimension) {
        Ok(mut cleaned) => {
            // The mirrored read transposes the symbol, so the as-scanned matrix has to be
            // transposed too or every module would compare as changed.
            cleaned.sampled = transpose(&sampled, dimension);
            Ok(cleaned)
        }
        // Report whichever attempt got further, so the UI's "how close was that" is honest.
        Err(second) => Err(further_of(first.err(), second)),
    }
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

fn transpose(modules: &[bool], dimension: usize) -> Vec<bool> {
    let mut out = vec![false; modules.len()];
    for y in 0..dimension {
        for x in 0..dimension {
            out[x * dimension + y] = modules[y * dimension + x];
        }
    }
    out
}

/// Pick the more informative of two failures — the one that decoded more blocks.
fn further_of(first: Option<CleanError>, second: CleanError) -> CleanError {
    let progress = |e: &CleanError| match e {
        CleanError::Unrecoverable { blocks_decoded, .. } => *blocks_decoded as i32,
        CleanError::Unreadable => -1,
        CleanError::NotDetected => -2,
    };
    match first {
        Some(f) if progress(&f) >= progress(&second) => f,
        _ => second,
    }
}

fn clean_with_parser(
    parser: &mut BitMatrixParser,
    dimension: usize,
) -> Result<Cleaned, CleanError> {
    let version = parser.readVersion().map_err(|_| CleanError::Unreadable)?;
    let format = parser
        .readFormatInformation()
        .map_err(|_| CleanError::Unreadable)?;
    let ec_level = format.getErrorCorrectionLevel();
    let mask = format.getDataMask() as i32;

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

    // Reassemble the corrected interleaved stream. Every block's codewords came back repaired —
    // error correction included — so writing them back through the block map reproduces exactly
    // what `readCodewords` would have returned from an undamaged symbol.
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
        px_per_module: 0.0,
        blocks_total: outcome.blocks_total,
        blocks_decoded_initially: outcome.blocks_decoded_initially,
        rounds: outcome.rounds,
        damaged_modules: outcome.damaged_codeword_modules,
        codewords: corrected,
        version: version.getVersionNumber(),
        ec_level: format!("{ec_level}"),
        mask,
        // Both filled in by the caller, which knows whether the mirrored and inverted paths were
        // taken.
        source_inverted: false,
        sampled: Vec::new(),
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

    #[test]
    fn a_defaced_symbol_reconstructs_to_the_pristine_original() {
        // The whole product, end to end: take a symbol, cover a fifth of it, recover it, and rebuild
        // the original. Compared module-by-module against the undamaged encode — payload equality
        // would pass while emitting a differently-segmented symbol, which is a re-creation rather
        // than a restoration.
        let pristine = symbol::generate(Symbology::QrCode, PAYLOAD, "H").unwrap();

        let cleaned = clean_bitmatrix(defaced("H", 0.20)).expect("recover");
        assert_eq!(cleaned.payload, PAYLOAD);

        let rebuilt = cleaned.reconstruct().expect("reconstruct");
        assert_eq!(rebuilt.dimension, pristine.truth.width);
        assert_eq!(
            rebuilt.differences(pristine.truth.modules()),
            Some(0),
            "the rebuilt symbol is not bit-identical to the original"
        );
    }

    #[test]
    fn reconstruction_survives_a_bootstrapped_rescue() {
        // Reconstruction depends on the corrected codewords being reassembled from every block,
        // including the ones that only came back via erasure decoding. A block rescued late must
        // contribute its repaired codewords like any other.
        let pristine = symbol::generate(Symbology::QrCode, PAYLOAD, "Q").unwrap();

        let mut proved = false;
        for step in 19..=25u32 {
            let Ok(cleaned) = clean_bitmatrix(defaced("Q", step as f32 * 0.01)) else {
                continue;
            };
            if !cleaned.needed_barclean() {
                continue;
            }
            proved = true;
            let rebuilt = cleaned.reconstruct().expect("reconstruct after rescue");
            assert_eq!(
                rebuilt.differences(pristine.truth.modules()),
                Some(0),
                "rescued at {step}%: rebuilt symbol differs from the original"
            );
        }
        assert!(proved, "no bootstrapped rescue occurred, so nothing was proved");
    }
}
