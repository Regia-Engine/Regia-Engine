// SPDX-License-Identifier: MPL-2.0
//! Cluster residency on a device (ADR-3118, band streaming ADR-3708): the settled frame against
//! the all-resident frame, and a warm streaming frame that creates nothing.

use super::*;
use crate::visbuffer::{BandDelivery, BandDisk, BandDiskError, BandPayload, BandSections, ClusterResidency};

/// A `BandDisk` over RAM for the streamed fixtures: sections at once, band payloads answered in
/// request order through a queue drained by the next frames (no wall clock).
struct MemoryDisk {
    meshlets: Arc<[crate::visbuffer::Meshlet]>,
    sections: BandSections,
    queue: std::sync::Mutex<std::collections::VecDeque<BandDelivery>>,
}

impl MemoryDisk {
    fn new(mesh: &MeshletMesh) -> Self {
        Self {
            meshlets: Arc::from(mesh.meshlets.clone()),
            sections: BandSections {
                vertices: mesh.vertices.clone(),
                triangles: mesh.triangles.clone(),
            },
            queue: std::sync::Mutex::new(std::collections::VecDeque::new()),
        }
    }
}

impl BandDisk for MemoryDisk {
    fn read_sections(&self) -> Result<BandSections, BandDiskError> {
        Ok(BandSections {
            vertices: self.sections.vertices.clone(),
            triangles: self.sections.triangles.clone(),
        })
    }

    fn request_band(&self, tag: u64, clusters: &[u32]) {
        let mut lists = Vec::new();
        let mut tris = Vec::new();
        for &c in clusters {
            let m = &self.meshlets[c as usize];
            lists.extend_from_slice(
                &self.sections.vertices
                    [m.vertex_offset as usize..(m.vertex_offset + m.vertex_count) as usize],
            );
            tris.extend_from_slice(
                &self.sections.triangles
                    [m.triangle_offset as usize..(m.triangle_offset + m.triangle_count) as usize],
            );
        }
        if let Ok(mut queue) = self.queue.lock() {
            queue.push_back(BandDelivery {
                tag,
                result: Ok(BandPayload { lists, tris }),
            });
        }
    }

    fn try_deliver(&self) -> Option<BandDelivery> {
        self.queue.lock().ok()?.pop_front()
    }
}

/// The cooked rock with its lists and triangles on a memory disk (stream-shaped, ADR-3708).
fn stream_rock(mesh: &Arc<MeshletMesh>) -> Arc<MeshletMesh> {
    let head = MeshletMesh::parse_head(&mesh.to_bytes()).expect("head");
    Arc::new(MeshletMesh::from_head(head, Arc::new(MemoryDisk::new(mesh))))
}

/// The close and residency tests' cooked rock: the cluster DAG (146,776 bytes) that the x86_64 Linux cook
/// writes for the platform-`sin`/`cos` 96 × 48 sphere, checked in so every host draws the same DAG
/// (ADR-7878). The close tests' frame-0 floors hold for this DAG; the deterministic sphere's own cook
/// (146,296 bytes, the same on every host since ADR-7878) misses two of them on lavapipe (ADR-7711), and
/// the floors stay as they are. `the_checked_in_rock_is_the_x86_64_cook` ties the file to the cooker.
const ROCK_COOK: &[u8] = include_bytes!("../../../tests/fixtures/visbuffer-close-rock.meshlet");

/// A dense rock (a 96 × 48 UV sphere) and its cluster DAG with attributes. The sphere comes from the
/// deterministic `sin`/`cos`, so every host renders the same input bits (the platform `sinf` moved the
/// M5's positions by an ulp, ADR-7711), and the DAG is the checked-in `ROCK_COOK`, so every host draws
/// the same clusters.
pub(super) fn dense_rock() -> ((Vec<LabVertex>, Vec<u32>), Arc<MeshletMesh>) {
    let (vertices, indices) = crate::uv_sphere_deterministic(1.0, 96, 48);
    let mesh = MeshletMesh::from_bytes(ROCK_COOK).expect("the checked-in rock parses");
    assert_eq!(mesh.source_vertex_count as usize, vertices.len());
    assert_eq!(mesh.source_index_count as usize, indices.len());
    assert!(mesh.level_count() >= 3, "levels {}", mesh.level_count());
    ((vertices, indices), Arc::new(mesh))
}

