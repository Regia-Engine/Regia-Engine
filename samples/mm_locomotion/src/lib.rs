// SPDX-License-Identifier: MIT-0
//! Motion-matching locomotion sample (`docs/specs/52-motion-matching.md`, ADR-2409).
//!
//! One character walks a scripted course: stand, walk, turn right, run, turn left, stop, then
//! the course repeats. Its clips are made in code
//! (`regia_animation::motion_matching::procedural`): no files, no binary assets. The database is
//! built from the same `.motiondb.ron` source a cook would read and registered in
//! `MotionDatabases` at [`DATABASE_PATH`]; the character names it with a `MotionDatabaseRef`, so
//! the plugin resolves it the way a scene's reference to a cooked database resolves. Motion
//! matching plays through the character's `Animator` (`AnimatorMotionMatch`), and the
//! character moves by the root motion the animator reports.
//!
//! The course does not write trajectories by hand (ADR-2858): each leg sets a desired speed and
//! turn rate, the course turns them into a desired world velocity, and the hero's
//! `TrajectoryPredictor` (critically damped springs, closed form) predicts the path the matcher
//! searches with.
//!
//! Agent surface: everything in `book/agents/motion-matching.md` (`anim.motion_match`,
//! `anim.motion_match_set`, `anim.motion_match_database { database: "procedural/locomotion.motiondb.ron" }`,
//! `anim.motion_match_databases`, `anim.mm_predict`), with the hero as the entity.
//! `tests/agent_dogfood.rs` drives it over the real protocol.

use std::sync::Arc;
use std::time::Duration;

use regia_animation::components::{Animator, RootMotionOutput, spawn_skeleton};
use regia_animation::motion_matching::WeightsSource;
use regia_animation::motion_matching::procedural::{ProceduralLocomotion, procedural_locomotion};
use regia_animation::motion_matching::{
    AnimatorMotionMatch, LoadedMotionDatabase, MotionDatabaseRef, MotionDatabases, MotionMatchPlugin,
    MotionMatching, TrajectoryPredictor, run_trajectory_predictors,
};
use regia_animation::{AnimationClip, AnimationPlugin, RootMotionRoute};
use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_scene::Transform;

/// The database's asset path in [`MotionDatabases`].
pub const DATABASE_PATH: &str = "procedural/locomotion.motiondb.ron";

/// The sample's fixed frame time, seconds.
pub const FRAME: f32 = 1.0 / 30.0;

/// One leg of the course.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Leg {
    /// Name (also what the summary prints).
    pub name: &'static str,
    /// Seconds.
    pub seconds: f32,
    /// Desired ground speed, meters per second.
    pub speed: f32,
    /// Desired turn rate, radians per second (positive toward +X).
    pub turn: f32,
}

/// The course: stand, walk, turn right, run, turn left, stop.
pub fn course() -> Vec<Leg> {
    vec![
        Leg {
            name: "stand",
            seconds: 1.0,
            speed: 0.0,
            turn: 0.0,
        },
        Leg {
            name: "walk",
            seconds: 2.0,
            speed: 1.5,
            turn: 0.0,
        },
        Leg {
            name: "turn_right",
            seconds: 2.0,
            speed: 1.5,
            turn: 1.2,
        },
        Leg {
            name: "run",
            seconds: 2.0,
            speed: 4.0,
            turn: 0.0,
        },
        Leg {
            name: "turn_left",
            seconds: 2.0,
            speed: 1.5,
            turn: -1.2,
        },
        Leg {
            name: "stop",
            seconds: 2.0,
            speed: 0.0,
            turn: 0.0,
        },
    ]
}

/// Where the course is.
#[derive(Resource, Clone, Debug)]
pub struct Course {
    /// The legs.
    pub legs: Vec<Leg>,
    /// Seconds since the course (re)started.
    pub elapsed: f32,
    /// The desired heading, radians (0 = +Z, positive toward +X): each leg's turn rate turns it.
    pub heading: f32,
}

impl Default for Course {
    fn default() -> Self {
        Self {
            legs: course(),
            elapsed: 0.0,
            heading: 0.0,
        }
    }
}

