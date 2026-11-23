// SPDX-License-Identifier: MPL-2.0
//! The probe bake's speed-ups (ADR-862) change no answer: the coherent ray order traces the
//! same rays and paths (only the order of the sums differs), geometry-only relocation traces
//! report the same distances and faces, per-probe streams make a probe's trace independent of
//! the probes traced with it, and a bake with every shortcut gives every valid probe the same
//! bits as a bake without them. Runs the real kernels on whatever adapter
//! `RenderDevice::headless` picks (lavapipe in CI).

use std::sync::OnceLock;

use regia_asset::baked_gi::{DetailBrick, ProbeFlags, VolumeLayout};
use regia_core::glam::Mat4;
use regia_pt::probe::{
    GiBakeSettings, ProbeTrace, ProbeTraceSettings, SKIP_INSIDE_MIN_RAYS, bake_gi_volume, detail_brick,
    skips_main_trace,
};
use regia_pt::scenes::{self, uv_sphere};
use regia_pt::{PathTracer, PtCamera, PtMaterial, PtMesh, PtScene, PtSky};
use regia_render::RenderDevice;

fn tracer() -> &'static PathTracer {
    static T: OnceLock<PathTracer> = OnceLock::new();
    T.get_or_init(|| {
        let device = RenderDevice::headless().expect("headless device");
        PathTracer::new(&device).expect("compile the path tracer")
    })
}

fn settings(rays: u32, samples: u32, bounces: u32) -> ProbeTraceSettings {
    ProbeTraceSettings {
        rays_per_probe: rays,
        samples,
        max_bounces: bounces,
        seed: 3,
        samples_per_submit: 2,
        ..ProbeTraceSettings::default()
    }
}

/// Probes in and around the Cornell box's blocks.
const CORNELL_PROBES: [[f32; 3]; 4] = [
    [0.278, 0.274, 0.28],
    [0.1, 0.02, 0.1],
    [0.45, 0.4, 0.45],
    [0.02, 0.5, 0.53],
];

fn close(a: f32, b: f32, scale: f32) -> bool {
    (a - b).abs() <= 1e-4 * (a.abs() + b.abs()) + 2e-5 * scale
}

/// Same rays, same paths: the counts agree exactly, the sums to the last bits.
fn assert_same_rays(a: &ProbeTrace, b: &ProbeTrace, what: &str) {
    assert_eq!(
        (a.rays, a.hits, a.back_faces, a.rejected),
        (b.rays, b.hits, b.back_faces, b.rejected),
        "{what}: counts"
    );
    let scale =
        a.sh.iter()
            .flatten()
            .map(|ch| ch[0].abs())
            .fold(1e-6f32, f32::max);
    for (c, (sa, sb)) in a.sh.iter().zip(&b.sh).enumerate() {
        for (va, vb) in sa.iter().flatten().zip(sb.iter().flatten()) {
            assert!(close(*va, *vb, scale), "{what}: component {c}: {va} vs {vb}");
        }
    }
    for (ma, mb) in a.moments.iter().zip(&b.moments) {
        assert!(
            close(ma[0], mb[0], 1.0) && close(ma[1], mb[1], 1.0),
            "{what}: moments {ma:?} vs {mb:?}"
        );
    }
    for (ca, cb) in [
        (a.closest_back, b.closest_back),
        (a.closest_front, b.closest_front),
    ] {
        match (ca, cb) {
            (Some((_, da)), Some((_, db))) => assert!(close(da, db, 1.0), "{what}: closest {da} vs {db}"),
            (None, None) => {}
            other => panic!("{what}: closest hit {other:?}"),
        }
    }
}

