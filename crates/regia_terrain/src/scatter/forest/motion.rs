// SPDX-License-Identifier: MPL-2.0
//! Scatter LOD for a fast camera (ADR-1503): look ahead along the eye's travel, keep the near
//! field at full detail, and check a set's fade and streaming ranges against a flight speed.
//!
//! The scatter field picks an instance's LOD from its projected size with hysteresis and
//! cross-fades a switch over `LOD_FADE_FRAMES` frames (ADR-208 family). A slow camera never notices
//! the fade's lag. At 100 m/s and 60 fps the eye moves 13 m during one fade, so a tree rushing at
//! the camera starts its LOD 1 → 0 fade late and arrives on screen big while still blending from
//! the coarse mesh: the switch is seen. Two fixes, both opt-in ([`MotionLod`]):
//!
//! - **Lookahead**: the LOD is chosen for where the instance will be `lookahead_frames` frames
//!   from now along the eye's last step (`closing` speed = the step's component toward the
//!   instance). With the default (the fade length), a switch toward a finer LOD completes by
//!   the time the instance reaches the distance the switch was meant for: switches happen
//!   farther away, smaller on screen. Receding instances are not changed (they switch behind
//!   the camera, where the field switches at once).
//! - **Full-detail radius**: instances whose surface is nearer than `full_detail_m` always draw
//!   LOD 0, so no switch ever happens in the band where trunks sweep past the lens.
//!
//! A step longer than `max_step_m` is a camera cut, not motion: that frame has no lookahead.
//! Everything is a function of the eye positions given to `ScatterField::update`, so a scripted
//! flight renders the same draws on every run.

use super::super::field::{LOD_FADE_FRAMES, ScatterLibrary, TILE_SIZE_M};
use super::super::set::ScatterSet;
use super::shape::ObstacleShape;

/// Speed-aware LOD selection for [`crate::ScatterField::set_motion_lod`].
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct MotionLod {
    /// Frames of eye travel the LOD choice looks ahead (0 = off).
    pub lookahead_frames: f32,
    /// Instances whose bounding sphere comes nearer than this draw LOD 0, metres (0 = off).
    pub full_detail_m: f32,
    /// An eye step longer than this (metres per frame) is a cut: no lookahead that frame.
    pub max_step_m: f32,
}

impl Default for MotionLod {
    fn default() -> Self {
        Self {
            lookahead_frames: LOD_FADE_FRAMES as f32,
            full_detail_m: 0.0,
            max_step_m: 50.0,
        }
    }
}

impl MotionLod {
    /// Finite, non-negative values (a bad `max_step_m` falls back to the default).
    #[must_use]
    pub fn sanitized(self) -> Self {
        let pos = |v: f32| if v.is_finite() && v > 0.0 { v } else { 0.0 };
        Self {
            lookahead_frames: pos(self.lookahead_frames).min(240.0),
            full_detail_m: pos(self.full_detail_m),
            max_step_m: if self.max_step_m.is_finite() && self.max_step_m > 0.0 {
                self.max_step_m
            } else {
                Self::default().max_step_m
            },
        }
    }

    /// The projected size the LOD is chosen at. `size` is the instance's projected size at its
    /// distance `d` (metres, centre to eye), `radius` its bounding radius, `closing` how many
    /// metres per frame it comes nearer (negative when receding).
    #[must_use]
    pub fn lod_size(&self, size: f32, d: f32, radius: f32, closing: f32) -> f32 {
        if self.full_detail_m > 0.0 && d - radius < self.full_detail_m {
            return f32::MAX;
        }
        if closing <= 0.0 || self.lookahead_frames <= 0.0 || !closing.is_finite() {
            return size;
        }
        let ahead = (d - closing * self.lookahead_frames).max(radius.max(0.5));
        if ahead < d { size * (d / ahead) } else { size }
    }

