// SPDX-License-Identifier: MPL-2.0
//! Dynamic diffuse irradiance probes (M15b).
//!
//! A grid of probes gathers one-bounce radiance by sphere-tracing the scene SDF,
//! stores it as L1 spherical harmonics plus a 4×4 octahedral depth map, and
//! shades with trilinear weights, a back-face wrap and Chebyshev visibility.
//! Screen-space gathering is the cheaper tier (no probe trace).
//!
//! ## Provenance
//!
//! Majercik, Marrs, Spjut, McGuire, "Dynamic Diffuse Global Illumination with
//! Ray-Traced Irradiance Fields", JCGT 8(2), 2019: probe grid, hysteresis,
//! back-face weight, Chebyshev visibility against probe depth. This build stores
//! L1 irradiance (Ramamoorthi and Hanrahan, "An Efficient Representation for
//! Irradiance Environment Maps", SIGGRAPH 2001; Sloan, "Stupid Spherical Harmonics
//! Tricks", GDC 2008) instead of an octahedral irradiance map, and 4×4 depth
//! instead of the paper's higher-resolution depth texture.
//! Rays: spherical Fibonacci (Keinert, Schumacher, et al., "Spherical Fibonacci
//! Mapping", ACM TOG 2015). Octahedral index: Cigolle, Donow, Evangelakos,
//! Mara, McGuire, Meyer, "A Survey of Efficient Representations for Independent
//! Unit Vectors", JCGT 2014.

use regia_core::glam::Vec3;

use crate::sdf::{SdfScene, sphere_trace};

const Y00: f32 = 0.282_094_8;
const Y1: f32 = 0.488_602_52;
const PI: f32 = std::f32::consts::PI;
/// Octahedral depth texels along one edge.
pub const PROBE_DEPTH_RES: usize = 4;
/// GPU upload cap. Larger grids are rejected by [`GiField::new`].
pub const MAX_PROBES: usize = 256;

/// Directional light used while tracing probes.
#[derive(Clone, Copy, Debug)]
pub struct ProbeSun {
    /// Direction the light travels (from the sun toward the scene).
    pub direction: [f32; 3],
    /// Linear colour.
    pub color: [f32; 3],
    /// Illuminance in lux.
    pub illuminance: f32,
}

/// One probe after an update.
#[derive(Clone, Debug)]
struct Probe {
    /// L1 radiance SH, `[L00, L1x, L1y, L1z]`, one set per channel.
    sh: [[f32; 4]; 3],
    depth_mean: [f32; 16],
    depth_msq: [f32; 16],
    /// Rays that landed in each octahedral cell on the last update. A cell with `0` was
    /// not sampled; its moments are carried or a near-hit sentinel, never a miss
    /// (eight-ray updates leave half the cells untraced).
    depth_n: [u32; 16],
}

/// The depth mean the inside-a-solid path and never-sampled cells store, metres: a near
/// hit, so the direction reads as occluding.
const BURIED_DEPTH_M: f32 = 0.05;

impl Probe {
    fn empty(max_distance: f32) -> Self {
        Self {
            sh: [[0.0; 4]; 3],
            depth_mean: [max_distance; 16],
            depth_msq: [max_distance * max_distance; 16],
            depth_n: [0; 16],
        }
    }
}

/// The inside-a-solid sentinel: black SH and a near-hit mean in every cell. Its Chebyshev
/// weight alone would pull a weighted average toward black.
fn is_buried(probe: &Probe) -> bool {
    probe.sh.iter().flatten().all(|&c| c == 0.0)
        && probe.depth_mean.iter().all(|&m| m <= BURIED_DEPTH_M + 1.0e-6)
}

/// Irradiance-probe volume.
#[derive(Clone, Debug)]
pub struct GiField {
    /// World position of probe `(0,0,0)`.
    pub origin: [f32; 3],
    /// Metres between probes.
    pub spacing: [f32; 3],
    /// Probe counts.
    pub dims: [u32; 3],
    /// Multiplier on the shaded irradiance (1 = the traced energy).
    pub intensity: f32,
    /// Metres the shade point is pushed along the normal before the lookup.
    pub normal_bias: f32,
    probes: Vec<Probe>,
    /// Previous update's probes. Reused so a later bounce does not allocate a second grid.
    bounce_probes: Vec<Probe>,
}

/// Probe grid construction failed.
#[derive(Debug, thiserror::Error)]
pub enum GiError {
    /// A dimension or spacing was not usable.
    #[error("probe grid needs positive spacing and at least one probe per axis")]
    BadGrid,
    /// The grid is larger than the GPU buffer.
    #[error("probe grid has {count} probes; the GPU buffer holds {MAX_PROBES}")]
    TooManyProbes {
        /// How many were requested.
        count: usize,
    },
}

impl GiField {
    /// Empty probes at `origin + (x,y,z) * spacing`.
    ///
    /// # Errors
    ///
    /// Non-positive spacing, a zero axis, or more than [`MAX_PROBES`] probes.
    pub fn new(origin: [f32; 3], spacing: [f32; 3], dims: [u32; 3]) -> Result<Self, GiError> {
        if spacing.iter().any(|s| !s.is_finite() || *s <= 0.0) || dims.contains(&0) {
            return Err(GiError::BadGrid);
        }
        // A wrapped product must not pass for a small probe count (release has no overflow checks).
        let count = (dims[0] as usize)
            .checked_mul(dims[1] as usize)
            .and_then(|c| c.checked_mul(dims[2] as usize))
            .unwrap_or(usize::MAX);
        if count > MAX_PROBES {
            return Err(GiError::TooManyProbes { count });
        }
        Ok(Self {
            origin,
            spacing,
            dims,
            intensity: 1.0,
            normal_bias: 0.08,
            probes: (0..count).map(|_| Probe::empty(8.0)).collect(),
            bounce_probes: Vec::new(),
        })
    }

    /// Number of probes.
    #[must_use]
    pub fn probe_count(&self) -> usize {
        self.probes.len()
    }

    /// World position of the probe at integer coordinates.
    #[must_use]
    pub fn probe_position(&self, x: u32, y: u32, z: u32) -> [f32; 3] {
        [
            self.origin[0] + x as f32 * self.spacing[0],
            self.origin[1] + y as f32 * self.spacing[1],
            self.origin[2] + z as f32 * self.spacing[2],
        ]
    }

    /// `true` when `position` lies inside the probe box (first to last probe).
    #[must_use]
    pub fn contains(&self, position: [f32; 3]) -> bool {
        (0..3).all(|axis| {
            let extent = (self.dims[axis].saturating_sub(1)) as f32 * self.spacing[axis];
            let local = position[axis] - self.origin[axis];
            (0.0..=extent).contains(&local)
        })
    }

    fn index(&self, x: u32, y: u32, z: u32) -> usize {
        (z as usize * self.dims[1] as usize + y as usize) * self.dims[0] as usize + x as usize
    }
}

/// One point or spot gathered into a probe, in the same units as the forward shader.
///
/// Point `candela` is lumens / 4π ([`Self::from_lab`] does that conversion).
/// Spot `candela` is the authored luminous intensity. `radius` is the influence
/// cutoff. The windowed inverse-square and the spot cone match `pbr_lighting.wgsl`.
#[derive(Clone, Copy, Debug)]
pub struct ProbeLocalLight {
    /// World position.
    pub position: [f32; 3],
    /// Spot aim, from the light into the scene. Ignored for a point.
    pub direction: [f32; 3],
    /// Linear RGB.
    pub color: [f32; 3],
    /// Luminous intensity in candela.
    pub candela: f32,
    /// Influence radius, metres. No contribution past this.
    pub radius: f32,
    /// Cosine of the inner cone half-angle. Ignored for a point.
    pub inner_cos: f32,
    /// Cosine of the outer cone half-angle. Ignored for a point.
    pub outer_cos: f32,
    /// Spot when true, point when false.
    pub spot: bool,
}

impl ProbeLocalLight {
    /// The shader's candela for one lab light.
    #[must_use]
    pub fn from_lab(light: &crate::lab_frame::LabLocalLight) -> Self {
        match *light {
            crate::lab_frame::LabLocalLight::Point(point) => Self {
                position: point.position,
                direction: [0.0, -1.0, 0.0],
                color: point.color,
                candela: point.intensity / (4.0 * PI),
                radius: point.radius,
                inner_cos: 1.0,
                outer_cos: 0.0,
                spot: false,
            },
            crate::lab_frame::LabLocalLight::Spot(spot) => Self {
                position: spot.position,
                direction: spot.direction,
                color: spot.color,
                candela: spot.intensity,
                radius: spot.range,
                inner_cos: spot.inner_angle.cos(),
                outer_cos: spot.outer_angle.cos(),
                spot: true,
            },
        }
    }
}