#[test]
fn the_coherent_order_traces_the_same_rays() {
    let scene = scenes::cornell_box(4, 4, true);
    for rays in [64u32, 100] {
        let s = settings(rays, 3, 3);
        let ordered = tracer()
            .trace_probes(&scene, &CORNELL_PROBES, &s, |_, _| true)
            .expect("coherent trace");
        let index = tracer()
            .trace_probes(
                &scene,
                &CORNELL_PROBES,
                &ProbeTraceSettings {
                    coherent_rays: false,
                    ..s
                },
                |_, _| true,
            )
            .expect("index-order trace");
        for (k, (a, b)) in ordered.iter().zip(&index).enumerate() {
            assert_eq!(a.rejected, 0);
            assert!(a.hits > 0, "probe {k} hits the box");
            assert_same_rays(a, b, &format!("{rays} rays, probe {k}"));
        }
    }
}

/// Compacted bounces run the same paths from a queue: every result is the same bit for bit,
/// whatever the batch split (one probe per batch, or all of them).
#[test]
fn path_compaction_changes_no_bit() {
    let scene = scenes::cornell_box(4, 4, true);
    // Russian roulette from the second vertex: many paths end early, so the queues shrink.
    let s = ProbeTraceSettings {
        rr_start: 1,
        ..settings(64, 3, 6)
    };
    let compacted = tracer()
        .trace_probes(&scene, &CORNELL_PROBES, &s, |_, _| true)
        .expect("compacted trace");
    for batch in [1u32, 4] {
        let plain = tracer()
            .trace_probes(
                &scene,
                &CORNELL_PROBES,
                &ProbeTraceSettings {
                    compact_paths: false,
                    probes_per_batch: batch,
                    ..s
                },
                |_, _| true,
            )
            .expect("plain trace");
        assert_eq!(compacted, plain, "probes per batch {batch}");
    }
    // Paths do go past the first bounce here (the comparison would be empty otherwise).
    let one_bounce = tracer()
        .trace_probes(
            &scene,
            &CORNELL_PROBES,
            &ProbeTraceSettings { max_bounces: 1, ..s },
            |_, _| true,
        )
        .expect("one bounce");
    assert_ne!(compacted[0].sh, one_bounce[0].sh);
}

#[test]
fn geometry_only_traces_give_relocation_the_same_answers() {
    let scene = scenes::cornell_box(4, 4, true);
    let quick = settings(64, 2, 1);
    let shaded = tracer()
        .trace_probes(&scene, &CORNELL_PROBES, &quick, |_, _| true)
        .expect("shaded trace");
    let geometry = tracer()
        .trace_probes(
            &scene,
            &CORNELL_PROBES,
            &ProbeTraceSettings {
                geometry_only: true,
                ..quick
            },
            |_, _| true,
        )
        .expect("geometry trace");
    for (k, (s, g)) in shaded.iter().zip(&geometry).enumerate() {
        assert_eq!(s.rejected, 0, "probe {k}");
        // Everything relocation and validity read is the same, bit for bit.
        assert_eq!(
            (s.rays, s.hits, s.back_faces),
            (g.rays, g.hits, g.back_faces),
            "probe {k}"
        );
        assert_eq!(s.closest_back, g.closest_back, "probe {k}");
        assert_eq!(s.closest_front, g.closest_front, "probe {k}");
        assert_eq!(s.moments, g.moments, "probe {k}");
        // And no light was gathered.
        assert!(g.sh.iter().flatten().flatten().all(|v| *v == 0.0), "probe {k}");
        assert!(s.sh.iter().flatten().flatten().any(|v| *v != 0.0), "probe {k}");
    }
}

