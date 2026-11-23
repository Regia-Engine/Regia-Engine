// SPDX-License-Identifier: MPL-2.0
//! Light inputs of a bake beyond the sun and a uniform sky (`.bake.ron` v2, ADR-1095): an HDRI
//! sky ([`SkyHdriDesc`]: an equirect map, rotation, scale or a target irradiance, optional sun
//! extraction) and area emitters ([`EmitterDesc`]: quads, discs and spheres of uniform radiance
//! or of a given luminous power), plus [`LightInputs`], the report of what lights a bake and how
//! much (`regia bake-gi --dry-run` prints it as `plan.lights`).
//!
//! Emitters are traced as emissive triangles: probes see them directly and through bounces, and
//! next-event estimation samples them (MIS with BSDF rays), so their light lands in the
//! emissive component. They are black: they reflect nothing, and they occlude like any surface.

use std::path::{Path, PathBuf};

use regia_core::glam::{Mat4, Vec3};
use serde::{Deserialize, Serialize};

use crate::analytic::{lambert_emitter_luminance, lambert_emitter_power};
use crate::bake_desc::BakeDescError;
use crate::env_map::{EnvMap, ExtractedSun, integrate_sphere};
use crate::scene::{PtMaterial, PtMesh, PtScene, PtSky, PtSun};
use crate::scenes;

fn lum(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

fn white() -> [f32; 3] {
    [1.0; 3]
}

fn default_max_width() -> u32 {
    2048
}

fn default_sun_threshold() -> f32 {
    100.0
}

fn default_sun_max_solid_angle() -> f32 {
    0.03
}

fn default_disc_segments() -> u32 {
    64
}

fn default_sphere_segments() -> u32 {
    32
}

/// An equirect HDRI as the sky (`sky_hdri`, instead of `sky`). The mapping is
/// [`crate::env_map`]'s: top row the zenith, the image centre looking along −Z.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkyHdriDesc {
    /// The map (`.hdr`/`.pic` RGBE, or an uncompressed `.exr`), relative to the root.
    pub source: PathBuf,
    /// Turn about +Y, radians (positive turns +Z toward +X).
    #[serde(default)]
    pub rotation: f32,
    /// Multiplier from map values to cd/m² (default 1; not with `irradiance`).
    #[serde(default)]
    pub scale: Option<f32>,
    /// Instead of `scale`: scale the map so a surface facing up receives this many lux from it
    /// (and from the extracted sun, when there is one).
    #[serde(default)]
    pub irradiance: Option<f32>,
    /// Maps wider than this are box-filtered down by an integer factor (energy kept), pixels.
    #[serde(default = "default_max_width")]
    pub max_width: u32,
    /// Lift the sun out of the map into the bake's sun ([`EnvMap::extract_sun`]): its direct
    /// light stays live at runtime, its bounce is baked. Not with a `sun` entry.
    #[serde(default)]
    pub extract_sun: bool,
    /// Sun extraction cut: texels brighter than this × the map's mean luminance.
    #[serde(default = "default_sun_threshold")]
    pub sun_threshold: f32,
    /// Sun extraction refuses a region larger than this, steradians.
    #[serde(default = "default_sun_max_solid_angle")]
    pub sun_max_solid_angle: f32,
}

/// A planet whose atmosphere lights a bake ([`SkyAtmosphereDesc`]); the runtime's presets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
pub enum PlanetPreset {
    /// `AtmosphereParameters::earth`.
    Earth,
    /// `AtmosphereParameters::mars`.
    Mars,
    /// `AtmosphereParameters::airless`: a black sky.
    Airless,
}

impl PlanetPreset {
    /// The runtime's parameters for this planet.
    #[must_use]
    pub fn parameters(self) -> regia_render::AtmosphereParameters {
        match self {
            Self::Earth => regia_render::AtmosphereParameters::earth(),
            Self::Mars => regia_render::AtmosphereParameters::mars(),
            Self::Airless => regia_render::AtmosphereParameters::airless(),
        }
    }

    /// Lowercase name (`earth`, `mars`, `airless`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Earth => "earth",
            Self::Mars => "mars",
            Self::Airless => "airless",
        }
    }
}

