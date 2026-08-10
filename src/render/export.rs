//! Encoding a rebuilt symbol for saving, and the four-colour comparison view.

use crate::render::Reconstructed;

/// How a module in the rebuild relates to what the camera actually saw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModuleVerdict {
    /// Matched the scan, and is light.
    MatchedLight,
    /// Matched the scan, and is dark.
    MatchedDark,
    /// Recovered — the scan disagreed — and the true value is light.
    RecoveredLight,
    /// Recovered, and the true value is dark.
    RecoveredDark,
    /// No comparison was possible — shown as the plain symbol, light module.
    ///
    /// Exists so a restoration without a comparable original renders as *the symbol*, in black and
    /// white, rather than borrowing the matched colours. Colouring it green and blue would claim
    /// "the scan already had this right" when nothing was compared at all, which is a stronger
    /// statement than any evidence supports.
    PlainLight,
    /// No comparison was possible — plain symbol, dark module.
    PlainDark,
}

impl ModuleVerdict {
    pub fn recovered(self) -> bool {
        matches!(
            self,
            ModuleVerdict::RecoveredLight | ModuleVerdict::RecoveredDark
        )
    }

    /// Whether this verdict carries a comparison at all.
    pub fn compared(self) -> bool {
        !matches!(self, ModuleVerdict::PlainLight | ModuleVerdict::PlainDark)
    }

    /// Display colour as `(r, g, b)`.
    ///
    /// Full-saturation primaries, deliberately. This is a diagnostic read at arm's length while
    /// holding a phone, and a muted palette makes the recovered patch blend into the matched field
    /// — which is the one distinction the view exists to draw.
    ///
    /// Green/blue for modules the scan already had right, yellow/red for ones barclean recovered.
    /// Within each pair the *lighter* colour marks a light module, so the grid still reads as the
    /// symbol rather than as its photographic negative.
    pub fn rgb(self) -> (u8, u8, u8) {
        match self {
            ModuleVerdict::MatchedLight => (0, 255, 0),
            ModuleVerdict::MatchedDark => (0, 0, 255),
            ModuleVerdict::RecoveredLight => (255, 255, 0),
            ModuleVerdict::RecoveredDark => (255, 0, 0),
            ModuleVerdict::PlainLight => (255, 255, 255),
            ModuleVerdict::PlainDark => (0, 0, 0),
        }
    }
}

/// Compare a rebuild against the modules the camera sampled.
///
/// Returns `None` if the two are not the same size, which would make any comparison meaningless.
pub fn compare(rebuilt: &Reconstructed, sampled: &[bool]) -> Option<Vec<ModuleVerdict>> {
    if sampled.len() != rebuilt.modules().len() {
        return None;
    }
    Some(
        rebuilt
            .modules()
            .iter()
            .zip(sampled)
            .map(|(&truth, &seen)| match (truth == seen, truth) {
                (true, false) => ModuleVerdict::MatchedLight,
                (true, true) => ModuleVerdict::MatchedDark,
                (false, false) => ModuleVerdict::RecoveredLight,
                (false, true) => ModuleVerdict::RecoveredDark,
            })
            .collect(),
    )
}

/// The quiet zone a symbology requires, in modules.
///
/// Not decoration. A detector locates a symbol by finding its finder patterns against clear
/// surroundings; export a symbol flush to its edge and it becomes harder to scan than the damaged
/// original, which would defeat the point entirely. QR and Aztec want 4 modules, DataMatrix 1, and
/// PDF417 2 on the sides.
pub fn quiet_zone_modules(symbology: crate::Symbology) -> usize {
    match symbology {
        crate::Symbology::QrCode => 4,
        crate::Symbology::Aztec => 4,
        crate::Symbology::DataMatrix => 1,
        crate::Symbology::Pdf417 => 2,
    }
}

/// Encode a rebuilt symbol as a black-and-white PNG.
///
/// Pure 1-bit content rendered as 8-bit greyscale: every pixel is 0 or 255, no anti-aliasing and no
/// interpolation, because a barcode's whole job is hard edges. `scale` is pixels per module.
pub fn to_png(
    rebuilt: &Reconstructed,
    symbology: crate::Symbology,
    scale: usize,
    inverted: bool,
) -> Result<Vec<u8>, String> {
    let quiet = quiet_zone_modules(symbology);
    let (mut luma, w, h) = rebuilt.to_luma(scale.max(1), quiet);

    // Preserve the source's polarity. A code printed light-on-dark is restored light-on-dark: the
    // export is meant to replace the original in situ, and handing back a black-on-white version of
    // a white-on-black sign would not match what it is going back onto. Both polarities scan.
    if inverted {
        for p in luma.iter_mut() {
            *p = 255 - *p;
        }
    }

    let img = image::GrayImage::from_raw(w as u32, h as u32, luma)
        .ok_or_else(|| "luma buffer did not match its dimensions".to_string())?;

    let mut png = Vec::new();
    img.write_to(
        &mut std::io::Cursor::new(&mut png),
        image::ImageFormat::Png,
    )
    .map_err(|e| e.to_string())?;
    Ok(png)
}

