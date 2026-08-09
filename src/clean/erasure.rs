//! Mapping module confidence onto codeword erasure positions.
//!
//! Two conversions happen here, and both are places where information is easy to
//! lose:
//!
//! 1. **Modules to codewords.** A codeword is spread across several modules —
//!    8 for QR and DataMatrix, 17 for PDF417, and a layer-dependent count for
//!    Aztec — and those modules are *not* contiguous. A codeword's confidence is
//!    the **minimum** over its modules, not the mean: a codeword with one module
//!    under a logo is a broken codeword, and averaging that away with seven
//!    healthy neighbours is precisely how the signal gets destroyed.
//!
//! 2. **Codewords to a budget.** Each error-correction block has its own
//!    independent capacity. Erasures must be allocated per block, worst first,
//!    and capped, because spending capacity on healthy codewords is a net loss.

/// One error-correction block's geometry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockLayout {
    /// Total codewords in the block, `n`.
    pub total: usize,
    /// Data codewords, `k`. The remainder is error correction.
    pub data: usize,
}

impl BlockLayout {
    pub fn new(total: usize, data: usize) -> Self {
        debug_assert!(data <= total, "data codewords cannot exceed block size");
        Self { total, data }
    }

    /// Error-correction codewords, `n - k`. The full erasure budget.
    pub fn ecc(&self) -> usize {
        self.total.saturating_sub(self.data)
    }

    /// Errors correctable with no erasures marked: `floor((n-k)/2)`.
    pub fn max_errors(&self) -> usize {
        self.ecc() / 2
    }
}

/// Which modules produced which codeword.
///
/// Compressed-sparse-row layout, because codeword width varies by symbology and
/// a fixed `[usize; 8]` would only fit QR and DataMatrix. Built by the forked
/// bit-matrix parser as it walks the placement pattern, recording provenance for
/// the same modules it is already visiting.
#[derive(Clone, Debug, Default)]
pub struct CodewordProvenance {
    offsets: Vec<usize>,
    modules: Vec<usize>,
}

impl CodewordProvenance {
    /// Build from a per-codeword list of contributing module indices.
    pub fn from_lists(lists: &[Vec<usize>]) -> Self {
        let mut offsets = Vec::with_capacity(lists.len() + 1);
        let mut modules = Vec::new();
        offsets.push(0);
        for list in lists {
            modules.extend_from_slice(list);
            offsets.push(modules.len());
        }
        Self { offsets, modules }
    }

