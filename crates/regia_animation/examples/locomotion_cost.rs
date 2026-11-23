// SPDX-License-Identifier: MPL-2.0
//! Cost of engine locomotion (animation-11, ADR-8728): bipeds with `FootPlacement` and hexapods
//! with `GaitRig` walking on an analytic rolling ground, a fixed number of frames.
//!
//! ```text
//! cargo run --release -p regia_animation --example locomotion_cost -- [--bipeds 64] [--hexapods 16]
//!     [--frames 2000] [--camera] [--scaling]
//! ```
//!
//! Prints, per run, the locomotion time per frame (the `foot_placement`, `gait_rig_step` and
//! `gait_rig_pose` spans summed over every rig, timed by a minimal span timer that only enables
//! those three spans) as p50, p99 and max, and the whole app frame p50/p99 with and without the
//! locomotion components. Without `--camera` every rig is at full IK detail; with it the LOD camera
//! sits at the origin and the rigs spread out to about 100 m, so some are reduced and some off.
//! `--scaling` runs 16, 64 and 256 characters (a quarter of them hexapods). Timing is evidence
//! only; nothing here asserts a number. The last line is a digest of every rig's joints, equal
//! between runs of one binary on one target.

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use regia_animation::clip::AnimationClip;
use regia_animation::components::spawn_skeleton;
use regia_animation::demo_rig::{
    DemoGait, DemoRig, RigNaming, demo_humanoid, demo_in_place, demo_locomotion,
};
use regia_animation::foot_ik::{FootLegDesc, FootPlacement, GroundHit, GroundProbe, GroundProbes};
use regia_animation::gait::{GaitParams, GaitPatternSet, GaitPosture, MAX_LEGS, TRIPOD_LEGS, leg_array};
use regia_animation::gait_rig::{GaitLeg, GaitRig};
use regia_animation::skeleton::LocalBind;
use regia_animation::{AnimLodCamera, AnimationPlugin, Animator, JointGlobals, Skeleton};
use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_scene::Transform;

const DT: f32 = 1.0 / 60.0;
const WARMUP: usize = 120;

// ------------------------------------------------------------------ span timer

/// The spans whose time is the locomotion cost. None of them nest inside another.
const TIMED: [&str; 3] = ["foot_placement", "gait_rig_step", "gait_rig_pose"];

/// Nanoseconds spent inside the timed spans since the last [`take_locomotion_ns`].
static LOCOMOTION_NS: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static STARTS: RefCell<Vec<Instant>> = const { RefCell::new(Vec::new()) };
}

/// A subscriber that enables only the timed spans and adds up the time spent inside them.
struct SpanTimer;

impl tracing::Subscriber for SpanTimer {
    fn register_callsite(
        &self,
        metadata: &'static tracing::Metadata<'static>,
    ) -> tracing::subscriber::Interest {
        if self.enabled(metadata) {
            tracing::subscriber::Interest::always()
        } else {
            tracing::subscriber::Interest::never()
        }
    }

    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        metadata.is_span() && TIMED.contains(&metadata.name())
    }

    fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
        Some(tracing::level_filters::LevelFilter::TRACE)
    }

    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

    fn event(&self, _event: &tracing::Event<'_>) {}

    fn enter(&self, _span: &tracing::span::Id) {
        STARTS.with(|s| s.borrow_mut().push(Instant::now()));
    }

    fn exit(&self, _span: &tracing::span::Id) {
        if let Some(start) = STARTS.with(|s| s.borrow_mut().pop()) {
            let ns = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
            LOCOMOTION_NS.fetch_add(ns, Ordering::Relaxed);
        }
    }
}

fn take_locomotion_ns() -> u64 {
    LOCOMOTION_NS.swap(0, Ordering::Relaxed)
}

// ------------------------------------------------------------------ ground

/// A rolling ground: `y = A sin(kx x) cos(kz z)`, probed straight down.
#[derive(Clone, Copy)]
struct Rolling;

const AMPLITUDE: f32 = 0.12;
const KX: f32 = 0.31;
const KZ: f32 = 0.23;

fn height(x: f32, z: f32) -> f32 {
    AMPLITUDE * (KX * x).sin() * (KZ * z).cos()
}

fn normal(x: f32, z: f32) -> Vec3 {
    let dx = AMPLITUDE * KX * (KX * x).cos() * (KZ * z).cos();
    let dz = -AMPLITUDE * KZ * (KX * x).sin() * (KZ * z).sin();
    Vec3::new(-dx, 1.0, -dz).normalize()
}

impl GroundProbe for Rolling {
    fn probe(&self, from: Vec3, _down: Vec3, max_distance: f32, _ignore: Entity) -> Option<GroundHit> {
        let y = height(from.x, from.z);
        let drop = from.y - y;
        (0.0..=max_distance).contains(&drop).then(|| GroundHit {
            point: Vec3::new(from.x, y, from.z),
            normal: normal(from.x, from.z),
        })
    }
}

// ------------------------------------------------------------------ rigs