/// Traces `rays` Fibonacci directions from every probe and blends spherical harmonics
/// with `hysteresis`.
///
/// `rays` is clamped to 8..=128. `hysteresis` is clamped to 0.0..=0.99: 0 replaces the
/// harmonics, and 1 is stored as 0.99 (99 % of the previous harmonics, 1 % of this trace).
/// Depth moments are written from this trace and are not blended. A probe whose centre
/// lies inside a solid is replaced with a black probe and 0.05 m depth texels before the
/// blend.
///
/// A miss uses the outdoor sky. Interiors that must not leak sky call
/// [`update_irradiance_with_lights`] with `sky_on_miss` false.
#[tracing::instrument(skip_all)]
pub fn update_irradiance(field: &mut GiField, scene: &SdfScene, sun: ProbeSun, rays: u32, hysteresis: f32) {
    update_irradiance_with_lights(field, scene, sun, &[], true, rays, hysteresis);
}

/// [`update_irradiance`] plus punctual lights, and a choice of what a miss stores.
///
/// `lights` are evaluated at each hit: windowed inverse-square, the spot cone,
/// and a shadow ray that stops at the light. The stored radiance is
/// `albedo / π · incident`, the same one-bounce quantity the sun term stores, so
/// the forward shader's `albedo · sample_ddgi` is the receiver's bounce.
/// `sky_on_miss` false stores black on a miss. An open tunnel mouth then does
/// not paint the bore with the sky; only surfaces the lights actually reach
/// bounce. `rays` and `hysteresis` use the same clamps as [`update_irradiance`]:
/// 8..=128 rays, hysteresis 0.0..=0.99, depth moments replaced, a buried probe cleared.
#[tracing::instrument(skip_all)]
pub fn update_irradiance_with_lights(
    field: &mut GiField,
    scene: &SdfScene,
    sun: ProbeSun,
    lights: &[ProbeLocalLight],
    sky_on_miss: bool,
    rays: u32,
    hysteresis: f32,
) {
    let procedural = |dir: [f32; 3]| crate::ibl::sky_radiance(dir);
    let black = |_: [f32; 3]| [0.0; 3];
    let sky: &dyn Fn([f32; 3]) -> [f32; 3] = if sky_on_miss { &procedural } else { &black };
    update_irradiance_with_sky(field, scene, sun, lights, sky, rays, hysteresis);
}

/// [`update_irradiance_with_lights`] with the radiance (cd/m²) an escaping ray sees
/// given by `sky`: the planet's sky and ground from the atmosphere tables, for a
/// daylit interior whose openings let the real sky in. `rays` and `hysteresis` use
/// the same clamps as [`update_irradiance`].
#[tracing::instrument(skip_all)]
pub fn update_irradiance_with_sky(
    field: &mut GiField,
    scene: &SdfScene,
    sun: ProbeSun,
    lights: &[ProbeLocalLight],
    sky: &dyn Fn([f32; 3]) -> [f32; 3],
    rays: u32,
    hysteresis: f32,
) {
    let rays = rays.clamp(8, 128);
    let hysteresis = hysteresis.clamp(0.0, 0.99);
    let max_distance = 12.0;
    let dw = 4.0 * PI / rays as f32;
    let sun_dir = Vec3::from_array(sun.direction);
    let sun_dir = if sun_dir.length_squared() > 1.0e-8 {
        sun_dir.normalize()
    } else {
        Vec3::new(0.0, -1.0, 0.0)
    };
    let toward_sun = -sun_dir;
    // Multi-bounce (Majercik et al. 2019 § 5): a ray that hits geometry picks up the
    // irradiance the *previous* update stored there, so repeated updates converge to
    // infinite bounces. The first update is single-bounce (sun only at hits).
    let prev_probes = previous_bounce(field);
    for z in 0..field.dims[2] {
        for y in 0..field.dims[1] {
            for x in 0..field.dims[0] {
                let position = field.probe_position(x, y, z);
                let mut sh = [[0.0; 4]; 3];
                let mut depth_sum = [0.0; 16];
                let mut depth_sq = [0.0; 16];
                let mut depth_n = [0u32; 16];
                if scene.sample(position).distance < 0.0 {
                    // Inside a solid: leave the probe black and fully occluding.
                    let idx = field.index(x, y, z);
                    field.probes[idx] = Probe::empty(BURIED_DEPTH_M);
                    continue;
                }
                for i in 0..rays {
                    let dir = fibonacci_direction(i, rays);
                    let cell = oct_cell(dir);
                    let hit = sphere_trace(scene, position, dir, max_distance);
                    let (radiance, dist) = if let Some(hit) = hit {
                        let n = Vec3::from_array(hit.normal);
                        // A second ray toward the sun. Without it a closed room's floor
                        // is lit as if the ceiling were missing (sky leak).
                        let shadow_origin = (Vec3::from_array(hit.position) + n * 0.02).to_array();
                        let blocked = sun.illuminance > 0.0
                            && sphere_trace(scene, shadow_origin, toward_sun.to_array(), max_distance)
                                .is_some();
                        let ndotl = if blocked { 0.0 } else { n.dot(toward_sun).max(0.0) };
                        let direct = sun.illuminance * ndotl;
                        // Hits outside the box read the nearest (clamped) probes, never
                        // the open sky: a wall just outside the grid of a closed room
                        // must not glow with sky light.
                        let indirect = prev_probes.as_deref().map_or([0.0; 3], |prev| {
                            // The bounce traces physical energy: `intensity` scales the shade
                            // only, so a stored bounce is not scaled a second time here.
                            sample_stored(field, prev, 1.0, hit.position, hit.normal)
                        });
                        let mut rgb: [f32; 3] = std::array::from_fn(|c| {
                            hit.albedo[c] * (sun.color[c] * direct / PI + indirect[c]) + hit.emissive[c]
                        });
                        let bounced = punctual_radiance(&hit, scene, lights);
                        for (out, bounce) in rgb.iter_mut().zip(bounced) {
                            *out += bounce;
                        }
                        (rgb, hit.t)
                    } else {
                        // An escaping ray sees the sky: the same radiance the IBL uses.
                        // (It used to add `sun_illuminance / π` across the whole sunward
                        // hemisphere: a second, unshadowed sun that flattened every
                        // probe-lit frame.) A sealed interior's bake passes a black sky,
                        // so an open tunnel mouth is not that sky.
                        (sky(dir), max_distance)
                    };
                    depth_sum[cell] += dist;
                    depth_sq[cell] += dist * dist;
                    depth_n[cell] += 1;
                    let basis = [Y00, Y1 * dir[0], Y1 * dir[1], Y1 * dir[2]];
                    for c in 0..3 {
                        for k in 0..4 {
                            sh[c][k] += radiance[c] * basis[k] * dw;
                        }
                    }
                }
                let mut depth_mean = [max_distance; 16];
                let mut depth_msq = [max_distance * max_distance; 16];
                let idx = field.index(x, y, z);
                let previous = &field.probes[idx];
                for cell in 0..16 {
                    if depth_n[cell] > 0 {
                        let inv = 1.0 / depth_n[cell] as f32;
                        depth_mean[cell] = depth_sum[cell] * inv;
                        depth_msq[cell] = depth_sq[cell] * inv;
                    } else if previous.depth_n[cell] > 0 {
                        // No ray landed here this pass: keep the previous moments, so a
                        // direction the low ray counts never reach stays occluded by what
                        // it last saw instead of resetting to a 12 m miss.
                        depth_mean[cell] = previous.depth_mean[cell];
                        depth_msq[cell] = previous.depth_msq[cell];
                        // Carried moments keep their sample count, so a second pass that
                        // misses the cell carries them again instead of resetting it.
                        depth_n[cell] = previous.depth_n[cell];
                    } else {
                        // Never sampled at all: store a near hit, so an untraced direction
                        // occludes instead of showing a receiver the far miss.
                        depth_mean[cell] = BURIED_DEPTH_M;
                        depth_msq[cell] = BURIED_DEPTH_M * BURIED_DEPTH_M;
                    }
                }
                let mut blended = Probe {
                    sh,
                    depth_mean,
                    depth_msq,
                    depth_n,
                };
                if hysteresis > 0.0 {
                    for c in 0..3 {
                        for k in 0..4 {
                            blended.sh[c][k] =
                                previous.sh[c][k] * hysteresis + blended.sh[c][k] * (1.0 - hysteresis);
                        }
                    }
                }
                field.probes[idx] = blended;
            }
        }
    }
    if let Some(prev) = prev_probes {
        field.bounce_probes = prev;
    }
}

/// The probe grid from the previous update, in the field's scratch buffer.
///
/// `None` on the first update (every L00 coefficient is still zero). The caller puts the
/// vec back into the field's bounce buffer so the next bounce reuses the allocation.
fn previous_bounce(field: &mut GiField) -> Option<Vec<Probe>> {
    let lit = field
        .probes
        .iter()
        .any(|probe| probe.sh.iter().any(|channel| channel[0] != 0.0));
    if !lit {
        return None;
    }
    let mut prev = std::mem::take(&mut field.bounce_probes);
    #[cfg(test)]
    let before = prev.capacity();
    prev.clone_from(&field.probes);
    #[cfg(test)]
    if prev.capacity() > before {
        note_probe_buffer_grow();
    }
    Some(prev)
}

