// SPDX-License-Identifier: MPL-2.0
//! Record/replay determinism stress harness (ADR-537, `book/agents/determinism.md`).
//!
//! For each scene and seed, [`record_replay`] runs the scene twice from a fresh `App`:
//!
//! 1. **Record.** The scene's `drive` function feeds seeded input (a pure function of the seed and
//!    the step, see [`input_noise`]) through [`InputInjection`] while
//!    [`InputRecorder`] records it; the world's state ([`scene_snapshot`]) is kept at every
//!    checkpoint.
//! 2. **Replay.** A second app built from the same seed plays the recording back (no `drive`), and
//!    its state is hashed at the same checkpoints.
//!
//! A run is clean when every checkpoint hash matches. The first checkpoint that differs is the
//! [`Divergence`]: its step and the `<entity>/<Component>` keys whose value changed, with short
//! excerpts of both values, which names the system to look at.
//!
//! Both runs use the engine's default (multi-threaded) executor, so a system pair with no order
//! between it that races shows up here as a divergence that comes and goes. The scenes run one fixed
//! step per frame with a manual clock; a scene that runs another number of steps is reported as an
//! error, not compared.
//!
//! [`stress`] runs every scene under every seed and gathers a [`ReproReport`]; [`physics_pile`] and
//! [`character_rig`] are the two engine scenes (`samples/arena` adds the bot match as the third).

use std::fmt::Write as _;

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_framework::character::{
    Character, CharacterInput, CharacterMovement, CharacterPlugin, CharacterState,
    use_rapier_character_controller,
};
use regia_framework::input_recording::{
    InputRecorder, InputRecordingPlugin, InputRecordingSet, RecorderMode,
};
use regia_framework::state_hash::{differing_keys, snapshot_hash, world_snapshot};
use regia_input::{ActionKind, Actions, InputConfig, InputInjection, InputPlugin};
use regia_physics::{CharacterBody, Collider, ColliderShape, RigidBody, Velocity};
use regia_scene::Transform;
use regia_scene::components::Name;

use crate::shuffle::Snapshot;

/// Longest value excerpt a [`Divergence`] keeps per side.
const EXCERPT_CHARS: usize = 160;
/// Most differing keys a [`Divergence`] lists.
const DIVERGENCE_KEYS: usize = 12;

/// One scene of the stress run.
#[derive(Clone, Copy)]
pub struct ReproScene {
    /// Short stable name (`physics_pile`, `character_rig`, `arena_match`).
    pub name: &'static str,
    /// Builds the app for a seed: plugins (including [`InputRecordingPlugin`] and an input layer
    /// declaring [`actions`](Self::actions)), entities and a fixed step of one step per frame. It
    /// must not run a frame.
    pub build: fn(u64) -> App,
    /// The player-0 actions recorded and replayed.
    pub actions: &'static [&'static str],
    /// Called before each frame of the **recording** run only, with the seed and the step about
    /// to run: injects that step's input (use [`inject_next_frame`] and [`input_noise`]).
    pub drive: fn(&mut App, u64, u32),
    /// Adds scene state that is not a reflected component (a resource, an RNG) to a snapshot.
    pub extra: Option<fn(&World, &mut Snapshot)>,
    /// Component short names left out of the comparison: values that are random or wall-clock by
    /// design (an `EntityGuid`, a timestamp) and are not simulation state.
    pub ignore: &'static [&'static str],
}

impl std::fmt::Debug for ReproScene {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReproScene")
            .field("name", &self.name)
            .field("actions", &self.actions)
            .field("ignore", &self.ignore)
            .finish_non_exhaustive()
    }
}

/// How long each run is and how often it is compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReproConfig {
    /// Frames (and fixed steps) per run.
    pub steps: u32,
    /// A checkpoint every this many steps (and always after the last step).
    pub checkpoint_every: u32,
}

impl Default for ReproConfig {
    fn default() -> Self {
        Self {
            steps: 240,
            checkpoint_every: 20,
        }
    }
}

impl ReproConfig {
    /// Whether the state after step `step` (0-based) is compared.
    #[must_use]
    pub fn is_checkpoint(&self, step: u32) -> bool {
        let done = step + 1;
        done == self.steps || done.is_multiple_of(self.checkpoint_every.max(1))
    }
}