/// The cooked rock of `sphere` with normals and UVs, at the cooker's default settings.
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
fn cook_rock(sphere: &(Vec<LabVertex>, Vec<u32>)) -> Vec<u8> {
    let (vertices, indices) = sphere;
    let positions: Vec<[f32; 3]> = vertices.iter().map(|v| v.position).collect();
    let normals: Vec<[f32; 3]> = vertices.iter().map(|v| v.normal).collect();
    let uvs: Vec<[f32; 2]> = vertices.iter().map(|v| v.uv).collect();
    regia_asset_pipeline::build_meshlet_mesh_with_attributes(
        &positions,
        Some(regia_asset_pipeline::MeshletAttributes {
            normals: &normals,
            uvs: &uvs,
        }),
        indices,
        &regia_asset_pipeline::MeshletSettings::default(),
    )
    .expect("cook")
    .to_bytes()
}

/// `ROCK_COOK` is what the cooker writes today for the platform sphere on x86_64 Linux (glibc
/// `sinf`/`cosf`), byte for byte, so the fixture never drifts from the cooker unnoticed: a cooker change
/// fails here first, and the test owner then regenerates the fixture together with the close tests.
/// Other hosts' platform sphere differs (ADR-7711), so the check runs on x86_64 Linux only.
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
#[test]
fn the_checked_in_rock_is_the_x86_64_cook() {
    let cooked = cook_rock(&crate::uv_sphere(1.0, 96, 48));
    assert_eq!(cooked.len(), ROCK_COOK.len());
    assert!(
        cooked == ROCK_COOK,
        "the cooker no longer writes the checked-in rock"
    );
}

/// The rock's input is the deterministic sphere, not the platform one, and the checked-in DAG indexes it.
#[test]
fn the_rock_input_is_the_deterministic_sphere() {
    let ((vertices, indices), mesh) = dense_rock();
    let (det, det_indices) = crate::uv_sphere_deterministic(1.0, 96, 48);
    assert_eq!(indices, det_indices);
    for (v, d) in vertices.iter().zip(&det) {
        assert_eq!(v.position.map(f32::to_bits), d.position.map(f32::to_bits));
        assert_eq!(v.normal.map(f32::to_bits), d.normal.map(f32::to_bits));
    }
    assert!(mesh.vertices.iter().all(|&i| (i as usize) < vertices.len()));
}

/// The fixture's scene with the rocks swapped for the dense rock (key 2) at three distances, a
/// second copy under key 3 far behind the camera at `z = 297`, and the temporal features off (so
/// a frame depends on its own geometry only).
fn rock_scene<'a>(
    f: &'a Fixture,
    post: &'a PostSettings,
    rock: &'a (Vec<LabVertex>, Vec<u32>),
    eye: [f32; 3],
) -> FrameScene<'a> {
    let mut s = scene(f, post, true, false);
    s.meshes.truncate(1);
    for (i, (key, x, z, scale)) in [
        (2, 0.0, -1.0, 0.8),
        (2, 1.6, -5.0, 0.8),
        (2, -2.5, -14.0, 1.2),
        (3, 0.0, 297.0, 0.8),
    ]
    .into_iter()
    .enumerate()
    {
        s.meshes.push(FrameMesh {
            is_static: false,
            mesh_key: key,
            material_key: 1002,
            object_key: 100 + i as u64,
            vertices: &rock.0,
            indices: &rock.1,
            material: &f.rock,
            model: translate(x, scale, z, scale),
            joints: &[],
            receiver_layer: 0,
            shadow: ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        });
    }
    s.eye = eye;
    s.target = [0.0, 0.5, -6.0];
    s.quality.screen_space_gi = false;
    s.quality.gi_probes = false;
    s.quality.ssr = false;
    s.quality.taau = false;
    s.quality.volumetric_fog = false;
    s.quality.volumetric_clouds = false;
    s.quality.ssao = crate::SsaoMode::Off;
    s
}