/// Outgoing radiance at `hit` from `lights`. Zero when every light misses or is shadowed.
fn punctual_radiance(hit: &crate::sdf::SdfHit, scene: &SdfScene, lights: &[ProbeLocalLight]) -> [f32; 3] {
    if lights.is_empty() {
        return [0.0; 3];
    }
    let n = Vec3::from_array(hit.normal);
    let p = Vec3::from_array(hit.position);
    let mut rgb = [0.0; 3];
    for light in lights {
        if !light.candela.is_finite()
            || light.candela <= 0.0
            || !light.radius.is_finite()
            || light.radius <= 0.05
        {
            continue;
        }
        let to = Vec3::from_array(light.position) - p;
        let dist = to.length();
        if dist >= light.radius || dist < 1.0e-3 {
            continue;
        }
        let ldir = to / dist;
        let ndotl = n.dot(ldir).max(0.0);
        if ndotl <= 0.0 {
            continue;
        }
        let mut att = distance_attenuation(dist, light.radius);
        if light.spot {
            let aim = Vec3::from_array(light.direction);
            if aim.length_squared() < 1.0e-8 {
                continue;
            }
            att *= spot_attenuation(ldir, aim.normalize(), light.inner_cos, light.outer_cos);
        }
        if att <= 0.0 {
            continue;
        }
        let origin = (p + n * 0.04).to_array();
        let reach = (dist - 0.06).max(0.0);
        if reach > 0.02 && sphere_trace(scene, origin, ldir.to_array(), reach).is_some() {
            continue;
        }
        let incident = light.candela * att * ndotl;
        for ((out, albedo), color) in rgb.iter_mut().zip(hit.albedo).zip(light.color) {
            *out += albedo * color * incident / PI;
        }
    }
    rgb
}

/// Windowed inverse-square (Lagarde, Frostbite 2014). Matches `distance_attenuation` in the shader.
fn distance_attenuation(distance: f32, radius: f32) -> f32 {
    let radius = radius.max(1.0e-4);
    let distance = distance.max(0.0);
    let window = (1.0 - (distance / radius).powi(4)).clamp(0.0, 1.0);
    (window * window) / (distance * distance + 1.0)
}

/// Smooth spot cone. `ldir` points from the surface toward the light.
fn spot_attenuation(ldir: Vec3, spot_dir: Vec3, inner_cos: f32, outer_cos: f32) -> f32 {
    let cos_angle = (-spot_dir).dot(ldir);
    let denom = (inner_cos - outer_cos).max(1.0e-4);
    ((cos_angle - outer_cos) / denom).clamp(0.0, 1.0)
}

/// Diffuse irradiance divided by π (same quantity the forward shader multiplies
/// by `diffuse_color`), at `position` with geometric normal `normal`.
#[must_use]
pub fn sample_irradiance(field: &GiField, position: [f32; 3], normal: [f32; 3]) -> [f32; 3] {
    sample_stored(field, &field.probes, field.intensity, position, normal)
}

/// [`sample_irradiance`] against `probes` instead of the field's current grid, so a bounce can
/// read the previous update while the current grid is being replaced.
fn sample_stored(
    field: &GiField,
    probes: &[Probe],
    intensity: f32,
    position: [f32; 3],
    normal: [f32; 3],
) -> [f32; 3] {
    let n = Vec3::from_array(normal);
    let n = if n.length_squared() > 1.0e-8 {
        n.normalize()
    } else {
        Vec3::Y
    };
    let biased = Vec3::from_array(position) + n * field.normal_bias;
    let mut local = [0.0; 3];
    for axis in 0..3 {
        local[axis] = (biased[axis] - field.origin[axis]) / field.spacing[axis];
    }
    let mut base = [0i32; 3];
    let mut frac = [0.0; 3];
    for axis in 0..3 {
        let max_i = field.dims[axis] as i32 - 1;
        let cell = local[axis].floor() as i32;
        let cell = cell.clamp(0, max_i.max(0));
        let next = (cell + 1).min(max_i.max(0));
        base[axis] = cell;
        frac[axis] = if next == cell {
            0.0
        } else {
            (local[axis] - cell as f32).clamp(0.0, 1.0)
        };
    }
    let mut accum = [0.0; 3];
    let mut weight_sum = 0.0;
    for dz in 0..2 {
        for dy in 0..2 {
            for dx in 0..2 {
                let x = (base[0] + dx).clamp(0, field.dims[0] as i32 - 1) as u32;
                let y = (base[1] + dy).clamp(0, field.dims[1] as i32 - 1) as u32;
                let z = (base[2] + dz).clamp(0, field.dims[2] as i32 - 1) as u32;
                let wx = if dx == 1 { frac[0] } else { 1.0 - frac[0] };
                let wy = if dy == 1 { frac[1] } else { 1.0 - frac[1] };
                let wz = if dz == 1 { frac[2] } else { 1.0 - frac[2] };
                let probe_pos = Vec3::from_array(field.probe_position(x, y, z));
                // Depth was traced from the probe, so the visibility ray points at
                // the receiver. The wrap weight still faces the probe.
                let from_probe = biased - probe_pos;
                let dist = from_probe.length().max(1.0e-4);
                let dir = (from_probe / dist).to_array();
                let wrap = (n.dot(-Vec3::from_array(dir)) * 0.5 + 0.5).clamp(0.0, 1.0);
                let probe = &probes[field.index(x, y, z)];
                // A probe replaced by the inside-a-solid sentinel carries black SH: its
                // Chebyshev weight alone (never below the 0.15 drop within a quarter
                // metre) would pull the weighted average toward black. Drop it.
                if is_buried(probe) {
                    continue;
                }
                // Bilinear moments, matching `depth_moments` in `lab_forward.wgsl`.
                // A point sample of a 4×4 cell flips along the cell edge.
                let (mean, mean_sq) = depth_moments(probe, dir);
                let vis = chebyshev(mean, mean_sq, dist);
                // A weighted average would promote a single occluded probe back to
                // full brightness. Drop it so a roof between the probe and the
                // receiver cannot leak sky.
                if vis < 0.15 {
                    continue;
                }
                let weight = wx * wy * wz * wrap * wrap * vis;
                if weight <= 0.0 {
                    continue;
                }
                let normal = n.to_array();
                for (slot, sum) in accum.iter_mut().enumerate() {
                    *sum += weight * irradiance_over_pi(probe.sh[slot], normal);
                }
                weight_sum += weight;
            }
        }
    }
    if weight_sum <= 1.0e-5 {
        return [0.0; 3];
    }
    [
        accum[0] / weight_sum * intensity,
        accum[1] / weight_sum * intensity,
        accum[2] / weight_sum * intensity,
    ]
}

/// `E/π` from L1 radiance coefficients. A constant radiance of 1 evaluates to 1.
#[must_use]
pub fn irradiance_over_pi(sh: [f32; 4], normal: [f32; 3]) -> f32 {
    let a0 = PI;
    let a1 = 2.0 * PI / 3.0;
    let e = a0 * sh[0] * Y00 + a1 * Y1 * (sh[1] * normal[0] + sh[2] * normal[1] + sh[3] * normal[2]);
    (e / PI).max(0.0)
}

/// Metres². A 4×4 octahedral cell often stores one ray, so the raw variance is
/// zero and Chebyshev collapses to a step (Donnelly & Lauritzen 2006). The
/// cockpit wall sat 17 cm past such a ray and went black beside a neighbour
/// the next cell still saw. 0.008 keeps that sample above the 0.15 reject and
/// still drops a receiver a metre past the mean, which is what blocks the sky.
const CHEBYSHEV_MIN_VARIANCE: f32 = 0.008;

fn chebyshev(mean: f32, mean_sq: f32, dist: f32) -> f32 {
    if dist <= mean + 0.05 {
        return 1.0;
    }
    let variance = (mean_sq - mean * mean).max(CHEBYSHEV_MIN_VARIANCE);
    let d = dist - mean;
    (variance / (variance + d * d)).clamp(0.0, 1.0)
}

/// Spherical Fibonacci direction `i` of `n`, Y-up.
#[must_use]
pub fn fibonacci_direction(i: u32, n: u32) -> [f32; 3] {
    let n = n.max(1) as f32;
    let i_f = i as f32;
    let phi = PI * (3.0 - 5.0_f32.sqrt());
    let y = 1.0 - (2.0 * i_f + 1.0) / n;
    let radius = (1.0 - y * y).max(0.0).sqrt();
    let theta = phi * i_f;
    [radius * theta.cos(), y, radius * theta.sin()]
}

/// Octahedral UV in `[0, 1)`. Shared with `oct_cell` and the bilinear fetch.
fn oct_uv(dir: [f32; 3]) -> (f32, f32) {
    let n = Vec3::from_array(dir);
    let n = if n.length_squared() > 1.0e-8 {
        n.normalize()
    } else {
        Vec3::Y
    };
    let l1 = n.x.abs() + n.y.abs() + n.z.abs();
    let mut u = n.x / l1;
    let mut v = n.y / l1;
    if n.z < 0.0 {
        let ou = u;
        u = (1.0 - v.abs()) * ou.signum();
        v = (1.0 - ou.abs()) * v.signum();
    }
    let su = (u * 0.5 + 0.5).clamp(0.0, 0.999);
    let sv = (v * 0.5 + 0.5).clamp(0.0, 0.999);
    (su, sv)
}

/// 4×4 octahedral cell for a direction.
#[must_use]
pub fn oct_cell(dir: [f32; 3]) -> usize {
    let (su, sv) = oct_uv(dir);
    let x = (su * PROBE_DEPTH_RES as f32) as usize;
    let y = (sv * PROBE_DEPTH_RES as f32) as usize;
    y * PROBE_DEPTH_RES + x
}

