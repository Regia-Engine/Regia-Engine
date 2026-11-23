// SPDX-License-Identifier: MPL-2.0
//! Determinism matrix: one headless scene, stepped a fixed number of frames, compared checkpoint
//! by checkpoint across the schedule executor, the compute pool size and the build profile
//! (`book/agents/determinism-matrix.md`, ADR-1347).
//!
//! A [`Trace`] is the [`crate::shuffle::snapshot`] hash at every checkpoint plus one hash per
//! `<entity>/<Component>` key at the last frame, so a mismatch names the first frame that
//! differs and the components that moved. Three comparisons use it:
//!
//! - **Executor**: [`record`] with [`Executor::SingleThreaded`] and
//!   [`Executor::MultiThreaded`] in one process.
//! - **Thread count**: the compute pool is process-global and sized once, so [`run_children`]
//!   re-runs the current test binary once per pool size with [`CHILD_ENV`] set. The child test
//!   calls [`child_threads`], sizes the pool with [`init_child_pool`], records its scenes and
//!   prints them with [`print_child_traces`]; the parent parses stdout.
//! - **Build profile**: a debug and a release build cannot run in one process, so each compares
//!   with a trace pinned in the repo per target ([`check_pinned`]); `REGIA_BLESS=1` writes it.
//!   The pinned trace also catches drift between commits.
//!
//! [`race_app`] is the scene that has the drift these checks exist for: three systems with no
//! order between them spawn colliding bodies through `Commands` on the same steps, and a fourth
//! despawns old ones, so entity ids are reserved (and reused) in thread-timing order.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use regia_app::prelude::*;
use regia_core::math::{Rng, Vec3};
use regia_core::tasks::TaskPoolOptions;
use regia_ecs::ecs::schedule::{MultiThreadedExecutor, Schedules, SingleThreadedExecutor};
use regia_physics::{Collider, ColliderShape, PhysicsStep, RigidBody, Velocity};
use regia_scene::Transform;
use regia_scene::components::Name;

use crate::shuffle::{fnv1a, snapshot, snapshot_hash};

/// First line of a trace in text form.
pub const TRACE_HEADER: &str = "regia-det-trace v1";

/// Environment variable that turns a test into a child run: the compute pool size.
pub const CHILD_ENV: &str = "REGIA_DET_CHILD_THREADS";

/// Prefix of the line a child prints with the compute pool size it actually got.
pub const POOL_LINE: &str = "regia-det-pool ";

/// Prefix of the line a child prints with [`race_out_of_order`].
pub const RACE_LINE: &str = "regia-det-race-out-of-order ";

/// The schedule executor every schedule of the app runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Executor {
    /// One system at a time, in the schedule's topological order.
    SingleThreaded,
    /// Systems without conflicting access run in parallel on the compute pool.
    MultiThreaded,
}

/// Puts every schedule of `app` on `executor`. Call it after adding plugins and systems and
/// before the first update.
pub fn set_executor(app: &mut App, executor: Executor) {
    let mut schedules = app.world_mut().resource_mut::<Schedules>();
    for (_, schedule) in schedules.iter_mut() {
        match executor {
            Executor::SingleThreaded => {
                schedule.set_executor(SingleThreadedExecutor::new());
            }
            Executor::MultiThreaded => {
                schedule.set_executor(MultiThreadedExecutor::new());
            }
        }
    }
}

/// One scene's run: snapshot hashes at the checkpoints and per-key hashes at the end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trace {
    /// Scene name (one word).
    pub scene: String,
    /// Frames stepped.
    pub frames: u32,
    /// Checkpoint spacing in frames.
    pub every: u32,
    /// `(frame, snapshot hash)` after each checkpoint frame, the last frame included.
    pub checkpoints: Vec<(u32, u64)>,
    /// `<entity>/<Component>` → FNV-1a of its value text, at the last frame.
    pub keys: BTreeMap<String, u64>,
}

