// SPDX-License-Identifier: MPL-2.0
//! Weather on the frame (spec 99 §§ 3.2, 5.6, 6; ADR-1847): the surface state a frame shades with,
//! the per-material response, the CPU mirror of the shading rule, and the types of the
//! precipitation occlusion view. The rule itself runs in `forward_surface_weather.wgsl` (and the
//! terrain's `@surface-weather` block) and sits behind a uniform branch: a dry frame skips it.
//!
//! A water film darkens a rough surface through total internal reflection (Lekner and Dorf 1988;
//! Jensen et al. 1999) and smooths its specular response; the split between porous darkening and
//! the film on sealed surfaces follows Lagarde 2012. Snow settles on what faces up (Fearing 2000;
//! Ohlsson and Seipel 2004) with a fresh-snow albedo (Warren and Wiscombe 1980). The constants and
//! the ripple formulation are Regia's own.

/// The surface rule's WGSL without its SPDX and import lines: the text `regia_terrain` holds verbatim in
/// `terrain.wgsl` between the `@surface-weather` markers.
#[must_use]
pub fn surface_weather_wgsl() -> &'static str {
    crate::shader_lib::library_snippet_body(include_str!("../shaders/forward_surface_weather.wgsl"))
}

/// The surface state a frame shades with, extracted from
/// `regia_scene::weather::SurfaceWeather`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SurfaceWeatherFrame {
    /// The water film, `0..1`.
    pub wetness: f32,
    /// How full the hollows are, `0..1`.
    pub puddles: f32,
    /// Snow cover, `0..1`.
    pub snow_cover: f32,
    /// Rain rate over the project's maximum, `0..1`.
    pub ripple: f32,
    /// The weather clock (s), for the ripples.
    pub clock_s: f32,
}

impl SurfaceWeatherFrame {
    /// No weather on surfaces.
    pub const DRY: Self = Self {
        wetness: 0.0,
        puddles: 0.0,
        snow_cover: 0.0,
        ripple: 0.0,
        clock_s: 0.0,
    };

    /// `true` when the frame shades as if there were no weather.
    #[must_use]
    pub fn is_dry(&self) -> bool {
        self.wetness <= 0.0 && self.puddles <= 0.0 && self.snow_cover <= 0.0
    }

    /// The frame uniform's `weather` row.
    #[must_use]
    pub fn row(&self) -> [f32; 4] {
        [self.wetness, self.puddles, self.snow_cover, self.ripple]
    }
}

/// How one material takes the weather.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WeatherResponse {
    /// Porosity `0..1`; `None` derives it from roughness and metallic.
    pub porosity: Option<f32>,
    /// Scales the wetness, `0..1`.
    pub wet: f32,
    /// Scales the snow, `0..1`.
    pub snow: f32,
    /// Flat parts of this material may pool.
    pub puddles: bool,
}

impl WeatherResponse {
    /// Derived porosity, full wetness and snow, no puddles.
    pub const DEFAULT: Self = Self {
        porosity: None,
        wet: 1.0,
        snow: 1.0,
        puddles: false,
    };
    /// The weather does not touch this material.
    pub const NONE: Self = Self {
        porosity: None,
        wet: 0.0,
        snow: 0.0,
        puddles: false,
    };

    /// Packs the response into the bits above the two flags of the material's `alpha.w`. The
    /// default packs to 0, so a material that never heard of weather keeps its bytes.
    #[must_use]
    pub fn packed_bits(&self) -> u32 {
        let four = |v: f32| ((1.0 - v.clamp(0.0, 1.0)) * 15.0).round() as u32;
        let porosity = self
            .porosity
            .map_or(0, |p| 1 + (p.clamp(0.0, 1.0) * 254.0).round() as u32);
        (four(self.wet) << 2) | (four(self.snow) << 6) | (u32::from(self.puddles) << 10) | (porosity << 11)
    }
}

impl Default for WeatherResponse {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// [`WeatherResponse`] for one surface point: the porosity decided.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResolvedWeatherResponse {
    /// Porosity, `0..1`.
    pub porosity: f32,
    /// Scales the wetness.
    pub wet: f32,
    /// Scales the snow.
    pub snow: f32,
    /// May pool.
    pub puddles: bool,
}

