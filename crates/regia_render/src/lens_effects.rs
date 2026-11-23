// SPDX-License-Identifier: MPL-2.0
//! Runtime lens effects (ADR-1863): ghost lens flare, lens dirt and lateral chromatic
//! aberration, as one pass over the scene-referred HDR image at the frame's `before_post` slot
//! (`frame/extension.rs`).
//!
//! # What a frame gets
//!
//! - **Ghost flare.** The lens description ([`LensPrescription`], a synthetic built-in by default:
//!   [`default_flare_lens`]) is traced once on the CPU with the reference module
//!   ([`crate::post_ref::lens_flare`]): its two-bounce ghosts are ranked by traced energy and the
//!   [`MAX_GHOSTS`] most visible (energy per footprint area) become a [`GhostTable`]. For each
//!   ghost the table holds, per red, green and blue probe wavelength, the energy that lands in
//!   the ghost (its colour, with a quarter-wave coating's tint on coated bounces) and
//!   its *magnification* `m`: a light at centred frame position `c` puts the ghost at `m · c`
//!   (paraxial ghosts are linear in the field; `m` is negative for a ghost on the other side of
//!   the centre), plus the ghost's footprint radius. On the GPU every bright pixel of the frame
//!   (above [`LensEffects::flare_threshold`]) is a light: the bright image is reduced to a grid of
//!   cells (energy, centroid, spread) and each lit cell draws one sprite per ghost, an image of
//!   the iris at `m · c`, per channel at that channel's `m`, spreading the cell's energy over the
//!   ghost's area. With [`LensEffects::flare_intensity`] 1 the ghosts carry the energy the trace
//!   gives them (Hullin, Eisemann, Seidel and Lee, "Physically-Based Real-Time Lens Flare
//!   Rendering", ACM TOG 30(4), 2011: two-bounce ghosts, their per-ghost probe and the aperture
//!   image; the cells are Regia's own simplification of that per-light rasterisation).
//! - **Lens dirt.** A procedural dirt mask (specks and smudges, deterministic) lit by the flare
//!   and by a wide blur of the bright image: dust on the front element catching the glare.
//! - **Chromatic aberration.** Lateral (transverse) colour: red is magnified and blue minified
//!   about the frame centre by [`LensEffects::chromatic_aberration`] (a fraction of the radius),
//!   green stays put.
//!
//! # Opt-in twice, like the camera lens (ADR-041)
//!
//! A quality preset allows the pass ([`RenderProfile::lens_effects`]: Desktop High, Desktop Ultra
//! and the cinematic profile), and the view asks for it: the project's
//! [`PostSettings::lens_effects`], a camera's [`LensEffects`] component (which replaces the
//! project's for that camera) and post volumes ([`crate::post_volumes::PostOverrides`]'s
//! `lens_*` and `chromatic_aberration` fields) set the strengths. Every strength is 0 by
//! default, so a frame that asks for nothing records nothing and is bit-identical to a frame
//! without the pass.

pub(crate) mod gpu;

pub use gpu::{
    DIRT_SCATTER, LENS_EFFECTS_PASS_NAME, LensEffectsCounters, LensEffectsPass, lens_effects_counters,
};
pub use gpu::{LensEffectsBudget, LensEffectsMemory, lens_effects_memory};

use regia_app::prelude::*;
use regia_core::glam::Vec3;

use crate::post::PostSettings;
use crate::post_ref::lens_flare::{
    self, LensPrescription, PROBE_WAVELENGTHS_NM, PrescriptionError, RAY_LEAD_MM, Ray, Surface, TraceSettings,
};
use crate::profile::RenderProfile;

/// Most ghosts the pass draws (the most visible of the ranked list).
pub const MAX_GHOSTS: usize = 8;

/// [`LensEffects::flare_intensity`]'s range (1 = the traced energy).
pub const FLARE_INTENSITY_RANGE: (f32, f32) = (0.0, 1000.0);
/// [`LensEffects::flare_threshold`]'s range (exposed luminance, 1 at display white).
pub const FLARE_THRESHOLD_RANGE: (f32, f32) = (0.0, 65_504.0);
/// [`LensEffects::dirt_intensity`]'s range.
pub const DIRT_INTENSITY_RANGE: (f32, f32) = (0.0, 10.0);
/// [`LensEffects::chromatic_aberration`]'s range (fraction of the centre-to-corner radius).
pub const CHROMATIC_ABERRATION_RANGE: (f32, f32) = (0.0, 0.05);
/// [`LensEffects::starburst`]'s range (1 = the spikes carry as much light as the source gives them).
pub const STARBURST_RANGE: (f32, f32) = (0.0, 10.0);
/// The `lens-effects` format version this build reads and writes (ADR-3196).
pub const LENS_EFFECTS_FORMAT_VERSION: u32 = 1;
/// The default [`LensEffects::flare_threshold`]: two stops above display white, so only
/// highlights (the sun, lamps, specular glints) cast ghosts.
pub const DEFAULT_FLARE_THRESHOLD: f32 = 4.0;
/// The smallest ghost magnification (`|m|` below it reads as this): a ghost that barely moves
/// with the light would sit as a fixed blob on the centre.
pub const MIN_GHOST_MAGNIFICATION: f32 = 0.125;
/// The largest ghost magnification kept: a ghost that runs off the frame faster than eight times
/// the light is invisible in practice and is dropped from the table.
pub const MAX_GHOST_MAGNIFICATION: f32 = 8.0;
/// The field position (centred frame coordinates) the magnification is measured at: small enough
/// to stay paraxial, large enough that the landing is not lost in rounding.
const MAGNIFICATION_PROBE_FIELD: f32 = 0.2;

