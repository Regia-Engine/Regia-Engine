// SPDX-License-Identifier: MPL-2.0
//! Bindless step 4 (ADR-427): the renderer's own material path at 1,000 and 10,000 distinct
//! materials (`--ignored`, prints numbers). `tests/bindless_bench.rs` measures the binding
//! models apart from the renderer; this one measures `FrameRenderer` itself.
//!
//! N quads of one shared mesh, each with its own material key and colour, at LAB quality with
//! no shadows or prepass, so the frame is the forward pass's N material draws. Per material count
//! and mode, the CPU time of `render_into` (`FrameCpuTimings`: total and encode, p50 / p99), and
//! the buffers and bind groups the frame path created per frame after warm-up, on every thread
//! (`frame_path_counters_all_threads`, so creates off the frame thread count too). Modes:
//! - `steady`: the same scene every frame;
//! - `edit`: 1 % of the materials change colour each frame (the record is rewritten in place);
//! - `churn`: 1 % of the material keys are replaced each frame (new materials: a new entry each).
//!
//! It runs on two devices of the same adapter that differ in one limit:
//! - `default`: the adapter's limits;
//! - `uniform`: `max_storage_buffers_per_shader_stage` capped at 10, below bindless step 1's
//!   material-storage-buffer threshold (11, ADR-316), so a renderer with step 1 merged takes the
//!   per-material uniform path there.
//!
//! - `table`: the adapter's limits plus the texture table features (bindless level 3, ADR-863):
//!   every standard material samples through the one texture table and binds the one shared
//!   record group at its own dynamic offset. Only when the adapter has the features.
//!
//! Before step 1 is merged both devices run the same classic path, and the two columns show
//! the noise between runs. After it, `default` is the storage path wherever step 1 picks it
//! (not on Android, ADR-316). Each line also prints the opaque pass's material batches (one per
//! material on the per-material paths, one in all on `table`) and, on `table`, the table's
//! rebuilds and record groups.
//!
//! ```text
//! cargo test -p regia_render --release --test material_scale_bench -- --ignored --nocapture
//! ```

use regia_render::{
    CpuMesh, DirectionalLightUniform, FrameMesh, FrameQuality, FrameRenderer, FrameScene, LabMaterial,
    LabMesh, LabScene, LabVertex, RenderCapabilities, RenderDevice, frame_path_counters_all_threads,
};

/// Material counts the bench runs.
const SCALES: [usize; 2] = [1_000, 10_000];
/// Frames rendered before timing starts (the first one creates every material).
const WARM: usize = 10;
/// Timed frames per mode.
const FRAMES: usize = 60;
/// Render target side, pixels (each quad covers about 3 × 3 pixels at 10,000 materials).
const SIZE: u32 = 320;
/// Storage buffers per stage on the `uniform` device: one below step 1's threshold (ADR-316).
const UNIFORM_PATH_STORAGE_BUFFERS: u32 = 10;
/// Top bits of the bench's material keys (clear of the renderer's reserved top bytes).
const MATERIAL_KEY_BASE: u64 = 0x00B1_0000_0000_0000;
/// Top bits of the bench's object keys.
const OBJECT_KEY_BASE: u64 = 0x00B2_0000_0000_0000;

/// A unit quad facing +z.
fn quad() -> CpuMesh {
    let corners = [[-0.5, -0.5], [0.5, -0.5], [0.5, 0.5], [-0.5, 0.5]];
    CpuMesh {
        vertices: corners
            .iter()
            .map(|&[x, y]| LabVertex::from_pos_normal_uv([x, y, 0.0], [0.0, 0.0, 1.0], [x + 0.5, y + 0.5]))
            .collect(),
        indices: vec![0, 1, 2, 0, 2, 3],
    }
}

/// The one-mesh scene the frames are built from (camera, light, quality).
fn lab(mesh: &CpuMesh) -> LabScene {
    LabScene {
        width: SIZE,
        height: SIZE,
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
        directional: DirectionalLightUniform::new([0.0, -0.3, -1.0], [1.0; 3], 20_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial::default()],
        meshes: vec![LabMesh {
            vertices: mesh.vertices.clone(),
            indices: mesh.indices.clone(),
            ..LabMesh::unskinned()
        }],
    }
}