impl ResolvedWeatherResponse {
    /// The response of a surface of `roughness` and `metallic`.
    #[must_use]
    pub fn of(response: &WeatherResponse, roughness: f32, metallic: f32) -> Self {
        Self {
            porosity: response
                .porosity
                .unwrap_or_else(|| derived_porosity(roughness, metallic)),
            wet: response.wet,
            snow: response.snow,
            puddles: response.puddles,
        }
    }
}

/// The default porosity: concrete, soil and rough stone come out porous, polished, painted and
/// metal surfaces do not.
#[must_use]
pub fn derived_porosity(roughness: f32, metallic: f32) -> f32 {
    (1.0 - metallic.clamp(0.0, 1.0)) * ((roughness - 0.25) / 0.5).clamp(0.0, 1.0)
}

/// One surface point after the surface stage and before lighting.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SurfaceSample {
    /// Linear albedo.
    pub albedo: [f32; 3],
    /// Perceptual roughness.
    pub roughness: f32,
    /// Metallic.
    pub metallic: f32,
    /// Shading normal, unit.
    pub normal: [f32; 3],
    /// Geometric normal, unit.
    pub geometric_normal: [f32; 3],
    /// World position (m).
    pub position: [f32; 3],
}

fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn mix(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

fn normalize(v: [f32; 3]) -> [f32; 3] {
    let length = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    if length > 1.0e-6 {
        [v[0] / length, v[1] / length, v[2] / length]
    } else {
        [0.0, 1.0, 0.0]
    }
}

fn mix3(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [mix(a[0], b[0], t), mix(a[1], b[1], t), mix(a[2], b[2], t)]
}

/// The bits of `hash_step` in `forward_surface_weather.wgsl`.
fn hash_step(state: u32) -> u32 {
    let word = state.wrapping_mul(747_796_405).wrapping_add(2_891_336_453);
    let mixed = ((word >> ((word >> 28) + 4)) ^ word).wrapping_mul(277_803_737);
    (mixed >> 22) ^ mixed
}

fn cell_hash(x: i32, y: i32, salt: u32) -> [f32; 3] {
    let a = hash_step((x as u32) ^ hash_step((y as u32) ^ salt));
    let b = hash_step(a);
    let c = hash_step(b);
    let unit = |v: u32| (v >> 8) as f32 / 16_777_216.0;
    [unit(a), unit(b), unit(c)]
}

/// Value noise on `p`, `0..1`.
#[must_use]
pub fn value_noise(p: [f32; 2]) -> f32 {
    let cell = [p[0].floor(), p[1].floor()];
    let f = [p[0] - cell[0], p[1] - cell[1]];
    let u = [f[0] * f[0] * (3.0 - 2.0 * f[0]), f[1] * f[1] * (3.0 - 2.0 * f[1])];
    let (ix, iy) = (cell[0] as i32, cell[1] as i32);
    let at = |dx: i32, dy: i32| cell_hash(ix.wrapping_add(dx), iy.wrapping_add(dy), 0)[0];
    mix(mix(at(0, 0), at(1, 0), u[0]), mix(at(0, 1), at(1, 1), u[0]), u[1])
}

/// The puddle key of a world point, `0..1`: noise at 1.5 m and 6 m, spread so its share above a
/// threshold follows the threshold. The lowest spots of the key's field fill first.
#[must_use]
pub fn puddle_key(xz: [f32; 2]) -> f32 {
    let fine = value_noise([xz[0] / 1.5, xz[1] / 1.5]);
    let broad = value_noise([xz[0] / 6.0, xz[1] / 6.0]);
    smoothstep(0.2, 0.8, 0.35 * fine + 0.65 * broad)
}

/// Ripple cell size (m).
const RIPPLE_CELL_M: f32 = 0.4;
/// Crest spacing of a ring (m).
const RIPPLE_WAVELENGTH_M: f32 = 0.06;
/// A ring's height (m), which sets the slope of its normal.
const RIPPLE_HEIGHT_M: f32 = 0.0012;

/// The ripple's height gradient on X and Z at a world point, at `clock_s` under a rain share
/// `rate`. Two offset grids of 0.4 m cells, four cells each, one expanding ring per active cell.
/// Fixed bounds, an analytic gradient, no derivatives.
#[must_use]
pub fn ripple_gradient(xz: [f32; 2], clock_s: f32, rate: f32) -> [f32; 2] {
    let mut gradient = [0.0_f32; 2];
    for grid in 0..2_u32 {
        let offset = grid as f32 * 0.37;
        let p = [
            xz[0] / RIPPLE_CELL_M + offset,
            xz[1] / RIPPLE_CELL_M + offset * 1.7,
        ];
        let base = [(p[0] - 0.5).floor() as i32, (p[1] - 0.5).floor() as i32];
        for j in 0..2_i32 {
            for i in 0..2_i32 {
                let cell = [base[0].wrapping_add(i), base[1].wrapping_add(j)];
                let first = cell_hash(cell[0], cell[1], grid + 1);
                let period = 0.5 + 0.4 * first[0];
                let t = clock_s / period + first[1];
                let cycle = t.floor();
                let age = t - cycle;
                let second = cell_hash(
                    cell[0],
                    cell[1],
                    (cycle as i32 as u32).wrapping_mul(31).wrapping_add(grid),
                );
                if second[2] >= rate {
                    continue;
                }
                let center = [
                    cell[0] as f32 + 0.25 + 0.5 * second[0],
                    cell[1] as f32 + 0.25 + 0.5 * second[1],
                ];
                let delta = [
                    (p[0] - center[0]) * RIPPLE_CELL_M,
                    (p[1] - center[1]) * RIPPLE_CELL_M,
                ];
                let distance = delta[0].hypot(delta[1]);
                if distance < 1.0e-5 {
                    continue;
                }
                let radius = 0.15 * age;
                let x = distance - radius;
                let window = 1.0 - smoothstep(0.0, 0.05, x.abs());
                let fade = (1.0 - age) * (1.0 - age);
                let phase = std::f32::consts::TAU * x / RIPPLE_WAVELENGTH_M;
                let slope = RIPPLE_HEIGHT_M * std::f32::consts::TAU / RIPPLE_WAVELENGTH_M
                    * phase.cos()
                    * fade
                    * window;
                gradient[0] += slope * delta[0] / distance;
                gradient[1] += slope * delta[1] / distance;
            }
        }
    }
    gradient
}

/// The CPU mirror of the shading rule (§ 6.3): `input` under `weather` at an exposure to the sky
/// of `exposure` (1 open, 0 covered). Zero weather, or zero exposure, returns `input` bit for bit.
#[must_use]
pub fn apply_surface_weather(
    input: SurfaceSample,
    response: &ResolvedWeatherResponse,
    weather: &SurfaceWeatherFrame,
    exposure: f32,
) -> SurfaceSample {
    let x = exposure.clamp(0.0, 1.0);
    if weather.is_dry() || x <= 0.0 {
        return input;
    }
    let phi = response.porosity.clamp(0.0, 1.0);
    let n_geo = input.geometric_normal;
    let mut out = input;
    // Wetness: porous darkening, a film that smooths the specular and calms the normal map.
    let wet = (weather.wetness * x * response.wet).clamp(0.0, 1.0);
    let darken = 1.0 - 0.5 * phi * wet;
    out.albedo = out.albedo.map(|c| c * darken);
    let film = wet * (1.0 - phi) + 0.6 * phi * smoothstep(0.7, 1.0, wet);
    out.roughness = mix(out.roughness, 0.08, 0.9 * film);
    out.normal = normalize(mix3(out.normal, n_geo, 0.5 * film));
    // Snow, decided first: it suppresses the puddles under it.
    let cover = (weather.snow_cover * x * response.snow).clamp(0.0, 1.0);
    let lumps = value_noise([input.position[0] / 0.7, input.position[2] / 0.7]) - 0.5;
    let snow = cover * smoothstep(0.5, 0.85, n_geo[1] + 0.15 * lumps);
    // Puddles: flat ground only, the lowest spots of the key first, at most about 60 % of it.
    if response.puddles && n_geo[1] > 0.97 {
        let threshold = 1.0 - 0.6 * (weather.puddles * x).clamp(0.0, 1.0);
        let key = puddle_key([input.position[0], input.position[2]]);
        let pool = smoothstep(threshold, threshold + 0.05, key) * (1.0 - snow);
        if pool > 0.0 {
            let gradient = ripple_gradient(
                [input.position[0], input.position[2]],
                weather.clock_s,
                weather.ripple,
            );
            let surface = normalize([-gradient[0], 1.0, -gradient[1]]);
            let dark = 1.0 - 0.5 * phi;
            out.albedo = mix3(out.albedo, input.albedo.map(|c| c * dark), pool);
            out.roughness = mix(out.roughness, 0.02, pool);
            out.metallic = mix(out.metallic, 0.0, pool);
            out.normal = normalize(mix3(out.normal, surface, pool));
        }
    }
    if snow > 0.0 {
        let bent = normalize([n_geo[0], n_geo[1] + 0.5, n_geo[2]]);
        out.normal = normalize(mix3(mix3(out.normal, n_geo, snow), bent, 0.5 * snow));
        out.albedo = mix3(out.albedo, [0.82; 3], snow);
        out.roughness = mix(out.roughness, 0.8, snow);
        out.metallic = mix(out.metallic, 0.0, snow);
    }
    out
}

/// The renderer's fall direction for the occlusion view: unit, pointing down.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PrecipOcclusionRequest {
    /// Mean fall direction.
    pub fall_dir: [f32; 3],
}

