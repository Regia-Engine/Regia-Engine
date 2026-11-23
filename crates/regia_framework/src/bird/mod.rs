// SPDX-License-Identifier: MPL-2.0
//! A bird that the camera startles (ADR-1502): it perches and idles until the camera comes too
//! close or too fast (or the wake's front hits it), flinches, takes off in a panic, flees on an
//! erratic seeded flight that the camera's wake throws about, and glides away.
//!
//! - [`BirdSpec`] (`bird.json`) places the bird: feet position, facing, perch, size class. It
//!   also sets when it startles ([`StartleSpec`]) and how it flees ([`FleeSpec`]).
//! - [`find_trigger`] derives the startle time from the camera path, so a host can place a bird
//!   by the path ([`place_on_path`]) and know when it will go.
//! - [`BirdSim`] is the behaviour as a pure function of time (fixed-step flight, seeded
//!   jinks); [`BirdMesh`] is the procedural model (14 rigid parts, no assets).
//! - [`BirdPlugin`] spawns birds queued in [`BirdFlock`] (the agent's `bird.spawn`), poses them
//!   every frame from the shot player's clock and reports their state (`bird.list`).
//!
//! The clock is the camera path's: the [`ShotPlayer`] time when a
//! sequence is loaded, otherwise the [`CameraBody`]'s. With no sequence the trigger is found
//! online, from the body's state each frame.

pub mod mesh;
pub mod sim;
pub mod spec;

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_core::math::wake::WakeShape;
use regia_render::MeshRenderer;
use regia_scene::{GlobalTransform, Transform};
use serde::Serialize;

pub use mesh::{BIRD_PARTS, BirdMesh, BirdPart, pose_parts};
pub use sim::{
    BirdPhase, BirdPose, BirdSim, FLIGHT_STEP_S, FlightEnv, Trigger, TriggerCause, find_trigger,
    perch_centre, place_on_path,
};
pub use spec::{
    BIRD_SPEC_VERSION, BirdBody, BirdSpec, BirdSpecError, BirdSpecies, FleeSpec, Perch, StartleSpec,
};

use crate::camera_body::proximity::closing_speed;
use crate::camera_body::{CameraBody, CameraBodySet, SprayGround};
use crate::shots::{ShotPlayer, ShotSequence};

/// One bird in the world: its behaviour and its part entities.
#[derive(Component, Debug)]
pub struct Bird {
    sim: BirdSim,
    parts: [Entity; BIRD_PARTS],
    pose: Option<BirdPose>,
}

impl Bird {
    /// The behaviour.
    #[must_use]
    pub fn sim(&self) -> &BirdSim {
        &self.sim
    }

    /// The part entities, [`BirdPart::ALL`] order.
    #[must_use]
    pub fn parts(&self) -> &[Entity; BIRD_PARTS] {
        &self.parts
    }

    /// The pose of the last frame.
    #[must_use]
    pub fn pose(&self) -> Option<&BirdPose> {
        self.pose.as_ref()
    }
}

/// What one bird is doing (`bird.list`).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct BirdReport {
    /// Name.
    pub name: String,
    /// Size class.
    pub species: &'static str,
    /// Phase now.
    pub phase: BirdPhase,
    /// Body centre now.
    pub position: [f32; 3],
    /// Velocity now, m/s.
    pub velocity: [f32; 3],
    /// The startle cue (`None`: not startled on this path).
    pub trigger: Option<Trigger>,
    /// When the flinch starts, seconds.
    pub flinch_s: Option<f64>,
    /// When the take-off starts, seconds.
    pub takeoff_s: Option<f64>,
    /// Eye scale now.
    pub eye_scale: f32,
    /// Shoulder angle now, degrees.
    pub wing_raise_deg: f32,
    /// The clock the pose is for, seconds.
    pub time_s: f64,
}

/// The agent-facing flock: birds to spawn and remove on the next frame, and every bird's last
/// report (sorted by name).
#[derive(Resource, Debug, Default)]
pub struct BirdFlock {
    /// Specs to spawn next frame (checked).
    pub pending: Vec<BirdSpec>,
    /// Names to remove next frame.
    pub remove: Vec<String>,
    reports: Vec<BirdReport>,
}