fn default_atmosphere_height() -> f32 {
    2.0
}

/// The runtime's physical sky as the bake's sky (`sky_atmosphere`, instead of `sky` and
/// `sky_hdri`): the planet's sky-view table for the description's sun direction, the same table
/// the raster frame draws and lights with (`regia_pt::extract::atmosphere_sky`). Its level is
/// the planet's own sun at the top of the atmosphere (`AtmosphereParameters::sun_illuminance`).
#[derive(Clone, Copy, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkyAtmosphereDesc {
    /// The planet.
    pub planet: PlanetPreset,
    /// Height of the viewpoint above the ground, metres.
    #[serde(default = "default_atmosphere_height")]
    pub height: f32,
    /// Multiplier on the table (default 1: physical for the planet's sun).
    #[serde(default)]
    pub scale: Option<f32>,
}

/// Checks `sky_atmosphere` (key paths under `sky_atmosphere.`).
///
/// # Errors
///
/// `bake_desc_invalid` naming the first bad key.
pub fn validate_sky_atmosphere(a: &SkyAtmosphereDesc) -> Result<(), BakeDescError> {
    if !positive(a.height) {
        return Err(BakeDescError::invalid(
            "sky_atmosphere.height",
            "must be positive".into(),
        ));
    }
    if a.scale.is_some_and(|s| !(s >= 0.0 && s.is_finite())) {
        return Err(BakeDescError::invalid(
            "sky_atmosphere.scale",
            "must be zero or positive".into(),
        ));
    }
    Ok(())
}

/// The sky of `desc` for a sun toward `toward_sun`, and its report fields.
#[must_use]
pub fn atmosphere_sky(desc: &SkyAtmosphereDesc, toward_sun: [f32; 3]) -> (PtSky, SkyInput) {
    let _span = tracing::info_span!("pt_bake_atmosphere_sky", planet = desc.planet.name()).entered();
    let toward = Vec3::from_array(toward_sun).normalize_or(Vec3::Y).to_array();
    let mut sky = crate::extract::atmosphere_sky(&desc.planet.parameters(), toward, desc.height);
    let scale = desc.scale.unwrap_or(1.0);
    if let PtSky::Table { scale: s, .. } = &mut sky {
        *s = scale;
    }
    let report = SkyInput {
        kind: "table".into(),
        source: Some(format!("atmosphere:{}", desc.planet.name())),
        downsample: 1,
        scale,
        ..SkyInput::default()
    };
    (sky, report)
}

/// The shape of an area emitter (world space, metres).
#[derive(Clone, Copy, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum EmitterShape {
    /// The parallelogram `corner + s·edge_u + t·edge_v`, `s, t ∈ [0, 1]`; its front faces
    /// `edge_u × edge_v`.
    Quad {
        /// One corner.
        corner: [f32; 3],
        /// First edge from the corner.
        edge_u: [f32; 3],
        /// Second edge from the corner.
        edge_v: [f32; 3],
    },
    /// A disc facing `normal`, as a fan of `segments` triangles.
    Disc {
        /// Centre.
        center: [f32; 3],
        /// Front direction (normalized on load).
        normal: [f32; 3],
        /// Radius.
        radius: f32,
        /// Triangles in the fan (3..=4096).
        #[serde(default = "default_disc_segments")]
        segments: u32,
    },
    /// A sphere emitting outward, as a UV sphere of `segments` × `segments / 2` quads.
    Sphere {
        /// Centre.
        center: [f32; 3],
        /// Radius.
        radius: f32,
        /// Segments around (4..=1024).
        #[serde(default = "default_sphere_segments")]
        segments: u32,
    },
}

