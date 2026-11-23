// SPDX-License-Identifier: MPL-2.0
//! ADR-5622: the lite forward shader (`forward_mobile.wgsl`) under `BrdfEnergy::Compensated`
//! (ADR-2632) gives every diffuse term one energy partition, not two.
//!
//! The compensated lite form weights the diffuse colour once per fragment by `1 − E_s(μo)`, the
//! share the specular layer leaves (`F0·scale + bias` from the analytic split-sum fit). ADR-4930's
//! direct BRDF also weights its diffuse by `1 − F(l·h)` (or `1 − E_s` with `multi_scatter`), and a
//! lightmap receiver with the baked specular on weights its diffuse by `1 − E_s` from the DFG table.
//! Applied on top of the per-fragment weight, both darkened the Mobile diffuse twice.
//!
//! Two checks, each on both lite tiers:
//!
//! - **Direct sun.** A white, fully rough dielectric sphere is lit head-on by the sun, no image
//!   light, no ambient. At the sphere's centre `n = v = l`, so the BRDF is known in closed form:
//!   legacy `(1 − F0)/π + F0·D·V`, compensated `(1 − E_s)/π + F0·D·V`. The compensated frame's
//!   centre radiance over the legacy frame's must match that ratio (1.025), not the
//!   double-partition one (0.984). With `multi_scatter` the compensated diffuse is the same
//!   `1 − E_s` (only the small specular lobe changes), not `(1 − E_s)²`.
//! - **Lightmap white furnace.** `lightmap_forward.rs`'s
//!   `receiver_energy_and_partial_capture_shares_are_conserved` under the compensated model: a white
//!   receiver under uniform baked light returns the same radiance with the baked specular on as off.
//!
//! The process-wide model is flipped here, so this file holds one test.

use std::sync::Arc;

use regia_asset::lightmap::{
    Coverage, Lightmap, LightmapBakeInfo, LightmapContents, LightmapEntry, LightmapLayout, MeshUnwrap,
};
use regia_render::forward_lite::ForwardLite;
use regia_render::lightmap::{LightmapSettings, receiver_mesh};
use regia_render::{
    BrdfEnergy, DirectionalLightUniform, FrameMesh, FrameQuality, FrameRenderer, GiMode, HdrTarget,
    LabMaterial, LabMesh, LabScene, LabVertex, PostSettings, RenderDevice, RenderProfile,
    RenderProfilePreset, translate_mesh, uv_sphere,
};

const TIERS: [(ForwardLite, RenderProfilePreset); 2] = [
    (ForwardLite::High, RenderProfilePreset::MobileHigh),
    (ForwardLite::Low, RenderProfilePreset::MobileLow),
];

/// Sun-test frame size; the sphere's centre is at the middle four pixels.
const SIZE: u32 = 64;
const EV: f32 = 12.0;
/// The sun's illuminance, lux.
const SUN_LUX: f32 = 3_000.0;
/// The material's normal-incidence reflectance (the shader's dielectric default).
const F0: f32 = 0.04;
/// Allowed error on the compensated / legacy ratio: RGBA16F storage and the centre pixels'
/// n·v (about 0.9996 at the pixel centres) against the closed form. The double partition is 4 %
/// away from the expectation.
const RATIO_TOLERANCE: f32 = 0.006;
const PI: f32 = std::f32::consts::PI;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// `forward_lite_low_luma.rs`'s quality: `preset` on the CPU colour loop, with an RGBA16F target
/// so the HDR read keeps three significant digits.
fn quality(preset: RenderProfilePreset) -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(preset));
    q.gpu_driven = false;
    q.visibility_buffer = false;
    q.taau = false;
    q.msaa_samples = 1;
    q.render_scale_permille = 1000;
    q.horizontal_plus = false;
    q.volumetric_fog = false;
    q.volumetric_clouds = false;
    q.ssr = false;
    q.reflection_probes = false;
    q.gi = GiMode::Ambient;
    q.screen_space_gi = false;
    q.gi_probes = false;
    q.baked_gi = false;
    q.hdr_target = HdrTarget::Rgba16f;
    q
}

