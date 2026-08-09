//! Camera policy, expressed as pure logic.
//!
//! Nothing here touches Camera2, JNI or any device API. The Kotlin shim reports what lenses exist
//! and what the current frame looked like; this module turns that into something a person can
//! choose from, and is therefore testable on a laptop with no phone attached.
//!
//! It decides nothing. Lens selection is the user's, and [`lens`] exists to make that choice an
//! informed one rather than a guess.

pub mod lens;

pub use lens::{LensOption, LensPicker, LensSpec, LensStatus, PickerParams};