/// An area emitter (`emitters`): a shape of uniform radiance, given directly or as luminous
/// power (`radiance = color · power / (π · area · sides)`, `lum(color)` normalized to 1).
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmitterDesc {
    /// Name in reports and errors (default `emitters[<index>]`).
    #[serde(default)]
    pub id: Option<String>,
    /// The shape.
    pub shape: EmitterShape,
    /// Emitted radiance, linear cd/m² (exactly one of `radiance` and `power`).
    #[serde(default)]
    pub radiance: Option<[f32; 3]>,
    /// Luminous power, lumens, over all emitting sides.
    #[serde(default)]
    pub power: Option<f32>,
    /// Linear colour of a `power` emitter (normalized to luminance 1).
    #[serde(default = "white")]
    pub color: [f32; 3],
    /// Quads and discs: emit from both faces (spheres emit outward only).
    #[serde(default)]
    pub two_sided: bool,
}

/// One emitter as baked.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct EmitterReport {
    /// Its id.
    pub id: String,
    /// `quad`, `disc` or `sphere`.
    pub shape: String,
    /// Emitting area of the traced triangles, m².
    pub area: f32,
    /// Radiance, linear cd/m².
    pub radiance: [f32; 3],
    /// Luminous power over the emitting sides, lumens.
    pub power_lm: f32,
    /// Triangles traced.
    pub triangles: usize,
    /// Both faces emit.
    pub two_sided: bool,
}

/// The sun a bake sees.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct SunInput {
    /// Unit vector toward the sun.
    pub toward: [f32; 3],
    /// Illuminance at normal incidence, lux.
    pub illuminance: f32,
    /// Linear colour.
    pub color: [f32; 3],
    /// Angular radius, radians.
    pub angular_radius: f32,
    /// Lifted out of the HDRI (`sky_hdri.extract_sun`).
    pub from_hdri: bool,
}

/// The sky a bake sees.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct SkyInput {
    /// `none`, `uniform`, `table` (an atmosphere sky-view table) or `equirect`.
    pub kind: String,
    /// The HDRI file, relative to the root (`equirect` from a description).
    pub source: Option<String>,
    /// Size of the HDRI file, pixels.
    pub source_size: Option<[u32; 2]>,
    /// Size traced (after `max_width`), texels (0 × 0 for `none` and `uniform`).
    pub size: [u32; 2],
    /// Box-filter factor applied on load (1 = none).
    pub downsample: u32,
    /// Map values → cd/m².
    pub scale: f32,
    /// Turn about +Y, radians.
    pub rotation: f32,
    /// Irradiance on an unoccluded surface facing up, lux per channel (the sky alone).
    pub upward_irradiance: [f32; 3],
    /// Mean radiance over the sphere, luminance, cd/m².
    pub mean_luminance: f32,
    /// What sun extraction lifted out (map units, before the rotation and scale).
    pub sun_extraction: Option<ExtractedSun>,
}

/// What lights a bake (`plan.lights`): an agent checks the energy of the inputs before
/// spending a bake on them.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct LightInputs {
    /// The sun.
    pub sun: Option<SunInput>,
    /// The suns after the first (`.bake.ron` v3 `suns`, or a frame's extra directional lights:
    /// `PtScene::extra_suns`, ADR-7818), in order; left out of the JSON when empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub extra_suns: Vec<SunInput>,
    /// The sky.
    pub sky: SkyInput,
    /// Sky bodies.
    pub sky_bodies: usize,
    /// Area emitters from `emitters`.
    pub emitters: Vec<EmitterReport>,
    /// Luminous power of all `emitters`, lumens.
    pub emitter_power_lm: f32,
    /// Emissive materials in the scene (emitters and emissive mesh parts).
    pub emissive_materials: usize,
}