/// With room in the budget, the frame a residency renderer settles on is the all-resident
/// renderer's frame byte for byte at the same frame number: the demand is conservative, so every
/// band the cut selects is resident, and the clamped cut is the full cut. The first frame (base
/// only) differs, so the comparison is not vacuous.
#[test]
fn the_settled_frame_equals_the_all_resident_frame() {
    let device = crate::test_device();
    let f = fixture(1);
    let settings = post();
    let (rock, meshlets) = dense_rock();
    let eye = [0.3, 1.2, 2.5];
    let mut whole = comparison_renderer(&device);
    whole.insert_meshlets(2, Arc::clone(&meshlets));
    whole.insert_meshlets(3, Arc::clone(&meshlets));
    let mut managed = comparison_renderer(&device);
    managed.insert_meshlets(2, Arc::clone(&meshlets));
    managed.insert_meshlets(3, Arc::clone(&meshlets));
    managed.set_visbuffer_settings(VisBufferSettings {
        residency: Some(ClusterResidency {
            enabled: true,
            budget_bytes: 64 << 20,
            upload_bytes_per_frame: 256 << 10,
            ram_budget_bytes: u64::MAX / 4,
            io_bytes_per_frame: u64::MAX / 4,
        }),
        ..VisBufferSettings::default()
    });
    let s = rock_scene(&f, &settings, &rock, eye);
    const FRAMES: usize = 24;
    let (mut first_whole, mut first_managed) = (Vec::new(), Vec::new());
    let (mut last_whole, mut last_managed) = (Vec::new(), Vec::new());
    for frame in 0..FRAMES {
        let a = render(&mut whole, &device, &s);
        let b = render(&mut managed, &device, &s);
        if frame == 0 {
            first_whole = a.clone();
            first_managed = b.clone();
        }
        last_whole = a;
        last_managed = b;
    }
    let r = managed.visbuffer_arena_stats().residency;
    assert_eq!(r.meshes, 2, "{r:?}");
    assert_eq!(r.pending_bands, 0, "settled: {r:?}");
    assert!(r.refinements > 0, "bands loaded: {r:?}");
    assert_eq!(
        whole.visbuffer_arena_stats().residency.meshes,
        0,
        "off manages nothing"
    );
    let (worst_first, _) = diff(&first_whole, &first_managed);
    assert!(worst_first > 0, "the base-only frame is coarser");
    let (worst, close) = diff(&last_whole, &last_managed);
    assert_eq!(
        (worst, close),
        (0, 1.0),
        "settled frame differs from the all-resident frame"
    );
}

/// The same settled-frame contract with the bands streamed from a disk (ADR-3708): the
/// stream-shaped rock reads its cluster lists and triangles through a memory `BandDisk`, and the
/// frame it settles on is the all-resident renderer's frame byte for byte. The way there counts
/// coarse fallbacks (a band loads or waits) and reads from the disk, and no bound is overrun.
#[test]
fn the_streamed_settled_frame_equals_the_all_resident_frame() {
    let device = crate::test_device();
    let f = fixture(1);
    let settings = post();
    let (rock, meshlets) = dense_rock();
    let eye = [0.3, 1.2, 2.5];
    let mut whole = comparison_renderer(&device);
    whole.insert_meshlets(2, Arc::clone(&meshlets));
    whole.insert_meshlets(3, Arc::clone(&meshlets));
    let stream = stream_rock(&meshlets);
    let mut streamed = comparison_renderer(&device);
    streamed.insert_meshlets(2, Arc::clone(&stream));
    streamed.insert_meshlets(3, Arc::clone(&stream));
    streamed.set_visbuffer_settings(VisBufferSettings {
        residency: Some(ClusterResidency {
            enabled: true,
            budget_bytes: 64 << 20,
            upload_bytes_per_frame: 256 << 10,
            ram_budget_bytes: u64::MAX / 4,
            io_bytes_per_frame: u64::MAX / 4,
        }),
        ..VisBufferSettings::default()
    });
    let s = rock_scene(&f, &settings, &rock, eye);
    const FRAMES: usize = 24;
    let (mut last_whole, mut last_streamed) = (Vec::new(), Vec::new());
    let mut max_fallbacks = 0u64;
    let mut io_total = 0u64;
    for _ in 0..FRAMES {
        last_whole = render(&mut whole, &device, &s);
        last_streamed = render(&mut streamed, &device, &s);
        let r = streamed.visbuffer_arena_stats().residency;
        max_fallbacks = max_fallbacks.max(r.fallbacks);
        io_total = r.io_bytes_total;
        assert_eq!(r.bound_overruns, 0, "frame within the upload bound: {r:?}");
    }
    let r = streamed.visbuffer_arena_stats().residency;
    assert_eq!(r.meshes, 2, "{r:?}");
    assert_eq!(r.pending_bands, 0, "settled: {r:?}");
    assert!(r.refinements > 0, "bands streamed in: {r:?}");
    assert!(io_total > 0, "bytes came over the disk: {r:?}");
    assert!(max_fallbacks > 0, "coarse levels drew while bands loaded: {r:?}");
    let (worst, close) = diff(&last_whole, &last_streamed);
    assert_eq!(
        (worst, close),
        (0, 1.0),
        "the streamed settled frame differs from the all-resident frame"
    );
}