fn biped() -> Option<(Arc<Skeleton>, AnimationClip)> {
    let skeleton = demo_humanoid(&DemoRig {
        naming: RigNaming::Game,
        ..DemoRig::default()
    })?;
    let gait = DemoGait::standard().into_iter().find(|g| g.name == "walk")?;
    let clip = demo_in_place(&demo_locomotion(&skeleton, std::slice::from_ref(&gait)).pop()?);
    Some((Arc::new(skeleton), clip))
}

fn feet() -> FootPlacement {
    let leg = |side: &str| FootLegDesc {
        hip: format!("thigh_{side}"),
        knee: format!("calf_{side}"),
        ankle: format!("foot_{side}"),
        toe: None,
        pole_offset: Vec3::ZERO,
    };
    let mut placement = FootPlacement::default();
    placement.legs = vec![leg("l"), leg("r")];
    placement
}

const BODY_HEIGHT: f32 = 0.5;

fn hexapod() -> Option<Arc<Skeleton>> {
    let mut names = vec!["root".to_string(), "body".to_string()];
    let mut parents = vec![None, Some(0)];
    let mut binds = vec![
        LocalBind::default(),
        LocalBind {
            translation: Vec3::new(0.0, BODY_HEIGHT, 0.0),
            ..LocalBind::default()
        },
    ];
    for leg in 0..TRIPOD_LEGS {
        let side = if leg % 2 == 0 { -1.0 } else { 1.0 };
        let z = [-0.5, 0.0, 0.5][leg / 2];
        let mut parent = 1_u32;
        let joints = [
            (format!("hip{leg}"), Vec3::new(side * 0.25, 0.0, z)),
            (format!("knee{leg}"), Vec3::new(side * 0.2, 0.15, 0.0)),
            (format!("foot{leg}"), Vec3::new(side * 0.15, -0.65, 0.0)),
        ];
        for (name, translation) in joints {
            names.push(name);
            parents.push(Some(parent));
            binds.push(LocalBind {
                translation,
                ..LocalBind::default()
            });
            parent = (names.len() - 1) as u32;
        }
    }
    Skeleton::from_local_bind(names, parents, binds)
        .ok()
        .map(Arc::new)
}

fn walker() -> GaitRig {
    let params = GaitParams {
        forward_speed: 0.6,
        reverse_speed: 0.3,
        strafe_speed: 0.3,
        acceleration: 2.0,
        step_length: 0.3,
        clearance: BODY_HEIGHT,
        swing_lift: 0.1,
        max_slope: 1.5,
        idle_speed: 0.02,
        max_yaw_rate: 0.8,
        hip_offsets: leg_array([[0.0; 2]; TRIPOD_LEGS], [0.0; 2]),
        settle_distance: 0.05,
        settle_period: 0.3,
        turn_step_distance: 0.1,
        turn_step_angle: 0.35,
        ground_band: 0.6,
        foothold_candidates: 0,
        foothold_spacing: 0.2,
        max_reach: [f32::INFINITY; MAX_LEGS],
        hang_drop: 0.2,
        patterns: GaitPatternSet::tripod(),
        posture: GaitPosture::UPRIGHT,
    };
    let legs = (0..TRIPOD_LEGS)
        .map(|i| GaitLeg::new(format!("hip{i}"), format!("foot{i}")))
        .collect();
    GaitRig::new(params, legs).with_body("body")
}

/// A rig's walk: a circle of `radius` around `center` at `speed`, starting at `phase`.
#[derive(Clone, Copy)]
struct Path {
    center: Vec3,
    radius: f32,
    speed: f32,
    phase: f32,
}

impl Path {
    fn at(&self, frame: usize) -> Transform {
        let angle = self.phase + self.speed / self.radius * DT * frame as f32;
        let (s, c) = angle.sin_cos();
        let x = self.center.x + self.radius * c;
        let z = self.center.z + self.radius * s;
        // Facing along the circle (counter-clockwise seen from above): forward is −Z at yaw 0.
        let tangent = Vec3::new(-s, 0.0, c);
        let yaw = (-tangent.x).atan2(-tangent.z);
        Transform {
            translation: Vec3::new(x, height(x, z), z),
            rotation: Quat::from_rotation_y(yaw),
            scale: Vec3::ONE,
        }
    }
}

struct Scene {
    app: App,
    rigs: Vec<(Entity, Path)>,
}

