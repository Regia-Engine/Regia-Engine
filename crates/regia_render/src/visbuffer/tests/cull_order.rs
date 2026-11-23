// SPDX-License-Identifier: MPL-2.0
//! The cull's lists are in (instance, cluster) order on every run (ADR-2294): no slot comes from
//! an atomic counter, so the raster's depth ties resolve the same each time.

use super::super::gpu::CullLists;
use super::*;

/// `state` words of `vis_cull.wgsl` the checks read.
const DS_ARGS: usize = 16;
const DROPPED: usize = 12;
const WORK: usize = 11;
const SOFT_BASE: usize = 30;
const SOFT_END: usize = 31;
/// The software clusters of single-sided instances each phase kept: they fill its part of the
/// software range first, the double-sided instances' follow (ADR-2689).
const SOFT_SINGLE_KEPT: usize = 42;

/// The third frame of `s` through a fresh renderer, and its cull output.
fn lists_of(device: &crate::RenderDevice, s: &FrameScene<'_>, settings: VisBufferSettings) -> CullLists {
    let mut renderer = comparison_renderer(device);
    renderer.set_meshlet_builder(Some(single_level_builder()));
    renderer.set_visbuffer_settings(settings);
    for _ in 0..3 {
        render(&mut renderer, device, s);
    }
    renderer
        .visbuffer_pass_mut()
        .read_cull_lists(device)
        .expect("readback")
        .expect("a visibility frame ran")
}

fn entry(lists: &CullLists, slot: usize) -> (u32, u32) {
    (lists.clusters[2 * slot], lists.clusters[2 * slot + 1])
}

/// Every list holds strictly increasing (instance, meshlet) entries: the order the scans give.
/// Returns the entries listed (hardware and software, both phases).
fn assert_ordered(name: &str, lists: &CullLists) -> usize {
    let cap = lists.capacity as usize;
    let word = |i: usize| lists.state[i] as usize;
    let increasing = |what: &str, entries: Vec<(u32, u32)>| {
        for pair in entries.windows(2) {
            assert!(pair[0] < pair[1], "{name}: {what} out of order: {pair:?}");
        }
        entries.len()
    };
    let mut listed = 0;
    for phase in 0..2 {
        let single = word(phase * 4 + 1);
        let double = word(DS_ARGS + phase * 4 + 1);
        assert!(
            single + double <= cap,
            "{name}: phase {phase} lists {single} + {double} > {cap}"
        );
        listed += increasing(
            "back-face-culled list",
            (0..single).map(|i| entry(lists, phase * cap + i)).collect(),
        );
        listed += increasing(
            "double-sided list",
            (0..double)
                .map(|d| entry(lists, phase * cap + cap - 1 - d))
                .collect(),
        );
    }
    let (mut base, end) = (word(SOFT_BASE), word(SOFT_END));
    let (kept1, kept2) = (word(SOFT_SINGLE_KEPT), word(SOFT_SINGLE_KEPT + 1));
    // The last round's range starts at 0 when phase 2 did not run: it is then all phase 1's.
    if base == 0 && kept2 == 0 {
        base = end;
    }
    let (split1, split2) = (kept1, base + kept2);
    assert!(
        split1 <= base && split2 <= end,
        "{name}: the software range's split ({split1}, {split2}) is past its phases ({base}, {end})"
    );
    let software = |what: &str, range: std::ops::Range<usize>| {
        increasing(what, range.map(|i| entry(lists, 2 * cap + i)).collect())
    };
    listed += software("phase-1 software range, single-sided", 0..split1);
    listed += software("phase-1 software range, double-sided", split1..base);
    listed += software("phase-2 software range, single-sided", base..split2);
    listed += software("phase-2 software range, double-sided", split2..end);
    listed
}

/// The entries of a frame's lists, by what draws them: (back-face-culled, double-sided, software).
fn listed_by_route(lists: &CullLists) -> [std::collections::BTreeSet<(u32, u32)>; 3] {
    let cap = lists.capacity as usize;
    let word = |i: usize| lists.state[i] as usize;
    let mut routes: [std::collections::BTreeSet<(u32, u32)>; 3] = Default::default();
    for phase in 0..2 {
        routes[0].extend((0..word(phase * 4 + 1)).map(|i| entry(lists, phase * cap + i)));
        routes[1].extend((0..word(DS_ARGS + phase * 4 + 1)).map(|d| entry(lists, phase * cap + cap - 1 - d)));
    }
    routes[2].extend((0..word(SOFT_END)).map(|i| entry(lists, 2 * cap + i)));
    routes
}

