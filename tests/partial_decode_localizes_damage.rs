//! Does a partially-decoded symbol tell us where the damage is?
//!
//! The idea under test: a Reed-Solomon block that decodes has proved *exactly* which of its
//! codewords were damaged. That is a ground-truth damage map derived from algebra, with no pixel
//! heuristics involved — strictly better than any confidence estimate, where it is available.
//!
//! The obstacle is that RS is all-or-nothing, so the map exists only for blocks that did not need
//! it. What might rescue that is interleaving: QR distributes consecutive codewords across every
//! block in rotation, so a contiguous occlusion is spread evenly rather than concentrated. If some
//! blocks stay inside budget while others blow it, the survivors localize the blob — and because an
//! occlusion *is* a blob, that locates the damage in the failed blocks too.
//!
//! Whether that holds is an empirical question about how the interleave distributes a centred
//! square, and these tests answer it with numbers rather than argument.
//!
//! Occlusion is applied at the module level, directly to the encoded matrix, which isolates the
//! algebra from every image-processing concern.

use barclean::Symbology;
use barclean::corpus::symbol::{self, Specimen};
use rxing::common::BitMatrix;
use rxing::qrcode::decoder::{BitMatrixParser, decode_blocks};

/// Overwrite a centred square of modules with `value`, the way an opaque logo does.
fn occlude(spec: &Specimen, area_fraction: f32, value: bool) -> BitMatrix {
    let n = spec.truth.width;
    let side = ((n * n) as f32 * area_fraction).sqrt() as usize;
    let x0 = n.saturating_sub(side) / 2;
    let y0 = n.saturating_sub(side) / 2;

    let mut m = BitMatrix::new(n as u32, n as u32).expect("bit matrix");
    for y in 0..n {
        for x in 0..n {
            let occluded = x >= x0 && x < x0 + side && y >= y0 && y < y0 + side;
            let bit = if occluded { value } else { spec.truth.get(x, y) };
            if bit {
                m.set(x as u32, y as u32);
            }
        }
    }
    m
}

/// How many blocks survived, out of how many, and how many damaged codewords they pinned down.
fn survey(spec: &Specimen, area: f32, value: bool) -> (usize, usize, usize) {
    let matrix = occlude(spec, area, value);
    let mut parser = match BitMatrixParser::new(matrix) {
        Ok(p) => p,
        Err(_) => return (0, 0, 0),
    };
    let Ok(version) = parser.readVersion() else {
        return (0, 0, 0);
    };
    let Ok(format) = parser.readFormatInformation() else {
        return (0, 0, 0);
    };
    let ec = format.getErrorCorrectionLevel();
    let Ok(codewords) = parser.readCodewords() else {
        return (0, 0, 0);
    };

    match decode_blocks(&codewords, version, ec, &[]) {
        Ok(outcomes) => {
            let total = outcomes.len();
            let survived = outcomes.iter().filter(|o| o.decoded()).count();
            let located: usize = outcomes.iter().map(|o| o.damaged.len()).sum();
            (survived, total, located)
        }
        Err(_) => (0, 0, 0),
    }
}

/// A payload long enough to force a multi-block symbol, which is where interleaving exists at all.
const LONG: &str = "https://example.com/barclean/partial-decode-localization-experiment?\
    id=0123456789abcdef&session=fedcba9876543210&token=aaaabbbbccccddddeeeeffff";

#[test]
fn survivors_localize_damage_across_the_occlusion_range() {
    println!(
        "\n{:<6} {:>4} {:>7}   {}",
        "ECC", "blks", "area", "survived/total (damaged codewords located)"
    );

    for ec in ["L", "M", "Q", "H"] {
        let spec = symbol::generate(Symbology::QrCode, LONG, ec).expect("generate");
        let (_, blocks, _) = survey(&spec, 0.0, true);
        println!(
            "--- ECC-{ec}  version {}  {} modules  {} blocks",
            spec.version,
            spec.truth.width,
            blocks
        );

        for area in [0.0f32, 0.05, 0.10, 0.15, 0.20, 0.25, 0.30] {
            let (survived, total, located) = survey(&spec, area, true);
            println!(
                "{:<6} {:>4} {:>6.0}%   {}/{}  ({} located)",
                ec,
                total,
                area * 100.0,
                survived,
                total,
                located
            );
        }
    }
}

