//! Rebuilding the pristine symbol from corrected codewords.
//!
//! # Why not re-encode the payload
//!
//! The obvious approach — decode to text, encode a fresh QR — does **not** reproduce the original
//! symbol, and the difference is not cosmetic. Encoders differ in how they segment a payload across
//! numeric, alphanumeric, byte and kanji modes, where they place mode switches, how they pad, and
//! which ECI they declare. Two encoders given identical text routinely emit different codewords and
//! therefore different symbols. A "cleaned" code that scans to the right string but is structurally
//! a different symbol is a re-creation, not a restoration.
//!
//! Rebuilding from the **corrected codewords** sidesteps all of it. Those codewords *are* what the
//! original encoder emitted — Reed-Solomon recovered them exactly, not approximately. Re-applying
//! the original version, error-correction level and mask reproduces the original symbol bit for
//! bit, minus whatever was covering it.
//!
//! Which is why the test for this compares module-by-module against the original matrix rather than
//! comparing payloads. Payload equality would pass while silently emitting a differently-segmented
//! symbol.

use rxing::common::BitArray;
use rxing::common::cpp_essentials::ByteMatrix;
use rxing::qrcode::common::{ErrorCorrectionLevel, VersionRef};
use rxing::qrcode::encoder::matrix_util;

/// A rebuilt symbol: `true` is a dark module, row-major.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reconstructed {
    pub dimension: usize,
    modules: Vec<bool>,
}

impl Reconstructed {
    pub fn get(&self, x: usize, y: usize) -> bool {
        self.modules[y * self.dimension + x]
    }

    pub fn modules(&self) -> &[bool] {
        &self.modules
    }

    /// Modules differing from another symbol of the same size. `Some(0)` is bit-exact.
    pub fn differences(&self, other: &[bool]) -> Option<usize> {
        if other.len() != self.modules.len() {
            return None;
        }
        Some(
            self.modules
                .iter()
                .zip(other)
                .filter(|(a, b)| a != b)
                .count(),
        )
    }

    /// Render to an 8-bit greyscale buffer at `scale` pixels per module, with a quiet zone.
    ///
    /// The quiet zone is not decoration — a detector needs it to find the symbol at all, and a
    /// "cleaned" export without one would be less scannable than the damaged original.
    pub fn to_luma(&self, scale: usize, quiet_zone: usize) -> (Vec<u8>, usize, usize) {
        let side = (self.dimension + 2 * quiet_zone) * scale;
        let mut out = vec![255u8; side * side];
        for my in 0..self.dimension {
            for mx in 0..self.dimension {
                if !self.get(mx, my) {
                    continue;
                }
                for dy in 0..scale {
                    for dx in 0..scale {
                        let x = (mx + quiet_zone) * scale + dx;
                        let y = (my + quiet_zone) * scale + dy;
                        out[y * side + x] = 0;
                    }
                }
            }
        }
        (out, side, side)
    }
}