impl Course {
    /// The leg at the current time (the course repeats).
    pub fn leg(&self) -> Leg {
        let total: f32 = self.legs.iter().map(|l| l.seconds).sum();
        let mut t = if total > 0.0 {
            self.elapsed.rem_euclid(total)
        } else {
            0.0
        };
        for leg in &self.legs {
            if t < leg.seconds {
                return *leg;
            }
            t -= leg.seconds;
        }
        self.legs.last().copied().unwrap_or(Leg {
            name: "stand",
            seconds: 1.0,
            speed: 0.0,
            turn: 0.0,
        })
    }
}

/// The walking character.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Hero;

/// What happened so far.
#[derive(Resource, Clone, Debug, Default, PartialEq)]
pub struct CourseLog {
    /// The clips played, in order, each listed once per stretch it played.
    pub clips: Vec<String>,
    /// Host `Update` calls observed (including calls made while agent-paused).
    pub frames: u64,
    /// Simulation updates with a positive gameplay delta.
    pub simulation_frames: u64,
    /// Ground distance the hero covered, meters.
    pub distance: f32,
}

/// The sample: [`MotionMatchPlugin`], the procedural database at [`DATABASE_PATH`], the hero and
/// the course.
pub struct MmLocomotionPlugin;

impl Plugin for MmLocomotionPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(MotionMatchPlugin)
            .init_resource::<Course>()
            .init_resource::<CourseLog>()
            .add_systems(Startup, (register_database, spawn_hero).chain())
            .add_systems(
                FixedUpdate,
                drive_course
                    .before(regia_animation::motion_matching::resolve_motion_database_refs)
                    .before(run_trajectory_predictors),
            )
            .add_systems(
                Update,
                (move_hero, log_course)
                    .chain()
                    .after(regia_animation::motion_matching::drive_animators_from_motion_matching),
            );
    }
}

/// The schema weights the sample authors: the desired path counts more than the current pose,
/// so the hero answers a new path within a few searches instead of drifting through the clips
/// whose pose is nearest (with equal weights the run's fast feet keep it from ever leaving walk).
pub fn authored_weights() -> WeightsSource {
    WeightsSource {
        root_velocity: 1.0,
        trajectory_position: 4.0,
        trajectory_direction: 2.0,
        foot_position: 0.5,
        foot_velocity: 0.25,
        ..WeightsSource::default()
    }
}

/// The procedural set with the sample's [`authored_weights`] in its source.
pub fn locomotion_set() -> ProceduralLocomotion {
    let mut set = procedural_locomotion();
    set.source.schema.weights = authored_weights();
    set
}

/// Build the database from its source (the call a cook makes) and register it with its clips.
fn register_database(mut databases: ResMut<MotionDatabases>) {
    let set = locomotion_set();
    match set.build() {
        Ok(database) => {
            databases.insert(
                DATABASE_PATH,
                LoadedMotionDatabase::built(database, set.skeleton.clone(), set.entry_clips()),
            );
        }
        Err(error) => {
            tracing::error!(target: "mm_locomotion", "the procedural database does not build: {error}")
        }
    }
}

/// The hero: a rig on the procedural skeleton whose graph plays a still pose, with motion
/// matching on top. The graph's own root motion is off so motion matching owns it.
fn spawn_hero(world: &mut World) {
    let skeleton = Arc::new(procedural_locomotion().skeleton);
    let binding = spawn_skeleton(world, skeleton);
    let mut animator = Animator::single(AnimationClip::empty("still", 1.0), true);
    animator.root_motion_route = RootMotionRoute::OnlyMontages;
    world.spawn((
        Hero,
        Transform::default(),
        binding,
        animator,
        MotionDatabaseRef::path(DATABASE_PATH),
        AnimatorMotionMatch::default(),
        TrajectoryPredictor::new(Vec3::ZERO, 0.0),
    ));
}

/// Advance the course and write the hero's desired velocity (the predictor makes the path).
fn drive_course(
    time: Res<regia_app::Time>,
    mut course: ResMut<Course>,
    mut heroes: Query<&mut TrajectoryPredictor, With<Hero>>,
) {
    let dt = time.delta_secs();
    course.elapsed += dt;
    let leg = course.leg();
    course.heading = (course.heading + leg.turn * dt).rem_euclid(std::f32::consts::TAU);
    let (sin, cos) = course.heading.sin_cos();
    for mut predictor in &mut heroes {
        predictor.desired_velocity = Vec3::new(sin, 0.0, cos) * leg.speed;
        // The course turns the desired velocity at the leg's rate: the prediction curves with it.
        predictor.desired_turn_rate = leg.turn;
    }
}