    /// The recommendation for a flight at `speed_m_s` and `fps`: look ahead one fade, keep the
    /// nearest third of a second (at least 10 m) at full detail, and treat a step of four
    /// frames' travel (at least 5 m) as a cut.
    #[must_use]
    pub fn for_flight(speed_m_s: f32, fps: f32) -> Self {
        let speed = speed_m_s.max(0.0);
        let step = speed / fps.max(1.0);
        Self {
            lookahead_frames: LOD_FADE_FRAMES as f32,
            full_detail_m: (speed / 3.0).max(10.0),
            max_step_m: (4.0 * step).max(5.0),
        }
    }
}

/// A planned flight to check a scatter set against.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct FlightPlan {
    /// Top camera speed, metres per second.
    pub speed_m_s: f32,
    /// Frames per second of the output.
    pub fps: f32,
    /// Output height, pixels.
    pub height_px: f32,
    /// Vertical field of view, radians.
    pub fov_y: f32,
}

impl Default for FlightPlan {
    fn default() -> Self {
        Self {
            speed_m_s: 100.0,
            fps: 60.0,
            height_px: 2160.0,
            fov_y: 50f32.to_radians(),
        }
    }
}

/// Shortest time (seconds) a fade or impostor band should take to cross: below it the dissolve
/// lasts fewer than ~15 frames at 60 fps and reads as a pop.
pub const MIN_BAND_CROSSING_S: f32 = 0.25;
/// Shortest lead (seconds) the background tile prefetch should have over the camera.
pub const MIN_PREFETCH_LEAD_S: f32 = 0.5;
/// Nearest (seconds ahead of the camera) a LOD 0 switch should happen without a full-detail
/// radius.
pub const MIN_LOD0_SWITCH_S: f32 = 0.5;

/// How one class fares at the planned speed.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct ClassTiming {
    /// Class name.
    pub class: String,
    /// Width of the fade band (mesh to impostor, or shrink-out), metres.
    pub fade_band_m: f32,
    /// Seconds a head-on approach takes to cross the band.
    pub band_crossing_s: f32,
    /// Seconds of travel the background prefetch ring (one tile side) stays ahead.
    pub prefetch_lead_s: f32,
    /// Distance where the class's typical instance leaves LOD 0, metres (`None`: no library
    /// given, or a one-LOD mesh).
    pub lod0_end_m: Option<f32>,
    /// Metres the camera travels during one LOD cross-fade.
    pub lod_fade_travel_m: f32,
    /// Warning codes: `fade_band_short`, `prefetch_lead_short`, `lod0_switch_near`.
    pub warnings: Vec<&'static str>,
    /// Fade band (metres) that would cross in [`MIN_BAND_CROSSING_S`].
    pub recommended_fade_band_m: f32,
}

/// The result of [`flight_check`].
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct FlightCheck {
    /// The plan checked.
    pub plan: FlightPlan,
    /// One entry per class.
    pub classes: Vec<ClassTiming>,
    /// Motion LOD settings for the plan ([`MotionLod::for_flight`]).
    pub motion: MotionLod,
    /// Classes with at least one warning.
    pub warned_classes: u32,
}

