// SPDX-License-Identifier: MPL-2.0
//! The per-preset frame work ratchet (ADR-2711, `docs/06-testing-and-ci.md` § "Frame work ratchet").
//!
//! Four fixed synthetic scenes (a forward box, a 10,000-object field, a terrain lab heightfield and a
//! closed GI room) are drawn at every shipped preset at 160 × 90, and each run's device-independent
//! counts ([`regia_render::FrameWork`]: passes, dispatches and workgroups, draws, triangles, binds,
//! allocations, uploads, heap allocations) are compared with the committed table
//! `tests/data/frame_work.ron`. A count more than 10 % above its row fails, naming the row; a count
//! more than 10 % under it prints a note, so the table only tightens. Re-record after a justified
//! change with
//!
//! ```text
//! REGIA_FRAME_WORK_RECORD=1 cargo test -p regia_render --test frame_work_ratchet -- --nocapture
//! ```
//!
//! and give the reason in the commit message, as for a golden image.
//!
//! The rows are counted on the CPU reference adapter (lavapipe, `device_type == Cpu`); another adapter
//! prints its counts and skips the compare (its fallbacks change which passes exist). Each scene runs
//! with one encode thread so the binds per pass do not depend on the host's core count.
//!
//! The same run proves the ratchet bites (a test-only extra pass registered at `AfterPost` with its own
//! full-resolution target must fail the `mobile_low` row) and counts the VRAM rows: tracked texture and
//! buffer bytes of the forward box at each `BudgetTier`'s resolution against the tier's VRAM budget
//! (`docs/perf-log.md`, "counted, device-independent").
//!
//! One test in its own binary: the counters are process-wide and a tagging allocator is installed.
//! On the build server's lavapipe the whole run takes about the time printed at its end (see the docs/06 page).

use std::time::Instant;

use regia_core::glam::{Mat4, Vec3};
use regia_core::memory::TaggingAllocator;
use regia_render::frame_work::counted::CountedEncoder;
use regia_render::frame_work::ratchet::{
    self, RECORD_COMMAND, RECORD_ENV, RatchetRow, RatchetTable, TABLE_FORMAT_VERSION, VramRow,
};
use regia_render::memory_budget::BudgetTier;
use regia_render::{
    DirectionalLightUniform, FramePass, FramePassContext, FrameQuality, FrameRenderer, FrameTarget,
    FrameWork, FrameWorkProbe, GraphSlot, LabLocalLight, LabMaterial, LabMesh, LabPointLight, LabScene,
    LabVertex, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset,
};

#[global_allocator]
static ALLOC: TaggingAllocator = TaggingAllocator::system();

const W: u32 = 160;
const H: u32 = 90;
/// Frames drawn before the steady count (pipelines, targets, uploads, temporal start-up).
const WARM_FRAMES: u64 = 4;
/// Frames the steady count sums.
const STEADY_FRAMES: u64 = 2;
/// The VRAM rows draw large targets on a CPU rasterizer: only what allocates them.
const VRAM_WARM_FRAMES: u64 = 2;
/// Steady frames of a VRAM row.
const VRAM_STEADY_FRAMES: u64 = 1;
/// Objects in the instanced field.
const FIELD_OBJECTS: usize = 10_000;

const TABLE_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/frame_work.ron");

#[derive(Clone, Copy, Debug)]
enum Scene {
    ForwardBox,
    InstancedField,
    TerrainLab,
    GiRoom,
}

impl Scene {
    const ALL: [Self; 4] = [
        Self::ForwardBox,
        Self::InstancedField,
        Self::TerrainLab,
        Self::GiRoom,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::ForwardBox => "forward_box",
            Self::InstancedField => "instanced_field",
            Self::TerrainLab => "terrain_lab",
            Self::GiRoom => "gi_room",
        }
    }
}

fn boxed(half: [f32; 3], model: Mat4, material: usize) -> LabMesh {
    let (vertices, indices) = regia_render::box_mesh(half);
    LabMesh {
        vertices,
        indices,
        material,
        model: model.to_cols_array_2d(),
        ..LabMesh::unskinned()
    }
}

fn matte(rgb: [f32; 3]) -> LabMaterial {
    LabMaterial {
        base_color: [rgb[0], rgb[1], rgb[2], 1.0],
        roughness: 0.8,
        ..LabMaterial::default()
    }
}

