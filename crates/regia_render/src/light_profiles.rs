// SPDX-License-Identifier: MPL-2.0
//! IES photometric profiles on point and spot lights, and authored rect and tube lights: what the frame takes
//! from the scene (ADR-2592).
//!
//! **Profiles.** A [`crate::LightProfile`] names a cooked profile ([`regia_asset::ies::CookedIesProfile`]) in the
//! [`LightProfileLibrary`]. A frame's [`LightExtras`] interns the profiles its lights use into at most
//! [`MAX_PROFILE_SLOTS`] slots. Each slot is a 256 × 32 band of the emitter probe atlas (binding 33 of the forward
//! pass's frame group, so no new binding): [`LightExtras::write_profile_bands`] writes the bands at the bottom of
//! the atlas, and [`LightExtras::adjust_record`] tags the light's GPU record. The tag is a fraction added to the
//! record's kind (`kind + code / 4096`, under one half), so every shader that reads the kind as `u32(w + 0.5)` and
//! every emitter test (`w > 3.5`) is unchanged; a light without a profile keeps a whole kind and its record is
//! bit-identical. `pbr_lighting.wgsl`'s `light_profile_relative` reads the band; [`relative_gpu`] is its CPU
//! mirror. The forward pass (`lab_forward.wgsl`) applies it on Desktop; the Mobile forward, the terrain's shared
//! light pass and volumetric fog ignore profiles (Known issues, ADR-2592).
//!
//! **Area lights.** [`crate::RectLight`] and [`crate::TubeLight`] become [`crate::emissive_lights::EmissiveLight`]
//! quads and tubes ([`rect_emitter`], [`tube_emitter`]) and are shaded by the fitted-emitter path (the GGX LTC
//! tables, `forward_ltc.wgsl`). Their shadow is the fitted emitters' own: a 16 × 16 distance probe traced once
//! against the scene from the light (static occluders), the nearest equivalent the emitter path has.

use std::collections::HashMap;
use std::sync::Arc;

use regia_app::prelude::*;
use regia_asset::ies::{COOKED_HORIZONTAL, COOKED_VERTICAL, CookedIesProfile};
use regia_core::glam::{Mat4, Quat, Vec3};

use crate::emissive_lights::{EmissiveLight, EmitterShape, PROBE_ATLAS_COLUMNS, PROBE_RES};

/// Profile slots a frame can hold (codes 1..=7 in the kind fraction).
pub const MAX_PROFILE_SLOTS: usize = 7;

/// Atlas texel rows of one profile band (the cooked horizontal sample count).
pub const PROFILE_BAND_ROWS: u32 = COOKED_HORIZONTAL as u32;

/// Steps of the horizontal turn a record carries (`code >> 3`).
pub const PROFILE_TURN_STEPS: u32 = 256;

const _: () = assert!(COOKED_VERTICAL as u32 == PROBE_ATLAS_COLUMNS * PROBE_RES);
const _: () = assert!(PROFILE_BAND_ROWS.is_multiple_of(PROBE_RES));

/// Texel rows of the emitter probe atlas (`frame/gpu_upload.rs` `create_probe_atlas`).
#[must_use]
pub fn atlas_rows() -> u32 {
    crate::MAX_UPLOADED_LIGHTS.div_ceil(PROBE_ATLAS_COLUMNS) * PROBE_RES
}

/// The first atlas row of profile slot `slot` (1-based).
#[must_use]
pub fn band_first_row(slot: u32) -> u32 {
    atlas_rows() - PROFILE_BAND_ROWS * slot
}

/// Cooked profiles by name (the `.ies` asset path, or any name an agent gave `render.set_light_profile`).
#[derive(Resource, Clone, Debug, Default)]
pub struct LightProfileLibrary {
    profiles: HashMap<String, Arc<CookedIesProfile>>,
}

impl LightProfileLibrary {
    /// Adds or replaces `name`.
    pub fn insert(&mut self, name: impl Into<String>, profile: CookedIesProfile) {
        self.profiles.insert(name.into(), Arc::new(profile));
    }

    /// Parses LM-63 text and adds it as `name`.
    ///
    /// # Errors
    /// The parse or cook error.
    pub fn insert_ies_text(
        &mut self,
        name: impl Into<String>,
        text: &str,
    ) -> Result<(), regia_asset::ies::IesError> {
        let file = regia_asset::ies::IesFile::parse(text)?;
        self.insert(name, CookedIesProfile::from_file(&file)?);
        Ok(())
    }

    /// The profile called `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Arc<CookedIesProfile>> {
        self.profiles.get(name)
    }

    /// Profile names, sorted.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.profiles.keys().cloned().collect();
        names.sort();
        names
    }

    /// Removes `name`; whether it was there.
    pub fn remove(&mut self, name: &str) -> bool {
        self.profiles.remove(name).is_some()
    }
}