/// Checks every class of `set` against `plan`: fade-band crossing time, background prefetch
/// lead and (with `library`, for the meshes' LOD switch sizes) how close to the camera LOD 0
/// ends.
#[must_use]
pub fn flight_check(set: &ScatterSet, library: Option<&ScatterLibrary>, plan: &FlightPlan) -> FlightCheck {
    let speed = plan.speed_m_s.max(1e-3);
    let fps = plan.fps.max(1.0);
    let inv_tan = 1.0 / (plan.fov_y * 0.5).tan().max(1e-4);
    let mut classes = Vec::with_capacity(set.classes.len());
    for class in &set.classes {
        let band = (class.fade_end - class.fade_start).max(0.0);
        let side = ((TILE_SIZE_M / class.cell_size).round().max(1.0)) * class.cell_size;
        let lod0_end_m = library.and_then(|lib| {
            class
                .meshes
                .iter()
                .filter_map(|m| {
                    let asset = lib.get(&m.mesh)?;
                    let switch = asset.lods.first().filter(|_| asset.lods.len() > 1)?.screen_size;
                    let shape = ObstacleShape::of_asset(asset);
                    let half_height = 0.5 * (shape.top - shape.bottom);
                    let radius = (shape.reach * shape.reach + half_height * half_height).sqrt();
                    let scale = 0.5 * (m.min_scale + m.max_scale);
                    (switch > 0.0).then(|| radius * scale * inv_tan * class.lod_bias / switch)
                })
                .reduce(f32::min)
        });
        let mut warnings = Vec::new();
        let crossing = band / speed;
        if crossing < MIN_BAND_CROSSING_S {
            warnings.push("fade_band_short");
        }
        let lead = side / speed;
        if lead < MIN_PREFETCH_LEAD_S {
            warnings.push("prefetch_lead_short");
        }
        if lod0_end_m.is_some_and(|d| d < speed * MIN_LOD0_SWITCH_S) {
            warnings.push("lod0_switch_near");
        }
        classes.push(ClassTiming {
            class: class.name.clone(),
            fade_band_m: band,
            band_crossing_s: crossing,
            prefetch_lead_s: lead,
            lod0_end_m,
            lod_fade_travel_m: speed / fps * LOD_FADE_FRAMES as f32,
            warnings,
            recommended_fade_band_m: speed * MIN_BAND_CROSSING_S,
        });
    }
    FlightCheck {
        plan: *plan,
        warned_classes: classes.iter().filter(|c| !c.warnings.is_empty()).count() as u32,
        classes,
        motion: MotionLod::for_flight(plan.speed_m_s, plan.fps),
    }
}

/// One class's fade band before and after [`tune_fade_bands`].
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct BandChange {
    /// Class name.
    pub class: String,
    /// `fade_start` before, metres.
    pub old_start: f32,
    /// `fade_start` after, metres (`fade_end` is kept).
    pub new_start: f32,
}

