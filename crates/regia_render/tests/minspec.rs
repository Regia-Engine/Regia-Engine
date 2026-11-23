// SPDX-License-Identifier: MPL-2.0
//! Minimum-spec device profiles (`regia_render::minspec`): the engine's device requests,
//! clamped to what a Pascal desktop (GTX 10xx) or a mid-range mobile GPU exposes, still
//! create a device and render a correct, visibly lit frame — with the renderer's fallbacks
//! for what the tier lacks active and reported (`render.problems`,
//! [`FrameRenderer::frame_problems`]).
//!
//! Runs on lavapipe: `RenderDevice::headless_with_profile` clamps whatever adapter the
//! host offers down to the profile, so CI exercises the same code path a GTX 1080 Ti or a
//! Mali-G710 would take (gpuinfo report citations in `minspec.rs`). Device creation must
//! *fail* the test (a min-spec profile must never be un-runnable), and a frame is only
//! accepted when it is visibly non-trivial: the luminance has real variance and both lit
//! and shadowed regions, not merely a positive mean (a wrong-colour or blown-out frame
//! would otherwise pass).

use regia_render::{
    DirectionalLightUniform, ForwardResult, FrameRenderer, FrameScene, HostChecklist, LabMaterial, LabMesh,
    LabScene, LabVertex, MinspecProfile, RenderDevice, host_checklist::FrameProblem, uv_sphere,
};

const W: u32 = 96;
const H: u32 = 96;

/// A deterministic, visibly lit scene: a diffuse ground plane and a rough sphere lit by an
/// angled sun, so the frame has a lit side, a shadowed side and the sphere's shadow on the
/// ground (real variance, not a flat quad under a vertical light).
fn lit_scene() -> LabScene {
    let mut ground = LabMesh::unskinned();
    ground.material = 0;
    let s = 4.0f32;
    ground.vertices = vec![
        vert([-s, 0.0, -s], [0.0, 1.0, 0.0]),
        vert([s, 0.0, -s], [0.0, 1.0, 0.0]),
        vert([s, 0.0, s], [0.0, 1.0, 0.0]),
        vert([-s, 0.0, s], [0.0, 1.0, 0.0]),
    ];
    ground.indices = vec![0, 2, 1, 0, 3, 2];

    let (sphere_verts, sphere_indices) = uv_sphere(0.7, 24, 16);
    let mut sphere = LabMesh::unskinned();
    sphere.material = 1;
    sphere.vertices = sphere_verts;
    sphere.indices = sphere_indices;
    sphere.model = regia_core::glam::Mat4::from_translation([0.0, 0.7, 0.0].into()).to_cols_array_2d();

    LabScene {
        width: W,
        height: H,
        eye: [0.0, 1.2, 4.0],
        target: [0.0, 0.7, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 50.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 14.0,
        ambient_lux: 600.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([-0.45, -0.8, -0.4], [1.0, 1.0, 1.0], 100_000.0),
        local_lights: Vec::new(),
        materials: vec![
            LabMaterial {
                base_color: [0.62, 0.62, 0.62, 1.0],
                metallic: 0.0,
                roughness: 1.0,
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.9, 0.9, 0.9, 1.0],
                metallic: 0.0,
                roughness: 0.45,
                ..LabMaterial::default()
            },
        ],
        meshes: vec![ground, sphere],
    }
}

fn vert(pos: [f32; 3], normal: [f32; 3]) -> LabVertex {
    LabVertex {
        position: pos,
        normal,
        uv: [pos[0] * 0.25 + 0.5, pos[2] * 0.25 + 0.5],
        tangent: [1.0, 0.0, 0.0, 1.0],
        color: [1.0; 4],
        joints: [0; 4],
        weights: [0.0; 4],
    }
}