/// The strengths of the lens effects of a view.
///
/// As a component on a [`Camera`](crate::Camera) entity it replaces the project's
/// [`PostSettings::lens_effects`] for that camera; post volumes then override single fields
/// (`crate::post_volumes`), and the quality preset must allow the pass
/// ([`RenderProfile::lens_effects`]). Every strength 0 ([`Self::OFF`], the default) is off.
///
/// # Example
///
/// ```
/// use regia_render::{LensEffects, PostSettings, RenderProfile, RenderProfilePreset};
///
/// let mut post = PostSettings::default();
/// LensEffects::CINEMATIC.apply_camera(&mut post);
/// regia_render::gate_lens_effects(&mut post, &RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
/// assert!(post.lens_effects.is_active());
///
/// let mut mobile = post.clone();
/// regia_render::gate_lens_effects(&mut mobile, &RenderProfile::from_preset(RenderProfilePreset::MobileLow));
/// assert!(!mobile.lens_effects.is_active(), "the preset does not allow the pass");
/// ```
///
/// Saved in scene files as the reflected component `regia_render::lens_effects::LensEffects`
/// (`lens-effects` v1, ADR-3196): a component without `format_version` loads as v1, a newer
/// one is ignored by the renderer (logged once).
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq)]
#[reflect(Component, Default, Clone)]
#[type_path = "regia_render::lens_effects"]
pub struct LensEffects {
    /// Serialized-form version ([`LENS_EFFECTS_FORMAT_VERSION`] when written by this engine).
    pub format_version: u32,
    /// Ghost flare strength: 1 draws the ghosts with the energy the lens trace gives them, 0 is
    /// off ([`FLARE_INTENSITY_RANGE`]). Small sources need more than 1 to read on screen.
    pub flare_intensity: f32,
    /// Exposed luminance (scene luminance × the view's exposure, before the tonemap: 1 is display
    /// white) above which a pixel casts ghosts and lights the dirt
    /// ([`DEFAULT_FLARE_THRESHOLD`]). The part above the threshold is what casts. Because it is
    /// exposed, the same highlight casts the same flare as an auto exposure adapts.
    pub flare_threshold: f32,
    /// The lens trace's coating dial, 0–1 ([`TraceSettings::coating`]): 1 applies the lens's
    /// anti-reflection coatings in full (dim ghosts), 0 traces bare glass (bright ghosts). The
    /// built-in lens snaps it to steps of 1/16 and traces each step once, on the first frame
    /// that uses it (the renderer's warm traces the default, 1).
    pub coating: f32,
    /// Lens dirt strength, 0 off ([`DIRT_INTENSITY_RANGE`]).
    pub dirt_intensity: f32,
    /// Lateral chromatic aberration: red is magnified and blue minified about the frame centre
    /// by this fraction of the radius ([`CHROMATIC_ABERRATION_RANGE`]); 0 is off.
    pub chromatic_aberration: f32,
    /// Diffraction starburst: the iris blades spread each bright pixel into spikes, one per blade
    /// (six), brightest at the source ([`STARBURST_RANGE`]); 0 is off.
    pub starburst: f32,
    /// Run the pass after the camera's depth of field and motion blur instead of before them
    /// (`false`, the default): the flare, dirt and starburst then stay sharp over a defocused
    /// frame. Moves the frame's `before_post` host passes with it.
    pub after_dof: bool,
}

/// A [`LensEffects`] from a newer engine.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LensEffectsError {
    /// The component's `format_version` is newer than the engine's.
    #[error("lens effects format_version {found} is newer than this engine's {LENS_EFFECTS_FORMAT_VERSION}")]
    NewerVersion {
        /// The version found.
        found: u32,
    },
}

impl Default for LensEffects {
    fn default() -> Self {
        Self::OFF
    }
}

impl LensEffects {
    /// Every effect off (the default).
    pub const OFF: Self = Self {
        format_version: LENS_EFFECTS_FORMAT_VERSION,
        flare_intensity: 0.0,
        flare_threshold: DEFAULT_FLARE_THRESHOLD,
        coating: 1.0,
        dirt_intensity: 0.0,
        chromatic_aberration: 0.0,
        starburst: 0.0,
        after_dof: false,
    };

    /// A filmic starting point: the traced ghosts, a little dirt and a faint colour fringe.
    pub const CINEMATIC: Self = Self {
        format_version: LENS_EFFECTS_FORMAT_VERSION,
        flare_intensity: 1.0,
        flare_threshold: DEFAULT_FLARE_THRESHOLD,
        coating: 1.0,
        dirt_intensity: 0.3,
        chromatic_aberration: 0.002,
        starburst: 0.0,
        after_dof: false,
    };

    /// Brings data written by an older engine up to [`LENS_EFFECTS_FORMAT_VERSION`]: `0` (a
    /// hand-written component without the field) becomes 1.
    ///
    /// # Errors
    /// [`LensEffectsError::NewerVersion`] for data from a newer engine.
    pub fn migrate(mut self) -> Result<Self, LensEffectsError> {
        match self.format_version {
            0 => {
                self.format_version = LENS_EFFECTS_FORMAT_VERSION;
                Ok(self)
            }
            LENS_EFFECTS_FORMAT_VERSION => Ok(self),
            found => Err(LensEffectsError::NewerVersion { found }),
        }
    }

    /// The renderer can read this version (it is not newer than the engine's).
    #[must_use]
    pub fn is_supported(&self) -> bool {
        self.format_version <= LENS_EFFECTS_FORMAT_VERSION
    }

    /// The diffraction starburst draws (a positive strength).
    #[must_use]
    pub fn starburst_active(&self) -> bool {
        self.starburst > 0.0 && self.starburst.is_finite()
    }

    /// The values clamped into their ranges; a value that is not finite takes [`Self::OFF`]'s.
    #[must_use]
    pub fn sanitized(self) -> Self {
        fn clamp(value: f32, range: (f32, f32), fallback: f32) -> f32 {
            if value.is_finite() {
                value.clamp(range.0, range.1)
            } else {
                fallback
            }
        }
        Self {
            format_version: self.format_version,
            flare_intensity: clamp(self.flare_intensity, FLARE_INTENSITY_RANGE, 0.0),
            flare_threshold: clamp(
                self.flare_threshold,
                FLARE_THRESHOLD_RANGE,
                DEFAULT_FLARE_THRESHOLD,
            ),
            coating: clamp(self.coating, (0.0, 1.0), 1.0),
            dirt_intensity: clamp(self.dirt_intensity, DIRT_INTENSITY_RANGE, 0.0),
            chromatic_aberration: clamp(self.chromatic_aberration, CHROMATIC_ABERRATION_RANGE, 0.0),
            starburst: clamp(self.starburst, STARBURST_RANGE, 0.0),
            after_dof: self.after_dof,
        }
    }

    /// The ghost flare draws (a positive strength).
    #[must_use]
    pub fn flare_active(&self) -> bool {
        self.flare_intensity > 0.0 && self.flare_intensity.is_finite()
    }

    /// The dirt draws.
    #[must_use]
    pub fn dirt_active(&self) -> bool {
        self.dirt_intensity > 0.0 && self.dirt_intensity.is_finite()
    }

    /// The chromatic aberration shifts colour.
    #[must_use]
    pub fn chromatic_aberration_active(&self) -> bool {
        self.chromatic_aberration > 0.0 && self.chromatic_aberration.is_finite()
    }

    /// Any effect changes pixels; `false` means the pass records nothing.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.flare_active()
            || self.dirt_active()
            || self.chromatic_aberration_active()
            || self.starburst_active()
    }

    /// Makes this the view's lens effects (a camera's component replacing the project's), before
    /// post volumes blend over it.
    pub fn apply_camera(&self, post: &mut PostSettings) {
        post.lens_effects = self.sanitized();
    }
}

/// Turns the view's lens effects off when the quality preset does not allow them
/// ([`RenderProfile::lens_effects`]); runs after post volumes, so no volume can turn them on
/// where the preset does not.
pub fn gate_lens_effects(post: &mut PostSettings, profile: &RenderProfile) {
    if !profile.lens_effects {
        post.lens_effects = LensEffects::OFF;
    }
}