fn white(roughness: f32, multi_scatter: bool) -> LabMaterial {
    let mut m = LabMaterial {
        base_color: [1.0; 4],
        metallic: 0.0,
        roughness,
        ..LabMaterial::default()
    };
    m.hard_surface.multi_scatter = if multi_scatter { 1.0 } else { 0.0 };
    m
}

/// A white sphere at the origin seen from +Z, lit by the sun from the camera (the light travels
/// toward −Z), with no image light and no ambient.
fn sun_scene(material: LabMaterial) -> LabScene {
    let (v, i) = uv_sphere(1.0, 192, 96);
    LabScene {
        width: SIZE,
        height: SIZE,
        eye: [0.0, 0.0, 6.0],
        target: [0.0; 3],
        fov_y: 0.42,
        near: 0.1,
        far: 100.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: EV,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(PostSettings {
            auto_exposure: false,
            exposure_ev: EV,
            bloom: false,
            ..PostSettings::default()
        }),
        directional: DirectionalLightUniform::new([0.0, 0.0, -1.0], [1.0; 3], SUN_LUX),
        local_lights: Vec::new(),
        materials: vec![material],
        meshes: vec![LabMesh {
            vertices: translate_mesh(&v, [0.0; 3]),
            indices: i,
            material: 0,
            ..LabMesh::unskinned()
        }],
    }
}

/// The mean red radiance (cd/m²) of the frame's middle four pixels, read before the tonemap.
fn centre_radiance(
    device: &RenderDevice,
    tier: ForwardLite,
    preset: RenderProfilePreset,
    scene: &LabScene,
) -> f32 {
    let mut renderer = FrameRenderer::new();
    renderer.set_forward_lite(Some(tier));
    let frame = scene.as_frame_scene_with_quality(quality(preset));
    renderer.render_offscreen(device, &frame).expect("frame");
    renderer.request_hdr_capture();
    renderer.render_offscreen(device, &frame).expect("frame");
    let hdr = renderer
        .take_hdr_capture(device)
        .expect("hdr read")
        .expect("a post stack frame");
    let mid = SIZE / 2;
    let mut sum = 0.0;
    for y in [mid - 1, mid] {
        for x in [mid - 1, mid] {
            sum += hdr.rgb[(y * hdr.width + x) as usize][0];
        }
    }
    sum / 4.0
}

/// The split-sum scale and bias of `forward_mobile.wgsl`'s `env_brdf_approx` (Lazarov 2013,
/// "Getting More Physical in Call of Duty: Black Ops II", with Karis 2014's fit, "Physically Based
/// Shading on Mobile").
fn env_brdf_approx(perceptual: f32, no_v: f32) -> [f32; 2] {
    let r = [
        1.0 - perceptual,
        perceptual * -0.0275 + 0.0425,
        perceptual * -0.572 + 1.04,
        perceptual * 0.022 - 0.04,
    ];
    let a004 = (r[0] * r[0]).min((-9.28 * no_v).exp2()) * r[0] + r[1];
    [-1.04 * a004 + r[2], 1.04 * a004 + r[3]]
}

/// The GGX lobe at `n = v = l = h` with roughness α = 1: `F0 · D · V`, `D = 1/π`, `V = 1/4`
/// (height-correlated Smith, Heitz 2014).
fn head_on_specular() -> f32 {
    F0 * (1.0 / PI) * 0.25
}

