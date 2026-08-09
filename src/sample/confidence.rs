//! Scoring modules, calibrated against the symbol's own function patterns.
//!
//! # Why calibration, not constants
//!
//! "Variance above 400 means occluded" is meaningless on its own. A crisp 600
//! DPI scan and a handheld phone shot at dusk differ by an order of magnitude in
//! every statistic worth measuring, and a threshold tuned on one is worthless on
//! the other.
//!
//! But every symbol carries its own reference sample. Function patterns —
//! finders, timing, alignment, the QR dark module — have values fixed by the
//! specification, so they are known before any decoding happens. The ones that
//! read *correctly* are, by definition, healthy modules photographed under
//! exactly the conditions in play: this lens, this lighting, this blur, this
//! printer, this paper. They define what "healthy" means for this image, and
//! nothing else can.
//!
//! Function patterns that read *incorrectly* are the other half of the gift:
//! proof of damage at a known location, requiring no inference at all. Those are
//! excluded from calibration (they would poison the reference) and fed straight
//! into the occlusion mask as certain-damage seeds.
//!
//! # The score
//!
//! Three signals, each normalized to `[0, 1]` against the calibrated reference,
//! multiplied:
//!
//! ```text
//! confidence = margin · flatness · neutrality
//! ```
//!
//! Multiplication rather than a weighted sum, because these are vetoes and not
//! votes. A module sitting exactly on the binarization threshold is a coin flip
//! no matter how flat and neutral it is. A module with photographic detail
//! inside it is not a module, however confidently dark it reads. Any one signal
//! going to zero should sink the score, which is what a product does and what a
//! sum specifically does not.

use super::{KnownModule, ModuleStats, StatsGrid};

/// Floors for the calibrated references.
///
/// Without these, a pathologically clean sample calibrates itself into
/// uselessness: if every known-good module has variance 0, then `var_ref` is 0,
/// every real module looks infinitely textured, and confidence collapses to zero
/// everywhere. The floors say "we will not be more discriminating than this
/// regardless of how good the reference looks", and they are in raw 8-bit luma
/// units.
const LUMA_SCALE_FLOOR: f32 = 8.0;
const VAR_REF_FLOOR: f32 = 256.0; // std-dev 16 of 255
const CHROMA_REF_FLOOR: f32 = 24.0;

/// How much worse than the reference a module may be before the corresponding
/// signal reads as fully degraded.
///
/// Applied to the 90th percentile of the known-good sample, so the bar is "four
/// times worse than the worst decile of modules we *know* are fine". Tolerant
/// enough not to flag ordinary print variation, tight enough that photographic
/// content inside a cell has nowhere to hide.
const DEGRADE_MULTIPLE: f32 = 4.0;

/// What this symbol's own healthy modules say a healthy module looks like.
#[derive(Clone, Copy, Debug)]
pub struct Calibration {
    /// Luma distance from threshold at which `margin` saturates to 1.0.
    ///
    /// Taken from the 25th percentile of the known-good sample, so roughly
    /// three quarters of modules that are definitely fine score a full margin.
    /// In a low-contrast photograph this drops automatically and the score stops
    /// punishing the whole symbol for being dim.
    pub luma_scale: f32,
    /// Intra-cell variance at which `flatness` reaches zero.
    pub var_ref: f32,
    /// Chroma magnitude at which `neutrality` reaches zero.
    pub chroma_ref: f32,
    /// Known modules that read as specified. The calibration sample.
    pub known_ok: usize,
    /// Known modules that read *wrong*. Certain damage, no inference needed.
    pub known_mismatched: usize,
}

impl Calibration {
    /// Fraction of function-pattern modules that read incorrectly.
    ///
    /// A useful early triage signal: high mismatch means the damage reaches into
    /// the structural patterns, which is a different and worse situation than a
    /// logo sitting politely in the data region.
    pub fn known_mismatch_rate(&self) -> f32 {
        let total = self.known_ok + self.known_mismatched;
        if total == 0 {
            return 0.0;
        }
        self.known_mismatched as f32 / total as f32
    }

