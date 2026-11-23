// SPDX-License-Identifier: MPL-2.0
//! The physics occlusion provider for entity emitters (`docs/specs/16-audio.md` § 3, Tier 2).
//!
//! `regia_audio` queues one ray per occluding emitter per frame (within a budget) on
//! [`AudioOcclusion`]; this system casts them through `regia_physics` and reports how many
//! solid bodies stand between each listener and emitter. The emitter's and the listener's own
//! bodies (they and their ancestors: a vehicle carrying an engine emitter, the player carrying
//! the camera) never occlude, and neither do sensors. [`DefaultPlugins`](crate::DefaultPlugins)
//! installs it; it is inert until a physics world exists.

use regia_app::prelude::*;
use regia_audio::{AudioOcclusion, OcclusionRay};
use regia_physics::{PhysicsQuery, PhysicsWorld, QueryFilter};

/// Distinct solid bodies one occlusion ray examines.
///
/// [`OcclusionTuning::amount_for_obstacles`](regia_audio::OcclusionTuning::amount_for_obstacles)
/// stops changing at 16 obstacles, so a ray that has already seen 16 distinct solids does not
/// look at the rest. [`OcclusionRayStats::last_ray_entities`] is never above this.
pub const OCCLUSION_OBSTACLE_CAP: usize = 16;

/// How many distinct solid entities the latest occlusion ray examined.
///
/// The physics occlusion system writes this. A saturated ray stays at
/// [`OCCLUSION_OBSTACLE_CAP`].
#[derive(Resource, Default, Debug, Clone, Copy)]
pub struct OcclusionRayStats {
    /// Distinct solid entities on the latest ray (at most [`OCCLUSION_OBSTACLE_CAP`]).
    pub last_ray_entities: usize,
}

/// Installs [`physics_audio_occlusion`] before the audio sync.
#[derive(Default)]
pub struct PhysicsAudioOcclusionPlugin;

impl Plugin for PhysicsAudioOcclusionPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<OcclusionRayStats>();
        app.add_systems(
            PostUpdate,
            physics_audio_occlusion
                .in_set(PostUpdateSet::AudioSync)
                .before(regia_audio::audio_sync)
                .run_if(resource_exists::<PhysicsWorld>),
        );
    }
}

/// Buffers the occlusion system reuses across frames. Public only because it is the
/// `Local` state of the public [`physics_audio_occlusion`] system; hosts do not build it.
#[derive(Default)]
pub struct OcclusionScratch {
    /// The emitter, the listener and their ancestors.
    ignore: Vec<Entity>,
    /// Distinct solid entities on the ray currently being cast.
    hits: Vec<Entity>,
    /// Rays drained from [`AudioOcclusion`].
    rays: Vec<OcclusionRay>,
}

/// Collects `entity` and its ancestors into `out`.
fn with_ancestors(entity: Entity, parents: &Query<&ChildOf>, out: &mut Vec<Entity>) {
    let mut current = Some(entity);
    // Bounded walk: a malformed hierarchy must not spin forever.
    for _ in 0..64 {
        let Some(e) = current else {
            return;
        };
        out.push(e);
        current = parents.get(e).ok().map(ChildOf::parent);
    }
}

/// `PostUpdate`/`AudioSync` system: answers the queued occlusion rays with physics raycasts.
#[tracing::instrument(target = "regia", skip_all, level = "trace")]
pub fn physics_audio_occlusion(
    physics: PhysicsQuery,
    occlusion: Option<ResMut<AudioOcclusion>>,
    mut stats: ResMut<OcclusionRayStats>,
    parents: Query<&ChildOf>,
    mut scratch: Local<OcclusionScratch>,
) {
    let Some(mut occlusion) = occlusion else {
        return;
    };
    let scratch = &mut *scratch;
    occlusion.take_pending_into(&mut scratch.rays);
    for ray in scratch.rays.iter().copied() {
        let span = ray.to - ray.from;
        let distance = span.length();
        if distance < 1e-3 || !distance.is_finite() {
            occlusion.report(ray.emitter, 0.0);
            continue;
        }
        scratch.ignore.clear();
        with_ancestors(ray.emitter, &parents, &mut scratch.ignore);
        if let Some(listener) = ray.listener {
            with_ancestors(listener, &parents, &mut scratch.ignore);
        }
        // Closest-hit casts, excluding each solid already counted. The amount saturates at
        // [`OCCLUSION_OBSTACLE_CAP`], so the rest of a dense segment is never built.
        let ancestors = scratch.ignore.len();
        scratch.hits.clear();
        while scratch.hits.len() < OCCLUSION_OBSTACLE_CAP {
            scratch.ignore.truncate(ancestors);
            scratch.ignore.extend_from_slice(&scratch.hits);
            let filter = QueryFilter::new()
                .exclude_sensors()
                .exclude_entities(&scratch.ignore);
            match physics.raycast(ray.from, span, distance, filter) {
                Some(hit) if !scratch.hits.contains(&hit.entity) => scratch.hits.push(hit.entity),
                // `None`, or the same entity again: excluding it did not move the ray on.
                _ => break,
            }
        }
        stats.last_ray_entities = scratch.hits.len();
        occlusion.report_obstacles(ray.emitter, scratch.hits.len());
    }
}