/// One ghost of a [`GhostTable`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GhostEntry {
    /// Index of the first (front) bounce surface.
    pub first_bounce: usize,
    /// Index of the second (rear) bounce surface.
    pub second_bounce: usize,
    /// Fraction of the light entering the pupil that lands in this ghost, at the red, green and
    /// blue probe wavelengths ([`PROBE_WAVELENGTHS_NM`]): the ghost's colour and brightness.
    pub energy: [f32; 3],
    /// Magnification per probe wavelength: a light at centred frame position `c` puts the ghost
    /// at `m · c`. Its magnitude is at least [`MIN_GHOST_MAGNIFICATION`] and at most
    /// [`MAX_GHOST_MAGNIFICATION`].
    pub magnification: [f32; 3],
    /// Footprint radius as a fraction of the frame width (the ghost's defocus disc): `√2 ×` the
    /// energy-weighted RMS spread of the probe fan's landings at the d line, the larger of on
    /// axis and in the field (the reference's bounding footprint when no ray lands).
    pub radius: f32,
}

/// The ghosts the pass draws for one lens and coating, most visible first.
#[derive(Debug, Clone, PartialEq)]
pub struct GhostTable {
    /// The lens's display name.
    pub lens: String,
    /// The coating dial the table was traced at.
    pub coating: f32,
    /// Ghosts the reference ranking found above its cutoff (before [`MAX_GHOSTS`] and the
    /// magnification limit).
    pub ranked: usize,
    /// At most [`MAX_GHOSTS`] ghosts, most visible first (energy per footprint area).
    pub ghosts: Vec<GhostEntry>,
}

/// Rings of the probes' pupil fan; ring `k` (1-based) holds `6 k` rays at `k / FAN_RINGS` of
/// the radius, so the fan (61 rays) samples the pupil about uniformly.
const FAN_RINGS: u8 = 4;
/// Fewest rays that must land with energy for a ghost's visible radius to be measured; below it
/// the reference's bounding footprint stands in (one or two surviving rays have no spread).
const MIN_LIVE_RAYS: usize = 4;

/// The pupil fan of the probes: the centre plus [`FAN_RINGS`] rings, in millimetres.
fn fan(radius: f32) -> Vec<(f32, f32)> {
    let mut out = vec![(0.0, 0.0)];
    for ring in 1..=FAN_RINGS {
        let r = radius * f32::from(ring) / f32::from(FAN_RINGS);
        let count = 6 * u16::from(ring);
        for i in 0..count {
            let angle = f32::from(i) * std::f32::consts::TAU / f32::from(count);
            out.push((r * angle.cos(), r * angle.sin()));
        }
    }
    out
}

/// Ray origins of a parallel bundle along `direction` through the pupil fan, in the plane
/// perpendicular to it, [`RAY_LEAD_MM`] in front of the first vertex.
fn bundle(direction: Vec3, radius: f32) -> Vec<Vec3> {
    let axis = if direction.x.abs() < 0.9 { Vec3::X } else { Vec3::Y };
    let e1 = direction.cross(axis).normalize_or_zero();
    let e2 = direction.cross(e1);
    fan(radius)
        .into_iter()
        .map(|(x, y)| e1 * x + e2 * y - direction * RAY_LEAD_MM)
        .collect()
}

/// The visible radius of a traced bundle on the image plane: `√2 ×` the energy-weighted RMS
/// distance of the landings from their weighted centroid (the radius of a uniform disc with that
/// RMS). Rays the housing stopped carry no weight, so unlike the reference's bounding footprint
/// they do not widen the ghost. `None` when fewer than [`MIN_LIVE_RAYS`] land.
fn visible_radius(traced: &[lens_flare::TracedRay]) -> Option<f32> {
    if traced.iter().filter(|r| r.weight > 1.0e-6).count() < MIN_LIVE_RAYS {
        return None;
    }
    let energy: f32 = traced.iter().map(|r| r.weight).sum();
    if energy <= 0.0 || !energy.is_finite() {
        return None;
    }
    let centroid = traced.iter().map(|r| r.landing * r.weight).sum::<Vec3>() / energy;
    let msd = traced
        .iter()
        .map(|r| {
            let d = r.landing - centroid;
            r.weight * (d.x * d.x + d.y * d.y)
        })
        .sum::<f32>()
        / energy;
    Some((2.0 * msd).sqrt())
}

/// Energy-weighted mean landing of a traced bundle (the plain mean when nothing lands), and the
/// mean landed energy.
fn landing(traced: impl Iterator<Item = lens_flare::TracedRay>) -> (Vec3, f32) {
    let (mut weighted, mut plain, mut energy, mut count) = (Vec3::ZERO, Vec3::ZERO, 0.0f32, 0u32);
    for ray in traced {
        weighted += ray.landing * ray.weight;
        plain += ray.landing;
        energy += ray.weight;
        count += 1;
    }
    if count == 0 {
        return (Vec3::ZERO, 0.0);
    }
    let mean = if energy > 0.0 {
        weighted / energy
    } else {
        plain / count as f32
    };
    (mean, energy / count as f32)
}

/// Refractive index of the single-layer anti-reflection film of [`quarter_wave_reflectance`]
/// (magnesium fluoride).
pub const FILM_INDEX: f32 = 1.38;
/// The wavelengths, nanometres, the films of a multicoated lens are tuned to, surface by surface
/// in turn: a film reflects least at its own wavelength, so each ghost takes the colour of its
/// two surfaces' films (one thickness for every surface would make every ghost the same magenta).
pub const FILM_DESIGN_NM: [f32; 5] = [550.0, 470.0, 620.0, 510.0, 590.0];
/// The most ranked ghosts [`GhostTable::trace`] probes (a long lens has hundreds of pairs; the
/// dimmest cannot be among the visible few).
const MAX_PROBED_GHOSTS: usize = 64;

/// Normal-incidence reflectance of a quarter-wave single-layer film of [`FILM_INDEX`] (tuned to
/// `design_nm`) between media `n_outer` and `n_glass`, at `wavelength_nm`: the two-beam
/// thin-film formula `R = (a² + b² + 2ab cos 2δ) / (1 + a²b² + 2ab cos 2δ)` with the interface
/// amplitudes `a`, `b` and the film's phase `δ = (π / 2) · design / λ` (Hecht, *Optics*, § 9.7;
/// Macleod, *Thin-Film Optical Filters*, ch. 3).
#[must_use]
pub fn quarter_wave_reflectance(n_outer: f32, n_glass: f32, wavelength_nm: f32, design_nm: f32) -> f32 {
    let a = (n_outer - FILM_INDEX) / (n_outer + FILM_INDEX);
    let b = (FILM_INDEX - n_glass) / (FILM_INDEX + n_glass);
    let cos = (std::f32::consts::PI * design_nm / wavelength_nm.max(1.0)).cos();
    (a * a + b * b + 2.0 * a * b * cos) / (1.0 + a * a * b * b + 2.0 * a * b * cos)
}

