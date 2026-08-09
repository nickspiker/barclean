//! Synthetic symbols with known ground truth, for grading.
//!
//! The premise of this project is a measurable claim: that marking a logo as
//! erasures recovers symbols that plain decoding cannot. A claim like that is
//! worth exactly as much as the harness that tests it, so the corpus comes
//! before the decoder that depends on it.
//!
//! Generation is end-to-end and keeps the **pristine module matrix**, not just a
//! rendered PNG. That matters: reconstruction is judged by module-by-module
//! identity against the original symbol, and re-deriving ground truth by
//! decoding a clean render would only prove the payload survived, not that the
//! symbol was rebuilt exactly. Segmentation drift would slip straight through.

pub mod degrade;
pub mod logo;
pub mod symbol;

pub use degrade::{Degradation, degrade};
pub use logo::{Logo, LogoKind, composite};
pub use symbol::{Specimen, TruthMatrix, render};