/// A tight budget and a camera that swings between the near rock and far away: residency keeps
/// evicting and loading bands, and once the arena has grown, no frame creates a pipeline, bind
/// group, buffer or texture through the path, while the uploads go on within the bound.
#[test]
fn warm_streaming_frames_create_nothing() {
    let device = crate::test_device();
    let f = fixture(1);
    let settings = post();
    let (rock, meshlets) = dense_rock();
    let mut probe = comparison_renderer(&device);
    probe.insert_meshlets(2, Arc::clone(&meshlets));
    probe.insert_meshlets(3, Arc::clone(&meshlets));
    probe.set_visbuffer_settings(VisBufferSettings {
        residency: Some(ClusterResidency {
            enabled: true,
            budget_bytes: u64::MAX / 4,
            upload_bytes_per_frame: u64::MAX / 4,
            ram_budget_bytes: u64::MAX / 4,
            io_bytes_per_frame: u64::MAX / 4,
        }),
        ..VisBufferSettings::default()
    });
    render(
        &mut probe,
        &device,
        &rock_scene(&f, &settings, &rock, [0.0, 1.0, 400.0]),
    );
    let base = probe.visbuffer_arena_stats().residency.resident_bytes;
    // What the near view settles on with room: the first rock's fine bands over both bases.
    let near_scene = rock_scene(&f, &settings, &rock, [0.3, 1.2, 2.5]);
    for _ in 0..8 {
        render(&mut probe, &device, &near_scene);
    }
    let settled = probe.visbuffer_arena_stats().residency;
    assert_eq!(settled.pending_bands, 0, "{settled:?}");
    let extra = settled.resident_bytes - base;
    assert!(extra > 0, "the near view loads finer bands: {settled:?}");
    let config = ClusterResidency {
        enabled: true,
        budget_bytes: base + extra * 3 / 2,
        upload_bytes_per_frame: 64 << 10,
        ram_budget_bytes: u64::MAX / 4,
        io_bytes_per_frame: u64::MAX / 4,
    };
    // Room for one rock's fine bands, not both: swinging between the rocks evicts the other's.
    let mut renderer = comparison_renderer(&device);
    renderer.insert_meshlets(2, Arc::clone(&meshlets));
    renderer.insert_meshlets(3, Arc::clone(&meshlets));
    renderer.set_visbuffer_settings(VisBufferSettings {
        residency: Some(config),
        ..VisBufferSettings::default()
    });
    let near = [0.3, 1.2, 2.5];
    // Next to the second rock (z = 297), the first 300 m away.
    let far = [0.3, 1.2, 300.0];
    let mut warm_frames = 0;
    let mut warm_uploads = 0;
    for cycle in 0..6 {
        for step in 0..16 {
            let eye = if step < 8 { near } else { far };
            render(&mut renderer, &device, &rock_scene(&f, &settings, &rock, eye));
            let r = renderer.visbuffer_arena_stats().residency;
            assert!(r.resident_bytes <= config.budget_bytes, "{r:?}");
            assert!(r.upload_bytes <= config.upload_bytes_per_frame, "{r:?}");
            if cycle >= 3 {
                let created = renderer.visbuffer_create_stats().last_frame;
                assert_eq!(
                    created.objects(),
                    0,
                    "cycle {cycle} step {step}: a warm streaming frame created {created:?}"
                );
                warm_frames += 1;
                warm_uploads += r.upload_bytes;
            }
        }
    }
    assert_eq!(warm_frames, 48);
    let r = renderer.visbuffer_arena_stats().residency;
    assert!(r.evictions > 0 && r.refinements > 0, "{r:?}");
    assert!(warm_uploads > 0, "warm frames still stream bands");
}