/// Bilinear mean and mean-square hit distance. Majercik et al. filter the
/// octahedral depth texture; a point sample of this 4×4 map is a step at
/// every cell edge, and the tunnel cap sat on one of those steps.
fn depth_moments(probe: &Probe, dir: [f32; 3]) -> (f32, f32) {
    let (su, sv) = oct_uv(dir);
    let res = PROBE_DEPTH_RES as f32;
    let fx = (su * res - 0.5).clamp(0.0, res - 1.0);
    let fy = (sv * res - 0.5).clamp(0.0, res - 1.0);
    let x0 = fx.floor() as usize;
    let y0 = fy.floor() as usize;
    let x1 = (x0 + 1).min(PROBE_DEPTH_RES - 1);
    let y1 = (y0 + 1).min(PROBE_DEPTH_RES - 1);
    let tx = fx - x0 as f32;
    let ty = fy - y0 as f32;
    let tap = |x: usize, y: usize| {
        let i = y * PROBE_DEPTH_RES + x;
        (probe.depth_mean[i], probe.depth_msq[i])
    };
    let (m00, s00) = tap(x0, y0);
    let (m10, s10) = tap(x1, y0);
    let (m01, s01) = tap(x0, y1);
    let (m11, s11) = tap(x1, y1);
    let lerp = |a: f32, b: f32, t: f32| a + (b - a) * t;
    (
        lerp(lerp(m00, m10, tx), lerp(m01, m11, tx), ty),
        lerp(lerp(s00, s10, tx), lerp(s01, s11, tx), ty),
    )
}

/// One texel of a screen-space gather (the cheaper GI tier).
#[derive(Clone, Copy, Debug)]
pub struct ScreenTexel {
    /// View-space or world depth. Smaller is closer.
    pub depth: f32,
    /// Display or linear colour.
    pub color: [f32; 3],
}

/// Adds coloured light from neighbours that sit near `depth` (Ritschel et al.
/// style image-space bounce, one gather, no probe trace).
#[must_use]
pub fn screen_space_gather(texels: &[ScreenTexel], depth: f32, radius: usize) -> [f32; 3] {
    if texels.is_empty() || radius == 0 {
        return [0.0; 3];
    }
    let mut sum = [0.0; 3];
    let mut weight = 0.0;
    for (i, texel) in texels.iter().enumerate() {
        let dist = (i as isize - texels.len() as isize / 2).unsigned_abs();
        if dist == 0 || dist > radius {
            continue;
        }
        let depth_delta = (texel.depth - depth).abs();
        if depth_delta > 0.25 {
            continue;
        }
        let w = 1.0 / dist as f32;
        sum[0] += texel.color[0] * w;
        sum[1] += texel.color[1] * w;
        sum[2] += texel.color[2] * w;
        weight += w;
    }
    if weight <= 1.0e-5 {
        return [0.0; 3];
    }
    [sum[0] / weight, sum[1] / weight, sum[2] / weight]
}

/// Texel step of a native-resolution screen-space GI gather.
pub const SSGI_GATHER_NATIVE: u32 = 1;
/// Texel step of a half-resolution gather (Desktop High at the 0.67 budget).
pub const SSGI_GATHER_HALF: u32 = 2;
/// Texel step of a quarter-resolution gather. Off unless [`ScreenSpaceGiSettings::quarter_gather`] is set.
pub const SSGI_GATHER_QUARTER: u32 = 4;

/// Screen-space GI gather settings.
///
/// The preset still chooses step [`SSGI_GATHER_NATIVE`] or [`SSGI_GATHER_HALF`].
/// [`Self::quarter_gather`] is the only override, and it stays off (ADR-3964). It is not a
/// field of `RenderProfile`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScreenSpaceGiSettings {
    /// Gather at quarter resolution (texel step 4) and joint-bilateral upsample onto the full frame.
    ///
    /// `false` (the default) keeps the preset: step 2 on the Desktop High 0.67 budget, step 1
    /// everywhere else.
    pub quarter_gather: bool,
}

/// The gather texel step: 4 when quarter is on, otherwise 2 when the preset gathers at half
/// resolution, otherwise 1.
#[must_use]
pub fn ssgi_gather_step(preset_half: bool, settings: ScreenSpaceGiSettings) -> u32 {
    if settings.quarter_gather {
        SSGI_GATHER_QUARTER
    } else if preset_half {
        SSGI_GATHER_HALF
    } else {
        SSGI_GATHER_NATIVE
    }
}

/// Gather target size in texels. A step other than 2 or 4 counts as native. A zero extent
/// becomes one texel (`div_ceil` of zero is zero, and a gather target cannot be empty).
#[must_use]
pub fn ssgi_gather_size(width: u32, height: u32, step: u32) -> (u32, u32) {
    let step = match step {
        SSGI_GATHER_HALF | SSGI_GATHER_QUARTER => step,
        _ => SSGI_GATHER_NATIVE,
    };
    (width.div_ceil(step).max(1), height.div_ceil(step).max(1))
}

/// Low-resolution coordinate of one full-resolution pixel under the joint bilateral upsample.
///
/// Matches `fs_upsample` in `ssgi.wgsl`: `(full_px + 0.5) / step - 0.5`. The gather fragment
/// `0` at step 2 samples full pixel 1 (low coordinate 0.25, texel 0); at step 4 it samples
/// full pixel 2 (low coordinate 0.125, texel 0).
#[must_use]
pub fn ssgi_upsample_low_coord(full_px: f32, step: u32) -> f32 {
    let step = match step {
        SSGI_GATHER_HALF | SSGI_GATHER_QUARTER => step as f32,
        _ => SSGI_GATHER_NATIVE as f32,
    };
    (full_px + 0.5) / step - 0.5
}

/// The screen-space gather switch (ADR-3964).
///
/// `FrameRenderer` is not edited this lane, so the quarter flag lives here and the gather
/// reads it when it writes `noise.y`. One process, one setting. A poisoned lock keeps the
/// last value.
struct GatherSwitch {
    settings: ScreenSpaceGiSettings,
    /// Texel step the last gather wrote. `0` before any gather.
    used_step: u32,
}

fn gather_switch() -> &'static std::sync::Mutex<GatherSwitch> {
    static SWITCH: std::sync::Mutex<GatherSwitch> = std::sync::Mutex::new(GatherSwitch {
        settings: ScreenSpaceGiSettings {
            quarter_gather: false,
        },
        used_step: 0,
    });
    &SWITCH
}

fn with_gather_switch<T>(f: impl FnOnce(&mut GatherSwitch) -> T) -> T {
    let mut guard = gather_switch()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    f(&mut guard)
}

/// The screen-space GI gather settings the next gather will read.
#[must_use]
pub fn ssgi_settings() -> ScreenSpaceGiSettings {
    with_gather_switch(|state| state.settings)
}

/// Replaces the screen-space GI gather settings. Quarter stays off unless `settings` turns it on.
pub fn set_ssgi_settings(settings: ScreenSpaceGiSettings) {
    with_gather_switch(|state| state.settings = settings);
}

/// Whether the next gather runs at quarter resolution.
#[must_use]
pub fn ssgi_quarter_gather() -> bool {
    ssgi_settings().quarter_gather
}

/// Records the texel step the gather just wrote into `noise.y`.
pub(crate) fn note_ssgi_gather_step(step: u32) {
    with_gather_switch(|state| state.used_step = step);
}

/// The texel step the last screen-space GI gather wrote (`1`, `2` or `4`). `0` before any gather.
#[must_use]
pub fn ssgi_gather_step_used() -> u32 {
    with_gather_switch(|state| state.used_step)
}

/// Restores the previous [`ScreenSpaceGiSettings`] when dropped.
pub struct SsgiGatherGuard {
    previous: ScreenSpaceGiSettings,
}

impl SsgiGatherGuard {
    /// Turns quarter gather on or off for the guard's lifetime.
    #[must_use]
    pub fn quarter(on: bool) -> Self {
        let previous = ssgi_settings();
        set_ssgi_settings(ScreenSpaceGiSettings { quarter_gather: on });
        Self { previous }
    }
}

impl Drop for SsgiGatherGuard {
    fn drop(&mut self) {
        set_ssgi_settings(self.previous);
    }
}

/// Packed probe for the forward shader. Layout matches `GiProbe` in `shaders/gi.wgsl`.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct GpuProbe {
    /// Red L1 SH.
    pub sh_r: [f32; 4],
    /// Green L1 SH.
    pub sh_g: [f32; 4],
    /// Blue L1 SH.
    pub sh_b: [f32; 4],
    /// 4×4 mean hit distance, row-major vec4s.
    pub depth_mean: [[f32; 4]; 4],
    /// 4×4 mean squared hit distance.
    pub depth_msq: [[f32; 4]; 4],
}

/// Grid uniform. Layout matches `GiGrid` in `shaders/gi.wgsl`.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct GpuGiGrid {
    /// `xyz` origin, `w` intensity.
    pub origin: [f32; 4],
    /// `xyz` spacing, `w` normal bias.
    pub spacing: [f32; 4],
    /// `xyz` dimensions, `w` probe count.
    pub dims: [u32; 4],
}

