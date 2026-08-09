//! Validating the experimental setup before anything is built on it.
//!
//! Phase 2 measures erasure decoding against plain decoding. That comparison is
//! only meaningful if the corpus actually contains cases plain decoding fails
//! on. If stock rxing reads every specimen we generate, an erasure decoder would
//! show a zero-point improvement and we would learn nothing — not because the
//! technique does not work, but because the test was too easy.
//!
//! So: establish the failure boundary first. These tests pin down where stock
//! decoding stops working, which is the baseline every later number is measured
//! against.

use barclean::Symbology;
use barclean::corpus::{Degradation, Logo, LogoKind, degrade, symbol};
use image::RgbImage;

/// Decode with the stock, unmodified path — the baseline barclean has to beat.
fn stock_decode(img: &RgbImage) -> Option<String> {
    let luma: Vec<u8> = img
        .pixels()
        .map(|p| {
            // Rec. 601 luma, which is what the phone pipeline hands us.
            let [r, g, b] = p.0;
            ((r as u32 * 299 + g as u32 * 587 + b as u32 * 114) / 1000) as u8
        })
        .collect();

    rxing::helpers::detect_in_luma(
        luma,
        img.width(),
        img.height(),
        Some(rxing::BarcodeFormat::QR_CODE),
    )
    .ok()
    .map(|r| r.getText().to_string())
}

/// Build one specimen: encode, render, occlude, degrade.
fn specimen(payload: &str, ec: &str, kind: LogoKind, area: f32, deg: &Degradation) -> RgbImage {
    let spec = symbol::generate(Symbology::QrCode, payload, ec).expect("generate");
    let mut img = symbol::render(&spec, 8, 4);
    if area > 0.0 {
        barclean::corpus::composite(&mut img, &Logo::new(kind, area));
    }
    degrade(&img, deg)
}

const PAYLOAD: &str = "https://example.com/barclean/corpus/specimen";

#[test]
fn clean_specimens_decode() {
    // The control. If this fails, every other result in the suite is noise.
    let img = specimen(PAYLOAD, "M", LogoKind::FlatColour, 0.0, &Degradation::PRISTINE);
    assert_eq!(
        stock_decode(&img).as_deref(),
        Some(PAYLOAD),
        "an undamaged render must decode, or the corpus itself is broken"
    );
}

#[test]
fn realistic_capture_degradation_alone_does_not_break_decoding() {
    // Blur, noise and JPEG must not be what defeats the decoder — otherwise
    // phase 2 would be measuring image quality, not occlusion recovery.
    for (name, deg) in [
        ("good", Degradation::GOOD_CAPTURE),
        ("poor", Degradation::POOR_CAPTURE),
    ] {
        let img = specimen(PAYLOAD, "M", LogoKind::FlatColour, 0.0, &deg);
        assert_eq!(
            stock_decode(&img).as_deref(),
            Some(PAYLOAD),
            "{name} capture degradation alone should not defeat decoding"
        );
    }
}

#[test]
fn small_occlusions_are_absorbed_by_plain_error_correction() {
    // The lower end. Reed-Solomon handles this unaided, so barclean should add
    // nothing here — and any later claim of improvement in this band would be a
    // measurement artifact.
    let img = specimen(
        PAYLOAD,
        "H",
        LogoKind::FlatColour,
        0.02,
        &Degradation::PRISTINE,
    );
    assert_eq!(
        stock_decode(&img).as_deref(),
        Some(PAYLOAD),
        "a 2% occlusion at ECC-H is well inside the plain error budget"
    );
}

/// The finding that matters: where stock decoding actually gives up.
///
/// Printed rather than asserted at a fixed threshold, because the exact
/// crossover depends on ECC level, symbol version and where the occlusion lands
/// relative to the block interleave. Pinning it to a constant would make this
/// test brittle without making it more informative. What *is* asserted is the
/// qualitative shape every ECC level must show: decodable when clean, not
/// decodable when heavily occluded.
#[test]
fn locates_the_plain_decoding_failure_boundary() {
    let areas = [0.0f32, 0.05, 0.10, 0.15, 0.20, 0.25, 0.30, 0.35];

    for ec in ["L", "M", "Q", "H"] {
        let mut outcomes = Vec::new();
        for &area in &areas {
            let img = specimen(
                PAYLOAD,
                ec,
                LogoKind::FlatColour,
                area,
                &Degradation::GOOD_CAPTURE,
            );
            outcomes.push((area, stock_decode(&img).as_deref() == Some(PAYLOAD)));
        }

        let summary: Vec<String> = outcomes
            .iter()
            .map(|(a, ok)| format!("{:.0}%:{}", a * 100.0, if *ok { "ok" } else { "--" }))
            .collect();
        println!("ECC-{ec}  {}", summary.join("  "));

        assert!(outcomes[0].1, "ECC-{ec} must decode when unoccluded");
        assert!(
            !outcomes.last().unwrap().1,
            "ECC-{ec} still decoded at 35% occlusion — the corpus is too easy \
             here and phase 2 would have no headroom to demonstrate anything"
        );
    }
}

/// The occlusion kinds are not interchangeable, and the corpus must contain the
/// hard one.
#[test]
fn every_occlusion_kind_can_defeat_plain_decoding() {
    for kind in [LogoKind::FlatColour, LogoKind::Textured, LogoKind::NeutralFlat] {
        let img = specimen(PAYLOAD, "M", kind, 0.30, &Degradation::GOOD_CAPTURE);
        assert!(
            stock_decode(&img).as_deref() != Some(PAYLOAD),
            "{kind:?} at 30% should defeat plain decoding, giving erasure \
             decoding something to actually recover"
        );
    }
}