/// Two fresh renderers list the same clusters in the same slots and the lists are in order, so the
/// raster draws them in the same order every run.
fn assert_deterministic(
    name: &str,
    device: &crate::RenderDevice,
    s: &FrameScene<'_>,
    settings: VisBufferSettings,
) {
    let first = lists_of(device, s, settings);
    let second = lists_of(device, s, settings);
    let listed = assert_ordered(name, &first);
    eprintln!(
        "{name}: {listed} clusters listed, {} work items, {} dropped, capacity {}",
        first.state[WORK], first.state[DROPPED], first.capacity
    );
    assert!(listed > 0, "{name}: nothing listed");
    assert_eq!(
        first.state, second.state,
        "{name}: the cull's counters differ between runs"
    );
    let differ = first
        .clusters
        .iter()
        .zip(&second.clusters)
        .filter(|(a, b)| a != b)
        .count();
    assert!(
        first.clusters == second.clusters,
        "{name}: {differ} cluster-list words differ between runs"
    );
}

/// The rock grid with and without the occluding wall (phase 2 then lists what the wall hid
/// last frame), with the software raster and at a capacity it overflows (ADR-1887 (8)): the
/// software range fills, the rest join the hardware list, in order.
#[test]
fn the_cull_lists_the_same_clusters_in_the_same_order_every_run() {
    let device = crate::test_device();
    let f = fixture(10);
    let settings = post();
    for wall in [false, true] {
        let s = scene(&f, &settings, true, wall);
        assert_deterministic(&format!("wall {wall}"), &device, &s, VisBufferSettings::default());
    }
    let f = fixture(16);
    let mut s = scene(&f, &settings, true, false);
    s.eye = [0.0, 6.0, 24.0];
    let software = VisBufferSettings {
        software_raster: true,
        ..VisBufferSettings::default()
    };
    assert_deterministic("software", &device, &s, software);
    let overflow = VisBufferSettings {
        max_clusters: 1024,
        ..software
    };
    let lists = lists_of(&device, &s, overflow);
    assert_eq!(lists.state[SOFT_END], 1024, "the software range fills");
    assert!(
        lists.state[1] + lists.state[5] >= 64,
        "the overflow draws in hardware"
    );
    assert_deterministic("software overflow", &device, &s, overflow);
}

/// 110 592 single-cluster boxes, every other one double-sided, all in view: 100k+ clusters, and
/// more instances and work items than one round of the top-level scan holds (65 536), so the
/// carry between rounds is in the lists. At a capacity of 1024 the lists drop the rest, the
/// back-face-culled list first in line, and drop the same clusters every run.
#[test]
fn the_cull_order_holds_over_a_hundred_thousand_clusters() {
    let device = crate::test_device();
    let side = 332usize;
    let cube = crate::box_mesh([0.004, 0.004, 0.004]);
    let one_sided = LabMaterial::default();
    let two_sided = LabMaterial {
        double_sided: true,
        ..LabMaterial::default()
    };
    let settings = post();
    let base = fixture(1);
    let mut s = scene(&base, &settings, true, false);
    s.directional_shadows = false;
    s.eye = [0.0, 14.0, 0.01];
    s.target = [0.0, 0.0, 0.0];
    s.meshes = (0..side * side)
        .map(|i| {
            let (x, z) = ((i % side) as f32, (i / side) as f32);
            let spacing = 0.03;
            let half = side as f32 * spacing * 0.5;
            let double = i % 2 == 1;
            FrameMesh {
                is_static: false,
                mesh_key: 1,
                material_key: 1001 + u64::from(double),
                object_key: 100 + i as u64,
                vertices: &cube.0,
                indices: &cube.1,
                material: if double { &two_sided } else { &one_sided },
                model: translate(x * spacing - half, 0.0, z * spacing - half, 1.0),
                joints: &[],
                receiver_layer: 0,
                shadow: ShadowCast::Off,
                uv1: &[],
                lod_dither: 0.0,
            }
        })
        .collect();
    let lists = lists_of(&device, &s, VisBufferSettings::default());
    let listed = assert_ordered("dense", &lists);
    assert!(
        lists.state[WORK] > 65_536,
        "work items {} must span two scan rounds",
        lists.state[WORK]
    );
    assert!(listed >= 100_000, "{listed} clusters listed");
    assert_deterministic("dense", &device, &s, VisBufferSettings::default());
    let small = VisBufferSettings {
        max_clusters: 1024,
        ..VisBufferSettings::default()
    };
    let lists = lists_of(&device, &s, small);
    assert!(lists.state[DROPPED] > 0, "the small lists drop clusters");
    assert_eq!(
        lists.state[1], 1024,
        "phase 1 keeps back-face-culled clusters first"
    );
    assert_deterministic("dense, capacity 1024", &device, &s, small);
}

