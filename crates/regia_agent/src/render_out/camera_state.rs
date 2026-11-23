// SPDX-License-Identifier: MPL-2.0
//! Camera shake state a render-out job restarts at its start or puts back at its end (ADR-2687).
//!
//! The director's shakes and every camera's received shakes play on the presentation clock, so a
//! job with the world held still moves them, as it moves the live effects (ADR-2633). The job's
//! own frames keep that motion; [`HeldShakes`] puts the shakes back when the frames end, so the
//! next held job starts from the same shakes. [`restart_idle_seeds`] starts the seeds of new
//! shakes over when none is running, so a shake added during a job plays the same noise in every
//! job.

use regia_ecs::prelude::*;
use regia_framework::camera::CameraDirector;
use regia_framework::camera::sensor::{ShakeField, ShakeReceiver};

/// The camera shakes as a held job found them.
pub(super) struct HeldShakes {
    director: Option<CameraDirector>,
    receivers: Vec<(Entity, ShakeReceiver)>,
    /// The emit field's next seed: a shake emitted during the job moves it (ADR-2814).
    field_seed: Option<u64>,
}

impl HeldShakes {
    /// Takes the director's shakes and every camera's received shakes as they stand.
    pub(super) fn capture(world: &mut World) -> Self {
        let receivers = world
            .query::<(Entity, &ShakeReceiver)>()
            .iter(world)
            .map(|(entity, receiver)| (entity, receiver.clone()))
            .collect();
        Self {
            director: world.get_resource::<CameraDirector>().cloned(),
            receivers,
            field_seed: world.get_resource::<ShakeField>().map(ShakeField::next_seed),
        }
    }

    /// Puts the shakes back. A camera keeps the pose it last added (the shake system takes it off
    /// next frame); a camera that went away stays gone.
    pub(super) fn restore(self, world: &mut World) {
        if let Some(director) = self.director {
            world.insert_resource(director);
        }
        for (entity, earlier) in self.receivers {
            if let Some(mut receiver) = world.get_mut::<ShakeReceiver>(entity) {
                receiver.restore_shakes(&earlier);
            }
        }
        if let (Some(seed), Some(mut field)) = (self.field_seed, world.get_resource_mut::<ShakeField>()) {
            field.restore_next_seed(seed);
        }
    }
}

/// Starts the seeds of new shakes over where nothing is running: the director's when it plays no
/// shake, the emit field's when no camera has a shake on its way or playing. Returns whether each
/// restarted (`[director, field]`).
pub(super) fn restart_idle_seeds(world: &mut World) -> [bool; 2] {
    let director = world
        .get_resource_mut::<CameraDirector>()
        .is_some_and(|mut director| director.restart_idle_seeds());
    let busy = world
        .query::<&ShakeReceiver>()
        .iter(world)
        .any(|receiver| !receiver.pending().is_empty() || !receiver.active().is_empty());
    let field = !busy
        && world.get_resource_mut::<ShakeField>().is_some_and(|mut field| {
            field.restart_seeds();
            true
        });
    [director, field]
}

#[cfg(test)]
mod tests {
    use regia_framework::camera::Shake;

    use super::*;

    #[test]
    fn a_held_job_puts_the_director_shakes_back() {
        let mut world = World::new();
        let mut director = CameraDirector::default();
        director.shake(Shake::default());
        world.insert_resource(director);
        let before = world.resource_mut::<CameraDirector>().sample_pose(0.05);

        let held = HeldShakes::capture(&mut world);
        // The job's frames move the shake on.
        for _ in 0..30 {
            world.resource_mut::<CameraDirector>().sample_pose(1.0 / 24.0);
        }
        held.restore(&mut world);

        // From the restored state the same step gives the same pose as from the captured one.
        let mut fresh = CameraDirector::default();
        fresh.shake(Shake::default());
        let _ = fresh.sample_pose(0.05);
        let expected = fresh.sample_pose(0.05);
        let again = world.resource_mut::<CameraDirector>().sample_pose(0.05);
        assert_eq!(again, expected);
        assert!(!before.is_idle(), "the shake was playing");
    }

    /// ADR-2814: a shake on its way keeps the field's seed at the job's start, so a shake emitted
    /// during the job moved it for good, and the next held job's emit oscillated another way.
    #[test]
    fn a_held_job_puts_the_emit_seed_back() {
        let mut world = World::new();
        let mut field = ShakeField::default();
        field.restore_next_seed(7);
        world.insert_resource(field);
        let held = HeldShakes::capture(&mut world);
        // What an emit read during the job does to it.
        world.resource_mut::<ShakeField>().restore_next_seed(9);
        held.restore(&mut world);
        assert_eq!(world.resource::<ShakeField>().next_seed(), 7);
    }

    #[test]
    fn idle_seeds_start_over_and_a_running_shake_keeps_them() {
        let mut world = World::new();
        let mut director = CameraDirector::default();
        director.shake(Shake::default());
        world.insert_resource(director);
        world.insert_resource(ShakeField::default());
        // A shake still runs: its seed stays unique.
        assert_eq!(restart_idle_seeds(&mut world), [false, true]);
        // Once it ended, the next shake takes the first seed again.
        for _ in 0..60 {
            world.resource_mut::<CameraDirector>().sample_pose(0.1);
        }
        assert_eq!(restart_idle_seeds(&mut world), [true, true]);
        let mut first = CameraDirector::default();
        first.shake(Shake::default());
        world.resource_mut::<CameraDirector>().shake(Shake::default());
        assert_eq!(
            world.resource_mut::<CameraDirector>().sample_pose(0.1),
            first.sample_pose(0.1)
        );
    }
}
