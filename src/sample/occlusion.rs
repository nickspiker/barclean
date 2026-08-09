//! Turning scattered low confidence into a localized occlusion mask.
//!
//! # Why morphology
//!
//! Thresholding confidence alone produces two very different populations mixed
//! together, and telling them apart is the whole job:
//!
//! - **Speckle** — isolated modules that read poorly because of sensor noise, a
//!   dust mote, print grain, a JPEG artifact. Scattered, one or two modules
//!   wide. Reed-Solomon handles these *well* as ordinary errors; marking them as
//!   erasures is strictly counterproductive, because erasures and errors draw on
//!   the same budget and an erasure that was going to be corrected anyway has
//!   spent capacity for nothing.
//! - **Occlusion** — a logo, a sticker, a thumb, a glare blob. Contiguous, tens
//!   to hundreds of modules, and hopeless as errors because it exhausts `t`
//!   almost immediately. Exactly what erasure decoding exists for.
//!
//! Logos are blobs. Noise is not. Opening removes the speckle, closing
//! consolidates what remains, and a minimum-area filter drops whatever survived
//! both but is still too small to be a deliberate occlusion.
//!
//! # Order of operations
//!
//! Open **then** close, not the reverse. Closing first would dilate nearby
//! speckle into each other and manufacture a blob that was never there; opening
//! first removes the speckle while it is still isolated, and only then does
//! closing fill the genuine holes inside the real occlusion — the modules that
//! happened to read confidently despite sitting under the logo.
//!
//! Certain-damage seeds (function patterns contradicting the specification) are
//! unioned in **after** all morphology. They are proven damage; no filter should
//! be able to erode away a fact.

use super::confidence::ConfidenceGrid;

/// Tuning for occlusion localization.
#[derive(Clone, Copy, Debug)]
pub struct OcclusionParams {
    /// Confidence at or below which a module is suspect.
    ///
    /// Because confidence is calibrated against the symbol's own healthy
    /// modules, this is a genuine constant rather than a per-image knob: it
    /// means "less than a quarter as trustworthy as this symbol's own function
    /// patterns", which travels across lighting and print quality.
    pub cut: f32,
    /// Opening radius, in modules. Removes features thinner than `2r+1`.
    ///
    /// `1` clears single-module speckle. Raise it for noisy captures; set `0` to
    /// disable when the occlusion is expected to be thin (a fold, a scratch),
    /// since opening would otherwise erase exactly that.
    pub open_radius: usize,
    /// Closing radius, in modules. Fills holes narrower than `2r+1`.
    ///
    /// `2` is a good default: logo interiors routinely contain modules that
    /// happen to read confidently, and leaving those unmarked splinters one
    /// occlusion into several.
    pub close_radius: usize,
    /// Minimum connected-component area, in modules, for a region to count as an
    /// occlusion rather than surviving noise.
    pub min_area: usize,
}

impl Default for OcclusionParams {
    fn default() -> Self {
        Self {
            cut: 0.25,
            open_radius: 1,
            close_radius: 2,
            min_area: 6,
        }
    }
}

/// Where the occlusion is.
#[derive(Clone, Debug)]
pub struct OcclusionMask {
    pub width: usize,
    pub height: usize,
    /// Number of distinct occluding regions that survived filtering.
    pub region_count: usize,
    /// Area of the largest region, in modules.
    pub largest_region: usize,
    occluded: Vec<bool>,
}

impl OcclusionMask {
    /// Wrap an existing row-major mask.
    ///
    /// For callers that already know the occluded set — a user-drawn region, a mask carried over
    /// from a previous frame during multi-frame fusion, or a hand-built one in tests.
    ///
    /// # Panics
    /// If `occluded.len() != width * height`.
    pub fn from_mask(width: usize, height: usize, occluded: Vec<bool>) -> Self {
        assert_eq!(
            occluded.len(),
            width * height,
            "OcclusionMask: {width}x{height} needs {} cells, got {}",
            width * height,
            occluded.len()
        );
        let (_, region_count, largest_region) = filter_by_area(&occluded, width, height, 1);
        Self {
            width,
            height,
            region_count,
            largest_region,
            occluded,
        }
    }

    pub fn get(&self, x: usize, y: usize) -> bool {
        self.occluded[y * self.width + x]
    }

    pub fn at(&self, index: usize) -> bool {
        self.occluded[index]
    }

    pub fn as_slice(&self) -> &[bool] {
        &self.occluded
    }

    /// Total occluded modules.
    pub fn count(&self) -> usize {
        self.occluded.iter().filter(|&&o| o).count()
    }