/// A software cluster past the range's room draws in hardware with its instance's cull mode
/// (ADR-2689): a double-sided one in the cull-none list, or its back faces would vanish the frame
/// the range fills. 1600 single-cluster boxes at a capacity of 1024, at two mixes: more single-sided
/// clusters than room, and fewer. No cluster is lost or listed twice, and the lists are in order.
#[test]
fn software_overflow_keeps_each_clusters_cull_mode() {
    let device = crate::test_device();
    let side = 40usize;
    let cube = crate::box_mesh([0.004, 0.004, 0.004]);
    let one_sided = LabMaterial::default();
    let two_sided = LabMaterial {
        double_sided: true,
        ..LabMaterial::default()
    };
    let settings = post();
    let base = fixture(1);
    // Every `n`th box is double-sided: 800 single-sided for n = 2, 1200 for n = 4.
    for every in [2usize, 4] {
        let mut s = scene(&base, &settings, true, false);
        s.directional_shadows = false;
        s.eye = [0.0, 14.0, 0.01];
        s.target = [0.0, 0.0, 0.0];
        s.meshes = (0..side * side)
            .map(|i| {
                let (x, z) = ((i % side) as f32, (i / side) as f32);
                let spacing = 0.03;
                let half = side as f32 * spacing * 0.5;
                let double = (i + i / side).is_multiple_of(every);
                FrameMesh {
                    is_static: false,
                    mesh_key: 1,
                    material_key: 1001 + u64::from(double),
                    object_key: 100 + i as u64,
                    vertices: &cube.0,
                    indices: &cube.1,
                    material: if double { &two_sided } else { &one_sided },
                    model: translate(x * spacing - half, 0.0, z * spacing - half, 1.0),
                    joints: &[],
                    receiver_layer: 0,
                    shadow: ShadowCast::Off,
                    uv1: &[],
                    lod_dither: 0.0,
                }
            })
            .collect();
        // The hardware raster says which clusters are double-sided, whatever the draw order.
        let [hw_single, hw_double, hw_soft] =
            listed_by_route(&lists_of(&device, &s, VisBufferSettings::default()));
        assert!(hw_soft.is_empty());
        assert_eq!(hw_single.len() + hw_double.len(), side * side, "every {every}");
        let overflow = VisBufferSettings {
            software_raster: true,
            max_clusters: 1024,
            ..VisBufferSettings::default()
        };
        let lists = lists_of(&device, &s, overflow);
        assert_eq!(
            lists.state[SOFT_END], 1024,
            "every {every}: the software range fills"
        );
        assert_eq!(lists.state[DROPPED], 0, "every {every}: nothing drops");
        let [single, double, software] = listed_by_route(&lists);
        assert_eq!(
            single.len() + double.len(),
            side * side - 1024,
            "every {every}: the overflow draws in hardware"
        );
        assert!(
            single.is_subset(&hw_single),
            "every {every}: {} double-sided clusters in the back-face-culled list",
            single.difference(&hw_single).count()
        );
        assert!(
            double.is_subset(&hw_double),
            "every {every}: {} single-sided clusters in the double-sided list",
            double.difference(&hw_double).count()
        );
        let all: std::collections::BTreeSet<_> =
            single.union(&double).chain(software.iter()).copied().collect();
        let hw_all: std::collections::BTreeSet<_> = hw_single.union(&hw_double).copied().collect();
        assert_eq!(all, hw_all, "every {every}: a cluster was lost or listed twice");
        assert_deterministic(
            &format!("software overflow, every {every}"),
            &device,
            &s,
            overflow,
        );
    }
}

