//! Module measurement, calibration and occlusion localization.
//!
//! This module holds every judgement call barclean makes about *what the pixels
//! meant*. The forked sampler upstream of it reports only what it measured; the
//! split is deliberate, so tuning happens here where the grading harness can
//! iterate without rebuilding a dependency.
//!
//! Three stages:
//!
//! 1. [`ModuleStats`] arrive from the sampler — one measurement per module.
//! 2. [`confidence`] calibrates against the symbol's own function patterns and
//!    scores every module.
//! 3. [`occlusion`] turns low-confidence scatter into a blob mask, because
//!    logos are blobs and noise is not.

pub mod confidence;
pub mod occlusion;

/// What the pixels said about one module cell.
///
/// Produced by the forked grid sampler, which reads an N×N pattern inside each
/// module's projected quad rather than upstream rxing's single centre pixel.
/// Deliberately policy-free: no thresholds, no normalization, no opinion about
/// whether the module is damaged. Scoring happens in [`confidence`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModuleStats {
    /// Mean luminance over the cell interior (inset from the edges, so
    /// neighbouring modules and print bleed do not leak in).
    pub mean_luma: u8,
    /// Luminance variance within the cell, capped at `u16::MAX`.
    ///
    /// The single most useful occlusion signal. A printed module is flat by
    /// construction — it is one ink decision. Photographic or illustrated logo
    /// content has structure *inside* a single module cell, which nothing in a
    /// legitimate symbol ever does.
    pub var_luma: u16,
    /// Mean chroma magnitude over the cell — distance from the neutral axis.
    ///
    /// Code modules are neutral by construction. Logos are usually not, and on
    /// Android this signal is free: `YUV_420_888` hands us the U and V planes
    /// already separated.
    pub chroma: u8,
    /// The binarization threshold in force at this cell's location. Local, not
    /// global — a hybrid binarizer's threshold varies across the image, and the
    /// margin is only meaningful against the threshold that actually applied.
    pub threshold: u8,
    /// The bit the binarizer settled on. `true` is dark, matching the
    /// convention that a set module is a 1.
    pub value: bool,
}

impl ModuleStats {
    /// Signed distance from the threshold. Positive means comfortably dark,
    /// negative comfortably light; near zero means the binarizer guessed.
    pub fn signed_margin(self) -> f32 {
        self.threshold as f32 - self.mean_luma as f32
    }

    /// Unsigned distance from the threshold, in luma units.
    pub fn margin(self) -> f32 {
        self.signed_margin().abs()
    }
}

/// A symbol-sized grid of module measurements, row-major.
#[derive(Clone, Debug)]
pub struct StatsGrid {
    pub width: usize,
    pub height: usize,
    cells: Vec<ModuleStats>,
}

impl StatsGrid {
    /// Wrap a row-major measurement vector.
    ///
    /// # Panics
    /// If `cells.len() != width * height`.
    pub fn new(width: usize, height: usize, cells: Vec<ModuleStats>) -> Self {
        assert_eq!(
            cells.len(),
            width * height,
            "StatsGrid: {width}x{height} needs {} cells, got {}",
            width * height,
            cells.len()
        );
        Self {
            width,
            height,
            cells,
        }
    }

    pub fn len(&self) -> usize {
        self.cells.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    pub fn get(&self, x: usize, y: usize) -> ModuleStats {
        self.cells[y * self.width + x]
    }

    pub fn at(&self, index: usize) -> ModuleStats {
        self.cells[index]
    }

    pub fn cells(&self) -> &[ModuleStats] {
        &self.cells
    }

    /// The decided bit values, in the layout a decoder expects.
    pub fn values(&self) -> Vec<bool> {
        self.cells.iter().map(|c| c.value).collect()
    }
}

/// A module whose value is fixed by the specification, and therefore known
/// before any decoding happens.
///
/// These are barclean's ground truth. Every symbology has them — QR's three
/// finder patterns, timing patterns, alignment patterns and dark module;
/// DataMatrix's L-shaped finder and alternating timing edges; Aztec's bullseye;
/// PDF417's per-row start and stop patterns. They serve two purposes at once:
///
/// - **Calibration.** The ones that read correctly show what a healthy module
///   looks like *in this photograph* — this lighting, this blur, this printer.
///   No universal constant can do that.
/// - **Certain damage.** The ones that read *incorrectly* are proof of damage at
///   a known location, free of any inference.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KnownModule {
    /// Row-major index into the [`StatsGrid`].
    pub index: usize,
    /// The value the specification requires here.
    pub expected: bool,
}

impl KnownModule {
    pub fn new(index: usize, expected: bool) -> Self {
        Self { index, expected }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn stats(mean_luma: u8, var_luma: u16, chroma: u8, threshold: u8) -> ModuleStats {
        ModuleStats {
            mean_luma,
            var_luma,
            chroma,
            threshold,
            value: mean_luma < threshold,
        }
    }

    #[test]
    fn margin_is_distance_from_threshold() {
        let dark = stats(20, 0, 0, 128);
        let light = stats(230, 0, 0, 128);
        let ambiguous = stats(126, 0, 0, 128);

        assert_eq!(dark.margin(), 108.0);
        assert_eq!(light.margin(), 102.0);
        assert_eq!(ambiguous.margin(), 2.0);

        // Sign carries which side of the threshold, magnitude carries how sure.
        assert!(dark.signed_margin() > 0.0, "dark reads above threshold");
        assert!(light.signed_margin() < 0.0, "light reads below threshold");
    }

    #[test]
    fn grid_indexing_is_row_major() {
        let cells = (0..6).map(|i| stats(i * 10, 0, 0, 128)).collect();
        let grid = StatsGrid::new(3, 2, cells);

        assert_eq!(grid.get(0, 0).mean_luma, 0);
        assert_eq!(grid.get(2, 0).mean_luma, 20);
        assert_eq!(grid.get(0, 1).mean_luma, 30);
        assert_eq!(grid.get(2, 1).mean_luma, 50);
        assert_eq!(grid.at(4), grid.get(1, 1));
    }

    #[test]
    #[should_panic(expected = "needs 6 cells, got 5")]
    fn grid_rejects_wrong_cell_count() {
        let cells = (0..5).map(|_| stats(0, 0, 0, 128)).collect();
        StatsGrid::new(3, 2, cells);
    }
}
