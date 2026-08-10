//! barclean — occlusion-aware 2D barcode cleaning.
//!
//! # The problem
//!
//! People paste logos over the middle of QR / Aztec / PDF417 / DataMatrix
//! symbols and let Reed-Solomon absorb the damage. It generally works, and it
//! is also why so many branded codes fail in the wild: the entire error budget
//! is spent on self-inflicted damage, leaving nothing for glare, creases, poor
//! printing or a bad angle.
//!
//! # The lever
//!
//! Reed-Solomon corrects `t` **errors** (unknown position, unknown value) where
//! `2t <= n-k`, but `e` **erasures** (known position, unknown value) where
//! `e <= n-k`. Knowing *where* the damage is worth is exactly twice the
//! correction power. Mainstream decoders never exploit this on defaced symbols
//! because they binarize before detection: by the time anything knows where a
//! module sits, the greyscale and colour that would have revealed the logo are
//! gone, and the occluded modules arrive as confident-but-wrong bits.
//!
//! barclean keeps the source pixels alive all the way to the sampler, measures
//! each module, localizes the occlusion, decodes it as erasures, and re-renders
//! the original symbol from the corrected codewords.
//!
//! # Pipeline
//!
//! ```text
//!   image (luma + chroma)
//!         |
//!         +--> binarize --> BitMatrix --> detect --> PerspectiveTransform
//!         |                                                |
//!         +----------------- source -----------------------+
//!                                                          |
//!                                        [sample]  per-module ModuleStats
//!                                                          |
//!                                    [confidence]  calibrated against the
//!                                                  symbol's own function
//!                                                  patterns
//!                                                          |
//!                                     [occlusion]  morphology -> blob mask
//!                                                          |
//!                                       [erasure]  modules -> codewords,
//!                                                  budget-capped per block
//!                                                          |
//!                                      [escalate]  plain -> erasure -> sweep
//!                                                  -> Chase-II -> multi-frame
//!                                                          |
//!                                        [render]  exact rebuild from the
//!                                                  corrected codewords
//! ```
//!
//! # Module layout
//!
//! - [`sample`] — turning measured module statistics into calibrated confidence,
//!   and confidence into an occlusion mask. Pure policy; no image decoding.
//! - [`clean`] — mapping the occlusion mask onto codeword erasure positions
//!   under the per-block correction budget, and the escalation ladder.
//! - [`render`] — exact reconstruction from corrected codewords, and the
//!   inspect overlay.
//! - [`camera`] — auto lens-selection policy, expressed as pure logic so it is
//!   testable without a device.
//! - [`corpus`] — synthetic symbol generation, logo compositing and degradation
//!   for the grading harness.
//!
//! The measurement layer itself lives in the forked rxing at
//! `/Users/nick/Code/rxing` (branch `barclean`): it carries the luminance and
//! chroma source into the grid sampler and adds erasure-aware Reed-Solomon.
//! That fork deliberately holds **no policy** — it reports what the pixels said
//! and leaves every judgement call here, where the grading harness can iterate
//! on it without recompiling a dependency.

// The UI layer needs fluor, so it rides the `gui` feature. The core (sample / clean / render /
// corpus) never touches it, which is what keeps `--no-default-features` a fast algorithm-only test
// cycle with no windowing toolchain in the graph.
#[cfg(feature = "gui")]
pub mod app;
pub mod camera;
#[cfg(feature = "gui")]
pub mod ui;

#[cfg(all(target_os = "android", feature = "gui"))]
pub mod jni;
pub mod clean;
pub mod corpus;
pub mod feed;
pub mod render;
pub mod sample;

/// The four symbologies barclean cleans.
///
/// Restricted to 2D on purpose: these are the formats that carry enough error
/// correction for erasure recovery to buy anything, and the ones people
/// actually deface. 1D symbols have thin ECC and rarely wear a centre logo.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Symbology {
    QrCode,
    Aztec,
    Pdf417,
    DataMatrix,
}

impl Symbology {
    pub const ALL: [Symbology; 4] = [
        Symbology::QrCode,
        Symbology::Aztec,
        Symbology::Pdf417,
        Symbology::DataMatrix,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Symbology::QrCode => "QR Code",
            Symbology::Aztec => "Aztec",
            Symbology::Pdf417 => "PDF417",
            Symbology::DataMatrix => "DataMatrix",
        }
    }

    /// Whether a centre occlusion threatens *detection* rather than merely
    /// correction.
    ///
    /// Aztec is the odd one out: its finder **is** the centre bullseye, so a
    /// logo over the middle removes the thing the detector locks onto. No
    /// amount of erasure decoding helps a symbol that was never located. QR
    /// keeps its three corner finders, DataMatrix its L-shaped edge finder, and
    /// PDF417 its per-row start/stop patterns — all untouched by a centre logo.
    pub fn centre_occlusion_breaks_detection(self) -> bool {
        matches!(self, Symbology::Aztec)
    }
}
