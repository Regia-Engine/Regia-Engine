// SPDX-License-Identifier: MPL-2.0
// Source: clean-room behavior spec `lens_flare.md` (derived from Lumit, GPL-3.0, commit
// fa959219e8920f4af445c2a3fd584a2c3061b190) for the schema, sign conventions, validity rules,
// ghost enumeration/ranking and per-crossing optics rules; Hullin, Eisemann, Seidel and Lee,
// "Physically-Based Real-Time Lens Flare Rendering", ACM Transactions on Graphics 30(4), 2011,
// for the two-bounce ghost model and the per-ghost size probe; Hecht, "Optics" (and Born & Wolf,
// "Principles of Optics") for Snell's law, the Fresnel equations and the paraxial thick-lens
// trace; A.-L. Cauchy's dispersion formula with the Fraunhofer F, d and C spectral lines and the
// Abbe number definition. No lens data from any source is reproduced: every prescription in the
// tests below is synthetic (invented for this file, not transcribed from any real lens).

//! CPU reference for the physically based ghost lens flare.
//!
//! This module computes, on the CPU and with no GPU or window, what the lens-flare effect needs
//! before it renders: it validates a lens description ([`LensPrescription`]), traces rays
//! sequentially through spherical and flat surfaces with vector Snell refraction and
//! unpolarized Fresnel reflectance ([`trace_sequential`], [`trace_ghost`]), enumerates the
//! two-bounce ghost paths of every ordered pair of reflecting surfaces
//! ([`enumerate_ghost_pairs`]), ranks them by their traced on-axis intensity
//! ([`rank_ghosts`]), and measures each ghost's sensor footprint with a ray fan
//! ([`ghost_footprint_fraction`], [`ghost_size_fraction`]). The GPU pass reproduces these
//! numbers; until it exists this reference is what the tests verify.
//!
//! # Frame, units and sign conventions
//!
//! Light travels along `+z`, from the object side (front, `z < 0`) to the image side (rear,
//! `z > 0`). All lengths are millimetres, all angles implicit in directions. Surface vertices
//! sit at `z = sum(distances before them)`; the image plane at infinity focus sits at
//! [`image_plane_z`], the sum of every axial distance. A surface's signed radius is positive
//! when its center of curvature lies on the image side of the vertex, so a biconvex singlet has
//! a positive front radius and a negative rear radius; a radius of exactly zero means a flat
//! (the spec also allows an "infinite" keyword in text data; both read the same here).
//!
//! # What this reference deliberately leaves out
//!
//! The spec's thin-film coating stacks, starburst, iris mask, spectral color matching and
//! reconstruction are GPU-pass concerns. Here a coating is one scale factor per surface
//! ([`Surface::coating_factor`]) on the bare-glass Fresnel reflectance, the two extremes of the
//! spec's Coating dial; the trace is monochromatic per call ([`Ray::wavelength_nm`]) with glass
//! dispersion from a two-term Cauchy fit. The stop's clear aperture is read as stored: scaling
//! it by a working f-stop is a render-side quantity this reference does not carry.
//!
//! # The clear-aperture flag the spec carries
//!
//! The spec (§2.2 and §13.3) flags that its source is internally inconsistent about whether the
//! per-surface clear aperture column is a semi-diameter (radius) or a full diameter, and asks
//! Regia to store the **semi-diameter** and name it so. This module does exactly that:
//! [`Surface::clear_semi_diameter_mm`] is a radius. A value of zero or less marks the surface
//! skipped (no housing limit, not counted as usable); a spherical surface's effective aperture
//! is additionally limited to its own `|radius|` (§6).

use regia_core::glam::Vec3;

/// Wavelength of the Fraunhofer F line (blue, hydrogen), nanometres.
pub const F_LINE_NM: f32 = 486.1327;
/// Wavelength of the Fraunhofer d line (yellow, helium), nanometres. The design wavelength:
/// refractive indices in a prescription are `n_d` and the reflecting-interface test runs here.
pub const D_LINE_NM: f32 = 587.5618;
/// Wavelength of the Fraunhofer C line (red, hydrogen), nanometres.
pub const C_LINE_NM: f32 = 656.2725;
/// The red, green and blue probe wavelengths ghost ranking traces on axis (spec §5.3).
pub const PROBE_WAVELENGTHS_NM: [f32; 3] = [C_LINE_NM, D_LINE_NM, F_LINE_NM];
/// An interface reflects only when the refractive index actually changes across it by more than
/// this (spec §2.3, one thousandth at the d line). The air-to-air aperture stop drops out.
pub const REFLECTING_INDEX_CHANGE: f32 = 1.0e-3;
/// Ranked ghosts below this mean landed weight are dropped (spec §5.3).
pub const GHOST_INTENSITY_CUTOFF: f32 = 1.0e-7;
/// A lens needs at least this many usable surfaces (clear semi-diameter > 0); see
/// [`PrescriptionError::TooFewUsableSurfaces`] (spec §2.3).
pub const MIN_USABLE_SURFACES: usize = 3;
/// The pupil sample disc carries this much margin over the entrance pupil, clamped to the front
/// element's clear semi-diameter (spec §7).
pub const PUPIL_MARGIN: f32 = 1.5;
/// Fan rays start this far in front of the first surface vertex (spec §7: a fixed distance).
pub const RAY_LEAD_MM: f32 = 1.0;
/// A signed radius smaller than this reads as flat (exact zero is the spec's flat encoding).
pub const FLAT_RADIUS_EPSILON_MM: f32 = 1.0e-9;
/// Below this ratio of radial height to clear aperture the housing weight is full; it fades by
/// smoothstep to zero at ratio 1.0 (spec §6).
pub const VIGNETTE_FULL_RATIO: f32 = 0.95;
/// Frame aspect of the per-ghost size probe (§5.5 probes a still frame; 16:9 is the reference
/// composition of §3.1's example light).
pub const SIZE_PROBE_FRAME_ASPECT: f32 = 16.0 / 9.0;
/// Where the size probe places the light: a third of the way into the frame from the top-left,
/// matching the spec's "a third into the frame" reference position (640, 360 at 1080p), in
/// frame coordinates where `[0, 0]` is the frame center and `[±1, ±1]` the frame corners.
pub const SIZE_PROBE_UV: [f32; 2] = [-1.0 / 3.0, -1.0 / 3.0];

/// One optical surface of a lens prescription, front (object side) to rear (image side).
///
/// All quantities are per the spec's schema (§2.2). `refractive_index_d` and `abbe_v_d` describe
/// the medium **after** this surface (the medium before the first surface is air, `n = 1`);
/// the last surface's medium is image space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Surface {
    /// Signed radius of curvature, millimetres. Positive when the center of curvature lies on
    /// the image side of the vertex. Exactly zero (or within [`FLAT_RADIUS_EPSILON_MM`] of it)
    /// means a flat surface.
    pub signed_radius_mm: f32,
    /// Axial distance to the next surface vertex; the last surface's value is the distance to
    /// the image plane at infinity focus, so the sum of all distances fixes the sensor position.
    pub axial_distance_to_next_mm: f32,
    /// Refractive index of the medium after this surface at the d line (`n_d`); 1 for air.
    pub refractive_index_d: f32,
    /// Abbe number (`V_d`) of the medium after this surface; 0 means no dispersion (air).
    pub abbe_v_d: f32,
    /// **Clear semi-diameter (radius), millimetres.** The spec flags that its source is
    /// inconsistent about radius versus diameter and asks Regia to store the semi-diameter and
    /// name it so; this field is that. Zero or negative marks the surface skipped: it still
    /// refracts (the index change is physical) but contributes no housing limit and does not
    /// count toward [`MIN_USABLE_SURFACES`].
    pub clear_semi_diameter_mm: f32,
    /// Coating scale on this surface's bare-glass Fresnel reflectance: 1 is uncoated glass, 0 a
    /// perfect anti-reflection, values between a proportional reduction. The full thin-film
    /// characteristic-matrix stacks of the spec (§6) are GPU-pass work; this factor is the
    /// reference stand-in and spans the two extremes of the spec's Coating dial.
    pub coating_factor: f32,
    /// True when this surface is the aperture stop. At most one surface may carry the flag; the
    /// stop is an air-to-air surface, so it never reflects (§2.2, §2.3).
    pub is_stop: bool,
}

/// A lens description: an ordered list of [`Surface`]s plus the lens-level quantities of the
/// spec's schema (§2.1). Regia transcribes real values from public patents; nothing here is
/// built in.
#[derive(Debug, Clone, PartialEq)]
pub struct LensPrescription {
    /// Display name, shown in a lens picker.
    pub name: String,
    /// Effective focal length, millimetres. Must be finite and > 0 (§2.1); a description
    /// without it is invalid.
    pub focal_length_mm: f32,
    /// Native (wide-open) f-number; 0 means "not stored", in which case
    /// [`entrance_pupil_radius`] estimates it from the front element's clear diameter (§2.1).
    pub native_f_number: f32,
    /// Image format: sensor width, millimetres. Regia stores the format per lens (§2.1, §13.6);
    /// this reference requires it (finite, > 0) because footprint fractions divide by it.
    pub sensor_width_mm: f32,
    /// Image format: sensor height, millimetres. See [`Self::sensor_width_mm`].
    pub sensor_height_mm: f32,
    /// Where the data came from (patent number and example, or "synthetic").
    pub provenance: String,
    /// The surfaces, front to rear. At least [`MIN_USABLE_SURFACES`] of them must be usable
    /// (clear semi-diameter > 0) for [`Self::validate`] to pass.
    pub surfaces: Vec<Surface>,
}