/// Widens every class's fade band (mesh to impostor, or shrink-out) that a head-on approach at
/// `plan.speed_m_s` would cross in under [`MIN_BAND_CROSSING_S`]: `fade_start` moves toward the
/// camera so the band is `speed × MIN_BAND_CROSSING_S` wide, `fade_end` (the streaming range) is
/// kept, and `fade_start` never goes below a fifth of `fade_end`. Returns the classes changed.
/// The edit is to `.scatter.ron` values the format already has.
pub fn tune_fade_bands(set: &mut ScatterSet, plan: &FlightPlan) -> Vec<BandChange> {
    let want = plan.speed_m_s.max(0.0) * MIN_BAND_CROSSING_S;
    let mut changed = Vec::new();
    for class in &mut set.classes {
        let band = class.fade_end - class.fade_start;
        if band >= want {
            continue;
        }
        let new_start = (class.fade_end - want).max(0.2 * class.fade_end);
        if new_start < class.fade_start {
            changed.push(BandChange {
                class: class.name.clone(),
                old_start: class.fade_start,
                new_start,
            });
            class.fade_start = new_start;
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scatter::set::{ScatterClass, ScatterMeshRef};

    #[test]
    fn lookahead_only_grows_approaching_sizes() {
        let m = MotionLod::default();
        // Approaching at 1.6 m/frame from 50 m: chosen as if 12.8 m nearer.
        let s = m.lod_size(0.1, 50.0, 2.0, 1.6);
        assert!((s - 0.1 * 50.0 / (50.0 - 12.8)).abs() < 1e-6);
        // Receding or still: unchanged.
        assert_eq!(m.lod_size(0.1, 50.0, 2.0, -1.6), 0.1);
        assert_eq!(m.lod_size(0.1, 50.0, 2.0, 0.0), 0.1);
        // Never projects past the instance's own radius.
        let near = m.lod_size(0.1, 5.0, 2.0, 10.0);
        assert!((near - 0.1 * 5.0 / 2.0).abs() < 1e-6);
        // Full-detail radius forces LOD 0.
        let full = MotionLod {
            full_detail_m: 20.0,
            ..m
        };
        assert_eq!(full.lod_size(0.001, 21.0, 2.0, 0.0), f32::MAX);
        assert_eq!(full.lod_size(0.001, 30.0, 2.0, 0.0), 0.001);
    }

    #[test]
    fn sanitizing_rejects_bad_values() {
        let m = MotionLod {
            lookahead_frames: f32::NAN,
            full_detail_m: -3.0,
            max_step_m: 0.0,
        }
        .sanitized();
        assert_eq!(m.lookahead_frames, 0.0);
        assert_eq!(m.full_detail_m, 0.0);
        assert_eq!(m.max_step_m, MotionLod::default().max_step_m);
    }

    #[test]
    fn the_check_flags_short_bands_and_prefetch_at_speed() {
        let set = ScatterSet {
            classes: vec![
                ScatterClass {
                    name: "trees".into(),
                    meshes: vec![ScatterMeshRef {
                        mesh: "tree.glb".into(),
                        weight: 1.0,
                        min_scale: 1.0,
                        max_scale: 1.0,
                        scale_bias: 1.0,
                    }],
                    cell_size: 4.0,
                    fade_start: 300.0,
                    fade_end: 380.0,
                    ..ScatterClass::default()
                },
                ScatterClass {
                    name: "brush".into(),
                    meshes: vec![ScatterMeshRef {
                        mesh: "bush.glb".into(),
                        weight: 1.0,
                        min_scale: 1.0,
                        max_scale: 1.0,
                        scale_bias: 1.0,
                    }],
                    cell_size: 2.0,
                    fade_start: 90.0,
                    fade_end: 100.0,
                    ..ScatterClass::default()
                },
            ],
            ..ScatterSet::default()
        };
        let slow = flight_check(&set, None, &FlightPlan::default());
        assert!(slow.classes[0].warnings.is_empty(), "{:?}", slow.classes[0]);
        assert_eq!(slow.classes[1].warnings, vec!["fade_band_short"]);
        let fast = flight_check(
            &set,
            None,
            &FlightPlan {
                speed_m_s: 750.0,
                ..FlightPlan::default()
            },
        );
        assert!(fast.classes[0].warnings.contains(&"prefetch_lead_short"));
        assert!(fast.classes[0].warnings.contains(&"fade_band_short"));
        assert_eq!(fast.warned_classes, 2);
        assert!((fast.classes[0].recommended_fade_band_m - 187.5).abs() < 1e-3);
        assert!(fast.motion.full_detail_m >= 250.0 - 1e-3);
        // Tuning widens the short bands until the check passes them.
        let mut tuned = set.clone();
        let plan = FlightPlan {
            speed_m_s: 200.0,
            ..FlightPlan::default()
        };
        let changes = tune_fade_bands(&mut tuned, &plan);
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].class, "brush");
        assert!((tuned.classes[1].fade_start - 50.0).abs() < 1e-4);
        assert_eq!(tuned.classes[1].fade_end, 100.0);
        assert_eq!(tuned.classes[0], set.classes[0]);
        let check = flight_check(&tuned, None, &plan);
        assert!(
            check
                .classes
                .iter()
                .all(|c| !c.warnings.contains(&"fade_band_short"))
        );
        // Never below a fifth of the range.
        let mut extreme = set.clone();
        tune_fade_bands(
            &mut extreme,
            &FlightPlan {
                speed_m_s: 10_000.0,
                ..FlightPlan::default()
            },
        );
        assert!((extreme.classes[1].fade_start - 20.0).abs() < 1e-4);
    }
}