/// A heightfield grid of `n × n` quads, `size` metres across (deterministic rolling hills).
fn heightfield(n: u32, size: f32) -> (Vec<LabVertex>, Vec<u32>) {
    let height =
        |x: f32, z: f32| 1.5 * (x * 0.21).sin() * (z * 0.17).cos() + 0.6 * (x * 0.53 + z * 0.37).sin();
    let step = size / n as f32;
    let mut vertices = Vec::with_capacity(((n + 1) * (n + 1)) as usize);
    for j in 0..=n {
        for i in 0..=n {
            let x = -size * 0.5 + i as f32 * step;
            let z = -size * 0.5 + j as f32 * step;
            let y = height(x, z);
            let dx = height(x + 0.05, z) - height(x - 0.05, z);
            let dz = height(x, z + 0.05) - height(x, z - 0.05);
            let normal = Vec3::new(-dx / 0.1, 1.0, -dz / 0.1).normalize();
            vertices.push(LabVertex::from_pos_normal_uv(
                [x, y, z],
                normal.to_array(),
                [i as f32 / n as f32, j as f32 / n as f32],
            ));
        }
    }
    let mut indices = Vec::with_capacity((n * n * 6) as usize);
    for j in 0..n {
        for i in 0..n {
            let a = j * (n + 1) + i;
            let b = a + 1;
            let c = a + n + 1;
            let d = c + 1;
            indices.extend_from_slice(&[a, c, b, b, c, d]);
        }
    }
    (vertices, indices)
}