#[test]
fn a_probe_traces_the_same_whatever_is_traced_with_it() {
    let scene = scenes::cornell_box(4, 4, true);
    let s = settings(64, 2, 3);
    let all = tracer()
        .trace_probes(&scene, &CORNELL_PROBES, &s, |_, _| true)
        .expect("all probes");
    let explicit = tracer()
        .trace_probe_streams(&scene, &CORNELL_PROBES, &[0, 1, 2, 3], &s, |_, _| true)
        .expect("explicit streams");
    assert_eq!(all, explicit, "trace_probes uses stream k for probe k");
    // Two of them, in another order and batch split, keep their streams.
    let some = tracer()
        .trace_probe_streams(
            &scene,
            &[CORNELL_PROBES[2], CORNELL_PROBES[0]],
            &[2, 0],
            &ProbeTraceSettings {
                probes_per_batch: 1,
                ..s
            },
            |_, _| true,
        )
        .expect("subset");
    assert_eq!(some[0], all[2]);
    assert_eq!(some[1], all[0]);
    // Another stream is another noise pattern.
    let moved = tracer()
        .trace_probe_streams(&scene, &[CORNELL_PROBES[0]], &[7], &s, |_, _| true)
        .expect("stream 7");
    assert_ne!(moved[0].sh, all[0].sh);
    // Bad streams are errors, not panics.
    assert!(
        tracer()
            .trace_probe_streams(&scene, &CORNELL_PROBES, &[0, 1], &s, |_, _| true)
            .is_err()
    );
    assert!(
        tracer()
            .trace_probe_streams(&scene, &[CORNELL_PROBES[0]], &[1 << 24], &s, |_, _| true)
            .is_err()
    );
}

/// A single-probe brick at `at` whose cell (relocation bound) is `cell` metres.
fn one_probe(at: [f32; 3], cell: f32) -> DetailBrick {
    let mut b = detail_brick(at, at, cell);
    b.counts = [1, 1, 1];
    b.spacing = [cell; 3];
    b
}

/// One probe at the centre of a 50 m ball (inside geometry: relocation pushes it to the corner
/// of its cell and it stays there), one half a metre outside the ball, one in the open.
fn ball_scene() -> (PtScene, VolumeLayout) {
    let mut scene = PtScene::new(PtCamera::pinhole([0.0, 0.0, 200.0], [0.0; 3], 0.5, 4, 4));
    scene.sky = PtSky::Uniform([1.0, 0.8, 0.6]);
    // Fine facets: the closest back face from inside points along the probe's offset.
    let ball = scene.add_mesh(uv_sphere(50.0, 256, 128, false));
    let grey = scene.add_material(PtMaterial::lambert([0.5; 3]));
    scene.add_instance(ball, grey, Mat4::IDENTITY);
    let mut layout = VolumeLayout {
        terrain: None,
        bricks: vec![
            one_probe([0.0, 0.0, 0.0], 0.5),
            one_probe([50.5, 0.0, 0.0], 0.5),
            one_probe([0.0, 120.0, 0.0], 0.5),
        ],
        moment_resolution: 4,
        max_distance: 8.0,
    };
    layout.assign_brick_offsets();
    (scene, layout)
}

#[test]
fn shortcuts_give_every_valid_probe_the_same_bits() {
    let (scene, layout) = ball_scene();
    let fast = GiBakeSettings {
        trace: ProbeTraceSettings {
            // The coherent order changes the last bits of the sums (the test above); leave it
            // out so the comparison can be exact.
            coherent_rays: false,
            ..settings(64, 2, 2)
        },
        relocation_iterations: 6,
        filter: false,
        ..GiBakeSettings::default()
    };
    assert!(fast.geometry_only_relocation && fast.skip_inside_fraction <= 1.0);
    let slow = fast.without_shortcuts();
    assert!(!slow.geometry_only_relocation && slow.skip_inside_fraction > 1.0);
    let a = bake_gi_volume(tracer(), &scene, &layout, &fast, |_, _, _| true).expect("fast bake");
    let b = bake_gi_volume(tracer(), &scene, &layout, &slow, |_, _, _| true).expect("slow bake");
    assert_eq!(a.positions, b.positions, "relocation decided the same");
    for (i, (pa, pb)) in a.raw.iter().zip(&b.raw).enumerate() {
        assert_eq!(pa.meta, pb.meta, "probe {i}");
        if pa.meta.is_valid() {
            assert_eq!(pa.sh, pb.sh, "probe {i}");
            assert_eq!(pa.moments, pb.moments, "probe {i}");
        }
    }
    assert_eq!(a.volume.meta_bytes(), b.volume.meta_bytes());
    // The centre probe is inside and was not traced again; the others are valid.
    let centre = a.raw[0].meta;
    assert!(
        !centre.is_valid() && centre.flags & ProbeFlags::INSIDE_GEOMETRY != 0,
        "{centre:?}"
    );
    assert!(a.raw[1].meta.is_valid() && a.raw[2].meta.is_valid());
    assert_eq!(a.report.inside_skipped, 1);
    assert_eq!(b.report.inside_skipped, 0);
    assert_eq!(a.report.inside_geometry, b.report.inside_geometry);
    assert_eq!(a.report.rays_traced, b.report.rays_traced - u64::from(64 * 2u32));
    // Relocation retraced only the probes that moved: never all three in every round. The
    // fast bake may add one classification trace for a probe the last round still moved.
    assert!(
        a.report.relocation_traces < 3 * 6,
        "{}",
        a.report.relocation_traces
    );
    let extra = a.report.relocation_traces - b.report.relocation_traces;
    assert!(extra <= 1, "{extra}");
    assert!(a.raw[0].sh.iter().flatten().flatten().all(|v| *v == 0.0));
}