/// The frame hookup (ADR-3118 frame patch): a profile with residency on reaches the pass, and
/// with no runtime override the pass follows it.
#[test]
fn the_profile_turns_residency_on() {
    let device = crate::test_device();
    let f = fixture(1);
    let settings = post();
    let (rock, meshlets) = dense_rock();
    let mut renderer = comparison_renderer(&device);
    renderer.insert_meshlets(2, Arc::clone(&meshlets));
    renderer.insert_meshlets(3, Arc::clone(&meshlets));
    let on = ClusterResidency {
        enabled: true,
        budget_bytes: 64 << 20,
        upload_bytes_per_frame: 256 << 10,
        ..ClusterResidency::OFF
    };
    let mut s = rock_scene(&f, &settings, &rock, [0.3, 1.2, 2.5]);
    s.quality.cluster_residency = on;
    render(&mut renderer, &device, &s);
    let r = renderer.visbuffer_arena_stats().residency;
    assert_eq!(r.config, on, "{r:?}");
    assert_eq!(r.meshes, 2, "{r:?}");
}

/// The bench's scene: the fixture's ground and `rocks` dense rocks in two rows down −Z, each its
/// own mesh key (10, 11, …), the camera at `eye` looking down the row.
fn bench_scene<'a>(
    f: &'a Fixture,
    post: &'a PostSettings,
    rock: &'a (Vec<LabVertex>, Vec<u32>),
    eye: [f32; 3],
    rocks: u64,
) -> FrameScene<'a> {
    let mut s = rock_scene(f, post, rock, eye);
    s.meshes.truncate(1);
    for i in 0..rocks {
        s.meshes.push(FrameMesh {
            is_static: false,
            mesh_key: 10 + i,
            material_key: 1002,
            object_key: 200 + i,
            vertices: &rock.0,
            indices: &rock.1,
            material: &f.rock,
            model: translate(
                if i.is_multiple_of(2) { -1.5 } else { 1.5 },
                0.8,
                -4.0 * i as f32,
                0.8,
            ),
            joints: &[],
            receiver_layer: 0,
            shadow: ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        });
    }
    s.target = [eye[0], eye[1] - 0.3, eye[2] - 6.0];
    s
}