    /// Occluded fraction of the symbol, `[0, 1]`.
    ///
    /// The headline number for triage and for the inspect overlay: a 5%
    /// occlusion is routine, 30% is at the edge of what any ECC level can
    /// return.
    pub fn fraction(&self) -> f32 {
        if self.occluded.is_empty() {
            return 0.0;
        }
        self.count() as f32 / self.occluded.len() as f32
    }

    /// Indices of every occluded module.
    pub fn indices(&self) -> Vec<usize> {
        (0..self.occluded.len())
            .filter(|&i| self.occluded[i])
            .collect()
    }
}

/// Localize the occlusion in a scored grid.
///
/// `certain` carries indices proven damaged by specification contradiction (see
/// [`super::confidence::certain_damage`]); they bypass every filter.
pub fn locate(
    conf: &ConfidenceGrid,
    certain: &[usize],
    params: &OcclusionParams,
) -> OcclusionMask {
    let (w, h) = (conf.width, conf.height);

    let raw: Vec<bool> = conf.scores().iter().map(|&s| s <= params.cut).collect();

    let opened = open(&raw, w, h, params.open_radius);
    let closed = close(&opened, w, h, params.close_radius);
    let (mut filtered, region_count, largest_region) = filter_by_area(&closed, w, h, params.min_area);

    // Proven damage is not subject to appeal.
    for &i in certain {
        if i < filtered.len() {
            filtered[i] = true;
        }
    }

    OcclusionMask {
        width: w,
        height: h,
        region_count,
        largest_region,
        occluded: filtered,
    }
}

/// Tuning for fitting a region to proven-damaged modules.
#[derive(Clone, Copy, Debug)]
pub struct FitParams {
    /// Closing radius. Must be large enough to bridge the gaps between proven points, which are
    /// sparse for the two reasons described on [`fit_region`].
    pub close_radius: usize,
    /// Minimum region area in modules.
    pub min_area: usize,
    /// Final dilation, in modules, applied after fitting.
    ///
    /// The proven points sample the occlusion's *interior* — its boundary modules are as likely to
    /// have been undamaged as any other. Growing outward covers the rim the sample cannot see.
    pub grow: usize,
}

impl Default for FitParams {
    fn default() -> Self {
        Self {
            close_radius: 3,
            min_area: 4,
            grow: 1,
        }
    }
}

/// Fit an occlusion region to modules *proven* damaged.
///
/// Where [`locate`] infers damage from estimated confidence, this starts from ground truth:
/// modules that Reed-Solomon proved were wrong, with no false positives possible. But the proof is
/// a **subsample** of the occlusion, sparse for two independent reasons:
///
/// - An opaque logo only damages the modules that disagree with it. A dark logo over a symbol
///   leaves every module that was already dark completely intact — roughly half the covered area
///   is undamaged and therefore invisible to this method.
/// - Only blocks that decoded contribute, and interleaving scatters each block's codewords across
///   the whole symbol, so even a full survivor set samples the blob in a fine mesh rather than
///   filling it.
///
/// So the points are a sparse, unbiased sample of a contiguous region, and the job is to recover
/// the region they sample: close to bridge the mesh, then grow to reach the rim.
pub fn fit_region(
    damaged: &[usize],
    width: usize,
    height: usize,
    params: &FitParams,
) -> OcclusionMask {
    let mut mask = vec![false; width * height];
    for &i in damaged {
        if i < mask.len() {
            mask[i] = true;
        }
    }

    let closed = close(&mask, width, height, params.close_radius);
    let (filtered, region_count, largest_region) =
        filter_by_area(&closed, width, height, params.min_area);
    let grown = if params.grow > 0 {
        dilate(&filtered, width, height, params.grow)
    } else {
        filtered
    };

    OcclusionMask {
        width,
        height,
        region_count,
        largest_region,
        occluded: grown,
    }
}

/// Erode then dilate: removes features thinner than `2r+1`.
fn open(mask: &[bool], w: usize, h: usize, r: usize) -> Vec<bool> {
    if r == 0 {
        return mask.to_vec();
    }
    let eroded = erode(mask, w, h, r);
    dilate(&eroded, w, h, r)
}

/// Dilate then erode: fills holes narrower than `2r+1`.
fn close(mask: &[bool], w: usize, h: usize, r: usize) -> Vec<bool> {
    if r == 0 {
        return mask.to_vec();
    }
    let dilated = dilate(mask, w, h, r);
    erode(&dilated, w, h, r)
}

/// Set where any neighbour within the square radius is set.
///
/// Coordinates clamp at the border (replicate), so an occlusion touching the
/// symbol edge behaves the same as one in the interior.
fn dilate(mask: &[bool], w: usize, h: usize, r: usize) -> Vec<bool> {
    neighbourhood(mask, w, h, r, |acc, v| acc || v, false)
}

/// Set only where every neighbour within the square radius is set.
fn erode(mask: &[bool], w: usize, h: usize, r: usize) -> Vec<bool> {
    neighbourhood(mask, w, h, r, |acc, v| acc && v, true)
}

