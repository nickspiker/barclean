//! Bootstrapping an occlusion map out of the symbol's own algebra.
//!
//! # The loop
//!
//! A Reed-Solomon block that decodes proves exactly which of its codewords were wrong. That is a
//! damage map with no false positives and no image statistics behind it — but it only exists for
//! blocks that stayed inside their budget, which are the blocks that did not need help.
//!
//! Interleaving turns that into a lever. QR scatters consecutive codewords across every block in
//! rotation, so a contiguous occlusion is spread across all of them. Blocks that survive report
//! damaged codewords; those codewords map back to damaged *modules*; and because an occlusion is a
//! blob rather than scattered noise, the modules outline where the blob is. That outline says which
//! codewords of the *failed* blocks must also be covered — and declaring those as erasures doubles
//! the correction power applied to them.
//!
//! The result compounds:
//!
//! ```text
//!   decode blocks ──> survivors prove damaged codewords
//!         ^                        |
//!         |                        v
//!   erasures for            map to modules, fit the blob
//!   failed blocks                  |
//!         ^                        v
//!         └──── which codewords does the blob cover? ────┘
//! ```
//!
//! Each round that rescues a block adds *that block's* exact damage to the evidence, sharpening the
//! fit, which can rescue another. A single pass only works across the narrow band where some blocks
//! survive and some do not; iterating extends it, because the evidence base grows with every
//! success.
//!
//! # What it costs, and the honest limit
//!
//! Nothing, when it fails: a round that rescues no block terminates the loop, and the answer is the
//! same one plain decoding gave. The floor is plain decoding's result.
//!
//! The ceiling is set by the first round. If *no* block decodes on the initial attempt, there is no
//! evidence to bootstrap from and the loop cannot start — that is where image-based confidence has
//! to take over, and why it remains in the design rather than being replaced by this.

use crate::clean::erasure::BlockLayout;
use crate::sample::occlusion::{FitParams, OcclusionMask, fit_region};
use std::collections::BTreeSet;

/// Tuning for the bootstrap loop.
#[derive(Clone, Copy, Debug)]
pub struct BootstrapParams {
    /// Cap on refinement rounds. Each round costs one decode attempt per still-failed block, so
    /// this bounds worst-case work; the loop almost always terminates on its own well before it.
    pub max_rounds: usize,
    pub fit: FitParams,
}

impl Default for BootstrapParams {
    fn default() -> Self {
        Self {
            max_rounds: 8,
            fit: FitParams::default(),
        }
    }
}

/// One block's decode state, as the loop sees it.
#[derive(Clone, Debug)]
pub struct BlockState {
    pub index: usize,
    pub layout: BlockLayout,
    /// Global codeword indices this block proved damaged, if it decoded.
    pub damaged: Vec<usize>,
    pub codewords: Option<Vec<u8>>,
}

impl BlockState {
    pub fn decoded(&self) -> bool {
        self.codewords.is_some()
    }
}

/// What the loop achieved.
#[derive(Clone, Debug)]
pub struct BootstrapOutcome {
    pub rounds: usize,
    pub blocks_total: usize,
    /// Blocks decoded on the first attempt, before any bootstrapping.
    pub blocks_decoded_initially: usize,
    pub blocks_decoded_finally: usize,
    /// Concatenated data codewords, present only when every block decoded.
    pub data: Option<Vec<u8>>,
    /// Modules belonging to codewords proven damaged.
    ///
    /// Note the indirection: Reed-Solomon proves a *codeword* was wrong, and a QR codeword's eight
    /// modules are scattered across the symbol by the placement walk. So this set contains every
    /// module of every damaged codeword — including modules that were themselves untouched, since
    /// nothing can say which of the eight was the bad one. The set is dense inside the occlusion and
    /// sparse outside it, which is exactly what a blob fit needs and is why the fit closes and grows
    /// rather than taking the points literally.
    pub damaged_codeword_modules: Vec<usize>,
    /// The fitted occlusion, if there was ever enough evidence to fit one.
    pub region: Option<OcclusionMask>,
    /// Final per-block state, carrying each block's corrected codewords. Needed to reassemble the
    /// corrected stream for an exact re-render.
    pub blocks: Vec<BlockState>,
}

impl BootstrapOutcome {
    pub fn complete(&self) -> bool {
        self.data.is_some()
    }

    /// Blocks rescued purely by bootstrapping — the loop's contribution over plain decoding.
    pub fn blocks_rescued(&self) -> usize {
        self.blocks_decoded_finally
            .saturating_sub(self.blocks_decoded_initially)
    }
}