    /// Whether enough healthy function modules survived to calibrate against.
    ///
    /// Below this the references are guesses, and the caller should fall back to
    /// the floors rather than trust a sample of three.
    pub fn is_well_founded(&self) -> bool {
        self.known_ok >= MIN_CALIBRATION_SAMPLE
    }
}

/// Minimum known-good modules for a calibration to mean anything. Every
/// supported symbology clears this comfortably when undamaged — QR's three
/// finder patterns alone contribute 3 × 49 modules.
pub const MIN_CALIBRATION_SAMPLE: usize = 24;

impl Default for Calibration {
    /// The uncalibrated fallback: pure floors, no reference sample.
    fn default() -> Self {
        Self {
            luma_scale: LUMA_SCALE_FLOOR,
            var_ref: VAR_REF_FLOOR,
            chroma_ref: CHROMA_REF_FLOOR,
            known_ok: 0,
            known_mismatched: 0,
        }
    }
}

/// Derive a calibration from the symbol's function-pattern modules.
///
/// Only modules whose measured value matches the specification contribute to the
/// reference — a mismatched module is damaged, and calibrating against damage
/// would define the damage as normal. Mismatches are counted separately and
/// surface as certain-damage seeds via [`certain_damage`].
pub fn calibrate(grid: &StatsGrid, known: &[KnownModule]) -> Calibration {
    let mut margins = Vec::with_capacity(known.len());
    let mut variances = Vec::with_capacity(known.len());
    let mut chromas = Vec::with_capacity(known.len());
    let mut mismatched = 0usize;

    for k in known {
        if k.index >= grid.len() {
            continue;
        }
        let cell = grid.at(k.index);
        if cell.value != k.expected {
            mismatched += 1;
            continue;
        }
        margins.push(cell.margin());
        variances.push(cell.var_luma as f32);
        chromas.push(cell.chroma as f32);
    }

    let known_ok = margins.len();
    if known_ok < MIN_CALIBRATION_SAMPLE {
        // Not enough healthy reference to trust. Fall back to the floors rather
        // than calibrate against a handful of modules, but keep the counts so
        // the caller can see how thin the evidence was.
        return Calibration {
            known_ok,
            known_mismatched: mismatched,
            ..Calibration::default()
        };
    }

    Calibration {
        luma_scale: percentile(&mut margins, 0.25).max(LUMA_SCALE_FLOOR),
        var_ref: (percentile(&mut variances, 0.90) * DEGRADE_MULTIPLE).max(VAR_REF_FLOOR),
        chroma_ref: (percentile(&mut chromas, 0.90) * DEGRADE_MULTIPLE).max(CHROMA_REF_FLOOR),
        known_ok,
        known_mismatched: mismatched,
    }
}

/// Score one module against a calibration. Returns `(0, 1]`.
///
/// The three terms fall off differently, and the difference is deliberate.
///
/// **Margin saturates.** A module three times further from the threshold than
/// the reference is not more readable than one at twice the reference — the
/// binarizer was already certain. Clamping at 1.0 says so.
///
/// **Flatness and neutrality decay rationally**, `ref / (ref + x)`, and never
/// reach zero. A hard clamp would map every module past the reference onto
/// exactly 0.0, and *ordering past the reference is the ordering that matters
/// most*: the erasure budget is finite, so when more codewords are suspect than
/// can be marked, the decision is entirely about which are worst. Ties at zero
/// throw that away and leave the allocator picking arbitrarily among the modules
/// it should be ranking. Rational decay keeps every comparison meaningful, all
/// the way out to the most destroyed module in the symbol.
pub fn score(cell: ModuleStats, cal: &Calibration) -> f32 {
    let margin = (cell.margin() / cal.luma_scale).clamp(0.0, 1.0);
    let flatness = cal.var_ref / (cal.var_ref + cell.var_luma as f32);
    let neutrality = cal.chroma_ref / (cal.chroma_ref + cell.chroma as f32);

    margin * flatness * neutrality
}

