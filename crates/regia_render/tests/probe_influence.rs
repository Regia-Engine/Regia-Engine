// SPDX-License-Identifier: MPL-2.0
//! In stochastic SSR (Desktop High and Ultra) a reflection probe reflects only around its own box.
//!
//! The renderer picks up to four probes for the camera and blends them with weights that sum
//! to one; before v0.1.5 the probe fallback then applied that blend at every pixel, so a probe
//! captured somewhere else (DO27's tunnel or cockpit probe, a site across the map) painted its
//! capture onto every glossy surface in view. Past a probe's box (and a margin) the pixel keeps
//! its own environment. The older linear-march pass (lab, editor and MSAA frames) still blends
//! the camera's probes everywhere: its lab goldens are blessed with that.

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, GiMode, LabMaterial, LabMesh, LabScene, LabVertex,
    PostSettings, ReflectionProbe, RenderDevice, RenderProfile, RenderProfilePreset, SsaoMode,
};

fn device() -> Option<RenderDevice> {
    match RenderDevice::headless() {
        Ok(device) => Some(device),
        Err(err) => {
            eprintln!("skip probe influence: {err}");
            None
        }
    }
}

const W: u32 = 96;
const H: u32 = 64;

fn quad(corners: [[f32; 3]; 4], n: [f32; 3]) -> LabMesh {
    LabMesh {
        vertices: corners
            .iter()
            .enumerate()
            .map(|(i, &p)| LabVertex::from_pos_normal_uv(p, n, [(i & 1) as f32, (i >> 1) as f32]))
            .collect(),
        indices: vec![0, 1, 2, 2, 1, 3, 0, 2, 1, 2, 3, 1],
        material: 0,
        ..LabMesh::unskinned()
    }
}

/// A glossy floor and a glossy wall facing the camera, which looks down at both: the wall
/// reflects downward, into the stochastic SSR's below-horizon probe fallback.
fn scene() -> LabScene {
    let mut glossy = LabMaterial::EMPTY;
    glossy.base_color = [0.9, 0.9, 0.9, 1.0];
    glossy.metallic = 1.0;
    glossy.roughness = 0.12;
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 3.0, 6.0],
        target: [0.0, 1.0, -2.0],
        fov_y: 1.0,
        near: 0.1,
        far: 200.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: true,
        exposure_ev: 11.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(PostSettings {
            enabled: true,
            bloom: false,
            auto_exposure: false,
            exposure_ev: 11.0,
            fxaa: false,
            taa: false,
            film_grain: 0.0,
            vignette: 0.0,
            ..PostSettings::default()
        }),
        directional: DirectionalLightUniform::new([0.3, -0.8, -0.4], [1.0; 3], 20_000.0),
        local_lights: Vec::new(),
        materials: vec![glossy],
        meshes: vec![
            quad(
                [
                    [-6.0, 0.0, -4.0],
                    [6.0, 0.0, -4.0],
                    [-6.0, 0.0, 4.0],
                    [6.0, 0.0, 4.0],
                ],
                [0.0, 1.0, 0.0],
            ),
            quad(
                [
                    [-6.0, 4.0, -4.0],
                    [6.0, 4.0, -4.0],
                    [-6.0, 0.0, -4.0],
                    [6.0, 0.0, -4.0],
                ],
                [0.0, 0.0, 1.0],
            ),
        ],
    }
}

/// A probe that captured a uniformly red world.
fn red_probe(center: [f32; 3], half_extent: [f32; 3]) -> ReflectionProbe {
    let (width, height) = (64u32, 32u32);
    let equirect = [255u8, 0, 0, 255].repeat((width * height) as usize);
    ReflectionProbe {
        id: 9,
        center,
        half_extent,
        equirect,
        width,
        height,
    }
}

/// Mean red minus mean green over the frame (the probe's red against a neutral scene).
fn redness(pixels: &[u8]) -> f32 {
    let mut acc = 0.0f32;
    for px in pixels.as_chunks::<4>().0 {
        acc += f32::from(px[0]) - f32::from(px[1]);
    }
    acc / (pixels.len() / 4).max(1) as f32
}

fn render(device: &RenderDevice, quality: FrameQuality, probes: Vec<ReflectionProbe>) -> Vec<u8> {
    let scene = scene();
    let mut renderer = FrameRenderer::new();
    renderer.set_reflection_probes(probes);
    let frame = scene.as_frame_scene_with_quality(quality);
    let mut last = Vec::new();
    for _ in 0..3 {
        last = renderer.render_offscreen(device, &frame).expect("frame").pixels;
    }
    last
}

fn check(name: &str, quality: FrameQuality) {
    let Some(device) = device() else { return };
    let none = render(&device, quality, Vec::new());
    let covering = render(
        &device,
        quality,
        vec![red_probe([0.0, 2.0, 0.0], [6.0, 4.0, 6.0])],
    );
    let far = render(
        &device,
        quality,
        vec![red_probe([400.0, 2.0, 400.0], [6.0, 4.0, 6.0])],
    );
    let (r_none, r_cover, r_far) = (redness(&none), redness(&covering), redness(&far));
    eprintln!(
        "{name}: redness without probes {r_none:.2}, covering probe {r_cover:.2}, far probe {r_far:.2}"
    );
    // The probe reflects where it was captured...
    assert!(
        r_cover > r_none + 3.0,
        "{name}: a covering probe left no reflection ({r_cover} vs {r_none})"
    );
    // ... and nowhere else.
    assert!(
        (r_far - r_none).abs() < 0.5,
        "{name}: a probe 560 m away reflects on the scene ({r_far} vs {r_none} without probes)"
    );
}

#[test]
fn a_far_probe_does_not_reflect_in_stochastic_ssr() {
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.ssao = SsaoMode::Off;
    quality.screen_space_gi = false;
    quality.gi = GiMode::Ambient;
    quality.render_scale_permille = 1000;
    quality.taau = false;
    assert!(quality.stochastic_ssr());
    check("sssr", quality);
}
