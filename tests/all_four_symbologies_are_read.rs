//! Every supported symbology must at least be *read*, even where cleaning is not wired yet.
//!
//! barclean's bootstrap path is QR-only so far. Aztec, PDF417 and DataMatrix share the erasure-aware
//! Reed-Solomon layer but not the QR-specific block/provenance machinery, so they fall through to a
//! stock multi-format decode. That fallback is what stops the app from silently reporting "no
//! symbol" while staring straight at a perfectly good Aztec code — a failure indistinguishable,
//! from behind the viewfinder, from the camera being broken.
//!
//! These tests go through the same `clean_luma` entry point the camera path calls.

use barclean::clean::clean_luma;
use rxing::{BarcodeFormat, EncodeHints, MultiFormatWriter, Writer};

/// Encode a symbol and render it to a luminance buffer at `scale` pixels per module.
fn render_luma(format: BarcodeFormat, payload: &str, scale: u32) -> (Vec<u8>, u32, u32) {
    let matrix = MultiFormatWriter
        .encode_with_hints(payload, &format, 0, 0, &EncodeHints::default())
        .unwrap_or_else(|e| panic!("{format:?} encode failed: {e}"));

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

const PAYLOAD: &str = "barclean symbology coverage 0123456789";

#[test]
fn qr_goes_through_the_cleaner() {
    let (luma, w, h) = render_luma(BarcodeFormat::QR_CODE, PAYLOAD, 6);
    let cleaned = clean_luma(&luma, w, h).expect("QR must decode through the cleaning path");
    assert_eq!(cleaned.payload, PAYLOAD);
    assert!(cleaned.blocks_total > 0);
}

#[test]
fn aztec_is_read_even_though_cleaning_is_not_wired() {
    // The case reported from the device: pointing at a clean Aztec code and getting nothing,
    // because the QR-only detector could never match it.
    let (luma, w, h) = render_luma(BarcodeFormat::AZTEC, PAYLOAD, 8);
    match clean_luma(&luma, w, h) {
        Ok(c) => panic!("unexpected: Aztec went through the QR cleaner as {:?}", c.payload),
        Err(e) => {
            // The cleaner declines it, which is correct — the fallback in BarcleanApp is what
            // reads it. Verify that fallback's underlying call directly.
            let stock = rxing::helpers::detect_in_luma(luma, w, h, None)
                .unwrap_or_else(|_| panic!("stock decode must read a clean Aztec (cleaner said {e})"));
            assert_eq!(stock.getText(), PAYLOAD);
            assert_eq!(*stock.getBarcodeFormat(), BarcodeFormat::AZTEC);
        }
    }
}

#[test]
fn datamatrix_and_pdf417_are_read_by_the_fallback() {
    for (format, scale) in [
        (BarcodeFormat::DATA_MATRIX, 8u32),
        (BarcodeFormat::PDF_417, 6),
    ] {
        let (luma, w, h) = render_luma(format, PAYLOAD, scale);
        let stock = rxing::helpers::detect_in_luma(luma, w, h, None)
            .unwrap_or_else(|e| panic!("{format:?} must be readable: {e}"));
        assert_eq!(stock.getText(), PAYLOAD, "{format:?} payload mismatch");
    }
}

/// A light-on-dark symbol must decode, and its export must come back in the same polarity.
///
/// Codes are printed inverted all the time — signage, dark packaging, dark-mode screens. The
/// binarizer marks the *background* as dark in that case, so the detector finds nothing at all
/// unless the image is retried inverted.
#[test]
fn inverted_source_decodes_and_round_trips_polarity() {
    let (mut luma, w, h) = render_luma(BarcodeFormat::QR_CODE, PAYLOAD, 6);
    for p in luma.iter_mut() {
        *p = 255 - *p;
    }

    let cleaned = clean_luma(&luma, w, h).expect("a light-on-dark QR must still decode");
    assert_eq!(cleaned.payload, PAYLOAD);
    assert!(
        cleaned.source_inverted,
        "the inverted path decoded it, so it must be recorded as inverted"
    );

    // The export preserves polarity: restored in the form it was found in.
    let rebuilt = cleaned.reconstruct().expect("reconstruct");
    let png = barclean::render::to_png(&rebuilt, barclean::Symbology::QrCode, 8, cleaned.source_inverted)
        .expect("encode");
    let img = image::load_from_memory(&png).unwrap().to_luma8();
    assert_eq!(
        img.get_pixel(0, 0).0[0],
        0,
        "quiet zone should be dark for a light-on-dark restoration"
    );

    // And it round-trips through barclean's own reader.
    //
    // Verified through `clean_luma` rather than the stock decoder on purpose: stock rxing does not
    // try an inverted read either, so it cannot scan this file. That is the cost of preserving
    // polarity — the restoration matches the original in situ, but a reader without inversion
    // support will not read it, exactly as it would not have read the original.
    let (iw, ih) = img.dimensions();
    let rescanned = clean_luma(&img.into_raw(), iw, ih).expect("inverted export must round-trip");
    assert_eq!(rescanned.payload, PAYLOAD);
    assert!(rescanned.source_inverted, "the export is light-on-dark");
}

/// The same symbol exported upright is readable by any stock decoder.
///
/// Together with the test above this pins the trade-off: upright exports are universally scannable,
/// inverted ones match their original but need an inversion-aware reader.
#[test]
fn upright_export_is_readable_by_a_stock_decoder() {
    let (mut luma, w, h) = render_luma(BarcodeFormat::QR_CODE, PAYLOAD, 6);
    for p in luma.iter_mut() {
        *p = 255 - *p;
    }
    let cleaned = clean_luma(&luma, w, h).expect("decode");
    let rebuilt = cleaned.reconstruct().unwrap();

    // Same recovered symbol, exported upright instead.
    let png = barclean::render::to_png(&rebuilt, barclean::Symbology::QrCode, 8, false).unwrap();
    let img = image::load_from_memory(&png).unwrap().to_luma8();
    let (iw, ih) = img.dimensions();
    let scanned = rxing::helpers::detect_in_luma(img.into_raw(), iw, ih, None)
        .expect("an upright export must scan anywhere");
    assert_eq!(scanned.getText(), PAYLOAD);
}

#[test]
fn upright_sources_are_not_reported_as_inverted() {
    let (luma, w, h) = render_luma(BarcodeFormat::QR_CODE, PAYLOAD, 6);
    let cleaned = clean_luma(&luma, w, h).expect("decode");
    assert!(!cleaned.source_inverted);
}