    pub fn len(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Module indices contributing to codeword `i`.
    pub fn modules_of(&self, i: usize) -> &[usize] {
        if i + 1 >= self.offsets.len() {
            return &[];
        }
        &self.modules[self.offsets[i]..self.offsets[i + 1]]
    }
}

/// Aggregate per-module confidence into per-codeword confidence.
///
/// Minimum, not mean. A codeword is a unit: one bad module makes the whole
/// codeword wrong, and the decoder cannot repair part of one. Taking the mean
/// would let seven clean modules disguise the one sitting under the logo, which
/// hands the decoder a confident wrong answer — the exact failure this project
/// exists to avoid.
pub fn codeword_confidence(module_conf: &[f32], provenance: &CodewordProvenance) -> Vec<f32> {
    (0..provenance.len())
        .map(|i| {
            provenance
                .modules_of(i)
                .iter()
                .filter_map(|&m| module_conf.get(m).copied())
                .fold(f32::INFINITY, f32::min)
        })
        .map(|c| if c.is_finite() { c } else { 1.0 })
        .collect()
}

/// Which codewords to declare erased, and what it cost.
#[derive(Clone, Debug, PartialEq)]
pub struct ErasurePlan {
    /// Codeword positions to erase, ascending. Indices are block-local.
    pub positions: Vec<usize>,
    /// Suspect codewords left unmarked because the budget ran out.
    ///
    /// Non-zero means this block is beyond erasure-only recovery, and the caller
    /// should escalate rather than expect a decode.
    pub unmarked_suspects: usize,
    /// Residual error capacity after these erasures: `floor((n - k - e) / 2)`.
    pub remaining_error_capacity: usize,
}

impl ErasurePlan {
    /// An empty plan — plain decoding, full error capacity.
    pub fn none(layout: BlockLayout) -> Self {
        Self {
            positions: Vec::new(),
            unmarked_suspects: 0,
            remaining_error_capacity: layout.max_errors(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.positions.is_empty()
    }
}

/// Allocate one block's erasure budget.
///
/// `confidences` is block-local, parallel to the block's codewords.
/// `cut` is the confidence at or below which a codeword is suspect.
/// `reserve_errors` holds back capacity for damage the occlusion mask missed —
/// each reserved error costs two units of budget. Zero spends everything on
/// erasures, which maximises reach when the mask is trusted and fails hard when
/// it is not.
pub fn plan_block(
    confidences: &[f32],
    layout: BlockLayout,
    cut: f32,
    reserve_errors: usize,
) -> ErasurePlan {
    let budget = layout.ecc().saturating_sub(2 * reserve_errors);

    let mut suspects: Vec<(usize, f32)> = confidences
        .iter()
        .enumerate()
        .filter(|&(_, &c)| c <= cut)
        .map(|(i, &c)| (i, c))
        .collect();

    // Worst first: if the budget cannot cover every suspect, the capacity should
    // go to the codewords least likely to be readable as-is.
    suspects.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

    let take = suspects.len().min(budget);
    let unmarked_suspects = suspects.len() - take;

    let mut positions: Vec<usize> = suspects.into_iter().take(take).map(|(i, _)| i).collect();
    positions.sort_unstable();

    ErasurePlan {
        positions,
        unmarked_suspects,
        remaining_error_capacity: layout.ecc().saturating_sub(take) / 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ecc_capacity_arithmetic() {
        // QR version 1-M: 26 codewords, 16 data, 10 ECC.
        let l = BlockLayout::new(26, 16);
        assert_eq!(l.ecc(), 10);
        assert_eq!(l.max_errors(), 5, "5 errors, or 10 erasures — the 2x lever");
    }

    #[test]
    fn codeword_confidence_takes_the_minimum() {
        // Seven pristine modules and one under a logo. The codeword is broken.
        let module_conf = vec![0.99, 0.98, 0.97, 0.02, 0.99, 0.98, 0.99, 0.97];
        let prov = CodewordProvenance::from_lists(&[vec![0, 1, 2, 3, 4, 5, 6, 7]]);

        let cw = codeword_confidence(&module_conf, &prov);
        assert_eq!(cw.len(), 1);
        assert!(
            (cw[0] - 0.02).abs() < 1e-6,
            "one damaged module must sink the codeword, got {}",
            cw[0]
        );
    }

    #[test]
    fn provenance_handles_variable_codeword_widths() {
        // PDF417 codewords are 17 modules; QR's are 8. One type covers both.
        let prov = CodewordProvenance::from_lists(&[
            vec![0, 1, 2, 3, 4, 5, 6, 7],
            (8..25).collect(),
        ]);
        assert_eq!(prov.len(), 2);
        assert_eq!(prov.modules_of(0).len(), 8);
        assert_eq!(prov.modules_of(1).len(), 17);
        assert_eq!(prov.modules_of(99), &[] as &[usize]);
    }

    #[test]
    fn missing_module_data_defaults_to_confident() {
        // A codeword with no resolvable modules must not read as damaged; that
        // would spend budget on nothing.
        let prov = CodewordProvenance::from_lists(&[vec![]]);
        let cw = codeword_confidence(&[], &prov);
        assert_eq!(cw, vec![1.0]);
    }

    #[test]
    fn marks_only_suspect_codewords() {
        let layout = BlockLayout::new(26, 16); // 10 ECC
        let mut conf = vec![0.9f32; 26];
        conf[3] = 0.05;
        conf[7] = 0.10;
        conf[20] = 0.01;

        let plan = plan_block(&conf, layout, 0.25, 0);
        assert_eq!(plan.positions, vec![3, 7, 20]);
        assert_eq!(plan.unmarked_suspects, 0);
        // 10 ECC - 3 erasures = 7, floor(7/2) = 3 errors still correctable.
        assert_eq!(plan.remaining_error_capacity, 3);
    }

    #[test]
    fn healthy_block_gets_no_erasures() {
        let layout = BlockLayout::new(26, 16);
        let plan = plan_block(&vec![0.95f32; 26], layout, 0.25, 0);
        assert!(plan.is_empty(), "spending budget on a clean block is a net loss");
        assert_eq!(plan.remaining_error_capacity, 5);
    }

    #[test]
    fn budget_caps_at_ecc_and_keeps_the_worst() {
        let layout = BlockLayout::new(26, 16); // 10 ECC, so 10 erasures max
        // 14 suspects — more than the block can absorb.
        let mut conf = vec![0.9f32; 26];
        for (n, i) in (0..14).enumerate() {
            conf[i] = 0.01 * n as f32; // ascending badness: index 0 is worst
        }

        let plan = plan_block(&conf, layout, 0.25, 0);
        assert_eq!(plan.positions.len(), 10, "capped at the ECC count");
        assert_eq!(plan.unmarked_suspects, 4, "caller must escalate");
        // The ten worst are indices 0..10; 10..14 were less bad and got dropped.
        assert_eq!(plan.positions, (0..10).collect::<Vec<_>>());
        assert_eq!(plan.remaining_error_capacity, 0);
    }

    #[test]
    fn reserving_error_capacity_costs_two_erasures_each() {
        let layout = BlockLayout::new(26, 16); // 10 ECC
        let mut conf = vec![0.9f32; 26];
        for c in conf.iter_mut().take(10) {
            *c = 0.01;
        }

        let none = plan_block(&conf, layout, 0.25, 0);
        assert_eq!(none.positions.len(), 10);

        // Holding back room for 2 unseen errors costs 4 units of erasure budget.
        let reserved = plan_block(&conf, layout, 0.25, 2);
        assert_eq!(reserved.positions.len(), 6);
        assert_eq!(reserved.unmarked_suspects, 4);
        assert_eq!(reserved.remaining_error_capacity, 2);
    }

    #[test]
    fn positions_are_ascending_regardless_of_badness_order() {
        let layout = BlockLayout::new(26, 16);
        let mut conf = vec![0.9f32; 26];
        conf[20] = 0.01; // worst
        conf[2] = 0.05;
        conf[11] = 0.10;

        let plan = plan_block(&conf, layout, 0.25, 0);
        assert_eq!(plan.positions, vec![2, 11, 20], "sorted for the decoder");
    }

    #[test]
    fn empty_plan_reports_full_error_capacity() {
        let layout = BlockLayout::new(26, 16);
        let plan = ErasurePlan::none(layout);
        assert!(plan.is_empty());
        assert_eq!(plan.remaining_error_capacity, 5);
    }
}