/// Move the hero by the root motion its animator accumulated.
fn move_hero(
    mut log: ResMut<CourseLog>,
    mut heroes: Query<(&mut Transform, &mut RootMotionOutput), With<Hero>>,
) {
    for (mut transform, mut output) in &mut heroes {
        let (translation, rotation, _) = output.take();
        let step = transform.rotation * translation;
        if step.is_finite() && rotation.is_finite() {
            transform.translation += step;
            // A zero-yaw delta is exactly the identity quaternion: skip the multiply-normalize,
            // whose float realization random-walks a unit quaternion by an ulp each call, so
            // zero-delta host updates (an agent pause) leave the transform bit for bit unchanged.
            if rotation != Quat::IDENTITY {
                transform.rotation = (transform.rotation * rotation).normalize();
            }
            log.distance += Vec3::new(step.x, 0.0, step.z).length();
        } else {
            transform.rotation = Quat::IDENTITY;
        }
    }
}

/// Record the clip sequence.
fn log_course(
    time: Res<regia_app::Time>,
    mut log: ResMut<CourseLog>,
    heroes: Query<&MotionMatching, With<Hero>>,
) {
    log.frames += 1;
    if !time.delta().is_zero() {
        log.simulation_frames += 1;
    }
    for matching in &heroes {
        let Some(step) = matching.last_step else {
            continue;
        };
        let Some(clip) = matching.database.clips().get(step.playback.clip) else {
            continue;
        };
        if log.clips.last() != Some(&clip.name) {
            log.clips.push(clip.name.clone());
        }
    }
}

/// A headless app of the sample at a fixed [`FRAME`] step (no window, no GPU).
pub fn headless_app() -> App {
    let mut app = App::new();
    app.add_plugins((regia_app::MinimalPlugins, AnimationPlugin, MmLocomotionPlugin));
    app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
        Duration::from_secs_f32(FRAME),
    ));
    app
}

/// The hero entity.
pub fn hero(world: &mut World) -> Option<Entity> {
    let mut query = world.query_filtered::<Entity, With<Hero>>();
    query.iter(world).next()
}

/// One line per fact: host updates, simulation frames, distance, clips played, searches and jumps.
pub fn summary(world: &mut World) -> Vec<String> {
    let log = world.resource::<CourseLog>().clone();
    let mut lines = vec![
        format!("host updates {}", log.frames),
        format!("simulation frames {}", log.simulation_frames),
        format!("distance {:.2} m", log.distance),
        format!("clips {}", log.clips.join(" > ")),
    ];
    if let Some(hero) = hero(world)
        && let Some(matching) = world.get::<MotionMatching>(hero)
    {
        lines.push(format!(
            "searches {} jumps {}",
            matching.matcher.searches(),
            matching.matcher.jumps()
        ));
    }
    lines
}

/// One trace line: time, course leg, playing clip and time, and the latest search's best frame.
pub fn trace_line(world: &mut World) -> String {
    let course = world.resource::<Course>().clone();
    let leg = course.leg();
    let Some(hero) = hero(world) else {
        return format!("{:6.2}s {:10} (no hero)", course.elapsed, leg.name);
    };
    let Some(matching) = world.get::<MotionMatching>(hero) else {
        return format!("{:6.2}s {:10} (no matcher)", course.elapsed, leg.name);
    };
    let database = &matching.database;
    let name = |clip: usize| database.clips().get(clip).map_or("?", |c| c.name.as_str());
    let playing = matching.matcher.playback().map_or_else(
        || "nothing".to_string(),
        |p| format!("{} @ {:.2}", name(p.clip), p.time),
    );
    let best = matching
        .matcher
        .last_search()
        .and_then(|s| s.best)
        .map_or_else(String::new, |b| {
            let clip = database.frame(b.frame).map_or(0, |f| f.clip);
            format!(" best {} cost {:.3}", name(clip), b.cost)
        });
    format!("{:6.2}s {:10} plays {playing}{best}", course.elapsed, leg.name)
}
