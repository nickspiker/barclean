//! Does the bootstrap loop actually recover symbols plain decoding cannot?
//!
//! End-to-end against real encoded QR symbols with real snake-walk provenance, because that is the
//! only geometry that matters. The synthetic fixtures in the unit tests exercise loop control flow;
//! this measures whether the idea works on the thing itself.
//!
//! Occlusion is applied at the module level, so the numbers isolate the algebra from every
//! image-processing concern.

use barclean::Symbology;
use barclean::clean::{BlockLayout, BootstrapParams, bootstrap};
use barclean::corpus::symbol::{self, Specimen};
use rxing::common::BitMatrix;
use rxing::qrcode::decoder::{BitMatrixParser, DataBlock, build_block_map};
use rxing::common::reedsolomon::{PredefinedGenericGF, ReedSolomonDecoder};

fn occlude(spec: &Specimen, area_fraction: f32, value: bool) -> BitMatrix {
    let n = spec.truth.width;
    let side = ((n * n) as f32 * area_fraction).sqrt() as usize;
    let x0 = n.saturating_sub(side) / 2;
    let y0 = n.saturating_sub(side) / 2;

    let mut m = BitMatrix::new(n as u32, n as u32).expect("bit matrix");
    for y in 0..n {
        for x in 0..n {
            let covered = x >= x0 && x < x0 + side && y >= y0 && y < y0 + side;
            let bit = if covered { value } else { spec.truth.get(x, y) };
            if bit {
                m.set(x as u32, y as u32);
            }
        }
    }
    m
}

struct Parsed {
    raw: Vec<u8>,
    provenance: Vec<Vec<usize>>,
    layouts: Vec<BlockLayout>,
    block_starts: Vec<Vec<usize>>,
    dimension: usize,
}

fn parse(matrix: BitMatrix) -> Option<Parsed> {
    let dimension = matrix.getHeight() as usize;
    let mut parser = BitMatrixParser::new(matrix).ok()?;
    let version = parser.readVersion().ok()?;
    let ec = parser.readFormatInformation().ok()?.getErrorCorrectionLevel();
    let (raw, provenance) = parser.read_codewords_with_provenance().ok()?;

    let blocks = DataBlock::getDataBlocks(&raw, version, ec).ok()?;
    let layouts: Vec<BlockLayout> = blocks
        .iter()
        .map(|b| BlockLayout::new(b.getCodewords().len(), b.getNumDataCodewords() as usize))
        .collect();

    let map = build_block_map(version, ec).ok()?;
    let block_starts: Vec<Vec<usize>> = (0..map.len()).map(|b| map.block(b).to_vec()).collect();

    Some(Parsed {
        raw,
        provenance,
        layouts,
        block_starts,
        dimension,
    })
}

/// Run the bootstrap loop over a parsed symbol, returning `(rescued, initially, finally, total)`.
fn run(p: &Parsed) -> (usize, usize, usize, usize) {
    let blocks: Vec<Vec<u8>> = p
        .block_starts
        .iter()
        .map(|globals| globals.iter().map(|&g| p.raw[g]).collect())
        .collect();

    let layouts = p.layouts.clone();
    let decoder = ReedSolomonDecoder::new(PredefinedGenericGF::QrCodeField256.into());

    let decode_block = |b: usize, erasures: &[usize]| -> Option<(Vec<u8>, Vec<usize>)> {
        let mut codewords = blocks[b].clone();
        let two_s = (layouts[b].total - layouts[b].data) as i32;
        let result = if erasures.is_empty() {
            decoder.decode_reporting(&mut codewords, two_s)
        } else {
            decoder.decode_with_erasures_reporting(&mut codewords, two_s, erasures)
        };
        result.ok().map(|damaged| (codewords, damaged))
    };

    let to_global = |b: usize, local: usize| p.block_starts.get(b)?.get(local).copied();

    let out = bootstrap(
        &p.layouts,
        &to_global,
        &p.provenance,
        p.dimension,
        &decode_block,
        &BootstrapParams::default(),
    );

    (
        out.blocks_rescued(),
        out.blocks_decoded_initially,
        out.blocks_decoded_finally,
        out.blocks_total,
    )
}

const LONG: &str = "https://example.com/barclean/partial-decode-localization-experiment?\
    id=0123456789abcdef&session=fedcba9876543210&token=aaaabbbbccccddddeeeeffff";

#[test]
fn bootstrap_extends_the_recovery_range() {
    println!(
        "\n{:<7} {:>5}  {:>18}  {:>18}",
        "ECC", "area", "plain (dec/total)", "bootstrap (dec/total)"
    );

    let mut total_rescued = 0usize;
    let mut extended_any = false;

    for ec in ["L", "M", "Q", "H"] {
        let spec = symbol::generate(Symbology::QrCode, LONG, ec).unwrap();
        for step in 1..=45 {
            let area = step as f32 * 0.01;
            let Some(p) = parse(occlude(&spec, area, true)) else {
                continue;
            };
            let (rescued, initially, finally, total) = run(&p);
            if rescued > 0 {
                total_rescued += rescued;
                println!(
                    "ECC-{ec:<3} {:>4.0}%  {:>13}/{}  {:>13}/{}{}",
                    area * 100.0,
                    initially,
                    total,
                    finally,
                    total,
                    if finally == total { "  <-- FULL RECOVERY" } else { "" }
                );
                if finally == total && initially < total {
                    extended_any = true;
                }
            }
        }
    }

    println!("\ntotal blocks rescued across the sweep: {total_rescued}");
    assert!(
        total_rescued > 0,
        "the bootstrap loop never rescued a single block on real symbols — survivor evidence is \
         not reaching the failed blocks, and the mechanism does not work as designed"
    );
    assert!(
        extended_any,
        "blocks were rescued but no symbol was ever fully recovered; the loop helps but never \
         crosses the finish line, which would make it a diagnostic rather than a decoder"
    );
}

#[test]
fn bootstrap_never_loses_ground() {
    // The floor must be plain decoding's result. A bootstrap that can *reduce* the number of
    // decoded blocks would be actively harmful, since erasures spend budget.
    for ec in ["L", "M", "Q", "H"] {
        let spec = symbol::generate(Symbology::QrCode, LONG, ec).unwrap();
        for step in 0..=45 {
            let area = step as f32 * 0.01;
            let Some(p) = parse(occlude(&spec, area, true)) else {
                continue;
            };
            let (_, initially, finally, _) = run(&p);
            assert!(
                finally >= initially,
                "ECC-{ec} at {:.0}%: bootstrapping dropped from {initially} to {finally} blocks",
                area * 100.0
            );
        }
    }
}

#[test]
fn clean_symbols_are_untouched() {
    for ec in ["L", "M", "Q", "H"] {
        let spec = symbol::generate(Symbology::QrCode, LONG, ec).unwrap();
        let p = parse(occlude(&spec, 0.0, true)).expect("parse clean symbol");
        let (rescued, initially, finally, total) = run(&p);
        assert_eq!(initially, total, "ECC-{ec}: clean symbol must decode fully");
        assert_eq!(finally, total);
        assert_eq!(rescued, 0, "nothing to rescue on a clean symbol");
    }
}