/// Steps `app` (built by `build`) `frames` frames on `executor` and records a [`Trace`] with a
/// checkpoint every `every` frames and at the last one.
pub fn record(
    scene: &str,
    build: impl FnOnce() -> App,
    frames: u32,
    every: u32,
    executor: Executor,
) -> Trace {
    let mut app = build();
    set_executor(&mut app, executor);
    let every = every.max(1);
    let mut checkpoints = Vec::new();
    for frame in 1..=frames {
        app.update();
        if frame.is_multiple_of(every) || frame == frames {
            checkpoints.push((frame, snapshot_hash(&snapshot(app.world()))));
        }
    }
    let keys = snapshot(app.world())
        .into_iter()
        .map(|(key, value)| (key, fnv1a(value.as_bytes())))
        .collect();
    Trace {
        scene: scene.to_string(),
        frames,
        every,
        checkpoints,
        keys,
    }
}

impl Trace {
    /// The text form: [`TRACE_HEADER`], then `scene`, `frames`, `every`, one `cp <frame> <hash>`
    /// line per checkpoint, one `key <hash> <key>` line per key, and `end`. Lines starting
    /// with `#` are comments.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "{TRACE_HEADER}");
        let _ = writeln!(out, "scene {}", self.scene);
        let _ = writeln!(out, "frames {}", self.frames);
        let _ = writeln!(out, "every {}", self.every);
        for (frame, hash) in &self.checkpoints {
            let _ = writeln!(out, "cp {frame} {hash:016x}");
        }
        for (key, hash) in &self.keys {
            let _ = writeln!(out, "key {hash:016x} {key}");
        }
        let _ = writeln!(out, "end");
        out
    }

    /// Every trace in `text`, in order. Lines outside a trace (test harness output) are skipped;
    /// a trace that is cut off or malformed is an error.
    pub fn parse_all(text: &str) -> Result<Vec<Trace>, String> {
        let mut traces = Vec::new();
        let mut lines = text.lines();
        while let Some(line) = lines.next() {
            if line.trim_end() != TRACE_HEADER {
                continue;
            }
            traces.push(Self::parse_body(&mut lines)?);
        }
        Ok(traces)
    }

    /// The single trace in `text`.
    pub fn parse(text: &str) -> Result<Trace, String> {
        let mut traces = Self::parse_all(text)?;
        match traces.len() {
            1 => Ok(traces.remove(0)),
            n => Err(format!("expected one trace, found {n}")),
        }
    }

    fn parse_body<'a>(lines: &mut impl Iterator<Item = &'a str>) -> Result<Trace, String> {
        let mut trace = Trace {
            scene: String::new(),
            frames: 0,
            every: 0,
            checkpoints: Vec::new(),
            keys: BTreeMap::new(),
        };
        let hex = |s: &str| u64::from_str_radix(s, 16).map_err(|e| format!("bad hash {s:?}: {e}"));
        let num = |s: &str| s.parse::<u32>().map_err(|e| format!("bad number {s:?}: {e}"));
        for line in lines {
            let line = line.trim_end();
            if line.starts_with('#') || line.is_empty() {
                continue;
            }
            let (tag, rest) = line.split_once(' ').unwrap_or((line, ""));
            match tag {
                "scene" => trace.scene = rest.to_string(),
                "frames" => trace.frames = num(rest)?,
                "every" => trace.every = num(rest)?,
                "cp" => {
                    let (frame, hash) = rest
                        .split_once(' ')
                        .ok_or_else(|| format!("bad checkpoint line {line:?}"))?;
                    trace.checkpoints.push((num(frame)?, hex(hash)?));
                }
                "key" => {
                    let (hash, key) = rest
                        .split_once(' ')
                        .ok_or_else(|| format!("bad key line {line:?}"))?;
                    trace.keys.insert(key.to_string(), hex(hash)?);
                }
                "end" => {
                    if trace.scene.is_empty() {
                        return Err("trace without a scene".into());
                    }
                    return Ok(trace);
                }
                other => return Err(format!("unknown trace line {other:?}")),
            }
        }
        Err(format!("trace {:?} is cut off (no `end`)", trace.scene))
    }
}

/// How two traces of the same scene differ.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceMismatch {
    /// The scene.
    pub scene: String,
    /// The runs were not comparable (scene, frame count or checkpoint spacing differ).
    pub shape: Option<String>,
    /// The first checkpoint frame whose hash differs.
    pub first_frame: Option<u32>,
    /// Keys whose last-frame value differs or that exist on one side only (at most the limit
    /// given to [`diff`]).
    pub keys: Vec<String>,
}

