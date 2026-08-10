//! Exact PDF417 restoration.
//!
//! # Why this one is different
//!
//! The other three symbologies are sampled onto a module grid, so a decoder can be instrumented to
//! record which module produced which codeword. PDF417 is not: it decodes by **scanning rows**,
//! tracking codeword boundaries along each one, with no grid to hang provenance off. There is
//! nothing to write corrected bits back *into*.
//!
//! What it has instead is a completely determined structure. Given the corrected codewords, the row
//! and column counts and the error-correction level, the symbol is reproducible exactly: every row
//! is start / left row indicator / data / right row indicator / stop, the indicators are computed
//! from the row's position and the symbol's shape, and every codeword maps to one of three cluster
//! patterns. So the restoration is a *redraw* rather than a write-back — and still exact, because
//! nothing about it is the encoder's choice.
//!
//! Note what is deliberately not used: the high-level encoder that turns text into codewords. That
//! is the part that would have invented a different symbol, and it is skipped entirely.
//!
//! # The comparison
//!
//! Without a sampled module grid there is nothing to diff module-by-module. But the decoder knows
//! the codewords as scanned *and* as corrected, and each codeword occupies a known 17-module span
//! in a known row — so the comparison is exact at codeword granularity, which is the granularity
//! the correction actually worked at.
//!
//! # No bootstrap loop
//!
//! PDF417 carries a single Reed-Solomon block across the whole symbol, so there are no surviving
//! blocks to localize damage for failed ones. It does already decode erasures — positions the
//! scanner could not read at all are passed to the error corrector as known-position damage, which
//! is the same lever the bootstrap loop builds for the others by inference.

use crate::clean::CleanError;
use crate::render::{ModuleVerdict, Reconstructed};
use rxing::common::HybridBinarizer;
use rxing::pdf417::decoder::pdf_417_scanning_decoder;
use rxing::pdf417::detector::pdf_417_detector;
use rxing::pdf417::pdf_417_common;
use rxing::{BinaryBitmap, DecodeHints, Luma8LuminanceSource, Point};

/// An exactly restored PDF417 symbol.
pub struct CleanedPdf417 {
    pub payload: String,
    pub width: usize,
    pub height: usize,
    pub rebuilt: Reconstructed,
    /// One verdict per module of the rebuild, at codeword granularity.
    pub verdicts: Vec<ModuleVerdict>,
    /// Codewords the error corrector changed.
    pub repaired_codewords: usize,
    /// Codewords the scanner could not read at all.
    pub erasures: usize,
    pub rows: usize,
    pub columns: usize,
}

/// Detect and exactly restore a PDF417 symbol in a luminance image.
pub fn clean_luma(luma: &[u8], width: u32, height: u32) -> Result<CleanedPdf417, CleanError> {
    let source = Luma8LuminanceSource::new(luma.to_vec(), width, height)
        .map_err(|_| CleanError::NotDetected)?;
    let mut bitmap = BinaryBitmap::new(HybridBinarizer::new(source));

    let hints = DecodeHints::default();
    let detected = pdf_417_detector::detect_with_hints(&mut bitmap, &hints, false)
        .map_err(|_| CleanError::NotDetected)?;
    let points = detected
        .getPoints()
        .first()
        .ok_or(CleanError::NotDetected)?
        .clone();

    // The detector reports eight corner points per symbol; the scanning decoder takes the four on
    // the right plus the module-width bounds it needs to walk each row. Widths are derived the way
    // the stock reader derives them — the stop pattern is a different number of modules from a
    // codeword, so the two have to be rescaled against each other before being compared.
    let (result, layout) = pdf_417_scanning_decoder::decode_reporting(
        detected.getBits(),
        points[4],
        points[5],
        points[6],
        points[7],
        min_codeword_width(&points),
        max_codeword_width(&points),
    )
    .map_err(|_| CleanError::Unrecoverable {
        blocks_total: 1,
        blocks_decoded: 0,
    })?;

    if layout.rows == 0 || layout.columns == 0 {
        return Err(CleanError::Unreadable);
    }

    // Compact symbols omit the right row indicator. The decoder does not report which it saw, and
    // full is overwhelmingly the common case; a compact symbol simply redraws as its full
    // equivalent, which carries the same data and scans identically.
    let modules = rxing::pdf417::encoder::render_from_codewords(
        &layout.codewords,
        layout.rows,
        layout.columns,
        layout.ec_level,
        false,
    )
    .map_err(|_| CleanError::Unreadable)?;

    // PDF417 rows are conventionally about three times taller than a module is wide — the stock
    // writer renders at that aspect, and a scanner tracking rows across a symbol needs the height to
    // follow them. Rendered one module tall, a structurally perfect symbol simply will not scan.
    const ROW_ASPECT: usize = 3;
    let modules: Vec<Vec<bool>> = modules
        .into_iter()
        .flat_map(|row| std::iter::repeat_n(row, ROW_ASPECT))
        .collect();

    let h = modules.len();
    let w = modules.first().map_or(0, |r| r.len());
    if w == 0 || h == 0 {
        return Err(CleanError::Unreadable);
    }
    let flat: Vec<bool> = modules.iter().flatten().copied().collect();
    let rebuilt = Reconstructed::from_modules(w, h, flat.clone()).ok_or(CleanError::Unreadable)?;

    let repaired = layout.repaired();
    let verdicts = build_verdicts(&flat, w, h, &layout, &repaired);

    Ok(CleanedPdf417 {
        payload: result.getText().to_string(),
        width: w,
        height: h,
        rebuilt,
        verdicts,
        repaired_codewords: repaired.len(),
        erasures: layout.erasures.len(),
        rows: layout.rows,
        columns: layout.columns,
    })
}