/// One frame of `scene` on `device`, plus the renderer (for `frame_problems`).
fn render(device: &RenderDevice, scene: &LabScene) -> (FrameRenderer, Vec<u8>) {
    let frame: FrameScene<'_> = scene.as_frame_scene();
    let mut renderer = FrameRenderer::new();
    let ForwardResult { pixels, .. } = renderer
        .render_offscreen(device, &frame)
        .unwrap_or_else(|err| panic!("frame on a min-spec profile: {err}"));
    (renderer, pixels)
}

/// Per-pixel luminance (0..1).
fn luma(pixels: &[u8]) -> Vec<f32> {
    pixels
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| (0.2126 * f32::from(p[0]) + 0.7152 * f32::from(p[1]) + 0.0722 * f32::from(p[2])) / 255.0)
        .collect()
}

fn variance(xs: &[f32]) -> f32 {
    let n = xs.len() as f32;
    let mean = xs.iter().sum::<f32>() / n;
    xs.iter().map(|x| (x - mean) * (x - mean)).sum::<f32>() / n
}

/// The frame is visibly non-trivial: real variance (not flat), a sane mid-range mean (not
/// black and not blown out), and a clear gap between its lit and shadowed regions.
fn assert_visibly_lit(pixels: &[u8]) {
    let lum = luma(pixels);
    let var = variance(&lum);
    assert!(
        var > 0.001,
        "the lit frame is flat (luminance variance {var:.5}); a missing or blown-out frame"
    );
    let mean = lum.iter().sum::<f32>() / lum.len() as f32;
    assert!(
        (0.02..=0.9).contains(&mean),
        "the lit frame's mean luminance {mean:.3} is black or blown out"
    );
    let mut sorted = lum.clone();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let lo = sorted[sorted.len() / 10];
    let hi = sorted[sorted.len() * 9 / 10];
    assert!(
        hi - lo > 0.04,
        "no lit-vs-shadowed contrast (p10 {lo:.3}, p90 {hi:.3}); a wrong or flat frame"
    );
}

fn codes(problems: &[FrameProblem]) -> Vec<&str> {
    problems.iter().map(|p| p.code).collect()
}

fn pascal_device() -> RenderDevice {
    RenderDevice::headless_with_profile(MinspecProfile::Pascal)
        .unwrap_or_else(|err| panic!("the Pascal profile must create a device: {err}"))
}

fn mobile_device() -> RenderDevice {
    RenderDevice::headless_with_profile(MinspecProfile::Mobile)
        .unwrap_or_else(|err| panic!("the Mobile profile must create a device: {err}"))
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn the_pascal_profile_clamps_the_device_to_pascals_limits() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = pascal_device();
    let limits = device.device.limits();
    // The Pascal floor on every clamped field, quoted as wgpu exposes it (measured on a real
    // GTX 1080 via wgpu 30 / Vulkan — the `minspec-hw` lane report, § 2), not raw Vulkan.
    assert!(
        limits.max_texture_dimension_2d <= 32_768,
        "{}",
        limits.max_texture_dimension_2d
    );
    assert!(limits.max_compute_workgroup_storage_size <= 48 * 1024);
    assert!(limits.max_compute_invocations_per_workgroup <= 1024);
    // wgpu caps `max_bind_groups` at 8 (the raw Vulkan `maxBoundDescriptorSets` is 32).
    assert!(limits.max_bind_groups <= 8);
    assert!(limits.max_uniform_buffers_per_shader_stage <= 15);
    assert!(limits.max_uniform_buffer_binding_size <= 64 * 1024);
    assert!(limits.max_storage_buffer_binding_size <= 2_147_483_644);
    assert!(limits.max_buffer_size <= 4_292_870_144);
    // The real card reports 31, not the downlevel default 16 (raw Vulkan is 32).
    assert!(limits.max_inter_stage_shader_variables <= 31);
    // Desktop NVIDIA (and so the profile) has no ASTC/ETC2: the mask removed them even
    // though the host adapter (lavapipe) has them.
    assert!(
        !device
            .device
            .features()
            .contains(wgpu::Features::TEXTURE_COMPRESSION_ASTC)
    );
    assert!(
        !device
            .device
            .features()
            .contains(wgpu::Features::TEXTURE_COMPRESSION_ETC2)
    );
    assert!(!device.capabilities.texture_compression_astc);
    assert!(!device.capabilities.texture_compression_etc2);
}