fn build(bipeds: usize, hexapods: usize, locomotion: bool, camera: bool) -> Option<Scene> {
    let (human, clip) = biped()?;
    let crawler = hexapod()?;
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, AnimationPlugin));
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(DT)));
    app.insert_resource(GroundProbes::new(Rolling));
    if camera {
        app.insert_resource(AnimLodCamera { position: Vec3::ZERO });
    }
    let total = bipeds + hexapods;
    // Spread on a spiral: about 3 m apart near the middle, out to about 100 m with --camera.
    let spread = if camera {
        100.0
    } else {
        3.0 * (total as f32).sqrt()
    };
    let mut rigs = Vec::with_capacity(total);
    for k in 0..total {
        let f = (k as f32 + 0.5) / total as f32;
        let r = spread * f.sqrt();
        let a = k as f32 * 2.399_963;
        let center = Vec3::new(r * a.cos(), 0.0, r * a.sin());
        let is_biped = k < bipeds;
        let path = Path {
            center,
            radius: 1.5 + (k % 5) as f32 * 0.4,
            speed: if is_biped { 1.3 } else { 0.4 },
            phase: k as f32 * 0.7,
        };
        let start = path.at(0);
        let entity = if is_biped {
            let binding = spawn_skeleton(app.world_mut(), human.clone());
            let mut e = app
                .world_mut()
                .spawn((start, binding, Animator::single(clip.clone(), true)));
            if locomotion {
                e.insert(feet());
            }
            e.id()
        } else {
            let binding = spawn_skeleton(app.world_mut(), crawler.clone());
            let animator = Animator::single(AnimationClip::empty("still", 1.0), true);
            let mut e = app.world_mut().spawn((start, binding, animator));
            if locomotion {
                e.insert(walker());
            }
            e.id()
        };
        rigs.push((entity, path));
    }
    Some(Scene { app, rigs })
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let i = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[i.min(sorted.len() - 1)]
}

struct Run {
    locomotion_ms: Vec<f64>,
    frame_ms: Vec<f64>,
    digest: u64,
}

fn run(bipeds: usize, hexapods: usize, frames: usize, locomotion: bool, camera: bool) -> Option<Run> {
    let mut scene = build(bipeds, hexapods, locomotion, camera)?;
    let mut locomotion_ms = Vec::with_capacity(frames);
    let mut frame_ms = Vec::with_capacity(frames);
    for frame in 0..(WARMUP + frames) {
        for &(entity, path) in &scene.rigs {
            if let Some(mut t) = scene.app.world_mut().get_mut::<Transform>(entity) {
                *t = path.at(frame);
            }
        }
        take_locomotion_ns();
        let start = Instant::now();
        scene.app.update();
        let elapsed = start.elapsed();
        let spans = take_locomotion_ns();
        if frame >= WARMUP {
            frame_ms.push(elapsed.as_secs_f64() * 1.0e3);
            locomotion_ms.push(spans as f64 * 1.0e-6);
        }
    }
    let mut digest = 0xcbf2_9ce4_8422_2325_u64;
    for &(entity, _) in &scene.rigs {
        if let Some(globals) = scene.app.world().get::<JointGlobals>(entity) {
            for m in &globals.globals {
                for v in m.to_cols_array() {
                    for byte in v.to_bits().to_le_bytes() {
                        digest ^= u64::from(byte);
                        digest = digest.wrapping_mul(0x0100_0000_01b3);
                    }
                }
            }
        }
    }
    locomotion_ms.sort_by(f64::total_cmp);
    frame_ms.sort_by(f64::total_cmp);
    Some(Run {
        locomotion_ms,
        frame_ms,
        digest,
    })
}

fn report(bipeds: usize, hexapods: usize, frames: usize, camera: bool) {
    let (Some(with), Some(without)) = (
        run(bipeds, hexapods, frames, true, camera),
        run(bipeds, hexapods, frames, false, camera),
    ) else {
        eprintln!("locomotion_cost: the demo rigs could not be built");
        return;
    };
    let l = &with.locomotion_ms;
    println!(
        "locomotion_cost bipeds={bipeds} hexapods={hexapods} frames={frames} lod={} \
         locomotion_ms p50={:.4} p99={:.4} max={:.4} | frame_ms with p50={:.3} p99={:.3} \
         without p50={:.3} p99={:.3} | digest={:016x}",
        if camera { "camera" } else { "full" },
        percentile(l, 0.5),
        percentile(l, 0.99),
        l.last().copied().unwrap_or(0.0),
        percentile(&with.frame_ms, 0.5),
        percentile(&with.frame_ms, 0.99),
        percentile(&without.frame_ms, 0.5),
        percentile(&without.frame_ms, 0.99),
        with.digest,
    );
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let value = |name: &str, default: usize| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    let bipeds = value("--bipeds", 64);
    let hexapods = value("--hexapods", 16);
    let frames = value("--frames", 2000).max(1);
    let camera = args.iter().any(|a| a == "--camera");
    if tracing::subscriber::set_global_default(SpanTimer).is_err() {
        eprintln!("locomotion_cost: a tracing subscriber is already installed; span times will read 0");
    }
    println!(
        "locomotion_cost: {} ({} threads available)",
        std::env::consts::ARCH,
        std::thread::available_parallelism().map_or(0, usize::from)
    );
    if args.iter().any(|a| a == "--scaling") {
        for total in [16_usize, 64, 256] {
            let hexapods = total / 4;
            report(total - hexapods, hexapods, frames, camera);
        }
    } else {
        report(bipeds, hexapods, frames, camera);
    }
}