/// The first checkpoint where the replay's state differs from the recording's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Divergence {
    /// Steps run when the states were compared.
    pub step: u32,
    /// The `<entity>/<Component>` keys whose value differs (at most 12).
    pub keys: Vec<String>,
    /// For the first keys: `(key, recorded excerpt, replayed excerpt)`; `<absent>` when the key is
    /// missing on one side.
    pub excerpts: Vec<(String, String, String)>,
}

/// One scene under one seed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReproRun {
    /// The scene name.
    pub scene: &'static str,
    /// The seed.
    pub seed: u64,
    /// `(steps run, state hash)` per checkpoint of the recording run.
    pub recorded: Vec<(u32, u64)>,
    /// The same for the replay run.
    pub replayed: Vec<(u32, u64)>,
    /// Snapshot keys at the last checkpoint of the recording.
    pub values: usize,
    /// The first checkpoint that differs, if any.
    pub divergence: Option<Divergence>,
    /// Why the run could not be compared (no recorder, wrong step count), if it could not.
    pub error: Option<String>,
}

impl ReproRun {
    /// Compared, and every checkpoint matched.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.error.is_none() && self.divergence.is_none() && self.recorded == self.replayed
    }
}

/// Every run of a [`stress`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReproReport {
    /// The run length and checkpoint spacing.
    pub config: ReproConfig,
    /// Every scene × seed, scene-major.
    pub runs: Vec<ReproRun>,
}

impl ReproReport {
    /// Runs that diverged or could not be compared.
    #[must_use]
    pub fn failures(&self) -> Vec<&ReproRun> {
        self.runs.iter().filter(|run| !run.is_clean()).collect()
    }

    /// True when every run is clean.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.failures().is_empty()
    }

    /// A markdown summary: one line per scene, then every failing run with its divergence.
    #[must_use]
    pub fn to_markdown(&self) -> String {
        let mut out = format!(
            "repro-check: {} runs, {} steps each, a checkpoint every {} steps\n",
            self.runs.len(),
            self.config.steps,
            self.config.checkpoint_every
        );
        let mut scenes: Vec<&str> = self.runs.iter().map(|run| run.scene).collect();
        scenes.dedup();
        for scene in scenes {
            let runs: Vec<&ReproRun> = self.runs.iter().filter(|run| run.scene == scene).collect();
            let clean = runs.iter().filter(|run| run.is_clean()).count();
            let values = runs.first().map_or(0, |run| run.values);
            let _ = writeln!(
                out,
                "- {scene}: {clean} of {} seeds replay identically ({values} values hashed)",
                runs.len()
            );
        }
        for run in self.failures() {
            let _ = write!(out, "  - {} seed {}: ", run.scene, run.seed);
            if let Some(error) = &run.error {
                let _ = writeln!(out, "ERROR {error}");
                continue;
            }
            let Some(divergence) = &run.divergence else {
                let _ = writeln!(out, "hash lists differ");
                continue;
            };
            let _ = writeln!(
                out,
                "diverges by step {}: {}",
                divergence.step,
                divergence.keys.join(", ")
            );
            for (key, recorded, replayed) in &divergence.excerpts {
                let _ = writeln!(
                    out,
                    "    - {key}\n      recorded {recorded}\n      replayed {replayed}"
                );
            }
        }
        out
    }
}