fn check_sun(device: &RenderDevice, problems: &mut Vec<String>) {
    let es = {
        let ab = env_brdf_approx(1.0, 1.0);
        (F0 * ab[0] + ab[1]).clamp(0.0, 1.0)
    };
    let spec = head_on_specular();
    let legacy_brdf = (1.0 - F0) / PI + spec;
    let single = ((1.0 - es) / PI + spec) / legacy_brdf;
    let double = ((1.0 - es) * (1.0 - F0) / PI + spec) / legacy_brdf;
    for (tier, preset) in TIERS {
        regia_render::set_process_brdf_energy(BrdfEnergy::Legacy);
        let legacy = centre_radiance(device, tier, preset, &sun_scene(white(1.0, false)));
        regia_render::set_process_brdf_energy(BrdfEnergy::Compensated);
        let compensated = centre_radiance(device, tier, preset, &sun_scene(white(1.0, false)));
        let compensated_ms = centre_radiance(device, tier, preset, &sun_scene(white(1.0, true)));
        let ratio = compensated / legacy.max(1e-6);
        let ms_ratio = compensated_ms / compensated.max(1e-6);
        println!(
            "{tier:?} sun: legacy {legacy:.3} cd/m² (closed form {:.3}), compensated {compensated:.3}, \
             ratio {ratio:.4} (single partition {single:.4}, double {double:.4}); \
             compensated multi-scatter / single-scatter {ms_ratio:.4}",
            SUN_LUX * legacy_brdf
        );
        let expected_legacy = SUN_LUX * legacy_brdf;
        if (legacy / expected_legacy - 1.0).abs() > 0.03 {
            problems.push(format!(
                "{tier:?}: the legacy centre radiance {legacy:.3} is not the closed form {expected_legacy:.3}"
            ));
        }
        if (ratio - single).abs() > RATIO_TOLERANCE {
            problems.push(format!(
                "{tier:?}: compensated / legacy diffuse {ratio:.4}, the single partition gives {single:.4} \
                 (the double partition {double:.4})"
            ));
        }
        // The compensated diffuse is `1 − E_s` with or without multi-scatter; only the specular
        // lobe (about 1 % of the radiance here) takes the multi-scatter compensation.
        if (ms_ratio - 1.0).abs() > 0.01 {
            problems.push(format!(
                "{tier:?}: the compensated multi-scatter frame is {ms_ratio:.4} × the single-scatter one; \
                 its diffuse must keep the same single partition"
            ));
        }
    }
}

// The lightmap furnace below is `lightmap_forward.rs`'s scene.

const W: u32 = 64;
const H: u32 = 64;
const RECEIVER: u64 = 0x0B1E_C7A1;
const LOW: f32 = 2_000.0;
const RECEIVER_LEFT: (u32, u32) = (17, 32);
const RECEIVER_RIGHT: (u32, u32) = (24, 32);

fn quad(x: f32) -> (Vec<LabVertex>, Vec<u32>) {
    let n = [0.0, 0.0, 1.0];
    (
        vec![
            LabVertex::from_pos_normal_uv([x - 0.3, -0.3, 0.0], n, [0.0, 1.0]),
            LabVertex::from_pos_normal_uv([x + 0.3, -0.3, 0.0], n, [1.0, 1.0]),
            LabVertex::from_pos_normal_uv([x + 0.3, 0.3, 0.0], n, [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([x - 0.3, 0.3, 0.0], n, [0.0, 0.0]),
        ],
        vec![0, 1, 2, 0, 2, 3],
    )
}

fn lab() -> LabScene {
    let (vertices, indices) = quad(-0.5);
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 0.0, 3.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 10.0,
        depth_prepass: false,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 5_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 0.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial::default()],
        meshes: vec![LabMesh {
            vertices,
            indices,
            ..LabMesh::unskinned()
        }],
    }
}