/// How one light uses its profile in a frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProfileUse {
    /// Slot, 1..=[`MAX_PROFILE_SLOTS`].
    pub slot: u8,
    /// Horizontal turn in [`PROFILE_TURN_STEPS`] steps.
    pub turn: u8,
    /// The light's nadir (a spot's direction; a point light's rotated `-Y`).
    pub nadir: [f32; 3],
    /// Intensity at a relative 1.0, cd, replacing the record's (`None`: the light's own, a spot's candela).
    pub peak_cd: Option<f32>,
}

/// One authored area light for the frame: the emitter, its range (≤ 0: from the frame's cutoff, as a fitted
/// emitter) and whether it traces a distance-probe shadow.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AreaLightInput {
    /// The emitter (world space).
    pub light: EmissiveLight,
    /// Influence range, metres; ≤ 0 takes the fitted emitters' cutoff range.
    pub range: f32,
    /// Trace a distance probe against the scene (the fitted emitters' shadow).
    pub cast_shadows: bool,
}

/// What a frame takes besides its lights: the profiles of its point and spot lights and its authored area
/// lights (ADR-2592). The default (no profile, no area light) changes nothing.
#[derive(Clone, Debug, Default)]
pub struct LightExtras {
    /// Profiles by slot (slot `i + 1`).
    pub profiles: Vec<Arc<CookedIesProfile>>,
    /// Per light of the frame's `local_lights` (same index; shorter means the rest have none).
    pub per_light: Vec<Option<ProfileUse>>,
    /// Rect and tube lights.
    pub area_lights: Vec<AreaLightInput>,
    /// Per light of the frame's `local_lights`: shade its sphere through the LTC tables ([`crate::LightSourceLtc`];
    /// same index; shorter means the rest do not).
    pub ltc_spheres: Vec<bool>,
    /// The frame's uploaded lights as indices into its `local_lights` ([`Self::note_selection`]).
    uploaded_from: Vec<usize>,
    /// Point and spot lights the extraction appended to the frame's `local_lights` as stand-ins for its rect and
    /// tube lights (`RenderProfile::area_light_fallback`, [`area_light_fallback`], ADR-3247): the fallback's
    /// cost counter, 0 when it is off or the tier has an emitter budget.
    pub area_fallback_lights: u32,
}