impl GiField {
    /// Packs probes in shader order.
    #[must_use]
    pub fn gpu_probes(&self) -> Vec<GpuProbe> {
        let mut out = Vec::with_capacity(self.probes.len());
        self.gpu_probes_into(&mut out);
        out
    }

    /// [`Self::gpu_probes`] appended to `out`, which keeps its allocation between frames.
    pub fn gpu_probes_into(&self, out: &mut Vec<GpuProbe>) {
        out.extend(self.probes.iter().map(|probe| {
            let pack = |src: &[f32; 16]| -> [[f32; 4]; 4] {
                let mut rows = [[0.0; 4]; 4];
                for row in 0..4 {
                    rows[row] = [src[row * 4], src[row * 4 + 1], src[row * 4 + 2], src[row * 4 + 3]];
                }
                rows
            };
            GpuProbe {
                sh_r: probe.sh[0],
                sh_g: probe.sh[1],
                sh_b: probe.sh[2],
                depth_mean: pack(&probe.depth_mean),
                depth_msq: pack(&probe.depth_msq),
            }
        }));
    }

    /// Grid uniform for the current field.
    #[must_use]
    pub fn gpu_grid(&self) -> GpuGiGrid {
        GpuGiGrid {
            origin: [self.origin[0], self.origin[1], self.origin[2], self.intensity],
            spacing: [
                self.spacing[0],
                self.spacing[1],
                self.spacing[2],
                self.normal_bias,
            ],
            dims: [self.dims[0], self.dims[1], self.dims[2], self.probes.len() as u32],
        }
    }
}

/// Rust and WGSL struct rows of this module (`shader_contract`, ADR-3002).
pub(crate) fn contract_rows() -> Vec<crate::ContractRow> {
    vec![
        crate::contract_row!(
            "lab_forward.wgsl,lab_forward_graph.wgsl,vis_resolve.wgsl",
            "GiGrid",
            GpuGiGrid {
                origin,
                spacing,
                dims
            }
        ),
        crate::contract_row!(
            "lab_forward.wgsl,lab_forward_graph.wgsl,vis_resolve.wgsl",
            "GiProbe",
            GpuProbe {
                sh_r,
                sh_g,
                sh_b,
                depth_mean,
                depth_msq
            }
        ),
    ]
}

