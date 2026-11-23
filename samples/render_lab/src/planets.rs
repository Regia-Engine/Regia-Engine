// SPDX-License-Identifier: MIT-0
//! `render_lab planets [--out DIR]`: the M15c planet presets on a procedural heightfield.
//!
//! Earth noon and dusk under a cumulus layer, Mars noon and sunset, Europa with Jupiter.
//! Desktop High, 960×540, fixed exposure per shot so the look is comparable run to run.

use regia_render::{
    AtmosphereParameters, CloudSettings, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene,
    LabVertex, OutdoorParams, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, SkyBody,
    TimeOfDay,
};

/// One evidence shot.
pub struct Shot {
    /// File stem.
    pub name: &'static str,
    /// Planet medium.
    pub planet: AtmosphereParameters,
    /// Sun clock.
    pub time: TimeOfDay,
    /// Camera eye (metres).
    pub eye: [f32; 3],
    /// Camera target.
    pub target: [f32; 3],
    /// Fixed EV100.
    pub ev: f32,
    /// Ground albedo.
    pub albedo: [f32; 3],
    /// Cloud coverage (0 none).
    pub clouds: f32,
    /// Sky body.
    pub body: Option<SkyBody>,
}

/// The five M15c evidence shots.
#[must_use]
pub fn shots() -> Vec<Shot> {
    let earth = AtmosphereParameters::earth();
    let mars = AtmosphereParameters::mars();
    let airless = AtmosphereParameters::airless();
    vec![
        Shot {
            name: "earth_noon_clouds",
            planet: earth,
            time: TimeOfDay {
                hour: 10.0,
                day_of_year: 172,
                latitude_deg: 40.0,
            },
            eye: [0.0, 60.0, 0.0],
            target: [0.0, 260.0, 3000.0],
            ev: 14.6,
            albedo: [0.16, 0.19, 0.10],
            clouds: 0.3,
            body: None,
        },
        Shot {
            name: "earth_dusk_clouds",
            planet: earth,
            time: TimeOfDay {
                hour: 19.25,
                day_of_year: 172,
                latitude_deg: 40.0,
            },
            eye: [0.0, 60.0, 0.0],
            target: [-3000.0, 260.0, 600.0],
            ev: 11.0,
            albedo: [0.16, 0.19, 0.10],
            clouds: 0.45,
            body: None,
        },
        Shot {
            name: "mars_noon",
            planet: mars,
            time: TimeOfDay {
                hour: 11.5,
                day_of_year: 172,
                latitude_deg: 20.0,
            },
            eye: [0.0, 40.0, 0.0],
            target: [0.0, 140.0, 3000.0],
            ev: 13.6,
            albedo: [0.42, 0.22, 0.11],
            clouds: 0.0,
            body: None,
        },
        Shot {
            name: "mars_sunset",
            planet: mars,
            time: TimeOfDay {
                hour: 18.55,
                day_of_year: 172,
                latitude_deg: 20.0,
            },
            eye: [0.0, 40.0, 0.0],
            target: [-3000.0, 180.0, 900.0],
            ev: 10.5,
            albedo: [0.42, 0.22, 0.11],
            clouds: 0.0,
            body: None,
        },
        Shot {
            name: "europa_jupiter",
            planet: airless,
            time: TimeOfDay {
                hour: 9.0,
                day_of_year: 172,
                latitude_deg: 10.0,
            },
            eye: [0.0, 40.0, 0.0],
            target: [-1500.0, 600.0, -2000.0],
            ev: 12.0,
            albedo: [0.66, 0.66, 0.70],
            clouds: 0.0,
            body: Some(SkyBody {
                direction: [-0.55, 0.38, -0.74],
                ..SkyBody::jupiter_from_europa()
            }),
        },
    ]
}

fn hash(x: i32, z: i32, seed: u32) -> f32 {
    let mut h = (x as u32).wrapping_mul(0x8da6_b343) ^ (z as u32).wrapping_mul(0xd816_3841) ^ seed;
    h ^= h >> 13;
    h = h.wrapping_mul(0x5bd1_e995);
    h ^= h >> 15;
    (h >> 8) as f32 / 16_777_216.0
}

fn value_noise(x: f32, z: f32, seed: u32) -> f32 {
    let (x0, z0) = (x.floor(), z.floor());
    let (tx, tz) = (x - x0, z - z0);
    let (sx, sz) = (tx * tx * (3.0 - 2.0 * tx), tz * tz * (3.0 - 2.0 * tz));
    let (i, k) = (x0 as i32, z0 as i32);
    let a = hash(i, k, seed);
    let b = hash(i + 1, k, seed);
    let c = hash(i, k + 1, seed);
    let d = hash(i + 1, k + 1, seed);
    let top = a + (b - a) * sx;
    let bottom = c + (d - c) * sx;
    top + (bottom - top) * sz
}