fn neighbourhood(
    mask: &[bool],
    w: usize,
    h: usize,
    r: usize,
    combine: fn(bool, bool) -> bool,
    init: bool,
) -> Vec<bool> {
    let mut out = vec![false; mask.len()];
    let ri = r as isize;
    for y in 0..h {
        for x in 0..w {
            let mut acc = init;
            for dy in -ri..=ri {
                for dx in -ri..=ri {
                    let sx = (x as isize + dx).clamp(0, w as isize - 1) as usize;
                    let sy = (y as isize + dy).clamp(0, h as isize - 1) as usize;
                    acc = combine(acc, mask[sy * w + sx]);
                }
            }
            out[y * w + x] = acc;
        }
    }
    out
}

/// Drop connected components smaller than `min_area`. 8-connected, because a
/// diagonal seam through a logo is still one logo.
///
/// Returns the filtered mask, the surviving region count, and the largest
/// region's area.
fn filter_by_area(mask: &[bool], w: usize, h: usize, min_area: usize) -> (Vec<bool>, usize, usize) {
    let mut out = vec![false; mask.len()];
    let mut visited = vec![false; mask.len()];
    let mut regions = 0usize;
    let mut largest = 0usize;
    let mut stack: Vec<usize> = Vec::new();
    let mut component: Vec<usize> = Vec::new();

    for start in 0..mask.len() {
        if !mask[start] || visited[start] {
            continue;
        }
        component.clear();
        stack.clear();
        stack.push(start);
        visited[start] = true;

        while let Some(i) = stack.pop() {
            component.push(i);
            let (x, y) = (i % w, i / w);
            for dy in -1isize..=1 {
                for dx in -1isize..=1 {
                    if dx == 0 && dy == 0 {
                        continue;
                    }
                    let nx = x as isize + dx;
                    let ny = y as isize + dy;
                    if nx < 0 || ny < 0 || nx >= w as isize || ny >= h as isize {
                        continue;
                    }
                    let n = ny as usize * w + nx as usize;
                    if mask[n] && !visited[n] {
                        visited[n] = true;
                        stack.push(n);
                    }
                }
            }
        }

        if component.len() >= min_area {
            regions += 1;
            largest = largest.max(component.len());
            for &i in &component {
                out[i] = true;
            }
        }
    }

    (out, regions, largest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sample::confidence::evaluate;
    use crate::sample::{KnownModule, ModuleStats, StatsGrid};

    fn stats(mean_luma: u8, var_luma: u16, chroma: u8) -> ModuleStats {
        ModuleStats {
            mean_luma,
            var_luma,
            chroma,
            threshold: 128,
            value: mean_luma < 128,
        }
    }

    /// A 20x20 symbol of healthy modules with a `size`x`size` logo blob at
    /// `(ox, oy)`: textured and coloured, the way real logo content reads.
    fn grid_with_logo(ox: usize, oy: usize, size: usize) -> (StatsGrid, Vec<KnownModule>) {
        const N: usize = 20;
        let mut cells: Vec<ModuleStats> = (0..N * N)
            .map(|i| if i % 3 == 0 { stats(20, 4, 1) } else { stats(235, 4, 1) })
            .collect();
        for y in oy..oy + size {
            for x in ox..ox + size {
                cells[y * N + x] = stats(90, 9000, 150);
            }
        }
        let known: Vec<KnownModule> = (0..N * N)
            .filter(|i| {
                let (x, y) = (i % N, i / N);
                !(x >= ox && x < ox + size && y >= oy && y < oy + size)
            })
            .map(|i| KnownModule::new(i, cells[i].value))
            .collect();
        (StatsGrid::new(N, N, cells), known)
    }

    #[test]
    fn dilate_and_erode_are_duals() {
        let mut m = vec![false; 25];
        m[12] = true; // centre of 5x5

        let d = dilate(&m, 5, 5, 1);
        assert_eq!(d.iter().filter(|&&v| v).count(), 9, "3x3 block");

        let e = erode(&d, 5, 5, 1);
        assert_eq!(e.iter().filter(|&&v| v).count(), 1, "back to the single cell");
        assert!(e[12]);
    }

    #[test]
    fn opening_removes_isolated_speckle() {
        let mut m = vec![false; 100];
        m[11] = true;
        m[35] = true;
        m[77] = true;

        let opened = open(&m, 10, 10, 1);
        assert!(
            opened.iter().all(|&v| !v),
            "single-module speckle should not survive opening"
        );
    }

    #[test]
    fn closing_fills_holes_in_a_blob() {
        let mut m = vec![false; 100];
        for y in 2..8 {
            for x in 2..8 {
                m[y * 10 + x] = true;
            }
        }
        m[4 * 10 + 4] = false; // a module that read fine despite the logo
        m[5 * 10 + 5] = false;

        let closed = close(&m, 10, 10, 2);
        assert!(closed[4 * 10 + 4], "interior hole filled");
        assert!(closed[5 * 10 + 5], "interior hole filled");
    }

    #[test]
    fn border_occlusion_survives_morphology() {
        // Clamped borders mean an occlusion running off the symbol edge is not
        // silently eroded away.
        let mut m = vec![false; 100];
        for y in 0..4 {
            for x in 0..4 {
                m[y * 10 + x] = true;
            }
        }
        let opened = open(&m, 10, 10, 1);
        assert!(opened[0], "corner blob survives opening");
        assert_eq!(opened.iter().filter(|&&v| v).count(), 16);
    }

    #[test]
    fn area_filter_drops_small_regions() {
        let mut m = vec![false; 100];
        // a 4-module region
        m[0] = true;
        m[1] = true;
        m[10] = true;
        m[11] = true;
        // a 9-module region
        for y in 5..8 {
            for x in 5..8 {
                m[y * 10 + x] = true;
            }
        }

        let (out, regions, largest) = filter_by_area(&m, 10, 10, 6);
        assert_eq!(regions, 1);
        assert_eq!(largest, 9);
        assert!(!out[0], "4-module region dropped");
        assert!(out[5 * 10 + 5], "9-module region kept");
    }

    #[test]
    fn area_filter_treats_diagonals_as_connected() {
        let mut m = vec![false; 100];
        // two 3-module runs joined only at a diagonal: one region of 6, not two of 3
        m[0] = true;
        m[1] = true;
        m[2] = true;
        m[13] = true;
        m[14] = true;
        m[15] = true;

        let (_, regions, largest) = filter_by_area(&m, 10, 10, 6);
        assert_eq!(regions, 1, "diagonal contact joins the regions");
        assert_eq!(largest, 6);
    }

    #[test]
    fn locates_a_logo_blob() {
        let (grid, known) = grid_with_logo(7, 7, 6);
        let conf = evaluate(&grid, &known);
        let mask = locate(&conf, &[], &OcclusionParams::default());

        assert_eq!(mask.region_count, 1, "one logo, one region");
        assert!(
            mask.largest_region >= 30,
            "expected roughly the 36-module logo, got {}",
            mask.largest_region
        );

        for y in 7..13 {
            for x in 7..13 {
                assert!(mask.get(x, y), "logo module ({x},{y}) should be occluded");
            }
        }
        // And nothing far from it.
        assert!(!mask.get(0, 0));
        assert!(!mask.get(19, 19));
    }

    #[test]
    fn ignores_scattered_noise() {
        const N: usize = 20;
        let mut cells: Vec<ModuleStats> = (0..N * N)
            .map(|i| if i % 3 == 0 { stats(20, 4, 1) } else { stats(235, 4, 1) })
            .collect();
        // Scatter genuinely bad modules that RS would fix as ordinary errors.
        for &i in &[5usize, 47, 111, 200, 333, 390] {
            cells[i] = stats(127, 4, 1);
        }
        let known: Vec<KnownModule> = (0..N * N)
            .filter(|i| ![5usize, 47, 111, 200, 333, 390].contains(i))
            .map(|i| KnownModule::new(i, cells[i].value))
            .collect();
        let grid = StatsGrid::new(N, N, cells);

        let conf = evaluate(&grid, &known);
        let mask = locate(&conf, &[], &OcclusionParams::default());

        assert_eq!(mask.count(), 0, "scattered noise is not an occlusion");
        assert_eq!(mask.region_count, 0);
    }

    #[test]
    fn certain_damage_bypasses_every_filter() {
        const N: usize = 20;
        let cells: Vec<ModuleStats> = (0..N * N).map(|_| stats(20, 4, 1)).collect();
        let grid = StatsGrid::new(N, N, cells);
        let known: Vec<KnownModule> = (0..N * N).map(|i| KnownModule::new(i, true)).collect();
        let conf = evaluate(&grid, &known);

        // A lone module, far below min_area and certain to be opened away — but
        // proven damaged, so it must appear regardless.
        let mask = locate(&conf, &[123], &OcclusionParams::default());
        assert!(mask.at(123), "proven damage must survive morphology");
        assert_eq!(mask.count(), 1);
    }

    #[test]
    fn fraction_reports_occluded_share() {
        let (grid, known) = grid_with_logo(7, 7, 6);
        let conf = evaluate(&grid, &known);
        let mask = locate(&conf, &[], &OcclusionParams::default());

        // 36 of 400 modules is 9%.
        let f = mask.fraction();
        assert!(f > 0.07 && f < 0.12, "occluded fraction was {f}");
    }
}