/// A value in `[-1, 1]` that depends only on `(seed, step, lane)`: SplitMix64 over the three,
/// so recording input needs no RNG state.
#[must_use]
pub fn input_noise(seed: u64, step: u32, lane: u32) -> f32 {
    let mut z = seed
        .wrapping_add(u64::from(step).wrapping_mul(0x9E37_79B9_7F4A_7C15))
        .wrapping_add(u64::from(lane).wrapping_mul(0xD1B5_4A32_D192_ED03));
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    // The top 24 bits as a fraction: exact in f32.
    ((z >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
}

/// Schedules `value` for `action` on the frame the next [`App::update`] runs.
pub fn inject_next_frame(app: &mut App, action: &str, value: Vec3) {
    let frame = app.world().get_resource::<Time>().map_or(0, Time::frame_count) + 1;
    if let Some(mut injection) = app.world_mut().get_resource_mut::<InputInjection>() {
        injection.schedule(action, value, frame, 1);
    }
}

/// The compared state of a scene's world: [`world_snapshot`], plus the scene's `extra` entries,
/// minus its `ignore`d components.
#[must_use]
pub fn scene_snapshot(scene: &ReproScene, world: &World) -> Snapshot {
    let mut snapshot = world_snapshot(world);
    if let Some(extra) = scene.extra {
        extra(world, &mut snapshot);
    }
    if !scene.ignore.is_empty() {
        snapshot.retain(|key, _| {
            let component = key.rsplit('/').next().unwrap_or(key);
            !scene.ignore.contains(&component)
        });
    }
    snapshot
}

fn excerpt(value: Option<&String>) -> String {
    match value {
        None => "<absent>".to_string(),
        Some(text) if text.chars().count() > EXCERPT_CHARS => {
            let cut: String = text.chars().take(EXCERPT_CHARS).collect();
            format!("{cut}…")
        }
        Some(text) => text.clone(),
    }
}

fn failed(scene: &ReproScene, seed: u64, error: String) -> ReproRun {
    ReproRun {
        scene: scene.name,
        seed,
        recorded: Vec::new(),
        replayed: Vec::new(),
        values: 0,
        divergence: None,
        error: Some(error),
    }
}

/// Records `scene` under `seed` for `config.steps` frames, replays the recording in a fresh app
/// and compares the two runs' state at every checkpoint (see the module docs).
#[must_use]
pub fn record_replay(scene: &ReproScene, seed: u64, config: &ReproConfig) -> ReproRun {
    let actions: Vec<String> = scene.actions.iter().map(|a| (*a).to_string()).collect();

    // Record.
    let mut app = (scene.build)(seed);
    match app.world_mut().get_resource_mut::<InputRecorder>() {
        Some(mut recorder) => recorder.record_with(seed, actions),
        None => return failed(scene, seed, "the scene has no InputRecordingPlugin".into()),
    }
    let mut checkpoints: Vec<(u32, Snapshot)> = Vec::new();
    for step in 0..config.steps {
        (scene.drive)(&mut app, seed, step);
        app.update();
        if config.is_checkpoint(step) {
            checkpoints.push((step + 1, scene_snapshot(scene, app.world())));
        }
    }
    let recording = match app.world_mut().get_resource_mut::<InputRecorder>() {
        Some(mut recorder) => {
            recorder.stop();
            recorder.recording.clone()
        }
        None => return failed(scene, seed, "the recorder resource disappeared".into()),
    };
    if recording.len() != config.steps as usize {
        return failed(
            scene,
            seed,
            format!(
                "the recording has {} fixed steps for {} frames; the scene must run one fixed step per frame",
                recording.len(),
                config.steps
            ),
        );
    }
    drop(app);

    // Replay.
    let mut app = (scene.build)(recording.seed);
    match app.world_mut().get_resource_mut::<InputRecorder>() {
        Some(mut recorder) => recorder.play(recording),
        None => return failed(scene, seed, "the replay app has no InputRecordingPlugin".into()),
    }
    let mut replayed = Vec::new();
    let mut divergence = None;
    let mut expected = checkpoints.iter();
    for step in 0..config.steps {
        app.update();
        if !config.is_checkpoint(step) {
            continue;
        }
        let snapshot = scene_snapshot(scene, app.world());
        replayed.push((step + 1, snapshot_hash(&snapshot)));
        let Some((_, recorded)) = expected.next() else {
            continue;
        };
        if divergence.is_none() && recorded != &snapshot {
            let keys = differing_keys(recorded, &snapshot, DIVERGENCE_KEYS);
            let excerpts = keys
                .iter()
                .take(4)
                .map(|key| {
                    (
                        key.clone(),
                        excerpt(recorded.get(key)),
                        excerpt(snapshot.get(key)),
                    )
                })
                .collect();
            divergence = Some(Divergence {
                step: step + 1,
                keys,
                excerpts,
            });
        }
    }
    let still_playing = app
        .world()
        .get_resource::<InputRecorder>()
        .is_some_and(|recorder| recorder.mode == RecorderMode::Playing);
    ReproRun {
        scene: scene.name,
        seed,
        recorded: checkpoints
            .iter()
            .map(|(step, snapshot)| (*step, snapshot_hash(snapshot)))
            .collect(),
        replayed,
        values: checkpoints.last().map_or(0, |(_, snapshot)| snapshot.len()),
        divergence,
        error: still_playing.then(|| "the replay was still playing after the recorded frames".to_string()),
    }
}

/// Runs [`record_replay`] for every scene under every seed (scene-major).
#[must_use]
pub fn stress(scenes: &[ReproScene], seeds: &[u64], config: &ReproConfig) -> ReproReport {
    let mut runs = Vec::with_capacity(scenes.len() * seeds.len());
    for scene in scenes {
        for &seed in seeds {
            runs.push(record_replay(scene, seed, config));
        }
    }
    ReproReport {
        config: *config,
        runs,
    }
}

/// `count` seeds starting at `first`, spread so neighbouring seeds share no low bits.
#[must_use]
pub fn seeds(first: u64, count: usize) -> Vec<u64> {
    (0..count as u64)
        .map(|i| first.wrapping_add(i.wrapping_mul(0x9E37_79B9_7F4A_7C15)))
        .collect()
}

// ---------------------------------------------------------------------------
// Engine scenes
// ---------------------------------------------------------------------------

/// The actions both engine scenes declare.
const HERO_ACTIONS: &[&str] = &["Move", "Jump"];

/// The input layer of the engine scenes.
fn hero_input() -> InputPlugin {
    InputPlugin::new(
        InputConfig::new()
            .action("Move", ActionKind::Axis2D)
            .action("Jump", ActionKind::Button),
    )
}

/// `FixedUpdate`, after the recorder and before physics: the hero's movement input from the
/// player-0 actions.
fn drive_hero(actions: Res<Actions>, mut heroes: Query<&mut CharacterInput, With<Character>>) {
    let axis = actions.value("Move");
    let jump = actions.value("Jump").x > 0.5;
    for mut input in &mut heroes {
        input.move_dir = Vec3::new(axis.x, 0.0, -axis.y).clamp_length_max(1.0);
        input.jump = jump;
    }
}

fn add_hero_input(app: &mut App) {
    app.add_plugins((hero_input(), InputRecordingPlugin));
    app.add_systems(
        FixedUpdate,
        drive_hero
            .after(InputRecordingSet)
            .before(regia_physics::PhysicsStep),
    );
}

/// Seeded hero input: a new `Move` heading every 20 steps and a jump press every 45 steps (from
/// a seed-dependent phase).
fn drive_hero_input(app: &mut App, seed: u64, step: u32) {
    let leg = step / 20;
    let axis = Vec3::new(input_noise(seed, leg, 0), input_noise(seed, leg, 1), 0.0);
    inject_next_frame(app, "Move", axis);
    let phase = (input_noise(seed, 0, 2) * 0.5 + 0.5) * 44.0;
    if (step + phase as u32).is_multiple_of(45) {
        inject_next_frame(app, "Jump", Vec3::X);
    }
}

fn hero_bundle(at: Vec3) -> impl Bundle {
    (
        Transform::from_translation(at),
        Character,
        CharacterMovement::default(),
        CharacterInput::default(),
        CharacterState::default(),
        CharacterBody::default(),
    )
}

/// Builds the [`physics_pile`] app: a walled floor, 24 seeded boxes and spheres dropped with
/// seeded velocities so they collide and settle, and a hero character walking into them.
#[must_use]
pub fn physics_pile_app(seed: u64) -> App {
    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        regia_scene::ScenePlugin,
        regia_physics::PhysicsPlugin,
        CharacterPlugin,
    ));
    use_rapier_character_controller(&mut app);
    add_hero_input(&mut app);
    enable_fixed_step(&mut app, 60.0);

    let wall = |half_extents: Vec3| Collider {
        shape: ColliderShape::Box { half_extents },
        ..Collider::default()
    };
    let world = app.world_mut();
    world.spawn((
        Name::new("floor"),
        Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
        RigidBody::static_body(),
        wall(Vec3::new(12.0, 0.5, 12.0)),
    ));
    for (i, (x, z, half)) in [
        (12.0, 0.0, Vec3::new(0.5, 2.0, 12.0)),
        (-12.0, 0.0, Vec3::new(0.5, 2.0, 12.0)),
        (0.0, 12.0, Vec3::new(12.0, 2.0, 0.5)),
        (0.0, -12.0, Vec3::new(12.0, 2.0, 0.5)),
    ]
    .into_iter()
    .enumerate()
    {
        world.spawn((
            Name::new(format!("wall{i}")),
            Transform::from_translation(Vec3::new(x, 2.0, z)),
            RigidBody::static_body(),
            wall(half),
        ));
    }
    for i in 0..24u32 {
        let n = |lane: u32| input_noise(seed, i, 10 + lane);
        let at = Vec3::new(n(0) * 4.0, 1.0 + i as f32 * 0.6, n(1) * 4.0);
        let shape = if i.is_multiple_of(3) {
            ColliderShape::Sphere {
                radius: 0.3 + 0.1 * (n(2) * 0.5 + 0.5),
            }
        } else {
            ColliderShape::Box {
                half_extents: Vec3::new(0.25, 0.2 + 0.1 * (n(2) * 0.5 + 0.5), 0.3),
            }
        };
        world.spawn((
            Name::new(format!("body{i:02}")),
            Transform::from_translation(at),
            RigidBody::dynamic(),
            Collider {
                shape,
                ..Collider::default()
            },
            Velocity {
                linear: Vec3::new(n(3) * 3.0, n(4), n(5) * 3.0),
                angular: Vec3::new(n(6), n(7), n(8)) * 2.0,
            },
        ));
    }
    world.spawn((Name::new("hero"), hero_bundle(Vec3::new(-6.0, 1.3, 0.0))));
    app
}