/// The colour a coated bounce gives a ghost at `wavelength_nm`: the film's reflectance there over
/// its reflectance at the d line (so the reference's d-line energy is kept), for an air-to-glass
/// surface whose coating factor is below 1. Bare or cemented surfaces give 1. `dial` blends it in
/// like the reference's coating dial.
fn coating_tint(lens: &LensPrescription, surface: usize, wavelength_nm: f32, dial: f32) -> f32 {
    let Some(s) = lens.surfaces.get(surface) else {
        return 1.0;
    };
    let before = if surface == 0 {
        1.0
    } else {
        lens.surfaces[surface - 1].refractive_index_d
    };
    let after = s.refractive_index_d;
    let (outer, glass) = (before.min(after), before.max(after));
    if s.coating_factor >= 1.0 || outer > 1.0 + lens_flare::REFLECTING_INDEX_CHANGE || glass <= FILM_INDEX {
        return 1.0;
    }
    let design = FILM_DESIGN_NM[surface % FILM_DESIGN_NM.len()];
    let at_d = quarter_wave_reflectance(outer, glass, lens_flare::D_LINE_NM, design);
    if at_d <= 0.0 {
        return 1.0;
    }
    // Near its design wavelength a film reflects almost nothing, so this ratio can grow past what
    // the surface's coating factor leaves room for: a coated bounce must stay below bare glass.
    let bare_limit = 1.0 / s.coating_factor.max(1.0e-3);
    let ratio = (quarter_wave_reflectance(outer, glass, wavelength_nm, design) / at_d).min(bare_limit);
    let dial = if dial.is_finite() {
        dial.clamp(0.0, 1.0)
    } else {
        1.0
    };
    1.0 + dial * (ratio - 1.0)
}

impl GhostTable {
    /// Traces `lens` at `coating` and keeps its most visible ghosts: the ranked ghosts with the
    /// most traced energy per footprint area (`energy / radius²`, the ghost's peak brightness),
    /// at most [`MAX_GHOSTS`], half of them held for ghosts across the frame centre from the
    /// light. Coated air-to-glass bounces tint each ghost with a quarter-wave
    /// film's spectral reflectance ([`quarter_wave_reflectance`]).
    ///
    /// # Errors
    /// The lens fails [`LensPrescription::validate`].
    pub fn trace(lens: &LensPrescription, coating: f32) -> Result<Self, PrescriptionError> {
        let _span = tracing::info_span!("lens_flare_ghost_table").entered();
        let (ghosts, ranked) = probe_ghosts(lens, coating)?;
        Ok(Self {
            lens: lens.name.clone(),
            coating,
            ranked,
            ghosts: select_ghosts(ghosts),
        })
    }
}

/// The ghosts the pass draws, most visible first: the most visible of each side of the frame
/// centre. Half of [`MAX_GHOSTS`] is held for ghosts across the centre from the light (`m < 0`) as
/// long as there are any: the compact ghosts of most lenses lie on the light's side, and with
/// those alone a flare is a smudge around the light instead of a chain through the frame.
fn select_ghosts(mut ghosts: Vec<GhostEntry>) -> Vec<GhostEntry> {
    // Ties go to the lower pair, so the pick does not depend on the probe order.
    ghosts.sort_by(|a, b| {
        visibility(b)
            .total_cmp(&visibility(a))
            .then(a.first_bounce.cmp(&b.first_bounce))
            .then(a.second_bounce.cmp(&b.second_bounce))
    });
    let across = ghosts
        .iter()
        .filter(|g| g.magnification[1] < 0.0)
        .take(MAX_GHOSTS / 2)
        .copied();
    let held = across.clone().count();
    let same_side = ghosts
        .iter()
        .filter(|g| g.magnification[1] >= 0.0)
        .take(MAX_GHOSTS - held)
        .copied();
    let mut kept: Vec<GhostEntry> = across.chain(same_side).collect();
    kept.sort_by(|a, b| visibility(b).total_cmp(&visibility(a)));
    kept
}

/// Every ranked ghost of `lens` the probes could measure (at most [`MAX_PROBED_GHOSTS`], unsorted)
/// and how many the reference ranking found.
fn probe_ghosts(
    lens: &LensPrescription,
    coating: f32,
) -> Result<(Vec<GhostEntry>, usize), PrescriptionError> {
    let settings = TraceSettings { coating };
    let ranked = lens_flare::rank_ghosts(lens, &settings)?;
    let pupil = lens_flare::entrance_pupil_radius(lens);
    let width = lens.sensor_width_mm;
    let diagonal = lens_flare::sensor_diagonal(lens);
    let aspect = lens.sensor_width_mm / lens.sensor_height_mm;
    let probe = lens_flare::field_direction(lens, aspect, [MAGNIFICATION_PROBE_FIELD, 0.0]);
    let axis_rays = bundle(Vec3::Z, pupil);
    let probe_rays = bundle(probe, pupil);
    let mut ghosts = Vec::with_capacity(MAX_PROBED_GHOSTS);
    for ghost in ranked.iter().take(MAX_PROBED_GHOSTS) {
        let mut energy = [0.0f32; 3];
        let mut magnification = [0.0f32; 3];
        let mut radius_mm: Option<f32> = None;
        let mut usable = true;
        for (channel, &wavelength) in PROBE_WAVELENGTHS_NM.iter().enumerate() {
            let trace_ghost = |origin: Vec3, direction: Vec3| {
                lens_flare::trace_ghost(
                    lens,
                    &settings,
                    ghost.first_bounce,
                    ghost.second_bounce,
                    Ray::new(origin, direction, wavelength),
                )
                .ok()
            };
            let axis: Vec<lens_flare::TracedRay> = axis_rays
                .iter()
                .filter_map(|o| trace_ghost(*o, Vec3::Z))
                .collect();
            let field: Vec<lens_flare::TracedRay> =
                probe_rays.iter().filter_map(|o| trace_ghost(*o, probe)).collect();
            let (_, on_axis) = landing(axis.iter().copied());
            energy[channel] = on_axis
                * coating_tint(lens, ghost.first_bounce, wavelength, coating)
                * coating_tint(lens, ghost.second_bounce, wavelength, coating);
            // The visible size at the design wavelength: the larger of on axis and in the field.
            if channel == 1 {
                radius_mm = match (visible_radius(&axis), visible_radius(&field)) {
                    (Some(a), Some(b)) => Some(a.max(b)),
                    (a, b) => a.or(b),
                };
            }
            let (ghost_at, _) = landing(field.iter().copied());
            let (image_at, _) = landing(probe_rays.iter().filter_map(|o| {
                lens_flare::trace_sequential(lens, &settings, Ray::new(*o, probe, wavelength)).ok()
            }));
            let m = if image_at.x.abs() > 1.0e-6 {
                ghost_at.x / image_at.x
            } else {
                f32::NAN
            };
            if !m.is_finite() || m.abs() > MAX_GHOST_MAGNIFICATION {
                usable = false;
                break;
            }
            magnification[channel] = if m.abs() < MIN_GHOST_MAGNIFICATION {
                MIN_GHOST_MAGNIFICATION.copysign(if m == 0.0 { 1.0 } else { m })
            } else {
                m
            };
        }
        if !usable || energy.iter().any(|e| !e.is_finite()) {
            continue;
        }
        // The reference's bounding footprint stands in when no probe ray lands.
        let radius = if width > 0.0 {
            radius_mm.unwrap_or(ghost.size_fraction * diagonal) / width
        } else {
            0.0
        };
        ghosts.push(GhostEntry {
            first_bounce: ghost.first_bounce,
            second_bounce: ghost.second_bounce,
            energy,
            magnification,
            radius: if radius.is_finite() {
                radius.clamp(0.0, 2.0)
            } else {
                0.0
            },
        });
    }
    Ok((ghosts, ranked.len()))
}

