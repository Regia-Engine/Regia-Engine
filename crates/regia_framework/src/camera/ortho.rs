// SPDX-License-Identifier: MPL-2.0
//! The orthographic pixel-perfect mode (`docs/specs/22-tier2.md` § T2.10): a [`Camera2d`]
//! component that drives the 2D sprite pass (`regia_2d::View2d`), with an optional
//! [`Camera2dFollow`] and the screen ↔ world conversions games need for mouse and touch input.
//!
//! The math is `regia_2d::camera` (macroquad's `Camera2D` conversions and letterbox, ported):
//! [`OrthoCamera`] (centre, pixels per world unit, rotation), [`PixelPerfect`] (a fixed virtual
//! resolution scaled by the largest integer factor that fits, centred with bars) and
//! [`Camera2dView`]. With `pixel_perfect` set, every virtual pixel becomes an exact
//! `scale × scale` block; with `snap` the camera centre is rounded to whole virtual pixels so
//! scrolling never shimmers.
//!
//! Systems ([`Camera2dPlugin`], `PostUpdate` after transform propagation, before the 2D frame is
//! extracted): [`follow_camera2d`] eases followers towards their target, then [`write_view2d`]
//! copies the active camera into `View2d`.

use regia_app::prelude::*;
use regia_core::math::{UVec2, Vec2, Vec3, det_exp};
use regia_scene::{GlobalTransform, Transform};

pub use regia_2d::camera::{Camera2dView, Letterbox, OrthoCamera, PixelPerfect, Rect2};

/// A 2D orthographic camera. Its `Transform` x, y is the view centre and its rotation about Z
/// rotates the view. The first active one drives the 2D pass.
#[derive(Component, Reflect, Clone, Debug, PartialEq)]
#[reflect(Component, Default)]
#[require(Transform)]
pub struct Camera2d {
    /// Output pixels per world unit (zoom). In pixel-perfect mode: virtual pixels per unit.
    pub pixels_per_unit: f32,
    /// Virtual resolution of the pixel-perfect mode (`None`: draw straight at window resolution).
    pub pixel_perfect: Option<UVec2>,
    /// Letterbox bar colour (sRGB RGBA).
    pub bar_color: [f32; 4],
    /// Round the centre to whole (virtual) pixels.
    pub snap: bool,
    /// Clear colour of the 2D view (sRGB RGBA), `None` = draw over the 3D scene.
    pub clear: Option<[f32; 4]>,
    /// Only active cameras drive the view.
    pub active: bool,
}

impl Default for Camera2d {
    fn default() -> Self {
        Self {
            pixels_per_unit: 1.0,
            pixel_perfect: None,
            bar_color: [0.0, 0.0, 0.0, 1.0],
            snap: true,
            clear: None,
            active: true,
        }
    }
}

/// Makes a [`Camera2d`] follow an entity with exponential lag
/// (`current = lerp(current, target, 1 − exp(−speed · Δt))`; `speed` 0 locks on).
#[derive(Component, Reflect, Clone, Debug, PartialEq)]
#[reflect(Component)]
pub struct Camera2dFollow {
    /// The followed entity.
    pub target: Entity,
    /// Catch-up speed (1/s); 0 = locked to the target.
    pub speed: f32,
    /// Offset from the target in world units.
    pub offset: Vec2,
    /// Half size of a box around the centre inside which the target moves freely.
    pub deadzone: Vec2,
}

impl Camera2dFollow {
    /// Locks onto `target`.
    #[must_use]
    pub fn new(target: Entity) -> Self {
        Self {
            target,
            speed: 0.0,
            offset: Vec2::ZERO,
            deadzone: Vec2::ZERO,
        }
    }
}

impl Camera2d {
    /// The ortho camera this component describes at `transform` (rotation about Z).
    #[must_use]
    pub fn ortho(&self, transform: &Transform) -> OrthoCamera {
        let (rotation, _, _) = transform.rotation.to_euler(regia_core::glam::EulerRot::ZYX);
        OrthoCamera {
            center: transform.translation.truncate(),
            pixels_per_unit: self.pixels_per_unit,
            rotation,
        }
    }

    /// The pixel-perfect mode, if enabled.
    #[must_use]
    pub fn pixel_perfect_mode(&self) -> Option<PixelPerfect> {
        self.pixel_perfect.map(|size| PixelPerfect {
            virtual_size: UVec2::new(size.x.max(1), size.y.max(1)),
            bar_color: self.bar_color,
        })
    }

    /// The camera the renderer uses for a `window`: snapped to the render target's pixels when
    /// [`Self::snap`], and the render target's size (virtual size in pixel-perfect mode).
    fn effective(&self, transform: &Transform, window: UVec2) -> (OrthoCamera, UVec2) {
        let size = self.pixel_perfect_mode().map_or(window, |pp| pp.virtual_size);
        let camera = self.ortho(transform);
        (if self.snap { camera.snapped(size) } else { camera }, size)
    }