/// Words of one shadow job record (`vis_shadow.wgsl` layout, 12 words per job).
mod shadow_record {
    /// The single-sided clusters this job draws (its DrawIndirect count).
    pub(super) const SINGLE_COUNT: usize = 1;
    /// This job's first work item in the shared work list.
    pub(super) const WORK_START: usize = 4;
    /// This job's admitted work items.
    pub(super) const WORK_COUNT: usize = 5;
    /// This job's first single-sided cluster in the shared list.
    pub(super) const SINGLE_BASE: usize = 6;
    /// This job's first double-sided cluster, counted from the list's end.
    pub(super) const DOUBLE_BASE: usize = 7;
    /// The double-sided clusters this job draws.
    pub(super) const DOUBLE_COUNT: usize = 9;
    pub(super) const WORDS: usize = 12;
}

use shadow_record as sr;

/// The entries of a shadow job's single-sided cluster range: (instance, meshlet) pairs in list
/// order.
fn shadow_single_entries(lists: &super::super::gpu::ShadowLists, job: usize) -> Vec<(u32, u32)> {
    let r = job * sr::WORDS;
    let (base, count) = (
        lists.records[r + sr::SINGLE_BASE] as usize,
        lists.records[r + sr::SINGLE_COUNT] as usize,
    );
    (0..count)
        .map(|i| (lists.clusters[2 * (base + i)], lists.clusters[2 * (base + i) + 1]))
        .collect()
}

/// The frames of `s` through a fresh renderer with cluster shadows on, and the shadow cull
/// lists of the last frame that ran shadow jobs (a static scene's later frames can skip the
/// shadow passes entirely when their page caches hold).
fn shadow_lists_of(
    device: &crate::RenderDevice,
    s: &FrameScene<'_>,
    settings: VisBufferSettings,
) -> super::super::gpu::ShadowLists {
    let mut renderer = comparison_renderer(device);
    renderer.set_meshlet_builder(Some(single_level_builder()));
    renderer.set_visbuffer_settings(VisBufferSettings {
        cluster_shadows: true,
        ..settings
    });
    let mut lists = None;
    for _ in 0..3 {
        render(&mut renderer, device, s);
        if let Some(read) = renderer.visbuffer_shadow_lists(device).expect("readback")
            && !read.records.is_empty()
        {
            lists = Some(read);
        }
    }
    lists.expect("a shadow cull ran")
}