/// Material `id`'s colour at `generation` (an edit bumps it).
fn tint(id: u64, generation: u32) -> LabMaterial {
    let k = id.wrapping_mul(7) + u64::from(generation) * 3;
    LabMaterial {
        base_color: [
            (k * 37 % 251) as f32 / 255.0,
            (k * 61 % 251) as f32 / 255.0,
            (k * 97 % 251) as f32 / 255.0,
            1.0,
        ],
        roughness: 0.3 + (k % 5) as f32 * 0.1,
        ..LabMaterial::default()
    }
}

/// Column-major model matrix of quad `i` in a `side × side` grid filling the view.
fn grid_model(i: usize, side: usize) -> [[f32; 4]; 4] {
    let span = 2.6;
    let cell = span / side as f32;
    let (x, y) = ((i % side) as f32, (i / side) as f32);
    [
        [cell * 0.9, 0.0, 0.0, 0.0],
        [0.0, cell * 0.9, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [
            -span / 2.0 + (x + 0.5) * cell,
            -span / 2.0 + (y + 0.5) * cell,
            0.0,
            1.0,
        ],
    ]
}

/// A device on `base`'s adapter with `base`'s features (plus `extra`) and limits, apart from an
/// optional cap on storage buffers per shader stage. Every bench device is built this way, so
/// they differ only in that limit or those features.
fn device_with(base: &RenderDevice, cap: Option<u32>, extra: wgpu::Features) -> RenderDevice {
    let mut limits = base.device.limits();
    if let Some(cap) = cap {
        limits.max_storage_buffers_per_shader_stage = limits.max_storage_buffers_per_shader_stage.min(cap);
    }
    let (device, queue) = regia_core::tasks::block_on(base.adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("material scale bench"),
        required_features: base.device.features() | extra,
        required_limits: limits,
        ..Default::default()
    }))
    .expect("bench device on the renderer's adapter");
    let capabilities = RenderCapabilities::from_adapter(&base.adapter, &device);
    RenderDevice::from_parts(
        base.instance.clone(),
        base.adapter.clone(),
        device,
        queue,
        capabilities,
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Steady,
    Edit,
    Churn,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Self::Steady => "steady",
            Self::Edit => "edit",
            Self::Churn => "churn",
        }
    }
}

fn percentile(sorted: &[f32], p: f32) -> f32 {
    sorted[((sorted.len() - 1) as f32 * p).round() as usize]
}