impl LightInputs {
    /// The sun, sky and sky bodies of `scene` (no emitter details).
    #[must_use]
    pub fn of_scene(scene: &PtScene) -> Self {
        let _span = tracing::info_span!("pt_light_inputs").entered();
        let (kind, size, scale, rotation) = match &scene.sky {
            PtSky::None => ("none", [0, 0], 1.0, 0.0),
            PtSky::Uniform(_) => ("uniform", [0, 0], 1.0, 0.0),
            PtSky::Table {
                width, height, scale, ..
            } => ("table", [*width, *height], *scale, 0.0),
            PtSky::Equirect {
                width,
                height,
                rotation,
                scale,
                ..
            } => ("equirect", [*width, *height], *scale, *rotation),
        };
        let rows = size[1].clamp(64, 1024);
        let mut up = [0.0f64; 3];
        let mut total = 0.0f64;
        if !matches!(scene.sky, PtSky::None) {
            integrate_sphere(rows, |d, w| {
                let l = scene.sky.radiance(d.to_array());
                total += f64::from(lum(l) * w);
                if d.y > 0.0 {
                    for (a, v) in up.iter_mut().zip(l) {
                        *a += f64::from(v * d.y * w);
                    }
                }
            });
        }
        Self {
            sun: scene.sun.map(|s| SunInput {
                toward: s.toward,
                illuminance: s.illuminance,
                color: s.color,
                angular_radius: s.angular_radius,
                from_hdri: false,
            }),
            extra_suns: scene
                .extra_suns
                .iter()
                .map(|s| SunInput {
                    toward: s.toward,
                    illuminance: s.illuminance,
                    color: s.color,
                    angular_radius: s.angular_radius,
                    from_hdri: false,
                })
                .collect(),
            sky: SkyInput {
                kind: kind.into(),
                size,
                downsample: 1,
                scale,
                rotation,
                upward_irradiance: up.map(|v| v as f32),
                mean_luminance: (total / (4.0 * std::f64::consts::PI)) as f32,
                ..SkyInput::default()
            },
            sky_bodies: scene.sky_bodies.len(),
            emissive_materials: scene.materials.iter().filter(|m| m.is_emissive()).count(),
            ..Self::default()
        }
    }
}

fn finite3(v: &[f32; 3]) -> bool {
    v.iter().all(|x| x.is_finite())
}

fn positive(v: f32) -> bool {
    v > 0.0 && v.is_finite()
}

/// Checks `sky_hdri` (key paths under `sky_hdri.`).
///
/// # Errors
///
/// `bake_desc_invalid` naming the first bad key.
pub fn validate_sky_hdri(h: &SkyHdriDesc) -> Result<(), BakeDescError> {
    let bad = |k: &str, m: &str| Err(BakeDescError::invalid(&format!("sky_hdri.{k}"), m.to_string()));
    if h.source.as_os_str().is_empty() {
        return bad("source", "must name a map");
    }
    if !h.rotation.is_finite() {
        return bad("rotation", "must be finite");
    }
    if h.scale.is_some() && h.irradiance.is_some() {
        return bad("irradiance", "give either scale or irradiance, not both");
    }
    if h.scale.is_some_and(|s| !(s >= 0.0 && s.is_finite())) {
        return bad("scale", "must be zero or positive");
    }
    if h.irradiance.is_some_and(|e| !positive(e)) {
        return bad("irradiance", "must be positive");
    }
    if h.max_width < 16 {
        return bad("max_width", "must be at least 16");
    }
    if !(h.sun_threshold > 1.0 && h.sun_threshold.is_finite()) {
        return bad("sun_threshold", "must be above 1");
    }
    if !(h.sun_max_solid_angle > 0.0 && h.sun_max_solid_angle < std::f32::consts::TAU) {
        return bad("sun_max_solid_angle", "must be within 0..2π steradians");
    }
    Ok(())
}

