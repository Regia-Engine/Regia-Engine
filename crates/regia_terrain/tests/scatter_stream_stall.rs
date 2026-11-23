// SPDX-License-Identifier: MPL-2.0
//! The frame thread never waits for the async compute pool in `ScatterField::update`.
//!
//! On a 12-thread CPU the async pool has 3 threads (cpus / 4), shared with other background
//! work: Desktop High's DDGI brick bakes queue there at spawn. `update` used to `block_on` a
//! needed tile whose background placement was still queued behind that work, which froze
//! DO27's bore_twelve spawn for 0.2–0.3 s on an i7-8700K (frame 31 of the bench's spawn
//! segment). Here the pool has one thread and a task holds it for the whole walk, so every
//! prefetched tile stays queued; each update must place what it needs itself, in a few
//! milliseconds, and end with exactly the tiles a blocking field places.
//!
//! One test per binary: the pool sizes are process-wide and set once.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use regia_core::tasks::{AsyncComputePool, TaskPoolOptions};
use regia_terrain::{
    RockStyle, ScatterField, ScatterLibrary, ScatterSet, ScatterSource, ScatterView, TerrainMap,
    procedural_rock,
};

const SET: &str = r#"
ScatterSet(
    version: 2,
    seed: 9,
    classes: [
        ScatterClass(
            name: "rocks",
            meshes: [ScatterMeshRef(mesh: "rock", weight: 1.0, min_scale: 0.8, max_scale: 2.0)],
            cell_size: 4.0,
            density: 0.5,
            base_blend: 0.0,
            fade_start: 40.0,
            fade_end: 60.0,
        ),
    ],
)
"#;

/// How long the held pool thread is kept before a watchdog lets it go (an update that waits
/// for the pool takes about this long instead of failing by hanging).
const HOLD: Duration = Duration::from_secs(3);
/// Longest update allowed while the pool is held. Placing a needed tile here costs well under
/// a millisecond; the margin is for a loaded test machine, not for any wait on the pool.
const MAX_UPDATE: Duration = Duration::from_millis(40);

fn field(background: bool) -> ScatterField {
    let map = TerrainMap::from_heights(129, 129, vec![0.0; 129 * 129], 512.0, 1.0, 0.0).expect("flat map");
    let set = ScatterSet::from_ron(SET, None).expect("scatter set");
    let mut library = ScatterLibrary::new();
    library.insert(
        "rock".into(),
        procedural_rock(7, RockStyle::angular([0.4, 0.38, 0.35])),
    );
    let mut f = ScatterField::new(&set, ScatterSource::new(Arc::new(map)), &library, 0x5ca7).expect("field");
    f.set_background_streaming(background);
    f
}

/// Emitted instances by key and transform (LOD cross-fade state depends on the frames each
/// field has seen, so the dither is not compared).
fn emitted(f: &ScatterField, eye: [f32; 3]) -> Vec<(u64, [[f32; 4]; 4])> {
    let view = ScatterView::look_at(eye, [eye[0] + 50.0, 0.0, eye[2]], 0.9, 16.0 / 9.0, 0.1, 2000.0);
    let mut out = Vec::new();
    f.collect(&view, &mut out);
    let mut keyed: Vec<_> = out.iter().map(|m| (m.object_key, m.model)).collect();
    keyed.sort_by_key(|k| k.0);
    keyed
}

#[test]
fn update_never_waits_for_a_held_async_pool() {
    let sizes = regia_core::tasks::init(TaskPoolOptions {
        async_compute_threads: Some(1),
        ..TaskPoolOptions::default()
    });
    assert_eq!(sizes.async_compute, 1, "this binary must size the pools first");
    assert_eq!(AsyncComputePool::get().thread_num(), 1);

    // Hold the only async thread (a stand-in for DDGI brick bakes), released by the watchdog.
    let started = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let clog = {
        let (started, release) = (Arc::clone(&started), Arc::clone(&release));
        regia_core::tasks::spawn(async move {
            started.store(true, Ordering::SeqCst);
            while !release.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(1));
            }
        })
    };
    let watchdog = {
        let release = Arc::clone(&release);
        std::thread::spawn(move || {
            let until = Instant::now() + HOLD;
            while Instant::now() < until && !release.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
            release.store(true, Ordering::SeqCst);
        })
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while !started.load(Ordering::SeqCst) {
        assert!(
            Instant::now() < deadline,
            "the async pool never ran the holding task"
        );
        std::thread::sleep(Duration::from_millis(1));
    }

    let mut f = field(true);
    // Places everything on the caller: every frame must emit exactly what it emits.
    let mut reference = field(false);
    // Spawn: the first update places what the camera needs and queues the next ring behind
    // the held thread.
    let first = f.update([0.0, 3.0, 0.0]);
    reference.update([0.0, 3.0, 0.0]);
    assert!(first.queued_tiles > 0, "{first:?}");
    // Walk 2 tile sides along +x: the queued ring becomes needed while still queued.
    let mut eye = [0.0, 3.0, 0.0];
    let (mut replaced, mut waited, mut slowest) = (0, 0, Duration::ZERO);
    for _ in 0..16 {
        eye[0] += 8.0;
        let t = Instant::now();
        let stats = f.update(eye);
        let took = t.elapsed();
        assert!(
            !release.load(Ordering::SeqCst),
            "the watchdog released the pool during the walk: an update waited for it ({took:?}, {stats:?})"
        );
        slowest = slowest.max(took);
        replaced += stats.replaced_tiles;
        waited += stats.waited_tiles;
        reference.update(eye);
        assert_eq!(
            f.resident_per_class(),
            reference.resident_per_class(),
            "at {eye:?}"
        );
        assert!(
            emitted(&f, eye) == emitted(&reference, eye),
            "different instances at {eye:?}"
        );
    }
    release.store(true, Ordering::SeqCst);
    watchdog.join().expect("watchdog");
    regia_core::tasks::block_on(clog);
    println!("walk: {replaced} queued tiles placed on the caller, slowest update {slowest:?}");
    assert!(
        replaced > 0,
        "the walk never needed a queued tile, so it tests nothing"
    );
    assert_eq!(
        waited, 0,
        "no placement can have finished while the pool was held"
    );
    assert!(
        slowest < MAX_UPDATE,
        "an update took {slowest:?} with the async pool held (limit {MAX_UPDATE:?})"
    );

    // With the pool free again the finished placements are taken in; still the same output.
    f.update(eye);
    reference.update(eye);
    assert_eq!(f.resident_per_class(), reference.resident_per_class());
    assert!(
        emitted(&f, eye) == emitted(&reference, eye),
        "different instances after the release"
    );
}
