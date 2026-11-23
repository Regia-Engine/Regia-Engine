// SPDX-License-Identifier: MPL-2.0
//! Scene-wide significance (ADR-3275): the acceptance checks of the vfx-scene-significance lane.
//! CPU only, fixed steps, no clock.

use regia_core::math::Vec3;
use regia_render::QualityTier;
use regia_vfx::VfxCamera;
use regia_vfx::VfxScene;
use regia_vfx::format::{VFX_FORMAT_VERSION, VfxSystemDef};
use regia_vfx::quality::significance_caps;
use regia_vfx::scene::VfxLibrary;
use regia_vfx::significance::{
    SignificanceCaps, SignificanceState, SignificanceView, StealPolicy, SystemCap,
};
use regia_vfx::sim::{LodInput, SpawnJob};

const DT: f32 = 1.0 / 64.0;
const LOD: LodInput = LodInput {
    camera_pos: Vec3::ZERO,
};

/// A looping rate emitter: `rate` particles a second, `life` seconds each.
fn def(name: &str, rate: f32, life: f32, budget: u32) -> VfxSystemDef {
    VfxSystemDef::from_ron(&format!(
        r#"(
    version: {VFX_FORMAT_VERSION},
    name: "{name}",
    system: (budget: {budget}),
    emitters: [
        (
            name: "puffs",
            spawn: (mode: Rate({rate:.1}), lifetime: (min: {life:.3}, max: {life:.3})),
            init: (size: (min: 0.2, max: 0.4)),
            render: (mode: Billboard),
        ),
    ],
)"#
    ))
    .expect("fixture parses")
}

/// A one-shot burst of `n`.
fn burst(name: &str, n: u32) -> VfxSystemDef {
    VfxSystemDef::from_ron(&format!(
        r#"(
    version: {VFX_FORMAT_VERSION},
    name: "{name}",
    emitters: [
        (
            name: "flash",
            spawn: (mode: Burst({n}), lifetime: (min: 0.5, max: 0.5)),
            init: (size: (min: 0.2, max: 0.4)),
            render: (mode: Billboard),
        ),
    ],
)"#
    ))
    .expect("fixture parses")
}

fn scene(defs: Vec<VfxSystemDef>) -> VfxScene {
    let mut library = VfxLibrary::new();
    for d in defs {
        library.add(d).expect("valid fixture");
    }
    VfxScene::new(library)
}

/// The camera at the origin looking down -Z.
fn view() -> SignificanceView {
    SignificanceView::from_camera(
        &VfxCamera::looking_at(Vec3::ZERO, Vec3::new(0.0, 0.0, -10.0)),
        1280,
        720,
    )
}

/// `n` instances of `name` on a line in front of the camera, `2 + 0.4 i` metres away (inside the
/// camera's 250 m far plane up to 500 of them), spawned in a
/// scrambled order (so slot order is not distance order). Returns ids by distance rank.
fn crowd(scene: &mut VfxScene, name: &str, n: u32) -> Vec<u64> {
    let mut by_rank = vec![0u64; n as usize];
    for k in 0..n {
        let i = (k * 7) % n;
        let at = Vec3::new(0.0, 0.0, -(2.0 + 0.4 * i as f32));
        by_rank[i as usize] = scene.spawn(name, at, Vec3::Z, u64::from(i) + 1).expect("spawn");
    }
    by_rank
}

fn spawning(scene: &VfxScene) -> Vec<u64> {
    let mut ids: Vec<u64> = scene
        .significance()
        .entries()
        .filter(|e| e.state.is_some_and(SignificanceState::spawns))
        .map(|e| e.id)
        .collect();
    ids.sort_unstable();
    ids
}

