// SPDX-License-Identifier: MPL-2.0
//! CPU mirrors of the VFX quality predicates.
//!
//! The billboard shader in [`crate::gpu::shaders`] uses the same constants
//! ([`SORT_DEPTH_SCALE`], [`SORT_INDEX_BITS`], [`SCREEN_FLOOR_MAX_SCALE`], the smoothstep soft
//! fade, the flipbook cursor and the spherical-puff shade). Unit tests here are the contract;
//! the offscreen GPU tests check that the shader agrees.
//!
//! Provenance (see `docs/provenance.md`):
//! - Soft fade: Lorach, "Soft Particles" (NVIDIA, 2007), with a Hermite smoothstep.
//! - Lit puffs: Harris, "Real-Time Cloud Rendering for Games" (GDC 2002) — a camera-facing
//!   card shaded as a sphere, plus a top-lit hemispheric term.
//! - Stable sort: quantized view depth (Batcher network already sorts the key) with the
//!   particle slot as a tie-break so equal keys cannot swap.
//! - Screen floor: perspective size `world * (height/2) / (depth * tan(fov_y/2))`.

/// View-depth quantisation: one bin is 2 cm (`depth_metres * 50`).
pub const SORT_DEPTH_SCALE: f32 = 50.0;
/// Low bits of an alpha sort key reserved for the particle slot.
pub const SORT_INDEX_BITS: u32 = 12;
/// High bits of an alpha sort key reserved for the inverted depth bin.
pub const SORT_DEPTH_BITS: u32 = 20;
/// A particle may grow by at most this factor to meet the screen-size floor.
pub const SCREEN_FLOOR_MAX_SCALE: f32 = 16.0;
/// Below this saturation a colour counts as white / grey (no hue to police).
pub const FIRE_WHITE_SATURATION: f32 = 0.18;
/// Smallest exposure the particle shader divides by.
///
/// Daylight EV 15 is about `2.54e-5`. A floor of `1e-4` (about EV 13) made the shader divide by
/// a larger number than the post pass multiplies, so Desktop High particles arrived at AgX a
/// quarter as bright and left the fire family. `1e-8` is about EV 26, under any EV this engine
/// authors. The WGSL literal `1e-8` in [`crate::gpu::shaders`] must match.
pub const EXPOSURE_FLOOR: f32 = 1e-8;
/// Largest finite value an `Rgba16Float` target stores. The HDR inject clips here.
pub const HDR_F16_MAX: f32 = 65504.0;

const INDEX_MASK: u32 = (1 << SORT_INDEX_BITS) - 1;
const DEPTH_MAX: u32 = (1 << SORT_DEPTH_BITS) - 1;

/// Soft-particle fade in `0..=1` (1 = fully visible).
///
/// `scene_view_depth` and `particle_view_depth` are positive view-space depths. The fade is 0 on
/// the surface and 1 once the particle is `softness` metres in front of it. `softness <= 0` is the
/// hard intersection (a step), which the ground-crossing test rejects.
pub fn soft_particle_fade(scene_view_depth: f32, particle_view_depth: f32, softness: f32) -> f32 {
    let delta = scene_view_depth - particle_view_depth;
    if softness <= 1e-4 {
        return if delta > 0.0 { 1.0 } else { 0.0 };
    }
    let x = (delta / softness).clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

/// Spherical-billboard fade in `0..=1` (1 = fully visible): the camera-facing puff is a
/// sphere of `radius` metres centred `particle_view_depth` in front of the eye, and this
/// pixel (`local` in `-1..=1` across the quad) sees the chord `2 · radius · √(1 − |local|²)`.
/// The opacity is that of a dense medium along the part of the chord in front of the scene,
/// `1 − e^(−τd)` with τ = [`SPHERE_DENSITY`] per radius (Umenhoffer et al., GI 2006),
/// normalised so a free chord is opaque; near the rim the span floors at `softness` or a
/// quarter radius. The
/// front is clamped to the near plane, so a puff around the camera still shows. The result
/// is at least the flat [`soft_particle_fade`] at the centre depth with a softness of at
/// least the radius, so a bigger puff always meets the ground more gently. Mirrors the
/// billboard fragment shader.
pub fn spherical_billboard_fade(
    scene_view_depth: f32,
    particle_view_depth: f32,
    radius: f32,
    local: [f32; 2],
    softness: f32,
    near: f32,
) -> f32 {
    let r2 = local[0] * local[0] + local[1] * local[1];
    let half_chord = radius * (1.0 - r2).max(0.0).sqrt();
    let front = (particle_view_depth - half_chord).max(near);
    let back = particle_view_depth + half_chord;
    let span = (back - front).max(softness.max(0.25 * radius)).max(1e-4);
    let tau = SPHERE_DENSITY / radius.max(1e-4);
    let seen = (scene_view_depth - front).clamp(0.0, span);
    let sphere = (1.0 - (-tau * seen).exp()) / (1.0 - (-tau * span).exp()).max(1e-4);
    sphere.max(soft_particle_fade(
        scene_view_depth,
        particle_view_depth,
        softness.max(radius),
    ))
}

/// Black-body chromaticity stops `(kelvin, linear sRGB normalised to the largest channel)`:
/// Planck's law against the CIE 1931 2° matching functions (analytic fit of Wyman, Sloan &
/// Shirley, JCGT 2013), XYZ → linear sRGB (IEC 61966-2-1). The billboard shader's
/// `blackbody_rgb` interpolates the same table.
pub const BLACKBODY_STOPS: [(f32, [f32; 3]); 8] = [
    (900.0, [1.0, 0.0089, 0.0]),
    (1100.0, [1.0, 0.0503, 0.0]),
    (1300.0, [1.0, 0.0968, 0.0]),
    (1500.0, [1.0, 0.1452, 0.0]),
    (1800.0, [1.0, 0.2184, 0.0]),
    (2200.0, [1.0, 0.3131, 0.0281]),
    (2700.0, [1.0, 0.4231, 0.0996]),
    (3500.0, [1.0, 0.5768, 0.2605]),
];

/// Linear interpolation of [`BLACKBODY_STOPS`] at `kelvin` (clamped to 900–3500 K).
pub fn blackbody_rgb(kelvin: f32) -> [f32; 3] {
    let t = kelvin.clamp(BLACKBODY_STOPS[0].0, BLACKBODY_STOPS[7].0);
    for pair in BLACKBODY_STOPS.windows(2) {
        let ((t0, c0), (t1, c1)) = (pair[0], pair[1]);
        if t <= t1 {
            let f = (t - t0) / (t1 - t0);
            return [0, 1, 2].map(|i| c0[i] + (c1[i] - c0[i]) * f);
        }
    }
    BLACKBODY_STOPS[7].1
}

/// Optical depth of a spherical billboard per radius: the puff is opaque once a pixel sees
/// about a third of its radius of it ([`spherical_billboard_fade`]).
pub const SPHERE_DENSITY: f32 = 3.0;

/// Alpha-blend sort key. An ascending sort is back-to-front.
///
/// Depth is quantized to [`SORT_DEPTH_SCALE`] bins so two particles inside 2 cm keep a fixed order
/// (the slot in the low 12 bits). Crossing a bin boundary swaps them once; they do not flicker.
pub fn alpha_sort_key(view_depth: f32, particle_index: u32) -> u32 {
    let bin = ((view_depth.max(0.0) * SORT_DEPTH_SCALE) as u32).min(DEPTH_MAX);
    let inverted = DEPTH_MAX - bin;
    (inverted << SORT_INDEX_BITS) | (particle_index & INDEX_MASK)
}

/// Which atlas frames a particle shows, and how much to blend toward the next one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FlipbookCursor {
    /// Current frame in `0..frames`.
    pub frame: u32,
    /// Next frame (equals `frame` when blending is off).
    pub next: u32,
    /// Blend weight toward `next` in `0..=1`.
    pub blend: f32,
}

