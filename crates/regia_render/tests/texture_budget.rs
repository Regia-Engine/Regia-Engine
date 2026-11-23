// SPDX-License-Identifier: MPL-2.0
//! The resident texture budget (`regia_render::texture_budget`, lane tex-budget): an eager
//! budgeted policy draws the unstreamed frame while the budget has room and keeps no CPU copy
//! of uploaded pixels; over the budget the least-recently-seen material drops its top mips (a
//! GPU copy) and the visible one sharpens; a level of uncompressed textures is reported loudly
//! (`render.problems`, the host checklist, `FrameRenderer::texture_memory`).

use regia_render::{
    DirectionalLightUniform, FogSettings, FrameMesh, FrameQuality, FrameRenderer, FrameScene, LabMaterial,
    LabTexture, LabVertex, RenderDevice, TextureStreaming,
};

/// A 4 m ground quad at `x`, UV 0..4 (1 m per UV unit).
fn quad(x: f32) -> Vec<LabVertex> {
    [[-2.0f32, -2.0], [2.0, -2.0], [2.0, 2.0], [-2.0, 2.0]]
        .into_iter()
        .map(|[qx, z]| LabVertex {
            position: [x + qx, 0.0, z],
            normal: [0.0, 1.0, 0.0],
            uv: [qx + 2.0, z + 2.0],
            tangent: [1.0, 0.0, 0.0, 1.0],
            color: [1.0; 4],
            joints: [0; 4],
            weights: [0.0; 4],
        })
        .collect()
}

const INDICES: [u32; 6] = [0, 2, 1, 0, 3, 2];

/// A `size`² pattern with detail at every scale.
fn pattern(size: u32, seed: u32) -> LabTexture {
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let h =
                (x.wrapping_mul(73_856_093) ^ y.wrapping_mul(19_349_663) ^ seed).wrapping_mul(2_654_435_761);
            let checker = if ((x / 8) + (y / 8)) % 2 == 0 { 200 } else { 60 };
            rgba.extend_from_slice(&[checker, (h >> 24) as u8, ((x * 255) / size) as u8, 255]);
        }
    }
    LabTexture {
        width: size,
        height: size,
        rgba,
    }
}

fn material(size: u32, seed: u32) -> LabMaterial {
    LabMaterial {
        albedo: Some(pattern(size, seed)),
        roughness: 0.6,
        metallic: 0.0,
        ..LabMaterial::default()
    }
}

/// One frame looking down at the quads from 1.5 m above `eye_x`.
fn scene<'a>(meshes: Vec<FrameMesh<'a>>, eye_x: f32, streaming: TextureStreaming) -> FrameScene<'a> {
    FrameScene {
        width: 96,
        height: 96,
        eye: [eye_x + 0.3, 1.5, 0.4],
        target: [eye_x, 0.0, -0.4],
        fov_y: 1.0,
        near: 0.05,
        far: 50.0,
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
        meshes,
    }
}

fn mesh<'a>(key: u64, vertices: &'a [LabVertex], material: &'a LabMaterial) -> FrameMesh<'a> {
    FrameMesh {
        is_static: false,
        mesh_key: key,
        material_key: key,
        object_key: key,
        vertices,
        indices: &INDICES,
        material,
        model: regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
        joints: &[],
        receiver_layer: 0,
        shadow: regia_render::ShadowCast::All,
        uv1: &[],
        lod_dither: 0.0,
    }
}

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

