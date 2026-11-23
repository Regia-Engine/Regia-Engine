// SPDX-License-Identifier: MPL-2.0
//! The runtime's frame → [`PtScene`] (`specs/27` § 1.1 "same inputs").
//!
//! Three entry points, from most to least faithful:
//!
//! - [`from_export`]: what the renderer actually drew ([`regia_render::FrameExport`], recorded
//!   with `FrameRenderer::set_scene_export`): every mesh instance of the frame (rigid, and
//!   skinned at this frame's pose), its cooked material, the camera, the sun after the
//!   time-of-day override and the outdoor parameters, plus the terrain a shared draw renders
//!   (`FrameRenderer::reference_heightfield`).
//! - [`from_extracted`]: the ECS extraction (`ExtractedFrame`) of one view, with the level's
//!   sun clock resolved the way the frame renderer resolves it.
//! - [`from_frame`]: a [`FrameScene`] plus [`ExtractOptions`] for what it does not carry (the
//!   sky and the terrain live outside it in the runtime: `OutdoorParams` / `TerrainSharedDraw`).
//!
//! The light around the scene is [`environment`]: with an atmosphere, the planet's sky-view
//! table and media; without one (an airless preset), the black sky, the star field and the
//! sun disc; in both cases every sky body (Jupiter from Europa, moons) as a disc area light
//! ([`sky_bodies_from_outdoor`]). Meshes are de-duplicated by `mesh_key` (or by vertex slice
//! when the key is 0), so a scatter field becomes one BLAS per rock shape plus instances.

use std::collections::HashMap;
use std::sync::Arc;

use regia_core::glam::{Mat4, Vec3};
use regia_render::{
    DirectionalLightUniform, FrameExport, FrameScene, JointMatrix, LabLocalLight, LabMaterial, LabTexture,
    LabVertex, OutdoorParams, ReferenceHeightfield,
};

use crate::scene::{
    PtCamera, PtCloudField, PtDetail, PtHeightfield, PtLight, PtMaterial, PtMedia, PtMesh, PtScene, PtSky,
    PtSkyBody, PtSun, PtTexture,
};

/// What the frame description does not carry.
#[derive(Clone, Debug, Default)]
pub struct ExtractOptions {
    /// Sky to trace. `None` derives one from the frame: a uniform sky of the frame's flat
    /// `ambient_lux` (radiance `ambient_lux / π`) when IBL is off, black otherwise.
    pub sky: Option<PtSky>,
    /// Angular radius of the sun disc, radians (0 keeps a delta sun like the forward pass).
    pub sun_angular_radius: f32,
    /// Terrain heightfield and its material (with the map's albedo texture over UV 0..1).
    pub terrain: Option<(PtHeightfield, PtMaterial, Option<PtTexture>)>,
    /// Drop meshes whose material is alpha-blended (VFX layers the PT does not trace in v1).
    pub skip_blended: bool,
    /// Thin lens `(aperture radius m, focus distance m)`; `None` = pinhole.
    pub lens: Option<(f32, f32)>,
    /// Participating media ([`media_from_outdoor`]).
    pub media: PtMedia,
    /// Bodies in the sky ([`sky_bodies_from_outdoor`]).
    pub sky_bodies: Vec<PtSkyBody>,
    /// Peak radiance (cd/m²) of the airless star field ([`star_radiance`]); 0 = none. Background
    /// only: stars light nothing. [`environment`] sets it for you; hosts that build a scene
    /// straight from a [`FrameScene`] (like `pt_plates`) pass [`star_radiance`] of the level's
    /// planet here.
    pub stars: f32,
    /// The frame's material clock `(now, prev)` (`FrameRenderer::material_clock`): graphs that
    /// read time take it in their slot 0, as the frame writes it (ADR-2725). `None` leaves the
    /// slot as each material carries it.
    pub material_clock: Option<(f32, f32)>,
    /// The scene wind the frame writes beside the clock (`FrameRenderer::set_foliage_wind`, m/s
    /// on world X and Z), for graphs that read time.
    pub foliage_wind: Option<[f32; 2]>,
    /// The frame's floating origin (`FrameRenderer::origin_offset`): graphs that read positions
    /// take it in their origin slot, as the frame writes it (ADR-2435, ADR-2725).
    pub origin_offset: [f32; 3],
    /// Trace each material's subsurface scattering (`LabMaterial::subsurface`) with Burley's
    /// normalized diffusion (ADR-2725). Off by default: the screen-space comparisons trace the
    /// light that lands on the surface and apply the profile themselves.
    pub subsurface: bool,
}

/// Burley's shape `d` (meters) per channel for a material's subsurface factors (ADR-2725): the
/// profile whose second radial moment (`E[r²] = 14 d²`) equals that of the frame's sum of
/// Gaussians (`Σ wᵢ 2 vᵢ / Σ wᵢ` per channel) at the material's reach, so both spread light as far
/// in the mean-square sense. `None` when the material does not scatter.
#[must_use]
pub fn burley_d(factors: &regia_render::SubsurfaceFactors) -> Option<[f32; 3]> {
    if !factors.is_on() {
        return None;
    }
    let own = factors.profile.reach_mm();
    let scale = if own > 0.0 { factors.reach_mm() / own } else { 1.0 };
    let gaussians = factors.profile.gaussians();
    let d = std::array::from_fn(|c| {
        let (mut moment, mut weight) = (0.0f64, 0.0f64);
        for (variance, w) in gaussians {
            moment += f64::from(w[c]) * 2.0 * f64::from(*variance);
            weight += f64::from(w[c]);
        }
        if weight <= 0.0 {
            return 0.0;
        }
        let r2_mm = moment / weight * f64::from(scale * scale);
        ((r2_mm / 14.0).sqrt() * 1e-3) as f32
    });
    Some(d)
}

/// The frame's graph on material `index` as the tracer's entry: the spliced surface text, the
/// parameter block the frame would upload (clock, wind, origin) and the graph textures.
fn graph_entry<'a>(
    graph: &'a regia_render::SurfaceGraph,
    index: usize,
    textures: &mut Textures<'a>,
    options: &ExtractOptions,
) -> Option<crate::scene::PtSurfaceGraph> {
    let surface = match regia_render::surface_graph::surface_source(&graph.code()) {
        Ok(text) => text,
        Err(err) => {
            tracing::warn!(target: "regia_pt", material = index, %err, "material graph not traced");
            return None;
        }
    };
    let mut params = graph.params;
    if graph.uses_time {
        if let Some((now, prev)) = options.material_clock {
            params[0][0] = now;
            params[0][1] = prev;
        }
        if let Some([x, z]) = options.foliage_wind {
            params[0][2] = x;
            params[0][3] = z;
        }
    }
    graph.write_origin(&mut params, options.origin_offset);
    let slots = std::array::from_fn(|slot| {
        let t = graph.textures[slot].as_ref()?;
        if !t.image.rgba.is_empty() {
            return textures.add(Some(&t.image), t.srgb);
        }
        if let Some(shared) = &t.shared
            && shared.has_pixels()
        {
            let (width, height) = shared.size();
            // Share the image's allocation: an extract runs per frame, and a copy of a large
            // graph texture per frame dwarfed the rest of the extract.
            let rgba = shared.pixels_arc().unwrap_or_else(|| Arc::from(shared.pixels()));
            return Some(textures.push_owned(PtTexture {
                width,
                height,
                rgba,
                srgb: shared.is_srgb(),
            }));
        }
        tracing::warn!(target: "regia_pt", material = index, slot, "graph texture has no CPU pixels; the tracer samples white");
        None
    });
    Some(crate::scene::PtSurfaceGraph {
        material: index,
        surface: surface.into(),
        params,
        textures: slots,
        vertex_offset: graph.vertex_wgsl.is_some(),
        uses_uv1: graph.uses_uv1,
        uses_time: graph.uses_time,
    })
}

/// Writes the frame's material clock `(now, prev)` (`FrameRenderer::material_clock`) into slot 0 of
/// every graph of `scene` that reads time, as the frame does when it uploads the block (ADR-2725).
/// For scenes built from a frame export or an extracted frame, which carry each graph's block as
/// the material holds it.
pub fn apply_material_clock(scene: &mut PtScene, clock: (f32, f32)) {
    for graph in &mut scene.surface_graphs {
        if graph.uses_time {
            graph.params[0][0] = clock.0;
            graph.params[0][1] = clock.1;
        }
    }
}

/// Converts a runtime texture (colour slots are sRGB-encoded bytes).
#[must_use]
pub fn texture(t: &LabTexture, srgb: bool) -> PtTexture {
    PtTexture {
        width: t.width,
        height: t.height,
        rgba: Arc::from(t.rgba.as_slice()),
        srgb,
    }
}

struct Textures<'a> {
    list: Vec<PtTexture>,
    seen: HashMap<(*const u8, bool), usize>,
    _marker: std::marker::PhantomData<&'a ()>,
}

impl<'a> Textures<'a> {
    fn add(&mut self, t: Option<&'a LabTexture>, srgb: bool) -> Option<usize> {
        let t = t?;
        if t.width == 0
            || t.height == 0
            || (t.rgba.len() as u64) < u64::from(t.width) * u64::from(t.height) * 4
        {
            return None;
        }
        let key = (t.rgba.as_ptr(), srgb);
        if let Some(&i) = self.seen.get(&key) {
            return Some(i);
        }
        self.list.push(texture(t, srgb));
        let i = self.list.len() - 1;
        self.seen.insert(key, i);
        Some(i)
    }

