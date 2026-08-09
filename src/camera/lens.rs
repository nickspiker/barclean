//! Presenting the physical lenses so the user can choose between them.
//!
//! # Selection is the user's
//!
//! This module decides nothing. It reports what each physical camera would deliver on the symbol
//! currently in frame, and the user picks. An app that silently swapped lenses would be fighting
//! whoever is holding it — they moved closer for a reason, they framed it that way for a reason,
//! and a camera that second-guesses that is worse than one that just does as it is told.
//!
//! # What it does compute
//!
//! Decoding quality is governed by **pixels per module**. Below roughly 3 a symbol is
//! unrecoverable no matter how good the decoder, because adjacent modules blur together before
//! sampling begins. Around 4 it works. At 6 or more the intra-cell variance signal that occlusion
//! detection depends on becomes reliable, since there are finally enough samples inside one module
//! for a variance to mean anything.
//!
//! A phone carries three or four cameras with genuinely different angular resolutions, and the
//! difference between them is not a crop — a telephoto delivers real optical detail that cropping
//! into the ultra-wide cannot recover. Which one is right depends on how far away the symbol is and
//! how large it is, neither of which is obvious from behind the viewfinder.
//!
//! So each lens is annotated with what it would actually give: predicted pixels per module, and
//! whether the symbol would still fit in frame. The user chooses with the numbers in front of them
//! instead of guessing.
//!
//! # How the prediction works
//!
//! Measure pixels per module on the lens in use, and divide by that lens's angular resolution. The
//! result is how much *angle* one module subtends — a property of the scene and the distance, not
//! of the camera. Multiply by any other lens's angular resolution and you have what that lens would
//! deliver from the same spot. One measurement annotates every option.

/// A physical camera module, as reported by `CameraCharacteristics`.
///
/// Populated from `LENS_INFO_AVAILABLE_FOCAL_LENGTHS`, `SENSOR_INFO_PHYSICAL_SIZE`,
/// `SENSOR_INFO_ACTIVE_ARRAY_SIZE` and `LENS_INFO_MINIMUM_FOCUS_DISTANCE` — the same enumeration
/// Lumis already performs when walking `physicalCameraIds` of a `LOGICAL_MULTI_CAMERA`.
#[derive(Clone, Debug, PartialEq)]
pub struct LensSpec {
    /// Physical camera id, passed back to `setPhysicalCameraId`.
    pub id: String,
    /// Human-facing label for the picker — "Ultra-wide", "Main", "5×".
    pub label: String,
    pub focal_length_mm: f32,
    /// Physical sensor width, millimetres.
    pub sensor_width_mm: f32,
    /// Active array width in pixels for the stream being configured.
    pub pixel_width: u32,
    /// Closest focusable distance in metres. `0.0` means fixed-focus or unreported, which Camera2
    /// signals as an infinite minimum focus distance.
    pub min_focus_distance_m: f32,
}

impl LensSpec {
    /// Horizontal field of view, radians.
    pub fn hfov_rad(&self) -> f32 {
        if self.focal_length_mm <= 0.0 || self.sensor_width_mm <= 0.0 {
            return 0.0;
        }
        2.0 * (self.sensor_width_mm / (2.0 * self.focal_length_mm)).atan()
    }

    /// Angular resolution: pixels per radian across the horizontal axis.
    ///
    /// The figure of merit. A 3× telephoto has roughly three times the pixels per radian of the
    /// main camera at equal sensor resolution, which is three times the px/module on the same
    /// symbol from the same position.
    pub fn px_per_radian(&self) -> f32 {
        let fov = self.hfov_rad();
        if fov <= 0.0 {
            return 0.0;
        }
        self.pixel_width as f32 / fov
    }
}

/// How a lens would fare on the symbol currently in frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LensStatus {
    /// Comfortable sampling — at or above the target.
    Good,
    /// Decodable, but below the target where occlusion detection gets reliable.
    Marginal,
    /// Below the decode threshold. The symbol will most likely fail on this lens.
    TooCoarse,
    /// The symbol would overflow this lens's field of view, taking its finder patterns with it.
    WouldCrop,
    /// Autofocus could not lock here — in practice, closer than this lens can focus.
    CannotFocus,
    /// No measurement yet, so nothing can be said.
    Unknown,
}