/// Acceptance: 500 instances of one system on the Low tier keep the instances that spawn within
/// the tier's cap, and the kept set is exactly the nearest ones, every frame; nothing past the cut
/// ever spawns a particle; the per-update spawn cap holds.
#[test]
fn five_hundred_instances_on_low_keep_the_nearest_within_the_cap() {
    let mut scene = scene(vec![def("smoke", 64.0, 1.0, 200)]);
    let caps = significance_caps(QualityTier::Low);
    let (max, full, per_frame) = (
        caps.max_instances.expect("cap"),
        caps.full_instances.expect("cap"),
        caps.max_spawns_per_frame.expect("cap"),
    );
    scene.significance_mut().set_caps(caps);
    scene.significance_mut().set_view(Some(view()));
    let by_rank = crowd(&mut scene, "smoke", 500);
    let mut nearest: Vec<u64> = by_rank[..max as usize].to_vec();
    nearest.sort_unstable();
    for frame in 0..128 {
        scene.update(DT, &LOD, 100_000);
        let stats = scene.significance().stats().clone();
        assert!(stats.full + stats.reduced <= max, "frame {frame}: {stats:?}");
        assert!(stats.full <= full, "frame {frame}: {stats:?}");
        assert_eq!(stats.full + stats.reduced + stats.culled, 500);
        assert!(scene.last_scheduled() <= per_frame);
        assert_eq!(
            spawning(&scene),
            nearest,
            "frame {frame}: the kept set is not the nearest"
        );
        let full_set: Vec<u64> = scene
            .significance()
            .entries()
            .filter(|e| e.state == Some(SignificanceState::Full))
            .map(|e| e.id)
            .collect();
        assert!(full_set.iter().all(|id| by_rank[..full as usize].contains(id)));
        for &id in &by_rank[max as usize..] {
            let instance = scene.instance(id).expect("live");
            assert_eq!(instance.live_particles(), 0, "frame {frame}: culled {id} spawned");
        }
    }
    // The kept instances do spawn: full at the authored rate, reduced at half of it.
    let live = |rank: usize| scene.instance(by_rank[rank]).expect("live").live_particles();
    assert_eq!(live(0), 64, "a full instance holds rate × lifetime");
    assert_eq!(live(max as usize - 1), 32, "a reduced instance holds half");
}

/// Acceptance: a priority override keeps a far effect alive (per instance and per system), and
/// the nearest one past the cap gives way.
#[test]
fn a_priority_override_keeps_a_far_effect_alive() {
    let mut scene = scene(vec![def("smoke", 64.0, 1.0, 200), def("beacon", 64.0, 1.0, 200)]);
    scene
        .significance_mut()
        .set_caps(significance_caps(QualityTier::Low));
    scene.significance_mut().set_view(Some(view()));
    let by_rank = crowd(&mut scene, "smoke", 100);
    let far = *by_rank.last().expect("crowd");
    assert!(scene.significance_mut().set_instance_priority(far, Some(1000.0)));
    let beacon = scene
        .spawn("beacon", Vec3::new(0.0, 0.0, -150.0), Vec3::Z, 9)
        .expect("beacon");
    assert!(
        scene
            .significance_mut()
            .set_system_priority("beacon", Some(1000.0))
    );
    for _ in 0..64 {
        scene.update(DT, &LOD, 100_000);
    }
    let sig = scene.significance();
    assert_eq!(
        sig.state_of(far),
        Some(SignificanceState::Full),
        "the far override"
    );
    assert_eq!(
        sig.state_of(beacon),
        Some(SignificanceState::Full),
        "the system override"
    );
    assert!(scene.instance(far).expect("live").live_particles() > 0);
    // Two slots went to the overrides: the 31st and 32nd nearest are now past the cut.
    assert_eq!(sig.state_of(by_rank[29]), Some(SignificanceState::Reduced));
    assert_eq!(sig.state_of(by_rank[30]), Some(SignificanceState::Culled));
    assert_eq!(sig.stats().full + sig.stats().reduced, 32);
}

/// Places `id` at `distance` metres down -Z.
fn place(scene: &mut VfxScene, id: u64, distance: f32) {
    scene
        .instance_mut(id)
        .expect("live")
        .set_transform(Vec3::new(0.0, 0.0, -distance), Vec3::Z, DT);
}

/// Runs 120 frames with `b` at `path(frame)` against `a` at 10 m under a one-instance cap; returns
/// how many times `b` changed state.
fn changes_at_the_cut(caps: SignificanceCaps, path: impl Fn(u32) -> f32) -> u32 {
    let mut scene = scene(vec![def("smoke", 64.0, 1.0, 200)]);
    scene.significance_mut().set_caps(SignificanceCaps {
        max_instances: Some(1),
        ..caps
    });
    scene.significance_mut().set_view(Some(view()));
    let a = scene
        .spawn("smoke", Vec3::new(0.0, 0.0, -10.0), Vec3::Z, 1)
        .expect("a");
    let b = scene
        .spawn("smoke", Vec3::new(0.0, 0.0, -path(0)), Vec3::Z, 2)
        .expect("b");
    let mut last = None;
    let mut changes = 0;
    for frame in 0..120 {
        place(&mut scene, b, path(frame));
        place(&mut scene, a, 10.0);
        scene.update(DT, &LOD, 100_000);
        let state = scene.significance().state_of(b);
        assert!(state.is_some());
        if last.is_some() && state != last {
            changes += 1;
        }
        last = state;
        let kept = [a, b]
            .iter()
            .filter(|&&id| scene.significance().state_of(id) == Some(SignificanceState::Full))
            .count();
        assert_eq!(kept, 1, "frame {frame}: the cap holds while b moves");
    }
    changes
}