#[cfg(test)]
thread_local! {
    static PROBE_BUFFER_GROWS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn note_probe_buffer_grow() {
    PROBE_BUFFER_GROWS.with(|cell| cell.set(cell.get() + 1));
}

#[cfg(test)]
fn probe_buffer_grows() -> u64 {
    PROBE_BUFFER_GROWS.with(std::cell::Cell::get)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sdf::{SdfScene, SdfShape};

    #[test]
    fn gi_field_rejects_bad_grids() {
        let origin = [0.0; 3];
        assert!(matches!(
            GiField::new(origin, [0.0, 1.0, 1.0], [1, 1, 1]),
            Err(GiError::BadGrid)
        ));
        assert!(matches!(
            GiField::new(origin, [f32::NAN, 1.0, 1.0], [1, 1, 1]),
            Err(GiError::BadGrid)
        ));
        assert!(matches!(
            GiField::new(origin, [1.0; 3], [0, 1, 1]),
            Err(GiError::BadGrid)
        ));
        assert!(matches!(
            GiField::new(origin, [1.0; 3], [257, 1, 1]),
            Err(GiError::TooManyProbes { count: 257 })
        ));
        assert!(GiField::new(origin, [1.0; 3], [256, 1, 1]).is_ok());
    }

    #[test]
    fn grid_product_overflow_is_rejected_not_wrapped() {
        // 2^22 cubed is 2^66, which wraps to 0 in usize and used to build an empty field.
        assert!(matches!(
            GiField::new([0.0; 3], [1.0; 3], [4_194_304; 3]),
            Err(GiError::TooManyProbes { .. })
        ));
    }

    fn cornell() -> SdfScene {
        let wall = |center, half, albedo| SdfShape::Box {
            center,
            half,
            albedo,
            emissive: [0.0; 3],
        };
        let mut scene = SdfScene::default();
        // Left wall (red), right wall (green), floor, back wall. Open toward +Z.
        scene
            .shapes
            .push(wall([-1.05, 1.0, 0.0], [0.05, 1.0, 1.0], [0.8, 0.05, 0.05]));
        scene
            .shapes
            .push(wall([1.05, 1.0, 0.0], [0.05, 1.0, 1.0], [0.05, 0.8, 0.05]));
        scene
            .shapes
            .push(wall([0.0, -0.05, 0.0], [1.0, 0.05, 1.0], [0.7, 0.7, 0.7]));
        scene
            .shapes
            .push(wall([0.0, 1.0, -1.05], [1.0, 1.0, 0.05], [0.7, 0.7, 0.7]));
        scene
            .shapes
            .push(wall([0.0, 2.05, 0.0], [1.0, 0.05, 1.0], [0.7, 0.7, 0.7]));
        scene
    }

    #[test]
    fn a_zero_variance_cell_does_not_blacken_a_wall_fifteen_centimetres_past_the_ray() {
        // Cockpit left wall, one ray in the cell: mean 0.717, receiver at 0.886.
        let mean = 0.717_f32;
        let island = chebyshev(mean, mean * mean, 0.886);
        assert!(
            island > 0.15,
            "the 0.15 reject must keep this wall, visibility {island}"
        );
        // A metre past the mean is a real occluder (the closed-room sky block).
        let blocked = chebyshev(1.0, 1.0, 2.0);
        assert!(
            blocked < 0.15,
            "a meter of occlusion must stay rejected, visibility {blocked}"
        );
    }

    #[test]
    fn constant_radiance_sh_is_one_after_the_pi_divide() {
        // c00 for radiance 1 is Y00 * 4π. Linear terms of a constant are 0.
        let sh = [Y00 * 4.0 * PI, 0.0, 0.0, 0.0];
        let up = irradiance_over_pi(sh, [0.0, 1.0, 0.0]);
        let side = irradiance_over_pi(sh, [1.0, 0.0, 0.0]);
        assert!((up - 1.0).abs() < 1.0e-4, "{up}");
        assert!((side - 1.0).abs() < 1.0e-4, "{side}");
    }

    #[test]
    fn cornell_probes_bleed_red_toward_the_red_wall_and_not_without_it() {
        let scene = cornell();
        let mut field = GiField::new([-0.6, 0.45, -0.6], [0.6, 0.7, 0.6], [3, 2, 3]).expect("grid");
        // Light travels toward -X so the red wall (normal +X) is the lit coloured wall.
        // A daylight sun: escaping rays see the physically scaled sky (`SKY_IBL_SCALE`),
        // so the sun must be in the same photometric units for its bounce to register.
        let sun = ProbeSun {
            direction: [-0.55, -0.75, -0.25],
            color: [1.0, 1.0, 1.0],
            illuminance: 100_000.0,
        };
        update_irradiance(&mut field, &scene, sun, 48, 0.0);
        let left = sample_irradiance(&field, [-0.4, 0.8, -0.7], [0.0, 0.0, 1.0]);
        let right = sample_irradiance(&field, [0.4, 0.8, -0.7], [0.0, 0.0, 1.0]);
        assert!(
            left[0] > left[1] && left[0] > right[0],
            "left of the back wall should be the red side, left {left:?} right {right:?}"
        );
        let mut bare = SdfScene::default();
        bare.shapes.push(SdfShape::Box {
            center: [0.0, -0.05, 0.0],
            half: [1.0, 0.05, 1.0],
            albedo: [0.7, 0.7, 0.7],
            emissive: [0.0; 3],
        });
        let mut white = GiField::new([-0.6, 0.45, -0.6], [0.6, 0.7, 0.6], [3, 2, 3]).expect("grid");
        update_irradiance(&mut white, &bare, sun, 48, 0.0);
        let left_white = sample_irradiance(&white, [-0.4, 0.8, -0.7], [0.0, 0.0, 1.0]);
        assert!(
            left[0] - left[1] > left_white[0] - left_white[1] + 0.002,
            "removing the red wall removes the tint, with {left:?} without {left_white:?}"
        );
    }

    #[test]
    fn a_closed_box_blocks_the_sun_inside() {
        let mut closed = SdfScene::default();
        closed.shapes.push(SdfShape::Box {
            center: [0.0, 0.0, 0.0],
            half: [1.0, 1.0, 1.0],
            albedo: [0.6, 0.6, 0.6],
            emissive: [0.0; 3],
        });
        // Hollow the box by not using a solid box: six thin walls.
        closed.shapes.clear();
        let wall = |center, half| SdfShape::Box {
            center,
            half,
            albedo: [0.6, 0.55, 0.5],
            emissive: [0.0; 3],
        };
        closed.shapes.push(wall([0.0, -1.0, 0.0], [1.0, 0.05, 1.0]));
        closed.shapes.push(wall([0.0, 1.0, 0.0], [1.0, 0.05, 1.0]));
        closed.shapes.push(wall([-1.0, 0.0, 0.0], [0.05, 1.0, 1.0]));
        closed.shapes.push(wall([1.0, 0.0, 0.0], [0.05, 1.0, 1.0]));
        closed.shapes.push(wall([0.0, 0.0, -1.0], [1.0, 1.0, 0.05]));
        closed.shapes.push(wall([0.0, 0.0, 1.0], [1.0, 1.0, 0.05]));
        let sun = ProbeSun {
            direction: [0.2, -1.0, 0.1],
            color: [1.0, 1.0, 1.0],
            illuminance: 20.0,
        };
        // Probes outside (y = 2) and inside (y = 0).
        let mut field = GiField::new([0.0, 0.0, 0.0], [0.01, 2.0, 0.01], [1, 2, 1]).expect("grid");
        field.normal_bias = 0.02;
        update_irradiance(&mut field, &closed, sun, 48, 0.0);
        let inside_probe = field.probes[0].sh.iter().flatten().map(|c| c.abs()).sum::<f32>();
        let outside_probe = field.probes[1].sh.iter().flatten().map(|c| c.abs()).sum::<f32>();
        assert!(
            inside_probe < outside_probe * 0.35,
            "the interior probe must not store the sky, inside {inside_probe} outside {outside_probe}"
        );
        let inside = sample_irradiance(&field, [0.0, 0.2, 0.0], [0.0, 1.0, 0.0]);
        let open = SdfScene::default();
        let mut open_field = GiField::new([0.0, 0.0, 0.0], [0.01, 2.0, 0.01], [1, 2, 1]).expect("grid");
        open_field.normal_bias = 0.02;
        update_irradiance(&mut open_field, &open, sun, 48, 0.0);
        let sky = sample_irradiance(&open_field, [0.0, 0.2, 0.0], [0.0, 1.0, 0.0]);
        let lin = |c: [f32; 3]| c[0] + c[1] + c[2];
        assert!(
            lin(inside) < lin(sky) * 0.55,
            "the box must block the sun, inside {inside:?} open {sky:?}"
        );
    }

    #[test]
    fn hysteresis_keeps_part_of_the_previous_probe() {
        let scene = cornell();
        let sun = ProbeSun {
            direction: [0.0, -1.0, 0.0],
            color: [1.0; 3],
            illuminance: 4.0,
        };
        let mut field = GiField::new([0.0, 0.8, 0.0], [1.0, 1.0, 1.0], [1, 1, 1]).expect("grid");
        update_irradiance(&mut field, &scene, sun, 16, 0.0);
        let before = field.probes[0].sh[0][0];
        // A closed box around the probe: no sun, no sky. (An empty SDF is an open sky
        // since escaping rays carry the physical sky radiance, not darkness.)
        let mut dark = SdfScene::default();
        for (center, half) in [
            ([0.0, -0.2, 0.0], [1.5, 0.1, 1.5]),
            ([0.0, 1.8, 0.0], [1.5, 0.1, 1.5]),
            ([-1.4, 0.8, 0.0], [0.1, 1.5, 1.5]),
            ([1.4, 0.8, 0.0], [0.1, 1.5, 1.5]),
            ([0.0, 0.8, -1.4], [1.5, 1.5, 0.1]),
            ([0.0, 0.8, 1.4], [1.5, 1.5, 0.1]),
        ] {
            dark.shapes.push(SdfShape::Box {
                center,
                half,
                albedo: [0.05; 3],
                emissive: [0.0; 3],
            });
        }
        update_irradiance(&mut field, &dark, sun, 16, 0.5);
        let after = field.probes[0].sh[0][0];
        assert!(
            (after - before).abs() > 1.0e-5
                && (after - before * 0.5).abs() < (before - after).abs() + before.abs(),
            "history must move the coefficient but not erase it, before {before} after {after}"
        );
        assert!(
            (after - before).abs() < before.abs().max(1.0),
            "half history cannot jump past the old value by its magnitude"
        );
    }

    fn one_probe(origin: [f32; 3]) -> GiField {
        GiField::new(origin, [1.0, 1.0, 1.0], [1, 1, 1]).expect("grid")
    }

    fn noon() -> ProbeSun {
        ProbeSun {
            direction: [0.0, -1.0, 0.0],
            color: [1.0; 3],
            illuminance: 4.0,
        }
    }

    /// Hysteresis 1 is stored as 0.99. Depth moments follow the new trace. A probe
    /// inside a solid is cleared to 0.05 m depth texels.
    #[test]
    fn hysteresis_one_keeps_ninety_nine_percent_and_replaces_depth() {
        let sun = noon();
        let open = SdfScene::default();
        let mut field = one_probe([0.0, 1.0, 0.0]);
        update_irradiance(&mut field, &open, sun, 16, 0.0);
        let old = field.probes[0].clone();

        let mut closer = SdfScene::default();
        closer.shapes.push(SdfShape::Box {
            center: [0.0, 0.0, 0.0],
            half: [4.0, 0.05, 4.0],
            albedo: [0.5; 3],
            emissive: [0.0; 3],
        });
        let mut fresh = field.clone();
        update_irradiance(&mut fresh, &closer, sun, 16, 0.0);
        let new = fresh.probes[0].clone();

        update_irradiance(&mut field, &closer, sun, 16, 1.0);
        let blended = &field.probes[0];
        // The tracer stores `hysteresis.clamp(0.0, 0.99)` and weights the new
        // sample by `(1.0 - hysteresis)`. That weight is not the f32 value `0.01`.
        let hysteresis = 1.0f32.clamp(0.0, 0.99);
        let mut moved = false;
        for c in 0..3 {
            for k in 0..4 {
                let expected = old.sh[c][k] * hysteresis + new.sh[c][k] * (1.0 - hysteresis);
                assert_eq!(
                    blended.sh[c][k], expected,
                    "sh[{c}][{k}] must be previous * 0.99 + new * (1 - 0.99)"
                );
                if (new.sh[c][k] - old.sh[c][k]).abs() > 1.0e-3 {
                    moved = true;
                }
            }
        }
        assert!(moved, "the closer floor must change the trace");
        assert_eq!(blended.depth_mean, new.depth_mean);
        assert_eq!(blended.depth_msq, new.depth_msq);

        let mut solid = SdfScene::default();
        solid.shapes.push(SdfShape::Box {
            center: [0.0, 1.0, 0.0],
            half: [2.0, 2.0, 2.0],
            albedo: [0.5; 3],
            emissive: [0.0; 3],
        });
        let mut buried = one_probe([0.0, 1.0, 0.0]);
        update_irradiance(&mut buried, &open, sun, 16, 0.0);
        assert!(
            buried.probes[0].sh[0][0].abs() > 0.0,
            "the open trace is not black"
        );
        update_irradiance(&mut buried, &solid, sun, 16, 1.0);
        assert_eq!(buried.probes[0].sh, [[0.0; 4]; 3]);
        assert_eq!(buried.probes[0].depth_mean, [0.05; 16]);
        assert_eq!(buried.probes[0].depth_msq, [0.05 * 0.05; 16]);
    }

    /// Counts outside 8..=128 trace the clamped count, so 4 matches 8 and 256 matches 128.
    #[test]
    fn ray_counts_outside_the_clamp_match_the_clamped_trace() {
        let scene = SdfScene::default();
        let sun = noon();
        let mut few = one_probe([0.0, 1.0, 0.0]);
        let mut eight = few.clone();
        update_irradiance(&mut few, &scene, sun, 4, 0.0);
        update_irradiance(&mut eight, &scene, sun, 8, 0.0);
        assert_eq!(few.probes[0].sh, eight.probes[0].sh);
        assert_eq!(few.probes[0].depth_mean, eight.probes[0].depth_mean);
        let mut many = one_probe([0.0, 1.0, 0.0]);
        let mut cap = many.clone();
        update_irradiance(&mut many, &scene, sun, 256, 0.0);
        update_irradiance(&mut cap, &scene, sun, 128, 0.0);
        assert_eq!(many.probes[0].sh, cap.probes[0].sh);
        assert_eq!(many.probes[0].depth_mean, cap.probes[0].depth_mean);
    }

    #[test]
    fn screen_space_gather_picks_up_a_red_neighbour() {
        let row = [
            ScreenTexel {
                depth: 0.5,
                color: [1.0, 0.0, 0.0],
            },
            ScreenTexel {
                depth: 0.5,
                color: [1.0, 1.0, 1.0],
            },
            ScreenTexel {
                depth: 0.5,
                color: [0.0, 0.0, 1.0],
            },
        ];
        let gathered = screen_space_gather(&row, 0.5, 1);
        assert!(
            (gathered[0] - gathered[2]).abs() < 1.0e-4 && gathered[0] > 0.4,
            "equal-distance red and blue neighbors must both contribute, {gathered:?}"
        );
        let red_side = screen_space_gather(
            &[
                ScreenTexel {
                    depth: 0.5,
                    color: [1.0, 0.0, 0.0],
                },
                ScreenTexel {
                    depth: 0.5,
                    color: [1.0, 1.0, 1.0],
                },
                ScreenTexel {
                    depth: 0.5,
                    color: [0.0, 0.0, 0.0],
                },
            ],
            0.5,
            1,
        );
        assert!(
            red_side[0] > red_side[2] + 0.4,
            "a red neighbor tints the gather, {red_side:?}"
        );
        let hidden = screen_space_gather(
            &[
                ScreenTexel {
                    depth: 0.0,
                    color: [1.0, 0.0, 0.0],
                },
                ScreenTexel {
                    depth: 0.5,
                    color: [1.0, 1.0, 1.0],
                },
            ],
            0.5,
            1,
        );
        assert!(hidden[0] < 0.2, "a much closer neighbor is occluded, {hidden:?}");
    }

    #[test]
    fn heightfield_and_mesh_occlude_a_probe_under_the_ground() {
        // Closed outward cube, off to the side so the probe is not inside the volume.
        let mut vertices = [
            [-0.3, -0.3, -0.3],
            [0.3, -0.3, -0.3],
            [0.3, 0.3, -0.3],
            [-0.3, 0.3, -0.3],
            [-0.3, -0.3, 0.3],
            [0.3, -0.3, 0.3],
            [0.3, 0.3, 0.3],
            [-0.3, 0.3, 0.3],
        ];
        for vertex in &mut vertices {
            vertex[0] += 2.5;
            vertex[1] += 0.4;
        }
        let indices = [
            0, 2, 1, 0, 3, 2, 4, 5, 6, 4, 6, 7, 0, 5, 4, 0, 1, 5, 3, 6, 2, 3, 7, 6, 0, 7, 3, 0, 4, 7, 1, 6,
            5, 1, 2, 6,
        ];
        let mesh = crate::sdf::MeshSdf::from_triangles(&vertices, &indices, 6, [0.8, 0.2, 0.2]);
        let ground = crate::sdf::HeightfieldGrid {
            origin_x: -4.0,
            origin_z: -4.0,
            spacing_x: 4.0,
            spacing_z: 4.0,
            width: 2,
            height: 2,
            samples: vec![0.0; 4],
            albedo: [0.3, 0.7, 0.3],
        };
        let mut scene = SdfScene::default();
        scene.shapes.push(SdfShape::Mesh(mesh));
        scene.shapes.push(SdfShape::Heightfield(ground));
        let sun = ProbeSun {
            direction: [0.2, -1.0, 0.1],
            color: [1.0; 3],
            illuminance: 8.0,
        };
        let mut above = GiField::new([0.0, 1.2, 2.0], [1.0, 1.0, 1.0], [1, 1, 1]).expect("grid");
        update_irradiance(&mut above, &scene, sun, 32, 0.0);
        let mut below = GiField::new([0.0, -1.0, 2.0], [1.0, 1.0, 1.0], [1, 1, 1]).expect("grid");
        update_irradiance(&mut below, &scene, sun, 32, 0.0);
        let lit = sample_irradiance(&above, [0.0, 1.5, 2.0], [0.0, -1.0, 0.0]);
        let buried = sample_irradiance(&below, [0.0, -1.3, 2.0], [0.0, 1.0, 0.0]);
        let energy = |c: [f32; 3]| c[0] + c[1] + c[2];
        assert!(
            energy(lit).is_finite() && energy(lit) > 0.05,
            "the open probe sees the ground and sky, {lit:?}"
        );
        assert!(
            energy(buried) < energy(lit) * 0.25,
            "a probe under the heightfield is solid, buried {buried:?} lit {lit:?}"
        );
    }

    fn room() -> SdfScene {
        let wall = |center: [f32; 3], half: [f32; 3]| SdfShape::Box {
            center,
            half,
            albedo: [0.7, 0.68, 0.62],
            emissive: [0.0; 3],
        };
        let mut scene = SdfScene::default();
        scene.shapes.push(wall([0.0, -1.05, 0.0], [1.3, 0.05, 1.3]));
        scene.shapes.push(wall([0.0, 1.05, 0.0], [1.3, 0.05, 1.3]));
        scene.shapes.push(wall([-1.05, 0.0, 0.0], [0.05, 1.1, 1.3]));
        scene.shapes.push(wall([1.05, 0.0, 0.0], [0.05, 1.1, 1.3]));
        scene.shapes.push(wall([0.0, 0.0, -1.05], [1.3, 1.1, 0.05]));
        scene.shapes.push(wall([0.0, 0.0, 1.05], [1.3, 1.1, 0.05]));
        scene
    }

    #[test]
    fn a_local_light_bounces_onto_the_facing_wall_and_a_miss_stays_black() {
        let scene = room();
        let sun = ProbeSun {
            direction: [0.0, -1.0, 0.0],
            color: [1.0; 3],
            illuminance: 0.0,
        };
        // Warm point near the -X wall. The +X wall faces it and should pick up the bounce.
        let light = ProbeLocalLight {
            position: [-0.55, 0.0, 0.0],
            direction: [0.0, -1.0, 0.0],
            color: [1.0, 0.45, 0.2],
            candela: 400.0,
            radius: 4.0,
            inner_cos: 1.0,
            outer_cos: 0.0,
            spot: false,
        };
        let mut field = GiField::new([0.0, 0.0, 0.0], [1.0, 1.0, 1.0], [1, 1, 1]).expect("grid");
        update_irradiance_with_lights(&mut field, &scene, sun, &[light], false, 48, 0.0);
        // Normal points back toward the lit -X side, so the sample sees that wall's outgoing light.
        let bounced = sample_irradiance(&field, [0.7, 0.0, 0.0], [-1.0, 0.0, 0.0]);
        let energy = |c: [f32; 3]| c[0] + c[1] + c[2];
        assert!(
            energy(bounced) > 0.05 && bounced[0] > bounced[2] + 0.01,
            "the warm light must tint the facing wall, {bounced:?}"
        );
        let mut dark = GiField::new([0.0, 0.0, 0.0], [1.0, 1.0, 1.0], [1, 1, 1]).expect("grid");
        update_irradiance_with_lights(&mut dark, &scene, sun, &[], false, 48, 0.0);
        let bare = sample_irradiance(&dark, [0.7, 0.0, 0.0], [-1.0, 0.0, 0.0]);
        assert!(
            energy(bare) < energy(bounced) * 0.15,
            "an interior miss is black, not sky, bare {bare:?} bounced {bounced:?}"
        );
        let mut blocked_scene = scene.clone();
        blocked_scene.shapes.push(SdfShape::Box {
            center: [0.0, 0.0, 0.0],
            half: [0.04, 1.2, 1.2],
            albedo: [0.2, 0.2, 0.2],
            emissive: [0.0; 3],
        });
        let mut blocked = GiField::new([0.55, 0.0, 0.0], [1.0, 1.0, 1.0], [1, 1, 1]).expect("grid");
        update_irradiance_with_lights(&mut blocked, &blocked_scene, sun, &[light], false, 48, 0.0);
        let hidden = sample_irradiance(&blocked, [0.8, 0.0, 0.0], [-1.0, 0.0, 0.0]);
        assert!(
            energy(hidden) < energy(bounced) * 0.45,
            "a wall between the light and the receiver cuts the bounce, hidden {hidden:?} open {bounced:?}"
        );
    }

    /// Two updates after the field already holds irradiance must allocate the previous-bounce
    /// probe buffer once. The first update stores sun-only SH and does not copy.
    #[test]
    fn bounce_probe_buffer_grows_once() {
        let scene = cornell();
        let mut field = GiField::new([-0.6, 0.45, -0.6], [0.6, 0.7, 0.6], [3, 2, 3]).expect("grid");
        let sun = ProbeSun {
            direction: [-0.55, -0.75, -0.25],
            color: [1.0, 1.0, 1.0],
            illuminance: 100_000.0,
        };
        update_irradiance(&mut field, &scene, sun, 32, 0.0);
        assert!(
            field
                .probes
                .iter()
                .any(|probe| probe.sh.iter().any(|c| c[0] != 0.0)),
            "the first update must store irradiance or the bounce buffer is never copied"
        );
        let base = probe_buffer_grows();
        update_irradiance(&mut field, &scene, sun, 32, 0.0);
        update_irradiance(&mut field, &scene, sun, 32, 0.0);
        let grew = probe_buffer_grows() - base;
        assert_eq!(
            grew, 1,
            "previous-bounce probe buffer grew {grew} times across two updates (want 1)"
        );
        let sample = sample_irradiance(&field, [-0.2, 0.6, -0.4], [0.0, 1.0, 0.0]);
        assert!(
            sample.iter().all(|c| c.is_finite()) && sample.iter().any(|c| *c > 0.0),
            "the updates must stay finite and lit, got {sample:?}"
        );
    }

    /// Six thin walls around the origin: every direction hits within about a metre and the
    /// probe at the origin stays in free space.
    fn closed_shell() -> SdfScene {
        let wall = |center, half| SdfShape::Box {
            center,
            half,
            albedo: [0.8, 0.8, 0.8],
            emissive: [0.0; 3],
        };
        let mut scene = SdfScene::default();
        scene.shapes.push(wall([0.0, -1.0, 0.0], [1.0, 0.05, 1.0]));
        scene.shapes.push(wall([0.0, 1.0, 0.0], [1.0, 0.05, 1.0]));
        scene.shapes.push(wall([-1.0, 0.0, 0.0], [0.05, 1.0, 1.0]));
        scene.shapes.push(wall([1.0, 0.0, 0.0], [0.05, 1.0, 1.0]));
        scene.shapes.push(wall([0.0, 0.0, -1.0], [1.0, 1.0, 0.05]));
        scene.shapes.push(wall([0.0, 0.0, 1.0], [1.0, 1.0, 0.05]));
        scene
    }

    #[test]
    fn eight_rays_do_not_store_unsampled_depth_cells_as_a_twelve_metre_miss() {
        let scene = closed_shell();
        let mut field = GiField::new([0.0; 3], [1.0; 3], [1, 1, 1]).expect("grid");
        let sun = ProbeSun {
            direction: [0.3, -1.0, 0.2],
            color: [1.0; 3],
            illuminance: 50_000.0,
        };
        // The minimum ray count touches at most eight of the sixteen octahedral cells.
        update_irradiance(&mut field, &scene, sun, 8, 0.0);
        let probe = &field.probes[field.index(0, 0, 0)];
        assert!(
            probe.depth_mean.iter().all(|&m| m < 2.0),
            "an unsampled cell kept the 12 m miss and shows a receiver everything: {:?}",
            probe.depth_mean
        );
        // A receiver a metre away along a direction no ray traced must be dropped,
        // not lit through the shell.
        let touched: std::collections::HashSet<usize> =
            (0..8).map(|i| oct_cell(fibonacci_direction(i, 8))).collect();
        let dir = (0..1024)
            .map(|i| fibonacci_direction(i, 1024))
            .find(|d| !touched.contains(&oct_cell(*d)))
            .expect("an untraced direction");
        let e = sample_irradiance(&field, dir, [-dir[0], -dir[1], -dir[2]]);
        assert_eq!(e, [0.0; 3], "the shell must occlude an untraced direction");
    }

    #[test]
    fn a_buried_probe_is_dropped_instead_of_darkening_a_facing_receiver() {
        // A small solid around probe 0 buries it; probe 1 a room away holds daylight.
        let mut scene = SdfScene::default();
        scene.shapes.push(SdfShape::Sphere {
            center: [0.0; 3],
            radius: 0.1,
            albedo: [0.5; 3],
            emissive: [0.0; 3],
        });
        let sun = ProbeSun {
            direction: [0.2, -1.0, 0.1],
            color: [1.0; 3],
            illuminance: 100_000.0,
        };
        let mut both = GiField::new([0.0; 3], [0.4, 1.0, 1.0], [2, 1, 1]).expect("grid");
        update_irradiance(&mut both, &scene, sun, 32, 0.0);
        let buried_probe = &both.probes[both.index(0, 0, 0)];
        assert!(
            buried_probe.sh.iter().flatten().all(|&c| c == 0.0)
                && buried_probe.depth_mean.iter().all(|&m| m <= 0.05 + 1.0e-6),
            "probe 0 sits inside the sphere"
        );
        assert!(
            both.probes[both.index(1, 0, 0)]
                .sh
                .iter()
                .flatten()
                .any(|&c| c != 0.0),
            "probe 1 holds daylight"
        );
        // Between the probes, 20 cm above the buried one, facing it.
        let with_buried = sample_irradiance(&both, [0.2, 0.2, 0.0], [0.0, -1.0, 0.0]);
        let mut alone = GiField::new([0.4, 0.0, 0.0], [0.4, 1.0, 1.0], [1, 1, 1]).expect("grid");
        update_irradiance(&mut alone, &scene, sun, 32, 0.0);
        let daylight = sample_irradiance(&alone, [0.2, 0.2, 0.0], [0.0, -1.0, 0.0]);
        for c in 0..3 {
            assert!(
                (with_buried[c] - daylight[c]).abs() < 1.0e-4,
                "the buried probe must not pull the average toward black: with {with_buried:?} \
                 without {daylight:?}"
            );
        }
    }

    #[test]
    fn intensity_scales_each_bounce_once() {
        // The Cornell room lets the sun in through its open +Z side, so the second update
        // stores a real indirect term; a fully sealed room with a down sun is black.
        let scene = cornell();
        let sun = ProbeSun {
            direction: [-0.55, -0.75, -0.25],
            color: [1.0; 3],
            illuminance: 100_000.0,
        };
        let mut at_one = GiField::new([-0.6, 0.45, -0.6], [0.6, 0.7, 0.6], [3, 2, 3]).expect("grid");
        let mut at_two = at_one.clone();
        at_two.intensity = 2.0;
        // Two updates light the second bounce; hysteresis 0 keeps the arithmetic exact.
        update_irradiance(&mut at_one, &scene, sun, 48, 0.0);
        update_irradiance(&mut at_one, &scene, sun, 48, 0.0);
        update_irradiance(&mut at_two, &scene, sun, 48, 0.0);
        update_irradiance(&mut at_two, &scene, sun, 48, 0.0);
        let one = sample_irradiance(&at_one, [-0.4, 0.8, -0.7], [0.0, 0.0, 1.0]);
        let two = sample_irradiance(&at_two, [-0.4, 0.8, -0.7], [0.0, 0.0, 1.0]);
        for c in 0..3 {
            assert!(one[c] > 1.0e-3, "the sample must not be vacuously black, {one:?}");
            assert!(
                (two[c] - 2.0 * one[c]).abs() < 1.0e-3 * two[c].max(1.0),
                "intensity 2 must scale the whole irradiance twice, not the later bounces four \
                 times: one {one:?} two {two:?}"
            );
        }
    }

    #[test]
    fn quarter_gather_defaults_off_and_the_step_is_1_2_or_4() {
        let off = ScreenSpaceGiSettings::default();
        assert!(!off.quarter_gather);
        assert_eq!(ssgi_gather_step(false, off), SSGI_GATHER_NATIVE);
        assert_eq!(ssgi_gather_step(true, off), SSGI_GATHER_HALF);
        let on = ScreenSpaceGiSettings { quarter_gather: true };
        assert_eq!(ssgi_gather_step(false, on), SSGI_GATHER_QUARTER);
        assert_eq!(ssgi_gather_step(true, on), SSGI_GATHER_QUARTER);
        // The GI plates are 160×96: half is 80×48, quarter is 40×24.
        assert_eq!(ssgi_gather_size(160, 96, 1), (160, 96));
        assert_eq!(ssgi_gather_size(160, 96, 2), (80, 48));
        assert_eq!(ssgi_gather_size(160, 96, 4), (40, 24));
        // Odd full sizes round up the same way `div_ceil` does for the half gather today.
        assert_eq!(ssgi_gather_size(1919, 1079, 2), (960, 540));
        assert_eq!(ssgi_gather_size(1919, 1079, 4), (480, 270));
        assert_eq!(ssgi_gather_size(0, 0, 4), (1, 1));
        assert_eq!(ssgi_gather_size(160, 96, 3), (160, 96));
        assert_eq!(ssgi_gather_size(160, 96, 0), (160, 96));
    }

    #[test]
    fn upsample_low_coord_lands_on_the_gather_texel() {
        // Gather fragment 0 at step 2 samples full pixel 1; that pixel's low coordinate is texel 0.
        let step2 = ssgi_upsample_low_coord(1.0, SSGI_GATHER_HALF);
        assert!((step2 - 0.25).abs() < 1.0e-6, "{step2}");
        assert_eq!(step2.floor() as i32, 0);
        // Step 4 samples full pixel 2; the low coordinate is still texel 0.
        let step4 = ssgi_upsample_low_coord(2.0, SSGI_GATHER_QUARTER);
        assert!((step4 - 0.125).abs() < 1.0e-6, "{step4}");
        assert_eq!(step4.floor() as i32, 0);
        // Native step keeps the pixel on its own texel.
        let native = ssgi_upsample_low_coord(3.0, SSGI_GATHER_NATIVE);
        assert!((native - 3.0).abs() < 1.0e-6, "{native}");
    }

    /// A depth cell no ray of the current count reaches keeps the moments a larger ray count
    /// measured, pass after pass: the carried cell keeps its sample count (corr-low-render,
    /// corr-render-rest finding 18; it reset to the buried sentinel on the second low pass).
    #[test]
    fn carried_depth_moments_survive_more_than_one_low_ray_pass() {
        let scene = cornell();
        let mut field = GiField::new([0.0, 1.0, 0.0], [1.0; 3], [1, 1, 1]).expect("grid");
        let sun = ProbeSun {
            direction: [-0.55, -0.75, -0.25],
            color: [1.0, 1.0, 1.0],
            illuminance: 100_000.0,
        };
        update_irradiance(&mut field, &scene, sun, 128, 0.0);
        let measured = field.probes[0].depth_mean;
        let hit: Vec<usize> = (0..8).map(|i| oct_cell(fibonacci_direction(i, 8))).collect();
        let carried: Vec<usize> = (0..16)
            .filter(|c| !hit.contains(c) && field.probes[0].depth_n[*c] > 0)
            .collect();
        assert!(
            !carried.is_empty(),
            "8 rays reach every cell the 128-ray pass saw"
        );
        for pass in 0..2 {
            update_irradiance(&mut field, &scene, sun, 8, 0.0);
            for &c in &carried {
                assert_eq!(
                    field.probes[0].depth_mean[c], measured[c],
                    "pass {pass}: cell {c} lost its carried mean"
                );
            }
        }
    }
}
