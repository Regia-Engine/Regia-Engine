// SPDX-License-Identifier: MPL-2.0
//! Exposure zones drive the frame's exposure (DO27 ask 5, ADR-257 and ADR-322;
//! `book/agents/exposure-zones.md`).
//!
//! [`ExposureZonesPlugin`] runs [`sync_exposure_zones`] every frame, after transform
//! propagation. It finds the active camera (the window camera with the lowest `order`, the one
//! the renderer draws first), weighs every [`ExposureZone`] at the camera's eye with
//! [`ExposureZone::weight_at`], combines the zones that cover it with
//! [`combine_weighted_zones`] against [`ZONE_OPEN_RANGE`], and hands the result to
//! `RuntimeRenderer::set_exposure_target`, with the frame's [`Time`] step as the adaptation
//! `dt`. When no zone covers the eye (or there is no camera) the target is `None`, and the
//! frame is exposed exactly as without zones once the eye has handed back.
//!
//! The zones are gathered into one scratch vector kept in [`ExposureZoneState`], so an idle
//! frame, and a steady one once the vector has grown, allocates nothing.
//!
//! A [`PostVolume`] whose overrides carry an `eye` range joins the same combine as a zone
//! with the volume's weight at the eye and its priority (ADR-1464,
//! `docs/specs/30-post-volumes-and-physical-camera.md` § 4), so zones and volumes drive one eye.

use regia_app::prelude::*;
use regia_app::{PostUpdate, PostUpdateSet};
use regia_render::exposure_zones::{EvTarget, ExposureZone, ZONE_OPEN_RANGE, combine_weighted_zones};
use regia_render::{Camera, PostVolume, RenderTarget, RuntimeRenderer};
use regia_scene::GlobalTransform;

/// What [`sync_exposure_zones`] found on the last frame (the agent's `render.get_exposure`).
#[derive(Resource, Default, Debug)]
pub struct ExposureZoneState {
    /// Zones with a positive weight at the camera's eye, every priority counted (post volumes
    /// with an eye range included).
    pub zone_count: usize,
    /// How many of [`Self::zone_count`] are post volumes with an eye range (ADR-1464).
    pub volume_count: usize,
    /// The combined target handed to the renderer; `None` outside every zone.
    pub target: Option<EvTarget>,
    /// World position of the active camera's eye; `None` without a window camera.
    pub eye: Option<[f32; 3]>,
    /// The camera the zones were weighed at.
    pub camera: Option<Entity>,
    /// Frames the system has run.
    pub frames: u64,
    scratch: Vec<(f32, ExposureZone)>,
}

/// Samples [`ExposureZone`]s into the renderer's exposure every frame ([`sync_exposure_zones`]).
#[derive(Default)]
pub struct ExposureZonesPlugin;

impl Plugin for ExposureZonesPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ExposureZoneState>().add_systems(
            PostUpdate,
            sync_exposure_zones.after(PostUpdateSet::TransformPropagate),
        );
    }
}

/// `PostUpdate` (after transform propagation): sets the renderer's exposure target from the
/// zones at the active camera (see the module docs). Without a `RuntimeRenderer` it only updates
/// [`ExposureZoneState`].
pub fn sync_exposure_zones(
    zones: Query<(&ExposureZone, &GlobalTransform)>,
    volumes: Query<(&PostVolume, &GlobalTransform)>,
    cameras: Query<(Entity, &Camera, &GlobalTransform)>,
    time: Option<Res<Time>>,
    renderer: Option<ResMut<RuntimeRenderer>>,
    mut state: ResMut<ExposureZoneState>,
) {
    let _span = tracing::info_span!("exposure_zones").entered();
    let state = &mut *state;
    state.frames = state.frames.wrapping_add(1);
    state.scratch.clear();
    let active = active_camera(&cameras);
    state.camera = active.map(|(entity, _)| entity);
    state.eye = active.map(|(_, eye)| eye);
    let mut volume_count = 0;
    if let Some((_, eye)) = active {
        for (zone, world) in &zones {
            let weight = zone.weight_at(world, eye);
            if weight.is_finite() && weight > 0.0 {
                state.scratch.push((weight, *zone));
            }
        }
        for (volume, world) in &volumes {
            let Some(zone) = volume.eye_zone().filter(|_| volume.is_supported()) else {
                continue;
            };
            let weight = volume.weight_at(world, eye);
            if weight.is_finite() && weight > 0.0 {
                state.scratch.push((weight, zone));
                volume_count += 1;
            }
        }
    }
    state.volume_count = volume_count;
    state.zone_count = state.scratch.len();
    state.target = if state.scratch.is_empty() {
        None
    } else {
        Some(combine_weighted_zones(&state.scratch, ZONE_OPEN_RANGE))
    };
    if let Some(mut renderer) = renderer {
        renderer.set_exposure_target(state.target);
        if let Some(time) = time {
            renderer.0.set_exposure_dt(Some(time.delta_secs()));
        }
    }
}