/// Checks `emitters[index]`.
///
/// # Errors
///
/// `bake_desc_invalid` naming the first bad key.
pub fn validate_emitter(index: usize, e: &EmitterDesc) -> Result<(), BakeDescError> {
    let bad = |k: &str, m: &str| {
        Err(BakeDescError::invalid(
            &format!("emitters[{index}].{k}"),
            m.to_string(),
        ))
    };
    match (e.radiance, e.power) {
        (Some(_), Some(_)) | (None, None) => {
            return bad("radiance", "give exactly one of radiance and power");
        }
        (Some(r), None) if !(finite3(&r) && r.iter().all(|v| *v >= 0.0)) => {
            return bad("radiance", "must be finite and not negative");
        }
        (None, Some(p)) if !positive(p) => return bad("power", "must be positive"),
        _ => {}
    }
    if e.power.is_some() && !(finite3(&e.color) && e.color.iter().all(|v| *v >= 0.0) && lum(e.color) > 0.0) {
        return bad("color", "must be finite, not negative and not black");
    }
    match e.shape {
        EmitterShape::Quad {
            corner,
            edge_u,
            edge_v,
        } => {
            if !(finite3(&corner) && finite3(&edge_u) && finite3(&edge_v)) {
                return bad("shape", "must be finite");
            }
            let area = Vec3::from_array(edge_u).cross(Vec3::from_array(edge_v)).length();
            if area.is_nan() || area <= 1e-10 {
                return bad("shape.edge_v", "the edges must span an area");
            }
        }
        EmitterShape::Disc {
            center,
            normal,
            radius,
            segments,
        } => {
            if !(finite3(&center) && finite3(&normal)) || Vec3::from_array(normal).length() < 1e-6 {
                return bad("shape.normal", "must be finite and non-zero");
            }
            if !positive(radius) {
                return bad("shape.radius", "must be positive");
            }
            if !(3..=4096).contains(&segments) {
                return bad("shape.segments", "must be within 3..=4096");
            }
        }
        EmitterShape::Sphere {
            center,
            radius,
            segments,
        } => {
            if !finite3(&center) {
                return bad("shape.center", "must be finite");
            }
            if !positive(radius) {
                return bad("shape.radius", "must be positive");
            }
            if !(4..=1024).contains(&segments) {
                return bad("shape.segments", "must be within 4..=1024");
            }
        }
    }
    Ok(())
}

/// The traced triangles of `shape` (front faces emit), and its shape name.
#[must_use]
pub fn emitter_mesh(shape: &EmitterShape) -> (PtMesh, &'static str) {
    match *shape {
        EmitterShape::Quad {
            corner,
            edge_u,
            edge_v,
        } => {
            let (c, u, v) = (
                Vec3::from_array(corner),
                Vec3::from_array(edge_u),
                Vec3::from_array(edge_v),
            );
            (
                scenes::quad([
                    c.to_array(),
                    (c + u).to_array(),
                    (c + u + v).to_array(),
                    (c + v).to_array(),
                ]),
                "quad",
            )
        }
        EmitterShape::Disc {
            center,
            normal,
            radius,
            segments,
        } => {
            let n = Vec3::from_array(normal).normalize_or(Vec3::Y);
            let a = if n.y.abs() < 0.9 { Vec3::Y } else { Vec3::X };
            let t = a.cross(n).normalize();
            let b = n.cross(t);
            let c = Vec3::from_array(center);
            let mut m = PtMesh::default();
            m.positions.push(c.to_array());
            for k in 0..segments {
                let phi = k as f32 / segments as f32 * std::f32::consts::TAU;
                let (s, co) = phi.sin_cos();
                m.positions.push((c + (t * co + b * s) * radius).to_array());
            }
            m.normals = vec![n.to_array(); m.positions.len()];
            for k in 0..segments {
                m.indices.extend_from_slice(&[0, 1 + k, 1 + (k + 1) % segments]);
            }
            (m, "disc")
        }
        EmitterShape::Sphere {
            center,
            radius,
            segments,
        } => {
            let mut m = scenes::uv_sphere(radius, segments, (segments / 2).max(2), false);
            for p in &mut m.positions {
                *p = [p[0] + center[0], p[1] + center[1], p[2] + center[2]];
            }
            (m, "sphere")
        }
    }
}

/// Total area of a mesh's triangles, m².
#[must_use]
pub fn mesh_area(mesh: &PtMesh) -> f32 {
    mesh.indices
        .as_chunks::<3>()
        .0
        .iter()
        .map(|t| {
            let p = |k: u32| Vec3::from_array(mesh.positions.get(k as usize).copied().unwrap_or([0.0; 3]));
            0.5 * (p(t[1]) - p(t[0])).cross(p(t[2]) - p(t[0])).length()
        })
        .sum()
}