/// Decode a block, given optional erasure positions. Supplied by the caller so this module stays
/// independent of any one symbology's decoder.
///
/// Returns the corrected codewords and the block-local positions that were actually damaged, or
/// `None` if the block exceeded its budget.
pub type BlockDecoder<'a> =
    &'a dyn Fn(usize, &[usize]) -> Option<(Vec<u8>, Vec<usize>)>;

/// Run the bootstrap loop.
///
/// - `layouts` — each block's `(n, k)`.
/// - `to_global` — maps `(block, local codeword position)` to an index in the interleaved stream.
/// - `provenance` — module indices for each global codeword.
/// - `dimension` — symbol width in modules (symbols here are square).
/// - `decode_block` — attempts one block, with erasures.
pub fn bootstrap(
    layouts: &[BlockLayout],
    to_global: &dyn Fn(usize, usize) -> Option<usize>,
    provenance: &[Vec<usize>],
    dimension: usize,
    decode_block: BlockDecoder<'_>,
    params: &BootstrapParams,
) -> BootstrapOutcome {
    let total = layouts.len();
    let mut proven_codewords: BTreeSet<usize> = BTreeSet::new();
    let mut region: Option<OcclusionMask> = None;
    let mut erasures: Vec<Vec<usize>> = vec![Vec::new(); total];
    let mut states: Vec<BlockState> = Vec::new();
    let mut initial_decoded = None;
    let mut rounds = 0;

    for round in 0..params.max_rounds.max(1) {
        rounds = round + 1;

        states = (0..total)
            .map(|b| match decode_block(b, &erasures[b]) {
                Some((codewords, local)) => BlockState {
                    index: b,
                    layout: layouts[b],
                    damaged: local
                        .iter()
                        .filter_map(|&p| to_global(b, p))
                        .collect(),
                    codewords: Some(codewords),
                },
                None => BlockState {
                    index: b,
                    layout: layouts[b],
                    damaged: Vec::new(),
                    codewords: None,
                },
            })
            .collect();

        let decoded_now = states.iter().filter(|s| s.decoded()).count();
        if initial_decoded.is_none() {
            initial_decoded = Some(decoded_now);
        }

        // Gather evidence BEFORE checking for completion. A symbol where every block decodes still
        // has a damage map worth keeping — it is exact, free, and drives the reconstruction mask and
        // the inspect overlay. Breaking out first would discard it precisely in the case where it is
        // most trustworthy.
        let before = proven_codewords.len();
        for state in states.iter().filter(|s| s.decoded()) {
            proven_codewords.extend(state.damaged.iter().copied());
        }

        if decoded_now == total {
            break;
        }

        // No evidence at all: nothing decoded, so there is nothing to bootstrap from. This is the
        // ceiling described in the module docs, and where image confidence has to take over.
        if proven_codewords.is_empty() {
            break;
        }
        // Evidence stopped growing, so the next round would fit the same region and produce the
        // same erasures. Stop rather than spin.
        if round > 0 && proven_codewords.len() == before {
            break;
        }

        let damaged_modules: Vec<usize> = proven_codewords
            .iter()
            .filter_map(|&cw| provenance.get(cw))
            .flatten()
            .copied()
            .collect();

        let mask = fit_region(&damaged_modules, dimension, dimension, &params.fit);
        erasures = plan_from_region(&states, to_global, provenance, &mask);
        region = Some(mask);
    }

    let decoded_finally = states.iter().filter(|s| s.decoded()).count();
    let data = if decoded_finally == total && total > 0 {
        Some(
            states
                .iter()
                .flat_map(|s| {
                    s.codewords
                        .as_ref()
                        .map(|c| c[..s.layout.data].to_vec())
                        .unwrap_or_default()
                })
                .collect(),
        )
    } else {
        None
    };

    let damaged_codeword_modules: Vec<usize> = proven_codewords
        .iter()
        .filter_map(|&cw| provenance.get(cw))
        .flatten()
        .copied()
        .collect();

    BootstrapOutcome {
        rounds,
        blocks_total: total,
        blocks_decoded_initially: initial_decoded.unwrap_or(0),
        blocks_decoded_finally: decoded_finally,
        data,
        damaged_codeword_modules,
        region,
        blocks: states,
    }
}