/// Acceptance: an instance moving back and forth across the cut changes state at most twice in
/// 120 frames, with the default hysteresis; a decisive move still swaps (the bonus does not freeze
/// the ranking), and the dwell alone bounds a cut with no bonus.
#[test]
fn an_instance_at_the_cut_changes_state_at_most_twice_in_120_frames() {
    let caps = SignificanceCaps::default;
    // Every frame across the cut: 9.5 m and 10.5 m against 10 m.
    let jitter = changes_at_the_cut(caps(), |f| if f.is_multiple_of(2) { 10.5 } else { 9.5 });
    assert!(jitter <= 2, "jitter: {jitter} changes");
    // A slower swing of ±20 % around the cut, period 8 frames.
    let swing = changes_at_the_cut(caps(), |f| {
        10.0 * (1.0 + 0.2 * ((f % 8) as f32 / 4.0 - 1.0).abs().mul_add(-2.0, 1.0))
    });
    assert!(swing <= 2, "swing: {swing} changes");
    // Without the bonus the dwell (30 frames) still holds it: at most 120 / 30 changes.
    let no_bonus = changes_at_the_cut(
        SignificanceCaps {
            hysteresis: 0.0,
            ..caps()
        },
        |f| if f.is_multiple_of(2) { 10.5 } else { 9.5 },
    );
    assert!(no_bonus <= 4, "dwell only: {no_bonus} changes");
    // A decisive move (b from 12 m to 4 m at frame 40) takes the slot: exactly one change.
    let decisive = changes_at_the_cut(caps(), |f| if f < 40 { 12.0 } else { 4.0 });
    assert_eq!(decisive, 1, "a clear winner must take the slot");
}

/// Acceptance: an instance out of view spawns nothing while it is out of view (a due burst is
/// spent without particles), and its warm-up on re-entry stays within its bound.
#[test]
fn an_offscreen_instance_spawns_nothing_and_its_warmup_is_bounded() {
    let mut scene = scene(vec![def("stream", 640.0, 1.0, 4000), burst("flash", 40)]);
    let caps = SignificanceCaps {
        pause_offscreen: true,
        warmup_s: 0.25,
        warmup_max_particles: 32,
        ..SignificanceCaps::default()
    };
    scene.significance_mut().set_caps(caps);
    scene.significance_mut().set_view(Some(view()));
    let behind = Vec3::new(0.0, 0.0, 30.0);
    let id = scene.spawn("stream", behind, Vec3::Z, 1).expect("stream");
    let flash = scene.spawn("flash", behind, Vec3::Z, 2).expect("flash");
    for frame in 0..64 {
        scene.update(DT, &LOD, 100_000);
        assert_eq!(scene.last_scheduled(), 0, "frame {frame}: an off-screen spawn");
        assert_eq!(scene.instance(id).expect("live").live_particles(), 0);
        assert_eq!(scene.significance().state_of(id), Some(SignificanceState::Paused));
    }
    // The burst was spent unseen: the one-shot retired with no particle.
    assert!(scene.instance(flash).is_none(), "the spent burst retired");
    // Back in view: one bounded catch-up frame, then the authored 10 a frame.
    scene
        .instance_mut(id)
        .expect("live")
        .set_transform(Vec3::new(0.0, 0.0, -30.0), Vec3::Z, DT);
    scene.update(DT, &LOD, 100_000);
    let warm = scene.last_scheduled();
    assert_eq!(scene.significance().state_of(id), Some(SignificanceState::Full));
    assert!(warm <= 32, "warm-up {warm} over its bound");
    assert!(warm > 10, "warm-up {warm} did not catch up at all");
    assert_eq!(scene.significance().stats().warmup_particles, warm);
    scene.update(DT, &LOD, 100_000);
    assert_eq!(
        scene.last_scheduled(),
        10,
        "the frame after spawns the authored rate"
    );
    // Out again past the margin: paused, nothing spawns.
    scene
        .instance_mut(id)
        .expect("live")
        .set_transform(behind, Vec3::Z, DT);
    scene.update(DT, &LOD, 100_000);
    assert_eq!(scene.significance().state_of(id), Some(SignificanceState::Paused));
    assert_eq!(scene.last_scheduled(), 0);
}

/// Every instance's jobs, flattened, after `frames` updates (the jobs drained each frame).
fn run_jobs(scene: &mut VfxScene, frames: u32, capacity: u32) -> Vec<Vec<SpawnJob>> {
    let mut out = Vec::new();
    for _ in 0..frames {
        scene.update_budgets(DT, &LOD, capacity, 2_000);
        let mut frame = Vec::new();
        for instance in scene.instances_mut() {
            instance.drain_jobs(&mut frame);
        }
        out.push(frame);
    }
    out
}

