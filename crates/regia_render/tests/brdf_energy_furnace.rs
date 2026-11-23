// SPDX-License-Identifier: MPL-2.0
//! ADR-2632: the forward pass's white furnace. A white sphere sits alone inside a uniform studio
//! environment (zenith, horizon and ground all at one radiance, under the studio authority, so
//! the environment is the frame's sky and its image light). Every sphere pixel's linear radiance,
//! read before the tonemap (`FrameRenderer::request_hdr_capture`), is divided by the sky's.
//! A surface that conserves energy shows at most 1.
//!
//! The legacy forward module (the default) shows more than 1: its image-light diffuse takes the
//! full base colour beside the specular. The compensated module (`BRDF_ENERGY_COMPENSATED`,
//! selected process-wide before the second renderer composes its pipelines) stays at or under
//! 1 + the stated tolerance at every pixel, rim included.
//!
//! The process-wide model is flipped here, so this file holds one test: no other test in this
//! binary composes a forward module while it is set.

use regia_render::{
    BrdfEnergy, DirectionalLightUniform, EnvironmentAuthority, FrameQuality, FrameRenderer, FrameScene,
    HdrCapture, LabMaterial, LabMesh, LabScene, PostSettings, RenderDevice, RenderProfile,
    RenderProfilePreset, StudioGradient, translate_mesh, uv_sphere,
};

const SIZE: u32 = 96;
const EV: f32 = 12.0;
/// The uniform environment's radiance, cd/m².
const SKY: f32 = 1000.0;
/// The image light's own error budget: the prefiltered and irradiance maps' storage and
/// filtering, and the 8-bit DFG table the split sum reads.
const FURNACE_TOLERANCE: f32 = 0.02;

fn quality() -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    q.taau = false;
    q.ssr = false;
    q.screen_space_gi = false;
    q.gi_probes = false;
    q.render_scale_permille = 1000;
    q.horizontal_plus = false;
    q
}

fn scene(material: LabMaterial) -> LabScene {
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
        ibl: true,
        exposure_ev: EV,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(PostSettings {
            auto_exposure: false,
            exposure_ev: EV,
            bloom: false,
            ..PostSettings::default()
        }),
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 0.0),
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

fn white(metallic: f32, roughness: f32, multi_scatter: bool) -> LabMaterial {
    let mut m = LabMaterial {
        base_color: [1.0; 4],
        metallic,
        roughness,
        ..LabMaterial::default()
    };
    m.hard_surface.multi_scatter = if multi_scatter { 1.0 } else { 0.0 };
    m
}

fn hdr_frame(device: &RenderDevice, renderer: &mut FrameRenderer, frame: &FrameScene<'_>) -> HdrCapture {
    renderer.render_offscreen(device, frame).expect("frame");
    renderer.request_hdr_capture();
    renderer.render_offscreen(device, frame).expect("frame");
    renderer
        .take_hdr_capture(device)
        .expect("hdr read")
        .expect("a post stack frame")
}

/// The ratio of every sphere pixel's red channel to the sky's (read at the frame's corner),
/// and the sky value. The sphere pixels are those within 0.92 of its projected radius, so no
/// silhouette pixel half covers the background.
fn ratios(hdr: &HdrCapture) -> (Vec<(f32, f32)>, f32) {
    let sky = hdr.rgb[0][0];
    let half = SIZE as f32 / 2.0;
    // Projected radius in pixels: asin(1 / 6) seen through tan(fov / 2).
    let radius_px = (1.0f32 / 6.0).asin().tan() / (0.21f32).tan() * half;
    let mut out = Vec::new();
    for y in 0..hdr.height {
        for x in 0..hdr.width {
            let dx = x as f32 + 0.5 - half;
            let dy = y as f32 + 0.5 - half;
            let r = (dx * dx + dy * dy).sqrt() / radius_px;
            if r < 0.92 {
                let v = hdr.rgb[(y * hdr.width + x) as usize][0];
                out.push((r, v / sky));
            }
        }
    }
    (out, sky)
}