/// 1600 single-cluster shadow-casting boxes, more than a `max_clusters` of 1024 selects: past
/// the cap the shadow cull must drop a stable tail in (instance, cluster) order, so two
/// submissions keep the same set and each job keeps the prefix of its uncapped list
/// (the audit of the shadow cull's atomic admission; ADR-2294 kept these atomics on the
/// reasoning that a depth-only map cannot change a texel, which holds only while the kept set
/// is the full set).
#[test]
fn shadow_cull_keeps_a_stable_prefix_tail_at_capacity() {
    let device = crate::test_device();
    let side = 40usize;
    let cube = crate::box_mesh([0.004, 0.004, 0.004]);
    let material = LabMaterial::default();
    let settings = post();
    let base = fixture(1);
    let mut s = scene(&base, &settings, true, false);
    s.eye = [0.0, 14.0, 0.01];
    s.target = [0.0, 0.0, 0.0];
    s.meshes = (0..side * side)
        .map(|i| {
            let (x, z) = ((i % side) as f32, (i / side) as f32);
            let spacing = 0.03;
            let half = side as f32 * spacing * 0.5;
            FrameMesh {
                is_static: false,
                mesh_key: 1,
                material_key: 1001,
                object_key: 100 + i as u64,
                vertices: &cube.0,
                indices: &cube.1,
                material: &material,
                model: translate(x * spacing - half, 0.0, z * spacing - half, 1.0),
                joints: &[],
                receiver_layer: 0,
                shadow: ShadowCast::All,
                uv1: &[],
                lod_dither: 0.0,
            }
        })
        .collect();
    // Uncapped: every selected cluster is kept, so each job's single-sided range is the full
    // list the capped run must take a prefix of. The scene is single-sided only.
    let full = shadow_lists_of(&device, &s, VisBufferSettings::default());
    let full_total: usize = (0..full.records.len() / sr::WORDS)
        .map(|job| full.records[job * sr::WORDS + sr::SINGLE_COUNT] as usize)
        .sum();
    assert!(
        full_total > 1024,
        "the uncapped shadow cull selects {full_total} clusters, past the small capacity"
    );
    assert!(
        full.records
            .iter()
            .skip(sr::DOUBLE_COUNT)
            .step_by(sr::WORDS)
            .all(|&count| count == 0),
        "the scene is single-sided only"
    );
    for job in 0..full.records.len() / sr::WORDS {
        let entries = shadow_single_entries(&full, job);
        assert!(
            entries.windows(2).all(|pair| pair[0] < pair[1]),
            "full job {job}: the single-sided range is in (instance, cluster) order"
        );
        let (start, count) = (
            full.records[job * sr::WORDS + sr::WORK_START] as usize,
            full.records[job * sr::WORDS + sr::WORK_COUNT] as usize,
        );
        let work: Vec<(u32, u32)> = (0..count)
            .map(|i| (full.work[2 * (start + i)], full.work[2 * (start + i) + 1]))
            .collect();
        assert!(
            work.windows(2)
                .all(|pair| pair[0].0 < pair[1].0 || (pair[0].0 == pair[1].0 && pair[0].1 < pair[1].1)),
            "full job {job}: the work items are in (instance, cluster) order"
        );
    }
    // Capped, twice: the same submission must produce the same records, work list and cluster
    // list (with the atomic admission the kept set is whichever invocations win the scheduler).
    let capped_settings = VisBufferSettings {
        max_clusters: 1024,
        ..VisBufferSettings::default()
    };
    let first = shadow_lists_of(&device, &s, capped_settings);
    let second = shadow_lists_of(&device, &s, capped_settings);
    assert_eq!(first.capacity, 1024);
    assert_eq!(
        first.header, second.header,
        "the admitted and dropped counts differ"
    );
    assert_eq!(first.records, second.records, "the job records differ");
    assert_eq!(first.work, second.work, "the work lists differ");
    assert_eq!(
        first.clusters.len(),
        2 * 1024,
        "the capped cluster buffer holds one capacity"
    );
    assert_eq!(first.clusters, second.clusters, "the cluster lists differ");
    // Job by job in job order: the capped run keeps each job's prefix while room lasts, so the
    // dropped tail is stable in (instance, cluster) order.
    assert_eq!(
        first.records.len(),
        full.records.len(),
        "the capped run lists the same shadow jobs"
    );
    let mut room = 1024usize;
    let mut kept_total = 0usize;
    for job in 0..full.records.len() / sr::WORDS {
        let full_entries = shadow_single_entries(&full, job);
        let keep = full_entries.len().min(room);
        let got = shadow_single_entries(&first, job);
        assert_eq!(
            got,
            full_entries[..keep],
            "job {job}: the capped list is not the prefix of the full list"
        );
        let r = job * sr::WORDS;
        assert_eq!(
            first.records[r + sr::SINGLE_COUNT] as usize,
            keep,
            "job {job}: the kept count"
        );
        assert_eq!(
            first.records[r + sr::SINGLE_BASE] as usize,
            kept_total,
            "job {job}: the single-sided range starts where the previous job's ended"
        );
        room -= keep;
        kept_total += keep;
    }
    assert_eq!(kept_total, 1024, "the capacity is full");
    assert!(
        first.header[1] as usize == 1024 && first.header[2] as usize == full_total - 1024,
        "the header admits {} and drops {} of {full_total}",
        first.header[1],
        first.header[2]
    );
}

/// The entries of a shadow job's double-sided cluster range, in list order (the reversed draw
/// reads the range backwards from the list's end).
fn shadow_double_entries(lists: &super::super::gpu::ShadowLists, job: usize) -> Vec<(u32, u32)> {
    let r = job * sr::WORDS;
    let (base, count) = (
        lists.records[r + sr::DOUBLE_BASE] as usize,
        lists.records[r + sr::DOUBLE_COUNT] as usize,
    );
    let cap = lists.capacity as usize;
    (0..count)
        .map(|d| {
            let at = cap - 1 - (base + d);
            (lists.clusters[2 * at], lists.clusters[2 * at + 1])
        })
        .collect()
}