#[test]
fn the_pascal_profile_keeps_every_other_engine_request_the_adapter_has() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = pascal_device();
    let adapter = device.adapter.features();
    for feature in [
        wgpu::Features::TEXTURE_COMPRESSION_BC,
        wgpu::Features::TIMESTAMP_QUERY,
        wgpu::Features::MULTI_DRAW_INDIRECT_COUNT,
        wgpu::Features::INDIRECT_FIRST_INSTANCE,
        wgpu::Features::RG11B10UFLOAT_RENDERABLE,
        wgpu::Features::FLOAT32_FILTERABLE,
    ] {
        assert_eq!(
            device.device.features().contains(feature),
            adapter.contains(feature),
            "{feature:?}: the Pascal profile keeps what the adapter has (Pascal has it)"
        );
    }
}

#[test]
fn a_pascal_profile_device_needs_no_fallback_the_adapter_did_not() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = pascal_device();
    // Clamping to Pascal never activates a fallback the unclamped adapter would not:
    // Pascal (with the current driver) has every optional feature the engine requests.
    let unclamped_notes = RenderDevice::headless()
        .unwrap_or_else(|err| panic!("no headless device (lavapipe?): {err}"))
        .capabilities
        .fallback_notes();
    for note in device.capabilities.fallback_notes() {
        assert!(
            unclamped_notes.iter().any(|n| n.code == note.code),
            "the Pascal profile activated fallback `{}` that the adapter alone does not need",
            note.code
        );
    }
}

#[test]
fn a_pascal_profile_renders_a_visibly_lit_frame() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = pascal_device();
    let scene = lit_scene();
    let (renderer, pixels) = render(&device, &scene);
    assert_visibly_lit(&pixels);
    // Pascal (current driver) needs no fallback the adapter itself does not have, so the
    // frame's problems are exactly the adapter's own.
    let creation_notes = device.capabilities.fallback_notes();
    let at_creation = codes(&creation_notes);
    for problem in renderer.frame_problems() {
        assert!(
            at_creation.contains(&problem.code),
            "the frame invented fallback `{}` the device did not report",
            problem.code
        );
    }
}

#[test]
fn the_mobile_profile_clamps_the_device_to_the_mobile_tier() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = mobile_device();
    let limits = device.device.limits();
    assert!(limits.max_bind_groups >= 5, "the terrain draw needs group 4");
    assert!(limits.max_storage_buffers_per_shader_stage <= 8);
    // 4 storage textures is below the 7 the single-pass downsample wants: the per-mip
    // depth-pyramid fallback must be active (and reported).
    assert!(limits.max_storage_textures_per_shader_stage <= 4);
    assert!(limits.max_compute_workgroup_storage_size <= 16 * 1024);
    assert!(limits.max_compute_invocations_per_workgroup <= 256);
    assert!(limits.max_texture_dimension_2d <= 8192);
    // The masked-off features are off even though lavapipe has them.
    for feature in [
        wgpu::Features::TIMESTAMP_QUERY,
        wgpu::Features::MULTI_DRAW_INDIRECT_COUNT,
        wgpu::Features::INDIRECT_FIRST_INSTANCE,
        wgpu::Features::RG11B10UFLOAT_RENDERABLE,
        wgpu::Features::FLOAT32_FILTERABLE,
        wgpu::Features::TEXTURE_COMPRESSION_BC,
    ] {
        assert!(
            !device.device.features().contains(feature),
            "{feature:?}: masked off by the mobile profile"
        );
    }
}