/// A layer's wish for the occlusion view aimed along the frame's sun
/// ([`crate::TranslucentLayer::sun_occlusion_request`]): its size, since the view may be drawn with
/// the weather's budget off. The frame aims it at the sun it draws and binds it through
/// [`crate::TranslucentLayer::bind_sun_occlusion`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SunOcclusionRequest {
    /// Tile edge (texels).
    pub texels: u32,
    /// Side of the square it covers (m).
    pub extent_m: f32,
}

/// What the renderer reports about the occlusion view.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PrecipOcclusionInfo {
    /// Tile edge.
    pub size_texels: u32,
    /// Side of the square it covers (m).
    pub extent_m: f32,
    /// The snapped center on X and Z (m).
    pub origin: [f64; 2],
    /// The quantized tilt of the view from straight down (degrees).
    pub tilt_deg: f32,
    /// Times the tile was drawn.
    pub redraws: u64,
    /// The frame of the last redraw.
    pub last_redraw_frame: u64,
    /// The tile's index in the shadow tile table.
    pub tile: u32,
}

/// The occlusion view as a layer binds it.
#[derive(Clone, Copy, Debug)]
pub struct PrecipOcclusionBinding<'a> {
    /// The shadow atlas.
    pub atlas: &'a wgpu::TextureView,
    /// World to the tile's clip space.
    pub view_proj: [[f32; 4]; 4],
    /// Scale `xy` and offset `zw` of the tile in the atlas.
    pub uv_rect: [f32; 4],
    /// Side of the square it covers (m).
    pub extent_m: f32,
    /// Atlas edge (texels).
    pub atlas_texels: f32,
    /// Tile edge (texels).
    pub tile_texels: f32,
}