/// Flipbook cursor.
///
/// `fps > 0` advances by particle age. `fps <= 0` spreads the sheet across the lifetime
/// (`OverLife`). `phase` is the random start in `0..1`. Frames wrap.
pub fn flipbook_cursor(
    age: f32,
    lifetime: f32,
    frames: u32,
    fps: f32,
    phase: f32,
    blend_frames: bool,
) -> FlipbookCursor {
    if frames <= 1 {
        return FlipbookCursor {
            frame: 0,
            next: 0,
            blend: 0.0,
        };
    }
    let n = frames as f32;
    let phase = phase.clamp(0.0, 0.999);
    let cursor = if fps > 0.0 {
        age.max(0.0) * fps + phase * n
    } else {
        let t = if lifetime > 1e-5 {
            (age / lifetime).clamp(0.0, 1.0)
        } else {
            0.0
        };
        t * n + phase * n
    };
    let wrapped = cursor.rem_euclid(n);
    let frame = (wrapped.floor() as u32) % frames;
    let fract = (wrapped - frame as f32).clamp(0.0, 0.999);
    if blend_frames {
        FlipbookCursor {
            frame,
            next: (frame + 1) % frames,
            blend: fract,
        }
    } else {
        FlipbookCursor {
            frame,
            next: frame,
            blend: 0.0,
        }
    }
}

/// Projected size of a world-space quad in pixels (vertical FOV).
pub fn screen_size_px(world_size: f32, view_depth: f32, viewport_height: f32, fov_y_rad: f32) -> f32 {
    let depth = view_depth.max(1e-3);
    let tan_half = (fov_y_rad * 0.5).tan().max(1e-6);
    world_size.abs() * (viewport_height * 0.5) / (depth * tan_half)
}

/// Grows `world_size` so the quad covers at least `floor_px`, capped at [`SCREEN_FLOOR_MAX_SCALE`].
///
/// `floor_px <= 0` leaves the size alone (the profile default is applied by the caller).
pub fn apply_screen_floor(
    world_size: f32,
    view_depth: f32,
    viewport_height: f32,
    fov_y_rad: f32,
    floor_px: f32,
) -> f32 {
    if floor_px <= 0.0 || world_size == 0.0 {
        return world_size;
    }
    let px = screen_size_px(world_size, view_depth, viewport_height, fov_y_rad);
    if px >= floor_px || px <= 1e-6 {
        return world_size;
    }
    let grow = (floor_px / px).min(SCREEN_FLOOR_MAX_SCALE);
    world_size * grow
}

/// `true` when `rgb` is white/grey or sits in the orange / yellow / white family.
///
/// Rejects pink (green channel below the blue), green (red below green) and pure red (green
/// collapsed). Linear or display values both work; the test is a channel order, not a magnitude.
pub fn in_fire_family(rgb: [f32; 3]) -> bool {
    let [r, g, b] = rgb;
    let maxc = r.max(g).max(b).max(0.0);
    let minc = r.min(g).min(b);
    if maxc <= 1e-4 {
        return true;
    }
    let sat = (maxc - minc) / maxc;
    if sat < FIRE_WHITE_SATURATION {
        return true;
    }
    r >= g && g >= b * 0.85 && g >= r * 0.35
}

/// Hue-preserving display map. Values with a peak at most 1 pass through; brighter peaks scale
/// so the brightest channel lands on 1. Backdrop pixels (the harness sky) are unchanged.
pub fn tonemap_display(rgb: [f32; 3]) -> [f32; 3] {
    let peak = rgb[0].max(rgb[1]).max(rgb[2]).max(0.0);
    if peak <= 1.0 {
        return [rgb[0].max(0.0), rgb[1].max(0.0), rgb[2].max(0.0)];
    }
    let scale = 1.0 / peak;
    [rgb[0] * scale, rgb[1] * scale, rgb[2] * scale]
}

/// Divides a display-referred colour by the post-exposure multiplier so an HDR buffer that is
/// later multiplied by `exposure` still shows the authored value.
///
/// `exposure` is raised to [`EXPOSURE_FLOOR`] so a zero multiplier cannot overflow the target.
pub fn apply_exposure(rgb: [f32; 3], exposure: f32) -> [f32; 3] {
    let e = exposure.max(EXPOSURE_FLOOR);
    [rgb[0] / e, rgb[1] / e, rgb[2] / e]
}