impl LensStatus {
    /// Whether choosing this lens is likely to produce a decode.
    pub fn is_usable(self) -> bool {
        matches!(self, LensStatus::Good | LensStatus::Marginal)
    }
}

/// One entry in the picker.
#[derive(Clone, Debug, PartialEq)]
pub struct LensOption {
    pub id: String,
    pub label: String,
    pub focal_length_mm: f32,
    /// Pixels per module this lens would deliver, given the current measurement. `0.0` when
    /// unknown.
    pub predicted_px_per_module: f32,
    /// Share of the frame width the symbol would occupy, `0.0..`. Above 1.0 it does not fit.
    pub frame_fill: f32,
    pub status: LensStatus,
    pub is_current: bool,
}

/// Thresholds for annotating the options.
#[derive(Clone, Copy, Debug)]
pub struct PickerParams {
    /// Pixels per module below which decoding is unlikely.
    pub min_px_per_module: f32,
    /// Pixels per module at and above which sampling is comfortable.
    pub target_px_per_module: f32,
    /// Largest share of frame width the symbol may occupy before the lens is flagged
    /// [`LensStatus::WouldCrop`]. Leaves room for hand shake and the quiet zone the detector needs.
    pub max_frame_fill: f32,
}

impl Default for PickerParams {
    fn default() -> Self {
        Self {
            min_px_per_module: 4.0,
            target_px_per_module: 6.0,
            max_frame_fill: 0.85,
        }
    }
}

/// Presents the available lenses, annotated. Chooses nothing.
#[derive(Clone, Debug)]
pub struct LensPicker {
    lenses: Vec<LensSpec>,
    params: PickerParams,
    focus_failed: Vec<String>,
}