impl BirdFlock {
    /// Queues a bird (a bird of the same name is replaced).
    pub fn spawn(&mut self, spec: BirdSpec) {
        self.remove.push(spec.name.clone());
        self.pending.push(spec);
    }

    /// The last frame's reports, by name.
    #[must_use]
    pub fn reports(&self) -> &[BirdReport] {
        &self.reports
    }
}

/// The trigger of `spec` on a loaded sequence (the agent's `bird.trigger`).
#[must_use]
pub fn sequence_trigger(spec: &BirdSpec, sequence: &ShotSequence, wake: &WakeShape) -> Option<Trigger> {
    find_trigger(spec, sequence, wake)
}

/// Spawns a bird's parts and its root entity (posed perched until the first frame).
pub fn spawn_bird(commands: &mut Commands, spec: BirdSpec, trigger: Option<Trigger>) -> Entity {
    let sim = BirdSim::new(spec, trigger);
    let mesh = BirdMesh::build(sim.body());
    let calm = BirdPose {
        phase: BirdPhase::Perched,
        position: perch_centre(sim.spec()),
        rotation: regia_core::math::Quat::from_rotation_y(sim.spec().facing_deg.to_radians()),
        velocity: Vec3::ZERO,
        head_yaw: 0.0,
        head_pitch: 0.0,
        wing_raise: 0.0,
        wing_tip: 0.0,
        wing_sweep: 0.0,
        wing_open: 0.0,
        tail_spread: 0.0,
        eye_scale: 1.0,
        ruffle: 0.0,
        crouch: 0.0,
        legs: 1.0,
        flap_phase: 0.0,
    };
    let transforms = pose_parts(sim.body(), &calm);
    let parts: [Entity; BIRD_PARTS] = std::array::from_fn(|i| {
        let t = transforms[i];
        commands
            .spawn((
                t,
                GlobalTransform(t.compute_affine()),
                MeshRenderer::new(mesh.parts[i].clone(), mesh.materials[i].clone()),
            ))
            .id()
    });
    commands
        .spawn((
            Bird {
                sim,
                parts,
                pose: None,
            },
            Transform {
                translation: calm.position,
                ..Transform::default()
            },
        ))
        .id()
}

/// The cue of a bird against the body's state now (no path: found frame by frame).
fn online_cue(
    spec: &BirdSpec,
    body: &regia_core::math::wake::WakeBody,
    wake: &WakeShape,
) -> Option<TriggerCause> {
    let at = perch_centre(spec);
    let s = spec.startle;
    let d = body.position.distance(at);
    let closing = closing_speed(body.position, body.velocity, at);
    if d <= s.distance_m {
        return Some(TriggerCause::Distance);
    }
    if d <= s.alert_distance_m && closing >= s.closing_speed_m_s && s.closing_speed_m_s > 0.0 {
        return Some(TriggerCause::Closing);
    }
    let w = wake.sample(body, at);
    (w.since_front_s >= 0.0 && w.since_front_s < 1.0 && w.peak_m_s >= s.gust_m_s && s.gust_m_s > 0.0)
        .then_some(TriggerCause::Wake)
}

/// The sequence the triggers were found on.
#[derive(Default)]
struct TriggerPath(Option<ShotSequence>);

/// What the bird system reads: the shot player's clock and path, the camera body, the ground.
type BirdWorld<'w> = (
    Option<Res<'w, ShotPlayer>>,
    Option<Res<'w, CameraBody>>,
    Option<Res<'w, SprayGround>>,
);

