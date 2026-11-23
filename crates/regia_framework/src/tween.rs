// SPDX-License-Identifier: MPL-2.0
//! Tweens (`docs/specs/14-scene-and-framework.md` § B6): animate any reflected `f32` property of
//! any component — position (`.translation.x`), colour channels, UI alpha — with easing.
//!
//! A [`Tween`] component on an entity animates one field of one of that entity's components from
//! `from` to `to` over `duration_secs`, advancing in [`FixedUpdate`] after the fixed script pass. When it
//! completes, the component is removed. Because it targets a reflected path, the same component
//! animates engine types (`Transform`) and game types alike — anything registered with
//! [`App::register_type`] whose leaf is an `f32`.
//!
//! Whole-value tweens of composites (a `Vec3` in one step, colours as one value) arrive with the
//! UI work that needs them (M20); animate the leaf fields meanwhile, e.g. three tweens for
//! `translation.{x,y,z}`.

use regia_app::prelude::*;
use regia_core::math::Easing;
use regia_ecs::reflect::GetPath;
use regia_scene::Transform;

use crate::detmath;

/// Animates one reflected `f32` field of one component on this entity (§ B6).
///
/// Build with [`Tween::field`]; convenience constructors for common Transform fields:
/// [`Tween::translation_axis`].
///
/// # Example
///
/// ```
/// use regia_core::math::Easing;
/// use regia_framework::tween::Tween;
///
/// let tween = Tween::field("Transform", ".translation.y", 0.0, 10.0, 1.0, Easing::Linear);
/// assert_eq!(tween.t(0.0), 0.0);
/// assert_eq!(tween.t(1.0), 1.0);
/// assert_eq!(tween.value_at(0.5), 5.0, "linear easing is halfway at t = 0.5");
/// ```
#[derive(Component, Reflect, Clone, Debug, PartialEq)]
#[reflect(Component, Default)]
pub struct Tween {
    /// Short type path of the target component, e.g. `"Transform"`.
    pub component: String,
    /// Reflected field path within the component, e.g. `".translation.y"`.
    pub path: String,
    /// Start value.
    pub from: f32,
    /// End value.
    pub to: f32,
    /// Total duration in seconds.
    pub duration_secs: f32,
    /// Elapsed game time in seconds.
    pub elapsed_secs: f32,
    /// The easing applied to the normalised time. Not reflected (the engine's `Easing` is a
    /// plain enum); serialises as part of the component's non-reflected state.
    #[reflect(ignore)]
    pub easing: Easing,
}

impl Default for Tween {
    fn default() -> Self {
        Self {
            component: "Transform".into(),
            path: ".translation.x".into(),
            from: 0.0,
            to: 1.0,
            duration_secs: 1.0,
            elapsed_secs: 0.0,
            easing: Easing::Linear,
        }
    }
}

impl Tween {
    /// Tweens `component`'s field at `path` from `from` to `to` over `secs` with `easing`.
    #[must_use]
    pub fn field(component: &str, path: &str, from: f32, to: f32, secs: f32, easing: Easing) -> Self {
        Self {
            component: component.into(),
            path: path.into(),
            from,
            to,
            duration_secs: secs.max(0.0),
            elapsed_secs: 0.0,
            easing,
        }
    }

    /// Convenience: tweens one axis (`'x' | 'y' | 'z'`) of this entity's `Transform.translation`.
    #[must_use]
    pub fn translation_axis(axis: char, from: f32, to: f32, secs: f32, easing: Easing) -> Self {
        Self::field(
            "Transform",
            &format!(".translation.{axis}"),
            from,
            to,
            secs,
            easing,
        )
    }

    /// Normalised progress `t ∈ [0, 1]` after `elapsed` seconds.
    #[must_use]
    pub fn t(&self, elapsed: f32) -> f32 {
        if self.duration_secs <= 0.0 {
            1.0
        } else {
            (elapsed / self.duration_secs).clamp(0.0, 1.0)
        }
    }

    /// The eased value at normalised time `t`.
    #[must_use]
    pub fn value_at(&self, t: f32) -> f32 {
        self.from + (self.to - self.from) * apply_easing(self.easing, t)
    }
}