impl LightExtras {
    /// Nothing to do: the frame is unchanged.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.profiles.is_empty() && self.area_lights.is_empty() && self.ltc_spheres.is_empty()
    }

    /// Records that light `index` of the frame shades its sphere through the LTC tables.
    pub fn mark_ltc_sphere(&mut self, index: usize) {
        if self.ltc_spheres.len() <= index {
            self.ltc_spheres.resize(index + 1, false);
        }
        self.ltc_spheres[index] = true;
    }

    /// The slot of `profile`, interned (by pointer, then content), or `None` when the slots are full.
    pub fn intern(&mut self, profile: &Arc<CookedIesProfile>) -> Option<u8> {
        if let Some(i) = self
            .profiles
            .iter()
            .position(|p| Arc::ptr_eq(p, profile) || **p == **profile)
        {
            return Some(i as u8 + 1);
        }
        if self.profiles.len() >= MAX_PROFILE_SLOTS {
            return None;
        }
        self.profiles.push(profile.clone());
        Some(self.profiles.len() as u8)
    }

    /// Records that light `index` of the frame uses `profile` (see [`profile_use`]); `false` when the slots are
    /// full (the light then shades without it).
    pub fn assign(&mut self, index: usize, profile: &Arc<CookedIesProfile>, mut uses: ProfileUse) -> bool {
        let Some(slot) = self.intern(profile) else {
            return false;
        };
        uses.slot = slot;
        if self.per_light.len() <= index {
            self.per_light.resize(index + 1, None);
        }
        self.per_light[index] = Some(uses);
        true
    }

    /// Remembers which of the frame's `local_lights` were uploaded, in upload order (the light budget's kept
    /// indices), for [`Self::adjust_uploaded`].
    pub fn note_selection(&mut self, kept: &[usize]) {
        self.uploaded_from.clear();
        if !self.per_light.is_empty() || !self.ltc_spheres.is_empty() {
            self.uploaded_from.extend_from_slice(kept);
        }
    }

    /// [`Self::adjust_record`] for the `i`-th uploaded light.
    pub fn adjust_uploaded(&self, i: usize, record: &mut [[f32; 4]; 4]) {
        if let Some(&index) = self.uploaded_from.get(i) {
            self.adjust_record(index, record);
        }
    }

    /// Emitter probe tiles the fitted and authored emitters may use with this frame's bands below them.
    #[must_use]
    pub fn emitter_tile_cap(&self) -> u32 {
        let band_tiles = PROFILE_BAND_ROWS / PROBE_RES * PROBE_ATLAS_COLUMNS;
        crate::MAX_UPLOADED_LIGHTS.saturating_sub(band_tiles * self.profiles.len() as u32)
    }

    /// Tags the GPU record (`GpuLight` as four `vec4`s) of light `index`: the kind fraction, the nadir in
    /// `direction_radius.xyz` (point lights; a spot's is already its direction) and, with a peak, the intensity.
    /// A light marked [`Self::mark_ltc_sphere`] with a source radius has that radius negated (`cone_shadow.w`: its
    /// magnitude is still the radius every reader takes, its sign selects the forward pass's LTC branch). A light
    /// with neither is left untouched (bit-identical).
    pub fn adjust_record(&self, index: usize, record: &mut [[f32; 4]; 4]) {
        let kind = record[0][3];
        if !(0.5..2.5).contains(&kind) || kind.fract() != 0.0 {
            return;
        }
        if self.ltc_spheres.get(index).copied().unwrap_or(false) && record[3][3] > 0.0 {
            record[3][3] = -record[3][3];
        }
        let Some(Some(uses)) = self.per_light.get(index) else {
            return;
        };
        record[0][3] = kind + kind_fraction(uses.slot, uses.turn);
        if kind < 1.5 {
            record[2][0] = uses.nadir[0];
            record[2][1] = uses.nadir[1];
            record[2][2] = uses.nadir[2];
        }
        if let Some(peak) = uses.peak_cd {
            record[1][3] = peak;
        }
    }

    /// Writes the profile bands into an emitter probe atlas holding `probes` (row-major, [`PROBE_ATLAS_COLUMNS`]
    /// · [`PROBE_RES`] wide): grows it to the whole atlas and fills each slot's band. Returns the slots written;
    /// none when the emitters' tiles reach the bands (keep the emitter cap at [`Self::emitter_tile_cap`]).
    pub fn write_profile_bands(&self, probes: &mut Vec<f32>) -> usize {
        if self.profiles.is_empty() {
            return 0;
        }
        let width = (PROBE_ATLAS_COLUMNS * PROBE_RES) as usize;
        let rows = atlas_rows() as usize;
        let first_band = band_first_row(self.profiles.len() as u32) as usize;
        if probes.len() > first_band * width {
            tracing::warn!(
                "light profiles: the emitter probes reach the profile bands; profiles skipped this frame"
            );
            return 0;
        }
        probes.resize(rows * width, 0.0);
        for (i, profile) in self.profiles.iter().enumerate() {
            let row0 = band_first_row(i as u32 + 1) as usize;
            for j in 0..COOKED_HORIZONTAL {
                let src = &profile.values[j * COOKED_VERTICAL..(j + 1) * COOKED_VERTICAL];
                let dst = (row0 + j) * width;
                probes[dst..dst + width].copy_from_slice(src);
            }
        }
        self.profiles.len()
    }
}

/// The kind fraction of a record whose profile is in `slot` (1..=7) turned by `turn` steps.
#[must_use]
pub fn kind_fraction(slot: u8, turn: u8) -> f32 {
    f32::from(u16::from(slot & 7) + 8 * u16::from(turn)) / 4096.0
}

/// The profile code a record's kind carries (`light_profile_code` in `pbr_lighting.wgsl`).
#[must_use]
pub fn profile_code(kind_w: f32) -> u32 {
    (kind_w.fract() * 4096.0).round() as u32
}

/// The profile's horizontal 0° axis before the record's turn (`light_profile_axis` in the shader).
#[must_use]
pub fn profile_axis(nadir: Vec3) -> Vec3 {
    let up = if nadir.y.abs() > 0.999 { Vec3::X } else { Vec3::Y };
    up.cross(nadir).normalize_or_zero()
}

/// How a light with `nadir` and a horizontal 0° reference along `reference` uses a profile (slot filled in by
/// [`LightExtras::assign`]): the turn from [`profile_axis`] to `reference`, plus `rotation` radians.
#[must_use]
pub fn profile_use(nadir: Vec3, reference: Vec3, rotation: f32, peak_cd: Option<f32>) -> ProfileUse {
    let n = nadir.normalize_or(Vec3::NEG_Y);
    let a0 = profile_axis(n);
    let a1 = a0.cross(n);
    let r = reference - n * reference.dot(n);
    let base = if r.length_squared() > 1e-8 {
        r.dot(a1).atan2(r.dot(a0))
    } else {
        0.0
    };
    let turn = (base + rotation).rem_euclid(std::f32::consts::TAU) / std::f32::consts::TAU;
    let turn = ((turn * PROFILE_TURN_STEPS as f32).round() as u32 % PROFILE_TURN_STEPS) as u8;
    ProfileUse {
        slot: 0,
        turn,
        nadir: n.to_array(),
        peak_cd,
    }
}