fn max_diff(a: &[u8], b: &[u8]) -> u8 {
    a.iter().zip(b).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0)
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn within_budget_the_first_frame_is_the_unstreamed_frame_and_no_cpu_copy_stays() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let vertices = quad(0.0);
    let material = LabMaterial {
        occlusion_map: Some(pattern(256, 2)),
        ..material(512, 1)
    };
    let reference = FrameRenderer::new()
        .render_offscreen(
            &device,
            &scene(vec![mesh(1, &vertices, &material)], 0.0, TextureStreaming::OFF),
        )
        .expect("unstreamed frame")
        .pixels;
    // Every game tier: eager, so the first frame is already the unstreamed one.
    for policy in [
        TextureStreaming::DESKTOP_HIGH,
        TextureStreaming::DESKTOP_ULTRA,
        TextureStreaming::MOBILE_HIGH,
    ] {
        let mut renderer = FrameRenderer::new();
        let frame = scene(vec![mesh(1, &vertices, &material)], 0.0, policy);
        let first = renderer
            .render_offscreen(&device, &frame)
            .expect("frame 0")
            .pixels;
        assert_eq!(max_diff(&first, &reference), 0, "{policy:?}");
        let report = renderer.stream_report();
        assert_eq!(report.materials, 1, "streamed under {policy:?}");
        assert_eq!(report.cpu_bytes, 0, "no CPU copy of uploaded pixels");
        let memory = renderer.texture_memory();
        assert_eq!(memory.textures.len(), 2);
        assert!(memory.textures.iter().all(|t| t.streamed && t.cpu_bytes == 0));
        assert_eq!(memory.textures[0].slot, "base_color", "largest first");
        assert_eq!(memory.textures[0].resident_width, 512);
        assert!(memory.streamed_bytes > 512 * 512 * 4 && memory.streamed_bytes < memory.budget_bytes);
        assert_eq!(memory.uncompressed, 2);
        // The only problems a frame may report are the device's own capability fallbacks (none on
        // lavapipe; Metal lacks timestamps, indirect count and forced early depth).
        let notes = device.capabilities.fallback_notes();
        assert!(
            renderer
                .frame_problems()
                .iter()
                .all(|p| notes.iter().any(|n| n.code == p.code)),
            "{:?}",
            renderer.frame_problems()
        );
    }
}

#[test]
fn over_budget_the_least_recently_seen_material_drops_its_top_mips() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let (va, vb, vc) = (quad(0.0), quad(20.0), quad(40.0));
    let (a, b, c) = (material(512, 1), material(512, 2), material(512, 3));
    // A 512² RGBA8 set with mips is 1 365 KiB; three do not fit in 3 MiB.
    // The per-frame upload is capped by bytes only (`upload_kib_per_frame`), not by the
    // wall-clock time budget: that one follows the measured upload rate, so a single slow
    // frame on a busy or older host (RTX 3080 Ti box, 12 threads) shrank it to 64 KiB and the
    // sets had not settled after the frame counts below. This test is about which material
    // the resident budget evicts, not the upload pacing (`texture_stream` unit tests).
    let policy = TextureStreaming {
        initial_max_px: 32,
        budget_mib: 3,
        fade_frames: 0,
        upload_us_per_frame: 1_000_000,
        ..TextureStreaming::DESKTOP_HIGH
    };
    let mut renderer = FrameRenderer::new();
    // The 3 MiB budget is the point of this test: keep it as set, not scaled with the GPU's
    // VRAM (v0.1.8 default, ADR-189: a 32 GB card would raise it to 8 GiB and nothing evicts).
    renderer.set_vram_scaled_budgets(false);
    // A, then B, then C come into view, one at a time, each close enough to want 512 px.
    for _ in 0..3 {
        renderer
            .render_offscreen(&device, &scene(vec![mesh(1, &va, &a)], 0.0, policy))
            .expect("A");
    }
    for _ in 0..3 {
        renderer
            .render_offscreen(&device, &scene(vec![mesh(2, &vb, &b)], 20.0, policy))
            .expect("B");
    }
    let memory = renderer.texture_memory();
    assert!(
        memory.textures.iter().all(|t| t.resident_width == 512),
        "two sets fit: {memory:?}"
    );
    for _ in 0..6 {
        renderer
            .render_offscreen(&device, &scene(vec![mesh(3, &vc, &c)], 40.0, policy))
            .expect("C");
    }
    let memory = renderer.texture_memory();
    let width = |key: u64| {
        memory
            .textures
            .iter()
            .find(|t| t.material_key == key)
            .map_or(0, |t| t.resident_width)
    };
    assert_eq!(width(3), 512, "the visible material sharpened: {memory:?}");
    assert_eq!(width(2), 512, "B was seen after A and keeps its set: {memory:?}");
    assert!(
        width(1) < 512,
        "A, least recently seen, dropped its top mips: {memory:?}"
    );
    assert!(
        memory.streamed_bytes <= 3 * 1024 * 1024,
        "the budget holds: {} bytes",
        memory.streamed_bytes
    );
    assert_eq!(memory.cpu_bytes, 0, "no CPU copies once settled");

    // A comes back into view: it sharpens again (rebuilding its chain from level 0) and B,
    // now the least recently seen, gives way.
    let back = scene(vec![mesh(1, &va, &a)], 0.0, policy);
    let mut last = Vec::new();
    for _ in 0..8 {
        last = renderer.render_offscreen(&device, &back).expect("A again").pixels;
    }
    let memory = renderer.texture_memory();
    let width = |key: u64| {
        memory
            .textures
            .iter()
            .find(|t| t.material_key == key)
            .map_or(0, |t| t.resident_width)
    };
    assert_eq!(width(1), 512, "{memory:?}");
    assert!(width(2) < 512, "{memory:?}");
    assert!(memory.streamed_bytes <= 3 * 1024 * 1024);
    // Settled, A draws exactly what an unbudgeted renderer draws.
    let reference = FrameRenderer::new()
        .render_offscreen(
            &device,
            &scene(vec![mesh(1, &va, &a)], 0.0, TextureStreaming::OFF),
        )
        .expect("reference")
        .pixels;
    assert_eq!(max_diff(&last, &reference), 0);
}