#[test]
fn the_mobile_tier_reports_its_fallbacks() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = mobile_device();
    let notes = device.capabilities.fallback_notes();
    let reported = codes(&notes);
    for code in [
        "fallback_no_timestamps",
        "fallback_cpu_indirect_count",
        "fallback_zero_first_instance",
        "fallback_rgba16f_hdr",
        "fallback_no_float32_filter",
        // BC is off even though ASTC is on: the cooked desktop maps still CPU-decode.
        "fallback_no_block_compression",
        "fallback_per_mip_depth_pyramid",
        "fallback_visbuffer_storage",
    ] {
        assert!(
            reported.contains(&code),
            "fallback `{code}` active, got {reported:?}"
        );
    }
    // Bind groups and early depth stay available on the mobile tier (the engine's minimum):
    // no fallback for either.
    assert!(!reported.contains(&"fallback_no_bind_groups"), "{reported:?}");
    // The tier's feature mask keeps early depth; a device that has it still reports it after the
    // clamp. (Metal has no early depth: the note is the device's, not the tier's.)
    assert!(
        MinspecProfile::Mobile
            .feature_mask()
            .contains(wgpu::Features::SHADER_EARLY_DEPTH_TEST),
        "the Mobile tier must keep early depth"
    );
    if device
        .adapter
        .features()
        .contains(wgpu::Features::SHADER_EARLY_DEPTH_TEST)
    {
        assert!(
            device.capabilities.early_depth_test,
            "the adapter has early depth, so the clamped Mobile device must report it"
        );
        assert!(
            !reported.contains(&"fallback_no_early_depth_test"),
            "{reported:?}"
        );
    }
}

#[test]
fn a_mobile_profile_renders_a_visibly_lit_frame_and_reports_its_fallbacks() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = mobile_device();
    let scene = lit_scene();
    let (renderer, pixels) = render(&device, &scene);
    assert_visibly_lit(&pixels);
    // `render.problems` (the agent protocol reads `frame_problems`) carries the device's
    // capability fallbacks after a frame on the clamped device.
    let reported = codes(renderer.frame_problems());
    for code in ["fallback_no_timestamps", "fallback_per_mip_depth_pyramid"] {
        assert!(reported.contains(&code), "{code} missing from {reported:?}");
    }
}

#[test]
fn a_full_feature_device_reports_no_fallbacks_after_a_frame() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device =
        RenderDevice::headless().unwrap_or_else(|err| panic!("no headless device (lavapipe?): {err}"));
    let creation_notes = device.capabilities.fallback_notes();
    let at_creation = codes(&creation_notes);
    let scene = lit_scene();
    let (renderer, pixels) = render(&device, &scene);
    assert_visibly_lit(&pixels);
    for problem in renderer.frame_problems() {
        assert!(
            at_creation.contains(&problem.code),
            "the frame invented a fallback the device did not report at creation: `{}`",
            problem.code
        );
    }
    if at_creation.is_empty() {
        assert!(
            renderer.frame_problems().is_empty(),
            "a full-feature device reports no problems: {:?}",
            renderer.frame_problems()
        );
    }
}

/// The host checklist lists capability fallbacks as information, never as frame-breaking
/// errors: a min-spec host sees which fast paths are off without a failing checklist.
#[test]
fn the_host_checklist_lists_capability_fallbacks_as_information() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = mobile_device();
    let scene = lit_scene();
    let (renderer, _pixels) = render(&device, &scene);
    let frame: FrameScene<'_> = scene.as_frame_scene();
    let checklist: HostChecklist = renderer.host_checklist_report(&frame);
    assert!(
        checklist.errors().is_empty(),
        "capability fallbacks are not checklist errors"
    );
    let item = checklist
        .checks
        .iter()
        .find(|c| c.item.starts_with("device capability fallbacks"))
        .expect("the checklist names the device capability fallbacks");
    assert!(item.actual.contains("fallback_no_timestamps"), "{}", item.actual);
    assert!(item.ok, "the fallback list is informational");
}