/// One relocation round moves the centre probe, so no relocation trace saw its final position:
/// the classification trace there judges it, and the main trace skips it.
#[test]
fn a_probe_moved_by_the_last_round_is_classified_before_the_main_trace() {
    let (scene, layout) = ball_scene();
    let settings = GiBakeSettings {
        trace: settings(64, 2, 2),
        relocation_iterations: 1,
        filter: false,
        ..GiBakeSettings::default()
    };
    let bake = bake_gi_volume(tracer(), &scene, &layout, &settings, |_, _, _| true).expect("bake");
    let centre = bake.raw[0].meta;
    assert!(centre.flags & ProbeFlags::RELOCATED != 0, "{centre:?}");
    assert!(!centre.is_valid() && centre.flags & ProbeFlags::INSIDE_GEOMETRY != 0);
    assert_eq!(bake.report.inside_skipped, 1);
    // One round over three probes, then one classification trace for the moved one.
    assert_eq!(bake.report.relocation_traces, 3 + 1);
    // Without the skip there is nothing to classify.
    let plain = bake_gi_volume(
        tracer(),
        &scene,
        &layout,
        &GiBakeSettings {
            skip_inside_fraction: 2.0,
            ..settings
        },
        |_, _, _| true,
    )
    .expect("bake");
    assert_eq!(plain.report.relocation_traces, 3);
    assert_eq!(plain.report.inside_skipped, 0);
    assert_eq!(plain.raw[0].meta, centre);
}

#[test]
fn the_skip_rule_needs_rays_and_a_clear_margin() {
    let s = GiBakeSettings::default();
    let rays = SKIP_INSIDE_MIN_RAYS;
    assert!(skips_main_trace(Some((1.0, rays)), &s));
    assert!(skips_main_trace(Some((0.5, rays)), &s));
    assert!(
        !skips_main_trace(Some((0.49, rays)), &s),
        "below the skip fraction"
    );
    assert!(
        !skips_main_trace(Some((1.0, rays - 1)), &s),
        "too few rays to be sure"
    );
    assert!(!skips_main_trace(None, &s), "no trace at the final position");
    // Never at or below the invalid threshold, whatever the setting says.
    let loose = GiBakeSettings {
        skip_inside_fraction: 0.1,
        ..s
    };
    assert!(!skips_main_trace(Some((0.2, rays)), &loose));
    assert!(skips_main_trace(Some((0.3, rays)), &loose));
    assert!(!skips_main_trace(Some((1.0, rays)), &s.without_shortcuts()));
}