/// Places `desc` (validated) in `scene` as a black emissive mesh and reports it.
pub fn add_emitter(scene: &mut PtScene, index: usize, desc: &EmitterDesc) -> EmitterReport {
    let (mesh, shape) = emitter_mesh(&desc.shape);
    let two_sided = desc.two_sided && !matches!(desc.shape, EmitterShape::Sphere { .. });
    let area = mesh_area(&mesh);
    let radiance = match (desc.radiance, desc.power) {
        (Some(r), _) => r,
        (None, Some(p)) => {
            let l = lambert_emitter_luminance(p, area, two_sided) / lum(desc.color).max(1e-12);
            desc.color.map(|c| c * l)
        }
        (None, None) => [0.0; 3],
    };
    let triangles = mesh.triangle_count();
    let material = scene.add_material(PtMaterial {
        emissive: radiance,
        one_sided_emission: !two_sided,
        ..PtMaterial::lambert([0.0; 3])
    });
    let mesh = scene.add_mesh(mesh);
    scene.add_instance(mesh, material, Mat4::IDENTITY);
    EmitterReport {
        id: desc.id.clone().unwrap_or_else(|| format!("emitters[{index}]")),
        shape: shape.into(),
        area,
        radiance,
        power_lm: lambert_emitter_power(lum(radiance), area, two_sided),
        triangles,
        two_sided,
    }
}

/// An HDRI sky loaded for a bake ([`load_sky_hdri`]).
#[derive(Clone, Debug)]
pub struct HdriSky {
    /// The traced sky.
    pub sky: PtSky,
    /// The extracted sun (world space, scaled), when `extract_sun` found one.
    pub sun: Option<PtSun>,
    /// The report (the irradiance fields are filled by [`LightInputs::of_scene`]).
    pub report: SkyInput,
}

