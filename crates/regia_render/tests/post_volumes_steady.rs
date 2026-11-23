// SPDX-License-Identifier: MPL-2.0
//! Spec 30 claims that need a process of their own (lane spec-12-29-spacemouse, ADR-5631):
//! - § 3.5: once its scratch vector has grown, gathering and blending the volumes of a steady
//!   frame allocates nothing (a counting global allocator, read per thread);
//! - § 6: a volume from a newer engine is skipped with one warning per process;
//! - § 10.2: at most `MAX_LOADED_LUTS` distinct volume-LUT paths are kept, and a path beyond
//!   them is not loaded and warned about once.
//!
//! The binary installs a global allocator and a global log subscriber, so these tests live here
//! only. CPU only (no device), no wall clock.

#![allow(unsafe_code)] // the counting `GlobalAlloc` (ADR-5631), as in `lens_effects_alloc.rs`

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Once};

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_render::lut::ColorLut;
use regia_render::post_volume_luts::{MAX_LOADED_LUTS, load_post_volume_luts};
use regia_render::post_volumes::{
    POST_VOLUME_FORMAT_VERSION, PostOverrides, PostVolume, PostVolumeShape, blend_post_volumes,
    gather_contributions,
};
use regia_render::{PostSettings, PostVolumeLuts, TonemapOperator};
use regia_scene::{GlobalTransform, Transform};

struct Counting;

thread_local! {
    /// The calling thread's allocations (other test threads and the harness do not count).
    static THREAD_ALLOCS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn thread_allocs() -> u64 {
    THREAD_ALLOCS.with(|c| c.get())
}

// SAFETY: forwards to the system allocator; only counts.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        THREAD_ALLOCS.with(|c| c.set(c.get() + 1));
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        THREAD_ALLOCS.with(|c| c.set(c.get() + 1));
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

static NEWER_WARNINGS: AtomicUsize = AtomicUsize::new(0);
static LUT_CAP_WARNINGS: AtomicUsize = AtomicUsize::new(0);

/// Counts the two `regia_render` warnings these tests are about; ignores everything else.
struct Counter;

struct Message(String);

impl tracing::field::Visit for Message {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        }
    }
}

impl tracing::Subscriber for Counter {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        *metadata.level() == tracing::Level::WARN
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let meta = event.metadata();
        if *meta.level() != tracing::Level::WARN || meta.target() != "regia_render" {
            return;
        }
        let mut message = Message(String::new());
        event.record(&mut message);
        if message.0.contains("PostVolume has format_version") {
            NEWER_WARNINGS.fetch_add(1, Ordering::Relaxed);
        }
        if message.0.contains("post volumes name more than") {
            LUT_CAP_WARNINGS.fetch_add(1, Ordering::Relaxed);
        }
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

fn install_counter() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        tracing::subscriber::set_global_default(Counter).expect("the only subscriber of this binary");
    });
}

fn at(translation: [f32; 3]) -> GlobalTransform {
    GlobalTransform(Transform::from_translation(Vec3::from_array(translation)).compute_affine())
}

fn entity(index: u32) -> Entity {
    Entity::from_raw_u32(index).expect("entity index")
}

/// § 3.5: a camera walking through overlapping volumes (inside, in the bands, outside) allocates
/// nothing per frame once the scratch vector has grown.
#[test]
fn a_steady_frame_gathers_and_blends_without_allocating() {
    let warm = |saturation: f32, priority: i32, shape: PostVolumeShape| PostVolume {
        shape,
        priority,
        blend_distance: 2.0,
        overrides: PostOverrides {
            saturation: Some(saturation),
            white_balance_kelvin: Some(3600.0),
            tonemap: Some(TonemapOperator::default()),
            local_detail_strength: Some(1.5),
            fog_tint: Some([1.0, 0.8, 0.7]),
            ..PostOverrides::default()
        },
        ..PostVolume::default()
    };
    let volumes = [
        (
            entity(1),
            warm(
                1.2,
                0,
                PostVolumeShape::Box {
                    half_extents: [4.0, 3.0, 4.0],
                },
            ),
            at([0.0, 0.0, 0.0]),
        ),
        (
            entity(2),
            warm(0.8, 0, PostVolumeShape::Sphere { radius: 3.0 }),
            at([3.0, 0.0, 0.0]),
        ),
        (
            entity(3),
            warm(
                1.5,
                2,
                PostVolumeShape::ConvexHull {
                    planes: vec![[0.0, 1.0, 0.0, 2.0], [1.0, 0.0, 0.0, 6.0], [-1.0, 0.0, 0.0, 6.0]],
                },
            ),
            at([0.0, 0.0, 0.0]),
        ),
        (
            entity(4),
            warm(1.1, -1, PostVolumeShape::Unbound),
            at([0.0, 0.0, 0.0]),
        ),
    ];
    let mut scratch = Vec::with_capacity(volumes.len());
    // The blend runs into the same settings every frame (a fresh copy is not needed to measure
    // the blend; `PostSettings` holds an `Arc`, which clones without allocating anyway).
    let mut post = PostSettings::default();
    let mut contributed = 0usize;
    let walk = |frame: u32| [-10.0 + frame as f32 * 0.1, 0.5, 0.25];
    // One warm-up walk, then the same walk counted.
    for counted in [false, true] {
        let before = thread_allocs();
        for frame in 0..200 {
            gather_contributions(
                volumes.iter().map(|(e, v, w)| (*e, v, w)),
                walk(frame),
                &mut scratch,
            );
            contributed = contributed.max(scratch.len());
            blend_post_volumes(&mut post, &mut scratch);
        }
        if counted {
            assert_eq!(thread_allocs() - before, 0, "a steady walk allocated");
        }
    }
    assert_eq!(
        contributed,
        volumes.len(),
        "the walk passed where every volume contributes"
    );
}