/// Colour the post stack sees after the particle divide and the `Rgba16Float` store.
///
/// The shader writes [`apply_exposure`] into the pre-exposure HDR target. Channels above
/// [`HDR_F16_MAX`] clip, which pulls a hot core toward white. The post pass then multiplies
/// by the same clamped exposure (Desktop High: exposure, then bloom, then AgX).
pub fn restore_exposed(rgb: [f32; 3], exposure: f32) -> [f32; 3] {
    let e = exposure.max(EXPOSURE_FLOOR);
    let written = apply_exposure(rgb, e);
    [
        written[0].clamp(0.0, HDR_F16_MAX) * e,
        written[1].clamp(0.0, HDR_F16_MAX) * e,
        written[2].clamp(0.0, HDR_F16_MAX) * e,
    ]
}

/// View-space normal of a spherical billboard. `uv` is the quad corner in `-1..=1`.
pub fn spherical_normal(uv: [f32; 2]) -> [f32; 3] {
    let r2 = uv[0] * uv[0] + uv[1] * uv[1];
    let nz = (1.0 - r2).max(0.0).sqrt();
    [uv[0], uv[1], nz]
}

/// Lit-puff RGB multiplier (not including the albedo).
///
/// `sun_dir_view` points toward the sun in view space. Lambert `N·L` lights one side of the
/// sphere; `self_shadow` (0 = off, 1 = black at the bottom) applies Harris's top-lit gradient.
pub fn puff_shade(
    uv: [f32; 2],
    sun_dir_view: [f32; 3],
    sun_rgb: [f32; 3],
    ambient: f32,
    self_shadow: f32,
) -> [f32; 3] {
    let n = spherical_normal(uv);
    let len = (sun_dir_view[0] * sun_dir_view[0]
        + sun_dir_view[1] * sun_dir_view[1]
        + sun_dir_view[2] * sun_dir_view[2])
        .sqrt()
        .max(1e-6);
    let l = [
        sun_dir_view[0] / len,
        sun_dir_view[1] / len,
        sun_dir_view[2] / len,
    ];
    let ndotl = (n[0] * l[0] + n[1] * l[1] + n[2] * l[2]).max(0.0);
    let up = (n[1] * 0.5 + 0.5).clamp(0.0, 1.0);
    let shadow = self_shadow.clamp(0.0, 1.0);
    let hemi = (1.0 - shadow) + shadow * up;
    let amb = ambient.clamp(0.0, 1.0);
    let lambert = amb + (1.0 - amb) * ndotl;
    let shade = lambert * hemi;
    [sun_rgb[0] * shade, sun_rgb[1] * shade, sun_rgb[2] * shade]
}

/// One pixel of a lit puff under the sky (ADR-1425): the inputs of [`puff_shade_volumetric`],
/// the CPU mirror of the sky-lit branch of `fs_billboard`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VolumetricPuff {
    /// Position in the puff's disc, `-1..=1` on each axis, already turned by the sprite's
    /// rotation (view space x right, y up).
    pub uv: [f32; 2],
    /// The pixel's opacity (sprite alpha × particle alpha).
    pub opacity: f32,
    /// The particle's linear albedo (its colour).
    pub albedo: [f32; 3],
    /// The emitter's `self_shadow` (0..1): how dense the puff is toward the sun.
    pub self_shadow: f32,
    /// Direction toward the sun in view space (any length).
    pub sun_dir_view: [f32; 3],
    /// The sun as a white surface facing it shows it (display units, `light1`).
    pub sun_rgb: [f32; 3],
    /// World up in view space (unit).
    pub up_view: [f32; 3],
    /// Sky radiance from above, display units (`sky_up`).
    pub sky_up: [f32; 3],
    /// Light from the ground below, display units (`sky_down`).
    pub sky_down: [f32; 3],
}

/// Optical depth toward the sun of the puffs around a puff, in units of its own chord, at
/// `self_shadow` 1 (`NEIGHBOUR_DEPTH` in the billboard shader, ADR-1454). The emitter's
/// `self_shadow` is how packed its puffs are: a dense cap or column is many puffs deep toward
/// the sun, so the neighbours add `NEIGHBOUR_DEPTH × self_shadow²` chords of extinction (0.6
/// adds 2.2, 0.88 adds 4.6, 0.3 adds 0.54).
pub const NEIGHBOUR_DEPTH: f32 = 6.0;

/// How much of the light's chroma a bright aerosol adapts away, `CHROMA_ADAPT × albedo²`
/// (`CHROMA_ADAPT` in the billboard shader, ADR-1454).
pub const CHROMA_ADAPT: f32 = 1.5;

/// The sun's (or the ground's) colour as a puff of single-scattering `albedo` takes it: the
/// chroma is partly adapted toward the colour's luma by `CHROMA_ADAPT × albedo²` (clamped to
/// 0..=1). Snow powder under a low orange sun reads white-grey, not cream (the eye adapts to
/// the illuminant, and a bright scatterer is what it takes as white: von Kries 1902 adaptation
/// toward the illuminant); soot (albedo about 0.1) keeps the full hue.
#[must_use]
pub fn adapt_chroma(light: [f32; 3], albedo: f32) -> [f32; 3] {
    let k = (CHROMA_ADAPT * albedo * albedo).clamp(0.0, 1.0);
    let y = luma(light);
    light.map(|c| c + (y - c) * k)
}

/// Fraction of the sun that reaches a backlit puff of pixel opacity `opacity` through the puffs
/// behind it (`sun_through` in the billboard shader, ADR-1454): the neighbours' extinction
/// `NEIGHBOUR_DEPTH × self_shadow² × ½ × −ln(1 − α)` (half a chord at the puff's own density).
/// A thin fringe keeps its forward-scattered glow; a dense cap's body does not.
#[must_use]
pub fn neighbour_transmission(opacity: f32, self_shadow: f32) -> f32 {
    let s = self_shadow.clamp(0.0, 1.0);
    let a = opacity.clamp(0.0, 0.98);
    (-NEIGHBOUR_DEPTH * s * s * 0.5 * -(1.0 - a).ln()).exp()
}