/// A ghost's peak brightness for the selection: its mean traced energy over its footprint area
/// (radii below 2 % of the width count as 2 %).
fn visibility(ghost: &GhostEntry) -> f32 {
    let energy = ghost.energy.iter().sum::<f32>() / 3.0;
    let r = ghost.radius.max(0.02);
    energy / (r * r)
}

/// A synthetic surface (the values below are invented for Regia, not transcribed from a lens).
const fn surface(radius: f32, distance: f32, n: f32, abbe: f32, clear: f32, stop: bool) -> Surface {
    Surface {
        signed_radius_mm: radius,
        axial_distance_to_next_mm: distance,
        refractive_index_d: n,
        abbe_v_d: abbe,
        clear_semi_diameter_mm: clear,
        coating_factor: 0.35,
        is_stop: stop,
    }
}

/// The built-in lens of the ghost flare: a synthetic six-element lens of the symmetric
/// double-Gauss family (two outer positive groups, two inner negative menisci around a central
/// stop), full-frame (36 × 24 mm), at f/22; its solved focal length is about 140 mm.
///
/// Its curvatures, spacings and glasses are invented for Regia (no real lens is transcribed;
/// `docs/provenance.md`, ADR-1441 row). It was picked over a three-element layout, and stopped
/// down to f/22, because its ghosts are then compact on both sides of the centre (about 0.02–0.2
/// of the frame width; at f/8 those across the centre are veils of 0.4–0.8): they read as
/// distinct images of the iris (ADR-1863 look review). The back focus and
/// the effective focal length are not stored: they are solved with a paraxial trace, so the
/// sensor sits at the lens's own infinity focus and the ghosts' magnifications are measured on
/// the right plane. Every surface carries a single-layer coating factor of 0.35.
#[must_use]
pub fn default_flare_lens() -> LensPrescription {
    flare_lens_at(22.0)
}

/// [`default_flare_lens`] stopped to `f_number`: ghosts scale with the pupil, so a smaller stop
/// gives smaller, crisper ghosts. For a host's own table
/// ([`LensEffectsPass::with_ghost_table`]).
#[must_use]
pub fn flare_lens_at(f_number: f32) -> LensPrescription {
    focused("Regia synthetic 6-element", f_number, six_element_surfaces())
}

/// The surfaces of [`default_flare_lens`] (the last distance is replaced by the solved back focus).
fn six_element_surfaces() -> Vec<Surface> {
    vec![
        surface(38.0, 4.8, 1.70, 48.0, 17.0, false),
        surface(120.0, 0.4, 1.0, 0.0, 16.5, false),
        surface(26.0, 5.5, 1.66, 50.0, 14.0, false),
        surface(48.0, 1.2, 1.0, 0.0, 12.5, false),
        surface(70.0, 1.6, 1.62, 36.0, 12.0, false),
        surface(18.0, 6.0, 1.0, 0.0, 10.5, false),
        surface(0.0, 6.0, 1.0, 0.0, 9.5, true),
        surface(-17.0, 1.6, 1.62, 36.0, 10.0, false),
        surface(-60.0, 1.2, 1.0, 0.0, 11.5, false),
        surface(-40.0, 5.0, 1.69, 53.0, 12.5, false),
        surface(-26.0, 0.4, 1.0, 0.0, 13.5, false),
        surface(200.0, 4.0, 1.69, 53.0, 14.0, false),
        surface(-75.0, 38.0, 1.0, 0.0, 14.5, false),
    ]
}

/// The three-element layout the first look review rejected (its ghosts are 0.5–1.8 of the frame
/// width), kept for the look-tuning test.
#[cfg(test)]
fn three_element_lens(f_number: f32) -> LensPrescription {
    focused(
        "Regia synthetic 3-element",
        f_number,
        vec![
            surface(28.0, 4.5, 1.62, 58.0, 13.0, false),
            surface(140.0, 3.0, 1.0, 0.0, 12.0, false),
            surface(-55.0, 1.6, 1.61, 37.0, 10.0, false),
            surface(26.0, 3.5, 1.0, 0.0, 9.5, false),
            surface(0.0, 3.5, 1.0, 0.0, 8.0, true),
            surface(95.0, 4.0, 1.62, 58.0, 10.0, false),
            surface(-34.0, 40.0, 1.0, 0.0, 10.5, false),
        ],
    )
}

/// A full-frame lens of `surfaces` at `f_number` whose last distance (the back focus) and focal
/// length are solved with a paraxial trace: a ray 0.5 mm off the axis crosses it at the infinity
/// focus, and its exit slope gives the effective focal length. The stored values are kept when
/// the trace does not converge.
fn focused(name: &str, f_number: f32, mut surfaces: Vec<Surface>) -> LensPrescription {
    let mut lens = LensPrescription {
        name: name.into(),
        focal_length_mm: 50.0,
        native_f_number: f_number,
        sensor_width_mm: 36.0,
        sensor_height_mm: 24.0,
        provenance: "synthetic (invented for Regia, not transcribed from any lens)".into(),
        surfaces: surfaces.clone(),
    };
    const HEIGHT: f32 = 0.5;
    if let Ok(traced) = lens_flare::trace_sequential(
        &lens,
        &TraceSettings::default(),
        Ray::new(
            Vec3::new(HEIGHT, 0.0, -RAY_LEAD_MM),
            Vec3::Z,
            lens_flare::D_LINE_NM,
        ),
    ) && traced.direction.x < 0.0
        && traced.direction.z > 0.0
    {
        let slope = traced.direction.x / traced.direction.z;
        let crossing = traced.landing.z - traced.landing.x / slope;
        let last_vertex: f32 = surfaces[..surfaces.len() - 1]
            .iter()
            .map(|s| s.axial_distance_to_next_mm)
            .sum();
        let back_focus = crossing - last_vertex;
        let focal = -HEIGHT / slope;
        if back_focus.is_finite() && back_focus > 1.0 && focal.is_finite() && focal > 1.0 {
            if let Some(last) = surfaces.last_mut() {
                last.axial_distance_to_next_mm = back_focus;
            }
            lens.surfaces = surfaces;
            lens.focal_length_mm = focal;
        }
    }
    lens
}