/// How a prescription fails the spec's validity rules (§2.3). Typed so callers can fall back to
/// another lens without string matching; the effect never fails a render on the back of it.
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
pub enum PrescriptionError {
    /// The effective focal length is missing, non-finite or not positive.
    #[error("effective focal length must be finite and > 0, got {0} mm")]
    FocalLength(f32),
    /// The image format is missing or not positive; the footprint fractions divide by it.
    #[error("sensor format must be finite and > 0, got {width} x {height} mm")]
    SensorFormat {
        /// The stored width.
        width: f32,
        /// The stored height.
        height: f32,
    },
    /// The native f-number is non-finite or negative (0 legitimately means "not stored").
    #[error("native f-number must be finite and >= 0 (0 = not stored), got {0}")]
    NativeFNumber(f32),
    /// Fewer than [`MIN_USABLE_SURFACES`] surfaces have a positive clear semi-diameter.
    #[error("a lens needs at least 3 usable surfaces (clear semi-diameter > 0), got {usable}")]
    TooFewUsableSurfaces {
        /// How many usable surfaces the description actually has.
        usable: usize,
    },
    /// More than one surface is flagged as the aperture stop.
    #[error("at most one surface may be the aperture stop, got {count}")]
    MultipleStops {
        /// How many surfaces carry the stop flag.
        count: usize,
    },
    /// A surface field is not a finite number (§13.9 makes non-finite rays die quietly; a
    /// non-finite *prescription* is a data error and reported).
    #[error("surface {index}: {field} is not finite")]
    NonFiniteField {
        /// Index of the offending surface.
        index: usize,
        /// Name of the offending field.
        field: &'static str,
    },
    /// A refractive index below 1 (air is 1; no physical medium bends less).
    #[error("surface {index}: refractive index {n} is below 1")]
    RefractiveIndexBelowOne {
        /// Index of the offending surface.
        index: usize,
        /// The stored index.
        n: f32,
    },
    /// A negative Abbe number (0 already means "no dispersion").
    #[error("surface {index}: Abbe number {v} is negative")]
    NegativeAbbe {
        /// Index of the offending surface.
        index: usize,
        /// The stored Abbe number.
        v: f32,
    },
    /// A negative axial distance to the next surface.
    #[error("surface {index}: axial distance {distance} mm is negative")]
    NegativeDistance {
        /// Index of the offending surface.
        index: usize,
        /// The stored distance.
        distance: f32,
    },
    /// A coating factor outside `0..=1`.
    #[error("surface {index}: coating factor {factor} is outside 0..=1")]
    CoatingFactor {
        /// Index of the offending surface.
        index: usize,
        /// The stored factor.
        factor: f32,
    },
}

impl LensPrescription {
    /// Checks the spec's validity rules (§2.3): a positive finite focal length, a positive
    /// finite image format, at most one stop, at least [`MIN_USABLE_SURFACES`] usable surfaces,
    /// and finite in-range values on every surface. Rows are not dropped here: a surface with a
    /// zero or negative clear semi-diameter is *skipped* (excluded from the usable count and
    /// from housing limits), not an error.
    ///
    /// The lower-level traces ([`trace_sequential`], [`trace_ghost`]) do **not** call this, so
    /// the spec's trivial-singlet fallback (§13.8) stays traceable; the ghost enumeration and
    /// ranking do.
    pub fn validate(&self) -> Result<(), PrescriptionError> {
        if !self.focal_length_mm.is_finite() || self.focal_length_mm <= 0.0 {
            return Err(PrescriptionError::FocalLength(self.focal_length_mm));
        }
        if !self.sensor_width_mm.is_finite()
            || !self.sensor_height_mm.is_finite()
            || self.sensor_width_mm <= 0.0
            || self.sensor_height_mm <= 0.0
        {
            return Err(PrescriptionError::SensorFormat {
                width: self.sensor_width_mm,
                height: self.sensor_height_mm,
            });
        }
        if !self.native_f_number.is_finite() || self.native_f_number < 0.0 {
            return Err(PrescriptionError::NativeFNumber(self.native_f_number));
        }
        let usable = self
            .surfaces
            .iter()
            .filter(|s| s.clear_semi_diameter_mm > 0.0)
            .count();
        if usable < MIN_USABLE_SURFACES {
            return Err(PrescriptionError::TooFewUsableSurfaces { usable });
        }
        let stops = self.surfaces.iter().filter(|s| s.is_stop).count();
        if stops > 1 {
            return Err(PrescriptionError::MultipleStops { count: stops });
        }
        for (index, s) in self.surfaces.iter().enumerate() {
            for (field, value) in [
                ("signed_radius_mm", s.signed_radius_mm),
                ("axial_distance_to_next_mm", s.axial_distance_to_next_mm),
                ("refractive_index_d", s.refractive_index_d),
                ("abbe_v_d", s.abbe_v_d),
                ("clear_semi_diameter_mm", s.clear_semi_diameter_mm),
                ("coating_factor", s.coating_factor),
            ] {
                if !value.is_finite() {
                    return Err(PrescriptionError::NonFiniteField { index, field });
                }
            }
            if s.refractive_index_d < 1.0 {
                return Err(PrescriptionError::RefractiveIndexBelowOne {
                    index,
                    n: s.refractive_index_d,
                });
            }
            if s.abbe_v_d < 0.0 {
                return Err(PrescriptionError::NegativeAbbe { index, v: s.abbe_v_d });
            }
            if s.axial_distance_to_next_mm < 0.0 {
                return Err(PrescriptionError::NegativeDistance {
                    index,
                    distance: s.axial_distance_to_next_mm,
                });
            }
            if !(0.0..=1.0).contains(&s.coating_factor) {
                return Err(PrescriptionError::CoatingFactor {
                    index,
                    factor: s.coating_factor,
                });
            }
        }
        Ok(())
    }
}

/// How a trace or footprint query was handed unusable indices.
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
pub enum TraceError {
    /// The prescription has no surfaces to trace.
    #[error("the prescription has no surfaces to trace")]
    EmptyPrescription,
    /// A bounce surface index is past the end of the prescription.
    #[error("bounce surface {index} is past the end of the prescription")]
    SurfaceIndex {
        /// The offending index.
        index: usize,
    },
    /// The two bounces are out of order: the first bounce must be the front (lower-index)
    /// surface of the pair (spec §5.1, "a before b").
    #[error("ghost bounces out of order: first {first} must be before second {second}")]
    GhostOrder {
        /// The offending first (front) bounce index.
        first: usize,
        /// The offending second (rear) bounce index.
        second: usize,
    },
}

/// Runtime dials of the reference trace. Everything the render animates per frame stays out of
/// the prescription; only the Coating dial belongs to the trace itself.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TraceSettings {
    /// The spec's Coating dial, `0..=1`: 0 leaves every surface at bare-glass Fresnel
    /// reflectance, 1 applies each surface's [`Surface::coating_factor`] in full. Blended
    /// linearly between. Non-finite reads as 1 ("as the lens description"), the spec's default.
    pub coating: f32,
}

impl Default for TraceSettings {
    fn default() -> Self {
        Self { coating: 1.0 }
    }
}

impl TraceSettings {
    /// The dial clamped to `0..=1` (non-finite reads as 1).
    fn dial(&self) -> f32 {
        if self.coating.is_finite() {
            self.coating.clamp(0.0, 1.0)
        } else {
            1.0
        }
    }
}

/// A ray handed to a trace.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ray {
    /// Origin, millimetres, in the trace frame (`+z` object to image).
    pub origin: Vec3,
    /// Travel direction; it is normalized by the trace. Zero or non-finite input kills the ray
    /// (weight 0, geometry along the axis) per §13.9: a bad ray never faults the trace.
    pub direction: Vec3,
    /// Starting energy, usually 1; the trace only ever multiplies it down.
    pub weight: f32,
    /// Wavelength in nanometres; non-finite or `<= 0` kills the ray. Glass media follow the
    /// two-term Cauchy fit at this wavelength ([`cauchy_refractive_index`]).
    pub wavelength_nm: f32,
}

impl Ray {
    /// A unit-weight ray of the given wavelength.
    pub fn new(origin: Vec3, direction: Vec3, wavelength_nm: f32) -> Self {
        Self {
            origin,
            direction,
            weight: 1.0,
            wavelength_nm,
        }
    }
}

impl Default for Ray {
    fn default() -> Self {
        Self::new(Vec3::ZERO, Vec3::Z, D_LINE_NM)
    }
}

/// One fully traced path: where it landed, with what direction, energy and vignette bookkeeping.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TracedRay {
    /// Where the ray meets the image plane (the plane through [`image_plane_z`]). Rays that
    /// never reach it (a backward-facing exit) keep their last position and a weight of 0.
    pub landing: Vec3,
    /// Direction of travel at the landing.
    pub direction: Vec3,
    /// Surviving energy fraction; `0..=1` for a unit-weight ray. A ray that missed glass, hit
    /// past its clear aperture, or suffered total internal reflection carries 0 here while its
    /// geometry above stays defined (spec §6, §13.9).
    pub weight: f32,
    /// The worst ratio of radial height to effective clear semi-diameter over all surfaces, the
    /// housing-vignette driver (full below [`VIGNETTE_FULL_RATIO`], zero at 1.0).
    pub worst_aperture_ratio: f32,
}