    /// The world point under a window pixel (origin top-left, +Y down: mouse/touch coordinates),
    /// or `None` on the letterbox bars.
    #[must_use]
    pub fn screen_to_world(&self, transform: &Transform, window: UVec2, point: Vec2) -> Option<Vec2> {
        let (camera, size) = self.effective(transform, window);
        let local = match self.pixel_perfect_mode() {
            Some(pp) => pp.window_to_virtual(window, point)?,
            None => point,
        };
        let viewport = Rect2::new(0.0, 0.0, size.x as f32, size.y as f32);
        Some(camera.view(viewport.size).screen_to_world(local, viewport))
    }

    /// The window pixel (origin top-left, +Y down) showing a world point.
    #[must_use]
    pub fn world_to_screen(&self, transform: &Transform, window: UVec2, point: Vec2) -> Vec2 {
        let (camera, size) = self.effective(transform, window);
        let viewport = Rect2::new(0.0, 0.0, size.x as f32, size.y as f32);
        let local = camera.view(viewport.size).world_to_screen(point, viewport);
        match self.pixel_perfect_mode() {
            Some(pp) => {
                let lb = pp.letterbox(window);
                let full = (pp.virtual_size * lb.scale).as_vec2();
                let origin = ((window.as_vec2() - full) * 0.5).floor();
                origin + local * lb.scale as f32
            }
            None => local,
        }
    }
}

/// The pinned follow easing: `1 − det_exp(−speed · dt)` for a positive speed (bit-identical
/// across platforms, spec 35 § 12.13), or a full snap (`1`) otherwise. One function, called by
/// [`follow_camera2d`] and pinned by its test.
fn follow_easing(speed: f32, dt: f32) -> f32 {
    if speed > 0.0 {
        1.0 - det_exp(-speed * dt)
    } else {
        1.0
    }
}

/// `PostUpdate`: moves every [`Camera2dFollow`] camera towards its target (deadzone, then lag).
#[tracing::instrument(skip_all)]
pub fn follow_camera2d(
    time: Res<Time>,
    targets: Query<&GlobalTransform>,
    mut cameras: Query<(&Camera2dFollow, &mut Transform), With<Camera2d>>,
) {
    let dt = time.delta_secs();
    for (follow, mut transform) in &mut cameras {
        let Ok(target) = targets.get(follow.target) else {
            continue;
        };
        let goal = target.translation().truncate() + follow.offset;
        let current = transform.translation.truncate();
        // Inside the deadzone the camera stays; outside it moves just enough to put the target
        // back on the deadzone edge.
        let delta = goal - current;
        // A negative or NaN deadzone (reflected, editable data) must not reach `clamp`, which
        // panics when min > max (ADR-1809): its magnitude is used, NaN counts as 0.
        let zone = |d: f32| if d.is_nan() { 0.0 } else { d.abs() };
        let (zx, zy) = (zone(follow.deadzone.x), zone(follow.deadzone.y));
        let excess = Vec2::new(delta.x - delta.x.clamp(-zx, zx), delta.y - delta.y.clamp(-zy, zy));
        let desired = current + excess;
        let next = current.lerp(desired, follow_easing(follow.speed, dt));
        transform.translation = Vec3::new(next.x, next.y, transform.translation.z);
    }
}

/// `PostUpdate`: copies the first active [`Camera2d`] (lowest entity) into `regia_2d::View2d`.
#[tracing::instrument(skip_all)]
pub fn write_view2d(cameras: Query<(Entity, &Camera2d, &Transform)>, view: Option<ResMut<regia_2d::View2d>>) {
    let Some(mut view) = view else { return };
    let Some((_, camera, transform)) = cameras
        .iter()
        .filter(|(_, c, _)| c.active)
        .min_by_key(|(e, _, _)| *e)
    else {
        return;
    };
    let next = regia_2d::View2d {
        camera: camera.ortho(transform),
        pixel_perfect: camera.pixel_perfect_mode(),
        snap: camera.snap,
        clear: camera.clear,
    };
    if *view != next {
        *view = next;
    }
}

/// Registers [`Camera2d`] / [`Camera2dFollow`] and runs [`follow_camera2d`] → [`write_view2d`]
/// before the 2D frame is extracted. Add it with `regia_2d::Sprite2dPlugin`.
#[derive(Default)]
pub struct Camera2dPlugin;