/// Per-module confidence, row-major, parallel to the [`StatsGrid`] it came from.
#[derive(Clone, Debug)]
pub struct ConfidenceGrid {
    pub width: usize,
    pub height: usize,
    pub calibration: Calibration,
    scores: Vec<f32>,
}

impl ConfidenceGrid {
    pub fn get(&self, x: usize, y: usize) -> f32 {
        self.scores[y * self.width + x]
    }

    pub fn at(&self, index: usize) -> f32 {
        self.scores[index]
    }

    pub fn scores(&self) -> &[f32] {
        &self.scores
    }

    pub fn len(&self) -> usize {
        self.scores.len()
    }

    pub fn is_empty(&self) -> bool {
        self.scores.is_empty()
    }

    /// The `n` least confident module indices, worst first.
    ///
    /// Drives both the erasure budget (which codewords to spend correction
    /// capacity on) and Chase-II (which modules are worth flipping).
    pub fn weakest(&self, n: usize) -> Vec<usize> {
        let mut idx: Vec<usize> = (0..self.scores.len()).collect();
        idx.sort_by(|&a, &b| {
            self.scores[a]
                .partial_cmp(&self.scores[b])
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        idx.truncate(n);
        idx
    }
}

/// Score an entire grid against its own function patterns.
pub fn evaluate(grid: &StatsGrid, known: &[KnownModule]) -> ConfidenceGrid {
    let calibration = calibrate(grid, known);
    let mut scores: Vec<f32> = grid.cells().iter().map(|&c| score(c, &calibration)).collect();

    // Function modules that read wrong are damaged as a matter of fact, not of
    // inference. Whatever the three signals concluded, override to zero.
    for k in known {
        if k.index < scores.len() && grid.at(k.index).value != k.expected {
            scores[k.index] = 0.0;
        }
    }

    ConfidenceGrid {
        width: grid.width,
        height: grid.height,
        calibration,
        scores,
    }
}

/// Indices of function-pattern modules that read incorrectly.
///
/// These seed the occlusion mask with zero false-positive risk: the
/// specification says what belongs there, and the image disagrees.
pub fn certain_damage(grid: &StatsGrid, known: &[KnownModule]) -> Vec<usize> {
    known
        .iter()
        .filter(|k| k.index < grid.len() && grid.at(k.index).value != k.expected)
        .map(|k| k.index)
        .collect()
}

/// Linear-interpolated percentile. Sorts `values` in place.
fn percentile(values: &mut [f32], p: f32) -> f32 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    if values.len() == 1 {
        return values[0];
    }
    let pos = p.clamp(0.0, 1.0) * (values.len() - 1) as f32;
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    if lo == hi {
        return values[lo];
    }
    let frac = pos - lo as f32;
    values[lo] * (1.0 - frac) + values[hi] * frac
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(mean_luma: u8, var_luma: u16, chroma: u8) -> ModuleStats {
        ModuleStats {
            mean_luma,
            var_luma,
            chroma,
            threshold: 128,
            value: mean_luma < 128,
        }
    }

    /// A grid of clean, high-contrast, flat, neutral modules.
    fn clean_grid(n: usize) -> StatsGrid {
        let cells = (0..n * n)
            .map(|i| if i % 2 == 0 { stats(20, 4, 1) } else { stats(235, 4, 1) })
            .collect();
        StatsGrid::new(n, n, cells)
    }

    fn all_known(grid: &StatsGrid) -> Vec<KnownModule> {
        (0..grid.len())
            .map(|i| KnownModule::new(i, grid.at(i).value))
            .collect()
    }

    #[test]
    fn percentile_interpolates() {
        let mut v = vec![0.0, 10.0, 20.0, 30.0, 40.0];
        assert_eq!(percentile(&mut v.clone(), 0.0), 0.0);
        assert_eq!(percentile(&mut v.clone(), 1.0), 40.0);
        assert_eq!(percentile(&mut v.clone(), 0.5), 20.0);
        // 0.25 * 4 = 1.0 exactly -> element 1
        assert_eq!(percentile(&mut v, 0.25), 10.0);
    }

    #[test]
    fn percentile_handles_degenerate_input() {
        assert_eq!(percentile(&mut [], 0.5), 0.0);
        assert_eq!(percentile(&mut [7.0], 0.9), 7.0);
    }

    #[test]
    fn clean_modules_score_high() {
        let grid = clean_grid(10);
        let conf = evaluate(&grid, &all_known(&grid));

        assert!(conf.calibration.is_well_founded());
        for i in 0..conf.len() {
            // Not 1.0: the reference floors levy a small uniform tax even on a
            // pristine module (var 4 against a 256 floor, chroma 1 against 24).
            // That is fine and intended — what matters is the enormous gap to
            // the occlusion cut at 0.25, not proximity to a perfect score.
            assert!(
                conf.at(i) > 0.9,
                "clean module {i} scored {}, expected well above the 0.25 cut",
                conf.at(i)
            );
        }
    }

    #[test]
    fn textured_module_scores_low_despite_strong_margin() {
        // The signal that matters most: this cell reads *confidently* dark, and
        // is neutral in colour. Only the intra-cell variance betrays it. A sum
        // would average that away; the product must not.
        let mut cells: Vec<ModuleStats> = (0..100)
            .map(|i| if i % 2 == 0 { stats(20, 4, 1) } else { stats(235, 4, 1) })
            .collect();
        cells[55] = stats(20, 9000, 1);
        let grid = StatsGrid::new(10, 10, cells);
        let known: Vec<KnownModule> = (0..100)
            .filter(|&i| i != 55)
            .map(|i| KnownModule::new(i, grid.at(i).value))
            .collect();

        let conf = evaluate(&grid, &known);
        assert!(
            conf.at(55) < 0.05,
            "textured module scored {}, should be near zero",
            conf.at(55)
        );
    }

    #[test]
    fn chromatic_module_scores_low_despite_strong_margin() {
        let mut cells: Vec<ModuleStats> = (0..100)
            .map(|i| if i % 2 == 0 { stats(20, 4, 1) } else { stats(235, 4, 1) })
            .collect();
        cells[42] = stats(20, 4, 200);
        let grid = StatsGrid::new(10, 10, cells);
        let known: Vec<KnownModule> = (0..100)
            .filter(|&i| i != 42)
            .map(|i| KnownModule::new(i, grid.at(i).value))
            .collect();

        let conf = evaluate(&grid, &known);
        // Comfortably under the 0.25 occlusion cut, which is the property that
        // matters. Rational decay deliberately stops short of zero so this stays
        // rankable against modules that are worse still.
        assert!(
            conf.at(42) < 0.15,
            "coloured module scored {}, should fall well below the cut",
            conf.at(42)
        );
    }

    #[test]
    fn ambiguous_module_scores_low() {
        let mut cells: Vec<ModuleStats> = (0..100)
            .map(|i| if i % 2 == 0 { stats(20, 4, 1) } else { stats(235, 4, 1) })
            .collect();
        cells[13] = stats(127, 4, 1); // one luma unit off the threshold
        let grid = StatsGrid::new(10, 10, cells);
        let known: Vec<KnownModule> = (0..100)
            .filter(|&i| i != 13)
            .map(|i| KnownModule::new(i, grid.at(i).value))
            .collect();

        let conf = evaluate(&grid, &known);
        assert!(
            conf.at(13) < 0.2,
            "threshold-straddling module scored {}",
            conf.at(13)
        );
    }

    #[test]
    fn calibration_adapts_to_low_contrast() {
        // Same symbol photographed dimly: margins of 8 rather than 108. The
        // scores must not collapse just because the whole image is flat.
        let cells = (0..100)
            .map(|i| if i % 2 == 0 { stats(120, 4, 1) } else { stats(136, 4, 1) })
            .collect();
        let grid = StatsGrid::new(10, 10, cells);
        let conf = evaluate(&grid, &all_known(&grid));

        assert!(
            conf.at(0) > 0.9,
            "low-contrast but internally consistent module scored {}",
            conf.at(0)
        );
    }

    #[test]
    fn damaged_function_modules_are_excluded_from_calibration() {
        // Half the function patterns are wrecked. The calibration must be built
        // from the survivors, and the wrecked ones must not drag the reference
        // toward calling damage normal.
        let mut cells: Vec<ModuleStats> = (0..100).map(|_| stats(20, 4, 1)).collect();
        for cell in cells.iter_mut().take(30) {
            *cell = stats(235, 9000, 180); // wrecked: reads light where dark expected
        }
        let grid = StatsGrid::new(10, 10, cells);
        let known: Vec<KnownModule> = (0..100).map(|i| KnownModule::new(i, true)).collect();

        let cal = calibrate(&grid, &known);
        assert_eq!(cal.known_mismatched, 30);
        assert_eq!(cal.known_ok, 70);
        assert!((cal.known_mismatch_rate() - 0.3).abs() < 1e-6);
        // Reference came from the 70 healthy modules, so it stays tight.
        assert_eq!(cal.var_ref, VAR_REF_FLOOR);
    }

    #[test]
    fn mismatched_function_modules_are_forced_to_zero() {
        // A module that reads wrong where the spec fixes its value is damaged as
        // a matter of fact. It must score zero even when every measured signal
        // looks pristine.
        let cells: Vec<ModuleStats> = (0..100).map(|_| stats(20, 4, 1)).collect();
        let grid = StatsGrid::new(10, 10, cells);
        let mut known: Vec<KnownModule> = (0..100).map(|i| KnownModule::new(i, true)).collect();
        known[7] = KnownModule::new(7, false); // spec says light, image reads dark

        let conf = evaluate(&grid, &known);
        assert_eq!(conf.at(7), 0.0, "contradicted function module must score 0");
        assert!(conf.at(8) > 0.9, "its neighbour is unaffected");
        assert_eq!(certain_damage(&grid, &known), vec![7]);
    }

    #[test]
    fn thin_reference_falls_back_to_floors() {
        let grid = clean_grid(10);
        let known: Vec<KnownModule> = (0..4).map(|i| KnownModule::new(i, grid.at(i).value)).collect();

        let cal = calibrate(&grid, &known);
        assert!(!cal.is_well_founded());
        assert_eq!(cal.luma_scale, LUMA_SCALE_FLOOR);
        assert_eq!(cal.var_ref, VAR_REF_FLOOR);
        assert_eq!(cal.chroma_ref, CHROMA_REF_FLOOR);
    }

    #[test]
    fn weakest_returns_worst_first() {
        let mut cells: Vec<ModuleStats> = (0..100).map(|_| stats(20, 4, 1)).collect();
        cells[10] = stats(127, 4, 1); // worst
        cells[20] = stats(20, 500, 1); // second worst
        let grid = StatsGrid::new(10, 10, cells);
        let known: Vec<KnownModule> = (0..100)
            .filter(|&i| i != 10 && i != 20)
            .map(|i| KnownModule::new(i, true))
            .collect();

        let conf = evaluate(&grid, &known);
        let weakest = conf.weakest(2);
        assert_eq!(weakest.len(), 2);
        assert_eq!(weakest[0], 10);
        assert_eq!(weakest[1], 20);
    }
}