/// Mark every module of a repaired codeword as recovered.
///
/// Each data codeword occupies 17 modules in one row, starting after the row's start pattern and
/// left indicator — both 17 wide. Everything outside the data columns is structure the correction
/// never touched.
fn build_verdicts(
    modules: &[bool],
    w: usize,
    h: usize,
    layout: &pdf_417_scanning_decoder::Pdf417Layout,
    repaired: &[usize],
) -> Vec<ModuleVerdict> {
    const PATTERN: usize = 17;
    let data_start = 2 * PATTERN;

    let mut verdicts: Vec<ModuleVerdict> = modules
        .iter()
        .map(|&dark| {
            if dark {
                ModuleVerdict::MatchedDark
            } else {
                ModuleVerdict::MatchedLight
            }
        })
        .collect();

    // Each symbol row was expanded vertically for aspect, so a codeword covers a band of raster
    // rows rather than one.
    let band = if layout.rows > 0 { h / layout.rows } else { 1 };

    for &cw in repaired {
        let (row, col) = (cw / layout.columns, cw % layout.columns);
        for dy in 0..band {
            let y = row * band + dy;
            if y >= h {
                break;
            }
            for i in 0..PATTERN {
                let x = data_start + col * PATTERN + i;
                if x >= w {
                    break;
                }
                let idx = y * w + x;
                verdicts[idx] = if modules[idx] {
                    ModuleVerdict::RecoveredDark
                } else {
                    ModuleVerdict::RecoveredLight
                };
            }
        }
    }

    verdicts
}

fn max_width(p1: &Option<Point>, p2: &Option<Point>) -> u64 {
    match (p1, p2) {
        (Some(a), Some(b)) => (a.x - b.x).abs() as u64,
        _ => 0,
    }
}

fn min_width(p1: &Option<Point>, p2: &Option<Point>) -> u64 {
    match (p1, p2) {
        (Some(a), Some(b)) => (a.x - b.x).abs() as u64,
        _ => u32::MAX as u64,
    }
}

/// Widest plausible codeword, in pixels. Mirrors the stock reader.
fn max_codeword_width(p: &[Option<Point>]) -> u32 {
    let scale = |w: u64| {
        w * pdf_417_common::MODULES_IN_CODEWORD as u64
            / pdf_417_common::MODULES_IN_STOP_PATTERN as u64
    };
    max_width(&p[0], &p[4])
        .max(scale(max_width(&p[6], &p[2])))
        .max(max_width(&p[1], &p[5]).max(scale(max_width(&p[7], &p[3])))) as u32
}