/// The nadir and horizontal reference of a point light rotated by `rotation` (`-Y` and `+X` at rest).
#[must_use]
pub fn point_light_frame(rotation: Quat) -> (Vec3, Vec3) {
    (rotation * Vec3::NEG_Y, rotation * Vec3::X)
}

/// The nadir and horizontal reference of a spot light rotated by `rotation` (`-Z` and `+X` at rest).
#[must_use]
pub fn spot_light_frame(rotation: Quat) -> (Vec3, Vec3) {
    (rotation * Vec3::NEG_Z, rotation * Vec3::X)
}

/// CPU mirror of `light_profile_relative`: the relative intensity toward `d` (unit, light → receiver) of a light
/// whose record carries `code` and `nadir`, read from `profile`'s grid.
#[must_use]
pub fn relative_gpu(profile: &CookedIesProfile, code: u32, nadir: Vec3, d: Vec3) -> f32 {
    if code & 7 == 0 {
        return 1.0;
    }
    let n = nadir.normalize_or_zero();
    let a0 = profile_axis(n);
    let a1 = a0.cross(n);
    let turn = (code >> 3) as f32 * (std::f32::consts::TAU / PROFILE_TURN_STEPS as f32);
    let t0 = a0 * turn.cos() + a1 * turn.sin();
    let t1 = t0.cross(n);
    let theta = d.dot(n).clamp(-1.0, 1.0).acos().to_degrees();
    let phi = d.dot(t1).atan2(d.dot(t0)).to_degrees().rem_euclid(360.0);
    profile.relative_at_angles(theta, phi)
}

/// The peak intensity (cd) a point light of `lumens` with `profile` takes, so its flux stays `lumens`.
#[must_use]
pub fn point_peak_for_lumens(profile: &CookedIesProfile, lumens: f32) -> f32 {
    // The cook stored ∫ I dΩ at the file's peak: the solid-angle integral of the relative profile is its ratio.
    let sr = (profile.flux_lm / profile.peak_candela.max(1e-12)).max(1e-6);
    lumens / sr
}

/// A rect light's emitter: centre at the transform's translation, `width` along its `X`, `height` along its
/// `Y`, emitting toward its `-Z` (both faces when two-sided). Radiance from the flux per face, `Φ / (π A)`.
#[must_use]
pub fn rect_emitter(rect: &crate::RectLight, model: &Mat4) -> EmissiveLight {
    let c = model.transform_point3(Vec3::ZERO);
    let ex = model.transform_vector3(Vec3::X * (0.5 * rect.width.max(0.0)));
    // ex × ey points along −Z: the emitting face (`EmitterShape::Quad`).
    let ey = model.transform_vector3(Vec3::NEG_Y * (0.5 * rect.height.max(0.0)));
    let area = 4.0 * ex.length() * ey.length();
    let nits = if area > 0.0 {
        rect.intensity.max(0.0) / (std::f32::consts::PI * area)
    } else {
        0.0
    };
    EmissiveLight {
        centre: c.to_array(),
        axis_x: ex.to_array(),
        axis_y: ey.to_array(),
        radiance: rect.color.map(|k| k.max(0.0) * nits),
        shape: EmitterShape::Quad {
            two_sided: rect.two_sided,
            taper: 0.0,
        },
        anchor: [0.0, 0.0],
    }
}

/// A tube light's emitter: a cylinder of `radius` around the transform's `X` axis, `length` long. Radiance from
/// the flux over the lateral area, `Φ / (π · 2πr · L)`.
#[must_use]
pub fn tube_emitter(tube: &crate::TubeLight, model: &Mat4) -> EmissiveLight {
    let c = model.transform_point3(Vec3::ZERO);
    let ex = model.transform_vector3(Vec3::X * (0.5 * tube.length.max(0.0)));
    let radius = tube.radius.max(1e-4);
    let area = std::f32::consts::TAU * radius * 2.0 * ex.length();
    let nits = if area > 0.0 {
        tube.intensity.max(0.0) / (std::f32::consts::PI * area)
    } else {
        0.0
    };
    EmissiveLight {
        centre: c.to_array(),
        axis_x: ex.to_array(),
        axis_y: [0.0; 3],
        radiance: tube.color.map(|k| k.max(0.0) * nits),
        shape: EmitterShape::Tube { radius },
        anchor: [0.0, 0.0],
    }
}