/// The occlusion tile read back for the agent protocol.
#[derive(Clone, Debug, PartialEq)]
pub struct PrecipOcclusionMap {
    /// The frame it was copied on.
    pub frame: u64,
    /// Tile edge (texels).
    pub size: u32,
    /// Depth, row by row.
    pub depth: Vec<f32>,
    /// World to the tile's clip space.
    pub view_proj: [[f32; 4]; 4],
    /// Compare depth per meter (the view's `1 / depth extent`).
    pub depth_per_m: f32,
    /// One texel, m.
    pub texel_m: f32,
}

impl PrecipOcclusionMap {
    /// How open `point` is, `0..1`, and the height of the first occluder above it (`None` under
    /// open sky): the CPU mirror of `precip_exposure`, without the geometric normal. A tap
    /// outside the tile counts as open and the clip edge is inclusive, as `shadow_pcf_rect`.
    #[must_use]
    pub fn exposure_at(&self, point: [f32; 3]) -> (f32, Option<f32>) {
        let vp = regia_core::glam::Mat4::from_cols_array_2d(&self.view_proj);
        let clip = vp * regia_core::glam::Vec3::from_array(point).extend(1.0);
        let ndc = clip.truncate() / clip.w;
        let local = [ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5];
        if !(0.0..=1.0).contains(&local[0]) || !(0.0..=1.0).contains(&local[1]) {
            return (1.0, None);
        }
        let size = self.size as i32;
        // A tap that leaves the tile is open (it would read a neighbour tile's depths); a tap
        // inside reads the stored depth, or the far plane when the map is short a texel.
        let at = |x: i32, y: i32| -> Option<f32> {
            if x < 0 || y < 0 || x >= size || y >= size {
                return None;
            }
            let index = y as usize * self.size as usize + x as usize;
            Some(self.depth.get(index).copied().unwrap_or(1.0))
        };
        // UV 1 is the last texel (the clip edge is inclusive, as in `precip_exposure`).
        let (x, y) = (
            ((local[0] * self.size as f32) as i32).min(size - 1),
            ((local[1] * self.size as f32) as i32).min(size - 1),
        );
        // A receiver below the view's far plane compares as the far plane: open, unless covered.
        let compare = (ndc.z - 0.02 * self.depth_per_m).min(1.0);
        let mut open = 0;
        for dy in -1..=1 {
            for dx in -1..=1 {
                open += match at(x + dx, y + dy) {
                    None => 1,
                    Some(stored) => i32::from(compare <= stored),
                };
            }
        }
        let stored = at(x, y).unwrap_or(1.0);
        let cover = (stored < 1.0 && stored < compare).then(|| {
            let world = vp.inverse() * regia_core::glam::Vec4::new(ndc.x, ndc.y, stored, 1.0);
            world.y / world.w
        });
        (open as f32 / 9.0, cover)
    }
}