fn lab(scene: Scene, width: u32, height: u32) -> LabScene {
    let sun = DirectionalLightUniform::new([0.3, -0.85, 0.25], [1.0, 0.97, 0.92], 90_000.0);
    let base = LabScene {
        width,
        height,
        eye: [0.0, 3.0, 8.0],
        target: [0.0, 0.5, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 300.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 14.0,
        ambient_lux: 15_000.0,
        double_sided: false,
        post: Some(PostSettings {
            exposure_ev: 14.0,
            ..PostSettings::default()
        }),
        directional: sun,
        local_lights: Vec::new(),
        materials: vec![matte([0.4, 0.4, 0.38]), matte([0.7, 0.25, 0.2])],
        meshes: Vec::new(),
    };
    match scene {
        Scene::ForwardBox => LabScene {
            meshes: vec![
                boxed(
                    [10.0, 0.05, 10.0],
                    Mat4::from_translation(Vec3::new(0.0, -0.05, 0.0)),
                    0,
                ),
                boxed(
                    [0.75, 0.75, 0.75],
                    Mat4::from_translation(Vec3::new(0.0, 0.75, 0.0)),
                    1,
                ),
            ],
            ..base
        },
        Scene::InstancedField => {
            let side = (FIELD_OBJECTS as f32).sqrt().ceil() as usize;
            let mut meshes = vec![boxed(
                [60.0, 0.05, 60.0],
                Mat4::from_translation(Vec3::new(0.0, -0.05, 0.0)),
                0,
            )];
            meshes.extend((0..FIELD_OBJECTS).map(|k| {
                let (i, j) = (k % side, k / side);
                let at = Vec3::new(i as f32 * 1.1 - 55.0, 0.2, j as f32 * 1.1 - 55.0);
                boxed([0.2, 0.2, 0.2], Mat4::from_translation(at), 1 - (k % 2))
            }));
            LabScene {
                eye: [0.0, 18.0, 40.0],
                target: [0.0, 0.0, 0.0],
                meshes,
                ..base
            }
        }
        Scene::TerrainLab => {
            let (vertices, indices) = heightfield(128, 120.0);
            LabScene {
                eye: [0.0, 12.0, 45.0],
                target: [0.0, 0.0, 0.0],
                meshes: vec![LabMesh {
                    vertices,
                    indices,
                    material: 0,
                    ..LabMesh::unskinned()
                }],
                ..base
            }
        }
        Scene::GiRoom => {
            let wall = |half: [f32; 3], at: [f32; 3], material: usize| {
                boxed(half, Mat4::from_translation(Vec3::from_array(at)), material)
            };
            LabScene {
                eye: [0.0, 1.6, 3.5],
                target: [0.0, 1.2, 0.0],
                directional_shadows: false,
                ambient_lux: 0.0,
                exposure_ev: 6.0,
                post: Some(PostSettings {
                    exposure_ev: 6.0,
                    ..PostSettings::default()
                }),
                materials: vec![
                    matte([0.8, 0.8, 0.8]),
                    matte([0.8, 0.1, 0.1]),
                    matte([0.1, 0.6, 0.15]),
                ],
                local_lights: vec![LabLocalLight::Point(LabPointLight {
                    position: [0.0, 2.4, 0.0],
                    color: [1.0, 0.95, 0.85],
                    intensity: 1500.0,
                    radius: 8.0,
                    cast_shadows: true,
                    source_radius: None,
                })],
                meshes: vec![
                    wall([2.5, 0.05, 2.5], [0.0, -0.05, 0.0], 0),
                    wall([2.5, 0.05, 2.5], [0.0, 3.05, 0.0], 0),
                    wall([0.05, 1.5, 2.5], [-2.55, 1.5, 0.0], 1),
                    wall([0.05, 1.5, 2.5], [2.55, 1.5, 0.0], 2),
                    wall([2.5, 1.5, 0.05], [0.0, 1.5, -2.55], 0),
                    wall([0.5, 0.5, 0.5], [0.6, 0.5, -0.6], 0),
                ],
                ..base
            }
        }
    }
}

/// The test-only extra pass: a full-resolution target of its own and one render pass into it
/// every frame, recorded at `AfterPost` through the counted encoder (what a new default-path
/// effect would add).
#[derive(Default)]
struct ExtraPass {
    target: Option<(wgpu::Texture, wgpu::TextureView)>,
}

impl FramePass for ExtraPass {
    fn name(&self) -> &str {
        "ratchet_extra_pass"
    }

    fn slot(&self) -> GraphSlot {
        GraphSlot::AfterPost
    }

    fn record(&mut self, ctx: &mut FramePassContext<'_>) -> Result<(), String> {
        let (w, h) = ctx.color_size;
        let (_, view) = self.target.get_or_insert_with(|| {
            let texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("ratchet extra target"),
                size: wgpu::Extent3d {
                    width: w.max(1),
                    height: h.max(1),
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba16Float,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            (texture, view)
        });
        let pass = ctx.encoder.counted_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("ratchet extra pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        drop(pass);
        Ok(())
    }
}

fn target(device: &RenderDevice, width: u32, height: u32) -> (wgpu::Texture, wgpu::TextureView) {
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("ratchet output"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    (texture, view)
}

/// Draws `scene` at `preset` and `width × height`: the warm frames' work and the steady frames'.
fn measure(
    device: &RenderDevice,
    scene: Scene,
    preset: RenderProfilePreset,
    width: u32,
    height: u32,
    extra: bool,
    (warm_frames, steady_frames): (u64, u64),
) -> RatchetRow {
    let started = Instant::now();
    let lab = lab(scene, width, height);
    let quality = FrameQuality::from_profile(&RenderProfile::from_preset(preset));
    let frame = lab.as_frame_scene_with_quality(quality);
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let (_texture, view) = target(device, width, height);
    let mut renderer = FrameRenderer::new();
    renderer.set_encode_threads(1);
    if extra {
        renderer
            .register_pass(Box::new(ExtraPass::default()))
            .unwrap_or_else(|err| panic!("register the extra pass: {err}"));
    }
    let draw = |renderer: &mut FrameRenderer| {
        renderer
            .render_into(device, &frame, FrameTarget::full(&view, format, width, height))
            .unwrap_or_else(|err| panic!("{} at {}: {err}", scene.name(), preset.name()));
        let _ = device.device.poll(wgpu::PollType::wait_indefinitely());
    };
    let probe = FrameWorkProbe::start();
    for _ in 0..warm_frames {
        draw(&mut renderer);
    }
    let warm = probe.finish(warm_frames);
    let probe = FrameWorkProbe::start();
    for _ in 0..steady_frames {
        draw(&mut renderer);
    }
    let steady = probe.finish(steady_frames);
    drop(renderer);
    let _ = device.device.poll(wgpu::PollType::wait_indefinitely());
    println!(
        "{}/{}/{width}x{height}: {:.1} s",
        scene.name(),
        preset.name(),
        started.elapsed().as_secs_f64()
    );
    RatchetRow {
        scene: scene.name().to_owned(),
        preset: preset.name().to_owned(),
        width,
        height,
        warm,
        steady,
    }
}

fn print_row(row: &RatchetRow) {
    let short = |w: &FrameWork| {
        format!(
            "passes {}+{}c, dispatches {} ({} groups, {} indirect), draws {} ({} indirect, {} tris), \
             sets {}p/{}bg, tex {} B, buf {} B, upload {} B, heap {:?}",
            w.render_passes,
            w.compute_passes,
            w.dispatches,
            w.workgroups,
            w.indirect_dispatches,
            w.draws,
            w.indirect_draws,
            w.triangles,
            w.pipeline_sets,
            w.bind_group_sets,
            w.texture_bytes,
            w.buffer_bytes,
            w.upload_bytes,
            w.heap_allocations
        )
    };
    println!(
        "{}\n  warm   {}\n  steady {}",
        row.key(),
        short(&row.warm),
        short(&row.steady)
    );
}

/// Writes `text` to `path` atomically (tmp + rename).
fn write_atomically(path: &str, text: &str) {
    if let Some(dir) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(dir).unwrap_or_else(|err| panic!("create {}: {err}", dir.display()));
    }
    let tmp = format!("{path}.tmp-{}", std::process::id());
    std::fs::write(&tmp, text).unwrap_or_else(|err| panic!("write {tmp}: {err}"));
    std::fs::rename(&tmp, path).unwrap_or_else(|err| panic!("rename {tmp} to {path}: {err}"));
}

#[test]
fn default_frame_work_stays_under_the_committed_table() {
    let started = Instant::now();
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (install Mesa lavapipe): {err}"),
    };
    let info = device.adapter.get_info();
    let reference = info.device_type == wgpu::DeviceType::Cpu;
    let record = std::env::var_os(RECORD_ENV).is_some_and(|v| v != "0");

    let mut rows = Vec::new();
    for scene in Scene::ALL {
        for preset in RenderProfilePreset::ALL {
            let row = measure(&device, scene, preset, W, H, false, (WARM_FRAMES, STEADY_FRAMES));
            print_row(&row);
            assert!(
                row.steady.render_passes > 0,
                "{}: the frame counted no render pass",
                row.key()
            );
            assert!(
                row.steady.draws + row.steady.indirect_draws > 0,
                "{}: no draw counted",
                row.key()
            );
            assert_eq!(
                row.steady.pipelines_created,
                0,
                "{}: a steady frame creates no pipeline (the hitch gate)",
                row.key()
            );
            rows.push(row);
        }
    }

    // The VRAM rows: the forward box at each tier's resolution, with the tier's preset.
    let mut vram = Vec::new();
    for tier in BudgetTier::ALL {
        let (w, h) = ratchet::tier_resolution(tier);
        let row = measure(
            &device,
            Scene::ForwardBox,
            ratchet::tier_preset(tier),
            w,
            h,
            false,
            (VRAM_WARM_FRAMES, VRAM_STEADY_FRAMES),
        );
        let counted = VramRow::new(tier, row.warm.tracked_bytes());
        println!(
            "vram {}: {}x{} {} tracked {} B ({:.2} % of the {} B budget)",
            tier.name(),
            w,
            h,
            row.preset,
            counted.tracked_bytes,
            counted.percent_of_budget(),
            counted.budget_bytes
        );
        assert!(
            counted.fits(),
            "{}: tracked bytes over the tier's VRAM budget",
            tier.name()
        );
        vram.push(counted);
        rows.push(row);
    }

    // The extra pass on the smallest preset must fail the ratchet.
    let extra = measure(
        &device,
        Scene::ForwardBox,
        RenderProfilePreset::MobileLow,
        W,
        H,
        true,
        (WARM_FRAMES, STEADY_FRAMES),
    );
    print_row(&extra);

    let measured = RatchetTable {
        format_version: TABLE_FORMAT_VERSION,
        adapter: info.name.clone(),
        rows,
    };
    if record {
        assert!(
            reference,
            "record the table on the CPU reference adapter (lavapipe), not {}",
            info.name
        );
        let text = measured
            .to_ron()
            .unwrap_or_else(|err| panic!("serialize the table: {err}"));
        write_atomically(TABLE_PATH, &text);
        println!("recorded {} rows to {TABLE_PATH}", measured.rows.len());
    }
    let text = std::fs::read_to_string(TABLE_PATH)
        .unwrap_or_else(|err| panic!("read {TABLE_PATH}: {err}; record it with {RECORD_COMMAND}"));
    let table = RatchetTable::parse(&text).unwrap_or_else(|err| panic!("{err}"));

    // Against this run's own row, so the check holds on any adapter: the extra pass adds one
    // render pass per frame and a full-resolution target; on `MobileLow` (13 passes a frame) the pass
    // count grows 7.7 %, inside the slack, and the target bytes fail the row.
    let baseline = RatchetTable {
        format_version: TABLE_FORMAT_VERSION,
        adapter: info.name.clone(),
        rows: vec![extra_baseline(&measured)],
    };
    let with = ratchet::compare(&baseline, std::slice::from_ref(&extra));
    assert!(
        !with.passes(),
        "a test-only extra pass with its own target must fail the mobile_low row: {with:?}"
    );
    assert!(
        with.over.iter().all(|m| m.row.starts_with(&extra.key())),
        "the failure names the row ({}): {}",
        extra.key(),
        with.failure_message()
    );
    assert!(with.failure_message().contains(RECORD_COMMAND));
    println!("extra pass check: {}", with.failure_message());

    println!(
        "frame work ratchet: {} rows on {} ({:?}) in {:.1} s",
        measured.rows.len(),
        info.name,
        info.device_type,
        started.elapsed().as_secs_f64()
    );
    if !reference {
        println!("not the CPU reference adapter: counts printed, compare skipped");
        return;
    }
    let comparison = ratchet::compare(&table, &measured.rows);
    for note in &comparison.under {
        println!(
            "note: {note} is more than {} % under its row; re-record to tighten",
            ratchet::UNDER_PERCENT
        );
    }
    assert!(comparison.passes(), "{}", comparison.failure_message());
    for row in &table.rows {
        assert!(
            measured.rows.iter().any(|m| m.key() == row.key()),
            "table row {} was not measured (a scene or preset was removed: re-record with {RECORD_COMMAND})",
            row.key()
        );
    }
}

/// The measured `forward_box/MobileLow` row (the extra-pass check's baseline).
fn extra_baseline(measured: &RatchetTable) -> RatchetRow {
    let key = format!(
        "{}/{}/{W}x{H}",
        Scene::ForwardBox.name(),
        RenderProfilePreset::MobileLow.name()
    );
    measured
        .rows
        .iter()
        .find(|row| row.key() == key)
        .cloned()
        .unwrap_or_else(|| panic!("{key} was measured"))
}

/// CPU encode time of the 10,000-object field at Desktop High (master plan § 5.1a "Render encode,
/// 10k draws"). Hand-run; prints the numbers and asserts nothing about time:
///
/// ```text
/// cargo test --release -p regia_render --test frame_work_ratchet -- --ignored --nocapture encode_bench
/// ```
///
/// On a card, name it (`REGIA_GPU_UUID=<uuid> REGIA_GPU_STRICT=1`) and drop the lavapipe variables.
#[test]
#[ignore = "hand-run bench: prints the 10k-draw encode time"]
fn encode_bench_10k_draws() {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU: {err}"),
    };
    let info = device.adapter.get_info();
    let (width, height) = (1280, 720);
    let lab = lab(Scene::InstancedField, width, height);
    let quality = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    let frame = lab.as_frame_scene_with_quality(quality);
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let (_texture, view) = target(&device, width, height);
    let mut renderer = FrameRenderer::new();
    let draw = |renderer: &mut FrameRenderer| {
        renderer
            .render_into(&device, &frame, FrameTarget::full(&view, format, width, height))
            .unwrap_or_else(|err| panic!("encode bench frame: {err}"));
        let _ = device.device.poll(wgpu::PollType::wait_indefinitely());
    };
    for _ in 0..WARM_FRAMES {
        draw(&mut renderer);
    }
    let probe = FrameWorkProbe::start();
    let mut encode = Vec::new();
    let mut total = Vec::new();
    const FRAMES: u64 = 30;
    for _ in 0..FRAMES {
        draw(&mut renderer);
        let cpu = renderer.cpu_timings();
        encode.push(cpu.encode_ms);
        total.push(cpu.total_ms);
    }
    let work = probe.finish(FRAMES);
    let pct = |v: &mut Vec<f32>, p: f32| {
        v.sort_by(f32::total_cmp);
        v[((v.len() - 1) as f32 * p).round() as usize]
    };
    println!(
        "encode bench on {} ({:?}), {width}x{height} Desktop High, {} objects, {} encode threads: \
         draws/frame {} (+{} indirect), encode ms p50 {:.3} p95 {:.3}, render_into ms p50 {:.3} p95 {:.3}",
        info.name,
        info.device_type,
        FIELD_OBJECTS,
        renderer.encode_threads(),
        work.draws / FRAMES,
        work.indirect_draws / FRAMES,
        pct(&mut encode, 0.5),
        pct(&mut encode, 0.95),
        pct(&mut total, 0.5),
        pct(&mut total, 0.95),
    );
}