/// One ranked two-bounce ghost.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ghost {
    /// Index of the first (front) bounce surface.
    pub first_bounce: usize,
    /// Index of the second (rear) bounce surface.
    pub second_bounce: usize,
    /// Mean landed weight of the on-axis red/green/blue probe fan: the ranking key (§5.3).
    /// Sorted brightest first; ties break by the lower pair, then the lower second index.
    pub intensity: f32,
    /// The ghost's sensor footprint radius as a fraction of the sensor diagonal, the larger of
    /// the on-axis and a-third-into-the-frame probes (§5.5).
    pub size_fraction: f32,
    /// Landed-energy-weighted mean landing position of the probe fan, millimetres (on the image plane).
    pub centroid_mm: Vec3,
}

/// Unpolarized (s and p averaged) power reflectance of a dielectric interface at incidence
/// cosine `cos_incidence` (0 grazing, 1 normal), non-magnetic media, from the Fresnel
/// equations. Returns 1 beyond the critical angle (total internal reflection) and 0 when the
/// indices are equal. Symmetric in `n1`/`n2`.
#[must_use]
pub fn fresnel_reflectance(n1: f32, n2: f32, cos_incidence: f32) -> f32 {
    let cos_i = cos_incidence.clamp(0.0, 1.0);
    let sin_i = (1.0 - cos_i * cos_i).sqrt();
    let sin_t = n1 / n2 * sin_i;
    if sin_t >= 1.0 {
        return 1.0; // total internal reflection
    }
    let cos_t = (1.0 - sin_t * sin_t).sqrt();
    let rs = (n1 * cos_i - n2 * cos_t) / (n1 * cos_i + n2 * cos_t);
    let rp = (n1 * cos_t - n2 * cos_i) / (n1 * cos_t + n2 * cos_i);
    0.5 * (rs * rs + rp * rp)
}

/// Refractive index of a medium at `wavelength_nm` from a two-term Cauchy fit
/// `n = A + B / lambda^2` derived from the medium's `n_d` and Abbe number `V_d` through the
/// standard F, C and d spectral lines (§6). `abbe_v_d <= 0` (air, or "no data") disables
/// dispersion and returns `n_d` unchanged; a non-finite or non-positive wavelength also returns
/// `n_d` (callers kill such rays first).
#[must_use]
pub fn cauchy_refractive_index(n_d: f32, abbe_v_d: f32, wavelength_nm: f32) -> f32 {
    if !n_d.is_finite() || !abbe_v_d.is_finite() || abbe_v_d <= 0.0 {
        return if n_d.is_finite() { n_d } else { 1.0 };
    }
    if !wavelength_nm.is_finite() || wavelength_nm <= 0.0 {
        return n_d;
    }
    // n_F - n_C = (n_d - 1) / V_d fixes B; the fit passes through n_d at the d line.
    let inverse_f2 = 1.0 / (F_LINE_NM * F_LINE_NM);
    let inverse_c2 = 1.0 / (C_LINE_NM * C_LINE_NM);
    let b = ((n_d - 1.0) / abbe_v_d) / (inverse_f2 - inverse_c2);
    let a = n_d - b / (D_LINE_NM * D_LINE_NM);
    a + b / (wavelength_nm * wavelength_nm)
}

/// The image plane's `z` at infinity focus: the sum of every axial distance (§2.2), so the last
/// surface's distance is the back focus working distance. 0 for an empty surface list.
#[must_use]
pub fn image_plane_z(prescription: &LensPrescription) -> f32 {
    prescription
        .surfaces
        .iter()
        .map(|s| s.axial_distance_to_next_mm)
        .sum()
}

/// The sensor diagonal, millimetres, that footprint fractions divide by. 0 when the format is
/// unset (footprints then read 0 rather than dividing by it).
#[must_use]
pub fn sensor_diagonal(prescription: &LensPrescription) -> f32 {
    let d = (prescription.sensor_width_mm * prescription.sensor_width_mm
        + prescription.sensor_height_mm * prescription.sensor_height_mm)
        .sqrt();
    if d.is_finite() && d > 0.0 { d } else { 0.0 }
}

/// The pupil sample disc radius (§7): the entrance pupil `focal_length / (2 * f_number)` with
/// [`PUPIL_MARGIN`] over it, clamped to the front element's clear semi-diameter. When the
/// native f-number is not stored it is estimated as `focal_length / front clear diameter` (the
/// crude fallback of §2.1); when neither is available the radius is 0. Meaningless (0) on an
/// unvalidated prescription.
#[must_use]
pub fn entrance_pupil_radius(prescription: &LensPrescription) -> f32 {
    let front = prescription
        .surfaces
        .first()
        .map_or(0.0, |s| s.clear_semi_diameter_mm);
    let f_number = if prescription.native_f_number > 0.0 {
        prescription.native_f_number
    } else if front > 0.0 {
        prescription.focal_length_mm / (2.0 * front)
    } else {
        return 0.0;
    };
    let pupil = prescription.focal_length_mm / (2.0 * f_number) * PUPIL_MARGIN;
    if front > 0.0 {
        pupil.max(0.0).min(front)
    } else {
        pupil.max(0.0)
    }
}

/// The incoming (light-at-infinity) direction for a light at normalized frame position
/// `light_uv` (`[0, 0]` frame center, `[±1, ±1]` frame corners), per §4: horizontal position
/// maps across the sensor width, vertical across the sensor width times the frame's
/// height/width ratio (`frame_aspect` = frame width / frame height, so a 16:9 frame is a
/// horizontal crop of a 3:2 sensor). Non-finite input reads as on-axis.
#[must_use]
pub fn field_direction(prescription: &LensPrescription, frame_aspect: f32, light_uv: [f32; 2]) -> Vec3 {
    let aspect = if frame_aspect.is_finite() && frame_aspect > 0.0 {
        frame_aspect
    } else {
        1.0
    };
    let target = Vec3::new(
        light_uv[0] * 0.5 * prescription.sensor_width_mm,
        light_uv[1] * 0.5 * prescription.sensor_width_mm / aspect,
        prescription.focal_length_mm,
    );
    let dir = target.normalize_or_zero();
    if dir == Vec3::ZERO { Vec3::Z } else { dir }
}

/// What happens when the ray meets a surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Event {
    /// Transmit: vector Snell refraction, energy times `1 - R`.
    Refract,
    /// Specular bounce: energy times `R`.
    Reflect,
}

/// The ray's live state through one trace.
struct RayState {
    origin: Vec3,
    dir: Vec3,
    weight: f32,
    worst_aperture_ratio: f32,
    /// Set when the ray stopped being trustworthy (non-finite input or arithmetic): it then
    /// passes straight through every remaining vertex plane at zero weight (§13.9).
    dead: bool,
}

impl RayState {
    fn new(ray: Ray) -> Self {
        let dir = ray.direction.normalize_or_zero();
        let live = dir.is_finite()
            && dir != Vec3::ZERO
            && ray.origin.is_finite()
            && ray.weight.is_finite()
            && ray.wavelength_nm.is_finite()
            && ray.wavelength_nm > 0.0;
        if live {
            Self {
                origin: ray.origin,
                dir,
                weight: ray.weight.max(0.0),
                worst_aperture_ratio: 0.0,
                dead: false,
            }
        } else {
            // A non-finite ray is killed, never an error (§13.9); it keeps a defined landing.
            Self {
                origin: if ray.origin.is_finite() {
                    ray.origin
                } else {
                    Vec3::ZERO
                },
                dir: Vec3::Z,
                weight: 0.0,
                worst_aperture_ratio: 0.0,
                dead: true,
            }
        }
    }

    fn kill(&mut self) {
        self.weight = 0.0;
    }

    /// Moves straight to the plane `z` (a virtual vertex-plane crossing or the dead ray's
    /// shortcut). A degenerate direction leaves the origin where it is.
    fn advance_to_plane(&mut self, z: f32) {
        if self.dir.z.abs() > f32::EPSILON {
            let t = (z - self.origin.z) / self.dir.z;
            if t.is_finite() && t > 0.0 {
                self.origin += self.dir * t;
            }
        }
    }

    /// Crosses one surface: intersect (sphere or vertex plane), apply the housing vignette,
    /// then refract or reflect with the coated reflectance.
    fn cross_surface(
        &mut self,
        surface: &Surface,
        vertex_z: f32,
        n_before: f32,
        n_after: f32,
        event: Event,
        dial: f32,
    ) {
        if self.dead {
            self.advance_to_plane(vertex_z);
            return;
        }
        let Some((hit, normal)) = intersect_surface(self.origin, self.dir, surface, vertex_z) else {
            // Missed the glass: continue virtually through the vertex plane at zero weight so
            // the grid around the miss stays defined (§6).
            self.advance_to_plane(vertex_z);
            self.kill();
            return;
        };
        // Housing vignette: worst ratio over all surfaces, full below VIGNETTE_FULL_RATIO,
        // smoothstep to zero at 1.0 (§6). Skipped surfaces (aperture <= 0) do not limit.
        let aperture = effective_clear_semi_diameter(surface);
        if aperture > 0.0 {
            let ratio = ((hit.x * hit.x + hit.y * hit.y).sqrt()) / aperture;
            self.worst_aperture_ratio = self.worst_aperture_ratio.max(ratio);
            self.weight *= 1.0 - smoothstep(VIGNETTE_FULL_RATIO, 1.0, ratio);
        }
        let cos_i = -self.dir.dot(normal);
        // A non-finite reflectance (unvalidated media data) reads as a dead crossing (§13.9),
        // never as NaN energy.
        let r = {
            let r =
                fresnel_reflectance(n_before, n_after, cos_i) * (1.0 - dial * (1.0 - surface.coating_factor));
            if r.is_finite() { r } else { 0.0 }
        };
        match event {
            Event::Reflect => {
                self.dir = (self.dir - normal * (2.0 * self.dir.dot(normal))).normalize_or_zero();
                self.weight *= r;
            }
            Event::Refract => {
                if cos_i <= 0.0 {
                    // Tangential: no meaningful refraction; dead rays keep geometry.
                    self.kill();
                    self.origin = hit;
                    return;
                }
                let eta = n_before / n_after;
                let sin_t2 = eta * eta * (1.0 - cos_i * cos_i);
                if sin_t2 >= 1.0 {
                    // Total internal reflection on a transmitted leg: zero weight, geometry
                    // continues straight (§6).
                    self.kill();
                    self.origin = hit;
                    return;
                }
                let cos_t = (1.0 - sin_t2).sqrt();
                self.dir = (self.dir * eta + normal * (eta * cos_i - cos_t)).normalize_or_zero();
                self.weight *= 1.0 - r;
            }
        }
        self.origin = hit;
        if self.dir == Vec3::ZERO || !self.dir.is_finite() || !self.origin.is_finite() {
            self.dead = true;
            self.dir = Vec3::Z;
            self.kill();
        }
    }
}

