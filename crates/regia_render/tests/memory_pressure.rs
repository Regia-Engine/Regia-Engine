// SPDX-License-Identifier: MPL-2.0
//! Memory pressure that actually fires on desktop (ADR-171): a wgpu out-of-memory error reaches
//! `gpu_health`, which signals memory pressure, and the next frame of a renderer whose stochastic
//! SSR pass is dormant frees its targets (ADR-168: `FrameRenderer::trim_memory` was dead code on
//! Linux/Windows, reachable only from the mobile low-memory event). The pressure and the error
//! count show in `render.problems`.
//!
//! Its own test binary: the pressure signal and the error counter are process-wide, and would
//! make the renderers of other tests trim (and report problems) mid-test.

use regia_render::gpu_health::{self, PROBLEM_GPU_ERRORS, PROBLEM_VRAM_PRESSURE, PressureSource};
use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, PostSettings,
    RenderDevice, RenderProfile, RenderProfilePreset,
};

const W: u32 = 192;
const H: u32 = 108;
const EV: f32 = 13.0;

/// A rough floor in view and a mirror ball behind the camera: the pass cannot be ruled out on
/// the CPU, its readbacks see nothing glossy, so it goes dormant (as in `tests/sssr.rs`).
/// `glossy` makes the floor a mirror, which keeps it awake.
fn scene(glossy: bool) -> LabScene {
    let (fv, fi) = regia_render::box_mesh([6.0, 0.05, 6.0]);
    let (sv, si) = regia_render::uv_sphere(0.5, 32, 16);
    let (bv, bi) = regia_render::uv_sphere(0.3, 16, 8);
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 0.9, 2.5],
        target: [0.0, 0.0, -1.0],
        fov_y: 0.9,
        near: 0.05,
        far: 100.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: EV,
        ambient_lux: 400.0,
        double_sided: false,
        post: Some(PostSettings {
            exposure_ev: EV,
            ..PostSettings::golden_deterministic()
        }),
        directional: DirectionalLightUniform::new([0.1, -0.4, -0.9], [1.0, 0.97, 0.92], 30_000.0),
        local_lights: Vec::new(),
        materials: vec![
            LabMaterial {
                base_color: [0.9, 0.9, 0.9, 1.0],
                metallic: if glossy { 1.0 } else { 0.0 },
                roughness: if glossy { 0.0 } else { 0.9 },
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.9, 0.35, 0.08, 1.0],
                roughness: 0.9,
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.9, 0.9, 0.9, 1.0],
                metallic: 1.0,
                roughness: 0.05,
                ..LabMaterial::default()
            },
        ],
        meshes: vec![
            LabMesh {
                vertices: regia_render::translate_mesh(&fv, [0.0, -0.05, 0.0]),
                indices: fi,
                material: 0,
                ..LabMesh::unskinned()
            },
            LabMesh {
                vertices: regia_render::translate_mesh(&sv, [0.0, 0.7, -1.5]),
                indices: si,
                material: 1,
                ..LabMesh::unskinned()
            },
            LabMesh {
                vertices: regia_render::translate_mesh(&bv, [0.0, 1.0, 12.0]),
                indices: bi,
                material: 2,
                ..LabMesh::unskinned()
            },
        ],
    }
}

fn desktop_high() -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    q.volumetric_clouds = false;
    q.volumetric_fog = false;
    q
}

#[test]
fn an_out_of_memory_error_frees_the_dormant_reflection_targets() {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    };
    let (hidden_scene, glossy_scene) = (scene(false), scene(true));
    let hidden = hidden_scene.as_frame_scene_with_quality(desktop_high());
    let glossy = glossy_scene.as_frame_scene_with_quality(desktop_high());
    let mut renderer = FrameRenderer::new();
    for _ in 0..3 {
        renderer.render_offscreen(&device, &glossy).expect("frame");
    }
    assert!(
        renderer.reflection_target_bytes() > 0,
        "a glossy frame allocates the targets"
    );
    let slept = (1..=90).find(|_| {
        renderer.render_offscreen(&device, &hidden).expect("frame");
        renderer.sssr_dormant()
    });
    assert!(slept.is_some(), "the pass went dormant");
    let dormant_bytes = renderer.reflection_target_bytes();
    assert!(dormant_bytes > 0, "dormant keeps its targets (ADR-168)");
    assert!(
        !renderer
            .frame_problems()
            .iter()
            .any(|p| p.code == PROBLEM_VRAM_PRESSURE || p.code == PROBLEM_GPU_ERRORS),
        "no health problem before any error"
    );

    // What the uncaptured-error handler does with wgpu's `Error::OutOfMemory`.
    let oom = wgpu::Error::OutOfMemory {
        source: Box::new(std::io::Error::other("test: allocation failed")),
    };
    gpu_health::note_uncaptured_error(&oom);
    assert_eq!(gpu_health::gpu_error_count(), 1);
    assert_eq!(gpu_health::gpu_oom_count(), 1);
    assert_eq!(
        gpu_health::memory_pressure_count(),
        1,
        "the OOM signalled pressure"
    );
    // The OOM source holds back a second signal for 5 s; the probe's source is separate.
    assert!(!gpu_health::signal_pressure(PressureSource::OutOfMemory));

    renderer.render_offscreen(&device, &hidden).expect("frame");
    assert_eq!(
        renderer.reflection_target_bytes(),
        0,
        "the signalled pressure freed the dormant targets ({dormant_bytes} bytes)"
    );
    let codes: Vec<&str> = renderer.frame_problems().iter().map(|p| p.code).collect();
    assert!(
        codes.contains(&PROBLEM_VRAM_PRESSURE),
        "render.problems: {codes:?}"
    );
    assert!(codes.contains(&PROBLEM_GPU_ERRORS), "render.problems: {codes:?}");

    // A probe signal trims but never lowers the texture budget (review fix: a healthy card
    // sits near its VRAM for minutes); OOMs do. v0.1.16 (ADR-223) changed the expected counts
    // by one: the first OOM above now steps the budget on its own (on the DO27 tester's
    // Radeon 610M the first OOM was also the fatal one, so a second never came), and a second
    // one within a minute still steps again.
    assert_eq!(
        gpu_health::texture_budget_steps(),
        1,
        "the first OOM steps the budget"
    );
    assert!(gpu_health::signal_pressure(PressureSource::VramHigh));
    assert_eq!(gpu_health::memory_pressure_count(), 2);
    assert_eq!(
        gpu_health::texture_budget_steps(),
        1,
        "the probe never steps the budget"
    );
    std::thread::sleep(std::time::Duration::from_millis(5100));
    gpu_health::note_uncaptured_error(&wgpu::Error::OutOfMemory {
        source: Box::new(std::io::Error::other("test: allocation failed again")),
    });
    assert_eq!(gpu_health::memory_pressure_count(), 3);
    assert_eq!(
        gpu_health::texture_budget_steps(),
        2,
        "a second OOM within 60 s steps the budget again"
    );
    assert!(
        gpu_health::texture_mips_held(),
        "an OOM 5 s after a trim holds the streamed textures a mip lower"
    );

    // Woken again by a glossy view, the pass allocates its targets anew and draws.
    for _ in 0..4 {
        renderer.render_offscreen(&device, &glossy).expect("frame");
    }
    assert!(!renderer.sssr_dormant());
    assert!(
        renderer.reflection_target_bytes() > 0,
        "the targets come back when needed"
    );
}