impl std::fmt::Display for TraceMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "scene {}: ", self.scene)?;
        if let Some(shape) = &self.shape {
            return write!(f, "{shape}");
        }
        match self.first_frame {
            Some(frame) => write!(f, "first differs at frame {frame}")?,
            None => write!(f, "checkpoints agree")?,
        }
        if !self.keys.is_empty() {
            write!(f, "; at the last frame: {}", self.keys.join(", "))?;
        }
        Ok(())
    }
}

/// `None` when `a` and `b` are identical, else what differs (at most `limit` keys).
pub fn diff(a: &Trace, b: &Trace, limit: usize) -> Option<TraceMismatch> {
    if a == b {
        return None;
    }
    let mut mismatch = TraceMismatch {
        scene: a.scene.clone(),
        shape: None,
        first_frame: None,
        keys: Vec::new(),
    };
    if a.scene != b.scene || a.frames != b.frames || a.every != b.every {
        mismatch.shape = Some(format!(
            "not comparable: {}/{} frames/every {} vs {}/{} frames/every {}",
            a.scene, a.frames, a.every, b.scene, b.frames, b.every
        ));
        return Some(mismatch);
    }
    mismatch.first_frame = a
        .checkpoints
        .iter()
        .zip(&b.checkpoints)
        .find(|(x, y)| x != y)
        .map(|(x, _)| x.0);
    let mut keys: Vec<String> = a
        .keys
        .iter()
        .filter(|(k, v)| b.keys.get(*k) != Some(*v))
        .map(|(k, _)| k.clone())
        .chain(b.keys.keys().filter(|k| !a.keys.contains_key(*k)).cloned())
        .collect();
    keys.sort();
    keys.dedup();
    keys.truncate(limit);
    mismatch.keys = keys;
    Some(mismatch)
}

/// The compute pool size this process was started with as a child of [`run_children`], or
/// `None` in a normal test run.
pub fn child_threads() -> Option<usize> {
    std::env::var(CHILD_ENV).ok()?.parse().ok()
}

/// Sizes the global compute pool to `threads` (before anything else creates it) and returns
/// the size it really has. A pool another crate created first keeps its size, which the parent
/// sees in the [`POOL_LINE`].
pub fn init_child_pool(threads: usize) -> usize {
    regia_core::tasks::init(TaskPoolOptions {
        compute_threads: Some(threads),
        ..TaskPoolOptions::default()
    })
    .compute
}

/// Prints the pool size, [`race_out_of_order`] and the traces for the parent to parse.
pub fn print_child_traces(pool_threads: usize, traces: &[Trace]) {
    // The leading newline ends libtest's `test det_matrix_child ... ` line (`--nocapture` prints
    // the output right after it), so the pool line starts a line of its own.
    let mut out = format!(
        "\n{POOL_LINE}{pool_threads}\n{RACE_LINE}{}\n",
        race_out_of_order()
    );
    for trace in traces {
        out.push_str(&trace.to_text());
    }
    println!("{out}");
}

/// One child process's result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildRun {
    /// The pool size asked for.
    pub threads: usize,
    /// The pool size the child reported.
    pub pool_threads: usize,
    /// The child's [`race_out_of_order`] count (informational: timing-dependent).
    pub race_out_of_order: Option<u64>,
    /// The child's traces, in the order it printed them.
    pub traces: Vec<Trace>,
}

/// Runs the test `test` (its exact name in this test binary) once per pool size in `threads`,
/// each in a child process of the current executable with [`CHILD_ENV`] set, and collects what
/// each child printed. An error names the pool size and carries the child's output tail.
pub fn run_children(test: &str, threads: &[usize]) -> Result<Vec<ChildRun>, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let mut runs = Vec::new();
    for &n in threads {
        let output = Command::new(&exe)
            .args([test, "--exact", "--nocapture", "--test-threads=1"])
            .env(CHILD_ENV, n.to_string())
            .output()
            .map_err(|e| format!("spawning the {n}-thread child: {e}"))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        if !output.status.success() {
            return Err(format!(
                "the {n}-thread child failed ({}):\n{}\n{}",
                output.status,
                tail(&stdout),
                tail(&String::from_utf8_lossy(&output.stderr))
            ));
        }
        let pool_threads = stdout
            .lines()
            .find_map(|line| line.trim_end().strip_prefix(POOL_LINE))
            .and_then(|n| n.parse().ok())
            .ok_or_else(|| format!("the {n}-thread child printed no pool line:\n{}", tail(&stdout)))?;
        let traces = Trace::parse_all(&stdout).map_err(|e| format!("the {n}-thread child: {e}"))?;
        let race_out_of_order = stdout
            .lines()
            .find_map(|line| line.trim_end().strip_prefix(RACE_LINE))
            .and_then(|n| n.parse().ok());
        runs.push(ChildRun {
            threads: n,
            pool_threads,
            race_out_of_order,
            traces,
        });
    }
    Ok(runs)
}