    /// A texture built here (not borrowed from the frame), never shared.
    fn push_owned(&mut self, t: PtTexture) -> usize {
        self.list.push(t);
        self.list.len() - 1
    }
}

/// The runtime material as the tracer shades it: textures, packed ORM metallic/roughness,
/// clearcoat, multi-scatter, anisotropy, detail maps, thin transmission with its volume tint,
/// the IOR and the thin film. Parallax and edge wear are not traced; occlusion maps only feed the packed ORM
/// channels (the tracer computes occlusion).
fn material<'a>(m: &'a LabMaterial, textures: &mut Textures<'a>) -> PtMaterial {
    let hs = &m.hard_surface;
    let detail_on = hs.detail_albedo_strength > 1e-5
        || hs.detail_normal_strength > 1e-5
        || hs.detail_roughness_strength > 1e-5;
    let detail = detail_on.then(|| {
        // The forward pass's packing (`hard_surface::pack_detail_*`), uploaded linear.
        let image = regia_render::hard_surface::pack_detail_image(
            m.detail_albedo.as_ref(),
            m.detail_roughness.as_ref(),
        )
        .map(|t| textures.push_owned(texture(&t, false)));
        let normal =
            regia_render::hard_surface::pack_detail_normal(m.detail_normal.as_ref(), m.detail_mask.as_ref())
                .map(|t| textures.push_owned(texture(&t, false)));
        PtDetail {
            image,
            normal,
            strength: [
                hs.detail_albedo_strength,
                hs.detail_normal_strength,
                hs.detail_roughness_strength,
            ],
            uv_scale: hs.detail_uv_scale,
            fade_start: hs.detail_fade_start,
            fade_end: hs.detail_fade_end,
        }
    });
    PtMaterial {
        base_color: m.base_color,
        metallic: m.metallic,
        roughness: m.roughness,
        emissive: [
            m.emissive[0] * m.emissive_intensity,
            m.emissive[1] * m.emissive_intensity,
            m.emissive[2] * m.emissive_intensity,
        ],
        normal_scale: m.normal_scale,
        // Hair draws in its strategy's mode whatever the host wrote (the raster does the same).
        alpha_mode: regia_render::hair::effective_alpha_mode(m),
        alpha_cutoff: m.alpha_cutoff,
        clearcoat: m.hard_surface.clearcoat,
        clearcoat_roughness: m.hard_surface.clearcoat_roughness,
        multi_scatter: m.hard_surface.multi_scatter > 0.5,
        unlit: m.unlit,
        lambert: false,
        albedo: textures.add(m.albedo.as_ref(), true),
        normal: textures.add(m.normal_map.as_ref(), false),
        emissive_map: textures.add(m.emissive_map.as_ref(), true),
        orm: if m.orm_packed {
            textures.add(m.occlusion_map.as_ref(), false)
        } else {
            None
        },
        orm_packed: m.orm_packed,
        anisotropy: hs.anisotropy,
        anisotropy_rotation: hs.anisotropy_rotation,
        detail,
        one_sided_emission: false,
        transmission: m.transmission.clamp(0.0, 1.0),
        transmission_tint: m.transmission_tint,
        transmission_roughness: regia_render::rough_transmission::effective_roughness(m),
        ior: m.ior,
        // The forward pass draws no film on a transmissive surface or a material graph.
        thin_film: (m.thin_film.is_on() && m.transmission <= 0.0 && m.surface_graph.is_none()).then(|| {
            crate::scene::PtThinFilm {
                factor: m.thin_film.factor,
                ior: m.thin_film.ior,
                thickness_min: m.thin_film.thickness_min,
                thickness_max: m.thin_film.thickness_max,
                map: textures.add(m.thin_film_map.as_ref(), false),
            }
        }),
    }
}

/// A mesh's vertices as the tracer's mesh, skinned like the forward pass when `joints` is
/// non-empty (`Σ weight · joint · position`; normals and tangents by the same blend,
/// renormalised).
fn pt_mesh(vertices: &[LabVertex], indices: &[u32], joints: &[JointMatrix]) -> PtMesh {
    let blend = |v: &LabVertex, x: [f32; 3], w: f32| -> Option<[f32; 3]> {
        let total: f32 = v.weights.iter().sum();
        if joints.is_empty() || total < 1e-5 {
            return None;
        }
        let mut out = [0.0f32; 3];
        for k in 0..4 {
            let weight = v.weights[k];
            let Some(j) = joints.get(v.joints[k] as usize) else {
                continue;
            };
            if weight == 0.0 {
                continue;
            }
            let m = j.m;
            for (r, o) in out.iter_mut().enumerate() {
                *o += weight * (m[r][0] * x[0] + m[r][1] * x[1] + m[r][2] * x[2] + m[r][3] * w);
            }
        }
        Some(out)
    };
    let unit = |x: [f32; 3], fallback: [f32; 3]| {
        Vec3::from_array(x)
            .normalize_or(Vec3::from_array(fallback))
            .to_array()
    };
    let mut positions = Vec::with_capacity(vertices.len());
    let mut normals = Vec::with_capacity(vertices.len());
    let mut tangents = Vec::with_capacity(vertices.len());
    for v in vertices {
        positions.push(blend(v, v.position, 1.0).unwrap_or(v.position));
        normals.push(blend(v, v.normal, 0.0).map_or(v.normal, |n| unit(n, v.normal)));
        let t = [v.tangent[0], v.tangent[1], v.tangent[2]];
        let t = blend(v, t, 0.0).map_or(t, |b| unit(b, t));
        tangents.push([t[0], t[1], t[2], v.tangent[3]]);
    }
    PtMesh {
        positions,
        normals,
        uvs: vertices.iter().map(|v| v.uv).collect(),
        colors: vertices.iter().map(|v| v.color).collect(),
        tangents,
        indices: indices.to_vec(),
    }
}

/// Meshes the tracer drops: alpha-blended VFX layers when `skip_blended` (thin transmissive
/// glass is blended in the raster frame but traced).
fn skipped(m: &LabMaterial, skip_blended: bool) -> bool {
    skip_blended
        && regia_render::hair::effective_alpha_mode(m) >= regia_render::ALPHA_BLEND
        && m.transmission <= 0.0
}

/// Builds the tracer's scene from the runtime frame.
#[must_use]
pub fn from_frame(frame: &FrameScene<'_>, options: &ExtractOptions) -> PtScene {
    let _span = tracing::info_span!("pt_extract", meshes = frame.meshes.len()).entered();
    let mut scene = PtScene::new(PtCamera {
        aperture_radius: options.lens.map_or(0.0, |l| l.0),
        focus_distance: options.lens.map_or(1.0, |l| l.1),
        ..PtCamera::pinhole(
            frame.eye,
            frame.target,
            frame.fov_y,
            frame.width.max(1),
            frame.height.max(1),
        )
    });

    let mut textures = Textures {
        list: Vec::new(),
        seen: HashMap::new(),
        _marker: std::marker::PhantomData,
    };
    let mut meshes: HashMap<(u64, usize, usize), usize> = HashMap::new();
    let mut materials: HashMap<(u64, usize), usize> = HashMap::new();
    for fm in &frame.meshes {
        // The outgoing copy of a LOD cross-fade: the incoming one is traced whole.
        if skipped(fm.material, options.skip_blended) || fm.lod_dither < 0.0 {
            continue;
        }
        let mesh = if fm.joints.is_empty() {
            let mesh_id = if fm.mesh_key != 0 {
                (fm.mesh_key, 0, 0)
            } else {
                (0, fm.vertices.as_ptr() as usize, fm.indices.as_ptr() as usize)
            };
            *meshes.entry(mesh_id).or_insert_with(|| {
                scene.meshes.push(pt_mesh(fm.vertices, fm.indices, &[]));
                scene.meshes.len() - 1
            })
        } else {
            // Skinned: this frame's pose, one mesh per instance.
            scene.add_mesh(pt_mesh(fm.vertices, fm.indices, fm.joints))
        };
        let mat_id = if fm.material_key != 0 {
            (fm.material_key, 0)
        } else {
            (0, std::ptr::from_ref(fm.material) as usize)
        };
        let mat = match materials.get(&mat_id) {
            Some(&m) => m,
            None => {
                let m = scene.add_material(material(fm.material, &mut textures));
                materials.insert(mat_id, m);
                if let Some(graph) = fm.material.surface_graph.as_deref()
                    && let Some(entry) = graph_entry(graph, m, &mut textures, options)
                {
                    // The forward pass ignores the standard block's emission on a graph material
                    // — but only when the graph actually composes. A refused graph traces with
                    // its standard factors (`crate::graph` module docs), so its emissive stays
                    // (ADR-2725).
                    if crate::graph::composes(&entry) {
                        scene.materials[m].emissive = [0.0; 3];
                        scene.materials[m].emissive_map = None;
                    }
                    scene.surface_graphs.push(entry);
                }
                if options.subsurface
                    && let Some(d) = burley_d(&fm.material.subsurface)
                {
                    scene.subsurface.push(crate::scene::PtSubsurface {
                        material: m,
                        d,
                        tint: fm.material.subsurface.tint,
                    });
                }
                m
            }
        };
        scene.add_instance(mesh, mat, Mat4::from_cols_array_2d(&fm.model));
    }
    let d = frame.directional;
    if d.illuminance > 0.0 {
        scene.sun = Some(PtSun {
            toward: [-d.direction[0], -d.direction[1], -d.direction[2]],
            color: d.color,
            illuminance: d.illuminance,
            angular_radius: options.sun_angular_radius.max(0.0),
        });
    }
    push_local_lights(&mut scene, frame.local_lights);
    scene.sky = match &options.sky {
        Some(sky) => sky.clone(),
        None if !frame.ibl && frame.ambient_lux > 0.0 => {
            PtSky::Uniform([frame.ambient_lux / std::f32::consts::PI; 3])
        }
        None => PtSky::None,
    };
    scene.textures = textures.list;
    if let Some(terrain) = options.terrain.clone() {
        add_terrain(&mut scene, terrain);
    }
    scene.media = options.media.clone();
    scene.sky_bodies = options.sky_bodies.clone();
    scene.stars = options.stars;
    scene
}