/// The occlusion view as a shared pass reads it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PrecipOcclusionView {
    /// World to the tile's clip space.
    pub view_proj: [[f32; 4]; 4],
    /// Scale `xy` and offset `zw` of the tile in the atlas.
    pub uv_rect: [f32; 4],
    /// Side of the square it covers (m).
    pub extent_m: f32,
    /// One texel, m.
    pub texel_m: f32,
    /// Compare depth per meter (the view's `1 / depth extent`).
    pub depth_per_m: f32,
    /// Atlas edge (texels).
    pub atlas_texels: f32,
}

/// Frames a caster stays "moving" for the occlusion view's key after its bounds or pose last
/// changed (half a second at 60 Hz): a parked vehicle settles back to its exact shape.
pub(crate) const PRECIP_SETTLE_FRAMES: u64 = 30;

/// Frames between sweeps of the casters the occlusion view has not seen lately.
const PRECIP_TRACK_SWEEP_FRAMES: u64 = 256;

/// Which casters over the occlusion view moved lately (spec 99 § 5.6, ADR-2585).
///
/// A caster whose exact bounds and pose changed within [`PRECIP_SETTLE_FRAMES`] joins the tile's
/// key by its bounds snapped to the view's snap, so a vehicle driving under the eye redraws the
/// tile about once per snap instead of every frame. A caster seen for the first time, or still
/// for longer, keys by its exact bounds and pose (an edited crate redraws the tile at once).
#[derive(Default)]
pub(crate) struct PrecipCasterMotion {
    /// Per caster id: the exact hash, the frame it last changed (`None`: never seen changing) and
    /// the frame it was last seen over the view.
    tracks: std::collections::HashMap<u64, (u64, Option<u64>, u64)>,
}

impl PrecipCasterMotion {
    /// Notes `exact` (the hash of the caster's bounds and pose) for `id` in `frame`; returns
    /// whether the caster counts as moving.
    pub(crate) fn moving(&mut self, id: u64, exact: u64, frame: u64) -> bool {
        let track = self.tracks.entry(id).or_insert((exact, None, frame));
        if track.0 != exact {
            track.0 = exact;
            track.1 = Some(frame);
        }
        track.2 = frame;
        track
            .1
            .is_some_and(|changed| frame.saturating_sub(changed) < PRECIP_SETTLE_FRAMES)
    }

    /// A moving caster's words in the view's key: its center cell and half size in cells of
    /// `cell` metres per axis (`+ 0.0` folds -0 into 0).
    pub(crate) fn cell_words(min: [f32; 3], max: [f32; 3], cell: f32) -> [u64; 6] {
        let mut words = [0; 6];
        for (axis, pair) in words.as_chunks_mut::<2>().0.iter_mut().enumerate() {
            let center = (0.5 * (min[axis] + max[axis]) / cell).floor() + 0.0;
            let half = (0.5 * (max[axis] - min[axis]) / cell).ceil() + 0.0;
            *pair = [u64::from(center.to_bits()), u64::from(half.to_bits())];
        }
        words
    }

    /// Forgets casters not seen over the view for a while (no allocation; every
    /// `PRECIP_TRACK_SWEEP_FRAMES` frames).
    pub(crate) fn sweep(&mut self, frame: u64) {
        if frame.is_multiple_of(PRECIP_TRACK_SWEEP_FRAMES) {
            self.tracks
                .retain(|_, track| track.2.saturating_add(PRECIP_TRACK_SWEEP_FRAMES) >= frame);
        }
    }