/// Fraction of the sun's light that reaches the diffuse field `tau` optical depths into a
/// medium of single-scattering albedo `albedo`: the two-stream conservative transport
/// `1 / (1 + ¾ (1 − g) τ)` (Bohren 1987) times the decay of the diffusion regime of an
/// absorbing medium, `exp(−κ τ)` with `κ = √(3 (1 − ω) (1 − g ω))` (van de Hulst 1980, the
/// diffusion exponent). A bright dust (ω near 1) carries light deep in; a soot-grey cap (ω
/// about 0.4) lets almost none past its first optical depth, so the sun-lit rind is warm and
/// the core is lit by the sky only.
#[must_use]
pub fn diffuse_transmission(tau: f32, albedo: f32) -> f32 {
    let w = albedo.clamp(0.0, 1.0);
    let kappa = (3.0 * (1.0 - w) * (1.0 - SMOKE_HG_G * w)).max(0.0).sqrt();
    (-kappa * tau).exp() / (1.0 + 0.75 * (1.0 - SMOKE_HG_G) * tau)
}

/// Lit-puff light multiplier under the sun and the sky (not including the albedo), ADR-1425.
///
/// The puff is a sphere of participating medium: its opacity at this pixel gives the extinction
/// (`−ln(1 − α)` over the view chord `2 n_z` radii), and the sun reaches the point under the
/// pixel through the chord toward the sun, so the sun side keeps a bright rind and the core and
/// far side go dark. Transmission blends single scattering `exp(−τ)` toward the two-stream
/// diffuse `1 / (1 + ¾ (1 − g) τ)` (Bohren 1987) by the albedo: bright dust carries light deep
/// in, soot does not. The sky lights it from above and the ground from below by the world-up
/// facing of the sphere's normal, dimmed inside by up to half the opacity.
#[must_use]
pub fn puff_shade_volumetric(p: &VolumetricPuff) -> [f32; 3] {
    let len2 = p.uv[0] * p.uv[0] + p.uv[1] * p.uv[1];
    let k = if len2 > 0.98 { 0.99 / len2.sqrt() } else { 1.0 };
    let s = [p.uv[0] * k, p.uv[1] * k, 0.0];
    let n = spherical_normal([s[0], s[1]]);
    let a = p.opacity.clamp(0.0, 0.98);
    let sigma = -(1.0 - a).ln() / (2.0 * n[2]).max(0.1);
    let len = (p.sun_dir_view[0] * p.sun_dir_view[0]
        + p.sun_dir_view[1] * p.sun_dir_view[1]
        + p.sun_dir_view[2] * p.sun_dir_view[2])
        .sqrt()
        .max(1e-6);
    let l = p.sun_dir_view.map(|c| c / len);
    let b = s[0] * l[0] + s[1] * l[1];
    let c = s[0] * s[0] + s[1] * s[1] - 1.0;
    let chord = -b + (b * b - c).max(0.0).sqrt();
    let shadow = p.self_shadow.clamp(0.0, 1.0);
    let tau = sigma * chord * (0.5 + NEIGHBOUR_DEPTH * shadow * shadow);
    let single = (-tau).exp();
    let albedo = luma(p.albedo).clamp(0.0, 1.0);
    let diffuse = diffuse_transmission(tau, albedo);
    let t = single + (diffuse - single) * albedo;
    let sun = adapt_chroma(p.sun_rgb, albedo);
    let down = adapt_chroma(p.sky_down, albedo);
    let up = ((n[0] * p.up_view[0] + n[1] * p.up_view[1] + n[2] * p.up_view[2]) * 0.5 + 0.5).clamp(0.0, 1.0);
    let inner = 1.0 - 0.5 * a * shadow;
    std::array::from_fn(|i| sun[i] * t + (down[i] + (p.sky_up[i] - down[i]) * up) * inner)
}

/// Henyey–Greenstein asymmetry of smoke and dust aerosols the lit puffs use (`SMOKE_HG_G` in
/// the billboard shader).
pub const SMOKE_HG_G: f32 = 0.6;

/// Henyey–Greenstein phase function (Henyey & Greenstein, ApJ 1941), multiplied by 4π so an
/// isotropic medium is 1. `cos_t` is the cosine between the light's travel direction and the
/// view ray (1 = looking straight toward the light); `g` is the asymmetry (0 isotropic,
/// toward 1 forward). Lit puffs add `sun × k × phase × (1 − density)` so a backlit fringe
/// glows; the shader's `SMOKE_HG_G` is the `g` it uses.
#[must_use]
pub fn henyey_greenstein_4pi(cos_t: f32, g: f32) -> f32 {
    let g = g.clamp(-0.99, 0.99);
    let d = (1.0 + g * g - 2.0 * g * cos_t.clamp(-1.0, 1.0)).max(1e-6);
    (1.0 - g * g) / (d * d.sqrt())
}

/// Rec.709 luma.
pub fn luma(rgb: [f32; 3]) -> f32 {
    0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2]
}

/// Desktop High display of one already-restored scene colour.
///
/// Mirrors the post stack after the exposure round-trip ([`restore_exposed`]): `post.wgsl`
/// mode 3 `mix(base, bloom, bloom_intensity)` at [`regia_render::DEFAULT_BLOOM_INTENSITY`]
/// (0.04), [`regia_render::tonemap_agx`] (inset, log sigmoid, outset — the outset mixes
/// channels), then [`regia_render::display_look`] at the default contrast and saturation.
/// This is not an additive `base + 0.15 * blur`, and AgX is not a per-channel polynomial on
/// linear RGB. A colour that has already clipped red and green to the same f16 ceiling can
/// leave [`in_fire_family`] here; keep albedo × emissive under that ceiling.
#[must_use]
pub fn desktop_high_display(scene: [f32; 3], bloom: [f32; 3]) -> [f32; 3] {
    let w = regia_render::DEFAULT_BLOOM_INTENSITY;
    let mixed = [
        scene[0] * (1.0 - w) + bloom[0] * w,
        scene[1] * (1.0 - w) + bloom[1] * w,
        scene[2] * (1.0 - w) + bloom[2] * w,
    ];
    regia_render::display_look(
        regia_render::tonemap_agx(mixed),
        regia_render::DEFAULT_LOOK_CONTRAST,
        regia_render::DEFAULT_LOOK_SATURATION,
    )
}