/// The last 30 lines of `text`.
fn tail(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(30)..].join("\n")
}

/// What [`check_pinned`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinnedOutcome {
    /// The pinned trace and this run agree.
    Matches,
    /// `REGIA_BLESS=1`: the trace was written to this path.
    Blessed(PathBuf),
    /// No trace is pinned for this target at this path.
    Missing(PathBuf),
    /// The pinned trace and this run differ.
    Differs(TraceMismatch),
    /// Not compared: `REGIA_SKIP_GOLDENS=1` (the real-GPU pass, which skips every pinned
    /// reference blessed on the reference machine).
    Skipped,
}

/// `<scene>.<arch>-<os>.trace` in `dir`: a trace is only comparable on the same target, because
/// the platform math library and rapier's float reductions differ between targets.
pub fn pinned_path(dir: &Path, scene: &str) -> PathBuf {
    dir.join(format!(
        "{scene}.{}-{}.trace",
        std::env::consts::ARCH,
        std::env::consts::OS
    ))
}

/// The build profile of this binary, for messages and the pinned file's comment.
pub fn build_profile() -> &'static str {
    if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    }
}

/// Compares `trace` with the one pinned in `dir` for this target, or writes it there when
/// `REGIA_BLESS=1`. With `REGIA_SKIP_GOLDENS=1` (and no bless) it compares nothing, like a
/// golden image.
pub fn check_pinned(dir: &Path, trace: &Trace) -> std::io::Result<PinnedOutcome> {
    let path = pinned_path(dir, &trace.scene);
    let bless = std::env::var("REGIA_BLESS").as_deref() == Ok("1");
    if crate::golden::goldens_skipped() && !bless {
        return Ok(PinnedOutcome::Skipped);
    }
    if bless {
        std::fs::create_dir_all(dir)?;
        let text = format!(
            "# Pinned determinism trace (ADR-1347): written by a {} build with REGIA_BLESS=1.\n{}",
            build_profile(),
            trace.to_text()
        );
        crate::write_atomic(&path, text.as_bytes())?;
        return Ok(PinnedOutcome::Blessed(path));
    }
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(PinnedOutcome::Missing(path)),
        Err(e) => return Err(e),
    };
    let pinned = Trace::parse(&text).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    Ok(match diff(&pinned, trace, 12) {
        None => PinnedOutcome::Matches,
        Some(mismatch) => PinnedOutcome::Differs(mismatch),
    })
}

// ---------------------------------------------------------------------------
// The spawn-race scene
// ---------------------------------------------------------------------------

/// Fixed steps the race scene has run.
#[derive(Resource, Default, Debug)]
struct RaceClock(u32);

/// Turns taken by the spawner lanes this step, in the order they ran (reset by `race_tick`).
/// Read through `Res`, so the lanes still run in parallel.
#[derive(Resource, Default, Debug)]
struct RaceTurn(AtomicU32);

/// Steps on which a spawner lane ran before a lower-numbered lane that spawned on the same
/// step, in every race scene of this process.
static RACE_OUT_OF_ORDER: AtomicU64 = AtomicU64::new(0);

/// How many times, in this process, a race-scene spawner lane ran before a lower-numbered lane
/// spawning on the same step: then the lanes reserved their entity ids out of the order their
/// commands apply in, which is the situation ADR-1347 fixes. Always 0 on the single-threaded
/// executor; on the multi-threaded one it depends on timing, so it is evidence, not a check.
pub fn race_out_of_order() -> u64 {
    RACE_OUT_OF_ORDER.load(Ordering::Relaxed)
}