/// Renders the warm-up and timed frames of one mode and prints its line; returns the table row.
fn run(device: &RenderDevice, label: &str, mesh: &CpuMesh, n: usize, mode: Mode) -> String {
    let lab = lab(mesh);
    let base = lab.as_frame_scene_with_quality(FrameQuality::LAB);
    let template = base.meshes[0];
    let side = (n as f64).sqrt().ceil() as usize;
    let models: Vec<[[f32; 4]; 4]> = (0..n).map(|i| grid_model(i, side)).collect();
    // Draw slot `i` shows material `ids[i]` at `generations[i]`.
    let mut ids: Vec<u64> = (0..n as u64).collect();
    let mut generations = vec![0u32; n];
    let mut next_id = n as u64;
    let per_frame = (n / 100).max(1);
    let mut renderer = FrameRenderer::new();
    let mut first_frame_ms = 0.0;
    let mut total = Vec::with_capacity(FRAMES);
    let mut encode = Vec::with_capacity(FRAMES);
    let mut at_warm = None;
    for frame in 0..WARM + FRAMES {
        if frame > 0 {
            for k in 0..per_frame {
                let slot = (frame * per_frame + k) % n;
                match mode {
                    Mode::Steady => {}
                    Mode::Edit => generations[slot] += 1,
                    Mode::Churn => {
                        ids[slot] = next_id;
                        next_id += 1;
                    }
                }
            }
        }
        let materials: Vec<LabMaterial> = ids
            .iter()
            .zip(&generations)
            .map(|(&id, &generation)| tint(id, generation))
            .collect();
        let scene = FrameScene {
            meshes: materials
                .iter()
                .zip(&ids)
                .zip(&models)
                .enumerate()
                .map(|(i, ((material, &id), &model))| FrameMesh {
                    material_key: MATERIAL_KEY_BASE | id,
                    object_key: OBJECT_KEY_BASE | i as u64,
                    material,
                    model,
                    ..template
                })
                .collect(),
            ..base.clone()
        };
        if frame == WARM {
            at_warm = Some((frame_path_counters_all_threads(), renderer.cache_stats()));
        }
        let _ = renderer.render_offscreen(device, &scene).expect("bench frame");
        let cpu = renderer.cpu_timings();
        if frame == 0 {
            first_frame_ms = cpu.total_ms;
        }
        if frame >= WARM {
            total.push(cpu.total_ms);
            encode.push(cpu.encode_ms);
        }
    }
    let (counters_warm, stats_warm) = at_warm.expect("warm-up frame");
    let created = frame_path_counters_all_threads().since(counters_warm);
    let stats = renderer.cache_stats();
    let new_materials = stats.material_uploads - stats_warm.material_uploads;
    let batches = renderer.draw_counts().opaque.material_batches;
    let table = renderer.texture_table_stats().map_or_else(String::new, |table| {
        format!(
            "; table: {} rebuilds, {} record groups, {} live",
            table.rebuilds, table.record_groups, table.live
        )
    });
    // The work happened: the first frame created every material, and churn exactly one entry
    // per replaced key (edits and steady frames none).
    let expected_new = if mode == Mode::Churn {
        (per_frame * FRAMES) as u64
    } else {
        0
    };
    assert_eq!(
        new_materials,
        expected_new,
        "{label} n={n} {}: material entries created",
        mode.name()
    );
    assert!(stats.materials >= n, "{label} n={n}: every material resident");
    total.sort_by(f32::total_cmp);
    encode.sort_by(f32::total_cmp);
    let frames = FRAMES as f64;
    let buffers = created.buffers as f64 / frames;
    let bind_groups = created.bind_groups as f64 / frames;
    println!(
        "material-scale {label} n={n} {}: first frame {first_frame_ms:.2} ms; render_into p50 {:.3} ms p99 {:.3} ms, encode p50 {:.3} ms; per frame {buffers:.1} buffers, {bind_groups:.1} bind groups; {new_materials} new materials; {batches} opaque material batches{table}",
        mode.name(),
        percentile(&total, 0.5),
        percentile(&total, 0.99),
        percentile(&encode, 0.5),
    );
    format!(
        "| {label} | {n} | {} | {first_frame_ms:.2} | {:.3} / {:.3} | {:.3} | {buffers:.1} / {bind_groups:.1} |",
        mode.name(),
        percentile(&total, 0.5),
        percentile(&total, 0.99),
        percentile(&encode, 0.5),
    )
}

#[test]
#[ignore = "bench: prints numbers"]
fn material_scale_bench() {
    let base = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe or use WARP): {err}"),
    };
    let info = base.adapter.get_info();
    println!("material scale bench on {} ({:?})", info.name, info.backend);
    if cfg!(debug_assertions) {
        println!("WARNING: a debug build; run with --release for numbers worth recording");
    }
    let mut devices = vec![
        ("default", device_with(&base, None, wgpu::Features::empty())),
        (
            "uniform",
            device_with(&base, Some(UNIFORM_PATH_STORAGE_BUFFERS), wgpu::Features::empty()),
        ),
    ];
    // Bindless level 3 (ADR-863), when the adapter can carry the texture table.
    let table_features = regia_render::texture_table::device_features_for_level(
        regia_render::texture_table::TEXTURE_TABLE_LEVEL,
        base.adapter.features(),
    );
    if table_features.is_empty() {
        println!("no `table` column: the adapter lacks the texture table features");
    } else {
        devices.push(("table", device_with(&base, None, table_features)));
    }
    let mesh = quad();
    let mut rows = Vec::new();
    for n in SCALES {
        for mode in [Mode::Steady, Mode::Edit, Mode::Churn] {
            for (label, device) in &devices {
                rows.push(run(device, label, &mesh, n, mode));
            }
        }
    }
    // The table `docs/evidence/bindless/README.md` records (milliseconds).
    println!();
    println!(
        "| device | materials | mode | first frame | render_into p50 / p99 | encode p50 | buffers / bind groups per frame |"
    );
    println!("|---|---|---|---|---|---|---|");
    for row in rows {
        println!("{row}");
    }
}