/// The scene-wide significance caps of a quality tier (ADR-3275 (5)): live instances that spawn,
/// how many of them at the full rate, particles spawned per update, live particles, and off-screen
/// pausing with a short bounded warm-up. Design values sized to each tier's particle pool, not
/// measurements; a host applies them with
/// [`crate::scene::VfxScene::significance_mut`]`.set_caps(..)`. The default caps set none.
#[must_use]
pub fn significance_caps(tier: regia_render::QualityTier) -> crate::significance::SignificanceCaps {
    use regia_render::QualityTier;
    let (instances, full, spawns, live, warm_particles) = match tier {
        QualityTier::MobileLow => (16, 6, 256, 4_000, 16),
        QualityTier::MobileHigh => (32, 12, 512, 10_000, 24),
        QualityTier::Deck => (48, 16, 1_024, 20_000, 32),
        QualityTier::Low | QualityTier::VeryLow => (32, 12, 512, 20_000, 32),
        QualityTier::Medium => (96, 32, 2_048, 60_000, 64),
        QualityTier::High => (160, 64, 4_096, 100_000, 128),
        QualityTier::Ultra => (256, 128, 8_192, 100_000, 256),
    };
    crate::significance::SignificanceCaps {
        max_instances: Some(instances),
        full_instances: Some(full),
        max_spawns_per_frame: Some(spawns),
        max_live: Some(live),
        pause_offscreen: true,
        warmup_s: 0.25,
        warmup_max_particles: warm_particles,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn blackbody_cools_from_white_yellow_to_deep_red() {
        let hot = blackbody_rgb(3500.0);
        let cool = blackbody_rgb(1000.0);
        assert!(hot[1] > 0.5 && hot[2] > 0.2, "{hot:?}");
        assert!(cool[1] < 0.05 && cool[2] == 0.0, "{cool:?}");
        // Green rises with temperature, every channel within 0..=1.
        let mut last = 0.0;
        for k in (900..=3500).step_by(100) {
            let c = blackbody_rgb(k as f32);
            assert!(
                c[1] >= last && c.iter().all(|v| (0.0..=1.0).contains(v)),
                "{k} K: {c:?}"
            );
            last = c[1];
        }
    }

    #[test]
    fn spherical_fade_spans_the_chord_and_floors_at_the_rim() {
        let dense = |seen: f32, span: f32, radius: f32| {
            let tau = SPHERE_DENSITY / radius;
            (1.0 - (-tau * seen).exp()) / (1.0 - (-tau * span).exp())
        };
        // Centre pixel of a 2 m-radius puff 10 m away: front 8 m, back 12 m.
        let at =
            |scene: f32, local: [f32; 2]| super::spherical_billboard_fade(scene, 10.0, 2.0, local, 0.0, 0.1);
        assert!(
            at(7.9, [0.0, 0.0]) == 0.0,
            "scene in front of the sphere hides it"
        );
        assert!(
            (at(10.0, [0.0, 0.0]) - dense(2.0, 4.0, 2.0)).abs() < 1e-5,
            "half the chord"
        );
        assert!(
            at(10.0, [0.0, 0.0]) > 0.9,
            "a dense puff is nearly opaque with half its chord"
        );
        assert!(
            (at(12.5, [0.0, 0.0]) - 1.0).abs() < 1e-5,
            "scene behind shows all of it"
        );
        // At the rim the chord vanishes; the span floors at a quarter radius (0.5 m).
        assert!((at(10.25, [1.0, 0.0]) - dense(0.25, 0.5, 2.0)).abs() < 1e-5);
        // Never below the flat fade with the radius as its softness.
        let flat = super::soft_particle_fade(11.0, 10.0, 2.0);
        assert!(at(11.0, [1.0, 0.0]) >= flat - 1e-6);
        // A bigger puff fades over a longer depth: the same 0.4 m in front of its front.
        let small = super::spherical_billboard_fade(8.9, 10.0, 1.5, [0.0, 0.0], 0.0, 0.1);
        let big = super::spherical_billboard_fade(6.4, 10.0, 4.0, [0.0, 0.0], 0.0, 0.1);
        assert!(big < small, "{big} vs {small}");
        // The camera inside the puff: the front clamps to the near plane.
        assert!(super::spherical_billboard_fade(3.0, 1.0, 2.0, [0.0, 0.0], 0.0, 0.1) > 0.0);
    }

    use super::*;

    #[test]
    fn soft_fade_is_a_ramp_and_zero_softness_is_a_step() {
        let soft = soft_particle_fade(5.0, 4.0, 2.0);
        // Halfway through the band: smoothstep(0.5) = 0.5.
        assert!((soft - 0.5).abs() < 1e-5, "halfway fade {soft}");
        assert!((soft_particle_fade(5.0, 2.0, 2.0) - 1.0).abs() < 1e-5);
        assert!(soft_particle_fade(5.0, 5.0, 2.0) < 1e-5);
        // A hard clip has no mid values. The GPU test rejects this shape.
        assert_eq!(soft_particle_fade(5.0, 4.9, 0.0), 1.0);
        assert_eq!(soft_particle_fade(5.0, 5.1, 0.0), 0.0);
        let mid = soft_particle_fade(5.0, 4.9, 0.0);
        assert!(mid < 0.05 || mid > 0.95, "hard clip must not sit in the ramp");
    }

    #[test]
    fn alpha_sort_is_back_to_front_and_stable_inside_a_bin() {
        let far = alpha_sort_key(20.0, 3);
        let near = alpha_sort_key(2.0, 9);
        assert!(far < near, "ascending sort draws the far particle first");
        // 1 cm apart shares a 2 cm bin, so the higher slot stays on top.
        let a = alpha_sort_key(10.00, 4);
        let b = alpha_sort_key(10.01, 8);
        assert_eq!(a >> SORT_INDEX_BITS, b >> SORT_INDEX_BITS);
        assert!(a < b);
        // Crossing into the next bin swaps once.
        let before = alpha_sort_key(10.00, 1);
        let after = alpha_sort_key(10.03, 1);
        assert!(before > after, "moving away lowers the key");
    }

    #[test]
    fn flipbook_random_start_and_blend() {
        let a = flipbook_cursor(0.0, 1.0, 8, 0.0, 0.0, true);
        let b = flipbook_cursor(0.0, 1.0, 8, 0.0, 0.5, true);
        assert_eq!(a.frame, 0);
        assert_eq!(b.frame, 4, "phase 0.5 starts halfway through the sheet");
        let mid = flipbook_cursor(0.5, 1.0, 8, 8.0, 0.0, true);
        // age 0.5 * 8 fps = frame 4.0 exactly, blend ~ 0.
        assert_eq!(mid.frame, 4);
        assert!(mid.blend < 1e-4);
        let blended = flipbook_cursor(0.5625, 1.0, 8, 8.0, 0.0, true);
        // 0.5625 * 8 = 4.5
        assert_eq!(blended.frame, 4);
        assert_eq!(blended.next, 5);
        assert!((blended.blend - 0.5).abs() < 1e-4);
        let hard = flipbook_cursor(0.5625, 1.0, 8, 8.0, 0.0, false);
        assert_eq!(hard.next, hard.frame);
        assert_eq!(hard.blend, 0.0);
    }

    #[test]
    fn screen_floor_lifts_a_distant_spark_to_two_pixels() {
        let fov = 60.0_f32.to_radians();
        let raw = screen_size_px(0.05, 250.0, 1080.0, fov);
        assert!(
            raw < 1.0,
            "0.05 m at 250 m is sub-pixel ({raw} px), the shimmer case"
        );
        let grown = apply_screen_floor(0.05, 250.0, 1080.0, fov, 2.0);
        let px = screen_size_px(grown, 250.0, 1080.0, fov);
        assert!(px >= 1.99, "floor must reach 2 px, got {px}");
        // Already-large quads are left alone.
        let big = apply_screen_floor(12.0, 69.0, 1080.0, fov, 2.0);
        assert!((big - 12.0).abs() < 1e-3);
        // The cap stops a 1 mm mote from becoming a billboard.
        let capped = apply_screen_floor(0.001, 250.0, 1080.0, fov, 2.0);
        assert!((capped - 0.001 * SCREEN_FLOOR_MAX_SCALE).abs() < 1e-4);
    }

    #[test]
    fn fire_family_accepts_orange_and_rejects_pink_green_red() {
        assert!(in_fire_family([1.0, 0.95, 0.85]), "white-hot");
        assert!(in_fire_family([1.0, 0.85, 0.2]), "yellow");
        assert!(in_fire_family([1.0, 0.45, 0.05]), "orange");
        assert!(!in_fire_family([1.0, 0.2, 0.8]), "pink");
        assert!(!in_fire_family([0.2, 1.0, 0.3]), "green");
        assert!(!in_fire_family([1.0, 0.02, 0.0]), "pure red");
        // Tonemap keeps an HDR orange core in the family and pins the peak at 1.
        let mapped = tonemap_display([6.0, 4.5, 1.2]);
        assert!((mapped[0] - 1.0).abs() < 1e-5);
        assert!(in_fire_family(mapped), "mapped {mapped:?}");
        // Identity at or below 1, so an 8-bit backdrop is unchanged.
        let sky = [0.40, 0.50, 0.70];
        assert_eq!(tonemap_display(sky), sky);
        // AgX (inset, log sigmoid, outset) must not repair a pink core. An orange that
        // stays under the f16 ceiling keeps its channel order; one that clips red and
        // green to the same value can leave the family, so the gallery ramp does not.
        let orange = regia_render::tonemap_agx([1.56, 0.96, 0.18]);
        assert!(in_fire_family(orange), "AgX orange {orange:?}");
        let shown = desktop_high_display([1.56, 0.96, 0.18], [1.40, 0.86, 0.16]);
        assert!(in_fire_family(shown), "Desktop High orange {shown:?}");
        let pink = regia_render::tonemap_agx([6.0, 0.4, 4.5]);
        assert!(!in_fire_family(pink), "AgX must not repair pink: {pink:?}");
        let pink_shown = desktop_high_display([1.66, 0.38, 1.66], [1.50, 0.34, 1.50]);
        assert!(
            !in_fire_family(pink_shown),
            "Desktop High repaired pink: {pink_shown:?}"
        );
    }

    #[test]
    fn daylight_exposure_round_trip_keeps_orange_and_rejects_pink() {
        let ev15 = regia_render::exposure_from_ev100(15.0);
        assert!(
            ev15 < 1e-4 && ev15 > EXPOSURE_FLOOR,
            "EV 15 exposure {ev15} must pass the floor and sit under the old 1e-4 clamp"
        );
        // Two channels above the f16 ceiling collapse to the same value. AgX's outset
        // can then put green above red. The gallery ramp stays under that ceiling.
        let clipped = restore_exposed([6.0, 3.2, 0.6], ev15);
        assert!(
            (clipped[0] - clipped[1]).abs() < 1e-3,
            "hot orange should clip red and green together: {clipped:?}"
        );
        // Red may clip. Green must stay under the ceiling or it collapses onto red.
        let hot = [4.2, 1.55, 0.28];
        assert!(in_fire_family(hot), "hot core left the family before the clip");
        let hot_back = restore_exposed(hot, ev15);
        assert!(
            in_fire_family(regia_render::tonemap_agx(hot_back)),
            "AgX of a red-clipped core left the family: restored {hot_back:?}"
        );
        assert!(
            in_fire_family(desktop_high_display(
                hot_back,
                [hot_back[0] * 0.85, hot_back[1] * 0.85, hot_back[2] * 0.85]
            )),
            "Desktop High of a red-clipped core left the family"
        );
        let orange = [1.56, 0.96, 0.18];
        let back = restore_exposed(orange, ev15);
        assert!(
            in_fire_family(regia_render::tonemap_agx(back)),
            "AgX of the f16 round-trip left the family: {back:?}"
        );
        assert!(
            in_fire_family(desktop_high_display(back, [1.40, 0.86, 0.16])),
            "Desktop High of the round-trip left the family"
        );
        // A dim orange does not hit the f16 clip, so the post multiply restores it.
        let mild = [0.40, 0.25, 0.05];
        let mild_back = restore_exposed(mild, ev15);
        for (got, want) in mild_back.iter().zip(mild) {
            assert!((got - want).abs() < 1e-3, "restored {mild_back:?}");
        }
        let pink = [6.0, 0.4, 4.5];
        let pink_back = restore_exposed(pink, ev15);
        assert!(
            !in_fire_family(pink_back),
            "f16 clip repaired pink: {pink_back:?}"
        );
        assert!(
            !in_fire_family(regia_render::tonemap_agx(pink_back)),
            "AgX repaired the clipped pink core: {pink_back:?}"
        );
        assert!(
            crate::gpu::shaders::BILLBOARD.contains("max(frame.sim.w, 1e-8)"),
            "billboard exposure floor drifted from EXPOSURE_FLOOR"
        );
        assert!(
            crate::gpu::shaders::MESH_PARTICLE.contains("max(frame.sim.w, 1e-8)"),
            "mesh exposure floor drifted from EXPOSURE_FLOOR"
        );
    }

    #[test]
    fn henyey_greenstein_is_normalised_and_forward() {
        // ∫ p dΩ / 4π = ½ ∫ p d(cos) over −1..1 must be 1 for any g.
        for g in [0.0f32, 0.3, 0.6, 0.85] {
            let n = 4000;
            let mut sum = 0.0f32;
            for i in 0..n {
                let c = -1.0 + (i as f32 + 0.5) * 2.0 / n as f32;
                sum += henyey_greenstein_4pi(c, g) * 2.0 / n as f32;
            }
            assert!((sum * 0.5 - 1.0).abs() < 0.01, "g {g}: {}", sum * 0.5);
        }
        assert!((henyey_greenstein_4pi(0.3, 0.0) - 1.0).abs() < 1e-6);
        // Smoke (g 0.6): toward the sun far brighter than away from it.
        assert!(henyey_greenstein_4pi(0.95, 0.6) > 5.0);
        assert!(henyey_greenstein_4pi(-0.9, 0.6) < 0.2);
    }

    #[test]
    fn lit_puff_is_brighter_on_the_sun_side() {
        // Sun toward -X in view space: the left side of the disc faces it.
        let sun = [-1.0, 0.15, 0.2];
        let sun_rgb = [1.05, 0.98, 0.85];
        let left = puff_shade([-0.65, 0.0], sun, sun_rgb, 0.35, 0.65);
        let right = puff_shade([0.65, 0.0], sun, sun_rgb, 0.35, 0.65);
        let top = puff_shade([0.0, 0.65], sun, sun_rgb, 0.35, 0.65);
        let bottom = puff_shade([0.0, -0.65], sun, sun_rgb, 0.35, 0.65);
        assert!(
            luma(left) > luma(right) * 1.15,
            "lit {left:?} should beat shadow {right:?}"
        );
        assert!(luma(top) > luma(bottom), "top-lit gradient");
        // Equal sun from the camera (no x component) does not invent a left/right gap from albedo.
        let head_on_l = puff_shade([-0.4, 0.0], [0.0, 0.0, 1.0], [1.0, 1.0, 1.0], 0.4, 0.0);
        let head_on_r = puff_shade([0.4, 0.0], [0.0, 0.0, 1.0], [1.0, 1.0, 1.0], 0.4, 0.0);
        assert!((luma(head_on_l) - luma(head_on_r)).abs() < 1e-4);
    }

    fn snow_puff(uv: [f32; 2], opacity: f32, albedo: [f32; 3]) -> VolumetricPuff {
        // A 21° sun from the camera's left, slightly behind; warm sun, blue sky, grey ground.
        VolumetricPuff {
            uv,
            opacity,
            albedo,
            self_shadow: 0.85,
            sun_dir_view: [-0.93, 0.36, -0.1],
            sun_rgb: [1.0, 0.86, 0.68],
            up_view: [0.0, 1.0, 0.0],
            sky_up: [0.16, 0.22, 0.34],
            sky_down: [0.08, 0.08, 0.08],
        }
    }

    /// Dense smoke side-lit by a low sun: a bright rind on the sun side, a dark core, and the
    /// far side never black (the sky fills it).
    #[test]
    fn volumetric_puff_has_a_sunlit_rind_a_dark_core_and_sky_in_shadow() {
        let soot = [0.08, 0.075, 0.07];
        let rind = puff_shade_volumetric(&snow_puff([-0.85, 0.1], 0.5, soot));
        let core = puff_shade_volumetric(&snow_puff([0.0, 0.0], 0.95, soot));
        let far = puff_shade_volumetric(&snow_puff([0.85, -0.1], 0.5, soot));
        assert!(luma(rind) > 2.0 * luma(core), "rind {rind:?} core {core:?}");
        assert!(luma(far) < 0.5 * luma(rind), "far {far:?} rind {rind:?}");
        let sky = luma([0.16, 0.22, 0.34]);
        assert!(luma(far) > 0.3 * sky, "the sky fills the far side: {far:?}");
        // Bright dust carries light deeper than soot.
        let dust_core = puff_shade_volumetric(&snow_puff([0.0, 0.0], 0.95, [0.85, 0.87, 0.9]));
        assert!(luma(dust_core) > luma(core));
    }

    /// ADR-1454: the cap of a crash column (self-shadow 0.88, a grey albedo) is soot-grey with a
    /// lit rim: its colour comes from the optical depth toward the sun, not a tint. The core,
    /// many puffs deep, is lit by the sky and is not warmer than it; only the thin sun-side rind
    /// takes the warm sun.
    #[test]
    fn a_dense_cap_is_grey_in_the_core_and_warm_only_on_the_rind() {
        let grey = [0.45, 0.45, 0.45];
        let cap = |uv: [f32; 2], opacity: f32| {
            puff_shade_volumetric(&VolumetricPuff {
                self_shadow: 0.88,
                ..snow_puff(uv, opacity, grey)
            })
        };
        let warmth = |c: [f32; 3]| c[0] / c[2].max(1e-6);
        let rind = cap([-0.85, 0.1], 0.5);
        let core = cap([0.0, 0.0], 0.8);
        let sky = [0.16f32, 0.22, 0.34];
        assert!(luma(rind) > 1.5 * luma(core), "rind {rind:?} core {core:?}");
        // The core is the sky's colour (blue over red), not the sun's (red over blue).
        assert!(warmth(core) < 0.95, "core {core:?}");
        assert!(
            warmth(core) < warmth(sky) * 2.2,
            "core {core:?} stays close to the sky's hue"
        );
        assert!(warmth(rind) > warmth(core), "the rind is the warm part");
    }

    /// ADR-1454: snow dust under the warm low sun is white-grey; soot keeps its hue.
    #[test]
    fn bright_dust_loses_the_sun_chroma_and_soot_keeps_it() {
        let sun = [0.637, 0.473, 0.3];
        let warmth = |c: [f32; 3]| c[0] / c[2];
        let snow = adapt_chroma(sun, 0.7);
        let soot = adapt_chroma(sun, 0.1);
        // Luma is kept, the chroma is not.
        assert!((luma(snow) - luma(sun)).abs() < 1.0e-5);
        assert!(warmth(sun) > 2.0);
        assert!(warmth(snow) < 1.5, "{snow:?}");
        assert!(warmth(soot) > 0.97 * warmth(sun), "{soot:?}");
        // A lit snow puff in the rig of the look rounds (warm sun, blue sky, warm ground) is
        // within 15 % of neutral red over blue on its sunlit side; unadapted it is not.
        let puff = |albedo: [f32; 3]| {
            puff_shade_volumetric(&VolumetricPuff {
                sun_rgb: sun,
                sky_up: [0.0763, 0.0973, 0.1353],
                sky_down: [0.0924, 0.0808, 0.0733],
                self_shadow: 0.6,
                ..snow_puff([-0.5, 0.2], 0.4, albedo)
            })
        };
        let dust = puff([0.7, 0.72, 0.73]);
        assert!(warmth(dust) < 1.2, "{dust:?}");
    }

    #[test]
    fn a_backlit_fringe_glows_and_a_dense_body_does_not() {
        // The thin fringe of an open puff keeps the glow.
        assert!(neighbour_transmission(0.05, 0.88) > 0.85);
        // A dense cap puff (0.88, opacity 0.8) lets a few percent through, a sparse spray more.
        let cap = neighbour_transmission(0.8, 0.88);
        assert!(cap > 0.01 && cap < 0.05, "{cap}");
        assert!(neighbour_transmission(0.8, 0.3) > 0.5);
        // Monotone in opacity.
        let mut last = 1.0;
        for i in 0..20 {
            let t = neighbour_transmission(i as f32 * 0.05, 0.88);
            assert!(t <= last);
            last = t;
        }
        // The forward glow of a dense cap body drops below the sky's own light.
        let glow = 0.3 * henyey_greenstein_4pi(0.8, SMOKE_HG_G) * 0.1 * cap;
        assert!(glow < 0.1, "{glow}");
    }

    #[test]
    fn the_diffuse_transmission_falls_with_depth_and_with_absorption() {
        // A white medium keeps the two-stream conservative value.
        for tau in [0.0f32, 0.5, 2.0, 6.0] {
            let two_stream = 1.0 / (1.0 + 0.75 * (1.0 - SMOKE_HG_G) * tau);
            assert!((diffuse_transmission(tau, 1.0) - two_stream).abs() < 1.0e-6);
        }
        assert_eq!(diffuse_transmission(0.0, 0.3), 1.0);
        let mut last = 1.0;
        for i in 1..40 {
            let t = diffuse_transmission(i as f32 * 0.25, 0.45);
            assert!(t < last);
            last = t;
        }
        // Soot carries less light into its core than dust at every depth.
        for tau in [0.5f32, 1.0, 3.0, 8.0] {
            assert!(diffuse_transmission(tau, 0.35) < diffuse_transmission(tau, 0.85));
        }
        // At 3 optical depths a grey cap passes under 10 % to its core.
        assert!(diffuse_transmission(3.0, 0.45) < 0.1);
    }

    /// Snow powder under a warm low sun and a blue sky reads near neutral; the old sun-only
    /// lighting turned it tan (look round 1).
    #[test]
    fn white_powder_is_not_tinted_by_the_sun_alone() {
        let white = [0.85, 0.87, 0.9];
        let p = snow_puff([-0.3, 0.3], 0.5, white);
        let new = puff_shade_volumetric(&p);
        let old = puff_shade([-0.3, 0.3], p.sun_dir_view, p.sun_rgb, 0.5, 0.85);
        let warmth = |c: [f32; 3]| c[0] / c[2].max(1e-6);
        assert!(warmth(new) < warmth(old), "new {new:?} old {old:?}");
        assert!(warmth(new) < 1.3, "{new:?}");
        // No sun and no sky: no light.
        let dark = VolumetricPuff {
            sun_rgb: [0.0; 3],
            sky_up: [0.0; 3],
            sky_down: [0.0; 3],
            ..p
        };
        assert_eq!(puff_shade_volumetric(&dark), [0.0; 3]);
    }

    /// Every tier caps the ranking, cheaper tiers no higher than dearer ones of their family, and
    /// the live cap never exceeds the tier's particle pool.
    #[test]
    fn significance_caps_grow_with_the_tier() {
        use regia_render::QualityTier;
        let caps = super::significance_caps;
        for tier in QualityTier::ALL {
            let c = caps(tier);
            assert!(c.is_active(), "{tier:?}");
            assert!(c.full_instances <= c.max_instances);
            assert!(c.pause_offscreen && c.warmup_s > 0.0);
        }
        for family in [
            &[QualityTier::MobileLow, QualityTier::MobileHigh][..],
            &[
                QualityTier::Low,
                QualityTier::Medium,
                QualityTier::High,
                QualityTier::Ultra,
            ],
        ] {
            for pair in family.windows(2) {
                let (a, b) = (caps(pair[0]), caps(pair[1]));
                assert!(
                    a.max_instances <= b.max_instances && a.max_live <= b.max_live,
                    "{pair:?}"
                );
            }
        }
        let mobile = crate::gpu::VfxProfile::mobile_high().particle_capacity;
        assert!(caps(QualityTier::MobileHigh).max_live <= Some(mobile));
        let desktop = crate::gpu::VfxProfile::desktop_high().particle_capacity;
        assert!(caps(QualityTier::Ultra).max_live <= Some(desktop));
    }
}