/// Spawns, removes and poses the birds.
#[tracing::instrument(skip_all, target = "regia_framework", name = "birds_drive")]
fn drive_birds(
    mut commands: Commands,
    mut flock: ResMut<BirdFlock>,
    (player, body, ground): BirdWorld<'_>,
    mut cached: Local<TriggerPath>,
    mut birds: Query<(Entity, &mut Bird)>,
    mut parts: Query<(&mut Transform, &mut GlobalTransform), Without<Bird>>,
) {
    let wake = body.as_ref().map_or_else(WakeShape::default, |b| b.spec().wake);
    let sequence = player.as_ref().and_then(|p| p.sequence());
    let path_changed = cached.0.as_ref() != sequence;
    if path_changed {
        cached.0 = sequence.cloned();
    }

    // Removals, then spawns (a replaced bird of the same name goes first).
    let remove = std::mem::take(&mut flock.remove);
    if !remove.is_empty() {
        for (entity, bird) in &birds {
            if remove.iter().any(|n| *n == bird.sim.spec().name) {
                for part in bird.parts {
                    commands.entity(part).despawn();
                }
                commands.entity(entity).despawn();
            }
        }
    }
    for spec in std::mem::take(&mut flock.pending) {
        let trigger = cached.0.as_ref().and_then(|s| find_trigger(&spec, s, &wake));
        spawn_bird(&mut commands, spec, trigger);
    }

    let time = match (player.as_ref(), cached.0.as_ref()) {
        (Some(p), Some(_)) => p.time(),
        _ => body.as_ref().map_or(0.0, |b| b.time()),
    };
    let path = cached.0.as_ref();
    let gust = |t: f64, p: Vec3| -> Vec3 {
        match (path, body.as_ref()) {
            (Some(seq), Some(b)) => b
                .sample_path(seq, t)
                .map_or(Vec3::ZERO, |s| wake.sample(&s, p).gust),
            (None, Some(b)) => b.wake_at(p).gust,
            _ => Vec3::ZERO,
        }
    };
    let height = ground.as_ref().and_then(|g| g.height.clone());
    let floor = |x: f32, z: f32| height.as_ref().and_then(|h| h(x, z));
    let env = FlightEnv {
        gust: &gust,
        floor: if height.is_some() { Some(&floor) } else { None },
    };

    let mut reports = Vec::new();
    for (_, mut bird) in &mut birds {
        if remove.iter().any(|n| *n == bird.sim.spec().name) {
            continue;
        }
        match path {
            Some(seq) if path_changed => {
                let trigger = find_trigger(bird.sim.spec(), seq, &wake);
                bird.sim.set_trigger(trigger);
            }
            None if bird.sim.trigger().is_none() => {
                if let Some(state) = body.as_ref().and_then(|b| b.state())
                    && let Some(cause) = online_cue(bird.sim.spec(), &state, &wake)
                {
                    let at = perch_centre(bird.sim.spec());
                    bird.sim.set_trigger(Some(Trigger {
                        time_s: time,
                        cause,
                        camera_position: state.position.to_array(),
                        camera_velocity: state.velocity.to_array(),
                        distance_m: state.position.distance(at),
                        closing_m_s: closing_speed(state.position, state.velocity, at),
                    }));
                }
            }
            _ => {}
        }
        let pose = bird.sim.pose_at(time, &env);
        let transforms = pose_parts(bird.sim.body(), &pose);
        for (entity, t) in bird.parts.iter().zip(transforms) {
            if let Ok((mut transform, mut global)) = parts.get_mut(*entity) {
                *transform = t;
                *global = GlobalTransform(t.compute_affine());
            }
        }
        bird.pose = Some(pose);
        let sim = &bird.sim;
        reports.push(BirdReport {
            name: sim.spec().name.clone(),
            species: sim.spec().species.name(),
            phase: pose.phase,
            position: pose.position.to_array(),
            velocity: pose.velocity.to_array(),
            trigger: sim.trigger().copied(),
            flinch_s: sim.flinch_time(),
            takeoff_s: sim.takeoff_time(),
            eye_scale: pose.eye_scale,
            wing_raise_deg: pose.wing_raise.to_degrees(),
            time_s: time,
        });
    }
    reports.sort_by(|a, b| a.name.cmp(&b.name));
    flock.reports = reports;
}

/// Adds [`BirdFlock`] and the system that spawns and poses the birds, after the camera body.
pub struct BirdPlugin;

impl Plugin for BirdPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<BirdFlock>().add_systems(
            PostUpdate,
            drive_birds
                .after(CameraBodySet)
                .before(PostUpdateSet::TransformPropagate),
        );
    }
}

#[cfg(test)]
mod tests;