/// Intersects a ray with one surface: the sphere of `signed_radius_mm` about the vertex, or the
/// vertex plane when flat. Returns the hit point and the unit normal oriented against the ray.
/// `None` is a miss (no real root ahead of the ray); the caller crosses virtually.
fn intersect_surface(origin: Vec3, dir: Vec3, surface: &Surface, vertex_z: f32) -> Option<(Vec3, Vec3)> {
    if surface.signed_radius_mm.abs() < FLAT_RADIUS_EPSILON_MM {
        // Flat: the vertex plane.
        if dir.z.abs() <= f32::EPSILON {
            return None; // parallel to the plane and not on it: a miss
        }
        let t = (vertex_z - origin.z) / dir.z;
        if t <= 0.0 {
            return None;
        }
        let normal = if dir.z > 0.0 { -Vec3::Z } else { Vec3::Z };
        Some((origin + dir * t, normal))
    } else {
        // Sphere: center one signed radius from the vertex along z (positive radius puts the
        // center on the image side, §2.2).
        let radius = surface.signed_radius_mm;
        let center = Vec3::new(0.0, 0.0, vertex_z + radius);
        let oc = origin - center;
        let b = oc.dot(dir);
        let c = oc.dot(oc) - radius * radius;
        let discriminant = b * b - c;
        if discriminant < 0.0 {
            return None;
        }
        let root = discriminant.sqrt();
        let t = if -b - root > 0.0 { -b - root } else { -b + root };
        if t <= 0.0 {
            return None;
        }
        let hit = origin + dir * t;
        let normal = (hit - center) / radius.abs();
        let normal = if normal.dot(dir) > 0.0 { -normal } else { normal };
        Some((hit, normal))
    }
}

/// The surface's effective clear semi-diameter: the stored value, limited to the sphere's own
/// radius where a transcription claims more (§6); 0 for skipped surfaces (no housing limit).
fn effective_clear_semi_diameter(surface: &Surface) -> f32 {
    if surface.clear_semi_diameter_mm <= 0.0 {
        return 0.0;
    }
    if surface.signed_radius_mm.abs() < FLAT_RADIUS_EPSILON_MM {
        surface.clear_semi_diameter_mm
    } else {
        surface.clear_semi_diameter_mm.min(surface.signed_radius_mm.abs())
    }
}

/// The classic smoothstep: 0 below `edge0`, 1 above `edge1`, the smooth Hermite between.
fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Cumulative vertex positions: surface 0 at z = 0, surface i at the sum of the distances
/// before it.
fn vertex_positions(prescription: &LensPrescription) -> Vec<f32> {
    let mut out = Vec::with_capacity(prescription.surfaces.len());
    let mut z = 0.0;
    for surface in &prescription.surfaces {
        out.push(z);
        z += surface.axial_distance_to_next_mm;
    }
    out
}

/// Refractive index of the medium after each surface at `wavelength_nm`; the medium before
/// surface 0 is air.
fn media_indices(prescription: &LensPrescription, wavelength_nm: f32) -> Vec<f32> {
    prescription
        .surfaces
        .iter()
        .map(|s| cauchy_refractive_index(s.refractive_index_d, s.abbe_v_d, wavelength_nm))
        .collect()
}

/// The media either side of surface `i` of a media list: (before, after).
fn media_pair(media: &[f32], i: usize) -> (f32, f32) {
    let before = if i == 0 { 1.0 } else { media[i - 1] };
    (before, media[i])
}

/// Propagates the state onto the image plane and freezes it into a [`TracedRay`].
fn finish(mut state: RayState, image_z: f32) -> TracedRay {
    if state.dir.z.abs() > f32::EPSILON {
        let t = (image_z - state.origin.z) / state.dir.z;
        if t.is_finite() && t > 0.0 {
            state.origin += state.dir * t;
            return TracedRay {
                landing: state.origin,
                direction: state.dir,
                weight: state.weight,
                worst_aperture_ratio: state.worst_aperture_ratio,
            };
        }
    }
    // The path never reaches the sensor (a backward-facing exit): zero energy, geometry frozen.
    TracedRay {
        landing: state.origin,
        direction: state.dir,
        weight: 0.0,
        worst_aperture_ratio: state.worst_aperture_ratio,
    }
}

/// The wavelength a trace uses, with the dead-ray fallback applied (non-finite or non-positive
/// rays are dead anyway; their media values are never read).
fn trace_wavelength(ray: &Ray) -> f32 {
    if ray.wavelength_nm.is_finite() && ray.wavelength_nm > 0.0 {
        ray.wavelength_nm
    } else {
        D_LINE_NM
    }
}

/// The imaging path: refract through every surface in order, then land on the image plane.
/// This is the trace the focus and transmission tests verify; it never bounces.
///
/// Does not call [`LensPrescription::validate`]: the spec's trivial-singlet fallback (§13.8)
/// must stay traceable, so any non-empty surface list works. An empty list is
/// [`TraceError::EmptyPrescription`].
pub fn trace_sequential(
    prescription: &LensPrescription,
    settings: &TraceSettings,
    ray: Ray,
) -> Result<TracedRay, TraceError> {
    if prescription.surfaces.is_empty() {
        return Err(TraceError::EmptyPrescription);
    }
    let vertices = vertex_positions(prescription);
    let media = media_indices(prescription, trace_wavelength(&ray));
    let mut state = RayState::new(ray);
    for (i, surface) in prescription.surfaces.iter().enumerate() {
        let (n_before, n_after) = media_pair(&media, i);
        state.cross_surface(
            surface,
            vertices[i],
            n_before,
            n_after,
            Event::Refract,
            settings.dial(),
        );
    }
    Ok(finish(state, image_plane_z(prescription)))
}

/// Checks the shared trace preconditions: a non-empty surface list, `second` in range, and
/// `first` strictly before `second`.
fn check_bounces(prescription: &LensPrescription, first: usize, second: usize) -> Result<(), TraceError> {
    if prescription.surfaces.is_empty() {
        return Err(TraceError::EmptyPrescription);
    }
    if second >= prescription.surfaces.len() {
        return Err(TraceError::SurfaceIndex { index: second });
    }
    if first >= second {
        return Err(TraceError::GhostOrder { first, second });
    }
    Ok(())
}

/// Traces one two-bounce ghost path (§5.1): forward to `second`, reflect; back to `first`,
/// reflect; forward to the sensor, refracting at every other crossing. Every crossing multiplies
/// the energy by `1 - R`, the two bounces by `R` (§6).
///
/// Like [`trace_sequential`] this does not validate the prescription; indices are checked
/// ([`TraceError`]) but any physically odd data simply traces to a dim or dead result.
pub fn trace_ghost(
    prescription: &LensPrescription,
    settings: &TraceSettings,
    first: usize,
    second: usize,
    ray: Ray,
) -> Result<TracedRay, TraceError> {
    check_bounces(prescription, first, second)?;
    Ok(trace_ghost_inner(
        prescription,
        settings.dial(),
        first,
        second,
        ray,
    ))
}

/// The infallible ghost trace, for callers that already hold validated indices.
fn trace_ghost_inner(
    prescription: &LensPrescription,
    dial: f32,
    first: usize,
    second: usize,
    ray: Ray,
) -> TracedRay {
    let surfaces = &prescription.surfaces;
    let vertices = vertex_positions(prescription);
    let media = media_indices(prescription, trace_wavelength(&ray));
    let mut state = RayState::new(ray);
    // Forward leg to the rear bounce.
    for i in 0..second {
        let (n_before, n_after) = media_pair(&media, i);
        state.cross_surface(&surfaces[i], vertices[i], n_before, n_after, Event::Refract, dial);
    }
    // Rear bounce: the ray arrives from the before side and stays in it.
    let (n_before, n_after) = media_pair(&media, second);
    state.cross_surface(
        &surfaces[second],
        vertices[second],
        n_before,
        n_after,
        Event::Reflect,
        dial,
    );
    // Backward leg to the front bounce: each crossing runs medium-after into medium-before.
    for i in ((first + 1)..second).rev() {
        let (n_before, n_after) = media_pair(&media, i);
        state.cross_surface(&surfaces[i], vertices[i], n_after, n_before, Event::Refract, dial);
    }
    // Front bounce: the ray arrives from the after side.
    let (n_before, n_after) = media_pair(&media, first);
    state.cross_surface(
        &surfaces[first],
        vertices[first],
        n_after,
        n_before,
        Event::Reflect,
        dial,
    );
    // Forward leg to the sensor.
    for i in (first + 1)..surfaces.len() {
        let (n_before, n_after) = media_pair(&media, i);
        state.cross_surface(&surfaces[i], vertices[i], n_before, n_after, Event::Refract, dial);
    }
    finish(state, image_plane_z(prescription))
}