/// 800 single-sided and 800 double-sided single-cluster casters at a capacity of 1024: the
/// frame's cluster capacity fills at each job's turn — the job's single-sided clusters keep
/// the room left, its double-sided ones the room after that, each a stable (instance, cluster)-
/// order tail read back from the list's end — and two submissions agree on the records and the
/// entries of both ranges.
#[test]
fn shadow_cull_keeps_both_sides_in_order_at_capacity() {
    let device = crate::test_device();
    let side = 40usize;
    let cube = crate::box_mesh([0.004, 0.004, 0.004]);
    let one_sided = LabMaterial::default();
    let two_sided = LabMaterial {
        double_sided: true,
        ..LabMaterial::default()
    };
    let settings = post();
    let base = fixture(1);
    let mut s = scene(&base, &settings, true, false);
    s.eye = [0.0, 14.0, 0.01];
    s.target = [0.0, 0.0, 0.0];
    s.meshes = (0..side * side)
        .map(|i| {
            let (x, z) = ((i % side) as f32, (i / side) as f32);
            let spacing = 0.03;
            let half = side as f32 * spacing * 0.5;
            let double = i % 2 == 1;
            FrameMesh {
                is_static: false,
                mesh_key: 1,
                material_key: 1001 + u64::from(double),
                object_key: 100 + i as u64,
                vertices: &cube.0,
                indices: &cube.1,
                material: if double { &two_sided } else { &one_sided },
                model: translate(x * spacing - half, 0.0, z * spacing - half, 1.0),
                joints: &[],
                receiver_layer: 0,
                shadow: ShadowCast::All,
                uv1: &[],
                lod_dither: 0.0,
            }
        })
        .collect();
    let full = shadow_lists_of(&device, &s, VisBufferSettings::default());
    let jobs = full.records.len() / sr::WORDS;
    let full_total: usize = (0..jobs)
        .map(|job| shadow_single_entries(&full, job).len() + shadow_double_entries(&full, job).len())
        .sum();
    assert!(
        full_total > 1024,
        "the uncapped shadow cull selects {full_total} clusters, past the small capacity"
    );
    for job in 0..jobs {
        for (what, entries) in [
            ("single-sided", shadow_single_entries(&full, job)),
            ("double-sided", shadow_double_entries(&full, job)),
        ] {
            assert!(
                entries.windows(2).all(|pair| pair[0] < pair[1]),
                "full job {job}: the {what} range is in (instance, cluster) order"
            );
        }
    }
    let capped_settings = VisBufferSettings {
        max_clusters: 1024,
        ..VisBufferSettings::default()
    };
    let first = shadow_lists_of(&device, &s, capped_settings);
    let second = shadow_lists_of(&device, &s, capped_settings);
    assert_eq!(
        first.header, second.header,
        "the admitted and dropped counts differ"
    );
    assert_eq!(first.records, second.records, "the job records differ");
    assert_eq!(first.clusters, second.clusters, "the cluster lists differ");
    assert_eq!(first.work, second.work, "the work lists differ");
    let mut room = 1024usize;
    let mut kept_total = 0usize;
    let mut s_kept_total = 0usize;
    let mut d_kept_total = 0usize;
    for job in 0..jobs {
        let full_single = shadow_single_entries(&full, job);
        let full_double = shadow_double_entries(&full, job);
        let keep_single = full_single.len().min(room);
        room -= keep_single;
        let keep_double = full_double.len().min(room);
        room -= keep_double;
        assert_eq!(
            shadow_single_entries(&first, job),
            full_single[..keep_single],
            "job {job}: the capped single-sided list is not the prefix of the full list"
        );
        assert_eq!(
            shadow_double_entries(&first, job),
            full_double[..keep_double],
            "job {job}: the capped double-sided list is not the prefix of the full list"
        );
        let r = job * sr::WORDS;
        assert_eq!(first.records[r + sr::SINGLE_COUNT] as usize, keep_single);
        assert_eq!(first.records[r + sr::DOUBLE_COUNT] as usize, keep_double);
        assert_eq!(first.records[r + sr::SINGLE_BASE] as usize, s_kept_total);
        assert_eq!(first.records[r + sr::DOUBLE_BASE] as usize, d_kept_total);
        s_kept_total += keep_single;
        d_kept_total += keep_double;
        kept_total += keep_single + keep_double;
    }
    assert_eq!(kept_total, 1024, "the capacity is full");
    assert!(
        first.header[1] as usize <= 1024 && first.header[2] as usize == full_total - kept_total,
        "the header admits {} singles and drops {} of {full_total}",
        first.header[1],
        first.header[2]
    );
}
