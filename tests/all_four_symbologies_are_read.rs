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