#[test]
fn the_compensated_forward_module_keeps_a_white_furnace_at_or_under_one() {
    let device = match RenderDevice::headless() {
        Ok(d) => d,
        Err(err) => {
            eprintln!("skip brdf_energy_furnace: {err}");
            return;
        }
    };
    let materials = [
        ("dielectric ms r0.3", white(0.0, 0.3, true)),
        ("dielectric ms r0.7", white(0.0, 0.7, true)),
        ("dielectric ss r0.5", white(0.0, 0.5, false)),
        ("metal ms r0.6", white(1.0, 0.6, true)),
    ];
    let gradient = StudioGradient::new([SKY; 3], [SKY; 3], [SKY; 3]);
    let mut lines = Vec::new();
    let mut results = Vec::new();
    for energy in BrdfEnergy::ALL {
        regia_render::set_process_brdf_energy(energy);
        assert_eq!(regia_render::process_brdf_energy(), energy);
        for (name, material) in &materials {
            // A new renderer per model: its pipelines compose with the model set above.
            let mut renderer = FrameRenderer::new();
            renderer.set_environment_authority(EnvironmentAuthority::Studio);
            assert_eq!(renderer.set_studio_gradient(&device, Some(&gradient)), Ok(true));
            let lab = scene(material.clone());
            let hdr = hdr_frame(
                &device,
                &mut renderer,
                &lab.as_frame_scene_with_quality(quality()),
            );
            let (r, sky) = ratios(&hdr);
            assert!(
                sky > 0.0 && !r.is_empty(),
                "{name}: sky {sky}, {} pixels",
                r.len()
            );
            let max = r.iter().map(|p| p.1).fold(0.0f32, f32::max);
            let centre: Vec<f32> = r.iter().filter(|p| p.0 < 0.3).map(|p| p.1).collect();
            let centre = centre.iter().sum::<f32>() / centre.len().max(1) as f32;
            let rim: Vec<f32> = r.iter().filter(|p| p.0 > 0.8).map(|p| p.1).collect();
            let rim = rim.iter().sum::<f32>() / rim.len().max(1) as f32;
            lines.push(format!(
                "{} {name}: sky {sky:.1} centre {centre:.4} rim {rim:.4} max {max:.4}",
                energy.name()
            ));
            results.push((energy, *name, centre, rim, max));
        }
    }
    regia_render::set_process_brdf_energy(BrdfEnergy::Legacy);
    eprintln!("forward furnace (sphere / sky):\n{}", lines.join("\n"));
    for (energy, name, centre, rim, max) in &results {
        if *energy == BrdfEnergy::Compensated {
            assert!(
                *max <= 1.0 + FURNACE_TOLERANCE,
                "{name}: compensated max {max} (centre {centre}, rim {rim})"
            );
        }
    }
    // The legacy module's white dielectric reflects more than the sky gives it.
    let legacy = results
        .iter()
        .find(|r| r.0 == BrdfEnergy::Legacy && r.1 == "dielectric ss r0.5")
        .map(|r| r.2)
        .unwrap_or(0.0);
    assert!(legacy > 1.0 + FURNACE_TOLERANCE, "legacy centre {legacy}");

    flat_ambient_keeps_the_legacy_diffuse(&device, &materials);
}

/// The sphere's pixels (within 0.92 of its projected radius) of one HDR frame, red channel.
fn sphere_pixels(hdr: &HdrCapture) -> Vec<(f32, f32)> {
    let half = SIZE as f32 / 2.0;
    let radius_px = (1.0f32 / 6.0).asin().tan() / (0.21f32).tan() * half;
    let mut out = Vec::new();
    for y in 0..hdr.height {
        for x in 0..hdr.width {
            let dx = x as f32 + 0.5 - half;
            let dy = y as f32 + 0.5 - half;
            let r = (dx * dx + dy * dy).sqrt() / radius_px;
            if r < 0.92 {
                out.push((r, hdr.rgb[(y * hdr.width + x) as usize][0]));
            }
        }
    }
    out
}

/// A flat ambient (no image light, no probes, no lights) adds no reflection beside the diffuse,
/// so the compensated module must not take the specular layer's share out of it: every sphere
/// pixel equals the legacy module's. Before the coverage weight (`compensated_env_diffuse_covered`)
/// the compensated rim of a white dielectric lost the whole split-sum Fresnel and went dark.
fn flat_ambient_keeps_the_legacy_diffuse(device: &RenderDevice, materials: &[(&str, LabMaterial)]) {
    let mut frames = Vec::new();
    for energy in BrdfEnergy::ALL {
        regia_render::set_process_brdf_energy(energy);
        for (name, material) in materials {
            let mut renderer = FrameRenderer::new();
            let mut lab = scene(material.clone());
            lab.ibl = false;
            lab.ambient_lux = 2000.0;
            let hdr = hdr_frame(device, &mut renderer, &lab.as_frame_scene_with_quality(quality()));
            frames.push((energy, *name, sphere_pixels(&hdr)));
        }
    }
    regia_render::set_process_brdf_energy(BrdfEnergy::Legacy);
    let mut lines = Vec::new();
    for (name, _) in materials {
        let find = |e: BrdfEnergy| {
            frames
                .iter()
                .find(|f| f.0 == e && f.1 == *name)
                .map(|f| f.2.clone())
                .unwrap_or_default()
        };
        let (legacy, compensated) = (find(BrdfEnergy::Legacy), find(BrdfEnergy::Compensated));
        assert!(
            !legacy.is_empty() && legacy.len() == compensated.len(),
            "{name}: pixel sets"
        );
        let lit = legacy.iter().filter(|p| p.1 > 0.0).count();
        assert!(
            lit * 2 > legacy.len(),
            "{name}: the flat ambient lights the sphere ({lit} lit)"
        );
        let mut worst = 0.0f32;
        for (l, c) in legacy.iter().zip(&compensated) {
            let rel = (c.1 - l.1).abs() / l.1.abs().max(1e-6);
            worst = worst.max(rel);
        }
        lines.push(format!("{name}: worst relative difference {worst:.5}"));
        assert!(
            worst <= 1e-3,
            "{name}: flat ambient, compensated differs from legacy by {worst}"
        );
    }
    eprintln!("flat ambient (compensated vs legacy):\n{}", lines.join("\n"));
}