/// § 6: a volume whose `format_version` is newer than the engine's is skipped, with one warning
/// per process however many frames and volumes meet it.
#[test]
fn a_newer_volume_is_skipped_with_one_warning() {
    install_counter();
    let newer = PostVolume {
        format_version: POST_VOLUME_FORMAT_VERSION + 1,
        shape: PostVolumeShape::Unbound,
        overrides: PostOverrides {
            saturation: Some(0.0),
            ..PostOverrides::default()
        },
        ..PostVolume::default()
    };
    let newest = PostVolume {
        format_version: POST_VOLUME_FORMAT_VERSION + 7,
        ..newer.clone()
    };
    let world = GlobalTransform::IDENTITY;
    let mut scratch = Vec::new();
    for _ in 0..30 {
        gather_contributions(
            [(entity(1), &newer, &world), (entity(2), &newest, &world)],
            [0.0; 3],
            &mut scratch,
        );
        assert!(scratch.is_empty(), "newer volumes contribute nothing");
    }
    assert_eq!(
        NEWER_WARNINGS.load(Ordering::Relaxed),
        1,
        "one warning per process"
    );
}

/// § 10.2: with `MAX_LOADED_LUTS` (64) paths known, a volume naming another path does not load
/// it, and says so once; `clear` forgets the tables and re-arms the warning.
#[test]
fn volume_luts_stop_at_the_path_cap_and_warn_once() {
    install_counter();
    assert_eq!(MAX_LOADED_LUTS, 64);
    let mut luts = PostVolumeLuts::default();
    luts.enabled = true;
    let identity = Arc::new(ColorLut::identity(2));
    for i in 0..MAX_LOADED_LUTS {
        luts.insert(format!("luts/known_{i}.cube"), Arc::clone(&identity));
    }
    let mut app = App::new();
    app.insert_resource(luts);
    app.add_systems(Update, load_post_volume_luts);
    for i in 0..3 {
        app.world_mut().spawn(PostVolume {
            lut: Some(format!("luts/over_the_cap_{i}.cube")),
            ..PostVolume::default()
        });
    }
    for _ in 0..10 {
        app.update();
    }
    let luts = app.world().resource::<PostVolumeLuts>();
    assert_eq!(
        luts.paths().len(),
        MAX_LOADED_LUTS,
        "nothing beyond the cap is kept"
    );
    for i in 0..3 {
        let path = format!("luts/over_the_cap_{i}.cube");
        assert!(
            luts.table(&path).is_none() && !luts.failed(&path),
            "{path} was never tried"
        );
    }
    assert_eq!(luts.stats().failures, 0, "no file was opened");
    assert_eq!(
        LUT_CAP_WARNINGS.load(Ordering::Relaxed),
        1,
        "one warning across frames and paths"
    );

    // After `clear` the paths are tried (and fail: they do not exist), and nothing warns about
    // the cap any more because it is not reached.
    app.world_mut().resource_mut::<PostVolumeLuts>().clear();
    app.update();
    let luts = app.world().resource::<PostVolumeLuts>();
    assert_eq!(luts.paths().len(), 3);
    assert!(luts.failed("luts/over_the_cap_0.cube"));
    assert_eq!(LUT_CAP_WARNINGS.load(Ordering::Relaxed), 1);
}