/// Whether lane `lane` spawns on `step`: every fourth step (offset by the lane) and every sixth.
fn lane_spawns(step: u32, lane: u32) -> bool {
    step <= RACE_SPAWN_STEPS && ((step + lane).is_multiple_of(4) || step.is_multiple_of(6))
}

/// A box of the race scene: the spawner lane and the step it was born on.
#[derive(Component, Reflect, Default, Debug, Clone, Copy, PartialEq, Eq)]
#[reflect(Component, Default)]
struct RaceBox {
    lane: u32,
    born: u32,
}

/// Spawners stop after this many steps, so the pile settles.
const RACE_SPAWN_STEPS: u32 = 180;
/// A box lives this many steps; then the reaper despawns it and its id goes back to the pool.
const RACE_LIFETIME: u32 = 75;
/// The race scene's seed.
const RACE_SEED: u64 = 0x5eed_1347;

fn race_tick(mut clock: ResMut<RaceClock>, mut turn: ResMut<RaceTurn>) {
    clock.0 += 1;
    *turn.0.get_mut() = 0;
}

/// Lane `LANE` drops a box on every fourth step (offset by the lane) and on every sixth step,
/// when all three lanes spawn at once.
fn race_spawn<const LANE: u32>(clock: Res<RaceClock>, turn: Res<RaceTurn>, mut commands: Commands) {
    let step = clock.0;
    if !lane_spawns(step, LANE) {
        return;
    }
    let taken = turn.0.fetch_add(1, Ordering::Relaxed);
    let lower = (0..LANE).filter(|lane| lane_spawns(step, *lane)).count();
    if taken as usize != lower {
        RACE_OUT_OF_ORDER.fetch_add(1, Ordering::Relaxed);
    }
    let mut rng = Rng::with_stream(RACE_SEED ^ u64::from(step), u64::from(LANE));
    let x = (rng.f32() - 0.5) * 1.2 + (LANE as f32 - 1.0) * 0.3;
    let z = (rng.f32() - 0.5) * 1.2;
    let label = format!("race{LANE}-{step}");
    commands.spawn((
        Name::new(label.as_str()),
        RaceBox {
            lane: LANE,
            born: step,
        },
        Transform::from_translation(Vec3::new(x, 2.5 + LANE as f32 * 0.6, z)),
        RigidBody::dynamic(),
        Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::new(0.2, 0.15, 0.25),
            },
            ..Collider::default()
        },
        Velocity::default(),
    ));
}

/// Despawns boxes older than `RACE_LIFETIME`, in query order.
fn race_reap(clock: Res<RaceClock>, boxes: Query<(Entity, &RaceBox)>, mut commands: Commands) {
    for (entity, race_box) in &boxes {
        if clock.0.saturating_sub(race_box.born) > RACE_LIFETIME {
            commands.entity(entity).despawn();
        }
    }
}

/// The spawn-race scene: a walled floor, three spawner systems and a reaper with no order
/// between them (all before the physics step), 16 ms frames with one fixed step each.
///
/// Spawning through `Commands` reserves entity ids when the command is recorded, so on the
/// multi-threaded executor the lanes get their ids (and reuse the reaped ones) in
/// thread-timing order, while the commands apply in system order. A simulation that orders
/// anything by entity id drifts here between pool sizes and runs.
pub fn race_app() -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
    let mut fixed = FixedTime::default();
    fixed.hz = 62.5;
    app.insert_resource(fixed);
    app.add_plugins((
        MinimalPlugins,
        regia_scene::ScenePlugin,
        regia_physics::PhysicsPlugin,
    ));
    app.init_resource::<RaceClock>()
        .init_resource::<RaceTurn>()
        .register_type::<RaceBox>();
    app.add_systems(
        FixedUpdate,
        (
            race_tick,
            (race_spawn::<0>, race_spawn::<1>, race_spawn::<2>, race_reap)
                .after(race_tick)
                .before(PhysicsStep),
        ),
    );
    let wall = |name: &str, at: Vec3, half: Vec3| {
        (
            Name::new(name),
            Transform::from_translation(at),
            RigidBody::static_body(),
            Collider {
                shape: ColliderShape::Box { half_extents: half },
                ..Collider::default()
            },
        )
    };
    let world = app.world_mut();
    world.spawn(wall("floor", Vec3::new(0.0, -0.5, 0.0), Vec3::new(3.0, 0.5, 3.0)));
    world.spawn(wall(
        "wall_w",
        Vec3::new(-1.5, 1.0, 0.0),
        Vec3::new(0.1, 1.0, 1.5),
    ));
    world.spawn(wall("wall_e", Vec3::new(1.5, 1.0, 0.0), Vec3::new(0.1, 1.0, 1.5)));
    world.spawn(wall(
        "wall_n",
        Vec3::new(0.0, 1.0, -1.5),
        Vec3::new(1.5, 1.0, 0.1),
    ));
    world.spawn(wall("wall_s", Vec3::new(0.0, 1.0, 1.5), Vec3::new(1.5, 1.0, 0.1)));
    app
}

