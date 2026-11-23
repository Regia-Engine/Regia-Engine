// SPDX-License-Identifier: MPL-2.0
//! Out-of-memory survival of material texture streaming (ADR-223, v0.1.16), on lavapipe.
//!
//! A DO27 tester's Radeon 610M lost its device on a wgpu out-of-memory error in
//! `stream_materials`. Streamed textures are now created inside an `OutOfMemory` error scope, so a
//! failed allocation is caught: the set retries one level smaller (first use) or is dropped
//! (growth), streaming backs off, the texture budget steps down on the first error, and an error
//! soon after a trim holds the streamed textures a mip lower until the pressure ends.
//!
//! The failures are real device allocations: the test hook
//! (`texture_stream::inject_stream_texture_oom`) makes the next streamed creations ask for a legal
//! but TiB-class texture, which the device refuses with an out-of-memory error; the upload hook
//! (`inject_stream_upload_oom`) does the same inside a set's upload scope. Both hooks are behind
//! the `test-hooks` cargo feature (this crate's dev-dependency turns it on).
//!
//! Its own test binary, one `#[test]`: the memory-pressure state (budget steps, the hold, the
//! caught-error count) is process-wide.

use std::time::{Duration, Instant};

use regia_render::gpu_health;
use regia_render::texture_stream::{
    inject_stream_texture_oom, inject_stream_upload_oom, injected_stream_texture_ooms_left,
    injected_stream_upload_ooms_left,
};
use regia_render::{
    DirectionalLightUniform, FogSettings, FrameMesh, FrameQuality, FrameRenderer, FrameScene, LabMaterial,
    LabTexture, LabVertex, RenderDevice, TextureStreaming,
};

const MATERIALS: usize = 200;
const EDGE: u32 = 256;
const MIB: u64 = 1024 * 1024;

/// A 1 m quad at grid cell `i` (20 × 10 cells on the ground), UV 0..1.
fn quad(i: usize) -> Vec<LabVertex> {
    let (x, z) = ((i % 20) as f32 * 1.1 - 11.0, (i / 20) as f32 * 1.1 - 5.5);
    [[0.0f32, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]]
        .into_iter()
        .map(|[u, v]| LabVertex {
            position: [x + u, 0.0, z + v],
            normal: [0.0, 1.0, 0.0],
            uv: [u, v],
            tangent: [1.0, 0.0, 0.0, 1.0],
            color: [1.0; 4],
            joints: [0; 4],
            weights: [0.0; 4],
        })
        .collect()
}

const INDICES: [u32; 6] = [0, 2, 1, 0, 3, 2];

fn material(seed: u32) -> LabMaterial {
    let mut rgba = Vec::with_capacity((EDGE * EDGE * 4) as usize);
    for y in 0..EDGE {
        for x in 0..EDGE {
            let h =
                (x.wrapping_mul(73_856_093) ^ y.wrapping_mul(19_349_663) ^ seed).wrapping_mul(2_654_435_761);
            rgba.extend_from_slice(&[(h >> 24) as u8, (seed * 37) as u8, ((x * 255) / EDGE) as u8, 255]);
        }
    }
    LabMaterial {
        albedo: Some(LabTexture {
            width: EDGE,
            height: EDGE,
            rgba,
        }),
        roughness: 0.6,
        ..LabMaterial::default()
    }
}

/// Everything to its authored size (`resident_all`), low mips first, uploads uncapped in practice.
fn policy(budget_mib: u32) -> TextureStreaming {
    TextureStreaming {
        enabled: true,
        initial_max_px: 32,
        max_px: 0,
        upload_kib_per_frame: 64 * 1024,
        upload_us_per_frame: 1_000_000,
        fade_frames: 0,
        need_margin_percent: 200,
        resident_all: true,
        budget_mib,
        eager: false,
    }
}

fn scene<'a>(meshes: &[FrameMesh<'a>], streaming: TextureStreaming) -> FrameScene<'a> {
    FrameScene {
        width: 96,
        height: 64,
        eye: [0.0, 14.0, 6.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 1.2,
        near: 0.1,
        far: 60.0,
        depth_prepass: false,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 0.0,
        ambient_lux: 0.0,
        double_sided: true,
        post: None,
        fog: FogSettings::default(),
        directional: DirectionalLightUniform::new([0.3, -1.0, 0.2], [1.0, 1.0, 1.0], 1.0),
        local_lights: &[],
        quality: FrameQuality {
            texture_streaming: streaming,
            ..FrameQuality::LAB
        },
        meshes: meshes.to_vec(),
    }
}