/// Narrowest plausible codeword, in pixels. Mirrors the stock reader.
fn min_codeword_width(p: &[Option<Point>]) -> u32 {
    let scale = |w: u64| {
        w * pdf_417_common::MODULES_IN_CODEWORD as u64
            / pdf_417_common::MODULES_IN_STOP_PATTERN as u64
    };
    min_width(&p[0], &p[4])
        .min(scale(min_width(&p[6], &p[2])))
        .min(min_width(&p[1], &p[5]).min(scale(min_width(&p[7], &p[3])))) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use rxing::{BarcodeFormat, EncodeHints, MultiFormatWriter, Writer};

    fn render(payload: &str, scale: u32) -> (Vec<u8>, u32, u32) {
        let matrix = MultiFormatWriter
            .encode_with_hints(payload, &BarcodeFormat::PDF_417, 0, 0, &EncodeHints::default())
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

    const PAYLOAD: &str = "barclean pdf417 exact restoration 0123456789";

    #[test]
    fn an_undamaged_symbol_restores_and_reports_no_repair() {
        let (luma, w, h) = render(PAYLOAD, 4);
        let cleaned = clean_luma(&luma, w, h).expect("clean PDF417 must restore");

        assert_eq!(cleaned.payload, PAYLOAD);
        assert_eq!(cleaned.repaired_codewords, 0, "clean symbol reported repairs");
        assert!(cleaned.rows > 0 && cleaned.columns > 0);
        assert!(
            cleaned.verdicts.iter().all(|v| !v.recovered()),
            "an undamaged symbol should show nothing recovered"
        );
    }

    #[test]
    fn the_restoration_still_scans() {
        // The redraw is built from corrected codewords, never from the payload — so this is the
        // check that the structure was reproduced correctly, not merely that the text survived.
        let (luma, w, h) = render(PAYLOAD, 4);
        let cleaned = clean_luma(&luma, w, h).unwrap();

        let (out, ow, oh) = cleaned.rebuilt.to_luma(4, 4);
        let scanned = rxing::helpers::detect_in_luma(out, ow as u32, oh as u32, None)
            .expect("restored PDF417 must scan");
        assert_eq!(scanned.getText(), PAYLOAD);
    }

    #[test]
    fn damage_is_repaired_and_marked() {
        // Swept rather than fixed at one blot size. PDF417's default error-correction budget is far
        // thinner than QR-H's: too small a smudge damages nothing, and one sized like the blots the
        // other symbologies shrug off exhausts the budget outright. The claim under test is that
        // there exists damage which is both harmful and recoverable — and that when it is
        // recovered, the restoration is marked and still scans.
        let mut proved = false;

        for divisor in [24usize, 20, 16, 14, 12, 10] {
            let (mut luma, w, h) = render(PAYLOAD, 4);
            let (bx, by) = (w / 3, h / 3);
            let (bw, bh) = (w / divisor as u32, h / divisor as u32);
            for y in by..(by + bh).min(h) {
                for x in bx..(bx + bw).min(w) {
                    luma[(y * w + x) as usize] = 0;
                }
            }

            let Ok(cleaned) = clean_luma(&luma, w, h) else {
                continue; // budget exhausted at this size; try a smaller one
            };
            if cleaned.repaired_codewords == 0 && cleaned.erasures == 0 {
                continue; // harmless at this size
            }

            assert_eq!(cleaned.payload, PAYLOAD, "payload must survive recoverable damage");
            assert!(
                cleaned.verdicts.iter().any(|v| v.recovered()) || cleaned.erasures > 0,
                "repaired codewords should be marked in the comparison"
            );

            // And the restoration is still a valid symbol.
            let (out, ow, oh) = cleaned.rebuilt.to_luma(4, 4);
            let scanned = rxing::helpers::detect_in_luma(out, ow as u32, oh as u32, None)
                .expect("restored symbol must scan");
            assert_eq!(scanned.getText(), PAYLOAD);

            proved = true;
            break;
        }

        assert!(
            proved,
            "no blot size was both harmful and recoverable, so damage recovery went untested"
        );
    }

    #[test]
    fn a_blank_image_is_not_detected() {
        let luma = vec![255u8; 300 * 200];
        assert!(clean_luma(&luma, 300, 200).is_err());
    }

    #[test]
    fn the_rebuild_is_bit_identical_to_the_original_symbol() {
        // The claim, stated as strongly as it can be: not "it decodes to the same text" but "it is
        // the same symbol". Compared module by module against the encoder's own output, with the
        // writer's margin stripped and its vertical scaling undone.
        let (luma, w, h) = render(PAYLOAD, 4);
        let cleaned = clean_luma(&luma, w, h).expect("decode");

        let original = MultiFormatWriter
            .encode_with_hints(PAYLOAD, &BarcodeFormat::PDF_417, 0, 0, &EncodeHints::default())
            .unwrap();
        let margin = (original.getWidth() as usize - cleaned.width) / 2;
        let v_scale = (original.getHeight() as usize - 2 * margin) / cleaned.rows;
        let band = cleaned.height / cleaned.rows;

        let mut differing = 0usize;
        for row in 0..cleaned.rows {
            for x in 0..cleaned.width {
                let mine = cleaned.rebuilt.get(x, row * band);
                let theirs =
                    original.get((x + margin) as u32, (margin + row * v_scale) as u32);
                if mine != theirs {
                    differing += 1;
                }
            }
        }
        assert_eq!(
            differing, 0,
            "{differing} of {} modules differ from the original symbol",
            cleaned.width * cleaned.rows
        );
    }
}