/// Rebuild a symbol from its corrected codeword stream.
///
/// `codewords` is the full interleaved stream — data and error correction together, both corrected
/// — exactly as `readCodewords` would have returned it from an undamaged symbol.
pub fn from_codewords(
    codewords: &[u8],
    version: VersionRef,
    ec_level: ErrorCorrectionLevel,
    mask_pattern: i32,
) -> Result<Reconstructed, String> {
    let expected = version.getTotalCodewords() as usize;
    if codewords.len() != expected {
        return Err(format!(
            "expected {expected} codewords for version {}, got {}",
            version.getVersionNumber(),
            codewords.len()
        ));
    }
    if !(0..8).contains(&mask_pattern) {
        return Err(format!("mask pattern {mask_pattern} out of range"));
    }

    let mut bits = BitArray::new();
    for &cw in codewords {
        bits.appendBits(cw as usize, 8).map_err(|e| e.to_string())?;
    }

    let dimension = version.getDimensionForVersion() as usize;
    let mut matrix = ByteMatrix::new(dimension as u32, dimension as u32);
    matrix_util::buildMatrix(&bits, &ec_level, version, mask_pattern, &mut matrix)
        .map_err(|e| e.to_string())?;

    let mut modules = Vec::with_capacity(dimension * dimension);
    for y in 0..dimension {
        for x in 0..dimension {
            modules.push(matrix.get(x as u32, y as u32) == 1);
        }
    }

    Ok(Reconstructed { dimension, modules })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Symbology;
    use crate::corpus::symbol;
    use rxing::common::BitMatrix;
    use rxing::qrcode::common::Version;
    use rxing::qrcode::decoder::BitMatrixParser;

    /// Read a symbol's codewords, mask and structure back out of its matrix.
    fn parse(truth: &symbol::TruthMatrix) -> (Vec<u8>, VersionRef, ErrorCorrectionLevel, i32) {
        let n = truth.width;
        let mut m = BitMatrix::new(n as u32, n as u32).unwrap();
        for y in 0..n {
            for x in 0..n {
                if truth.get(x, y) {
                    m.set(x as u32, y as u32);
                }
            }
        }
        let mut parser = BitMatrixParser::new(m).unwrap();
        let version = parser.readVersion().unwrap();
        let format = parser.readFormatInformation().unwrap();
        let (ec, mask) = (format.getErrorCorrectionLevel(), format.getDataMask() as i32);
        let codewords = parser.readCodewords().unwrap();
        (codewords, version, ec, mask)
    }

    #[test]
    fn rebuilds_a_symbol_bit_for_bit() {
        // The claim that matters: not "it decodes to the same text" but "it is the same symbol".
        for ec in ["L", "M", "Q", "H"] {
            for payload in [
                "12345678901234567890",              // numeric segmentation
                "HELLO WORLD $%*+-./: 42",           // alphanumeric
                "https://example.com/x?a=1&b=2",     // byte mode
                "mixed 123 ABC and lowercase bytes", // several mode switches
            ] {
                let spec = symbol::generate(Symbology::QrCode, payload, ec).unwrap();
                let (codewords, version, ec_level, mask) = parse(&spec.truth);

                let rebuilt = from_codewords(&codewords, version, ec_level, mask)
                    .unwrap_or_else(|e| panic!("ECC-{ec} {payload:?}: {e}"));

                assert_eq!(rebuilt.dimension, spec.truth.width);
                assert_eq!(
                    rebuilt.differences(spec.truth.modules()),
                    Some(0),
                    "ECC-{ec} {payload:?}: rebuilt symbol differs from the original"
                );
            }
        }
    }

    #[test]
    fn rebuilt_symbol_still_decodes() {
        let spec = symbol::generate(Symbology::QrCode, "round trip check", "Q").unwrap();
        let (codewords, version, ec_level, mask) = parse(&spec.truth);
        let rebuilt = from_codewords(&codewords, version, ec_level, mask).unwrap();

        let (luma, w, h) = rebuilt.to_luma(6, 4);
        let decoded = rxing::helpers::detect_in_luma(luma, w as u32, h as u32, None)
            .expect("a rebuilt symbol must be scannable");
        assert_eq!(decoded.getText(), "round trip check");
    }

    #[test]
    fn quiet_zone_is_present_and_light() {
        let spec = symbol::generate(Symbology::QrCode, "quiet zone", "M").unwrap();
        let (codewords, version, ec_level, mask) = parse(&spec.truth);
        let rebuilt = from_codewords(&codewords, version, ec_level, mask).unwrap();

        let (luma, w, _) = rebuilt.to_luma(4, 4);
        assert_eq!(luma[0], 255, "top-left corner must be quiet zone");
        assert_eq!(luma[w - 1], 255);
        assert_eq!(luma[luma.len() - 1], 255);
        // The first dark pixel should be at the finder's outer corner, 4 modules in.
        assert_eq!(luma[(4 * 4) * w + (4 * 4)], 0);
    }

    #[test]
    fn wrong_codeword_count_is_rejected() {
        let version = Version::getVersionForNumber(5).unwrap();
        let err = from_codewords(&[0u8; 3], version, ErrorCorrectionLevel::M, 0)
            .expect_err("a short stream cannot be a version 5 symbol");
        assert!(err.contains("expected"), "unhelpful error: {err}");
    }

    #[test]
    fn out_of_range_mask_is_rejected() {
        let spec = symbol::generate(Symbology::QrCode, "mask range", "M").unwrap();
        let (codewords, version, ec_level, _) = parse(&spec.truth);
        assert!(from_codewords(&codewords, version, ec_level, 8).is_err());
        assert!(from_codewords(&codewords, version, ec_level, -1).is_err());
    }
}