/// A bake with relocation and a main trace packs the scene once and uploads it once.
/// The second bake of the same scene does neither again (ADR-5154).
#[test]
fn a_default_bake_packs_and_uploads_the_scene_once() {
    // Its own tracer: the counters and the packed-scene cache are per tracer, and the shared
    // `tracer()` is baking the other tests' scenes on other threads at the same time (ADR-5517).
    let device = RenderDevice::headless().expect("headless device");
    let tracer = PathTracer::new(&device).expect("compile the path tracer");
    let own = &tracer;
    let mut scene = PtScene::new(PtCamera::pinhole([0.0, 2.0, 6.0], [0.0, 0.5, 0.0], 0.6, 4, 4));
    scene.sky = PtSky::Uniform([0.4, 0.5, 0.6]);
    let grey = scene.add_material(PtMaterial::lambert([0.5; 3]));
    let floor = scene.add_mesh(PtMesh {
        positions: vec![
            [-4.0, 0.0, -4.0],
            [4.0, 0.0, -4.0],
            [-4.0, 0.0, 4.0],
            [4.0, 0.0, 4.0],
        ],
        normals: vec![[0.0, 1.0, 0.0]; 4],
        indices: vec![0, 2, 1, 1, 2, 3],
        ..PtMesh::default()
    });
    scene.add_instance(floor, grey, Mat4::IDENTITY);
    let box_mesh = scene.add_mesh(PtMesh {
        positions: vec![
            [-0.5, 0.0, -0.5],
            [0.5, 0.0, -0.5],
            [0.5, 1.0, -0.5],
            [-0.5, 1.0, -0.5],
            [-0.5, 0.0, 0.5],
            [0.5, 0.0, 0.5],
            [0.5, 1.0, 0.5],
            [-0.5, 1.0, 0.5],
        ],
        normals: vec![
            [0.0, 0.0, -1.0],
            [0.0, 0.0, -1.0],
            [0.0, 0.0, -1.0],
            [0.0, 0.0, -1.0],
            [0.0, 0.0, 1.0],
            [0.0, 0.0, 1.0],
            [0.0, 0.0, 1.0],
            [0.0, 0.0, 1.0],
        ],
        indices: vec![0, 1, 2, 0, 2, 3, 4, 6, 5, 4, 7, 6],
        ..PtMesh::default()
    });
    scene.add_instance(box_mesh, grey, Mat4::IDENTITY);
    let mut layout = VolumeLayout {
        terrain: None,
        bricks: vec![detail_brick([0.2, 1.2, 0.2], [0.8, 1.8, 0.8], 1.0)],
        moment_resolution: 1,
        max_distance: 4.0,
    };
    layout.assign_brick_offsets();
    let settings = GiBakeSettings {
        trace: ProbeTraceSettings {
            rays_per_probe: 16,
            samples: 1,
            max_bounces: 2,
            probes_per_batch: 8,
            ..ProbeTraceSettings::default()
        },
        relocation_iterations: 2,
        relocation_samples: 1,
        filter: false,
        wall_test: false,
        ..GiBakeSettings::default()
    };
    let packs0 = own.probe_scene_packs();
    let uploads0 = own.probe_scene_uploads();
    let blas0 = own.probe_blas_built();
    let baked = bake_gi_volume(own, &scene, &layout, &settings, |_, _, _| true).expect("bake");
    assert!(baked.report.probes >= 1, "{:?}", baked.report);
    assert!(
        baked.report.relocation_traces >= 1,
        "relocation ran: {:?}",
        baked.report
    );
    let packs = own.probe_scene_packs() - packs0;
    let uploads = own.probe_scene_uploads() - uploads0;
    let blas = own.probe_blas_built() - blas0;
    assert_eq!(packs, 1, "packs {packs}");
    assert_eq!(uploads, 1, "uploads {uploads}");
    assert_eq!(blas, 2, "bottom-level BVHs built {blas}");
    let _again = bake_gi_volume(own, &scene, &layout, &settings, |_, _, _| true).expect("second bake");
    assert_eq!(own.probe_scene_packs() - packs0, 1, "second bake packed again");
    assert_eq!(
        own.probe_scene_uploads() - uploads0,
        1,
        "second bake uploaded again"
    );
    assert_eq!(own.probe_blas_built() - blas0, blas, "second bake rebuilt a BVH");
}