/// Steps of the coating dial [`default_ghost_table`] traces: an animated dial traces each step
/// once instead of a new table per value on the frame thread.
const COATING_STEPS: f32 = 16.0;

/// The built-in lens's ghost table at `coating`, snapped to steps of 1/16 (each step is traced
/// once and kept).
///
/// # Errors
/// Never in practice: the built-in lens is valid (tested); the error is the trace's.
pub fn default_ghost_table(coating: f32) -> Result<std::sync::Arc<GhostTable>, PrescriptionError> {
    use std::sync::{Arc, Mutex, OnceLock};
    static LENS: OnceLock<LensPrescription> = OnceLock::new();
    static TABLES: Mutex<Vec<(u32, Arc<GhostTable>)>> = Mutex::new(Vec::new());
    let coating = if coating.is_finite() {
        coating.clamp(0.0, 1.0)
    } else {
        1.0
    };
    let step = (coating * COATING_STEPS).round() as u32;
    let mut tables = TABLES.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((_, table)) = tables.iter().find(|(k, _)| *k == step) {
        return Ok(Arc::clone(table));
    }
    let lens = LENS.get_or_init(default_flare_lens);
    let table = Arc::new(GhostTable::trace(lens, step as f32 / COATING_STEPS)?);
    gpu::lens_effects_counters()
        .tables_traced
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    tables.push((step, Arc::clone(&table)));
    Ok(table)
}

/// The `from` versions of the `lens-effects` migration steps (`0`: no `format_version`).
#[must_use]
pub fn lens_effects_migration_sources() -> Vec<u32> {
    vec![0]
}

/// The serialized shape of [`LensEffects`] for `cargo xtask check-formats` (entry
/// `lens-effects`): every reflected field (name and type), in declaration order.
#[must_use]
pub fn lens_effects_schema_fingerprint() -> String {
    let mut out = String::new();
    crate::post_volumes::struct_fields(&mut out, "LensEffects", LensEffects::type_info());
    out
}

/// Logs once per process that a lens effects component from a newer engine was ignored.
pub(crate) fn warn_newer_lens_effects_once(found: u32) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            target: "regia_render",
            "a LensEffects has format_version {found}, newer than this engine's \
             {LENS_EFFECTS_FORMAT_VERSION}; the camera renders without lens effects (logged once)"
        );
    }
}