/// Card numbers for the report (ADR-3118; prints only, asserts nothing timed): a row of 12 dense
/// rocks, each its own mesh, flown down and back over 240 frames with residency off and at the
/// Desktop High and Deck budgets scaled to the scene. Prints the cluster arena's capacity and used
/// bytes (the VRAM the path holds for cluster data), the managed bytes against the whole-DAG
/// bytes, the upload per frame, and the frame times (mean, p99, max) and buffer creates while
/// streaming. Run on a card with
/// `cargo test -p regia_render --release --lib residency_card_bench -- --ignored --nocapture`.
#[test]
#[ignore = "bench: cargo test -p regia_render --release --lib residency_card_bench -- --ignored --nocapture"]
fn residency_card_bench() {
    let device = crate::test_device();
    let f = fixture(1);
    let settings = post();
    let (rock, meshlets) = dense_rock();
    let info = device.adapter.get_info();
    println!("adapter: {} ({:?})", info.name, info.backend);
    const ROCKS: u64 = 12;
    let path: Vec<[f32; 3]> = (0..240)
        .map(|frame| {
            let t = if frame < 120 {
                frame as f32 / 119.0
            } else {
                (239 - frame) as f32 / 119.0
            };
            [0.0, 1.2, 6.0 - 52.0 * t]
        })
        .collect();
    let run = |residency: Option<ClusterResidency>| {
        let mut renderer = comparison_renderer(&device);
        for i in 0..ROCKS {
            renderer.insert_meshlets(10 + i, Arc::clone(&meshlets));
        }
        renderer.set_visbuffer_settings(VisBufferSettings {
            residency: residency.or(Some(ClusterResidency::OFF)),
            ..VisBufferSettings::default()
        });
        let mut times = Vec::with_capacity(path.len());
        let mut creates = 0u64;
        let mut peak_capacity = 0u64;
        for (frame, &eye) in path.iter().enumerate() {
            let s = bench_scene(&f, &settings, &rock, eye, ROCKS);
            let start = std::time::Instant::now();
            render(&mut renderer, &device, &s);
            if frame >= 10 {
                times.push(start.elapsed().as_secs_f64() * 1000.0);
                creates += renderer.visbuffer_create_stats().last_frame.objects();
            }
            peak_capacity = peak_capacity.max(renderer.visbuffer_arena_stats().capacity_bytes);
        }
        times.sort_by(f64::total_cmp);
        let mean = times.iter().sum::<f64>() / times.len().max(1) as f64;
        let p99 = times.get(times.len() * 99 / 100).copied().unwrap_or(0.0);
        let max = times.last().copied().unwrap_or(0.0);
        let a = renderer.visbuffer_arena_stats();
        (a, peak_capacity, mean, p99, max, creates)
    };
    let (off, off_cap, mean, p99, max, creates) = run(None);
    println!(
        "off: arena used {} B, capacity peak {off_cap} B; frame ms mean {mean:.2} p99 {p99:.2} max {max:.2}; \
         creates after frame 10: {creates}",
        off.used_bytes
    );
    // The whole-DAG bytes of the scene, then budgets at 60 % and 30 % of them (a tier budget is far
    // larger than this scene; the ratio is what the run measures).
    let full = {
        let mut probe = comparison_renderer(&device);
        for i in 0..ROCKS {
            probe.insert_meshlets(10 + i, Arc::clone(&meshlets));
        }
        probe.set_visbuffer_settings(VisBufferSettings {
            residency: Some(ClusterResidency {
                enabled: true,
                budget_bytes: u64::MAX / 4,
                upload_bytes_per_frame: u64::MAX / 4,
                ram_budget_bytes: u64::MAX / 4,
                io_bytes_per_frame: u64::MAX / 4,
            }),
            ..VisBufferSettings::default()
        });
        render(
            &mut probe,
            &device,
            &bench_scene(&f, &settings, &rock, path[0], ROCKS),
        );
        probe.visbuffer_arena_stats().residency.full_bytes
    };
    for (name, share, upload) in [("60 %", 60, 16u64 << 20), ("30 %", 30, 4 << 20)] {
        let config = ClusterResidency {
            enabled: true,
            budget_bytes: full * share / 100,
            upload_bytes_per_frame: upload,
            ram_budget_bytes: u64::MAX / 4,
            io_bytes_per_frame: u64::MAX / 4,
        };
        let (a, cap, mean, p99, max, creates) = run(Some(config));
        let r = a.residency;
        println!(
            "budget {name} of {full} B, {upload} B/frame: arena used {} B, capacity peak {cap} B; managed peak {} B; \
             upload peak {} B/frame, total {} B; refinements {} evictions {} degraded {} overruns {}; frame ms \
             mean {mean:.2} p99 {p99:.2} max {max:.2}; creates after frame 10: {creates}",
            a.used_bytes,
            r.peak_resident_bytes,
            r.peak_upload_bytes,
            r.upload_bytes_total,
            r.refinements,
            r.evictions,
            r.degraded_meshes,
            r.bound_overruns,
        );
    }
}