/// Every two-bounce candidate of the lens: the ordered pairs (front, rear) of reflecting
/// surfaces, front to back (§5.1). An interface reflects only when the refractive index changes
/// across it by more than [`REFLECTING_INDEX_CHANGE`] at the d line, so the air-to-air stop and
/// any index-matched cement drop out (§2.3). With `n` reflecting surfaces this is the full
/// `n * (n - 1) / 2` pairs — culling happens later, in [`rank_ghosts`]. Pairs come out sorted.
pub fn enumerate_ghost_pairs(
    prescription: &LensPrescription,
) -> Result<Vec<(usize, usize)>, PrescriptionError> {
    prescription.validate()?;
    let media = media_indices(prescription, D_LINE_NM);
    let reflecting: Vec<usize> = (0..media.len())
        .filter(|&i| {
            let (before, after) = media_pair(&media, i);
            (after - before).abs() > REFLECTING_INDEX_CHANGE
        })
        .collect();
    // n * (n - 1) / 2 ordered pairs, saturating so an all-air lens (zero reflecting surfaces)
    // underflows nowhere.
    let pair_count = reflecting.len() * reflecting.len().saturating_sub(1) / 2;
    let mut pairs = Vec::with_capacity(pair_count);
    for front in 0..reflecting.len() {
        for rear in (front + 1)..reflecting.len() {
            pairs.push((reflecting[front], reflecting[rear]));
        }
    }
    Ok(pairs)
}

/// Radii of the fixed pupil fan (fractions of the sample radius), hexagonal rings plus the
/// center ray: 19 rays, the same for every probe so results are comparable and deterministic.
const FAN_RING_FRACTIONS: [f32; 3] = [1.0 / 3.0, 2.0 / 3.0, 1.0];

/// The pupil fan's `(x, y)` offsets about the axis, millimetres.
fn pupil_fan_offsets(radius: f32) -> Vec<Vec3> {
    let mut offsets = vec![Vec3::ZERO];
    for &fraction in &FAN_RING_FRACTIONS {
        let r = radius * fraction;
        for i in 0..6u16 {
            let angle = f32::from(i) * (std::f32::consts::TAU / 6.0);
            offsets.push(Vec3::new(r * angle.cos(), r * angle.sin(), 0.0));
        }
    }
    offsets
}

/// An orthonormal basis perpendicular to a unit direction (deterministic; `direction` may be
/// any finite vector, normalized by the caller).
fn perpendicular_basis(direction: Vec3) -> (Vec3, Vec3) {
    let axis = if direction.x.abs() < 0.9 { Vec3::X } else { Vec3::Y };
    let e1 = direction.cross(axis).normalize_or_zero();
    let e2 = direction.cross(e1);
    (e1, e2)
}

/// Ray origins for a parallel bundle of the given direction: the pupil fan laid out in the
/// plane perpendicular to the direction, [`RAY_LEAD_MM`] in front of the first vertex.
fn fan_origins(direction: Vec3, radius: f32) -> Vec<Vec3> {
    let (e1, e2) = perpendicular_basis(direction);
    pupil_fan_offsets(radius)
        .into_iter()
        .map(|offset| e1 * offset.x + e2 * offset.y - direction * RAY_LEAD_MM)
        .collect()
}

/// The ghost's sensor footprint radius as a fraction of the sensor diagonal, for one incoming
/// light direction: the bounding radius of the fan's landings about their mean (§5.5's "small
/// ray spray"). Weight-zero rays still count: dead rays keep their geometry (§6), which is what
/// keeps the bounding size defined where the ghost folds. Indices are trusted; see
/// [`ghost_footprint_fraction`] for the checked form.
fn footprint_fraction_inner(
    prescription: &LensPrescription,
    dial: f32,
    first: usize,
    second: usize,
    direction: Vec3,
) -> f32 {
    let direction = if direction.is_finite() {
        direction.normalize_or_zero()
    } else {
        Vec3::ZERO
    };
    let direction = if direction == Vec3::ZERO {
        Vec3::Z
    } else {
        direction
    };
    let diag = sensor_diagonal(prescription);
    if diag <= 0.0 {
        return 0.0;
    }
    let origins = fan_origins(direction, entrance_pupil_radius(prescription));
    let mut centroid = Vec3::ZERO;
    let mut landings = Vec::with_capacity(origins.len());
    for origin in origins {
        let traced = trace_ghost_inner(
            prescription,
            dial,
            first,
            second,
            Ray::new(origin, direction, D_LINE_NM),
        );
        landings.push(traced.landing);
        centroid += traced.landing;
    }
    centroid /= landings.len() as f32;
    let mut radius_sq = 0.0f32;
    for landing in &landings {
        let d = *landing - centroid;
        radius_sq = radius_sq.max(d.x * d.x + d.y * d.y);
    }
    (radius_sq.sqrt() / diag).max(0.0)
}

/// `footprint_fraction_inner` with checked indices.
pub fn ghost_footprint_fraction(
    prescription: &LensPrescription,
    settings: &TraceSettings,
    first: usize,
    second: usize,
    direction: Vec3,
) -> Result<f32, TraceError> {
    check_bounces(prescription, first, second)?;
    Ok(footprint_fraction_inner(
        prescription,
        settings.dial(),
        first,
        second,
        direction,
    ))
}

/// The ghost's image size for the ray budget (§5.5): the larger of the footprint on axis and
/// with the light about a third of the way into the frame ([`SIZE_PROBE_UV`] at
/// [`SIZE_PROBE_FRAME_ASPECT`]).
pub fn ghost_size_fraction(
    prescription: &LensPrescription,
    settings: &TraceSettings,
    first: usize,
    second: usize,
) -> Result<f32, TraceError> {
    check_bounces(prescription, first, second)?;
    let on_axis = footprint_fraction_inner(prescription, settings.dial(), first, second, Vec3::Z);
    let off_axis = footprint_fraction_inner(
        prescription,
        settings.dial(),
        first,
        second,
        field_direction(prescription, SIZE_PROBE_FRAME_ASPECT, SIZE_PROBE_UV),
    );
    Ok(on_axis.max(off_axis))
}