/// Loads `desc`'s map from `path`: decode, downsample to `max_width`, extract the sun, scale.
///
/// # Errors
///
/// `bake_source_missing` (no file), `bake_hdri_decode` (unreadable or bad texels),
/// `bake_hdri_sun` (extraction refused the region), `bake_desc_invalid` (`irradiance` on a map
/// that is black above the horizon).
pub fn load_sky_hdri(desc: &SkyHdriDesc, path: &Path) -> Result<HdriSky, BakeDescError> {
    let _span = tracing::info_span!("pt_load_sky_hdri").entered();
    if !path.is_file() {
        return Err(BakeDescError::missing(
            "sky_hdri.source",
            format!("{}: no such file", path.display()),
        ));
    }
    let decode = |m: String| BakeDescError {
        code: "bake_hdri_decode",
        path: Some("sky_hdri.source".into()),
        message: format!("{}: {m}", path.display()),
    };
    let mut map = EnvMap::load(path).map_err(decode)?;
    map.validate().map_err(decode)?;
    let source_size = [map.width, map.height];
    let downsample = map.downsample_to(desc.max_width);
    let extracted = if desc.extract_sun {
        map.extract_sun(desc.sun_threshold, desc.sun_max_solid_angle)
            .map_err(|m| BakeDescError {
                code: "bake_hdri_sun",
                path: Some("sky_hdri.extract_sun".into()),
                message: m,
            })?
    } else {
        None
    };
    let scale = match (desc.scale, desc.irradiance) {
        (_, Some(target)) => {
            let mut up = 0.0f64;
            for j in 0..map.height {
                let dw = f64::from(map.texel_solid_angle(j));
                for i in 0..map.width {
                    let y = map.texel_dir(i, j)[1];
                    if y > 0.0 {
                        up += f64::from(lum(map.rgb[(j * map.width + i) as usize]) * y) * dw;
                    }
                }
            }
            let sun_up = extracted.map_or(0.0, |s| s.illuminance * s.toward[1].max(0.0));
            let up = up as f32 + sun_up;
            if up.is_nan() || up <= 0.0 {
                return Err(BakeDescError::invalid(
                    "sky_hdri.irradiance",
                    "the map is black above the horizon: give scale instead".into(),
                ));
            }
            target / up
        }
        (Some(s), None) => s,
        (None, None) => 1.0,
    };
    let sun = extracted.map(|s| s.to_sun(desc.rotation, scale));
    let size = [map.width, map.height];
    let sky = map
        .into_sky(desc.rotation, scale)
        .map_err(|e| decode(e.to_string()))?;
    Ok(HdriSky {
        sky,
        sun,
        report: SkyInput {
            kind: "equirect".into(),
            source: Some(desc.source.display().to_string()),
            source_size: Some(source_size),
            size,
            downsample,
            scale,
            rotation: desc.rotation,
            sun_extraction: extracted,
            ..SkyInput::default()
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    fn disc(power: Option<f32>, radiance: Option<[f32; 3]>) -> EmitterDesc {
        EmitterDesc {
            id: None,
            shape: EmitterShape::Disc {
                center: [0.0, 3.0, 0.0],
                normal: [0.0, -1.0, 0.0],
                radius: 0.5,
                segments: 256,
            },
            radiance,
            power,
            color: [1.0, 0.8, 0.6],
            two_sided: false,
        }
    }

    #[test]
    fn emitter_meshes_face_their_front_and_have_their_area() {
        let (m, name) = emitter_mesh(&disc(None, Some([1.0; 3])).shape);
        assert_eq!(name, "disc");
        assert!((mesh_area(&m) - PI * 0.25).abs() < 1e-3 * PI * 0.25);
        for t in m.indices.as_chunks::<3>().0 {
            let p = |k: u32| Vec3::from_array(m.positions[k as usize]);
            let n = (p(t[1]) - p(t[0])).cross(p(t[2]) - p(t[0]));
            assert!(n.y < 0.0, "a fan triangle faces away from the disc's normal");
        }
        let quad = EmitterShape::Quad {
            corner: [0.0; 3],
            edge_u: [2.0, 0.0, 0.0],
            edge_v: [0.0, 0.0, 0.5],
        };
        let (m, _) = emitter_mesh(&quad);
        assert!((mesh_area(&m) - 1.0).abs() < 1e-6);
        // edge_u × edge_v = (2, 0, 0) × (0, 0, 0.5) = (0, −1, 0).
        assert!(m.normals[0][1] < -0.99);
        let sphere = EmitterShape::Sphere {
            center: [1.0, 2.0, 3.0],
            radius: 0.5,
            segments: 64,
        };
        let (m, _) = emitter_mesh(&sphere);
        let area = mesh_area(&m);
        assert!((area - PI).abs() < 0.01 * PI, "{area}");
    }

    #[test]
    fn power_sets_the_radiance_from_the_traced_area() {
        let mut scene = PtScene::new(crate::scene::PtCamera::pinhole(
            [0.0; 3],
            [0.0, 0.0, -1.0],
            1.0,
            4,
            4,
        ));
        let r = add_emitter(&mut scene, 0, &disc(Some(1000.0), None));
        assert!((r.power_lm - 1000.0).abs() < 0.1, "{r:?}");
        assert!((lum(r.radiance) - 1000.0 / (PI * r.area)).abs() < 1e-2);
        // The colour is kept, at luminance 1 × L.
        assert!((r.radiance[0] / r.radiance[2] - 1.0 / 0.6).abs() < 1e-4);
        assert_eq!(r.id, "emitters[0]");
        let m = &scene.materials[scene.instances[0].material];
        assert!(m.is_emissive() && m.one_sided_emission && m.lambert);
        assert_eq!(m.base_color, [0.0, 0.0, 0.0, 1.0]);
        let mut two = disc(None, Some([2.0; 3]));
        two.two_sided = true;
        let r2 = add_emitter(&mut scene, 1, &two);
        assert!((r2.power_lm - 2.0 * PI * 2.0 * r2.area).abs() < 1e-2);
        assert!(!scene.materials[scene.instances[1].material].one_sided_emission);
    }

    #[test]
    fn bad_emitters_and_maps_name_their_key() {
        let cases: [(EmitterDesc, &str); 5] = [
            (disc(None, None), "emitters[3].radiance"),
            (disc(Some(1.0), Some([1.0; 3])), "emitters[3].radiance"),
            (disc(Some(-1.0), None), "emitters[3].power"),
            (
                EmitterDesc {
                    color: [0.0; 3],
                    ..disc(Some(1.0), None)
                },
                "emitters[3].color",
            ),
            (
                EmitterDesc {
                    shape: EmitterShape::Quad {
                        corner: [0.0; 3],
                        edge_u: [1.0, 0.0, 0.0],
                        edge_v: [2.0, 0.0, 0.0],
                    },
                    ..disc(None, Some([1.0; 3]))
                },
                "emitters[3].shape.edge_v",
            ),
        ];
        for (e, key) in cases {
            let err = validate_emitter(3, &e).expect_err(key);
            assert_eq!(
                (err.code, err.path.as_deref()),
                ("bake_desc_invalid", Some(key)),
                "{err}"
            );
        }
        let h = SkyHdriDesc {
            source: "sky.hdr".into(),
            rotation: 0.0,
            scale: Some(2.0),
            irradiance: Some(1000.0),
            max_width: 2048,
            extract_sun: false,
            sun_threshold: 100.0,
            sun_max_solid_angle: 0.03,
        };
        let err = validate_sky_hdri(&h).expect_err("both");
        assert_eq!(err.path.as_deref(), Some("sky_hdri.irradiance"));
        assert!(
            validate_sky_hdri(&SkyHdriDesc {
                irradiance: None,
                ..h
            })
            .is_ok()
        );
    }

    /// An `.hdr` on disk: loaded, downsampled, sun lifted, and `irradiance` scales map + sun to
    /// the target on an upward surface.
    #[test]
    fn an_hdri_loads_with_its_sun_and_target_irradiance() {
        let dir = regia_test_utils::unique_temp_dir("regia_pt_hdri_load");
        let sun_dir = Vec3::new(0.0, 0.6, -0.8);
        let map = EnvMap::from_fn(512, 256, |d| {
            if Vec3::from_array(d).dot(sun_dir) > 3.0f32.to_radians().cos() {
                [20_000.0; 3]
            } else if d[1] > 0.0 {
                [0.4, 0.6, 1.0]
            } else {
                [0.1; 3]
            }
        });
        let path = dir.join("sky.hdr");
        let tmp = dir.join("sky.hdr.tmp");
        std::fs::write(&tmp, crate::hdr::encode_flat(map.width, map.height, &map.rgb)).expect("write");
        std::fs::rename(&tmp, &path).expect("rename");
        let desc = SkyHdriDesc {
            source: "sky.hdr".into(),
            rotation: PI / 2.0,
            scale: None,
            irradiance: Some(50_000.0),
            max_width: 256,
            extract_sun: true,
            sun_threshold: 100.0,
            sun_max_solid_angle: 0.03,
        };
        let loaded = load_sky_hdri(&desc, &path).expect("load");
        assert_eq!(loaded.report.source_size, Some([512, 256]));
        assert_eq!((loaded.report.size, loaded.report.downsample), ([256, 128], 2));
        let sun = loaded.sun.expect("sun");
        // The quarter turn takes the map's −Z sun to −X.
        let toward = Vec3::from_array(sun.toward);
        assert!(toward.dot(Vec3::new(-0.8, 0.6, 0.0)) > 0.995, "{toward}");
        let mut scene = PtScene::new(crate::scene::PtCamera::pinhole(
            [0.0; 3],
            [0.0, 0.0, -1.0],
            1.0,
            4,
            4,
        ));
        scene.sky = loaded.sky;
        let inputs = LightInputs::of_scene(&scene);
        assert_eq!(inputs.sky.kind, "equirect");
        let up = lum(inputs.sky.upward_irradiance) + sun.illuminance * sun.toward[1];
        assert!((up - 50_000.0).abs() < 0.01 * 50_000.0, "{up}");
        let err = load_sky_hdri(&desc, &dir.join("none.hdr")).expect_err("missing");
        assert_eq!(err.code, "bake_source_missing");
        std::fs::write(dir.join("bad.hdr"), b"not an hdr").expect("write");
        let err = load_sky_hdri(&desc, &dir.join("bad.hdr")).expect_err("bad");
        assert_eq!(err.code, "bake_hdri_decode");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