fn mixed_scene() -> VfxScene {
    let mut s = scene(vec![def("smoke", 300.0, 0.7, 150), burst("flash", 40)]);
    for i in 0..6u32 {
        s.spawn(
            "smoke",
            Vec3::new(i as f32, 0.0, -5.0 - i as f32),
            Vec3::Z,
            u64::from(i) + 10,
        )
        .expect("smoke");
    }
    s.spawn("flash", Vec3::new(0.0, 1.0, -3.0), Vec3::Z, 99)
        .expect("flash");
    s
}

/// Acceptance: with no caps every job is bit-identical to a scene that never had the module (caps
/// set and then cleared included), and with caps that never bind each instance's jobs are too.
#[test]
fn no_caps_and_unbinding_caps_schedule_bit_identically() {
    let reference = run_jobs(&mut mixed_scene(), 96, 600);
    let mut cleared = mixed_scene();
    cleared
        .significance_mut()
        .set_caps(significance_caps(QualityTier::MobileLow));
    cleared.significance_mut().set_caps(SignificanceCaps::default());
    assert_eq!(
        run_jobs(&mut cleared, 96, 600),
        reference,
        "cleared caps changed the schedule"
    );

    // Caps far above the scene: the same jobs per instance (the scene order is by rank, so compare
    // sorted by bits).
    let key = |j: &SpawnJob| j.position.map(f32::to_bits);
    let sorted = |mut frames: Vec<Vec<SpawnJob>>| {
        for f in &mut frames {
            f.sort_by_key(key);
        }
        frames
    };
    let free_reference = sorted(run_jobs(&mut mixed_scene(), 96, 100_000));
    let mut loose = mixed_scene();
    loose.significance_mut().set_caps(SignificanceCaps {
        max_instances: Some(1000),
        max_spawns_per_frame: Some(1_000_000),
        max_live: Some(1_000_000),
        ..SignificanceCaps::default()
    });
    assert_eq!(sorted(run_jobs(&mut loose, 96, 100_000)), free_reference);
}

/// The per-system cap: `reject` refuses, `oldest` stops the oldest, `least_significant` stops the
/// least significant or refuses a newcomer that scores lower.
#[test]
fn per_system_caps_steal_or_refuse() {
    let capped = |steal| {
        let mut s = scene(vec![def("smoke", 64.0, 1.0, 200)]);
        let mut caps = SignificanceCaps::default();
        caps.set_system_cap(SystemCap {
            system: "smoke".into(),
            max_instances: 3,
            steal,
        });
        s.significance_mut().set_caps(caps);
        s
    };
    let active = |s: &VfxScene| s.instances().filter(|i| i.is_active()).count();

    let mut s = capped(StealPolicy::Reject);
    for i in 0..3 {
        s.spawn("smoke", Vec3::new(0.0, 0.0, -5.0), Vec3::Z, i)
            .expect("under the cap");
    }
    assert!(s.spawn("smoke", Vec3::ZERO, Vec3::Z, 9).is_none());
    s.update(DT, &LOD, 100_000);
    assert_eq!(s.significance().stats().rejected_spawns, 1);
    assert_eq!(active(&s), 3);

    let mut s = capped(StealPolicy::Oldest);
    let first = s.spawn("smoke", Vec3::ZERO, Vec3::Z, 1).expect("first");
    s.update(DT, &LOD, 100_000);
    for i in 2..=4 {
        s.spawn("smoke", Vec3::ZERO, Vec3::Z, i).expect("steals");
        s.update(DT, &LOD, 100_000);
    }
    // Stopped: its unuploaded jobs were dropped, so it may already have retired.
    assert!(
        s.instance(first).is_none_or(|i| !i.is_active()),
        "the oldest was stopped"
    );
    assert_eq!(active(&s), 3);
    assert_eq!(s.significance().stats().stolen_total, 1);

    let mut s = capped(StealPolicy::LeastSignificant);
    s.significance_mut().set_view(Some(view()));
    let far = s
        .spawn("smoke", Vec3::new(0.0, 0.0, -60.0), Vec3::Z, 1)
        .expect("far");
    for i in 2..=3 {
        s.spawn("smoke", Vec3::new(0.0, 0.0, -5.0), Vec3::Z, i)
            .expect("near");
    }
    s.update(DT, &LOD, 100_000);
    assert!(
        s.spawn("smoke", Vec3::new(0.0, 0.0, -90.0), Vec3::Z, 7).is_none(),
        "a newcomer farther than all is refused"
    );
    s.spawn("smoke", Vec3::new(0.0, 0.0, -4.0), Vec3::Z, 8)
        .expect("a near one steals");
    assert!(
        s.instance(far).is_none_or(|i| !i.is_active()),
        "the far one was stopped"
    );
    assert_eq!(active(&s), 3);
}