/// Closed-form irradiance (lux) at a point on the axis of a one-sided rectangle `a × b` of uniform radiance
/// `nits`, `d` in front of its centre, facing it: `E = L · 4 · ff` with the corner form factor of a `a/2 × b/2`
/// rectangle (Howell's catalogue, configuration C-11). The reference the LTC integration is tested against.
#[must_use]
pub fn rect_on_axis_irradiance(nits: f32, a: f32, b: f32, d: f32) -> f32 {
    let x = f64::from(a) * 0.5 / f64::from(d);
    let y = f64::from(b) * 0.5 / f64::from(d);
    let sx = (1.0 + x * x).sqrt();
    let sy = (1.0 + y * y).sqrt();
    // Corner form factor (receiver element at a corner's normal): 1/(2π)·[x/√(1+x²)·atan(y/√(1+x²)) + …].
    let ff = (x / sx * (y / sx).atan() + y / sy * (x / sy).atan()) / (2.0 * std::f64::consts::PI);
    (f64::from(nits) * std::f64::consts::PI * 4.0 * ff) as f32
}

/// The representative-point stand-ins of an authored rect or tube light for a frame with no emitter budget
/// (`RenderProfile::area_light_fallback`, ADR-3247): the clustered point and spot lights every tier shades, the
/// lite (Mobile) shader included, so a rect or tube emits on `DesktopLow`, `MobileLow` and `MobileHigh`.
///
/// - A rect becomes a spot at its centre along its emitting normal, of the rect's on-axis intensity `L · A` and a
///   cosine falloff (inner half-angle 0, outer 90°: the spot ramp is then exactly `cos θ`, Lambert's law for a flat
///   emitter); a two-sided rect adds the mirrored spot. Its source size is the equal-area disc, `√(A/π)`, so its
///   specular takes the sized lights' representative point and energy term (ADR-1367).
/// - A tube becomes three cosine spots at its centre pointing out from its axis, 120° apart (the first toward the
///   ground, world `−Y`, where the axis allows), each of intensity `Φ / 3π` with `Φ = L · 2πrL · π` the tube's lateral
///   flux. A Lambertian cylinder's intensity is `(Φ / π²) · sin θ` from its axis; the three clamped cosines sum to
///   `sin θ` times 0.866 to 1 round the axis (mean `3/π`), so the stand-ins keep the flux, give nothing along the
///   axis and stay within −9 % / +5 % of the tube across it. Each carries the source size of the disc of the tube's
///   projected area, `√(2rL/π)`. (One isotropic point measured 33 % off a traced tube on a floor.)
///
/// Like every point and spot light these keep the 1 m falloff knee (`distance_attenuation_source`): within about
/// a metre of the emitter they read darker than the traced area light. Shadows follow `cast_shadows`.
#[must_use]
pub fn area_light_fallback(input: &AreaLightInput) -> Vec<crate::LabLocalLight> {
    let light = &input.light;
    let centre = Vec3::from_array(light.centre);
    let ex = Vec3::from_array(light.axis_x);
    let ey = Vec3::from_array(light.axis_y);
    let peak = light.radiance.iter().fold(0.0f32, |m, &c| m.max(c));
    if !(peak.is_finite() && peak > 0.0) {
        return Vec::new();
    }
    let color = light.radiance.map(|c| (c / peak).max(0.0));
    let range = if input.range > 0.0 { input.range } else { 40.0 };
    match light.shape {
        EmitterShape::Quad { two_sided, .. } => {
            let area = 4.0 * ex.length() * ey.length();
            let normal = ex.cross(ey).normalize_or_zero();
            if area <= 0.0 || normal == Vec3::ZERO {
                return Vec::new();
            }
            let spot = |dir: Vec3| {
                crate::LabLocalLight::Spot(crate::LabSpotLight {
                    position: light.centre,
                    direction: dir.to_array(),
                    color,
                    intensity: peak * area,
                    range,
                    inner_angle: 0.0,
                    outer_angle: std::f32::consts::FRAC_PI_2,
                    cast_shadows: input.cast_shadows,
                    source_radius: Some((area / std::f32::consts::PI).sqrt()),
                })
            };
            let mut out = vec![spot(normal)];
            if two_sided {
                out.push(spot(-normal));
            }
            out
        }
        EmitterShape::Tube { radius } => {
            let length = 2.0 * ex.length();
            if length <= 0.0 || radius <= 0.0 {
                return Vec::new();
            }
            let flux = peak * std::f32::consts::PI * std::f32::consts::TAU * radius * length;
            let axis = ex.normalize_or_zero();
            // The first lobe toward the ground where the axis is not vertical.
            let down = Vec3::NEG_Y - axis * axis.dot(Vec3::NEG_Y);
            let u = if down.length_squared() > 1e-6 {
                down.normalize()
            } else {
                axis.any_orthonormal_vector()
            };
            let w = axis.cross(u);
            let source = Some((2.0 * radius * length / std::f32::consts::PI).sqrt());
            (0..3)
                .map(|k| {
                    let (sin, cos) = (k as f32 * std::f32::consts::TAU / 3.0).sin_cos();
                    crate::LabLocalLight::Spot(crate::LabSpotLight {
                        position: centre.to_array(),
                        direction: (u * cos + w * sin).to_array(),
                        color,
                        intensity: flux / (3.0 * std::f32::consts::PI),
                        range,
                        inner_angle: 0.0,
                        outer_angle: std::f32::consts::FRAC_PI_2,
                        cast_shadows: input.cast_shadows,
                        source_radius: source,
                    })
                })
                .collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-3247: a rect stands in as a cosine spot of its on-axis intensity (two for a two-sided one), a tube as three
    /// radial cosine spots of its flux over 3π; all carry the equal-area source size.
    #[test]
    fn area_lights_fall_back_to_sized_spots_and_points() {
        let rect = crate::RectLight {
            width: 1.2,
            height: 0.6,
            intensity: 6000.0,
            range: 30.0,
            ..crate::RectLight::default()
        };
        let input = |light, two| AreaLightInput {
            light,
            range: 30.0,
            cast_shadows: two,
        };
        let down = Mat4::from_rotation_translation(
            Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2),
            Vec3::new(0.0, 2.0, 0.0),
        );
        let lights = area_light_fallback(&input(rect_emitter(&rect, &down), false));
        assert_eq!(lights.len(), 1);
        let crate::LabLocalLight::Spot(s) = &lights[0] else {
            panic!("a rect stands in as a spot")
        };
        // L · A = Φ / π cd along the normal (a spot light's intensity is candela).
        assert!(
            (s.intensity - 6000.0 / std::f32::consts::PI).abs() < 0.05,
            "{}",
            s.intensity
        );
        let normal = Vec3::from_array(rect_emitter(&rect, &down).axis_x)
            .cross(Vec3::from_array(rect_emitter(&rect, &down).axis_y))
            .normalize();
        assert!((Vec3::from_array(s.direction) - normal).length() < 1e-5);
        assert_eq!((s.inner_angle, s.outer_angle), (0.0, std::f32::consts::FRAC_PI_2));
        let r = s.source_radius.expect("sized");
        assert!((std::f32::consts::PI * r * r - 0.72).abs() < 1e-4);
        assert_eq!(s.color, [1.0; 3]);
        let two = crate::RectLight {
            two_sided: true,
            ..rect.clone()
        };
        let both = area_light_fallback(&input(rect_emitter(&two, &down), true));
        assert_eq!(both.len(), 2);
        let crate::LabLocalLight::Spot(back) = &both[1] else {
            panic!("spot")
        };
        assert!((Vec3::from_array(back.direction) + normal).length() < 1e-5);
        assert!(back.cast_shadows);
        let tube = crate::TubeLight {
            length: 1.0,
            radius: 0.05,
            intensity: 2000.0,
            ..crate::TubeLight::default()
        };
        let lights = area_light_fallback(&input(tube_emitter(&tube, &Mat4::IDENTITY), false));
        let spots: Vec<_> = lights
            .iter()
            .map(|l| match l {
                crate::LabLocalLight::Spot(s) => s,
                crate::LabLocalLight::Point(_) => panic!("a tube stands in as spots"),
            })
            .collect();
        assert_eq!(spots.len(), 3);
        // Each Φ / 3π cd: the three cosine lobes (π · I each) carry the tube's 2000 lm.
        for s in &spots {
            assert!(
                (s.intensity * 3.0 * std::f32::consts::PI - 2000.0).abs() < 0.5,
                "{}",
                s.intensity
            );
            assert!(
                Vec3::from_array(s.direction).dot(Vec3::X).abs() < 1e-5,
                "perpendicular to the axis"
            );
            let r = s.source_radius.expect("sized");
            assert!((std::f32::consts::PI * r * r - 0.1).abs() < 1e-4);
        }
        assert!(
            (Vec3::from_array(spots[0].direction) - Vec3::NEG_Y).length() < 1e-5,
            "the first lobe points down"
        );
        // Round the axis the clamped cosines sum to 0.866..1 of sin θ (mean 3/π), so the pattern is within −9 % / +5 %
        // of the cylinder's, and zero along the axis.
        for deg in 0..360 {
            let phi = (deg as f32).to_radians();
            let d = Vec3::new(0.0, -phi.cos(), phi.sin());
            let sum: f32 = spots
                .iter()
                .map(|s| Vec3::from_array(s.direction).dot(d).max(0.0))
                .sum();
            let ratio = sum / (3.0 / std::f32::consts::PI);
            assert!((0.906..=1.048).contains(&ratio), "{deg}°: {ratio}");
        }
        let along: f32 = spots
            .iter()
            .map(|s| Vec3::from_array(s.direction).dot(Vec3::X).max(0.0))
            .sum();
        assert!(along < 1e-5);
        // A dark or degenerate emitter stands in as nothing.
        let dark = crate::RectLight {
            intensity: 0.0,
            ..rect
        };
        assert!(area_light_fallback(&input(rect_emitter(&dark, &down), false)).is_empty());
    }

    fn example() -> Arc<CookedIesProfile> {
        let file = regia_asset::ies::IesFile::parse(&regia_asset::ies::example_text()).expect("parse");
        Arc::new(CookedIesProfile::from_file(&file).expect("cook"))
    }

    /// The kind fraction never moves the integer kind any reader takes, and decodes back.
    #[test]
    fn the_kind_fraction_keeps_the_kind_and_decodes() {
        for kind in [1.0f32, 2.0] {
            for slot in 1..=7u8 {
                for turn in [0u8, 1, 128, 255] {
                    let w = kind + kind_fraction(slot, turn);
                    assert_eq!((w + 0.5) as u32, kind as u32, "u32(w + 0.5)");
                    assert!(w < 3.5, "not an emitter");
                    let code = profile_code(w);
                    assert_eq!(code & 7, u32::from(slot));
                    assert_eq!(code >> 3, u32::from(turn));
                }
            }
        }
        assert_eq!(profile_code(2.0), 0);
    }

    /// A light without a profile keeps its record bit for bit; one with a profile gains only the tag, its nadir
    /// (points) and its peak.
    #[test]
    fn records_without_a_profile_stay_bit_identical() {
        let mut extras = LightExtras::default();
        let p = example();
        let uses = profile_use(Vec3::NEG_Y, Vec3::X, 0.0, Some(500.0));
        assert!(extras.assign(1, &p, uses));
        let point = [
            [1.0, 2.0, 3.0, 1.0],
            [1.0, 1.0, 1.0, 63.66],
            [0.0, -1.0, 0.0, 10.0],
            [1.0, 0.0, -1.0, 0.0],
        ];
        let mut a = point;
        extras.adjust_record(0, &mut a);
        extras.adjust_record(7, &mut a);
        assert_eq!(
            bytemuck::cast::<_, [u32; 16]>(a),
            bytemuck::cast::<_, [u32; 16]>(point)
        );
        let mut b = point;
        extras.adjust_record(1, &mut b);
        assert_eq!(profile_code(b[0][3]) & 7, 1);
        assert_eq!(b[1][3], 500.0);
        // An emitter record (kind ≥ 4) is never tagged.
        let mut e = point;
        e[0][3] = 4.5;
        extras.adjust_record(1, &mut e);
        assert_eq!(e[0][3], 4.5);
        assert!(LightExtras::default().is_empty());
    }

    #[test]
    fn an_ltc_sphere_negates_only_the_source_radius() {
        let record = |kind: f32, radius: f32| {
            [
                [1.0, 2.0, 3.0, kind],
                [0.5, 0.5, 0.5, 80.0],
                [0.0, -1.0, 0.0, 10.0],
                [0.9, 0.8, -1.0, radius],
            ]
        };
        let bits = |r: &[[f32; 4]; 4]| r.iter().flatten().map(|v| v.to_bits()).collect::<Vec<_>>();
        let mut extras = LightExtras::default();
        assert!(extras.is_empty());
        extras.mark_ltc_sphere(1);
        assert!(!extras.is_empty());
        extras.note_selection(&[1, 0, 2]);

        // Uploaded first: light 1, marked, sized point and sized spot.
        for kind in [1.0, 2.0] {
            let mut r = record(kind, 0.2);
            let mut want = r;
            want[3][3] = -0.2;
            extras.adjust_uploaded(0, &mut r);
            assert_eq!(bits(&r), bits(&want));
        }
        // Marked without a radius stays punctual: 0 keeps its bits.
        let mut r = record(1.0, 0.0);
        let want = r;
        extras.adjust_uploaded(0, &mut r);
        assert_eq!(bits(&r), bits(&want));
        // Unmarked lights, and an emitter record even when marked, are untouched.
        let mut r = record(1.0, 0.2);
        let want = r;
        extras.adjust_uploaded(1, &mut r);
        extras.adjust_uploaded(2, &mut r);
        assert_eq!(bits(&r), bits(&want));
        let mut r = record(4.5, 0.2);
        let want = r;
        extras.adjust_uploaded(0, &mut r);
        assert_eq!(bits(&r), bits(&want));
    }

    #[test]
    fn profiles_intern_and_the_slots_fill() {
        let mut extras = LightExtras::default();
        let p = example();
        let copy = Arc::new((*p).clone());
        assert_eq!(extras.intern(&p), Some(1));
        assert_eq!(extras.intern(&copy), Some(1), "equal content shares a slot");
        for k in 0..6 {
            let mut other = (*p).clone();
            other.values[0] = 0.01 * (k + 1) as f32;
            assert_eq!(extras.intern(&Arc::new(other)), Some(k as u8 + 2));
        }
        let mut full = (*p).clone();
        full.values[1] = 0.5;
        assert_eq!(extras.intern(&Arc::new(full)), None);
        assert_eq!(extras.emitter_tile_cap(), crate::MAX_UPLOADED_LIGHTS - 7 * 32);
    }

    /// The band the atlas holds, read the way the shader reads it, is the profile's own lookup; the nadir and
    /// the turn orient it.
    #[test]
    fn the_atlas_band_reads_back_the_profile() {
        let p = example();
        let mut extras = LightExtras::default();
        let nadir = Vec3::new(0.3, -1.0, 0.2).normalize();
        let uses = profile_use(nadir, Vec3::X, 0.4, None);
        assert!(extras.assign(0, &p, uses));
        let mut atlas = vec![0.0f32; crate::emissive_lights::probe_atlas_len(3)];
        assert_eq!(extras.write_profile_bands(&mut atlas), 1);
        let width = (PROBE_ATLAS_COLUMNS * PROBE_RES) as usize;
        assert_eq!(atlas.len(), atlas_rows() as usize * width);
        let row0 = band_first_row(1) as usize;
        assert_eq!(&atlas[row0 * width..row0 * width + 4], &p.values[..4]);
        let code = u32::from(uses.slot.max(1)) | (u32::from(uses.turn) << 3);
        // Straight down the nadir is the profile's θ = 0.
        let down = relative_gpu(&p, code, nadir, nadir);
        assert!((down - p.relative_at_angles(0.0, 0.0)).abs() < 1e-4);
        // The turned 0° axis (θ = 30°) reads the profile's φ = 0 column.
        let a0 = profile_axis(nadir);
        let a1 = a0.cross(nadir);
        let turn = f32::from(uses.turn) * std::f32::consts::TAU / 256.0;
        let t0 = a0 * turn.cos() + a1 * turn.sin();
        let d = (nadir * 30f32.to_radians().cos() + t0 * 30f32.to_radians().sin()).normalize();
        let got = relative_gpu(&p, code, nadir, d);
        assert!((got - p.relative_at_angles(30.0, 0.0)).abs() < 2e-3, "{got}");
        // A full atlas of emitter tiles leaves no room: nothing is written.
        let mut busy = vec![0.0f32; atlas_rows() as usize * width];
        assert_eq!(extras.write_profile_bands(&mut busy), 0);
    }

    /// Flux is conserved for a point light whose lumens a profile redistributes.
    #[test]
    fn a_profiled_point_light_keeps_its_lumens() {
        let p = example();
        let peak = point_peak_for_lumens(&p, 1000.0);
        assert!((peak * p.relative_flux() - 1000.0).abs() < 0.5);
    }

    /// The LTC polygon integral the forward pass shades rect lights with matches the closed form on the axis.
    #[test]
    fn rect_light_irradiance_matches_the_closed_form() {
        let rect = crate::RectLight {
            width: 2.0,
            height: 1.0,
            intensity: 3000.0,
            ..crate::RectLight::default()
        };
        // At rest the light emits toward −Z: a receiver below it facing +Z.
        let model = Mat4::IDENTITY;
        let light = rect_emitter(&rect, &model);
        let nits = light.radiance[1];
        for d in [0.25f32, 1.0, 3.0, 10.0] {
            let p = Vec3::new(0.0, 0.0, -d);
            let ltc = crate::emissive_lights::irradiance_at(&light, p, Vec3::Z);
            let exact = rect_on_axis_irradiance(nits, 2.0, 1.0, d);
            assert!((ltc - exact).abs() / exact < 2e-3, "d {d}: {ltc} vs {exact}");
        }
        // The flux the emitter reports is the authored lumens (white light).
        assert!((light.flux() - 3000.0).abs() < 1.0);
        // Nothing reaches the back of a one-sided rect.
        let behind = crate::emissive_lights::irradiance_at(&light, Vec3::new(0.0, 0.0, 1.0), Vec3::NEG_Z);
        assert_eq!(behind, 0.0);
    }

    #[test]
    fn tube_light_flux_is_its_lumens() {
        let tube = crate::TubeLight {
            length: 1.2,
            radius: 0.02,
            intensity: 2000.0,
            ..crate::TubeLight::default()
        };
        let light = tube_emitter(&tube, &Mat4::IDENTITY);
        assert!((light.flux() - 2000.0).abs() < 1.0);
        // Far away a tube is a line source: E ≈ I⊥ / d² with I⊥ = L · 2r · length.
        let d = 40.0;
        let e = crate::emissive_lights::irradiance_at(&light, Vec3::new(0.0, -d, 0.0), Vec3::Y);
        let expect = light.radiance[1] * 2.0 * 0.02 * 1.2 / (d * d);
        assert!((e - expect).abs() / expect < 0.02, "{e} vs {expect}");
    }
}