/// Ridged hills: tall far away, flattened near the camera.
fn height(x: f32, z: f32) -> f32 {
    let mut sum = 0.0;
    let mut amp = 1.0;
    let mut freq = 1.0 / 2600.0;
    for octave in 0..6 {
        let n = value_noise(x * freq, z * freq, 17 + octave);
        let ridge = 1.0 - (2.0 * n - 1.0).abs();
        sum += amp * ridge * ridge;
        amp *= 0.48;
        freq *= 2.1;
    }
    let dist = (x * x + z * z).sqrt();
    let rise = ((dist - 400.0) / 2500.0).clamp(0.0, 1.0);
    sum * 520.0 * rise * rise
}

/// A 14 km square heightfield centred on the origin, denser near the middle.
fn terrain_mesh() -> LabMesh {
    let n = 241usize;
    let half = 7000.0_f32;
    let coord = |i: usize| {
        let t = i as f32 / (n - 1) as f32 * 2.0 - 1.0;
        // Quadratic spacing: ~6 m cells at the centre, ~110 m at the edge.
        t * t.abs().max(0.05) * half
    };
    let mut vertices = Vec::with_capacity(n * n);
    for j in 0..n {
        for i in 0..n {
            let (x, z) = (coord(i), coord(j));
            let e = 2.0;
            let dx = height(x + e, z) - height(x - e, z);
            let dz = height(x, z + e) - height(x, z - e);
            let normal = [-dx, 2.0 * e, -dz];
            let len = (normal[0] * normal[0] + normal[1] * normal[1] + normal[2] * normal[2]).sqrt();
            vertices.push(LabVertex::from_pos_normal_uv(
                [x, height(x, z), z],
                [normal[0] / len, normal[1] / len, normal[2] / len],
                [x / 50.0, z / 50.0],
            ));
        }
    }
    let mut indices = Vec::with_capacity((n - 1) * (n - 1) * 6);
    for j in 0..n - 1 {
        for i in 0..n - 1 {
            let a = (j * n + i) as u32;
            let b = a + 1;
            let c = a + n as u32;
            let d = c + 1;
            indices.extend_from_slice(&[a, c, b, b, c, d]);
        }
    }
    LabMesh {
        vertices,
        indices,
        ..LabMesh::unskinned()
    }
}

/// Renders every shot at `width`×`height`, returning `(name, rgba8, gpu report)`.
pub fn render_shots(device: &RenderDevice, width: u32, height: u32) -> Vec<(&'static str, Vec<u8>, String)> {
    let mesh = terrain_mesh();
    let mut out = Vec::new();
    for shot in shots() {
        let mut quality =
            FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
        quality.gpu_driven = false;
        let mut material = LabMaterial::EMPTY;
        material.base_color = [shot.albedo[0], shot.albedo[1], shot.albedo[2], 1.0];
        material.roughness = 0.92;
        material.metallic = 0.0;
        let scene = LabScene {
            width,
            height,
            eye: shot.eye,
            target: shot.target,
            fov_y: 48.0_f32.to_radians(),
            near: 0.5,
            far: 20_000.0,
            depth_prepass: true,
            directional_shadows: true,
            ibl: true,
            exposure_ev: shot.ev,
            ambient_lux: 0.0,
            double_sided: false,
            post: Some(PostSettings {
                enabled: true,
                bloom: true,
                auto_exposure: false,
                exposure_ev: shot.ev,
                fxaa: true,
                taa: false,
                film_grain: 0.0,
                vignette: 0.0,
                ..PostSettings::default()
            }),
            directional: regia_render::DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 100_000.0),
            local_lights: Vec::new(),
            materials: vec![material],
            meshes: vec![LabMesh {
                material: 0,
                ..mesh.clone()
            }],
        };
        let mut renderer = FrameRenderer::new();
        renderer.set_gpu_timing(device, true);
        renderer.set_outdoor(OutdoorParams {
            time: Some(shot.time),
            clouds: CloudSettings::coverage_layer(shot.clouds),
            planet: shot.planet,
            sky_body: shot.body,
            ..OutdoorParams::default()
        });
        let frame = scene.as_frame_scene_with_quality(quality);
        let mut pixels = Vec::new();
        // A few frames so the delayed GPU timestamps (ADR-027) land and TAA settles.
        for _ in 0..4 {
            match renderer.render_offscreen(device, &frame) {
                Ok(image) => pixels = image.pixels,
                Err(err) => {
                    eprintln!("{}: render failed: {err}", shot.name);
                    break;
                }
            }
        }
        let report = renderer.gpu_timings().report();
        out.push((shot.name, pixels, report));
    }
    out
}