    /// Casters tracked (tests).
    #[cfg(test)]
    pub(crate) fn tracked(&self) -> usize {
        self.tracks.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn concrete() -> SurfaceSample {
        SurfaceSample {
            albedo: [0.5, 0.48, 0.45],
            roughness: 0.9,
            metallic: 0.0,
            normal: normalize([0.1, 1.0, 0.05]),
            geometric_normal: [0.0, 1.0, 0.0],
            position: [3.3, 0.0, -7.7],
        }
    }

    fn resolved(sample: &SurfaceSample, response: &WeatherResponse) -> ResolvedWeatherResponse {
        ResolvedWeatherResponse::of(response, sample.roughness, sample.metallic)
    }

    fn weather(wetness: f32, puddles: f32, snow_cover: f32, ripple: f32) -> SurfaceWeatherFrame {
        SurfaceWeatherFrame {
            wetness,
            puddles,
            snow_cover,
            ripple,
            clock_s: 12.5,
        }
    }

    /// § 11 test 10.
    #[test]
    fn zero_weather_and_zero_exposure_are_bitwise_identities() {
        let sample = concrete();
        let response = resolved(&sample, &WeatherResponse::DEFAULT);
        assert_eq!(
            apply_surface_weather(sample, &response, &SurfaceWeatherFrame::DRY, 1.0),
            sample
        );
        assert_eq!(
            apply_surface_weather(sample, &response, &weather(1.0, 1.0, 1.0, 1.0), 0.0),
            sample
        );
    }

    /// § 11 test 10: φ = 0 keeps the albedo, φ = 1 at `w = 1` halves it, metal keeps it.
    #[test]
    fn porosity_decides_the_darkening_and_metal_gets_the_film() {
        let sample = concrete();
        let wet = weather(1.0, 0.0, 0.0, 0.0);
        let sealed = ResolvedWeatherResponse {
            porosity: 0.0,
            wet: 1.0,
            snow: 1.0,
            puddles: false,
        };
        let porous = ResolvedWeatherResponse {
            porosity: 1.0,
            ..sealed
        };
        assert_eq!(
            apply_surface_weather(sample, &sealed, &wet, 1.0).albedo,
            sample.albedo
        );
        let dark = apply_surface_weather(sample, &porous, &wet, 1.0);
        for c in 0..3 {
            assert!((dark.albedo[c] - 0.5 * sample.albedo[c]).abs() < 1.0e-6);
        }
        let metal = SurfaceSample {
            metallic: 1.0,
            roughness: 0.6,
            ..sample
        };
        let derived = resolved(&metal, &WeatherResponse::DEFAULT);
        assert_eq!(derived.porosity, 0.0);
        let filmed = apply_surface_weather(metal, &derived, &wet, 1.0);
        assert_eq!(filmed.albedo, metal.albedo);
        assert!(filmed.roughness < 0.2, "{}", filmed.roughness);
    }

    #[test]
    fn derived_porosity_matches_the_formula() {
        assert_eq!(derived_porosity(0.9, 0.0), 1.0);
        assert_eq!(derived_porosity(0.2, 0.0), 0.0);
        assert_eq!(derived_porosity(0.5, 0.0), 0.5);
        assert_eq!(derived_porosity(0.9, 1.0), 0.0);
    }

    /// § 11 test 10.
    #[test]
    fn puddles_need_the_flag_and_flat_ground() {
        let full = weather(1.0, 1.0, 0.0, 0.0);
        let flat = SurfaceSample {
            normal: [0.0, 1.0, 0.0],
            ..concrete()
        };
        let pools = ResolvedWeatherResponse {
            puddles: true,
            ..resolved(&flat, &WeatherResponse::DEFAULT)
        };
        // Sample a floor: some of it pools to a mirror, at most about 60 % of it.
        let mut pooled = 0;
        let total = 200 * 200;
        for x in 0..200 {
            for z in 0..200 {
                let sample = SurfaceSample {
                    position: [x as f32 * 0.25, 0.0, z as f32 * 0.25],
                    ..flat
                };
                if apply_surface_weather(sample, &pools, &full, 1.0).roughness < 0.05 {
                    pooled += 1;
                }
            }
        }
        let share = pooled as f32 / total as f32;
        assert!((0.2..=0.65).contains(&share), "{share}");
        let wall = SurfaceSample {
            normal: [1.0, 0.0, 0.0],
            geometric_normal: [1.0, 0.0, 0.0],
            ..flat
        };
        let out = apply_surface_weather(wall, &pools, &full, 1.0);
        assert!(out.roughness > 0.05, "no pool on a wall");
        let no_flag = ResolvedWeatherResponse {
            puddles: false,
            ..pools
        };
        for x in 0..50 {
            let sample = SurfaceSample {
                position: [x as f32 * 0.5, 0.0, 1.0],
                ..flat
            };
            assert!(apply_surface_weather(sample, &no_flag, &full, 1.0).roughness > 0.05);
        }
    }

    /// § 11 test 10: the ripple normal is unit with a bounded tilt, and fixed inputs give fixed
    /// bits.
    #[test]
    fn ripples_stay_bounded_and_repeat() {
        let mut moved = false;
        for k in 0..2000 {
            let xz = [k as f32 * 0.013, k as f32 * 0.007];
            let g = ripple_gradient(xz, 12.5 + k as f32 * 0.01, 1.0);
            assert_eq!(g, ripple_gradient(xz, 12.5 + k as f32 * 0.01, 1.0));
            let tilt = g[0].hypot(g[1]);
            assert!(tilt < 1.0, "{tilt}");
            moved |= tilt > 0.01;
        }
        assert!(moved, "the rings show");
        assert_eq!(
            ripple_gradient([1.0, 2.0], 3.0, 0.0),
            [0.0, 0.0],
            "no rain, no rings"
        );
    }

    /// § 11 test 10: in full puddles under the heaviest rain, the shaded normal (the ripple
    /// surface) is unit length with a bounded tilt, the rings move it, and fixed inputs give
    /// fixed bits.
    #[test]
    fn the_rippled_puddle_normal_is_unit_with_a_bounded_tilt() {
        let pools = ResolvedWeatherResponse {
            puddles: true,
            ..resolved(&concrete(), &WeatherResponse::DEFAULT)
        };
        let full = weather(1.0, 1.0, 0.0, 1.0);
        let flat = SurfaceSample {
            normal: [0.0, 1.0, 0.0],
            ..concrete()
        };
        let (mut pooled, mut tilted) = (0, 0);
        for k in 0..4000 {
            let sample = SurfaceSample {
                position: [k as f32 * 0.037, 0.0, (k / 64) as f32 * 0.29],
                ..flat
            };
            let out = apply_surface_weather(sample, &pools, &full, 1.0);
            assert_eq!(out, apply_surface_weather(sample, &pools, &full, 1.0));
            let n = out.normal;
            let length = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
            assert!((length - 1.0).abs() < 1.0e-5, "point {k}: |n| = {length}");
            // The ripple gradient's tilt stays under 1 (ripples_stay_bounded_and_repeat), so the
            // normal stays within 45 degrees of up.
            assert!(
                n[1] >= std::f32::consts::FRAC_1_SQRT_2 - 1.0e-5,
                "point {k}: {n:?}"
            );
            if out.roughness < 0.03 {
                pooled += 1;
                tilted += usize::from(n[1] < 0.9999);
            }
        }
        assert!(pooled > 100, "full puddles pool: {pooled}");
        assert!(tilted > 0, "the rings tilt the pool's normal");
    }

    /// § 11 test 10.
    #[test]
    fn snow_covers_what_faces_up_and_not_a_wall() {
        let snowy = weather(0.0, 0.0, 1.0, 0.0);
        let flat = concrete();
        let response = resolved(&flat, &WeatherResponse::DEFAULT);
        let covered = apply_surface_weather(flat, &response, &snowy, 1.0);
        assert!((covered.albedo[0] - 0.82).abs() < 1.0e-5);
        assert!((covered.roughness - 0.8).abs() < 1.0e-5);
        let wall = SurfaceSample {
            normal: [1.0, 0.0, 0.0],
            geometric_normal: [1.0, 0.0, 0.0],
            ..flat
        };
        assert_eq!(
            apply_surface_weather(wall, &response, &snowy, 1.0).albedo,
            wall.albedo
        );
        let none = ResolvedWeatherResponse {
            snow: 0.0,
            wet: 0.0,
            ..response
        };
        assert_eq!(
            apply_surface_weather(flat, &none, &snowy, 1.0).albedo,
            flat.albedo
        );
    }

    /// Snow over a pool: the pool's mirror is gone.
    #[test]
    fn snow_suppresses_the_puddles_under_it() {
        let flat = SurfaceSample {
            normal: [0.0, 1.0, 0.0],
            ..concrete()
        };
        let pools = ResolvedWeatherResponse {
            puddles: true,
            ..resolved(&flat, &WeatherResponse::DEFAULT)
        };
        for x in 0..100 {
            let sample = SurfaceSample {
                position: [x as f32 * 0.7, 0.0, 2.0],
                ..flat
            };
            let out = apply_surface_weather(sample, &pools, &weather(1.0, 1.0, 1.0, 1.0), 1.0);
            assert!(out.roughness > 0.7, "{}", out.roughness);
        }
    }

    /// The default packs to nothing, so old materials keep their bytes.
    #[test]
    fn the_default_response_packs_to_zero() {
        assert_eq!(WeatherResponse::DEFAULT.packed_bits(), 0);
        assert_eq!(WeatherResponse::default(), WeatherResponse::DEFAULT);
        let none = WeatherResponse::NONE.packed_bits();
        assert_eq!(none, (15 << 2) | (15 << 6));
        let custom = WeatherResponse {
            porosity: Some(1.0),
            wet: 1.0,
            snow: 1.0,
            puddles: true,
        };
        assert_eq!(custom.packed_bits(), (1 << 10) | (255 << 11));
        assert!(
            custom.packed_bits() < 1 << 24,
            "exact in the f32 the record stores"
        );
    }

    #[test]
    fn dry_frames_are_dry() {
        assert!(SurfaceWeatherFrame::DRY.is_dry());
        assert_eq!(SurfaceWeatherFrame::default(), SurfaceWeatherFrame::DRY);
        assert!(!weather(0.1, 0.0, 0.0, 0.0).is_dry());
        assert_eq!(weather(0.1, 0.2, 0.3, 0.4).row(), [0.1, 0.2, 0.3, 0.4]);
    }

    /// The CPU mirror of `precip_exposure` counts a tap outside the tile as open and treats the
    /// clip edge (local UV 1) as inside, like `shadow_pcf_rect`.
    #[test]
    fn exposure_at_matches_the_tile_edge_filter() {
        let n = 16u32;
        let map = PrecipOcclusionMap {
            frame: 0,
            size: n,
            // Depth 0.2 everywhere: covered for a 0.5 compare on every in-tile tap.
            depth: vec![0.2; (n * n) as usize],
            view_proj: regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
            depth_per_m: 0.0,
            texel_m: 1.0 / n as f32,
        };
        // The centre of texel (0, 0): five of the nine taps leave the tile and count open.
        let corner = [2.0 * (0.5 / n as f32) - 1.0, 1.0 - 2.0 * (0.5 / n as f32), 0.5];
        let (open, cover) = map.exposure_at(corner);
        assert!((open - 5.0 / 9.0).abs() < 1.0e-6, "{open}");
        assert!(cover.is_some(), "the covered centre reports a cover height");
        // A non-corner edge texel: three taps out.
        let edge = [2.0 * (0.5 / n as f32) - 1.0, 0.0, 0.5];
        let (open, _) = map.exposure_at(edge);
        assert!((open - 3.0 / 9.0).abs() < 1.0e-6, "{open}");
        // Local UV exactly 1 (the +U clip edge): inside, sampling the last texel, not open sky.
        let rim = [1.0, 0.0, 0.5];
        let (open, cover) = map.exposure_at(rim);
        assert!(open < 1.0, "the rim is not fully open: {open}");
        assert!((open - 3.0 / 9.0).abs() < 1.0e-6, "{open}");
        assert!(cover.is_some(), "the rim still reports its cover height");
        // Well outside the map: open sky.
        assert_eq!(map.exposure_at([2.0, 0.0, 0.5]), (1.0, None));
    }

    /// ADR-2585: a caster is moving while its shape changed within the settle window, and only
    /// then; a new caster is not moving, and the sweep forgets casters gone from the view.
    #[test]
    fn a_caster_moves_until_it_settles() {
        let mut motion = PrecipCasterMotion::default();
        assert!(!motion.moving(7, 100, 0), "first seen: exact");
        assert!(!motion.moving(7, 100, 1), "still: exact");
        assert!(motion.moving(7, 101, 2), "changed: moving");
        assert!(
            motion.moving(7, 101, 2 + PRECIP_SETTLE_FRAMES - 1),
            "inside the window"
        );
        assert!(!motion.moving(7, 101, 2 + PRECIP_SETTLE_FRAMES), "settled");
        assert!(!motion.moving(8, 5, 40), "another caster, first seen");
        motion.sweep(256);
        assert_eq!(motion.tracked(), 2, "both seen within the sweep window");
        motion.sweep(512);
        assert_eq!(motion.tracked(), 0, "gone from the view: forgotten");
    }
}