/// Filename for a save, in the format `2016-08-10 14:33:48.png`.
///
/// Takes the parts rather than reading a clock so it stays testable and so the platform layer owns
/// timezone handling — the phone knows what local time is; this does not.
pub fn timestamped_name(
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
) -> String {
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}.png")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Symbology;
    use crate::corpus::symbol;
    use rxing::common::BitMatrix;
    use rxing::qrcode::decoder::BitMatrixParser;

    fn rebuilt_symbol(payload: &str, ec: &str) -> (Reconstructed, Vec<bool>) {
        let spec = symbol::generate(Symbology::QrCode, payload, ec).unwrap();
        let n = spec.truth.width;
        let mut m = BitMatrix::new(n as u32, n as u32).unwrap();
        for y in 0..n {
            for x in 0..n {
                if spec.truth.get(x, y) {
                    m.set(x as u32, y as u32);
                }
            }
        }
        let mut parser = BitMatrixParser::new(m).unwrap();
        let version = parser.readVersion().unwrap();
        let format = parser.readFormatInformation().unwrap();
        let (ec_level, mask) = (format.getErrorCorrectionLevel(), format.getDataMask() as i32);
        let codewords = parser.readCodewords().unwrap();
        let rebuilt = crate::render::from_codewords(&codewords, version, ec_level, mask).unwrap();
        let sampled = spec.truth.modules().to_vec();
        (rebuilt, sampled)
    }

    #[test]
    fn an_undamaged_scan_shows_no_recovered_modules() {
        let (rebuilt, sampled) = rebuilt_symbol("nothing to recover", "M");
        let verdicts = compare(&rebuilt, &sampled).expect("same size");

        assert_eq!(verdicts.len(), rebuilt.dimension * rebuilt.dimension);
        assert_eq!(
            verdicts.iter().filter(|v| v.recovered()).count(),
            0,
            "an undamaged symbol should be entirely blue and green"
        );
        // And both matched colours are present — a symbol is never all one shade.
        assert!(verdicts.contains(&ModuleVerdict::MatchedLight));
        assert!(verdicts.contains(&ModuleVerdict::MatchedDark));
    }

    #[test]
    fn flipped_modules_are_marked_recovered_with_the_true_value() {
        let (rebuilt, mut sampled) = rebuilt_symbol("damage marking", "H");
        // Flip a run through the middle, as a logo would.
        let n = rebuilt.dimension;
        let mid = n / 2;
        for x in (mid - 3)..(mid + 3) {
            sampled[mid * n + x] = !sampled[mid * n + x];
        }

        let verdicts = compare(&rebuilt, &sampled).unwrap();
        assert_eq!(verdicts.iter().filter(|v| v.recovered()).count(), 6);

        for x in (mid - 3)..(mid + 3) {
            let v = verdicts[mid * n + x];
            assert!(v.recovered(), "({x},{mid}) should read as recovered");
            // The colour must carry the TRUE value, not what the camera saw — the point of the
            // view is to show what the module actually is.
            let expected = if rebuilt.get(x, mid) {
                ModuleVerdict::RecoveredDark
            } else {
                ModuleVerdict::RecoveredLight
            };
            assert_eq!(v, expected);
        }
    }

    #[test]
    fn mismatched_sizes_refuse_to_compare() {
        let (rebuilt, _) = rebuilt_symbol("size check", "M");
        assert!(compare(&rebuilt, &[true, false]).is_none());
    }

    #[test]
    fn every_verdict_has_a_distinct_colour() {
        let colours: Vec<_> = [
            ModuleVerdict::MatchedLight,
            ModuleVerdict::MatchedDark,
            ModuleVerdict::RecoveredLight,
            ModuleVerdict::RecoveredDark,
            ModuleVerdict::PlainLight,
            ModuleVerdict::PlainDark,
        ]
        .iter()
        .map(|v| v.rgb())
        .collect();
        for i in 0..colours.len() {
            for j in (i + 1)..colours.len() {
                assert_ne!(colours[i], colours[j], "verdict colours must be tellable apart");
            }
        }
    }

    #[test]
    fn png_is_valid_black_and_white_and_still_scans() {
        let (rebuilt, _) = rebuilt_symbol("https://example.com/png-export", "Q");
        let png = to_png(&rebuilt, Symbology::QrCode, 8, false).expect("encode");

        assert_eq!(&png[1..4], b"PNG", "not a PNG");

        let decoded = image::load_from_memory(&png).expect("decodable").to_luma8();
        let expected = (rebuilt.dimension + 2 * 4) * 8;
        assert_eq!(decoded.dimensions(), (expected as u32, expected as u32));

        // Strictly two-valued: a barcode has no midtones, and any would be a resampling bug.
        assert!(
            decoded.pixels().all(|p| p.0[0] == 0 || p.0[0] == 255),
            "export contains grey pixels"
        );

        // And it has to actually scan, which is the only test that matters to a user.
        let (w, h) = decoded.dimensions();
        let scanned = rxing::helpers::detect_in_luma(decoded.into_raw(), w, h, None)
            .expect("exported PNG must be scannable");
        assert_eq!(scanned.getText(), "https://example.com/png-export");
    }

    #[test]
    fn quiet_zone_matches_each_symbology() {
        assert_eq!(quiet_zone_modules(Symbology::QrCode), 4);
        assert_eq!(quiet_zone_modules(Symbology::Aztec), 4);
        assert_eq!(quiet_zone_modules(Symbology::DataMatrix), 1);
        assert_eq!(quiet_zone_modules(Symbology::Pdf417), 2);
    }

    #[test]
    fn filename_is_the_requested_format() {
        assert_eq!(
            timestamped_name(2016, 8, 10, 14, 33, 48),
            "2016-08-10 14:33:48.png"
        );
        // Zero padding throughout, or files sort wrongly in a gallery.
        assert_eq!(timestamped_name(2026, 1, 2, 3, 4, 5), "2026-01-02 03:04:05.png");
    }

    #[test]
    fn inverted_export_is_the_photographic_negative_and_still_scans() {
        let (rebuilt, _) = rebuilt_symbol("https://example.com/inverted", "Q");
        let normal = to_png(&rebuilt, Symbology::QrCode, 8, false).unwrap();
        let inverted = to_png(&rebuilt, Symbology::QrCode, 8, true).unwrap();

        let n = image::load_from_memory(&normal).unwrap().to_luma8();
        let i = image::load_from_memory(&inverted).unwrap().to_luma8();
        assert_eq!(n.dimensions(), i.dimensions());
        assert!(
            n.pixels().zip(i.pixels()).all(|(a, b)| a.0[0] == 255 - b.0[0]),
            "inverted export is not the exact negative"
        );
        // The quiet zone flips to dark, which is correct for a light-on-dark symbol.
        assert_eq!(i.get_pixel(0, 0).0[0], 0);
    }

    #[test]
    fn verdict_brightness_tracks_module_value() {
        // Green/yellow mark light modules, blue/red dark ones. If this inverts, the grid reads as
        // the symbol's negative and stops being recognisable as the code it came from.
        let luma = |v: ModuleVerdict| {
            let (r, g, b) = v.rgb();
            0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32
        };
        assert!(luma(ModuleVerdict::MatchedLight) > luma(ModuleVerdict::MatchedDark));
        assert!(luma(ModuleVerdict::RecoveredLight) > luma(ModuleVerdict::RecoveredDark));
    }

    #[test]
    fn plain_verdicts_carry_no_claim_of_comparison() {
        // Rendering an uncompared restoration in the matched colours would assert "the scan already
        // had this right" on evidence that does not exist.
        assert!(!ModuleVerdict::PlainLight.compared());
        assert!(!ModuleVerdict::PlainDark.compared());
        assert!(ModuleVerdict::MatchedLight.compared());
        assert!(ModuleVerdict::RecoveredDark.compared());

        // And they render as the symbol itself, black on white.
        assert_eq!(ModuleVerdict::PlainLight.rgb(), (255, 255, 255));
        assert_eq!(ModuleVerdict::PlainDark.rgb(), (0, 0, 0));
    }

    #[test]
    fn comparison_colours_are_fully_saturated_primaries() {
        // Muted colours let the recovered patch blend into the matched field, which is the one
        // distinction this view exists to draw.
        for v in [
            ModuleVerdict::MatchedLight,
            ModuleVerdict::MatchedDark,
            ModuleVerdict::RecoveredLight,
            ModuleVerdict::RecoveredDark,
        ] {
            let (r, g, b) = v.rgb();
            assert!(
                [r, g, b].iter().all(|&c| c == 0 || c == 255),
                "{v:?} is not a primary: {:?}",
                v.rgb()
            );
        }
    }
}