impl Plugin for Camera2dPlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<Camera2d>()
            .register_type::<Camera2dFollow>()
            .add_systems(
                PostUpdate,
                (follow_camera2d, write_view2d)
                    .chain()
                    .after(PostUpdateSet::TransformPropagate)
                    .before(regia_2d::extract_2d_frame),
            );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: Vec2, b: Vec2) -> bool {
        (a - b).abs().max_element() < 1e-3
    }

    /// Pin: the 2D follow easing uses the pinned `det_exp`, so a 30-frame chase lands on the same
    /// camera centre bits on every platform (a cursor or cull rect reads those bits). The pin
    /// calls the same [`follow_easing`] function the system evaluates: reverting that helper to
    /// the platform `exp()` flips these bits.
    #[test]
    fn follow_easing_is_pinned_across_platforms() {
        // 30 frames at dt = 1/60 with speed 8 and a target one unit further along x each frame.
        let dt = 1.0 / 60.0;
        let t = follow_easing(8.0, dt);
        assert_eq!(t.to_bits(), 0x3dff_a520);
        let mut current = Vec2::ZERO;
        for frame in 1..=30 {
            let goal = Vec2::new(frame as f32, 0.0);
            current += (goal - current) * t;
        }
        assert_eq!(current.x.to_bits(), 0x41b8_f03e);
        assert_eq!(current.y.to_bits(), 0);
    }

    #[test]
    fn conversions_round_trip_through_the_letterbox() {
        let camera = Camera2d {
            pixels_per_unit: 2.0,
            pixel_perfect: Some(UVec2::new(320, 180)),
            ..Camera2d::default()
        };
        let transform = Transform::from_translation(Vec3::new(100.0, 40.0, 0.0));
        let window = UVec2::new(1366, 768);
        // The window centre shows the camera centre.
        let centre = camera.screen_to_world(&transform, window, Vec2::new(683.0, 384.0));
        assert!(
            centre.is_some_and(|c| close(c, Vec2::new(100.0, 40.0))),
            "{centre:?}"
        );
        // Bars map to nothing.
        assert_eq!(
            camera.screen_to_world(&transform, window, Vec2::new(5.0, 5.0)),
            None
        );
        for world in [
            Vec2::new(100.0, 40.0),
            Vec2::new(60.25, 10.5),
            Vec2::new(150.0, 80.0),
        ] {
            let screen = camera.world_to_screen(&transform, window, world);
            let back = camera.screen_to_world(&transform, window, screen);
            assert!(
                back.is_some_and(|b| close(b, world)),
                "{world} -> {screen} -> {back:?}"
            );
        }
    }

    #[test]
    fn direct_mode_maps_world_units_to_window_pixels() {
        let camera = Camera2d::default();
        let transform = Transform::default();
        let window = UVec2::new(800, 600);
        assert!(close(
            camera.world_to_screen(&transform, window, Vec2::new(10.0, 10.0)),
            Vec2::new(410.0, 290.0)
        ));
    }

    /// ADR-1809: a negative or NaN deadzone neither panics nor changes the meaning of the zone.
    #[test]
    fn a_negative_or_nan_deadzone_does_not_panic() {
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            regia_scene::ScenePlugin,
            regia_2d::Sprite2dPlugin,
            Camera2dPlugin,
        ));
        app.insert_resource(TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_millis(100),
        ));
        let target = app
            .world_mut()
            .spawn(Transform::from_translation(Vec3::new(50.0, 20.0, 0.0)))
            .id();
        let camera = app
            .world_mut()
            .spawn((
                Camera2d::default(),
                Camera2dFollow {
                    deadzone: Vec2::new(-10.0, f32::NAN),
                    ..Camera2dFollow::new(target)
                },
            ))
            .id();
        app.update();
        app.update();
        let t = app
            .world()
            .get::<Transform>(camera)
            .map(|t| t.translation.truncate());
        // -10 acts as a 10-unit zone; NaN as none.
        assert_eq!(t, Some(Vec2::new(40.0, 20.0)));
    }

    #[test]
    fn follower_locks_or_lags_and_writes_the_view() {
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            regia_scene::ScenePlugin,
            regia_2d::Sprite2dPlugin,
            Camera2dPlugin,
        ));
        app.insert_resource(TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_millis(100),
        ));
        let target = app
            .world_mut()
            .spawn(Transform::from_translation(Vec3::new(50.0, 20.0, 0.0)))
            .id();
        let camera = app
            .world_mut()
            .spawn((
                Camera2d {
                    pixel_perfect: Some(UVec2::new(320, 180)),
                    ..Camera2d::default()
                },
                Camera2dFollow {
                    deadzone: Vec2::new(10.0, 0.0),
                    ..Camera2dFollow::new(target)
                },
            ))
            .id();
        app.update();
        app.update();
        let t = app
            .world()
            .get::<Transform>(camera)
            .map(|t| t.translation.truncate());
        // Locked on, minus the horizontal deadzone.
        assert_eq!(t, Some(Vec2::new(40.0, 20.0)));
        let view = app.world().resource::<regia_2d::View2d>();
        assert_eq!(view.camera.center, Vec2::new(40.0, 20.0));
        assert_eq!(
            view.pixel_perfect.map(|p| p.virtual_size),
            Some(UVec2::new(320, 180))
        );
        // With lag, the camera closes part of the gap per frame.
        if let Some(mut f) = app.world_mut().get_mut::<Camera2dFollow>(camera) {
            f.speed = 5.0;
            f.deadzone = Vec2::ZERO;
        }
        app.update();
        let x = app
            .world()
            .get::<Transform>(camera)
            .map(|t| t.translation.x)
            .unwrap_or_default();
        assert!(x > 40.0 && x < 50.0, "{x}");
    }
}