// ---------------------------------------------------------------------------
// The ragdoll scene
// ---------------------------------------------------------------------------

/// A 17-joint humanoid in a T-pose, hips 1 m above the rig origin.
fn humanoid() -> regia_animation::Skeleton {
    const NAMES: [&str; 17] = [
        "hips",
        "spine",
        "chest",
        "neck",
        "head",
        "upperarm_l",
        "lowerarm_l",
        "hand_l",
        "upperarm_r",
        "lowerarm_r",
        "hand_r",
        "thigh_l",
        "calf_l",
        "foot_l",
        "thigh_r",
        "calf_r",
        "foot_r",
    ];
    let parents: Vec<Option<u32>> = vec![
        None,
        Some(0),
        Some(1),
        Some(2),
        Some(3),
        Some(2),
        Some(5),
        Some(6),
        Some(2),
        Some(8),
        Some(9),
        Some(0),
        Some(11),
        Some(12),
        Some(0),
        Some(14),
        Some(15),
    ];
    let t = |x: f32, y: f32| regia_animation::pose::translated(Vec3::new(x, y, 0.0));
    let binds = vec![
        t(0.0, 1.0),
        t(0.0, 0.15),
        t(0.0, 0.25),
        t(0.0, 0.25),
        t(0.0, 0.1),
        t(0.2, 0.2),
        t(0.3, 0.0),
        t(0.27, 0.0),
        t(-0.2, 0.2),
        t(-0.3, 0.0),
        t(-0.27, 0.0),
        t(0.1, -0.05),
        t(0.0, -0.45),
        t(0.0, -0.42),
        t(-0.1, -0.05),
        t(0.0, -0.45),
        t(0.0, -0.42),
    ];
    regia_animation::Skeleton::from_local_bind(
        NAMES.iter().map(|n| (*n).to_string()).collect(),
        parents,
        binds,
    )
    .unwrap_or_else(|_| unreachable!("the humanoid skeleton is valid"))
}

/// The ragdoll scene: three limp humanoid ragdolls (capsules on joints with limits) dropped on
/// a floor and on each other, 16 ms frames with one fixed step each.
pub fn ragdoll_app() -> App {
    use regia_animation::{AnimationClip, AnimationPlugin, Animator};
    use regia_framework::ragdoll::{Ragdoll, RagdollMode, RagdollPlugin, RagdollProfile};

    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
    let mut fixed = FixedTime::default();
    fixed.hz = 62.5;
    app.insert_resource(fixed);
    app.add_plugins((
        MinimalPlugins,
        regia_scene::ScenePlugin,
        regia_physics::PhysicsPlugin,
        AnimationPlugin,
        RagdollPlugin,
    ));
    app.world_mut().spawn((
        Name::new("floor"),
        Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
        RigidBody::static_body(),
        Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::new(20.0, 0.5, 20.0),
            },
            ..Collider::default()
        },
    ));
    let skeleton = std::sync::Arc::new(humanoid());
    for i in 0..3u8 {
        let mut ragdoll = Ragdoll::new(RagdollProfile::humanoid(&skeleton, 75.0));
        ragdoll.mode = RagdollMode::Limp;
        let binding =
            regia_animation::components::spawn_skeleton(app.world_mut(), std::sync::Arc::clone(&skeleton));
        let f = f32::from(i);
        let name = format!("ragdoll{i}");
        app.world_mut().spawn((
            Name::new(name.as_str()),
            Transform {
                translation: Vec3::new(0.25 * f, 0.4 + 0.9 * f, 0.1 * f),
                rotation: regia_core::math::Quat::from_rotation_y(0.7 * f),
                scale: Vec3::ONE,
            },
            binding,
            Animator::single(AnimationClip::empty("idle", 1.0), true),
            ragdoll,
        ));
    }
    app
}