/// Choose erasure positions for every still-failed block from a fitted occlusion.
///
/// A codeword is a candidate when any of its modules falls inside the region, and is ranked by
/// **how many** do. That ranking matters: budget is capped at `n-k`, and a codeword with all eight
/// modules under the logo is far more likely to be damaged than one clipped at the rim by a single
/// module. Spending the cap on the deeply-buried codewords first is the difference between an
/// erasure set that decodes and one that wastes its budget on the boundary.
///
/// Blocks that already decoded get an empty list — re-erasing a solved block could only make it
/// worse.
fn plan_from_region(
    states: &[BlockState],
    to_global: &dyn Fn(usize, usize) -> Option<usize>,
    provenance: &[Vec<usize>],
    region: &OcclusionMask,
) -> Vec<Vec<usize>> {
    states
        .iter()
        .map(|state| {
            if state.decoded() {
                return Vec::new();
            }
            let mut scored: Vec<(usize, usize)> = (0..state.layout.total)
                .filter_map(|local| {
                    let global = to_global(state.index, local)?;
                    let modules = provenance.get(global)?;
                    let covered = modules.iter().filter(|&&m| region.at(m)).count();
                    (covered > 0).then_some((local, covered))
                })
                .collect();

            // Most-buried first, then cap at the block's erasure budget.
            scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            scored.truncate(state.layout.ecc());

            let mut positions: Vec<usize> = scored.into_iter().map(|(local, _)| local).collect();
            positions.sort_unstable();
            positions
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two blocks of 26 codewords, 16 data, 10 ECC. Codewords interleave, matching QR.
    fn layouts(n: usize) -> Vec<BlockLayout> {
        vec![BlockLayout::new(26, 16); n]
    }

    fn interleaved_to_global(blocks: usize) -> impl Fn(usize, usize) -> Option<usize> {
        move |b, local| Some(local * blocks + b)
    }

    /// Every codeword owns 8 consecutive modules in a `dimension`-wide grid.
    fn simple_provenance(codewords: usize) -> Vec<Vec<usize>> {
        (0..codewords)
            .map(|i| (i * 8..i * 8 + 8).collect())
            .collect()
    }

    #[test]
    fn complete_first_round_decode_needs_no_bootstrapping() {
        let l = layouts(2);
        let prov = simple_provenance(52);
        let decode = |_b: usize, _e: &[usize]| Some((vec![0u8; 26], Vec::new()));

        let out = bootstrap(
            &l,
            &interleaved_to_global(2),
            &prov,
            32,
            &decode,
            &BootstrapParams::default(),
        );

        assert!(out.complete());
        assert_eq!(out.rounds, 1, "no refinement needed");
        assert_eq!(out.blocks_rescued(), 0);
        assert!(out.damaged_codeword_modules.is_empty());
    }

    #[test]
    fn total_failure_terminates_without_spinning() {
        // Nothing decodes, so there is no evidence to bootstrap from. The loop must recognise that
        // immediately rather than burning every round fitting an empty region.
        let l = layouts(4);
        let prov = simple_provenance(104);
        let decode = |_b: usize, _e: &[usize]| None;

        let out = bootstrap(
            &l,
            &interleaved_to_global(4),
            &prov,
            64,
            &decode,
            &BootstrapParams::default(),
        );

        assert!(!out.complete());
        assert_eq!(out.rounds, 1, "must stop on the first round, not iterate uselessly");
        assert_eq!(out.blocks_decoded_finally, 0);
        assert!(out.region.is_none());
    }

    /// Control flow only: survivor evidence reaches a failed block as erasures, the retry is made,
    /// and the loop terminates having rescued it.
    ///
    /// Deliberately does **not** test whether the fitted region geometrically covers the right
    /// codewords — this fixture's provenance is a linear module layout, which segregates the two
    /// blocks into disjoint column bands and misrepresents how QR actually places codewords (the
    /// snake walk scatters every block's bits throughout the symbol, so blocks are thoroughly
    /// intermixed). Tuning a toy geometry until it passes would prove nothing about the real one.
    /// Geometric behaviour is validated end-to-end against a real encoded symbol in
    /// `tests/bootstrap_recovers_occluded_symbols.rs`.
    #[test]
    fn rescues_a_failed_block_using_a_survivor_s_evidence() {
        let l = layouts(2);
        let prov = simple_provenance(52);
        let damaged_locals: Vec<usize> = (4..12).collect();

        let decode = move |b: usize, erasures: &[usize]| -> Option<(Vec<u8>, Vec<usize>)> {
            if b == 0 {
                // Survivor: reports the same damaged codewords every round.
                return Some((vec![0u8; 26], damaged_locals.clone()));
            }
            // Failed block: decodes once the loop hands it erasures derived from the survivor.
            (!erasures.is_empty()).then(|| (vec![1u8; 26], Vec::new()))
        };

        let out = bootstrap(
            &l,
            &interleaved_to_global(2),
            &prov,
            32,
            &decode,
            &BootstrapParams::default(),
        );

        assert!(out.complete(), "the failed block should have been rescued");
        assert_eq!(out.blocks_decoded_initially, 1);
        assert_eq!(out.blocks_decoded_finally, 2);
        assert_eq!(out.blocks_rescued(), 1);
        assert!(out.rounds >= 2, "rescue requires at least one refinement round");
        assert!(out.region.is_some());
    }

    #[test]
    fn stops_when_a_round_rescues_nothing() {
        // A failed block that never recovers must not spin the loop to max_rounds: once the
        // evidence set stops growing, every later round would fit the same region and plan the
        // same erasures.
        let l = layouts(2);
        let prov = simple_provenance(52);

        let decode = |b: usize, _e: &[usize]| -> Option<(Vec<u8>, Vec<usize>)> {
            (b == 0).then(|| (vec![0u8; 26], vec![4, 5, 6]))
        };

        let out = bootstrap(
            &l,
            &interleaved_to_global(2),
            &prov,
            32,
            &decode,
            &BootstrapParams::default(),
        );

        assert!(!out.complete());
        assert_eq!(out.blocks_decoded_finally, 1, "floor is plain decoding's result");
        assert_eq!(out.rounds, 2, "one round to gather evidence, one to find it stale");
        // Even having failed, it still yields the exact damage the survivor proved — useful for
        // the reconstruction mask and the inspect overlay.
        assert!(!out.damaged_codeword_modules.is_empty());
    }

    #[test]
    fn erasure_budget_is_respected_per_block() {
        let states = vec![BlockState {
            index: 0,
            layout: BlockLayout::new(26, 16), // 10 ECC
            damaged: Vec::new(),
            codewords: None,
        }];
        let prov = simple_provenance(26);
        // A region covering every module, so every codeword is a candidate.
        let region = fit_region(&(0..208).collect::<Vec<_>>(), 32, 32, &FitParams::default());

        let plans = plan_from_region(&states, &|_b, local| Some(local), &prov, &region);
        assert!(
            plans[0].len() <= 10,
            "planned {} erasures against a 10-codeword budget",
            plans[0].len()
        );
    }

    #[test]
    fn decoded_blocks_are_never_given_erasures() {
        let states = vec![BlockState {
            index: 0,
            layout: BlockLayout::new(26, 16),
            damaged: vec![1, 2],
            codewords: Some(vec![0u8; 26]),
        }];
        let prov = simple_provenance(26);
        let region = fit_region(&(0..208).collect::<Vec<_>>(), 32, 32, &FitParams::default());

        let plans = plan_from_region(&states, &|_b, local| Some(local), &prov, &region);
        assert!(
            plans[0].is_empty(),
            "a solved block must be left alone; erasing it could only lose information"
        );
    }

    #[test]
    fn deeply_covered_codewords_are_preferred_over_rim_codewords() {
        // Budget is scarce, so ranking has to favour codewords the occlusion actually buried.
        let states = vec![BlockState {
            index: 0,
            layout: BlockLayout::new(6, 2), // 4 ECC — only 4 erasures available
            damaged: Vec::new(),
            codewords: None,
        }];
        // Codeword 0 sits fully inside the region; 1..5 clip it by one module each.
        let prov: Vec<Vec<usize>> = vec![
            vec![0, 1, 2, 3, 4, 5, 6, 7],
            vec![7, 100, 101, 102, 103, 104, 105, 106],
            vec![6, 110, 111, 112, 113, 114, 115, 116],
            vec![5, 120, 121, 122, 123, 124, 125, 126],
            vec![4, 130, 131, 132, 133, 134, 135, 136],
            vec![3, 140, 141, 142, 143, 144, 145, 146],
        ];
        // A region covering only modules 0..8 — codeword 0's territory exactly.
        let region = OcclusionMask::from_mask(32, 32, (0..32 * 32).map(|i| i < 8).collect());

        let plans = plan_from_region(&states, &|_b, local| Some(local), &prov, &region);
        assert!(
            plans[0].contains(&0),
            "the fully-buried codeword must make the cut, got {:?}",
            plans[0]
        );
        assert_eq!(plans[0].len(), 4, "capped at the 4-codeword ECC budget");
    }
}
