//! Producing output: the cleaned symbol, and the evidence for it.
//!
//! Two deliverables, and the second is what makes the first trustworthy.
//!
//! # Exact reconstruction
//!
//! The cleaned symbol is re-rendered from the **corrected codeword stream**, not
//! from the decoded payload. The distinction is the whole point and is easy to
//! miss: re-encoding decoded *text* does **not** reproduce the original symbol.
//! Encoders differ in how they segment a payload across numeric, alphanumeric,
//! byte and kanji modes, where they place mode switches, how they pad, and which
//! ECI they declare. Two encoders given identical text routinely emit different
//! codewords, and therefore different symbols.
//!
//! Rebuilding from the corrected codewords sidesteps all of it. Those codewords
//! *are* what the original encoder emitted — Reed-Solomon recovered them
//! exactly, not approximately. Re-applying the original version, ECC level and
//! mask reproduces the original symbol bit for bit, minus the logo.
//!
//! Which is why the test for this is module-by-module identity against the
//! original matrix, not payload equality. Payload equality would pass while
//! silently emitting a differently-segmented symbol.
//!
//! # Verification
//!
//! Every reconstruction is re-decoded through the unmodified path and the
//! payload byte-compared before it is allowed out. A cleaner that emits a
//! plausible-looking symbol carrying corrupted data is worse than one that
//! refuses, because the failure is silent and lands in print.
//!
//! # Inspect overlay
//!
//! The confidence heat map, occlusion mask, erasure positions, which codewords
//! Reed-Solomon actually corrected, and how much budget is left. Reconstruction
//! is an extraordinary claim — that we know what the covered modules *were* —
//! and the overlay is the evidence for it.

pub mod exact;

pub use exact::{Reconstructed, from_codewords};