fn meshes<'a>(
    vertices: &'a [Vec<LabVertex>],
    materials: &'a [LabMaterial],
    count: usize,
) -> Vec<FrameMesh<'a>> {
    (0..count)
        .map(|i| FrameMesh {
            is_static: false,
            mesh_key: 1 + i as u64,
            material_key: 1 + i as u64,
            object_key: 1 + i as u64,
            vertices: &vertices[i],
            indices: &INDICES,
            material: &materials[i],
            model: regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
            joints: &[],
            receiver_layer: 0,
            shadow: regia_render::ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        })
        .collect()
}

/// Resident widths of every material's base colour, smallest first.
fn resident_widths(renderer: &FrameRenderer) -> Vec<u32> {
    let mut widths: Vec<u32> = renderer
        .texture_memory()
        .textures
        .iter()
        .filter(|t| t.streamed)
        .map(|t| t.resident_width)
        .collect();
    widths.sort_unstable();
    widths
}

/// Renders until `done`, at most `limit`, asserting the device stays healthy every frame.
fn render_until(
    renderer: &mut FrameRenderer,
    device: &RenderDevice,
    frame: &FrameScene<'_>,
    frames: &mut u32,
    limit: Duration,
    mut done: impl FnMut(&FrameRenderer) -> bool,
) -> bool {
    let start = Instant::now();
    loop {
        renderer.render_offscreen(device, frame).expect("frame");
        *frames += 1;
        assert!(!gpu_health::device_lost(), "the device survives (frame {frames})");
        assert_eq!(
            gpu_health::gpu_error_count(),
            0,
            "no error reaches the uncaptured handler"
        );
        if done(renderer) {
            return true;
        }
        if start.elapsed() > limit {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn stream_oom_survives() {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    };
    gpu_health::install_device_hooks(&device.device);
    // One more than MATERIALS: the last one arrives later (step 4).
    let vertices: Vec<Vec<LabVertex>> = (0..=MATERIALS).map(quad).collect();
    let materials: Vec<LabMaterial> = (0..=MATERIALS).map(|i| material(i as u32 + 1)).collect();
    let meshes_plus = meshes(&vertices, &materials, MATERIALS + 1);
    let meshes = &meshes_plus[..MATERIALS];
    let mut frames = 0u32;

    // 1. A 20 MiB budget (the 200 floor-size mip chains alone are 17.5 MB, so a smaller budget cannot be met) for 200 materials that want ~70 MB at full size: streaming stays under
    //    it with no error at all (no out-of-memory path is needed to respect a budget).
    let small = scene(meshes, policy(20));
    let mut renderer = FrameRenderer::new();
    // A discrete card scales a small budget up to a quarter of its VRAM (by design); this test is about the small budget.
    renderer.set_vram_scaled_budgets(false);
    for _ in 0..40 {
        renderer.render_offscreen(&device, &small).expect("frame");
        frames += 1;
        let report = renderer.stream_report();
        assert_eq!(report.materials, MATERIALS as u32);
        assert!(
            report.resident_bytes <= 20 * MIB,
            "{} bytes over 20 MiB",
            report.resident_bytes
        );
    }
    assert!(
        renderer.stream_report().resident_bytes > 8 * MIB,
        "the sets grew into the budget: {} bytes",
        renderer.stream_report().resident_bytes
    );
    assert_eq!(gpu_health::captured_oom_count(), 0);
    assert_eq!(gpu_health::gpu_error_count(), 0);
    assert_eq!(gpu_health::texture_budget_steps(), 0);
    drop(renderer);

    // 2. 512 MiB, and the first three streamed creations of the first frame fail on the device:
    //    the first material's first set is retried one level smaller each time (32 px floor →
    //    4 px).
    let roomy = scene(meshes, policy(512));
    let mut renderer = FrameRenderer::new();
    renderer.set_vram_scaled_budgets(false);
    inject_stream_texture_oom(3);
    renderer.render_offscreen(&device, &roomy).expect("first frame");
    frames += 1;
    assert_eq!(injected_stream_texture_ooms_left(), 0);
    assert!(!gpu_health::device_lost());
    assert_eq!(
        gpu_health::gpu_error_count(),
        0,
        "the failures were caught, not uncaptured"
    );
    assert_eq!(
        gpu_health::captured_oom_count(),
        3,
        "three caught out-of-memory errors"
    );
    assert_eq!(
        gpu_health::texture_budget_steps(),
        1,
        "the first out-of-memory error steps the budget"
    );
    let widths = resident_widths(&renderer);
    assert_eq!(widths.len(), MATERIALS);
    assert_eq!(widths[0], 4, "the failed set retried three levels lower");
    assert!(
        widths[1..].iter().all(|&w| w == 32),
        "the others start at the floor: {widths:?}"
    );
    assert_eq!(renderer.stream_report().materials, MATERIALS as u32);

    // The budget is stepped (512 → 256 MiB) and streaming backs off: nothing uploads.
    renderer.render_offscreen(&device, &roomy).expect("frame");
    frames += 1;
    assert_eq!(renderer.texture_budget_bytes(), 256 * MIB);
    assert!(renderer.texture_streaming_backing_off());
    assert_eq!(
        renderer.stream_report().uploaded_bytes,
        0,
        "no uploads while backing off"
    );
    let below = renderer.stream_report().levels_below_cap;
    assert!(below > 0);

    // 3. After the back-off streaming recovers: every set grows to its full size, material 1 too.
    let settled = render_until(
        &mut renderer,
        &device,
        &roomy,
        &mut frames,
        Duration::from_secs(60),
        |r| r.stream_report().levels_below_cap == 0,
    );
    assert!(
        settled,
        "streaming recovered ({} levels below cap)",
        renderer.stream_report().levels_below_cap
    );
    assert!(!renderer.texture_streaming_backing_off());
    assert!(
        resident_widths(&renderer).iter().all(|&w| w == EDGE),
        "the failed set too"
    );
    let full = renderer.stream_report().resident_bytes;
    assert!(
        full <= renderer.texture_budget_bytes(),
        "resident within the stepped budget"
    );

    // 4. Memory runs out again soon after, on a real allocation: a new material arrives and its
    //    first upload fails once (retried one level smaller). The error comes within a minute of
    //    the last trim, so the pressure persists: from the next frame every streamed set gives up
    //    a level's worth (least recently seen first, the ones in view too) and stays held.
    std::thread::sleep(Duration::from_millis(5100));
    let plus = scene(&meshes_plus, policy(512));
    let caught = gpu_health::captured_oom_count();
    inject_stream_texture_oom(1);
    renderer.render_offscreen(&device, &plus).expect("frame");
    frames += 1;
    assert_eq!(gpu_health::captured_oom_count(), caught + 1);
    assert_eq!(
        resident_widths(&renderer)[0],
        16,
        "the new material's first set retried one level lower"
    );
    assert!(
        gpu_health::texture_mips_held(),
        "an out-of-memory error soon after a trim holds the mips"
    );
    assert_eq!(gpu_health::texture_budget_steps(), 2);
    renderer.render_offscreen(&device, &plus).expect("frame");
    frames += 1;
    assert!(renderer.texture_streaming_held());
    let dropped = renderer.stream_report();
    assert!(
        dropped.resident_bytes * 2 < full,
        "about one level of every set freed: {} of {full} bytes left",
        dropped.resident_bytes
    );
    assert!(
        dropped.levels_below_cap >= MATERIALS as u32,
        "a level's worth of every set, least recently seen first: {} levels",
        dropped.levels_below_cap
    );
    for _ in 0..30 {
        renderer.render_offscreen(&device, &plus).expect("frame");
        frames += 1;
        assert!(
            renderer.stream_report().resident_bytes <= dropped.resident_bytes,
            "held: no growth back while the pressure lasts"
        );
    }

    // 5. A growth step whose texture the device refuses (after the back-off of the last error):
    //    the larger set is dropped, the material keeps what it has, nothing is lost.
    gpu_health::release_texture_mip_hold();
    let resumed = render_until(
        &mut renderer,
        &device,
        &plus,
        &mut frames,
        Duration::from_secs(60),
        |r| !r.texture_streaming_backing_off() && !r.texture_streaming_held(),
    );
    assert!(resumed);
    let before = renderer.stream_report();
    inject_stream_texture_oom(1);
    let caught = gpu_health::captured_oom_count();
    let failed = render_until(
        &mut renderer,
        &device,
        &plus,
        &mut frames,
        Duration::from_secs(10),
        |_| injected_stream_texture_ooms_left() == 0,
    );
    assert!(failed, "a growth step tried a creation");
    assert_eq!(gpu_health::captured_oom_count(), caught + 1);
    assert!(renderer.texture_streaming_backing_off());
    assert!(
        renderer.stream_report().resident_bytes >= before.resident_bytes,
        "nothing resident was lost"
    );

    // 6. The pressure ended (the hold released): every set grows back to full size.
    let regrown = render_until(
        &mut renderer,
        &device,
        &plus,
        &mut frames,
        Duration::from_secs(90),
        |r| r.stream_report().levels_below_cap == 0,
    );
    assert!(
        regrown,
        "the sets grew back ({} levels below cap)",
        renderer.stream_report().levels_below_cap
    );
    // Every material's set is the same size: 201 of them at full size.
    assert_eq!(
        renderer.stream_report().resident_bytes,
        full / MATERIALS as u64 * (MATERIALS as u64 + 1)
    );
    while frames < 300 {
        renderer.render_offscreen(&device, &plus).expect("frame");
        frames += 1;
    }
    assert!(!gpu_health::device_lost(), "the device survived {frames} frames");
    assert_eq!(gpu_health::gpu_error_count(), 0);
    let codes: Vec<&str> = renderer.frame_problems().iter().map(|p| p.code).collect();
    assert!(
        codes.contains(&gpu_health::PROBLEM_VRAM_PRESSURE),
        "render.problems: {codes:?}"
    );
    assert!(
        !codes.contains(&gpu_health::PROBLEM_GPU_ERRORS),
        "no uncaptured error: {codes:?}"
    );

    // 7. A renderer created after all of the above: the caught failures were another renderer's.
    //    It does not back off for them (it used to, for 5 s with a zero upload budget).
    let mut fresh = FrameRenderer::new();
    fresh.set_vram_scaled_budgets(false);
    let few = scene(&meshes_plus[..4], policy(512));
    fresh.render_offscreen(&device, &few).expect("frame");
    fresh.render_offscreen(&device, &few).expect("frame");
    assert!(
        !fresh.texture_streaming_backing_off(),
        "a new renderer does not back off for old failures"
    );
    assert!(
        fresh.stream_report().uploaded_bytes > 0,
        "and streams from its second frame"
    );
    drop(fresh);

    // 8. The device refuses the staging memory of a first set's upload: that is caught too, and
    //    the set retries one level smaller, like a refused texture.
    let mut renderer = FrameRenderer::new();
    renderer.set_vram_scaled_budgets(false);
    let caught = gpu_health::captured_oom_count();
    inject_stream_upload_oom(1);
    renderer.render_offscreen(&device, &few).expect("frame");
    assert_eq!(injected_stream_upload_ooms_left(), 0);
    assert_eq!(
        gpu_health::captured_oom_count(),
        caught + 1,
        "the upload failure was caught"
    );
    assert_eq!(gpu_health::gpu_error_count(), 0, "not uncaptured");
    assert!(!gpu_health::device_lost());
    let widths = resident_widths(&renderer);
    assert_eq!(
        widths,
        [16, 32, 32, 32],
        "the refused set retried one level lower"
    );
    drop(renderer);

    // 9. A set whose every level the device refuses (a slot with no texture at all) holds no
    //    memory: the resident bytes count only the sets that have one (they used to count it
    //    whole).
    std::thread::sleep(Duration::from_millis(100));
    let mut renderer = FrameRenderer::new();
    renderer.set_vram_scaled_budgets(false);
    // 256 px base colour: 9 levels, the floor at 32 px (skip 3): tries at skips 3..=8.
    inject_stream_texture_oom(6);
    renderer.render_offscreen(&device, &few).expect("frame");
    assert_eq!(injected_stream_texture_ooms_left(), 0);
    assert_eq!(gpu_health::gpu_error_count(), 0);
    assert!(!gpu_health::device_lost());
    // The 32 px chain: (32² + 16² + 8² + 4² + 2² + 1) texels × 4 bytes.
    let floor_set = 4 * (1024 + 256 + 64 + 16 + 4 + 1);
    assert_eq!(
        renderer.stream_report().resident_bytes,
        3 * floor_set,
        "the material without a texture holds nothing"
    );
}