/// Registers the reflected type a scene file needs ([`LensEffects`]); `RenderPlugin` does the same.
pub fn register_lens_effects_types(registry: &mut regia_ecs::reflect::TypeRegistry) {
    registry.register::<LensEffects>();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::RenderProfilePreset;

    /// Look tuning aid (not a check): prints the ghost tables of candidate synthetic lenses
    /// (`cargo test -p regia_render --lib explore_candidate_lenses -- --ignored --nocapture`).
    #[test]
    #[ignore = "prints candidate lens tables for the look review"]
    fn explore_candidate_lenses() {
        let gauss = |f: f32| focused("gauss", f, six_element_surfaces());
        for (name, lens) in [
            ("triplet f2.8", three_element_lens(2.8)),
            ("triplet f11", three_element_lens(11.0)),
            ("gauss f2", gauss(2.0)),
            ("gauss f4", gauss(4.0)),
            ("gauss f5.6", gauss(5.6)),
            ("gauss f8", gauss(8.0)),
            ("gauss f11", gauss(11.0)),
            ("gauss f16", gauss(16.0)),
            ("gauss f22", gauss(22.0)),
        ] {
            if let Ok((mut all, _)) = probe_ghosts(&lens, 1.0) {
                all.sort_by(|a, b| a.magnification[1].total_cmp(&b.magnification[1]));
                let rows: Vec<String> = all
                    .iter()
                    .map(|g| {
                        format!(
                            "m={:.2} ({},{}) e={:.2e} r={:.3} vis={:.3}",
                            g.magnification[1],
                            g.first_bounce,
                            g.second_bounce,
                            g.energy[1],
                            g.radius,
                            visibility(g)
                        )
                    })
                    .collect();
                println!("{name} all probed:\n  {}", rows.join("\n  "));
            }
            match GhostTable::trace(&lens, 1.0) {
                Ok(table) => {
                    let mut rows: Vec<String> = table
                        .ghosts
                        .iter()
                        .map(|g| {
                            format!(
                                "({},{}) e={:.2e} m={:.2} r={:.3}",
                                g.first_bounce, g.second_bounce, g.energy[1], g.magnification[1], g.radius
                            )
                        })
                        .collect();
                    rows.sort();
                    println!(
                        "{name}: f={:.1} mm, ranked {}, kept: {}",
                        lens.focal_length_mm,
                        table.ranked,
                        rows.join(" | ")
                    );
                }
                Err(err) => println!("{name}: {err}"),
            }
        }
    }

    #[test]
    fn the_table_holds_half_its_ghosts_for_the_far_side_of_the_centre() {
        let ghost = |pair: usize, m: f32, energy: f32| GhostEntry {
            first_bounce: pair,
            second_bounce: pair + 1,
            energy: [energy; 3],
            magnification: [m; 3],
            radius: 0.1,
        };
        // Twelve bright ghosts on the light's side, three dim ones across the centre.
        let mut ghosts: Vec<GhostEntry> = (0..12).map(|i| ghost(i, 1.2, 1.0 + i as f32)).collect();
        ghosts.extend((20..23).map(|i| ghost(i, -0.5, 0.01)));
        let kept = select_ghosts(ghosts.clone());
        assert_eq!(kept.len(), MAX_GHOSTS);
        let across = kept.iter().filter(|g| g.magnification[1] < 0.0).count();
        assert_eq!(across, 3, "every ghost across the centre stays while it fits");
        assert_eq!(kept[0].energy[0], 12.0, "most visible first");
        // More across the centre than half: the rest of the slots stay with the light's side.
        ghosts.extend((30..40).map(|i| ghost(i, -0.5, 0.02)));
        let kept = select_ghosts(ghosts);
        let across = kept.iter().filter(|g| g.magnification[1] < 0.0).count();
        assert_eq!(across, MAX_GHOSTS / 2);
        // No ghost across the centre: nothing is held.
        let one_side: Vec<GhostEntry> = (0..12).map(|i| ghost(i, 1.2, 1.0)).collect();
        assert_eq!(select_ghosts(one_side).len(), MAX_GHOSTS);
    }

    #[test]
    fn off_is_the_default_and_inactive() {
        assert_eq!(LensEffects::default(), LensEffects::OFF);
        assert!(!LensEffects::OFF.is_active());
        assert!(LensEffects::CINEMATIC.is_active());
        assert_eq!(PostSettings::default().lens_effects, LensEffects::OFF);
    }

    #[test]
    fn sanitizing_clamps_and_replaces_non_finite_values() {
        let wild = LensEffects {
            flare_intensity: f32::NAN,
            flare_threshold: -3.0,
            coating: 7.0,
            dirt_intensity: f32::INFINITY,
            chromatic_aberration: 1.0,
            starburst: f32::NEG_INFINITY,
            ..LensEffects::OFF
        }
        .sanitized();
        assert_eq!(wild.flare_intensity, 0.0);
        assert_eq!(wild.flare_threshold, 0.0);
        assert_eq!(wild.coating, 1.0);
        assert_eq!(wild.dirt_intensity, 0.0);
        assert_eq!(wild.chromatic_aberration, CHROMATIC_ABERRATION_RANGE.1);
        assert!(wild.chromatic_aberration_active() && !wild.flare_active() && !wild.dirt_active());
        assert_eq!(wild.starburst, 0.0);
        let loud = LensEffects {
            starburst: 99.0,
            ..LensEffects::OFF
        }
        .sanitized();
        assert_eq!(loud.starburst, STARBURST_RANGE.1);
        assert!(loud.starburst_active() && loud.is_active());
    }

    fn registry() -> regia_ecs::reflect::TypeRegistry {
        let mut registry = regia_ecs::reflect::TypeRegistry::default();
        register_lens_effects_types(&mut registry);
        registry
    }

    fn decode(raw: &str, registry: &regia_ecs::reflect::TypeRegistry) -> LensEffects {
        let registration = registry
            .get_with_type_path(LensEffects::type_path())
            .expect("registered");
        let value = regia_ecs::serialize::ReflectCodec::new(registry)
            .from_ron_typed(raw, registration)
            .expect("decodes");
        *value.downcast::<LensEffects>().expect("a LensEffects")
    }

    #[test]
    fn it_saves_in_scenes_and_reads_back_unchanged() {
        let registry = registry();
        assert_eq!(
            LensEffects::type_path(),
            "regia_render::lens_effects::LensEffects"
        );
        let effects = LensEffects {
            flare_intensity: 2.5,
            dirt_intensity: 0.4,
            starburst: 3.0,
            after_dof: true,
            ..LensEffects::CINEMATIC
        };
        let ron = regia_ecs::serialize::ReflectCodec::new(&registry)
            .to_ron_typed(&effects)
            .expect("encodes");
        assert_eq!(decode(&ron, &registry), effects);
        assert_eq!(effects.format_version, LENS_EFFECTS_FORMAT_VERSION);
    }

    #[test]
    fn a_hand_written_component_without_a_version_migrates_and_a_newer_one_is_refused() {
        let registry = registry();
        // Only the fields it needs: the rest take the off values, the version reads as 0.
        let old = decode("(format_version: 0, flare_intensity: 1.0)", &registry);
        assert_eq!(old.format_version, 0);
        assert_eq!((old.starburst, old.after_dof), (0.0, false));
        let migrated = old.migrate().expect("0 migrates");
        assert_eq!(migrated.format_version, LENS_EFFECTS_FORMAT_VERSION);
        assert_eq!(migrated.flare_intensity, 1.0);
        let newer = LensEffects {
            format_version: LENS_EFFECTS_FORMAT_VERSION + 1,
            ..LensEffects::OFF
        };
        assert!(!newer.is_supported());
        assert_eq!(
            newer.migrate(),
            Err(LensEffectsError::NewerVersion {
                found: LENS_EFFECTS_FORMAT_VERSION + 1
            })
        );
        assert_eq!(lens_effects_migration_sources(), vec![0]);
        let shape = lens_effects_schema_fingerprint();
        for field in ["format_version", "starburst", "after_dof", "chromatic_aberration"] {
            assert!(shape.contains(field), "fingerprint lacks {field}: {shape}");
        }
    }

    #[test]
    fn presets_allow_the_pass_on_desktop_high_and_ultra_only() {
        for preset in RenderProfilePreset::ALL {
            let profile = RenderProfile::from_preset(preset);
            let mut post = PostSettings::default();
            LensEffects::CINEMATIC.apply_camera(&mut post);
            gate_lens_effects(&mut post, &profile);
            let allowed = matches!(
                preset,
                RenderProfilePreset::DesktopHigh | RenderProfilePreset::DesktopUltra
            );
            assert_eq!(profile.lens_effects, allowed, "{preset:?}");
            assert_eq!(post.lens_effects.is_active(), allowed, "{preset:?}");
        }
        assert!(crate::cinematic::cinematic_profile().lens_effects);
    }

    #[test]
    fn the_built_in_lens_is_valid_focused_and_full_frame() {
        let lens = default_flare_lens();
        lens.validate().expect("valid built-in lens");
        assert!(
            (20.0..200.0).contains(&lens.focal_length_mm),
            "solved focal length {}",
            lens.focal_length_mm
        );
        // A paraxial on-axis ray lands on the axis at the solved back focus.
        let traced = lens_flare::trace_sequential(
            &lens,
            &TraceSettings::default(),
            Ray::new(Vec3::new(0.3, 0.0, -RAY_LEAD_MM), Vec3::Z, lens_flare::D_LINE_NM),
        )
        .expect("trace");
        assert!(traced.landing.x.abs() < 0.01, "focus miss {}", traced.landing.x);
        assert!(traced.weight > 0.5);
    }

    #[test]
    fn the_ghost_table_is_bounded_ordered_finite_and_deterministic() {
        let lens = default_flare_lens();
        let table = GhostTable::trace(&lens, 1.0).expect("table");
        assert!(!table.ghosts.is_empty(), "the built-in lens casts ghosts");
        assert!(table.ghosts.len() <= MAX_GHOSTS);
        assert!(table.ranked >= table.ghosts.len());
        for ghost in &table.ghosts {
            assert!(ghost.first_bounce < ghost.second_bounce);
            for c in 0..3 {
                assert!(ghost.energy[c].is_finite() && ghost.energy[c] >= 0.0 && ghost.energy[c] < 0.1);
                let m = ghost.magnification[c].abs();
                assert!(
                    (MIN_GHOST_MAGNIFICATION..=MAX_GHOST_MAGNIFICATION).contains(&m),
                    "{m}"
                );
            }
            assert!(ghost.radius.is_finite() && ghost.radius >= 0.0);
            // A uniform disc's √2·RMS is its radius; no distribution inside the reference's
            // bounding footprint can exceed it by more than √2.
            let bound = lens_flare::ghost_size_fraction(
                &lens,
                &TraceSettings { coating: 1.0 },
                ghost.first_bounce,
                ghost.second_bounce,
            )
            .expect("size")
                * lens_flare::sensor_diagonal(&lens)
                / lens.sensor_width_mm;
            assert!(
                ghost.radius <= bound * 1.4143 + 1.0e-5,
                "{} vs {bound}",
                ghost.radius
            );
        }
        // Most visible first.
        for pair in table.ghosts.windows(2) {
            assert!(visibility(&pair[0]) >= visibility(&pair[1]), "{pair:?}");
        }
        assert_eq!(GhostTable::trace(&lens, 1.0).expect("again"), table);
    }

    #[test]
    fn bare_glass_ghosts_are_brighter_than_coated_ones() {
        let lens = default_flare_lens();
        let (coated, _) = probe_ghosts(&lens, 1.0).expect("coated");
        let (bare, _) = probe_ghosts(&lens, 0.0).expect("bare");
        let total = |g: &GhostEntry| g.energy.iter().sum::<f32>();
        // Over the same ghosts: the two tables keep different ones, so their own sums do not
        // compare. Not ghost by ghost either: a coating also lets more light through the rest of
        // the lens, so one ghost can beat bare glass where its film reflects most.
        let (mut compared, mut coated_sum, mut bare_sum) = (0, 0.0, 0.0);
        for g in &coated {
            let same =
                |b: &&GhostEntry| (b.first_bounce, b.second_bounce) == (g.first_bounce, g.second_bounce);
            if let Some(b) = bare.iter().find(same) {
                coated_sum += total(g);
                bare_sum += total(b);
                compared += 1;
            }
        }
        assert!(compared > 8, "only {compared} ghosts in both");
        assert!(bare_sum > coated_sum * 2.0, "{bare_sum} vs {coated_sum}");
    }

    #[test]
    fn coated_bounces_tint_ghosts_and_bare_glass_does_not() {
        // A film tuned near the d line reflects least there: a ghost off it turns red and blue
        // against green (the familiar magenta), more red than blue at 550 nm.
        let n = 1.62;
        let green_film = FILM_DESIGN_NM[0];
        let r = |nm: f32| quarter_wave_reflectance(1.0, n, nm, green_film);
        assert!(r(lens_flare::D_LINE_NM) < r(lens_flare::F_LINE_NM));
        assert!(r(lens_flare::F_LINE_NM) < r(lens_flare::C_LINE_NM));
        // At its design wavelength a quarter-wave layer gives ((n_g - n_f²) / (n_g + n_f²))².
        let ideal = ((n - FILM_INDEX * FILM_INDEX) / (n + FILM_INDEX * FILM_INDEX)).powi(2);
        assert!(
            (r(green_film) - ideal).abs() < 1.0e-6,
            "{} vs {ideal}",
            r(green_film)
        );
        // Below bare glass everywhere in the visible.
        let bare = ((n - 1.0) / (n + 1.0)).powi(2);
        assert!([400.0, 500.0, 600.0, 700.0].iter().all(|nm| r(*nm) < bare));

        let lens = default_flare_lens();
        let coated = GhostTable::trace(&lens, 1.0).expect("coated");
        let bare_table = GhostTable::trace(&lens, 0.0).expect("bare");
        let red_over_green = |g: &GhostEntry| g.energy[0] / g.energy[1];
        let mut compared = 0;
        for g in &coated.ghosts {
            if let Some(b) = bare_table
                .ghosts
                .iter()
                .find(|b| (b.first_bounce, b.second_bounce) == (g.first_bounce, g.second_bounce))
            {
                assert!(
                    (red_over_green(g) - red_over_green(b)).abs() > 1.0e-3,
                    "the film left its colour: {g:?} vs {b:?}"
                );
                compared += 1;
            }
        }
        assert!(compared > 0, "the coated and bare tables share ghosts");
        // Each surface's film has its own tuning, so the ghosts do not all share one colour.
        let hues: Vec<f32> = coated.ghosts.iter().map(red_over_green).collect();
        let (low, high) = hues
            .iter()
            .fold((f32::MAX, 0.0f32), |(l, h), v| (l.min(*v), h.max(*v)));
        assert!(high > 1.25 * low, "one colour for every ghost: {hues:?}");
        // A bare surface (coating factor 1) is not tinted.
        let mut uncoated = lens.clone();
        for s in &mut uncoated.surfaces {
            s.coating_factor = 1.0;
        }
        assert_eq!(coating_tint(&uncoated, 0, lens_flare::C_LINE_NM, 1.0), 1.0);
        assert_eq!(coating_tint(&lens, 0, lens_flare::C_LINE_NM, 0.0), 1.0);
        assert!(coating_tint(&lens, 0, lens_flare::C_LINE_NM, 1.0) > 1.0);
        // At no wavelength does a coated surface reflect more than bare glass, even next to its
        // film's design wavelength, where the ratio to the d line is largest.
        for surface in 0..lens.surfaces.len() {
            let factor = lens.surfaces[surface].coating_factor;
            for nm in (400..=700).step_by(10) {
                let tint = coating_tint(&lens, surface, nm as f32, 1.0);
                assert!(
                    tint * factor <= 1.0 + 1.0e-5,
                    "surface {surface} at {nm} nm: {tint}"
                );
            }
        }
    }

    #[test]
    fn magnification_is_linear_in_the_field() {
        // The table's m is measured at one field; a ghost traced at twice that field lands at
        // about twice the distance (paraxial linearity, which the screen-space gather relies on).
        let lens = default_flare_lens();
        let table = GhostTable::trace(&lens, 1.0).expect("table");
        let ghost = table.ghosts[0];
        let settings = TraceSettings::default();
        let aspect = lens.sensor_width_mm / lens.sensor_height_mm;
        let pupil = lens_flare::entrance_pupil_radius(&lens);
        let at = |field: f32| {
            let dir = lens_flare::field_direction(&lens, aspect, [field, 0.0]);
            let rays = bundle(dir, pupil);
            let (g, _) = landing(rays.iter().filter_map(|o| {
                lens_flare::trace_ghost(
                    &lens,
                    &settings,
                    ghost.first_bounce,
                    ghost.second_bounce,
                    Ray::new(*o, dir, lens_flare::D_LINE_NM),
                )
                .ok()
            }));
            let (i, _) = landing(rays.iter().filter_map(|o| {
                lens_flare::trace_sequential(&lens, &settings, Ray::new(*o, dir, lens_flare::D_LINE_NM)).ok()
            }));
            g.x / i.x
        };
        let near = at(0.1);
        let far = at(0.4);
        assert!(
            (near - far).abs() <= 0.15 * near.abs().max(0.2),
            "m at 0.1: {near}, at 0.4: {far}"
        );
        assert!((near - ghost.magnification[1]).abs() <= 0.15 * near.abs().max(0.2));
    }

    #[test]
    fn the_default_table_is_cached_per_coating() {
        let a = default_ghost_table(1.0).expect("a");
        let b = default_ghost_table(1.0).expect("b");
        assert!(std::sync::Arc::ptr_eq(&a, &b));
        let c = default_ghost_table(0.5).expect("c");
        assert!((c.coating - 0.5).abs() < 1e-6);
        assert!(!std::sync::Arc::ptr_eq(&a, &c));
        // An animated dial cannot trace per value: nearby values share a step.
        let d = default_ghost_table(0.51).expect("d");
        assert!(std::sync::Arc::ptr_eq(&c, &d));
    }
}