impl LensPicker {
    /// Lenses are held in focal-length order, which is the order a picker should show them: wide on
    /// the left, tele on the right, matching every phone camera UI ever built.
    pub fn new(mut lenses: Vec<LensSpec>, params: PickerParams) -> Self {
        lenses.sort_by(|a, b| {
            a.focal_length_mm
                .partial_cmp(&b.focal_length_mm)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Self {
            lenses,
            params,
            focus_failed: Vec::new(),
        }
    }

    pub fn lenses(&self) -> &[LensSpec] {
        &self.lenses
    }

    /// Record that autofocus could not lock on `id`, so the picker can say *why* a lens is a poor
    /// choice right now rather than leaving the user to wonder.
    pub fn note_focus_failure(&mut self, id: &str) {
        if !self.focus_failed.iter().any(|f| f == id) {
            self.focus_failed.push(id.to_string());
        }
    }

    /// Clear focus failures — on a successful decode, or when the symbol is lost and reacquired.
    pub fn clear_focus_failures(&mut self) {
        self.focus_failed.clear();
    }

    /// Annotate every lens for the symbol currently in frame.
    ///
    /// - `current_id` — the physical camera in use.
    /// - `measured_px_per_module` — from the detected finder pattern's run lengths, or gradient
    ///   autocorrelation before detection succeeds. Pass `0.0` when nothing has been measured yet;
    ///   every option comes back [`LensStatus::Unknown`], which is honest rather than invented.
    /// - `symbol_modules` — the symbol's width in modules (21 for QR version 1, 177 for version 40).
    ///
    /// Returned in focal-length order, ready to render as-is.
    pub fn options(
        &self,
        current_id: &str,
        measured_px_per_module: f32,
        symbol_modules: u32,
    ) -> Vec<LensOption> {
        let scene = self.module_subtense(current_id, measured_px_per_module);

        self.lenses
            .iter()
            .map(|lens| {
                let is_current = lens.id == current_id;
                let focus_failed = self.focus_failed.iter().any(|f| *f == lens.id);

                let Some(module_rad) = scene else {
                    return LensOption {
                        id: lens.id.clone(),
                        label: lens.label.clone(),
                        focal_length_mm: lens.focal_length_mm,
                        predicted_px_per_module: 0.0,
                        frame_fill: 0.0,
                        status: if focus_failed {
                            LensStatus::CannotFocus
                        } else {
                            LensStatus::Unknown
                        },
                        is_current,
                    };
                };

                let predicted = module_rad * lens.px_per_radian();
                let hfov = lens.hfov_rad();
                let frame_fill = if hfov > 0.0 {
                    module_rad * symbol_modules as f32 / hfov
                } else {
                    0.0
                };

                let status = if focus_failed {
                    LensStatus::CannotFocus
                } else if frame_fill > self.params.max_frame_fill {
                    LensStatus::WouldCrop
                } else if predicted < self.params.min_px_per_module {
                    LensStatus::TooCoarse
                } else if predicted < self.params.target_px_per_module {
                    LensStatus::Marginal
                } else {
                    LensStatus::Good
                };

                LensOption {
                    id: lens.id.clone(),
                    label: lens.label.clone(),
                    focal_length_mm: lens.focal_length_mm,
                    predicted_px_per_module: predicted,
                    frame_fill,
                    status,
                    is_current,
                }
            })
            .collect()
    }

    /// The lens that would sample the symbol best, for the UI to *highlight*.
    ///
    /// Advisory only. Nothing in barclean calls this to change lenses — it exists so the picker can
    /// mark one option as the likely best pick, leaving the decision where it belongs. Returns
    /// `None` when nothing has been measured or no lens is usable.
    pub fn suggestion(&self, options: &[LensOption]) -> Option<String> {
        options
            .iter()
            .filter(|o| o.status.is_usable())
            .max_by(|a, b| {
                a.predicted_px_per_module
                    .partial_cmp(&b.predicted_px_per_module)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|o| o.id.clone())
    }

    /// How much angle one module subtends, from the current lens's measurement.
    ///
    /// This is the scene-dependent quantity that transfers between lenses; everything else is
    /// derived from it.
    fn module_subtense(&self, current_id: &str, measured_px_per_module: f32) -> Option<f32> {
        if measured_px_per_module <= 0.0 {
            return None;
        }
        let current = self.lenses.iter().find(|l| l.id == current_id)?;
        let ppr = current.px_per_radian();
        (ppr > 0.0).then(|| measured_px_per_module / ppr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Roughly a Pixel 8 Pro's rear cluster on a 1280-wide preview stream.
    fn phone_lenses() -> Vec<LensSpec> {
        vec![
            LensSpec {
                id: "tele".into(),
                label: "5×".into(),
                focal_length_mm: 18.0,
                sensor_width_mm: 5.6,
                pixel_width: 1280,
                min_focus_distance_m: 0.35,
            },
            LensSpec {
                id: "ultrawide".into(),
                label: "Ultra-wide".into(),
                focal_length_mm: 2.2,
                sensor_width_mm: 5.6,
                pixel_width: 1280,
                min_focus_distance_m: 0.10,
            },
            LensSpec {
                id: "main".into(),
                label: "Main".into(),
                focal_length_mm: 6.9,
                sensor_width_mm: 9.8,
                pixel_width: 1280,
                min_focus_distance_m: 0.10,
            },
        ]
    }

    fn picker() -> LensPicker {
        LensPicker::new(phone_lenses(), PickerParams::default())
    }

    fn find<'a>(options: &'a [LensOption], id: &str) -> &'a LensOption {
        options.iter().find(|o| o.id == id).expect("lens present")
    }

    #[test]
    fn options_are_ordered_wide_to_tele() {
        // Picker order is UI order, and every phone camera UI puts wide on the left.
        let p = picker();
        let ids: Vec<&str> = p.lenses().iter().map(|l| l.id.as_str()).collect();
        assert_eq!(ids, vec!["ultrawide", "main", "tele"]);

        let options = p.options("main", 4.0, 25);
        let option_ids: Vec<&str> = options.iter().map(|o| o.id.as_str()).collect();
        assert_eq!(option_ids, vec!["ultrawide", "main", "tele"]);
    }

    #[test]
    fn every_lens_is_offered_however_bad() {
        // A selectable picker must never hide an option. The user may have a reason we cannot see.
        let options = picker().options("main", 0.2, 177);
        assert_eq!(options.len(), 3, "all lenses stay on offer");
        assert!(
            options.iter().any(|o| !o.status.is_usable()),
            "this fixture should include an unusable lens"
        );
    }

    #[test]
    fn narrower_lens_predicts_more_pixels_per_module() {
        let options = picker().options("main", 4.0, 25);
        let uw = find(&options, "ultrawide").predicted_px_per_module;
        let main = find(&options, "main").predicted_px_per_module;
        let tele = find(&options, "tele").predicted_px_per_module;

        assert!((main - 4.0).abs() < 0.01, "the measured lens reports what was measured");
        assert!(tele > main, "tele resolves the symbol more finely");
        assert!(main > uw, "main resolves it more finely than ultra-wide");
    }

    #[test]
    fn statuses_bracket_the_thresholds() {
        let options = picker().options("main", 4.0, 25);
        // 4.0 px/module is decodable but under the 6.0 target.
        assert_eq!(find(&options, "main").status, LensStatus::Marginal);
        // Tele triples it, clearing the target.
        assert_eq!(find(&options, "tele").status, LensStatus::Good);
        // Ultra-wide drops well under the 4.0 floor.
        assert_eq!(find(&options, "ultrawide").status, LensStatus::TooCoarse);
    }

    #[test]
    fn a_symbol_too_large_for_a_lens_is_flagged_would_crop() {
        // A 177-module version 40 at 4 px/module on this preview stream subtends more than the
        // tele's field of view; choosing it would slice off the finder patterns.
        let options = picker().options("main", 4.0, 177);
        let tele = find(&options, "tele");
        assert_eq!(tele.status, LensStatus::WouldCrop);
        assert!(tele.frame_fill > 1.0, "frame fill was {}", tele.frame_fill);
        // The lens is still listed, just labelled.
        assert!(options.iter().any(|o| o.id == "tele"));
    }

    #[test]
    fn without_a_measurement_nothing_is_claimed() {
        let options = picker().options("main", 0.0, 25);
        for o in &options {
            assert_eq!(o.status, LensStatus::Unknown);
            assert_eq!(o.predicted_px_per_module, 0.0);
        }
    }

    #[test]
    fn unknown_current_lens_yields_no_predictions() {
        let options = picker().options("nonexistent", 4.0, 25);
        assert!(options.iter().all(|o| o.status == LensStatus::Unknown));
    }

    #[test]
    fn focus_failure_is_reported_not_hidden() {
        let mut p = picker();
        p.note_focus_failure("tele");

        let options = p.options("main", 4.0, 25);
        let tele = find(&options, "tele");
        assert_eq!(
            tele.status,
            LensStatus::CannotFocus,
            "the user should see why the tele is a poor choice, not find it missing"
        );
        assert!(options.iter().any(|o| o.id == "tele"), "still selectable");

        p.clear_focus_failures();
        assert_eq!(find(&p.options("main", 4.0, 25), "tele").status, LensStatus::Good);
    }

    #[test]
    fn current_lens_is_marked() {
        let options = picker().options("main", 4.0, 25);
        assert!(find(&options, "main").is_current);
        assert!(!find(&options, "tele").is_current);
        assert_eq!(options.iter().filter(|o| o.is_current).count(), 1);
    }

    #[test]
    fn suggestion_is_advisory_and_never_picks_an_unusable_lens() {
        let p = picker();

        let options = p.options("main", 4.0, 25);
        assert_eq!(p.suggestion(&options).as_deref(), Some("tele"));

        // With a symbol too large for the tele, the suggestion must fall back rather than
        // recommend a lens that would crop it.
        let options = p.options("main", 4.0, 177);
        let suggested = p.suggestion(&options);
        assert_ne!(suggested.as_deref(), Some("tele"));

        // Nothing measured, nothing suggested.
        assert_eq!(p.suggestion(&p.options("main", 0.0, 25)), None);
    }

    #[test]
    fn degenerate_specs_do_not_divide_by_zero() {
        let bad = LensSpec {
            id: "bad".into(),
            label: "Broken".into(),
            focal_length_mm: 0.0,
            sensor_width_mm: 0.0,
            pixel_width: 0,
            min_focus_distance_m: 0.0,
        };
        assert_eq!(bad.hfov_rad(), 0.0);
        assert_eq!(bad.px_per_radian(), 0.0);

        let p = LensPicker::new(vec![bad], PickerParams::default());
        let options = p.options("bad", 4.0, 25);
        assert_eq!(options[0].status, LensStatus::Unknown);
    }
}