/// The per-update spawn cap is granted in rank order: the nearest instances spawn first.
#[test]
fn the_spawn_cap_goes_to_the_most_significant_first() {
    let mut s = scene(vec![def("smoke", 640.0, 2.0, 4000)]);
    s.significance_mut().set_caps(SignificanceCaps {
        max_spawns_per_frame: Some(25),
        ..SignificanceCaps::default()
    });
    s.significance_mut().set_view(Some(view()));
    let by_rank = crowd(&mut s, "smoke", 10);
    for _ in 0..8 {
        s.update(DT, &LOD, 100_000);
        assert_eq!(s.last_scheduled(), 25);
        assert!(s.significance().stats().spawn_cap_reached);
    }
    // 10 a frame each: the two nearest get everything they ask for, the third the rest.
    let live = |rank: usize| s.instance(by_rank[rank]).expect("live").live_particles();
    assert_eq!((live(0), live(1), live(2), live(3)), (80, 80, 40, 0));
}

/// ADR-7431 (1): a newcomer that outscores a held incumbent (by the hysteresis bonus) displaces it
/// on its first ranking. Under a one-instance cap, a low-priority instance ranked at frame 0 and a
/// higher-priority one spawned at frame 1: the newcomer spawns from frame 1 and the incumbent is
/// culled, and neither flickers back over the next 60 frames. Before ADR-7431 the incumbent's
/// dwell (30 frames) held the slot and the newcomer stayed culled for 30 frames.
#[test]
fn a_better_newcomer_displaces_a_held_incumbent() {
    let mut scene = scene(vec![def("smoke", 64.0, 1.0, 200)]);
    scene.significance_mut().set_caps(SignificanceCaps {
        max_instances: Some(1),
        ..SignificanceCaps::default()
    });
    scene.significance_mut().set_view(Some(view()));
    let low = scene
        .spawn("smoke", Vec3::new(0.0, 0.0, -10.0), Vec3::Z, 1)
        .expect("low");
    scene.update(DT, &LOD, 100_000);
    assert_eq!(
        scene.significance().state_of(low),
        Some(SignificanceState::Full),
        "frame 0: the only instance takes the slot"
    );
    let high = scene
        .spawn("smoke", Vec3::new(0.0, 0.0, -10.0), Vec3::Z, 2)
        .expect("high");
    assert!(scene.significance_mut().set_instance_priority(high, Some(4.0)));
    for frame in 1..=60 {
        scene.update(DT, &LOD, 100_000);
        let sig = scene.significance();
        assert_eq!(
            (sig.state_of(high), sig.state_of(low)),
            (Some(SignificanceState::Full), Some(SignificanceState::Culled)),
            "frame {frame}: the better newcomer holds the slot"
        );
        assert!(sig.stats().full + sig.stats().reduced <= 1);
    }
    assert!(scene.instance(high).expect("live").live_particles() > 0);
    // A newcomer that does not beat the incumbent's bonus waits: same priority at 14 m against
    // 10 m, its youth boost (x1.5) short of the incumbent's 25 % bonus.
    let mut scene2 = scene_with_incumbent();
    let weaker = scene2
        .spawn("smoke", Vec3::new(0.0, 0.0, -14.0), Vec3::Z, 3)
        .expect("weaker");
    for frame in 0..60 {
        scene2.update(DT, &LOD, 100_000);
        assert_eq!(
            scene2.significance().state_of(weaker),
            Some(SignificanceState::Culled),
            "frame {frame}: a weaker newcomer must not displace the incumbent"
        );
    }
}

/// One instance at 10 m under a one-instance cap, ranked for 80 frames (its youth spent).
fn scene_with_incumbent() -> VfxScene {
    let mut scene = scene(vec![def("smoke", 64.0, 1.0, 200)]);
    scene.significance_mut().set_caps(SignificanceCaps {
        max_instances: Some(1),
        ..SignificanceCaps::default()
    });
    scene.significance_mut().set_view(Some(view()));
    scene
        .spawn("smoke", Vec3::new(0.0, 0.0, -10.0), Vec3::Z, 1)
        .expect("incumbent");
    for _ in 0..80 {
        scene.update(DT, &LOD, 100_000);
    }
    scene
}