/// Enumerates, probes and ranks the lens's two-bounce ghosts (§5.3): every pair from
/// [`enumerate_ghost_pairs`] is traced on axis with the fixed pupil fan at the three
/// [`PROBE_WAVELENGTHS_NM`]; its mean landed weight is its intensity. Paths below
/// [`GHOST_INTENSITY_CUTOFF`] are dropped; the rest are returned brightest first with
/// deterministic tie-breaking (the lower first bounce, then the lower second). Deterministic
/// throughout: fixed rays, fixed order, no scheduling.
pub fn rank_ghosts(
    prescription: &LensPrescription,
    settings: &TraceSettings,
) -> Result<Vec<Ghost>, PrescriptionError> {
    let pairs = enumerate_ghost_pairs(prescription)?;
    let dial = settings.dial();
    let fan = pupil_fan_offsets(entrance_pupil_radius(prescription));
    let mut ghosts = Vec::new();
    for &(first, second) in &pairs {
        let mut energy = 0.0f32;
        let mut centroid = Vec3::ZERO;
        let mut count = 0u16;
        for &wavelength in PROBE_WAVELENGTHS_NM.iter() {
            for offset in &fan {
                let ray = Ray::new(*offset - Vec3::Z * RAY_LEAD_MM, Vec3::Z, wavelength);
                let traced = trace_ghost_inner(prescription, dial, first, second, ray);
                energy += traced.weight;
                // Weighted by landed energy: a dead ray (weight 0) has no position worth averaging.
                centroid += traced.landing * traced.weight;
                count += 1;
            }
        }
        let count = f32::from(count);
        let intensity = energy / count;
        if intensity < GHOST_INTENSITY_CUTOFF {
            continue;
        }
        let size_fraction = ghost_size_fraction(prescription, settings, first, second).unwrap_or(0.0);
        ghosts.push(Ghost {
            first_bounce: first,
            second_bounce: second,
            intensity,
            size_fraction,
            centroid_mm: centroid / energy,
        });
    }
    ghosts.sort_by(|a, b| {
        b.intensity
            .total_cmp(&a.intensity)
            .then(a.first_bounce.cmp(&b.first_bounce))
            .then(a.second_bounce.cmp(&b.second_bounce))
    });
    Ok(ghosts)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Approximate equality on f32.
    fn close(a: f32, b: f32, tol: f32) -> bool {
        (a - b).abs() <= tol
    }

    /// A synthetic surface. The values are invented for these tests, not transcribed from any
    /// real lens (the spec's Regia condition: no lens data).
    fn surface(
        radius: f32,
        distance: f32,
        n: f32,
        abbe: f32,
        clear: f32,
        coating: f32,
        stop: bool,
    ) -> Surface {
        Surface {
            signed_radius_mm: radius,
            axial_distance_to_next_mm: distance,
            refractive_index_d: n,
            abbe_v_d: abbe,
            clear_semi_diameter_mm: clear,
            coating_factor: coating,
            is_stop: stop,
        }
    }

    /// A synthetic full-frame 50 mm f/2 prescription with the given surfaces.
    fn lens(surfaces: Vec<Surface>) -> LensPrescription {
        LensPrescription {
            name: "synthetic test lens".into(),
            focal_length_mm: 50.0,
            native_f_number: 2.0,
            sensor_width_mm: 36.0,
            sensor_height_mm: 24.0,
            provenance: "synthetic test prescription (invented, not transcribed)".into(),
            surfaces,
        }
    }

    /// A flat air-glass-air plate: two parallel flats 3 mm apart with 20 mm to the image plane.
    fn parallel_plate(clear: f32) -> LensPrescription {
        lens(vec![
            surface(0.0, 3.0, 1.5, 0.0, clear, 1.0, false),
            surface(0.0, 20.0, 1.0, 0.0, clear, 1.0, false),
        ])
    }

    const SETTINGS: TraceSettings = TraceSettings { coating: 1.0 };

    #[test]
    fn flat_parallel_plate_passes_rays_straight() {
        let plate = parallel_plate(10.0);
        // On-axis ray: straight through, exact.
        let straight = trace_sequential(
            &plate,
            &SETTINGS,
            Ray::new(Vec3::new(0.0, 0.0, -1.0), Vec3::Z, D_LINE_NM),
        )
        .expect("trace");
        assert_eq!(straight.landing, Vec3::new(0.0, 0.0, 23.0));
        assert_eq!(straight.direction, Vec3::Z);
        // Normal-incidence Fresnel at 1 -> 1.5 and back: ((1-1.5)/(1+1.5))^2 = 0.04 each way.
        assert!(close(straight.weight, 0.9216, 1e-6), "{}", straight.weight);

        // Tilted ray: the exit direction equals the entry direction exactly; the exit point is
        // shifted by the plate thickness along the refracted slope.
        let d_in = Vec3::new(0.15, 0.1, 1.0).normalize_or_zero();
        let tilted = trace_sequential(
            &plate,
            &SETTINGS,
            Ray::new(Vec3::new(0.3, -0.2, -1.0), d_in, D_LINE_NM),
        )
        .expect("trace");
        for axis in 0..3 {
            assert!(
                close(tilted.direction[axis], d_in[axis], 1e-6),
                "axis {axis}: {} vs {}",
                tilted.direction[axis],
                d_in[axis]
            );
        }
        let sin_i = (d_in.x * d_in.x + d_in.y * d_in.y).sqrt();
        let u = Vec3::new(d_in.x, d_in.y, 0.0) / sin_i;
        let sin_t = sin_i / 1.5;
        let tan_t = sin_t / (1.0 - sin_t * sin_t).sqrt();
        let entry = Vec3::new(0.3, -0.2, -1.0) + d_in * (1.0 / d_in.z);
        let exit = entry + u * (3.0 * tan_t) + Vec3::Z * 3.0;
        let expected = exit + d_in * ((23.0 - 3.0) / d_in.z);
        for axis in 0..3 {
            assert!(
                close(tilted.landing[axis], expected[axis], 1e-5),
                "axis {axis}: {} vs {}",
                tilted.landing[axis],
                expected[axis]
            );
        }
        let cos_entry = d_in.z;
        let cos_exit = (1.0 - sin_t * sin_t).sqrt();
        let energy = (1.0 - fresnel_reflectance(1.0, 1.5, cos_entry))
            * (1.0 - fresnel_reflectance(1.5, 1.0, cos_exit));
        assert!(close(tilted.weight, energy, 1e-6), "{}", tilted.weight);
        assert!(tilted.weight <= 1.0);
        // The worst ratio is at the exit flat, where the ray is farthest from the axis; the
        // plate's clear semi-diameter is 10 mm.
        let exit_ratio = (exit.x * exit.x + exit.y * exit.y).sqrt() / 10.0;
        assert!(
            close(tilted.worst_aperture_ratio, exit_ratio, 1e-4),
            "{} vs {exit_ratio}",
            tilted.worst_aperture_ratio
        );
    }

    #[test]
    fn vignette_fades_to_zero_at_the_clear_aperture() {
        let plate = parallel_plate(1.0);
        let weights = [0.5f32, 0.96, 0.99, 2.0].map(|x| {
            trace_sequential(
                &plate,
                &SETTINGS,
                Ray::new(Vec3::new(x, 0.0, -1.0), Vec3::Z, D_LINE_NM),
            )
            .expect("trace")
            .weight
        });
        // Full below 0.95, smoothstep-faded to exactly zero at ratio >= 1 (both surfaces).
        assert!(close(weights[0], 0.9216, 1e-6), "{}", weights[0]);
        let t = 0.2f32; // smoothstep argument at ratio 0.96
        let fade = 1.0 - (t * t * (3.0 - 2.0 * t));
        assert!(close(weights[1], 0.9216 * fade * fade, 1e-5), "{}", weights[1]);
        assert!(weights[1] < weights[0] && weights[2] < weights[1]);
        assert_eq!(weights[3], 0.0);
        let blocked = trace_sequential(
            &plate,
            &SETTINGS,
            Ray::new(Vec3::new(2.0, 0.0, -1.0), Vec3::Z, D_LINE_NM),
        )
        .expect("trace");
        assert_eq!(blocked.worst_aperture_ratio, 2.0);
        // The blocked ray keeps its geometry: it lands on the image plane.
        assert_eq!(blocked.landing, Vec3::new(2.0, 0.0, 23.0));
    }

    #[test]
    fn singlet_focus_lands_at_the_analytic_focal_length() {
        // Biconvex singlet, 50/-50 mm radii, 5 mm thick, n = 1.5: the spec's trivial-singlet
        // class, traced raw (validate deliberately does not gate the trace).
        let singlet = lens(vec![
            surface(50.0, 5.0, 1.5, 0.0, 10.0, 1.0, false),
            surface(-50.0, 60.0, 1.0, 0.0, 10.0, 1.0, false),
        ]);
        // Paraxial reference (standard first-order y-nu transfer/refraction equations).
        let (efl, bfl) = paraxial_thick_lens(50.0, -50.0, 5.0, 1.5);
        assert!(close(bfl, 49.152542, 1e-3) && close(efl, 50.847458, 1e-3));

        let h = 0.5f32;
        let traced = trace_sequential(
            &singlet,
            &SETTINGS,
            Ray::new(Vec3::new(h, 0.0, -1.0), Vec3::Z, D_LINE_NM),
        )
        .expect("trace");
        assert!(close(traced.landing.z, 65.0, 1e-4));
        // Where the marginal ray crosses the axis: the exact-trace back focus.
        let slope = traced.direction.x / traced.direction.z;
        assert!(slope < 0.0, "the ray converges");
        let z_focus = traced.landing.z - traced.landing.x / slope;
        let traced_bfl = z_focus - 5.0; // from the rear vertex
        assert!((traced_bfl - bfl).abs() / bfl < 0.01, "bfl {traced_bfl} vs {bfl}");
        // The effective focal length from the exit slope of a ray entering at height h.
        let traced_efl = h / -slope;
        assert!((traced_efl - efl).abs() / efl < 0.01, "efl {traced_efl} vs {efl}");
    }

    /// Paraxial (first-order) effective and back focal length of a thick biconvex lens in air,
    /// from the standard transfer/refraction equations (y-nu trace of a ray from infinity).
    fn paraxial_thick_lens(r1: f32, r2: f32, thickness: f32, n: f32) -> (f32, f32) {
        let h = 1.0f32;
        let n1u1 = -(h * (n - 1.0) / r1); // surface 1: air -> glass
        let u1 = n1u1 / n;
        let y2 = h + thickness * u1;
        let n2u2 = n * u1 - (y2 * (1.0 - n) / r2); // surface 2: glass -> air
        let efl = h / -n2u2;
        let bfl = y2 / -n2u2;
        (efl, bfl)
    }

    #[test]
    fn total_internal_reflection_zeroes_weight_but_keeps_geometry() {
        // A steeply curved rear surface sends a marginal ray past the glass-air critical angle.
        let bent = lens(vec![
            surface(25.0, 6.0, 1.5, 0.0, 12.0, 1.0, false),
            surface(-5.0, 20.0, 1.0, 0.0, 12.0, 1.0, false),
        ]);
        let tir = trace_sequential(
            &bent,
            &SETTINGS,
            Ray::new(Vec3::new(3.5, 0.0, -1.0), Vec3::Z, D_LINE_NM),
        )
        .expect("trace");
        assert_eq!(tir.weight, 0.0);
        assert!(tir.landing.is_finite());
        assert!(
            close(tir.landing.z, 26.0, 1e-4),
            "the dead ray still lands on the plane"
        );
        // The reflectance itself reads 1 beyond the critical angle (sin_c = 1/1.5).
        let cos_critical = (1.0_f32 - (1.0 / 1.5) * (1.0 / 1.5)).sqrt();
        assert!(fresnel_reflectance(1.5, 1.0, cos_critical) >= 1.0 - 1e-6);
        // A paraxial ray through the same lens transmits with nearly the bare-glass losses.
        let mild = trace_sequential(
            &bent,
            &SETTINGS,
            Ray::new(Vec3::new(1.0, 0.0, -1.0), Vec3::Z, D_LINE_NM),
        )
        .expect("trace");
        assert!(mild.weight > 0.9 && mild.weight <= 1.0, "{}", mild.weight);
    }

    #[test]
    fn ghost_pair_count_is_n_choose_2_before_culling() {
        // Every surface of this stack changes index, so all N surfaces reflect.
        let all_reflecting = |count: usize| {
            let media = [1.5f32, 1.2, 1.6, 1.3, 1.0];
            let mut surfaces = Vec::new();
            for (i, &index) in media.iter().enumerate().take(count) {
                surfaces.push(surface(
                    if i.is_multiple_of(2) { 40.0 } else { -40.0 },
                    2.0,
                    index,
                    30.0,
                    12.0,
                    1.0,
                    false,
                ));
            }
            lens(surfaces)
        };
        for n in [3usize, 5] {
            let pairs = enumerate_ghost_pairs(&all_reflecting(n)).expect("valid");
            assert_eq!(pairs.len(), n * (n - 1) / 2, "n = {n}");
            assert_eq!(pairs[0], (0, 1));
            assert_eq!(pairs[pairs.len() - 1], (n - 2, n - 1));
        }
    }

    #[test]
    fn the_stop_and_index_matched_surfaces_never_reflect() {
        // Two singlets with an air-to-air stop between them: 5 surfaces, 4 reflecting.
        let prescription = lens(vec![
            surface(30.0, 4.0, 1.5, 50.0, 12.0, 1.0, false),
            surface(-30.0, 2.0, 1.0, 0.0, 12.0, 1.0, false),
            surface(0.0, 3.0, 1.0, 0.0, 12.0, 1.0, true),
            surface(25.0, 4.0, 1.6, 35.0, 12.0, 1.0, false),
            surface(-25.0, 10.0, 1.0, 0.0, 12.0, 1.0, false),
        ]);
        prescription.validate().expect("valid");
        let pairs = enumerate_ghost_pairs(&prescription).expect("valid");
        assert_eq!(pairs.len(), 4 * 3 / 2);
        for (first, second) in &pairs {
            assert!(first != second);
            assert!(
                *first != 2 && *second != 2,
                "the stop must not reflect: {pairs:?}"
            );
            assert!(first < second);
        }
    }

    #[test]
    fn ranked_ghosts_are_brightest_first_and_deterministic() {
        let prescription = lens(vec![
            surface(30.0, 4.0, 1.5, 50.0, 12.0, 1.0, false),
            surface(-30.0, 2.0, 1.0, 0.0, 12.0, 1.0, false),
            surface(0.0, 3.0, 1.0, 0.0, 12.0, 1.0, true),
            surface(25.0, 4.0, 1.6, 35.0, 12.0, 1.0, false),
            surface(-25.0, 10.0, 1.0, 0.0, 12.0, 1.0, false),
        ]);
        let ranked = rank_ghosts(&prescription, &SETTINGS).expect("valid");
        assert_eq!(ranked.len(), 6, "every pair survives the 1e-7 cut: {ranked:?}");
        let mut seen = Vec::new();
        for ghost in &ranked {
            assert!(ghost.intensity >= GHOST_INTENSITY_CUTOFF);
            assert!(ghost.intensity <= 1.0, "energy never increases");
            assert!(ghost.size_fraction >= 0.0 && ghost.size_fraction.is_finite());
            assert!(close(ghost.centroid_mm.z, 23.0, 1e-4));
            assert!(!seen.contains(&(ghost.first_bounce, ghost.second_bounce)));
            seen.push((ghost.first_bounce, ghost.second_bounce));
        }
        for pair in ranked.windows(2) {
            assert!(
                pair[0].intensity >= pair[1].intensity,
                "sorted brightest first: {} then {}",
                pair[0].intensity,
                pair[1].intensity
            );
        }
        // Determinism: the same inputs give the identical list, bit for bit.
        let again = rank_ghosts(&prescription, &SETTINGS).expect("valid");
        assert_eq!(ranked, again);
    }

    #[test]
    fn coating_trades_reflection_for_transmission() {
        let base = lens(vec![
            surface(30.0, 4.0, 1.5, 50.0, 12.0, 1.0, false),
            surface(-30.0, 2.0, 1.0, 0.0, 12.0, 1.0, false),
            surface(0.0, 3.0, 1.0, 0.0, 12.0, 1.0, true),
            surface(25.0, 4.0, 1.6, 35.0, 12.0, 1.0, false),
            surface(-25.0, 10.0, 1.0, 0.0, 12.0, 1.0, false),
        ]);
        // With every factor at 1 (bare glass) the dial has nothing to blend: identical results.
        let uncoated = rank_ghosts(&base, &TraceSettings { coating: 0.0 }).expect("valid");
        let as_described = rank_ghosts(&base, &SETTINGS).expect("valid");
        assert_eq!(uncoated, as_described);

        // Coating one surface dims exactly the ghosts that bounce off it; ghosts that only
        // transmit through it get brighter (less light lost). All six pairs still survive the
        // cut at factor 0.5.
        let mut coated = base.clone();
        coated.surfaces[1].coating_factor = 0.5;
        let ranked = rank_ghosts(&coated, &SETTINGS).expect("valid");
        assert_eq!(ranked.len(), 6, "{ranked:?}");
        let lookup = |list: &[Ghost], first: usize, second: usize| {
            list.iter()
                .find(|g| g.first_bounce == first && g.second_bounce == second)
                .map(|g| g.intensity)
        };
        for ghost in &as_described {
            let coated_intensity = lookup(&ranked, ghost.first_bounce, ghost.second_bounce);
            let bounces_on_coated = ghost.first_bounce == 1 || ghost.second_bounce == 1;
            match coated_intensity {
                Some(v) if bounces_on_coated => assert!(v < ghost.intensity),
                Some(v) => assert!(v >= ghost.intensity),
                None => panic!("a half-coated surface must not cull its pairs"),
            }
        }

        // A fully coated (factor 0) surface kills its bounce pairs outright: they fall below
        // the cut, and only the three pairs that bounce elsewhere survive.
        let mut black = base.clone();
        black.surfaces[1].coating_factor = 0.0;
        let ranked = rank_ghosts(&black, &SETTINGS).expect("valid");
        let survivors: Vec<(usize, usize)> =
            ranked.iter().map(|g| (g.first_bounce, g.second_bounce)).collect();
        assert!(ranked.iter().all(|g| g.first_bounce != 1 && g.second_bounce != 1));
        assert_eq!(survivors.len(), 3, "{survivors:?}");
    }

    #[test]
    fn energy_never_increases() {
        let prescription = lens(vec![
            surface(30.0, 4.0, 1.5, 50.0, 12.0, 1.0, false),
            surface(-30.0, 2.0, 1.0, 0.0, 12.0, 1.0, false),
            surface(0.0, 3.0, 1.0, 0.0, 12.0, 1.0, true),
            surface(25.0, 4.0, 1.6, 35.0, 12.0, 1.0, false),
            surface(-25.0, 10.0, 1.0, 0.0, 12.0, 1.0, false),
        ]);
        // Every crossing multiplies by R or 1 - R, both in 0..=1, so no ray or ghost can gain.
        let pairs = enumerate_ghost_pairs(&prescription).expect("valid");
        for offset in pupil_fan_offsets(entrance_pupil_radius(&prescription)) {
            let ray = Ray::new(offset - Vec3::Z * RAY_LEAD_MM, Vec3::Z, D_LINE_NM);
            let traced = trace_sequential(&prescription, &SETTINGS, ray).expect("trace");
            assert!(traced.weight <= 1.0 && traced.weight >= 0.0, "{}", traced.weight);
            for &(first, second) in &pairs {
                let ghost = trace_ghost(&prescription, &SETTINGS, first, second, ray).expect("ghost");
                assert!(ghost.weight <= 1.0 && ghost.weight >= 0.0, "{}", ghost.weight);
            }
        }
        // The reflectance itself stays a probability, and vanishes at an index match.
        for &n in &[1.0f32, 1.2, 1.5, 1.6, 1.9] {
            assert_eq!(fresnel_reflectance(n, n, 0.7), 0.0);
            for &cos in &[1.0f32, 0.9, 0.7, 0.4, 0.1] {
                let r = fresnel_reflectance(1.0, n, cos);
                assert!((0.0..=1.0).contains(&r), "r({n}, {cos}) = {r}");
            }
        }
    }

    #[test]
    fn flat_plate_ghost_footprint_is_the_pupil_fan() {
        // A parallel plate's two-bounce ghost is the pupil disc itself: every fan ray returns
        // along its own path, so the bounding radius is exactly the fan radius.
        let plate = parallel_plate(10.0);
        let fraction = ghost_footprint_fraction(&plate, &SETTINGS, 0, 1, Vec3::Z).expect("trace");
        let expected = 10.0 / (36.0f32 * 36.0 + 24.0 * 24.0).sqrt();
        assert!(close(fraction, expected, 1e-4), "{fraction} vs {expected}");
        // The size probe keeps the larger of on-axis and a third into the frame.
        let size = ghost_size_fraction(&plate, &SETTINGS, 0, 1).expect("trace");
        assert!(size >= fraction - 1e-6 && size < fraction * 1.5 + 1e-6);
        // Deterministic: identical bits on a second call.
        assert_eq!(
            ghost_footprint_fraction(&plate, &SETTINGS, 0, 1, Vec3::Z),
            Ok(fraction)
        );
        // And the ghost carries the expected energy: (1-R0) R1 R0 (1-R1) at normal incidence.
        let ghost = trace_ghost(
            &plate,
            &SETTINGS,
            0,
            1,
            Ray::new(Vec3::new(0.5, 0.0, -1.0), Vec3::Z, D_LINE_NM),
        )
        .expect("ghost");
        assert!(
            close(ghost.weight, 0.96 * 0.04 * 0.04 * 0.96, 1e-8),
            "{}",
            ghost.weight
        );
        assert_eq!(ghost.landing, Vec3::new(0.5, 0.0, 23.0));
    }

    #[test]
    fn invalid_prescriptions_are_typed_errors() {
        let good = lens(vec![
            surface(30.0, 4.0, 1.5, 50.0, 12.0, 1.0, false),
            surface(-30.0, 2.0, 1.0, 0.0, 12.0, 1.0, false),
            surface(0.0, 3.0, 1.0, 0.0, 12.0, 1.0, true),
            surface(25.0, 4.0, 1.6, 35.0, 12.0, 1.0, false),
            surface(-25.0, 10.0, 1.0, 0.0, 12.0, 1.0, false),
        ]);
        good.validate().expect("the baseline is valid");

        let mut bad = good.clone();
        bad.focal_length_mm = 0.0;
        assert_eq!(bad.validate(), Err(PrescriptionError::FocalLength(0.0)));

        let mut bad = good.clone();
        bad.focal_length_mm = f32::NAN;
        assert!(matches!(
            bad.validate(),
            Err(PrescriptionError::FocalLength(v)) if v.is_nan()
        ));

        let mut bad = good.clone();
        bad.sensor_width_mm = 0.0;
        assert_eq!(
            bad.validate(),
            Err(PrescriptionError::SensorFormat {
                width: 0.0,
                height: 24.0
            })
        );

        let mut bad = good.clone();
        bad.native_f_number = -2.0;
        assert_eq!(bad.validate(), Err(PrescriptionError::NativeFNumber(-2.0)));

        // Only two usable surfaces (three skipped): not a lens.
        let mut bad = good.clone();
        bad.surfaces[2].clear_semi_diameter_mm = 0.0;
        bad.surfaces[3].clear_semi_diameter_mm = 0.0;
        bad.surfaces[4].clear_semi_diameter_mm = -1.0;
        assert_eq!(
            bad.validate(),
            Err(PrescriptionError::TooFewUsableSurfaces { usable: 2 })
        );
        let mut two = lens(vec![
            surface(50.0, 5.0, 1.5, 0.0, 10.0, 1.0, false),
            surface(-50.0, 60.0, 1.0, 0.0, 10.0, 1.0, false),
        ]);
        two.surfaces[0].clear_semi_diameter_mm = 0.0;
        assert_eq!(
            two.validate(),
            Err(PrescriptionError::TooFewUsableSurfaces { usable: 1 })
        );

        let mut bad = good.clone();
        bad.surfaces[1].is_stop = true;
        assert_eq!(bad.validate(), Err(PrescriptionError::MultipleStops { count: 2 }));

        let mut bad = good.clone();
        bad.surfaces[1].signed_radius_mm = f32::NAN;
        assert_eq!(
            bad.validate(),
            Err(PrescriptionError::NonFiniteField {
                index: 1,
                field: "signed_radius_mm"
            })
        );

        let mut bad = good.clone();
        bad.surfaces[0].refractive_index_d = 0.9;
        assert_eq!(
            bad.validate(),
            Err(PrescriptionError::RefractiveIndexBelowOne { index: 0, n: 0.9 })
        );

        let mut bad = good.clone();
        bad.surfaces[2].abbe_v_d = -1.0;
        assert_eq!(
            bad.validate(),
            Err(PrescriptionError::NegativeAbbe { index: 2, v: -1.0 })
        );

        let mut bad = good.clone();
        bad.surfaces[1].axial_distance_to_next_mm = -2.0;
        assert_eq!(
            bad.validate(),
            Err(PrescriptionError::NegativeDistance {
                index: 1,
                distance: -2.0
            })
        );

        let mut bad = good.clone();
        bad.surfaces[0].coating_factor = 1.5;
        assert_eq!(
            bad.validate(),
            Err(PrescriptionError::CoatingFactor {
                index: 0,
                factor: 1.5
            })
        );

        // Enumeration refuses an invalid prescription; the raw traces check their own inputs.
        let mut bad = good.clone();
        bad.surfaces[3].coating_factor = 2.0;
        assert!(matches!(
            enumerate_ghost_pairs(&bad),
            Err(PrescriptionError::CoatingFactor { index: 3, .. })
        ));
        let empty = lens(Vec::new());
        assert_eq!(
            trace_sequential(&empty, &SETTINGS, Ray::default()),
            Err(TraceError::EmptyPrescription)
        );
        assert_eq!(
            trace_ghost(&good, &SETTINGS, 3, 2, Ray::default()),
            Err(TraceError::GhostOrder { first: 3, second: 2 })
        );
        assert_eq!(
            trace_ghost(&good, &SETTINGS, 0, 7, Ray::default()),
            Err(TraceError::SurfaceIndex { index: 7 })
        );
    }

    #[test]
    fn cauchy_dispersion_passes_through_the_d_line() {
        // The fit reproduces n_d at the d line and the Abbe spread between C and F.
        let (n_d, v_d) = (1.5f32, 50.0f32);
        assert!(close(cauchy_refractive_index(n_d, v_d, D_LINE_NM), n_d, 1e-6));
        let n_f = cauchy_refractive_index(n_d, v_d, F_LINE_NM);
        let n_c = cauchy_refractive_index(n_d, v_d, C_LINE_NM);
        assert!(close(n_f - n_c, (n_d - 1.0) / v_d, 1e-6), "{}", n_f - n_c);
        assert!(n_f > n_d && n_d > n_c, "blue bends more");
        // Air and "no dispersion data" stay flat; a bad wavelength reads as the d line.
        assert_eq!(cauchy_refractive_index(1.0, 0.0, F_LINE_NM), 1.0);
        assert_eq!(cauchy_refractive_index(n_d, 0.0, F_LINE_NM), n_d);
        assert_eq!(cauchy_refractive_index(n_d, v_d, 0.0), n_d);
        assert_eq!(cauchy_refractive_index(n_d, v_d, f32::NAN), n_d);
    }

    #[test]
    fn fresnel_reflectance_matches_normal_incidence_and_tir() {
        // Normal incidence: ((n1 - n2) / (n1 + n2))^2, exactly for air -> glass of 1.5.
        assert!(close(fresnel_reflectance(1.0, 1.5, 1.0), 0.04, 1e-7));
        assert_eq!(
            fresnel_reflectance(1.0, 1.5, 1.0),
            fresnel_reflectance(1.5, 1.0, 1.0),
            "symmetric across the interface"
        );
        // Grazing incidence reflects everything; TIR is exactly 1; mid angles sit between.
        assert!(fresnel_reflectance(1.0, 1.5, 0.01) > 0.9);
        assert_eq!(fresnel_reflectance(1.5, 1.0, 0.5), 1.0);
        assert_eq!(fresnel_reflectance(1.5, 1.0, 0.0), 1.0);
        let steep = fresnel_reflectance(1.0, 1.5, 0.8);
        let shallow = fresnel_reflectance(1.0, 1.5, 0.4);
        assert!(steep < 0.1 && shallow > steep && shallow <= 1.0);
    }

    #[test]
    fn field_direction_crops_the_sensor_to_the_frame() {
        let prescription = lens(Vec::new());
        // A 16:9 frame on a 36 x 24 sensor: vertical maps across 36 * 9/16 = 20.25 mm.
        let d = field_direction(&prescription, 16.0 / 9.0, [-1.0 / 3.0, -1.0 / 3.0]);
        let expected = Vec3::new(-6.0, -3.375, 50.0).normalize_or_zero();
        for axis in 0..3 {
            assert!(close(d[axis], expected[axis], 1e-6));
        }
        assert!(d.z > 0.0, "light still travels toward the image");
        // Center of the frame is on-axis for any aspect.
        assert_eq!(field_direction(&prescription, 2.0, [0.0, 0.0]), Vec3::Z);
        // A 3:2 frame on the 3:2 sensor maps the corner to the sensor corner.
        let corner = field_direction(&prescription, 1.5, [1.0, 1.0]);
        let expected = Vec3::new(18.0, 12.0, 50.0).normalize_or_zero();
        for axis in 0..3 {
            assert!(close(corner[axis], expected[axis], 1e-6));
        }
    }

    #[test]
    fn entrance_pupil_follows_the_front_element() {
        let mut prescription = lens(Vec::new());
        prescription.surfaces = vec![surface(30.0, 4.0, 1.5, 50.0, 12.0, 1.0, false)];
        // f/2: 50 / 4 = 12.5, with 50% margin 18.75, clamped to the front semi-diameter 12.
        assert_eq!(entrance_pupil_radius(&prescription), 12.0);
        // No stored f-number: estimated as focal length over the front clear diameter (25),
        // so the pupil is 50 / (2 * 2.5) * 1.5 = 15, again clamped to 10.
        prescription.native_f_number = 0.0;
        prescription.surfaces[0].clear_semi_diameter_mm = 10.0;
        assert_eq!(entrance_pupil_radius(&prescription), 10.0);
        // Neither stored nor estimable: no pupil.
        prescription.surfaces[0].clear_semi_diameter_mm = 0.0;
        assert_eq!(entrance_pupil_radius(&prescription), 0.0);
        // The image plane sits at the sum of all axial distances.
        assert_eq!(image_plane_z(&parallel_plate(10.0)), 23.0);
        assert_eq!(
            sensor_diagonal(&parallel_plate(10.0)),
            (36.0f32 * 36.0 + 24.0 * 24.0).sqrt()
        );
    }
}