/// The planet's sky as the tracer's environment: the runtime's sky-view table for `planet`
/// with the sun toward `toward_sun`, seen from `camera_height_m` above the ground.
#[must_use]
pub fn atmosphere_sky(
    planet: &regia_render::AtmosphereParameters,
    toward_sun: [f32; 3],
    camera_height_m: f32,
) -> PtSky {
    let luts = regia_render::build_atmosphere_luts_for(planet, toward_sun, camera_height_m.max(1.0));
    PtSky::from_atmosphere(&luts)
}

/// The planet's air and the level's height fog as the tracer's media, with the parameters
/// the real-time path uses: Rayleigh and Mie layers of [`regia_render::AtmosphereParameters`]
/// (flat-ground approximation of the spherical shells; world Y is the altitude, clamped at 0
/// like the aerial-perspective table) as aerial layers, and
/// [`regia_render::VolumetricFogSettings`] height fog with `OutdoorParams::fog_medium`'s albedo
/// and asymmetry out to the froxel fog's layer range. Ozone (10–40 km) and fog volumes are not
/// traced. An airless planet has no air layers.
#[must_use]
pub fn media_from_outdoor(params: &regia_render::OutdoorParams) -> PtMedia {
    use crate::scene::{PtMediumLayer, PtPhase};
    let mut layers = Vec::new();
    let p = &params.planet;
    if !p.is_airless() {
        let km = |v: [f32; 3]| v.map(|c| (c / 1000.0).max(0.0));
        let ray = km(p.rayleigh_scattering);
        let mie_s = km(p.mie_scattering);
        let mie_a = km(std::array::from_fn(|c| p.mie_extinction[c] - p.mie_scattering[c]));
        for (scattering, absorption, height_km, phase) in [
            (ray, [0.0; 3], p.rayleigh_scale_height_km, PtPhase::Rayleigh),
            (
                mie_s,
                mie_a,
                p.mie_scale_height_km,
                PtPhase::HenyeyGreenstein(p.mie_g),
            ),
        ] {
            let air = PtMediumLayer {
                scattering,
                absorption,
                reference_height: 0.0,
                falloff: 1.0 / (height_km.max(1e-3) * 1000.0),
                min_height: 0.0,
                max_height: 1.0e9,
                phase,
                aerial: true,
            };
            layers.push(air);
            // Below the datum the table clamps the altitude to 0: ground-level density.
            layers.push(PtMediumLayer {
                falloff: 0.0,
                min_height: -1.0e9,
                max_height: 0.0,
                ..air
            });
        }
    }
    let fog = &params.fog;
    if fog.density > 0.0 {
        let (albedo, g) = params.fog_medium();
        let sigma_t = fog.density;
        layers.push(PtMediumLayer {
            scattering: albedo.map(|a| sigma_t * a.clamp(0.0, 1.0)),
            absorption: albedo.map(|a| sigma_t * (1.0 - a.clamp(0.0, 1.0))),
            reference_height: fog.base_height,
            falloff: fog.height_falloff.max(0.0),
            min_height: -1.0e9,
            max_height: 1.0e9,
            phase: PtPhase::HenyeyGreenstein([g; 3]),
            aerial: false,
        });
    }
    PtMedia {
        layers,
        escape_distance: regia_render::FOG_LAYER_RANGE_M,
    }
}

/// Radiance (cd/m²) of a sky body toward `view`: the sky shader's `lit_sphere` (Lambert from
/// the sun toward `to_sun` with the planet's `sun_lux`, mild limb darkening, procedural bands
/// or the body's albedo map), without the one-pixel edge antialiasing.
#[must_use]
pub fn lit_sphere_radiance(
    view: [f32; 3],
    body: &regia_render::SkyBody,
    albedo_map: Option<&regia_render::SkyBodyTexture>,
    to_sun: [f32; 3],
    sun_lux: f32,
) -> [f32; 3] {
    let radius = (0.5 * body.angular_diameter_deg).to_radians();
    let view = Vec3::from_array(view).normalize_or(Vec3::Y);
    let c = Vec3::from_array(body.direction).normalize_or(Vec3::Y);
    let cos_v = view.dot(c).clamp(-1.0, 1.0);
    if radius <= 0.0 || cos_v.acos() > radius {
        return [0.0; 3];
    }
    let sr = radius.sin();
    let disc = (cos_v * cos_v - (1.0 - sr * sr)).max(0.0);
    let t = cos_v - disc.sqrt();
    let n = (view * t - c).normalize_or(-c);
    let up = Vec3::Y;
    let mut pole = up - c * up.dot(c);
    pole = if pole.length() > 1e-3 {
        pole.normalize()
    } else {
        Vec3::X
    };
    let east = pole.cross(c).normalize_or(Vec3::X);
    let lat = n.dot(pole);
    let lon = n.dot(east).atan2(n.dot(-c));
    let tint = Vec3::from_array(body.tint);
    let mut albedo = tint;
    if let Some(tex) = albedo_map {
        let u = (lon / std::f32::consts::TAU + 0.5).rem_euclid(1.0);
        let v = 0.5 - lat.clamp(-1.0, 1.0).asin() / std::f32::consts::PI;
        albedo = tint * Vec3::from_array(sample_srgb_repeat(tex, u, v));
    } else if body.bands > 0.0 {
        let swirl = (lon * 3.0 + lat * 41.0).sin() * 0.5 + (lon * 7.0 - lat * 23.0).sin() * 0.25;
        let band = 0.5 + 0.5 * (lat * 19.0 + 1.6 * (lat * 6.0).sin() + 0.35 * swirl).sin();
        let zone = Vec3::new(1.04, 0.98, 0.88);
        let belt = Vec3::new(0.72, 0.52, 0.36);
        let banded = belt.lerp(zone, band);
        let s = ((lat.abs() - 0.75) / 0.2).clamp(0.0, 1.0);
        let polar = Vec3::ONE.lerp(Vec3::new(0.78, 0.76, 0.74), s * s * (3.0 - 2.0 * s));
        albedo = tint * Vec3::ONE.lerp(banded * polar, body.bands.clamp(0.0, 1.0));
    }
    let ndl = n.dot(Vec3::from_array(to_sun).normalize_or(Vec3::Y)).max(0.0);
    let limb = n.dot(-view).max(0.0).powf(0.2);
    (albedo * (sun_lux * ndl * limb / std::f32::consts::PI)).to_array()
}