#[test]
fn a_clean_symbol_reports_no_damage() {
    // The control. Every block must decode and report zero damaged codewords, or the "damaged"
    // list means nothing.
    for ec in ["L", "M", "Q", "H"] {
        let spec = symbol::generate(Symbology::QrCode, LONG, ec).unwrap();
        let (survived, total, located) = survey(&spec, 0.0, true);
        assert!(total > 0, "ECC-{ec} produced no blocks");
        assert_eq!(survived, total, "ECC-{ec}: every block should decode");
        assert_eq!(located, 0, "ECC-{ec}: a clean symbol has nothing to report");
    }
}

#[test]
fn surviving_blocks_report_damage_when_the_symbol_is_occluded() {
    // The core claim: at an occlusion level heavy enough to break some blocks, the blocks that
    // survive still pin down damaged codeword positions exactly.
    // Scanned finely and across every ECC level, because the partial band turns out to be narrow:
    // interleaving distributes a centred occlusion so evenly that all blocks carry nearly the same
    // error count and therefore cross their budget at nearly the same occlusion level. Finding the
    // band at all takes looking for it.
    let mut bands: Vec<(String, f32, usize, usize, usize)> = Vec::new();

    for ec in ["L", "M", "Q", "H"] {
        let spec = symbol::generate(Symbology::QrCode, LONG, ec).unwrap();
        for step in 1..=48 {
            let area = step as f32 * 0.01;
            let (survived, total, located) = survey(&spec, area, true);
            if survived > 0 && survived < total {
                assert!(
                    located > 0,
                    "ECC-{ec} at {:.0}%: {survived}/{total} blocks decoded but located no damage — \
                     the survivors should have proved which codewords were wrong",
                    area * 100.0
                );
                bands.push((ec.to_string(), area, survived, total, located));
            }
        }
    }

    println!("\npartial-decode band (where survivors localize damage for failed blocks):");
    for (ec, area, survived, total, located) in &bands {
        println!(
            "  ECC-{ec}  {:>3.0}%  {survived}/{total} blocks survived, {located} codewords located",
            area * 100.0
        );
    }

    assert!(
        !bands.is_empty(),
        "no occlusion level anywhere produced a partial decode; the interleave never splits these \
         symbols, so survivor-based localization would have nothing to work with"
    );
}

#[test]
fn located_damage_is_confined_to_genuinely_damaged_codewords() {
    // Precision matters more than recall here. A codeword reported as damaged that was in fact
    // clean would send the blob fit off in the wrong direction, and erasures spent on it are
    // budget burned for nothing. Compare against the truth by decoding the clean symbol's
    // codewords and diffing.
    let spec = symbol::generate(Symbology::QrCode, LONG, "H").unwrap();

    let clean_matrix = occlude(&spec, 0.0, true);
    let mut clean_parser = BitMatrixParser::new(clean_matrix).unwrap();
    clean_parser.readVersion().unwrap();
    clean_parser.readFormatInformation().unwrap();
    let clean_codewords = clean_parser.readCodewords().unwrap();

    let damaged_matrix = occlude(&spec, 0.15, true);
    let mut parser = BitMatrixParser::new(damaged_matrix).unwrap();
    let version = parser.readVersion().unwrap();
    let ec = parser.readFormatInformation().unwrap().getErrorCorrectionLevel();
    let damaged_codewords = parser.readCodewords().unwrap();

    let truly_damaged: Vec<usize> = clean_codewords
        .iter()
        .zip(&damaged_codewords)
        .enumerate()
        .filter(|(_, (a, b))| a != b)
        .map(|(i, _)| i)
        .collect();

    let outcomes = decode_blocks(&damaged_codewords, version, ec, &[]).unwrap();
    let reported: Vec<usize> = outcomes.iter().flat_map(|o| o.damaged.clone()).collect();

    println!(
        "15% occlusion: {} codewords truly damaged, {} located by surviving blocks",
        truly_damaged.len(),
        reported.len()
    );

    for r in &reported {
        assert!(
            truly_damaged.contains(r),
            "codeword {r} was reported damaged but is identical to the clean symbol — a false \
             positive here would misdirect the blob fit and waste erasure budget"
        );
    }
}