/// The window camera with the lowest `order` (ties: the first in query order) and its eye.
fn active_camera(cameras: &Query<(Entity, &Camera, &GlobalTransform)>) -> Option<(Entity, [f32; 3])> {
    cameras
        .iter()
        .filter(|(_, camera, _)| matches!(camera.target, RenderTarget::Window))
        .min_by_key(|(_, camera, _)| camera.order)
        .map(|(entity, _, world)| (entity, world.0.translation.to_array()))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use regia_core::math::Vec3;
    use regia_render::exposure_zones::{ExposureZoneShape, ZoneExposureMode};
    use regia_scene::Transform;

    use super::*;

    const TUBE_EV: f32 = 8.5;

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins((
            regia_app::MinimalPlugins,
            regia_scene::ScenePlugin,
            ExposureZonesPlugin,
        ));
        app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
            Duration::from_millis(33),
        ));
        app.init_resource::<RuntimeRenderer>();
        app
    }

    fn tube() -> ExposureZone {
        ExposureZone {
            shape: ExposureZoneShape::Box {
                half_extents: [8.0, 4.0, 200.0],
            },
            ev_min: TUBE_EV,
            ev_max: TUBE_EV,
            blend_distance: 25.0,
            priority: 1,
            ..ExposureZone::default()
        }
    }

    fn move_camera(app: &mut App, camera: Entity, z: f32) {
        if let Some(mut transform) = app.world_mut().get_mut::<Transform>(camera) {
            transform.translation = Vec3::new(0.0, 1.5, z);
        }
        app.update();
    }

    fn renderer_target(app: &App) -> Option<EvTarget> {
        app.world()
            .resource::<RuntimeRenderer>()
            .0
            .exposure_zones()
            .target()
    }

    #[test]
    fn the_target_is_set_and_cleared_as_the_camera_crosses() {
        let mut app = app();
        app.world_mut()
            .spawn((Transform::from_translation(Vec3::ZERO), tube()));
        let camera = app
            .world_mut()
            .spawn((
                Transform::from_translation(Vec3::new(0.0, 1.5, -400.0)),
                Camera::default(),
            ))
            .id();
        // A texture camera closer to the tube is not the eye.
        app.world_mut().spawn((
            Transform::from_translation(Vec3::new(0.0, 1.5, 0.0)),
            Camera {
                target: RenderTarget::Texture { asset_id: 7 },
                order: -5,
                ..Camera::default()
            },
        ));
        app.update();
        app.update();

        let state = app.world().resource::<ExposureZoneState>();
        assert_eq!(state.camera, Some(camera));
        assert_eq!(state.zone_count, 0);
        assert_eq!(state.target, None);
        assert_eq!(renderer_target(&app), None, "open ground: no target");

        // Halfway across the 25 m mouth: blended with the open range.
        move_camera(&mut app, camera, -212.5);
        let state = app.world().resource::<ExposureZoneState>();
        assert_eq!(state.zone_count, 1);
        let mouth = state.target.expect("a target in the mouth");
        assert!(mouth.ev_min < TUBE_EV && mouth.ev_max > TUBE_EV, "{mouth:?}");
        assert_eq!(renderer_target(&app), Some(mouth));

        // Inside: the tube's own range.
        move_camera(&mut app, camera, 0.0);
        let inside = renderer_target(&app).expect("inside");
        assert!((inside.ev_min - TUBE_EV).abs() < 1.0e-5 && (inside.ev_max - TUBE_EV).abs() < 1.0e-5);

        // Out the far side and past the band: cleared.
        move_camera(&mut app, camera, 230.0);
        assert_eq!(renderer_target(&app), None);
        assert_eq!(app.world().resource::<ExposureZoneState>().zone_count, 0);
    }

    #[test]
    fn the_host_step_reaches_the_renderer_eye() {
        let mut app = app();
        app.world_mut()
            .spawn((Transform::from_translation(Vec3::ZERO), tube()));
        app.world_mut().spawn((
            Transform::from_translation(Vec3::new(0.0, 1.5, 0.0)),
            Camera::default(),
        ));
        app.update();
        app.update();
        // No GPU frame here: drive the renderer's eye the way the post stack does, with the
        // step the system handed over.
        let world = app.world_mut();
        let mut renderer = world.resource_mut::<RuntimeRenderer>();
        let zones = renderer.0.exposure_zones_mut();
        let ev = zones.apply(regia_render::exposure_zones::MeterReading::steady(14.5));
        let report = zones.report().expect("report");
        assert_eq!(report.mode, ZoneExposureMode::Zone);
        assert!(ev < 14.5 && ev > TUBE_EV, "one step toward the tube: {ev}");
        // A second apply in the same frame has no host step left: it reads the wall clock from
        // a first reading, which is 0 s, so the eye holds.
        let held = zones.apply(regia_render::exposure_zones::MeterReading::steady(14.5));
        assert!(held == ev, "{held} vs {ev}");
    }

    #[test]
    fn a_post_volume_eye_range_drives_the_same_eye() {
        use regia_render::post_volumes::{EyeRange, PostOverrides, PostVolumeShape};
        let mut app = app();
        let cellar = PostVolume {
            shape: PostVolumeShape::Box {
                half_extents: [8.0, 4.0, 20.0],
            },
            blend_distance: 10.0,
            priority: 2,
            overrides: PostOverrides {
                eye: Some(EyeRange {
                    ev_min: 5.0,
                    ev_max: 7.0,
                    adapt_up_speed: 1.0,
                    adapt_down_speed: 1.0,
                }),
                // A volume without an eye range never joins the zones.
                saturation: Some(0.5),
                ..PostOverrides::default()
            },
            ..PostVolume::default()
        };
        app.world_mut()
            .spawn((Transform::from_translation(Vec3::ZERO), cellar));
        app.world_mut().spawn((
            Transform::from_translation(Vec3::new(30.0, 0.0, 0.0)),
            PostVolume {
                shape: PostVolumeShape::Unbound,
                ..PostVolume::default()
            },
        ));
        // A lower-priority tube zone in the same place loses to the volume's priority.
        app.world_mut()
            .spawn((Transform::from_translation(Vec3::ZERO), tube()));
        let camera = app
            .world_mut()
            .spawn((
                Transform::from_translation(Vec3::new(0.0, 1.5, -400.0)),
                Camera::default(),
            ))
            .id();
        app.update();
        app.update();
        assert_eq!(renderer_target(&app), None, "far away: neither zone nor volume");

        move_camera(&mut app, camera, 0.0);
        let state = app.world().resource::<ExposureZoneState>();
        assert_eq!(state.zone_count, 2, "the tube and the volume both cover the eye");
        assert_eq!(state.volume_count, 1);
        let inside = renderer_target(&app).expect("inside the volume");
        assert!(
            (inside.ev_min - 5.0).abs() < 1.0e-5 && (inside.ev_max - 7.0).abs() < 1.0e-5,
            "{inside:?}"
        );
        assert!((inside.adapt_up_speed - 1.0).abs() < 1.0e-5);

        // Half way through the band (5 m outside the 20 m half length): blended with the open
        // range, so the range is wider than the volume's but contains it.
        move_camera(&mut app, camera, -25.0);
        let mouth = renderer_target(&app).expect("in the band");
        assert!(mouth.ev_min < 5.0 && mouth.ev_max > 7.0, "{mouth:?}");
    }

    #[test]
    fn no_zones_and_no_camera_leave_the_target_clear() {
        let mut app = app();
        app.update();
        let state = app.world().resource::<ExposureZoneState>();
        assert_eq!(state.camera, None);
        assert_eq!(state.eye, None);
        assert_eq!(state.target, None);
        assert!(state.frames >= 1);
        assert_eq!(renderer_target(&app), None);
    }
}