/// Easing with deterministic kernels for every family that otherwise calls the host libm.
fn apply_easing(easing: Easing, t: f32) -> f32 {
    use std::f32::consts::{FRAC_PI_2, PI};

    let t = t.clamp(0.0, 1.0);
    match easing {
        Easing::SineIn => 1.0 - detmath::sin_cos(t * FRAC_PI_2).1,
        Easing::SineOut => detmath::sin_cos(t * FRAC_PI_2).0,
        Easing::SineInOut => (1.0 - detmath::sin_cos(t * PI).1) / 2.0,
        Easing::ExpoIn => {
            if t <= 0.0 {
                0.0
            } else {
                detmath::exp2(10.0 * t - 10.0)
            }
        }
        Easing::ExpoOut => {
            if t >= 1.0 {
                1.0
            } else {
                1.0 - detmath::exp2(-10.0 * t)
            }
        }
        Easing::ExpoInOut => {
            if t <= 0.0 {
                0.0
            } else if t >= 1.0 {
                1.0
            } else if t < 0.5 {
                detmath::exp2(20.0 * t - 10.0) / 2.0
            } else {
                (2.0 - detmath::exp2(10.0 - 20.0 * t)) / 2.0
            }
        }
        Easing::ElasticIn => {
            if t <= 0.0 {
                0.0
            } else if t >= 1.0 {
                1.0
            } else {
                -detmath::exp2(10.0 * t - 10.0) * detmath::sin_cos((10.0 * t - 10.75) * (2.0 * PI / 3.0)).0
            }
        }
        Easing::ElasticOut => 1.0 - apply_easing(Easing::ElasticIn, 1.0 - t),
        Easing::ElasticInOut => {
            if t <= 0.0 {
                0.0
            } else if t >= 1.0 {
                1.0
            } else {
                let wave = detmath::sin_cos((20.0 * t - 11.125) * (2.0 * PI / 4.5)).0;
                if t < 0.5 {
                    -detmath::exp2(20.0 * t - 10.0) * wave / 2.0
                } else {
                    detmath::exp2(10.0 - 20.0 * t) * wave / 2.0 + 1.0
                }
            }
        }
        _ => easing.apply(t),
    }
}

/// `FixedUpdate` (after the fixed script pass): advances every tween and writes its eased value into the
/// reflected field. Completed tweens are removed.
///
/// A tween whose component or path does not resolve (type not registered, entity lost the
/// component, field not an `f32`) logs one warning and removes itself — user-authored data must
/// never panic or spin forever. Exclusive because reflected writes need `&mut World`.
#[tracing::instrument(skip_all, target = "regia_framework", name = "tweens_advance")]
fn advance_tweens(world: &mut World) {
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let delta = world.resource::<Time>().delta_secs();

    let entities: Vec<Entity> = world
        .query_filtered::<Entity, With<Tween>>()
        .iter(world)
        .collect();
    for entity in entities {
        let Some(mut tween) = world.get_mut::<Tween>(entity) else {
            continue;
        };
        tween.elapsed_secs += delta;
        let t = tween.t(tween.elapsed_secs);
        let value = tween.value_at(t);
        let done = t >= 1.0;
        let (component, path) = (tween.component.clone(), tween.path.clone());

        // A non-finite value (a NaN or infinite `from`/`to` built in a script) is never written:
        // it would reach the transform hierarchy and culling every frame until the tween ended
        // (audit-gameplay Medium). The tween is dropped like one whose target does not resolve.
        let applied = if value.is_finite() {
            apply_value(world, &registry, entity, &component, &path, value)
        } else {
            tracing::warn!(target: "regia_framework", "tween {component}{path} produced {value}; dropped");
            false
        };
        if (done || !applied)
            && let Ok(mut entity_mut) = world.get_entity_mut(entity)
        {
            entity_mut.remove::<Tween>();
        }
    }
}

/// Writes `value` into the tween's reflected field. Returns false (with one warning) when the
/// target cannot be resolved.
fn apply_value(
    world: &mut World,
    registry: &regia_ecs::reflect::TypeRegistry,
    entity: Entity,
    component: &str,
    path: &str,
    value: f32,
) -> bool {
    let Some(registration) = registry
        .get_with_short_type_path(component)
        .or_else(|| regia_core::brand::lookup_type_path(component, |p| registry.get_with_type_path(p)))
    else {
        tracing::warn!(target: "regia_framework", "tween target {component} is not a registered type");
        return false;
    };
    let Some(reflect_component) = registration.data::<ReflectComponent>() else {
        tracing::warn!(target: "regia_framework", "tween target {component} is not a reflected component");
        return false;
    };
    let Ok(mut entity_mut) = world.get_entity_mut(entity) else {
        return false;
    };
    let Some(mut reflected) = reflect_component.reflect_mut(&mut entity_mut) else {
        tracing::warn!(target: "regia_framework", "tween target entity has no {component}");
        return false;
    };
    let Ok(field) = reflected.as_reflect_mut().reflect_path_mut(normalized_path(path)) else {
        tracing::warn!(target: "regia_framework", "tween path {component}{path} does not resolve");
        return false;
    };
    match field.try_downcast_mut::<f32>() {
        Some(slot) => {
            *slot = value;
            true
        }
        None => {
            tracing::warn!(target: "regia_framework", "tween path {component}{path} is not an f32");
            false
        }
    }
}