/// Builds one scene of the matrix.
pub type SceneBuilder = fn() -> App;

/// The scenes of the matrix: `(name, builder)`. `smoke` is [`crate::shuffle::smoke_app`]
/// (bodies, a character, spring bones, a clip and a socket), `race` is [`race_app`] and
/// `ragdoll` is [`ragdoll_app`].
pub fn scenes() -> Vec<(&'static str, SceneBuilder)> {
    vec![
        ("smoke", crate::shuffle::smoke_app),
        ("race", race_app),
        ("ragdoll", ragdoll_app),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Trace {
        Trace {
            scene: "demo".into(),
            frames: 40,
            every: 20,
            checkpoints: vec![(20, 0xabc), (40, 0xdef)],
            keys: [
                ("hero/Transform".to_string(), 1),
                ("a box/Velocity".to_string(), 2),
            ]
            .into_iter()
            .collect(),
        }
    }

    #[test]
    fn text_round_trips_and_skips_noise_around_it() {
        let trace = sample();
        let noisy = format!(
            "running 1 test\n{POOL_LINE}4\n{}test child ... ok\n",
            trace.to_text()
        );
        assert_eq!(Trace::parse(&noisy), Ok(trace.clone()));
        let two = format!("{}{}", trace.to_text(), trace.to_text());
        assert_eq!(Trace::parse_all(&two).map(|t| t.len()), Ok(2));
        let commented = format!("# a comment\n{}", trace.to_text());
        assert_eq!(Trace::parse(&commented), Ok(trace));
    }

    #[test]
    fn cut_off_and_malformed_traces_are_errors() {
        let text = sample().to_text();
        let cut = text.replace("end\n", "");
        assert!(Trace::parse(&cut).is_err());
        let bad = text.replace("cp 20 0000000000000abc", "cp twenty abc");
        assert!(Trace::parse(&bad).is_err());
        let unknown = text.replace("every 20", "sometimes 20");
        assert!(Trace::parse(&unknown).is_err());
        assert!(Trace::parse("no trace here").is_err());
    }

    #[test]
    fn diff_names_the_first_frame_and_the_keys() {
        let a = sample();
        assert_eq!(diff(&a, &a, 10), None);
        let mut b = a.clone();
        b.checkpoints[1].1 = 0x123;
        b.keys.insert("hero/Transform".into(), 9);
        b.keys.insert("ghost/Name".into(), 3);
        let mismatch = diff(&a, &b, 10).expect("differs");
        assert_eq!(mismatch.first_frame, Some(40));
        assert_eq!(
            mismatch.keys,
            vec!["ghost/Name".to_string(), "hero/Transform".to_string()]
        );
        assert!(mismatch.to_string().contains("frame 40"), "{mismatch}");
        let mut c = a.clone();
        c.frames = 41;
        assert!(diff(&a, &c, 10).and_then(|m| m.shape).is_some());
    }

    #[test]
    fn pinned_files_are_per_target_and_bless_round_trips() {
        let dir = crate::unique_temp_dir("det-matrix-pinned");
        let path = pinned_path(&dir, "demo");
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        assert!(name.starts_with("demo.") && name.ends_with(".trace"), "{name}");
        assert!(name.contains(std::env::consts::OS), "{name}");
        // Neither REGIA_BLESS nor REGIA_SKIP_GOLDENS is set in a normal run: a missing file
        // is reported, not written.
        let bless = std::env::var("REGIA_BLESS").as_deref() == Ok("1");
        if !bless && !crate::golden::goldens_skipped() {
            assert_eq!(
                check_pinned(&dir, &sample()).ok(),
                Some(PinnedOutcome::Missing(path.clone()))
            );
            let text = format!("# comment\n{}", sample().to_text());
            crate::write_atomic(&path, text.as_bytes()).expect("write the pinned trace");
            assert_eq!(check_pinned(&dir, &sample()).ok(), Some(PinnedOutcome::Matches));
            let mut other = sample();
            other.checkpoints[0].1 = 7;
            assert!(matches!(
                check_pinned(&dir, &other),
                Ok(PinnedOutcome::Differs(m)) if m.first_frame == Some(20)
            ));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