/// Rigid bodies, contacts and the character controller under seeded drops and seeded input.
#[must_use]
pub fn physics_pile() -> ReproScene {
    ReproScene {
        name: "physics_pile",
        build: physics_pile_app,
        actions: HERO_ACTIONS,
        drive: drive_hero_input,
        extra: None,
        ignore: &[],
    }
}

/// Builds the [`character_rig`] app: the schedule-shuffle smoke scene
/// ([`crate::shuffle::smoke_app`]: falling boxes, a character, spring bones on a rope and a
/// clip-driven arm with a socket) with the hero driven by recorded input. The seed moves the
/// hero's start.
#[must_use]
pub fn character_rig_app(seed: u64) -> App {
    let mut app = crate::shuffle::smoke_app();
    add_hero_input(&mut app);
    let start = Vec3::new(
        -3.0 + input_noise(seed, 0, 20),
        1.3,
        input_noise(seed, 0, 21) * 2.0,
    );
    let world = app.world_mut();
    let mut heroes = world.query_filtered::<&mut Transform, With<Character>>();
    for mut transform in heroes.iter_mut(world) {
        transform.translation = start;
    }
    app
}

/// Character movement, spring bones, clip playback and sockets under seeded input.
#[must_use]
pub fn character_rig() -> ReproScene {
    ReproScene {
        name: "character_rig",
        build: character_rig_app,
        actions: HERO_ACTIONS,
        drive: drive_hero_input,
        extra: None,
        ignore: &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_noise_is_a_pure_function_in_range() {
        for step in 0..200 {
            let a = input_noise(7, step, 1);
            assert_eq!(a, input_noise(7, step, 1));
            assert!((-1.0..=1.0).contains(&a), "{a}");
        }
        assert_ne!(input_noise(7, 3, 0), input_noise(8, 3, 0));
        assert_ne!(input_noise(7, 3, 0), input_noise(7, 3, 1));
    }

    #[test]
    fn checkpoints_land_on_the_spacing_and_the_last_step() {
        let config = ReproConfig {
            steps: 45,
            checkpoint_every: 20,
        };
        let at: Vec<u32> = (0..config.steps).filter(|s| config.is_checkpoint(*s)).collect();
        assert_eq!(at, vec![19, 39, 44]);
    }

    #[test]
    fn seeds_are_distinct() {
        let list = seeds(1, 20);
        let mut sorted = list.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 20);
    }
}