/// Strips the leading dot some authoring styles use (`.translation.y` → `translation.y`), the
/// form `GetPath::reflect_path_mut` expects.
fn normalized_path(path: &str) -> &str {
    path.trim_start_matches('.')
}
/// Registers the [`Tween`] type (and [`Transform`], the most common target) and advances tweens
/// every [`FixedUpdate`].
pub struct TweenPlugin;

impl Plugin for TweenPlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<Tween>()
            .register_type::<Transform>()
            .add_systems(
                FixedUpdate,
                advance_tweens.after(crate::script::ScriptsSet::FixedUpdate),
            );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_scene::Transform;
    use std::time::Duration;

    fn make_app() -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, TweenPlugin))
            .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(100)));
        app.world_mut().resource_mut::<FixedTime>().hz = 10.0;
        app
    }

    #[test]
    fn a_non_finite_tween_never_writes_its_field() {
        let mut app = make_app();
        let entity = app
            .world_mut()
            .spawn((
                Transform::from_translation(regia_core::math::Vec3::new(0.0, 3.0, 0.0)),
                Tween::translation_axis('y', f32::NAN, 10.0, 1.0, Easing::Linear),
            ))
            .id();
        app.update();
        app.update();
        assert_eq!(
            app.world().get::<Transform>(entity).map(|t| t.translation.y),
            Some(3.0)
        );
        assert!(
            app.world().get::<Tween>(entity).is_none(),
            "the bad tween is removed"
        );
    }

    #[test]
    fn tweens_advance_on_the_same_fixed_steps_under_different_frame_pacing() {
        fn run(frame_steps: &[u32]) -> Vec<u32> {
            #[derive(Resource, Default)]
            struct Seen(Vec<u32>);

            let mut app = make_app();
            let mut fixed = app.world_mut().resource_mut::<FixedTime>();
            fixed.hz = 60.0;
            fixed.max_substeps = 8;
            let entity = app
                .world_mut()
                .spawn((
                    Transform::default(),
                    Tween::translation_axis('x', 0.0, 1.0, 0.1, Easing::Linear),
                ))
                .id();
            app.init_resource::<Seen>().add_systems(
                FixedUpdate,
                (move |mut seen: ResMut<Seen>, transforms: Query<&Transform>| {
                    let value = transforms.get(entity).map_or(f32::NAN, |t| t.translation.x);
                    seen.0.push(value.to_bits());
                })
                .after(advance_tweens),
            );
            let step = app.world().resource::<FixedTime>().step();
            for &steps in frame_steps {
                app.insert_resource(TimeUpdateStrategy::ManualDuration(step * steps));
                app.update();
            }
            app.world_mut().remove_resource::<Seen>().unwrap_or_default().0
        }

        let hitch = run(&[3]);
        let short = run(&[1, 1, 1]);
        assert_eq!(hitch, short, "the same fixed steps see the same transform bits");
        assert_ne!(
            hitch[0],
            0.0_f32.to_bits(),
            "the first fixed step advances the tween"
        );
        assert!(
            hitch.windows(2).all(|pair| pair[0] != pair[1]),
            "every fixed step advances"
        );
    }

    #[test]
    fn linear_tween_reaches_the_target_and_removes_itself() {
        let mut app = make_app();
        let entity = app
            .world_mut()
            .spawn((
                Transform::default(),
                Tween::translation_axis('y', 0.0, 10.0, 1.0, Easing::Linear),
            ))
            .id();

        for _ in 0..10 {
            app.update(); // 1.0 s total
        }
        let transform = app.world().get::<Transform>(entity).expect("transform");
        assert!(
            (transform.translation.y - 10.0).abs() < 1e-4,
            "y = {}",
            transform.translation.y
        );
        assert!(
            app.world().get::<Tween>(entity).is_none(),
            "completed tween removed"
        );
    }

    #[test]
    fn eased_midpoint_matches_the_easing_curve() {
        let mut app = make_app();
        let entity = app
            .world_mut()
            .spawn((
                Transform::default(),
                Tween::translation_axis('x', 0.0, 4.0, 2.0, Easing::QuadIn),
            ))
            .id();

        for _ in 0..10 {
            app.update(); // 1.0 s = t 0.5
        }
        // QuadIn(0.5) = 0.25 → value 1.0.
        let transform = app.world().get::<Transform>(entity).expect("transform");
        assert!(
            (transform.translation.x - 1.0).abs() < 1e-4,
            "x = {}",
            transform.translation.x
        );
    }

    #[test]
    fn unresolvable_target_removes_the_tween_without_panicking() {
        let mut app = make_app();
        let entity = app
            .world_mut()
            .spawn((
                Transform::default(),
                Tween::field("NoSuchComponent", ".x", 0.0, 1.0, 1.0, Easing::Linear),
            ))
            .id();

        app.update();
        assert!(app.world().get::<Tween>(entity).is_none(), "bad tween dropped");
        assert!(app.world().get::<Transform>(entity).is_some(), "entity untouched");
    }

    #[test]
    fn a_non_finite_tween_value_is_never_written() {
        let mut app = make_app();
        let start = Transform::from_translation(regia_core::math::Vec3::new(3.0, 4.0, 5.0));
        let nan = app
            .world_mut()
            .spawn((
                start,
                Tween::field("Transform", ".translation.x", f32::NAN, 1.0, 1.0, Easing::Linear),
            ))
            .id();
        let inf = app
            .world_mut()
            .spawn((
                start,
                Tween::field(
                    "Transform",
                    ".translation.y",
                    0.0,
                    f32::INFINITY,
                    1.0,
                    Easing::Linear,
                ),
            ))
            .id();
        app.update();
        for entity in [nan, inf] {
            assert!(app.world().get::<Tween>(entity).is_none(), "bad tween dropped");
            let t = app.world().get::<Transform>(entity).expect("transform");
            assert_eq!(t.translation, start.translation, "nothing written");
        }
    }

    #[test]
    fn non_f32_field_is_rejected() {
        let mut app = make_app();
        let entity = app
            .world_mut()
            .spawn((
                Transform::default(),
                // `.rotation` is a Quat, not an f32.
                Tween::field("Transform", ".rotation", 0.0, 1.0, 1.0, Easing::Linear),
            ))
            .id();
        let position = app
            .world_mut()
            .spawn((
                Transform::default(),
                Tween::field("Transform", ".translation", 0.0, 1.0, 1.0, Easing::Linear),
            ))
            .id();

        app.update();
        assert!(
            app.world().get::<Tween>(entity).is_none(),
            "non-f32 tween dropped"
        );
        assert!(
            app.world().get::<Tween>(position).is_none(),
            "a whole translation is not an f32 leaf"
        );
        assert_eq!(
            app.world()
                .get::<Transform>(position)
                .expect("transform")
                .translation,
            Transform::default().translation
        );
    }

    #[test]
    fn transcendental_tweens_use_deterministic_easing_bits() {
        let sine = Tween::field("Transform", ".translation.x", 0.0, 1.0, 1.0, Easing::SineOut);
        assert_eq!(sine.value_at(0.01306).to_bits(), 0x3ca8_0b37);
        let expo = Tween::field("Transform", ".translation.x", 0.0, 1.0, 1.0, Easing::ExpoIn);
        assert_eq!(expo.value_at(0.02088).to_bits(), 0x3a93_eeda);
    }

    #[test]
    fn deterministic_easings_stay_on_the_authored_curves() {
        for &easing in Easing::ALL {
            let tween = Tween::field("Transform", ".translation.x", 0.0, 1.0, 1.0, easing);
            for step in 0..=100 {
                let t = step as f32 / 100.0;
                let actual = tween.value_at(t);
                let expected = easing.apply(t);
                assert!(
                    (actual - expected).abs() <= 1e-5,
                    "{easing:?}({t}) = {actual}, expected {expected}"
                );
            }
        }
    }

    #[test]
    fn tween_value_at_matches_easing() {
        let tween = Tween::field("Transform", ".translation.y", 0.0, 2.0, 1.0, Easing::SineOut);
        assert!((tween.value_at(0.5) - 2.0 * Easing::SineOut.apply(0.5)).abs() < 1e-6);
        assert_eq!(tween.t(0.5), 0.5);
        assert_eq!(tween.t(2.0), 1.0, "clamped");
    }
}