/// Bilinear, repeat, sRGB-decoded sample of a body albedo map (the sky pass's sampler).
fn sample_srgb_repeat(tex: &regia_render::SkyBodyTexture, u: f32, v: f32) -> [f32; 3] {
    let (w, h) = (tex.width.max(1) as i64, tex.height.max(1) as i64);
    let fx = u * w as f32 - 0.5;
    let fy = v * h as f32 - 0.5;
    let (x0, y0) = (fx.floor(), fy.floor());
    let (tx, ty) = (fx - x0, fy - y0);
    let decode = |c: u8| {
        let c = f32::from(c) / 255.0;
        if c <= 0.040_45 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    let at = |x: i64, y: i64| {
        let (xi, yi) = (x.rem_euclid(w) as usize, y.rem_euclid(h) as usize);
        let o = (yi * w as usize + xi) * 4;
        tex.rgba
            .get(o..o + 3)
            .map_or([0.0; 3], |p| [decode(p[0]), decode(p[1]), decode(p[2])])
    };
    let (x0, y0) = (x0 as i64, y0 as i64);
    let (a, b, c, d) = (at(x0, y0), at(x0 + 1, y0), at(x0, y0 + 1), at(x0 + 1, y0 + 1));
    std::array::from_fn(|k| {
        let top = a[k] + (b[k] - a[k]) * tx;
        let bot = c[k] + (d[k] - c[k]) * tx;
        top + (bot - top) * ty
    })
}

/// The level's sky bodies as the tracer's: the moons, then the main body (drawn in front),
/// each tabulated from [`lit_sphere_radiance`] for a sun toward `to_sun`. The main body lights
/// the scene with the real-time fill's illuminance (`SkyBody::fill_illuminance`: the authored
/// `fill_lux`, else the physical planet-shine) — its `light_scale` maps the disc's own
/// illuminance onto that — but, unlike the real-time fill, it casts shadows. Moons light the
/// scene physically (scale 1); the real-time frame has no fill from them.
#[must_use]
pub fn sky_bodies_from_outdoor(params: &regia_render::OutdoorParams, to_sun: [f32; 3]) -> Vec<PtSkyBody> {
    let sun_lux = params.planet.sun_illuminance;
    let mut out = Vec::new();
    let mut add = |body: &regia_render::SkyBody, map: Option<&regia_render::SkyBodyTexture>, fill: bool| {
        let radius = (0.5 * body.angular_diameter_deg).to_radians();
        if radius <= 0.0 || radius >= 1.4 {
            return;
        }
        let d = body.direction;
        let len = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
        if len < 1e-6 {
            return;
        }
        let size = (radius.to_degrees() * 40.0).clamp(32.0, 384.0) as u32;
        let mut pt = PtSkyBody {
            direction: [d[0] / len, d[1] / len, d[2] / len],
            angular_radius: radius,
            radiance: Vec::new(),
            size,
            light_scale: 1.0,
        };
        pt.radiance = pt.tabulate(|v| lit_sphere_radiance(v.to_array(), body, map, to_sun, sun_lux));
        if fill {
            let own = pt.normal_irradiance();
            let want = body.fill_illuminance(sun_lux, to_sun);
            pt.light_scale = if own > 0.0 { want / own } else { 0.0 };
        }
        out.push(pt);
    };
    for (i, moon) in params.moons.iter().take(2).enumerate() {
        add(moon, params.moon_albedo.get(i).and_then(Option::as_deref), false);
    }
    if let Some(body) = params.sky_body.as_ref() {
        add(body, params.sky_body_albedo.as_deref(), true);
    }
    out
}

/// The sky, sun, media, sky bodies and stars around a frame lit by `sun` (the light the frame
/// used, after any time-of-day override) on `outdoor`'s planet.
#[derive(Clone, Debug)]
pub struct Environment {
    /// The sky.
    pub sky: PtSky,
    /// The sun (`None` when the frame has no sunlight).
    pub sun: Option<PtSun>,
    /// Participating media.
    pub media: PtMedia,
    /// Sky bodies ([`sky_bodies_from_outdoor`]).
    pub sky_bodies: Vec<PtSkyBody>,
    /// Star peak radiance (airless planets; 0 = none).
    pub stars: f32,
}

/// The star peak radiance (cd/m²) the raster sky draws on `planet`: `day_star_radiance` on an
/// airless planet, 0 where the sky has air (there stars show only at night, which the tracer
/// does not reproduce). The single source of the frame's star field — [`environment`] and
/// [`ExtractOptions::stars`] both use it, so [`from_frame`] and [`from_extracted`] agree.
#[must_use]
pub fn star_radiance(planet: &regia_render::AtmosphereParameters) -> f32 {
    if planet.is_airless() {
        planet.day_star_radiance.max(0.0)
    } else {
        0.0
    }
}

/// The environment the raster frame shows and lights with, seen from `eye_height_m` above the
/// ground:
///
/// - Sky: lit by IBL, the planet's sky-view table and its media ([`atmosphere_sky`],
///   [`media_from_outdoor`]; black and empty on an airless planet); otherwise a uniform sky of
///   the flat `ambient_lux` (radiance `ambient_lux / π`), or none.
/// - Sun: a disc of the planet's angular radius delivering the frame's illuminance.
/// - Sky bodies: [`sky_bodies_from_outdoor`] for this sun (the level's body lights with the
///   frame's planet-shine fill, moons physically).
/// - Stars: the planet's `day_star_radiance` on an airless planet, as the raster sky draws them.
#[must_use]
pub fn environment(
    outdoor: &OutdoorParams,
    sun: DirectionalLightUniform,
    ibl: bool,
    ambient_lux: f32,
    eye_height_m: f32,
) -> Environment {
    let planet = &outdoor.planet;
    let toward_sun = (-Vec3::from_array(sun.direction).normalize_or(Vec3::NEG_Y)).to_array();
    let sky = if ibl {
        atmosphere_sky(planet, toward_sun, eye_height_m)
    } else if ambient_lux > 0.0 {
        PtSky::Uniform([ambient_lux / std::f32::consts::PI; 3])
    } else {
        PtSky::None
    };
    let media = if ibl {
        media_from_outdoor(outdoor)
    } else {
        PtMedia::default()
    };
    Environment {
        sky,
        sun: (sun.illuminance > 0.0).then(|| PtSun {
            toward: toward_sun,
            color: sun.color,
            illuminance: sun.illuminance,
            angular_radius: planet.sun_angular_radius.max(0.0),
        }),
        media,
        sky_bodies: sky_bodies_from_outdoor(outdoor, toward_sun),
        stars: star_radiance(planet),
    }
}

fn apply_environment(scene: &mut PtScene, env: Environment) {
    scene.sky = env.sky;
    scene.sun = env.sun;
    scene.media = env.media;
    scene.sky_bodies = env.sky_bodies;
    scene.stars = env.stars;
}

/// The frame's cloud layer as the tracer's cloud field (ADR-2127): the physical reference (albedo 1,
/// droplet phase, [`PtCloudField::from_settings`]). `None` when the layer draws no volumetric cloud
/// or the planet has no air. The traces of a frame only hold it when asked for
/// (`seeing.reference_trace {clouds}`): a frame's reference has been cloudless until now.
#[must_use]
pub fn clouds_from_outdoor(params: &OutdoorParams) -> Option<PtCloudField> {
    if params.clouds.coverage <= 0.0 || params.planet.is_airless() {
        return None;
    }
    Some(PtCloudField::from_settings(
        params.clouds,
        params.planet.bottom_radius_km * 1000.0,
    ))
}

/// The directional light the frame renderer lights with: the level's sun clock
/// (`OutdoorParams::time`) through the planet's air, else the authored light
/// (`OutdoorRuntime::apply_sun` with the sky on).
#[must_use]
pub fn effective_sun(
    outdoor: &OutdoorParams,
    authored: DirectionalLightUniform,
    eye_y: f32,
) -> DirectionalLightUniform {
    let Some(time) = outdoor.time else {
        return authored;
    };
    let toward = regia_render::sun_position(&time).direction;
    let luts = regia_render::build_atmosphere_luts_for(&outdoor.planet, toward, eye_y.max(1.0));
    let (color, lux) = regia_render::transmitted_sun(&luts, &luts.params);
    DirectionalLightUniform::new([-toward[0], -toward[1], -toward[2]], color, lux)
}

/// The terrain a shared draw renders, as the tracer's heightfield, its material and its albedo
/// ([`ExtractOptions::terrain`]).
#[must_use]
pub fn heightfield(reference: &ReferenceHeightfield) -> (PtHeightfield, PtMaterial, Option<PtTexture>) {
    let hf = PtHeightfield {
        width: reference.width,
        depth: reference.depth,
        origin: reference.origin,
        spacing: reference.spacing,
        heights: reference.heights.clone(),
        material: 0,
    };
    let material = PtMaterial {
        base_color: [1.0; 4],
        roughness: reference.roughness,
        ..PtMaterial::default()
    };
    (hf, material, reference.albedo.as_ref().map(|t| texture(t, true)))
}

fn add_terrain(scene: &mut PtScene, terrain: (PtHeightfield, PtMaterial, Option<PtTexture>)) {
    let (hf, mut mat, tex) = terrain;
    if let Some(t) = tex {
        scene.textures.push(t);
        mat.albedo = Some(scene.textures.len() - 1);
    }
    let m = scene.add_material(mat);
    scene.terrain = Some(PtHeightfield { material: m, ..hf });
}

/// The ground height under the eye (for the sky table's camera altitude).
fn ground_below(terrain: Option<&ReferenceHeightfield>, eye: [f32; 3]) -> f32 {
    let Some(t) = terrain else { return 0.0 };
    if t.width < 2 || t.depth < 2 || t.spacing[0] == 0.0 || t.spacing[1] == 0.0 {
        return 0.0;
    }
    let i = ((eye[0] - t.origin[0]) / t.spacing[0])
        .round()
        .clamp(0.0, (t.width - 1) as f32) as usize;
    let j = ((eye[2] - t.origin[1]) / t.spacing[1])
        .round()
        .clamp(0.0, (t.depth - 1) as f32) as usize;
    // A cut sample (a hole under the eye) has no ground: 0 m, as off the map.
    t.heights
        .get(j * t.width as usize + i)
        .copied()
        .filter(|h| h.is_finite())
        .unwrap_or(0.0)
}

/// The tracer's scene for what the renderer drew ([`FrameExport`]), with the terrain the shared
/// draw renders (`FrameRenderer::reference_heightfield`) when there is one.
///
/// Every exported instance is traced (rigid meshes share one BLAS per `mesh_key`; skinned ones
/// are posed with the frame's joint palette) with its cooked material ([`ExtractOptions`]'s
/// conversion: base colour, packed ORM, normal, emissive, detail, transmission). Alpha-blended
/// VFX layers are dropped. The light is the frame's own: its sun and its planet
/// ([`environment`]).
#[must_use]
pub fn from_export(export: &FrameExport, terrain: Option<&ReferenceHeightfield>) -> PtScene {
    from_export_with(export, terrain, &ExtractOptions::default())
}

/// [`from_export`] with the graph and subsurface fields of `options`
/// ([`ExtractOptions::material_clock`], [`ExtractOptions::foliage_wind`],
/// [`ExtractOptions::origin_offset`], [`ExtractOptions::subsurface`]; ADR-2725). Its other fields
/// do not apply to an export.
#[must_use]
pub fn from_export_with(
    export: &FrameExport,
    terrain: Option<&ReferenceHeightfield>,
    options: &ExtractOptions,
) -> PtScene {
    let _span = tracing::info_span!("pt_from_export", instances = export.instances.len()).entered();
    let mut scene = PtScene::new(PtCamera::pinhole(
        export.eye,
        export.target,
        export.fov_y,
        export.width.max(1),
        export.height.max(1),
    ));
    let mut textures = Textures {
        list: Vec::new(),
        seen: HashMap::new(),
        _marker: std::marker::PhantomData,
    };
    let mut meshes: HashMap<u64, usize> = HashMap::new();
    let mut materials: HashMap<u64, usize> = HashMap::new();
    for inst in &export.instances {
        let (Some(cpu), Some(mat)) = (
            export.meshes.get(&inst.mesh_key),
            export.materials.get(&inst.material_key),
        ) else {
            continue;
        };
        if skipped(mat, true) {
            continue;
        }
        let mesh = if inst.joints.is_empty() {
            *meshes.entry(inst.mesh_key).or_insert_with(|| {
                scene.meshes.push(pt_mesh(&cpu.vertices, &cpu.indices, &[]));
                scene.meshes.len() - 1
            })
        } else {
            scene.add_mesh(pt_mesh(&cpu.vertices, &cpu.indices, &inst.joints))
        };
        let m = match materials.get(&inst.material_key) {
            Some(&m) => m,
            None => {
                let m = scene.add_material(material(mat, &mut textures));
                materials.insert(inst.material_key, m);
                // Graph materials as in `from_frame` (ADR-2725); the export carries each graph's
                // parameter block as the frame last wrote it. The standard emissive is zeroed
                // only when the graph composes, as in `from_frame`.
                if let Some(graph) = mat.surface_graph.as_deref()
                    && let Some(entry) = graph_entry(graph, m, &mut textures, options)
                {
                    if crate::graph::composes(&entry) {
                        scene.materials[m].emissive = [0.0; 3];
                        scene.materials[m].emissive_map = None;
                    }
                    scene.surface_graphs.push(entry);
                }
                if options.subsurface
                    && let Some(d) = burley_d(&mat.subsurface)
                {
                    scene.subsurface.push(crate::scene::PtSubsurface {
                        material: m,
                        d,
                        tint: mat.subsurface.tint,
                    });
                }
                m
            }
        };
        scene.add_instance(mesh, m, Mat4::from_cols_array_2d(&inst.model));
    }
    scene.textures = textures.list;
    let eye_height = export.eye[1] - ground_below(terrain, export.eye);
    let env = environment(
        &export.outdoor,
        export.sun,
        export.ibl,
        export.ambient_lux,
        eye_height,
    );
    apply_environment(&mut scene, env);
    scene.extra_suns = extra_suns(
        &export.extra_directional,
        export.outdoor.planet.sun_angular_radius,
    );
    push_local_lights(&mut scene, &export.local_lights);
    if let Some(t) = terrain {
        add_terrain(&mut scene, heightfield(t));
    }
    scene
}

/// Adds the frame's point and spot lights to `scene`.
///
/// A point light with a source radius (`LightSourceSize`, ADR-1367) becomes a sphere light of
/// that radius and the same intensity, `radiance = candela / (π r²)`, so the reference sees the
/// size the forward specular and the PCSS shadow use. A spot light with a source radius becomes a
/// [`PtLight::SphereSpot`] (ADR-2592): the same sphere, its radiance shaped by the spot's cone.
pub fn push_local_lights(scene: &mut PtScene, lights: &[LabLocalLight]) {
    for light in lights {
        match *light {
            LabLocalLight::Point(p) => {
                // Lumens → candela (KHR_lights_punctual § 5.3); physical inverse square, no window.
                let cd = p.intensity / (4.0 * std::f32::consts::PI);
                let intensity = [p.color[0] * cd, p.color[1] * cd, p.color[2] * cd];
                let radius = regia_render::light_source::specular_source_radius(p.source_radius);
                match regia_render::light_source::sphere_radiance_scale(radius) {
                    Some(scale) => scene.lights.push(PtLight::Sphere {
                        position: p.position,
                        radius,
                        radiance: intensity.map(|c| c * scale),
                    }),
                    None => scene.lights.push(PtLight::Point {
                        position: p.position,
                        intensity,
                    }),
                }
            }
            LabLocalLight::Spot(s) => {
                let intensity = [
                    s.color[0] * s.intensity,
                    s.color[1] * s.intensity,
                    s.color[2] * s.intensity,
                ];
                let radius = regia_render::light_source::specular_source_radius(s.source_radius);
                match regia_render::light_source::sphere_radiance_scale(radius) {
                    Some(scale) => scene.lights.push(PtLight::SphereSpot {
                        position: s.position,
                        radius,
                        radiance: intensity.map(|c| c * scale),
                        direction: s.direction,
                        inner_cos: s.inner_angle.cos(),
                        outer_cos: s.outer_angle.cos(),
                    }),
                    None => scene.lights.push(PtLight::Spot {
                        position: s.position,
                        direction: s.direction,
                        intensity,
                        inner_cos: s.inner_angle.cos(),
                        outer_cos: s.outer_angle.cos(),
                    }),
                }
            }
        }
    }
}

/// Puts the IES profiles of a frame's lights (`regia_render::light_profiles::LightExtras`, ADR-2592) on the
/// lights [`push_local_lights`] added for `lights`, starting at `scene.lights[first]`. A profile with a peak
/// (`file_intensity`, or a point light's lumens spread over the profile) rescales the light's intensity to it.
pub fn push_light_profiles(
    scene: &mut PtScene,
    first: usize,
    lights: &[LabLocalLight],
    extras: &regia_render::light_profiles::LightExtras,
) {
    for (i, uses) in extras.per_light.iter().enumerate() {
        let (Some(uses), Some(lab)) = (uses, lights.get(i)) else {
            continue;
        };
        let Some(profile) = usize::from(uses.slot)
            .checked_sub(1)
            .and_then(|s| extras.profiles.get(s))
        else {
            continue;
        };
        let index = first + i;
        if let (Some(peak), Some(light)) = (uses.peak_cd, scene.lights.get_mut(index)) {
            let own = match *lab {
                LabLocalLight::Point(p) => p.intensity / (4.0 * std::f32::consts::PI),
                LabLocalLight::Spot(s) => s.intensity,
            };
            let k = if own > 0.0 { peak / own } else { 0.0 };
            match light {
                PtLight::Point { intensity, .. } | PtLight::Spot { intensity, .. } => {
                    *intensity = intensity.map(|c| c * k);
                }
                PtLight::Sphere { radiance, .. } | PtLight::SphereSpot { radiance, .. } => {
                    *radiance = radiance.map(|c| c * k);
                }
                PtLight::Quad { .. } => {}
            }
        }
        scene.light_profiles.push(crate::PtLightProfile {
            light: index,
            profile: profile.clone(),
            nadir: uses.nadir,
            turn: uses.turn,
        });
    }
}

/// Adds authored rect and tube lights (`regia_render::RectLight`, `TubeLight`, ADR-2592) to `scene`: a rect is a
/// [`PtLight::Quad`] of the same radiance; a tube is a 12-sided prism of outward quads around its axis (the same
/// lateral radiance; its area is 1.1 % under the cylinder's, so the prism's radiance is raised to keep the flux).
pub fn push_area_lights(scene: &mut PtScene, lights: &[regia_render::light_profiles::AreaLightInput]) {
    use regia_core::glam::Vec3;
    use regia_render::emissive_lights::EmitterShape;
    for a in lights {
        let l = &a.light;
        let c = Vec3::from_array(l.centre);
        let ex = Vec3::from_array(l.axis_x);
        match l.shape {
            EmitterShape::Quad { two_sided, .. } => {
                let ey = Vec3::from_array(l.axis_y);
                scene.lights.push(PtLight::Quad {
                    corner: (c - ex - ey).to_array(),
                    edge_u: (ex * 2.0).to_array(),
                    edge_v: (ey * 2.0).to_array(),
                    radiance: l.radiance,
                    two_sided,
                });
            }
            EmitterShape::Tube { radius } => {
                const SIDES: usize = 12;
                let axis = ex.normalize_or(Vec3::X);
                let u = axis.any_orthonormal_vector();
                let v = axis.cross(u);
                // The prism's lateral area is 2·n·sin(π/n)·r·L against the cylinder's 2π·r·L.
                let n = SIDES as f32;
                let gain = std::f32::consts::PI / (n * (std::f32::consts::PI / n).sin());
                let radiance = l.radiance.map(|r| r * gain);
                for k in 0..SIDES {
                    let a0 = std::f32::consts::TAU * k as f32 / n;
                    let a1 = std::f32::consts::TAU * (k + 1) as f32 / n;
                    let p0 = c + (u * a0.cos() + v * a0.sin()) * radius;
                    let p1 = c + (u * a1.cos() + v * a1.sin()) * radius;
                    // edge_u × edge_v points outward (the emitting side).
                    scene.lights.push(PtLight::Quad {
                        corner: (p0 - ex).to_array(),
                        edge_u: (p1 - p0).to_array(),
                        edge_v: (ex * 2.0).to_array(),
                        radiance,
                        two_sided: false,
                    });
                }
            }
        }
    }
}

/// The tracer's scene for view `view_index` of the runtime's extracted frame (what
/// `seeing.reference_trace` with `scene: "current"` traces when the renderer has no
/// [`FrameExport`]). The sun is the frame renderer's ([`effective_sun`]) and the light around
/// the scene is [`environment`]. Terrain drawn through a `SharedPassDraw` is not part of the
/// extracted frame; use [`from_export`] or add it with [`ExtractOptions::terrain`] via
/// [`from_frame`].
#[must_use]
pub fn from_extracted(
    frame: &regia_render::runtime::ExtractedFrame,
    view_index: usize,
    width: u32,
    height: u32,
) -> PtScene {
    from_extracted_with(frame, view_index, width, height, &ExtractOptions::default())
}

/// [`from_extracted`] with the graph and subsurface fields of `options` (as
/// [`from_export_with`]; ADR-2725). The sky, sun, terrain and media come from the frame.
#[must_use]
pub fn from_extracted_with(
    frame: &regia_render::runtime::ExtractedFrame,
    view_index: usize,
    width: u32,
    height: u32,
    options: &ExtractOptions,
) -> PtScene {
    let fs = frame.frame_scene(view_index, width, height);
    let mut scene = from_frame(
        &fs,
        &ExtractOptions {
            sky: Some(PtSky::None),
            sun_angular_radius: frame.outdoor.planet.sun_angular_radius,
            skip_blended: true,
            material_clock: options.material_clock,
            foliage_wind: options.foliage_wind,
            origin_offset: options.origin_offset,
            subsurface: options.subsurface,
            ..ExtractOptions::default()
        },
    );
    let sun = if fs.ibl {
        effective_sun(&frame.outdoor, fs.directional, fs.eye[1])
    } else {
        fs.directional
    };
    let env = environment(&frame.outdoor, sun, fs.ibl, fs.ambient_lux, fs.eye[1]);
    apply_environment(&mut scene, env);
    scene.extra_suns = extra_suns(
        &frame.lights_views.extra_directional,
        frame.outdoor.planet.sun_angular_radius,
    );
    scene
}

/// The frame's extra directional lights as the tracer's suns (ADR-7211, ADR-7254): the first
/// `regia_render::MAX_EXTRA_DIRECTIONAL` the frame draws, each a disc of the planet's angular
/// radius (as the sun), unlit ones dropped. The atmosphere does not tint them: the forward
/// shades them with their own colour.
#[must_use]
pub fn extra_suns(lights: &[DirectionalLightUniform], angular_radius: f32) -> Vec<PtSun> {
    lights
        .iter()
        .take(regia_render::MAX_EXTRA_DIRECTIONAL)
        .filter(|l| l.illuminance > 0.0)
        .map(|l| PtSun {
            toward: (-Vec3::from_array(l.direction).normalize_or(Vec3::NEG_Y)).to_array(),
            color: l.color,
            illuminance: l.illuminance,
            angular_radius: angular_radius.max(0.0),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outdoor_media_mirror_the_runtime_parameters() {
        let mut params = regia_render::OutdoorParams {
            planet: regia_render::AtmosphereParameters::mars(),
            ..Default::default()
        };
        params.fog.density = 2.0e-3;
        params.fog.base_height = 10.0;
        let media = media_from_outdoor(&params);
        // Rayleigh and Mie, each above and below the datum, then the fog.
        assert_eq!(media.layers.len(), 5);
        assert!(media.layers[..4].iter().all(|l| l.aerial));
        let ray = media.layers[0];
        assert!((ray.scattering[2] - params.planet.rayleigh_scattering[2] / 1000.0).abs() < 1e-12);
        assert!((ray.falloff - 1.0 / (params.planet.rayleigh_scale_height_km * 1000.0)).abs() < 1e-9);
        let fog = media.layers[4];
        assert!(!fog.aerial);
        let (albedo, _) = params.fog_medium();
        for ((s, a), want) in fog.scattering.iter().zip(&fog.absorption).zip(&albedo) {
            let t = s + a;
            assert!((t - 2.0e-3).abs() < 1e-9);
            assert!((s / t - want).abs() < 1e-5);
        }
        assert_eq!(media.escape_distance, regia_render::FOG_LAYER_RANGE_M);
        // No air on an airless moon.
        let airless = regia_render::OutdoorParams {
            planet: regia_render::AtmosphereParameters::airless(),
            ..Default::default()
        };
        assert!(media_from_outdoor(&airless).layers.is_empty());
        assert_eq!(clouds_from_outdoor(&airless), None);
        // Earth with a layer: the field holds the layer, and no layer is no field.
        let mut cloudy = regia_render::OutdoorParams::default();
        assert_eq!(clouds_from_outdoor(&cloudy), None);
        cloudy.clouds = regia_render::CloudSettings::coverage_layer(0.4);
        let field = clouds_from_outdoor(&cloudy).expect("a field");
        assert_eq!(field.settings, cloudy.clouds);
        assert_eq!(field.planet_radius_m, cloudy.planet.bottom_radius_km * 1000.0);
    }

    #[test]
    fn moons_sample_their_albedo_map_in_the_tracer() {
        let moon = regia_render::SkyBody::io_from_europa();
        let c = Vec3::from_array(moon.direction).normalize();
        let to_sun = (-c).to_array();
        let sun_lux = 4400.0;
        let red = regia_render::SkyBodyTexture::new(1, 1, vec![255, 0, 0, 255]).expect("1x1");

        // The CPU mirror samples the map: a pure-red map removes the green and blue channels
        // while leaving the red channel at the tint's value.
        let view = c.to_array();
        let tinted = lit_sphere_radiance(view, &moon, None, to_sun, sun_lux);
        let textured = lit_sphere_radiance(view, &moon, Some(&red), to_sun, sun_lux);
        assert!(
            tinted[1] > 0.05 && tinted[2] > 0.02,
            "tint has green/blue: {tinted:?}"
        );
        assert!(
            textured[1] < 1e-3 && textured[2] < 1e-3,
            "red map removed green/blue: {textured:?}"
        );
        assert!(
            (textured[0] - tinted[0]).abs() < 1e-3,
            "red map leaves the red channel: {tinted:?} vs {textured:?}"
        );

        // The plumbing passes each moon's map: slot 0 textured, slot 1 still tint-only.
        let params = regia_render::OutdoorParams {
            planet: regia_render::AtmosphereParameters::airless(),
            moons: vec![moon, moon],
            moon_albedo: [Some(std::sync::Arc::new(red)), None],
            ..Default::default()
        };
        let bodies = sky_bodies_from_outdoor(&params, to_sun);
        assert_eq!(bodies.len(), 2);
        let centre = |b: &PtSkyBody| {
            let n = b.size as usize;
            b.radiance[n / 2 * n + n / 2]
        };
        let m0 = centre(&bodies[0]);
        let m1 = centre(&bodies[1]);
        assert!(m0[1] < 1e-3 && m0[2] < 1e-3, "moon0 sampled its red map: {m0:?}");
        assert!(m1[1] > 0.05, "moon1 stays tint-only: {m1:?}");
    }

    #[test]
    fn a_sky_body_lights_with_the_runtime_fill() {
        let body = regia_render::SkyBody {
            bands: 0.0,
            ..regia_render::SkyBody::jupiter_from_europa()
        };
        let c = Vec3::from_array(body.direction).normalize();
        // Sun behind the viewer: full phase, the disc centre is tint · E / π.
        let to_sun = (-c).to_array();
        let l = lit_sphere_radiance(c.to_array(), &body, None, to_sun, 4400.0);
        let lum = 0.2126 * l[0] + 0.7152 * l[1] + 0.0722 * l[2];
        let tint = 0.2126 * body.tint[0] + 0.7152 * body.tint[1] + 0.0722 * body.tint[2];
        assert!(
            (lum / (tint * 4400.0 / std::f32::consts::PI) - 1.0).abs() < 1e-3,
            "{lum}"
        );
        let mut params = regia_render::OutdoorParams {
            sky_body: Some(body),
            ..Default::default()
        };
        params.planet.sun_illuminance = 4400.0;
        let bodies = sky_bodies_from_outdoor(&params, to_sun);
        assert_eq!(bodies.len(), 1);
        let lit = bodies[0].normal_irradiance() * bodies[0].light_scale;
        let want = body.fill_illuminance(4400.0, to_sun);
        assert!((lit / want - 1.0).abs() < 1e-3, "{lit} vs {want}");
        // An authored fill wins.
        params.sky_body = Some(regia_render::SkyBody {
            fill_lux: Some(50.0),
            ..body
        });
        let bodies = sky_bodies_from_outdoor(&params, to_sun);
        assert!((bodies[0].normal_irradiance() * bodies[0].light_scale - 50.0).abs() < 0.05);
    }
    use regia_core::glam::Vec3;
    use regia_render::{
        DirectionalLightUniform, FrameMesh, FrameQuality, LabPointLight, LabVertex, ShadowCast,
    };

    #[test]
    fn skinned_meshes_take_the_frames_pose_and_materials_keep_transmission() {
        let mut v = LabVertex::from_pos_normal_uv([1.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0]);
        v.joints = [0, 1, 0, 0];
        v.weights = [0.5, 0.5, 0.0, 0.0];
        let joints = [
            JointMatrix::translation(0.0, 2.0, 0.0),
            JointMatrix::from_mat4(Mat4::from_rotation_z(std::f32::consts::FRAC_PI_2)),
        ];
        let mesh = pt_mesh(&[v], &[0, 0, 0], &joints);
        // 0.5 · (1, 2, 0) + 0.5 · (0, 1, 0) = (0.5, 1.5, 0).
        let p = mesh.positions[0];
        assert!((p[0] - 0.5).abs() < 1e-6 && (p[1] - 1.5).abs() < 1e-6, "{p:?}");
        let n = Vec3::from_array(mesh.normals[0]);
        assert!((n - Vec3::new(1.0, 1.0, 0.0).normalize()).length() < 1e-5, "{n}");
        // No palette: the bind pose.
        assert_eq!(pt_mesh(&[v], &[0, 0, 0], &[]).positions[0], [1.0, 0.0, 0.0]);

        let glass = LabMaterial {
            transmission: 0.9,
            ior: 1.33,
            transmission_tint: [0.8, 0.9, 1.0],
            alpha_mode: regia_render::ALPHA_PREMULTIPLIED,
            ..LabMaterial::EMPTY
        };
        let mut textures = Textures {
            list: Vec::new(),
            seen: HashMap::new(),
            _marker: std::marker::PhantomData,
        };
        let m = material(&glass, &mut textures);
        assert_eq!(m.transmission, 0.9);
        assert_eq!(m.transmission_tint, [0.8, 0.9, 1.0]);
        assert!((m.dielectric_f0() - (0.33f32 / 2.33).powi(2)).abs() < 1e-6);
        // Glass is traced; a blended VFX layer is not.
        assert!(!skipped(&glass, true));
        let smoke = LabMaterial {
            alpha_mode: regia_render::ALPHA_BLEND,
            ..LabMaterial::EMPTY
        };
        assert!(skipped(&smoke, true));
        assert!(!skipped(&smoke, false));
    }

    /// Review w11 #2: a sorted hair card the host left `ALPHA_OPAQUE` is a blended layer to the
    /// tracer, and a hashed one is alpha-tested, as the raster draws them.
    #[test]
    fn hair_cards_trace_in_their_effective_alpha_mode() {
        let mut card = LabMaterial {
            hair: regia_render::HairFactors {
                alpha: regia_render::HairAlpha::Sorted,
                ..regia_render::HairFactors::strands()
            },
            ..LabMaterial::EMPTY
        };
        assert_eq!(card.alpha_mode, regia_render::ALPHA_OPAQUE);
        assert!(skipped(&card, true), "sorted hair is a blended layer");
        card.hair.alpha = regia_render::HairAlpha::Hashed;
        assert!(!skipped(&card, true));
        let mut textures = Textures {
            list: Vec::new(),
            seen: HashMap::new(),
            _marker: std::marker::PhantomData,
        };
        assert_eq!(
            material(&card, &mut textures).alpha_mode,
            regia_render::ALPHA_MASK
        );
    }

    #[test]
    fn airless_environment_has_black_sky_stars_sun_and_jupiter() {
        let outdoor = OutdoorParams {
            planet: regia_render::AtmosphereParameters::airless(),
            sky_body: Some(regia_render::SkyBody {
                fill_lux: Some(1600.0),
                ..regia_render::SkyBody::jupiter_from_europa()
            }),
            moons: vec![regia_render::SkyBody::io_from_europa()],
            ..OutdoorParams::default()
        };
        let sun = DirectionalLightUniform::new([0.0, -0.5, 0.86], [1.0; 3], 4_400.0);
        let env = environment(&outdoor, sun, true, 0.0, 2.0);
        assert!(env.media.layers.is_empty(), "no air");
        assert!(env.stars > 0.0);
        assert_eq!(env.stars, star_radiance(&outdoor.planet));
        // Io, then Jupiter (drawn in front), Jupiter lighting with the authored fill.
        assert_eq!(env.sky_bodies.len(), 2);
        let jupiter = &env.sky_bodies[1];
        let e = jupiter.normal_irradiance() * jupiter.light_scale;
        assert!((e - 1600.0).abs() < 1600.0 * 1e-3, "{e}");
        assert_eq!(env.sky_bodies[0].light_scale, 1.0, "moons light physically");
        let s = env.sun.expect("sun");
        assert_eq!(s.illuminance, 4_400.0);
        assert!((s.angular_radius - outdoor.planet.sun_angular_radius).abs() < 1e-9);
        // The table of an airless planet is black.
        match &env.sky {
            PtSky::Table { texels, .. } => {
                assert!(texels.iter().all(|t| t[0] + t[1] + t[2] < 1e-3));
            }
            other => panic!("unexpected sky {other:?}"),
        }
        // A planet with air: no stars, media, the same bodies.
        let earth = OutdoorParams {
            planet: regia_render::AtmosphereParameters::earth(),
            ..outdoor
        };
        let env = environment(&earth, sun, true, 0.0, 2.0);
        assert_eq!(env.stars, 0.0);
        assert!(!env.media.layers.is_empty());
        assert_eq!(env.sky_bodies.len(), 2);
    }

    #[test]
    fn star_radiance_is_airless_only_and_from_frame_carries_it() {
        // The raster sky's rule: `day_star_radiance` on an airless planet, nothing where there
        // is air (stars show only at night there, which the tracer does not reproduce).
        assert_eq!(
            star_radiance(&regia_render::AtmosphereParameters::airless()),
            regia_render::AtmosphereParameters::airless().day_star_radiance
        );
        assert!(star_radiance(&regia_render::AtmosphereParameters::airless()) > 0.0);
        assert_eq!(star_radiance(&regia_render::AtmosphereParameters::earth()), 0.0);
        assert_eq!(star_radiance(&regia_render::AtmosphereParameters::mars()), 0.0);

        // `from_frame` (the `pt_plates` path) carries the star field through the options, so it
        // matches what `environment` (the `from_extracted`/`from_export` path) sets.
        let frame = FrameScene {
            width: 8,
            height: 8,
            eye: [0.0, 1.0, 5.0],
            target: [0.0; 3],
            fov_y: 0.9,
            near: 0.1,
            far: 100.0,
            depth_prepass: false,
            directional_shadows: false,
            ibl: true,
            exposure_ev: 10.0,
            ambient_lux: 0.0,
            double_sided: false,
            post: None,
            fog: regia_render::FogSettings::default(),
            directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 1000.0),
            local_lights: &[],
            quality: FrameQuality::LAB,
            meshes: vec![],
        };
        // Default: no stars.
        assert_eq!(from_frame(&frame, &ExtractOptions::default()).stars, 0.0);
        // The airless star field flows through to the scene.
        let stars = star_radiance(&regia_render::AtmosphereParameters::airless());
        let scene = from_frame(
            &frame,
            &ExtractOptions {
                stars,
                ..ExtractOptions::default()
            },
        );
        assert_eq!(scene.stars, stars);
    }

    #[test]
    fn instances_share_meshes_and_lights_convert() {
        let verts = vec![
            LabVertex::from_pos_normal_uv([0.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0]),
            LabVertex::from_pos_normal_uv([1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([0.0, 0.0, 1.0], [0.0, 1.0, 0.0], [0.0, 1.0]),
        ];
        let idx = vec![0u32, 1, 2];
        let mat = LabMaterial {
            emissive: [1.0, 0.5, 0.25],
            emissive_intensity: 4.0,
            ..LabMaterial::EMPTY
        };
        let mesh = |x: f32| FrameMesh {
            is_static: false,
            mesh_key: 7,
            material_key: 0,
            object_key: 0,
            vertices: &verts,
            indices: &idx,
            material: &mat,
            model: Mat4::from_translation(regia_core::glam::Vec3::new(x, 0.0, 0.0)).to_cols_array_2d(),
            joints: &[],
            receiver_layer: 0,
            shadow: ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        };
        let lights = [LabLocalLight::Point(LabPointLight {
            position: [0.0, 2.0, 0.0],
            color: [1.0; 3],
            intensity: 4.0 * std::f32::consts::PI * 100.0,
            radius: 10.0,
            cast_shadows: false,
            source_radius: None,
        })];
        let frame = FrameScene {
            width: 32,
            height: 16,
            eye: [0.0, 1.0, 5.0],
            target: [0.0; 3],
            fov_y: 0.9,
            near: 0.1,
            far: 100.0,
            depth_prepass: false,
            directional_shadows: false,
            ibl: false,
            exposure_ev: 10.0,
            ambient_lux: 300.0,
            double_sided: false,
            post: None,
            fog: regia_render::FogSettings::default(),
            directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 1000.0),
            local_lights: &lights,
            quality: FrameQuality::LAB,
            meshes: vec![mesh(0.0), mesh(3.0), mesh(6.0)],
        };
        let scene = from_frame(&frame, &ExtractOptions::default());
        assert_eq!(scene.meshes.len(), 1);
        assert_eq!(scene.instances.len(), 3);
        assert_eq!(scene.materials.len(), 1);
        assert_eq!(scene.materials[0].emissive, [4.0, 2.0, 1.0]);
        assert_eq!(scene.sun.map(|s| s.toward), Some([0.0, 1.0, 0.0]));
        match scene.lights[0] {
            PtLight::Point { intensity, .. } => assert!((intensity[0] - 100.0).abs() < 1e-3),
            other => panic!("unexpected light {other:?}"),
        }
        match scene.sky {
            PtSky::Uniform(c) => assert!((c[0] - 300.0 / std::f32::consts::PI).abs() < 1e-3),
            ref other => panic!("unexpected sky {other:?}"),
        }
    }

    /// A material graph the tracer refuses leaves the standard emissive alone: the documented
    /// fallback is "traces with its standard factors". A graph that composes takes the emissive
    /// over (the forward pass ignores the standard block there, ADR-2725).
    #[test]
    fn a_refused_graph_keeps_the_standard_emissive() {
        let graph = |body: &str| {
            std::sync::Arc::new(regia_render::SurfaceGraph::new(format!(
                "fn material_surface(in: regia::pbr::surface::SurfaceInput) -> regia::pbr::surface::Surface {{\n    {body}\n    return regia::pbr::surface::Surface(vec3<f32>(0.5), 1.0, 0.0, 0.5, in.world_normal, 1.0, vec3<f32>(0.0), 0.0);\n}}"
            )))
        };
        let lab = |surface_graph: Option<std::sync::Arc<regia_render::SurfaceGraph>>| LabMaterial {
            emissive: [4.0, 2.0, 1.0],
            emissive_intensity: 1.0,
            surface_graph,
            ..LabMaterial::EMPTY
        };
        let verts = vec![
            LabVertex::from_pos_normal_uv([0.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0]),
            LabVertex::from_pos_normal_uv([1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([0.0, 0.0, 1.0], [0.0, 1.0, 0.0], [0.0, 1.0]),
        ];
        let idx = vec![0u32, 1, 2];
        let scene_of = |mat: &LabMaterial| {
            let mesh = FrameMesh {
                is_static: false,
                mesh_key: 7,
                material_key: 0,
                object_key: 0,
                vertices: &verts,
                indices: &idx,
                material: mat,
                model: Mat4::IDENTITY.to_cols_array_2d(),
                joints: &[],
                receiver_layer: 0,
                shadow: ShadowCast::All,
                uv1: &[],
                lod_dither: 0.0,
            };
            let frame = FrameScene {
                width: 32,
                height: 16,
                eye: [0.0, 1.0, 5.0],
                target: [0.0; 3],
                fov_y: 0.9,
                near: 0.1,
                far: 100.0,
                depth_prepass: false,
                directional_shadows: false,
                ibl: false,
                exposure_ev: 10.0,
                ambient_lux: 300.0,
                double_sided: false,
                post: None,
                fog: regia_render::FogSettings::default(),
                directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 1000.0),
                local_lights: &[],
                quality: FrameQuality::LAB,
                meshes: vec![mesh],
            };
            from_frame(&frame, &ExtractOptions::default())
        };
        let refused = lab(Some(graph("let edge = fwidth(in.uv.x);")));
        let scene = scene_of(&refused);
        assert_eq!(
            scene.materials[0].emissive,
            [4.0, 2.0, 1.0],
            "a refused graph traces with its standard factors"
        );
        assert_eq!(scene.surface_graphs.len(), 1, "the refusal is still reported");
        let composed = lab(Some(graph("let shade = 0.5;")));
        let scene = scene_of(&composed);
        assert_eq!(
            scene.materials[0].emissive, [0.0; 3],
            "a composed graph's own emission shades the hit"
        );
        let plain = lab(None);
        let scene = scene_of(&plain);
        assert_eq!(scene.materials[0].emissive, [4.0, 2.0, 1.0]);
    }

    /// ADR-1367: a point light with a source radius is traced as a sphere of that radius and the
    /// same intensity; without one (or with 0) it stays a point; a spot stays a spot.
    #[test]
    fn a_light_source_size_becomes_a_sphere_light_of_the_same_intensity() {
        let point = |source_radius: Option<f32>| {
            LabLocalLight::Point(LabPointLight {
                position: [0.0, 2.0, 0.0],
                color: [1.0, 0.5, 0.25],
                intensity: 4.0 * std::f32::consts::PI * 100.0,
                radius: 10.0,
                cast_shadows: false,
                source_radius,
            })
        };
        let spot = LabLocalLight::Spot(regia_render::LabSpotLight {
            position: [0.0; 3],
            direction: [0.0, -1.0, 0.0],
            color: [1.0; 3],
            intensity: 50.0,
            range: 5.0,
            inner_angle: 0.2,
            outer_angle: 0.4,
            cast_shadows: false,
            source_radius: Some(0.1),
        });
        let mut scene = PtScene::new(PtCamera::pinhole([0.0, 0.0, 5.0], [0.0; 3], 0.9, 8, 8));
        push_local_lights(
            &mut scene,
            &[point(Some(0.2)), point(None), point(Some(0.0)), spot],
        );
        assert_eq!(scene.lights.len(), 4);
        match scene.lights[0] {
            PtLight::Sphere { radius, radiance, .. } => {
                assert_eq!(radius, 0.2);
                let area = std::f32::consts::PI * radius * radius;
                for (c, want) in radiance.iter().zip([100.0f32, 50.0, 25.0]) {
                    assert!((c * area - want).abs() < 1e-3, "{c} × {area} vs {want}");
                }
            }
            other => panic!("expected a sphere, got {other:?}"),
        }
        assert!(matches!(scene.lights[1], PtLight::Point { .. }));
        assert!(matches!(scene.lights[2], PtLight::Point { .. }));
        // ADR-2592: a sized spot is a cone-limited sphere of the same intensity (it was a delta spot).
        match scene.lights[3] {
            PtLight::SphereSpot {
                radius,
                radiance,
                inner_cos,
                outer_cos,
                ..
            } => {
                assert_eq!(radius, 0.1);
                let area = std::f32::consts::PI * radius * radius;
                assert!((radiance[0] * area - 50.0).abs() < 1e-3);
                assert_eq!((inner_cos, outer_cos), (0.2f32.cos(), 0.4f32.cos()));
            }
            other => panic!("expected a sized spot, got {other:?}"),
        }
        let unsized_spot = PtLight::Spot {
            position: [0.0; 3],
            direction: [0.0, -1.0, 0.0],
            intensity: [50.0; 3],
            inner_cos: 0.2f32.cos(),
            outer_cos: 0.4f32.cos(),
        };
        let (sized, delta) = (scene.lights[3].power(), unsized_spot.power());
        assert!((sized / delta - 1.0).abs() < 1e-4, "{sized} vs {delta}");
        // Power-weighted selection sees the same power for the sphere and the point.
        let (sphere, point) = (scene.lights[0].power(), scene.lights[1].power());
        assert!((sphere / point - 1.0).abs() < 1e-4, "{sphere} vs {point}");
    }

    /// ADR-2725: Burley's radial density `(e^(-r/d) + e^(-r/3d)) / 4d` has `E[r²] = 14 d²`, the constant
    /// `burley_d` matches the frame's Gaussians with; skin reaches furthest in red, a wider reach scales
    /// `d` linearly, and a material that does not scatter has none.
    #[test]
    fn burley_d_matches_the_frames_second_moment() {
        let d = 0.003f64;
        let (mut m2, mut total) = (0.0f64, 0.0f64);
        let dr = d / 400.0;
        for k in 0..400 * 80 {
            let r = (f64::from(k) + 0.5) * dr;
            let p = ((-r / d).exp() + (-r / (3.0 * d)).exp()) / (4.0 * d);
            m2 += r * r * p * dr;
            total += p * dr;
        }
        assert!((total - 1.0).abs() < 1e-4, "{total}");
        assert!((m2 / (d * d) - 14.0).abs() < 1e-2, "{}", m2 / (d * d));

        let skin = regia_render::SubsurfaceFactors::new(regia_render::SubsurfaceProfile::Skin);
        let ds = burley_d(&skin).expect("skin scatters");
        assert!(ds[0] > ds[1] && ds[1] > ds[2], "{ds:?}");
        // Gaussians: E[r²] = Σ w 2v / Σ w per channel, so 14 d² equals that (meters → mm²).
        for (c, d) in ds.iter().enumerate() {
            let (mut num, mut den) = (0.0f64, 0.0f64);
            for (v, w) in regia_render::SubsurfaceProfile::Skin.gaussians() {
                num += f64::from(w[c]) * 2.0 * f64::from(*v);
                den += f64::from(w[c]);
            }
            let d_mm = f64::from(*d) * 1e3;
            assert!(
                (14.0 * d_mm * d_mm - num / den).abs() < 1e-3 * (num / den),
                "channel {c}"
            );
        }
        let wide = regia_render::SubsurfaceFactors {
            radius_mm: 2.0 * skin.reach_mm(),
            ..skin
        };
        let dw = burley_d(&wide).expect("wide");
        for c in 0..3 {
            assert!((dw[c] / ds[c] - 2.0).abs() < 1e-4);
        }
        assert_eq!(burley_d(&regia_render::SubsurfaceFactors::OFF), None);
    }

    /// ADR-2725: the material clock reaches only the graphs that read time.
    #[test]
    fn the_material_clock_reaches_only_time_reading_graphs() {
        let graph = |uses_time| crate::scene::PtSurfaceGraph {
            material: 0,
            surface: "fn material_surface() {}".into(),
            params: [[0.5; 4]; regia_render::surface_graph::GRAPH_PARAM_SLOTS],
            textures: [None; regia_render::surface_graph::GRAPH_TEXTURE_SLOTS],
            vertex_offset: false,
            uses_uv1: false,
            uses_time,
        };
        let mut scene = PtScene::new(PtCamera::pinhole([0.0; 3], [0.0, 0.0, -1.0], 1.0, 4, 4));
        scene.surface_graphs = vec![graph(true), graph(false)];
        apply_material_clock(&mut scene, (2.5, 2.25));
        assert_eq!(scene.surface_graphs[0].params[0], [2.5, 2.25, 0.5, 0.5]);
        assert_eq!(scene.surface_graphs[1].params[0], [0.5; 4]);
        assert_eq!(scene.surface_graphs[0].params[1], [0.5; 4]);
    }
}