/// A uniform `irradiance` over the receiver entry (the atlas's right half).
fn lightmap(irradiance: f32) -> Lightmap {
    let irr: Vec<[f32; 3]> = (0..2)
        .flat_map(|_| 0..8u32)
        .map(|x| if x < 4 { [0.0; 3] } else { [irradiance; 3] })
        .collect();
    Lightmap::from_irradiance(
        LightmapLayout {
            width: 8,
            height: 2,
            density: 1.0,
            padding: 1,
            entries: vec![LightmapEntry {
                name: "receiver".into(),
                source_vertices: 4,
                rect: [4, 0, 4, 2],
                scale_offset: [0.5, 1.0, 0.5, 0.0],
                vertex_count: 0,
                index_count: 0,
                first_vertex: 0,
                first_index: 0,
                charts: 1,
                area_m2: 0.36,
                texels_per_metre: 1.0,
                key: None,
                seams: None,
                page: 0,
            }],
            unwraps: vec![MeshUnwrap {
                xref: vec![0, 1, 2, 3],
                uv: vec![[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]],
                indices: vec![0, 1, 2, 0, 2, 3],
            }],
        },
        &irr,
        vec![Coverage::BAKED; 16],
        LightmapBakeInfo {
            contents: LightmapContents::SKY,
            ..LightmapBakeInfo::default()
        },
    )
    .expect("valid lightmap")
}

fn px(img: &[u8], x: u32, y: u32) -> u32 {
    let i = ((y * W + x) * 4) as usize;
    u32::from(img[i]) + u32::from(img[i + 1]) + u32::from(img[i + 2])
}

fn lightmap_frame(device: &RenderDevice, specular: bool, lite: ForwardLite, roughness: f32) -> Vec<u8> {
    let mut lab = lab();
    lab.materials[0].base_color = [1.0; 4];
    lab.materials[0].metallic = 0.0;
    lab.materials[0].roughness = roughness;
    let map = Arc::new(lightmap(LOW));
    let (vertices, indices) = quad(-0.5);
    let built = receiver_mesh(&map, 0, &vertices, &indices).expect("receiver");
    let mut scene = lab.as_frame_scene();
    scene.meshes[0] = FrameMesh {
        object_key: RECEIVER,
        vertices: &built.vertices,
        indices: &built.indices,
        uv1: &built.uv2,
        ..scene.meshes[0]
    };
    let mut renderer = FrameRenderer::new();
    renderer.set_forward_lite(Some(lite));
    renderer.set_lightmap(
        Some(map),
        LightmapSettings {
            enabled: true,
            specular,
            ..LightmapSettings::default()
        },
    );
    renderer.set_lightmap_receivers(&[(RECEIVER, 0)]);
    renderer.render_offscreen(device, &scene).expect("frame").pixels
}

fn check_lightmap_furnace(device: &RenderDevice, problems: &mut Vec<String>) {
    regia_render::set_process_brdf_energy(BrdfEnergy::Compensated);
    for (lite, _) in TIERS {
        for roughness in [0.1, 0.5, 1.0] {
            let off = lightmap_frame(device, false, lite, roughness);
            let on = lightmap_frame(device, true, lite, roughness);
            let reference = px(&off, RECEIVER_LEFT.0, RECEIVER_LEFT.1);
            assert!(
                (30..700).contains(&reference),
                "white-furnace reference must be visible and unsaturated: {reference}"
            );
            for (x, y) in [RECEIVER_LEFT, RECEIVER_RIGHT] {
                let (a, b) = (px(&on, x, y), px(&off, x, y));
                println!(
                    "{lite:?} lightmap furnace roughness {roughness} at ({x}, {y}): specular on {a}, off {b}"
                );
                if a.abs_diff(b) > 3 {
                    problems.push(format!(
                        "{lite:?}: compensated white furnace, roughness {roughness}, ({x}, {y}): \
                         specular on {a} vs off {b}"
                    ));
                }
            }
        }
    }
}

#[test]
fn the_compensated_lite_shader_partitions_each_diffuse_term_once() {
    let device = device();
    let mut problems = Vec::new();
    check_sun(&device, &mut problems);
    check_lightmap_furnace(&device, &mut problems);
    regia_render::set_process_brdf_energy(BrdfEnergy::Legacy);
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}