#[test]
fn a_level_of_uncompressed_textures_is_reported_with_its_worst_offenders() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let count = regia_render::UNCOMPRESSED_TEXTURE_LIMIT as u32 + 1;
    let vertices: Vec<Vec<LabVertex>> = (0..count).map(|i| quad(i as f32 * 5.0)).collect();
    let materials: Vec<LabMaterial> = (0..count).map(|i| material(8 + 4 * (i % 3), i)).collect();
    regia_render::name_material_textures(&materials[2], "M_Biggest");
    let meshes = (0..count as usize)
        .map(|i| mesh(i as u64 + 1, &vertices[i], &materials[i]))
        .collect();
    let frame = scene(meshes, 0.0, TextureStreaming::DESKTOP_HIGH);
    let mut renderer = FrameRenderer::new();
    renderer.render_offscreen(&device, &frame).expect("frame");
    let problems = renderer.frame_problems();
    let problem = problems
        .iter()
        .find(|p| p.code == regia_render::PROBLEM_UNCOMPRESSED_TEXTURES)
        .unwrap_or_else(|| panic!("{problems:?}"));
    assert!(problem.message.contains("regia cook"), "{}", problem.message);
    assert!(
        problem.message.contains("M_Biggest"),
        "names an offender: {}",
        problem.message
    );
    let memory = renderer.texture_memory();
    assert_eq!(memory.uncompressed, count as usize);
    assert_eq!(memory.textures[0].width, 16, "largest first");
    // The host checklist lists it as a difference, not a frame-breaking error.
    let checklist = renderer.host_checklist_report(&frame);
    let diffs: Vec<&str> = checklist.differences().iter().map(|c| c.item).collect();
    assert!(
        diffs.contains(&"uncompressed (RGBA8) material textures"),
        "{checklist}"
    );
    assert!(
        checklist
            .errors()
            .iter()
            .all(|c| c.item != "last frame" || !c.actual.contains("uncompressed")),
        "{checklist}"
    );
}
