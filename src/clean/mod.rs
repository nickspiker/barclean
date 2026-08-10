//! Spending the error-correction budget well.
//!
//! Localizing the occlusion was the hard perception problem. Turning that
//! localization into a decode is an economics problem, and getting the economics
//! wrong throws away the perception work entirely.
//!
//! Reed-Solomon over `n` codewords carrying `k` of data corrects any combination
//! of `t` errors and `e` erasures satisfying:
//!
//! ```text
//! 2t + e <= n - k
//! ```
//!
//! An **error** is unknown in both position and value, and costs two units of
//! budget: one to find where it is, one to fix it. An **erasure** is unknown in
//! value only — the position is given — and costs one. That factor of two is the
//! entire lever barclean pulls.
//!
//! It cuts both ways, which is the part that is easy to get wrong. Marking a
//! genuinely damaged codeword as an erasure converts a 2-unit problem into a
//! 1-unit problem: pure profit. Marking a *healthy* codeword as an erasure
//! spends a unit correcting something that was never broken: pure loss. An
//! over-broad erasure list is worse than no erasure list at all, and a decoder
//! that marks every slightly-suspicious codeword will fail on symbols that plain
//! decoding would have read.
//!
//! So the budget is allocated to the codewords we are most confident are
//! *damaged*, worst first, capped hard, with room optionally held back for the
//! errors we did not see coming.

pub mod any;
pub mod aztec;
pub mod bootstrap;
pub mod datamatrix;
pub mod pdf417;
pub mod erasure;
pub mod qr;

pub use any::{CleanedAny, Fidelity, clean};
pub use bootstrap::{BootstrapOutcome, BootstrapParams, bootstrap};
pub use erasure::{BlockLayout, CodewordProvenance, ErasurePlan};
pub use qr::{CleanError, Cleaned, clean_bitmatrix, clean_luma};
