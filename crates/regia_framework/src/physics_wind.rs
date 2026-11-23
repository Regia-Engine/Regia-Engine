// SPDX-License-Identifier: MPL-2.0
//! Wind on rigid bodies (ADR-3348, `book/agents/physics-fields.md`): a [`WindDrag`] body feels
//! quadratic drag from the air moving past it, using the scene wind ([`SceneWind`]) as it is,
//! gusts included.
//!
//! ```text
//! FixedUpdate (PhysicsStep): … → apply_force_fields → apply_wind_drag → step_physics
//! ```
//!
//! The drag is `F = -½ ρ Cd A |u| u` with `u = v - wind` the body's velocity relative to the air.
//! It is applied as a velocity update before the step (`u' = u (1 - ½ ρ Cd A |u| dt / m)`,
//! clamped at zero so the body never overshoots the wind), so a body falling in still air
//! settles on `sqrt(2 m g / (ρ Cd A))` exactly in the discrete system. The wind is the 10 m
//! surface wind on world X and Z and is applied at every height; without a [`SceneWind`], or in
//! `SceneWindMode::Off`, the air is still. Bodies without [`WindDrag`] are untouched, and nothing
//! runs while no body carries it.
//!
//! Provenance: quadratic drag is the textbook drag equation (Wikipedia, "Drag (physics)";
//! "Terminal velocity"); the sea-level air density 1.225 kg/m³ is the International Standard
//! Atmosphere value. The update is original.

use regia_app::FixedTime;
use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_physics::convert::{from_rapier_vec3, to_rapier_vec3};
use regia_physics::{PhysicsStep, PhysicsWorld, RigidBodyHandle, apply_force_fields, step_physics};
use regia_scene::weather::{SceneWind, SceneWindMode};
use tracing::{debug_span, instrument};

/// The sea-level air density of the International Standard Atmosphere (kg/m³).
pub const SEA_LEVEL_AIR_DENSITY: f32 = 1.225;

/// Quadratic wind drag on a rigid body: `F = -½ ρ cd area |u| u`.
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq)]
#[reflect(Component, Default)]
pub struct WindDrag {
    /// Reference area facing the air (m²).
    pub area: f32,
    /// Drag coefficient (about 0.47 for a sphere, 1.05 for a cube, 1.28 for a flat plate).
    pub cd: f32,
}

impl Default for WindDrag {
    fn default() -> Self {
        Self { area: 1.0, cd: 1.0 }
    }
}

/// The air density the drag uses (kg/m³).
#[derive(Resource, Reflect, Debug, Clone, Copy, PartialEq)]
#[reflect(Resource, Default)]
pub struct AirDensity(pub f32);

impl Default for AirDensity {
    fn default() -> Self {
        Self(SEA_LEVEL_AIR_DENSITY)
    }
}

/// The wind vector bodies feel: the scene wind with gusts, or still air.
#[must_use]
pub fn air_velocity(scene: Option<&SceneWind>) -> Vec3 {
    match scene {
        Some(w) if w.mode != SceneWindMode::Off => {
            let [x, y, z] = w.velocity3();
            Vec3::new(x, y, z)
        }
        _ => Vec3::ZERO,
    }
}

/// The velocity after one step of drag: `wind + (v - wind) (1 - ½ ρ cd A |v - wind| dt / m)`,
/// the factor clamped at zero. `mass <= 0`, a negative density, a negative `dt`, or any
/// non-finite input gives `v` back. A negative density would make the factor larger than one
/// and turn drag into thrust; a non-finite `dt` would snap the body to the wind.
#[must_use]
pub fn drag_velocity(v: Vec3, wind: Vec3, drag: &WindDrag, density: f32, mass: f32, dt: f32) -> Vec3 {
    if !(mass > 0.0
        && mass.is_finite()
        && v.is_finite()
        && wind.is_finite()
        && drag.area.is_finite()
        && drag.cd.is_finite()
        && density.is_finite()
        && density >= 0.0
        && dt.is_finite()
        && dt >= 0.0)
    {
        return v;
    }
    let rel = v - wind;
    let rate = 0.5 * density.max(0.0) * drag.cd.max(0.0) * drag.area.max(0.0) * rel.length() / mass;
    let out = wind + rel * (1.0 - rate * dt).max(0.0);
    if out.is_finite() { out } else { v }
}

/// Applies [`WindDrag`] to the bodies that carry it, in rapier handle order.
///
/// The order reaches rapier: every `get_mut` queues the body as modified and a velocity change
/// wakes it, and rapier handles that queue (island wake-ups) in push order. The query's order
/// is the ECS table layout, which depends on each entity's component-insertion history (a
/// replay, a net client or a loaded scene can differ from the live run), so the bodies are
/// sorted by their rapier handle instead: rapier inserts bodies in spawn order (ADR-1347), so
/// the handle order is the same in every run. `order` is kept between steps, so a steady
/// scene allocates nothing.
#[instrument(skip_all, target = "regia_framework")]
pub fn apply_wind_drag(
    fixed: Res<FixedTime>,
    wind: Option<Res<SceneWind>>,
    density: Res<AirDensity>,
    mut physics: ResMut<PhysicsWorld>,
    bodies: Query<(&'static RigidBodyHandle, &'static WindDrag)>,
    mut order: Local<Vec<(RigidBodyHandle, WindDrag)>>,
) {
    let _span = debug_span!("apply_wind_drag").entered();
    let dt = fixed.step().as_secs_f32();
    let air = air_velocity(wind.as_deref());
    order.clear();
    order.extend(bodies.iter().map(|(handle, drag)| (*handle, *drag)));
    // Live handles are unique, so the unstable sort (which allocates nothing) is repeatable.
    order.sort_unstable_by_key(|(handle, _)| (handle.raw, handle.generation));
    for (handle, drag) in order.iter() {
        let Some(rb) = physics.bodies.get_mut(handle.to_handle()) else {
            continue;
        };
        if !rb.is_dynamic() {
            continue;
        }
        let v = from_rapier_vec3(rb.linvel());
        let next = drag_velocity(v, air, drag, density.0, rb.mass(), dt);
        if next != v {
            rb.set_linvel(to_rapier_vec3(next), true);
        }
    }
}

/// Run condition: some body carries [`WindDrag`].
fn any_wind_drag(bodies: Query<(), With<WindDrag>>) -> bool {
    !bodies.is_empty()
}

/// Registers [`WindDrag`] and [`AirDensity`] and runs [`apply_wind_drag`] between the force
/// fields and the physics step. Add it after `regia_physics::PhysicsPlugin`.
#[derive(Default)]
pub struct PhysicsWindPlugin;

impl Plugin for PhysicsWindPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<AirDensity>()
            .register_type::<WindDrag>()
            .register_type::<AirDensity>()
            .add_systems(
                FixedUpdate,
                apply_wind_drag
                    .run_if(any_wind_drag)
                    .after(apply_force_fields)
                    .before(step_physics)
                    .in_set(PhysicsStep),
            );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_app::{FixedTime, TimeUpdateStrategy};
    use regia_physics::{Collider, ColliderShape, Mass, PhysicsPlugin, PhysicsSettings, RigidBody};
    use regia_scene::Transform;
    use std::time::Duration;

    fn app(gravity: Vec3) -> App {
        let mut app = App::new();
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
            1.0 / 60.0,
        )));
        let mut fixed = FixedTime::default();
        fixed.hz = 60.0;
        app.insert_resource(fixed);
        app.insert_resource(PhysicsSettings {
            gravity,
            ..PhysicsSettings::default()
        });
        app.add_plugins((PhysicsPlugin, PhysicsWindPlugin));
        app
    }

    fn body(app: &mut App, mass: f32, drag: WindDrag) -> Entity {
        app.world_mut()
            .spawn((
                Transform::from_translation(Vec3::new(0.0, 5000.0, 0.0)),
                RigidBody::dynamic(),
                Mass::Explicit {
                    mass,
                    center_of_mass: Vec3::ZERO,
                    principal_inertia: Vec3::splat(0.5),
                },
                Collider {
                    shape: ColliderShape::Sphere { radius: 0.05 },
                    sensor: true,
                    density: 0.0,
                    ..Collider::default()
                },
                drag,
            ))
            .id()
    }

    fn velocity(app: &App, e: Entity) -> Vec3 {
        let handle = app.world().get::<RigidBodyHandle>(e).expect("handle").to_handle();
        let rb = app
            .world()
            .resource::<PhysicsWorld>()
            .bodies
            .get(handle)
            .expect("body");
        from_rapier_vec3(rb.linvel())
    }

    #[test]
    fn falling_body_reaches_wind_drag_terminal_velocity() {
        let (m, cd, a) = (2.0f32, 0.8f32, 0.5f32);
        let mut app = app(Vec3::new(0.0, -9.81, 0.0));
        let e = body(&mut app, m, WindDrag { area: a, cd });
        for _ in 0..1800 {
            app.update();
        }
        let terminal = (2.0 * m * 9.81 / (SEA_LEVEL_AIR_DENSITY * cd * a)).sqrt();
        let v = -velocity(&app, e).y;
        assert!(
            (v / terminal - 1.0).abs() < 0.02,
            "v = {v}, terminal = {terminal}"
        );
    }

    #[test]
    fn body_in_wind_is_carried_toward_the_wind_speed_including_gusts() {
        let mut app = app(Vec3::ZERO);
        app.insert_resource(SceneWind::manual([6.0, -2.0]));
        let e = body(&mut app, 1.0, WindDrag { area: 1.0, cd: 1.0 });
        for _ in 0..1200 {
            app.update();
        }
        let v = velocity(&app, e);
        assert!(
            (v.x - 6.0).abs() < 0.1 && (v.z + 2.0).abs() < 0.1 && v.y.abs() < 1e-4,
            "{v:?}"
        );
        // The gusting value is read as it is: a stronger instantaneous wind pushes harder.
        let mut gusty = SceneWind::manual([6.0, -2.0]);
        gusty.velocity_m_per_s = [12.0, -2.0];
        app.insert_resource(gusty);
        for _ in 0..1200 {
            app.update();
        }
        assert!((velocity(&app, e).x - 12.0).abs() < 0.2);
        // Off: still air, the body is braked to rest.
        app.insert_resource(SceneWind {
            mode: SceneWindMode::Off,
            ..SceneWind::CALM
        });
        for _ in 0..3000 {
            app.update();
        }
        assert!(velocity(&app, e).length() < 0.05);
    }

    #[test]
    fn drag_never_overshoots_and_ignores_bad_input() {
        let drag = WindDrag { area: 100.0, cd: 2.0 };
        let v = Vec3::new(50.0, 0.0, 0.0);
        let wind = Vec3::new(3.0, 0.0, 0.0);
        let out = drag_velocity(v, wind, &drag, 1.225, 0.1, 1.0 / 60.0);
        assert_eq!(out, wind, "a huge drag stops at the wind speed, not past it");
        assert_eq!(drag_velocity(v, wind, &drag, 1.225, 0.0, 0.016), v);
        assert_eq!(
            drag_velocity(
                v,
                wind,
                &WindDrag {
                    area: f32::NAN,
                    cd: 1.0
                },
                1.225,
                1.0,
                0.016
            ),
            v
        );
        // A non-positive density applies no drag instead of amplifying the speed.
        assert_eq!(drag_velocity(v, wind, &drag, -1.0, 1.0, 0.016), v);
        assert_eq!(drag_velocity(v, wind, &drag, 0.0, 1.0, 0.016), v);
    }

    #[test]
    fn negative_density_does_not_turn_drag_into_thrust() {
        let drag = WindDrag { area: 1.0, cd: 1.0 };
        let v = Vec3::X * 10.0;
        let out = drag_velocity(v, Vec3::ZERO, &drag, -1.225, 1.0, 1.0 / 60.0);
        assert_eq!(out, v, "a negative density must not accelerate the body: {out}");
    }

    #[test]
    fn a_non_finite_step_leaves_the_velocity() {
        let drag = WindDrag { area: 1.0, cd: 1.0 };
        let v = Vec3::X * 50.0;
        let wind = Vec3::X * 3.0;
        assert_eq!(
            drag_velocity(v, wind, &drag, 1.225, 1.0, f32::NAN),
            v,
            "NaN dt must not snap the body to the wind"
        );
    }

    /// A marker that moves an entity into another table.
    #[derive(Component)]
    struct Shuffle;

    /// Twelve stacked, sleeping `WindDrag` boxes woken by a gust; `shuffle` reverses their
    /// table order (a marker inserted in reverse order) just before the gust. Returns the `WindDrag`
    /// query's order at the gust, rapier's awake-body order after the gust's first step (the
    /// order the bodies woke in) and a digest of the final poses and velocities.
    fn gust_digest(shuffle: bool) -> (Vec<u64>, Vec<(u32, u32)>, u64) {
        let mut app = app(Vec3::new(0.0, -9.81, 0.0));
        app.insert_resource(AirDensity(0.0));
        app.world_mut().spawn((
            Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
            RigidBody::static_body(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(50.0, 0.5, 50.0),
                },
                ..Collider::default()
            },
        ));
        let mut boxes = Vec::new();
        for stack in 0..4 {
            for level in 0..3 {
                let at = Vec3::new(stack as f32 * 0.6, 0.25 + level as f32 * 0.5, 0.0);
                boxes.push(
                    app.world_mut()
                        .spawn((
                            Transform::from_translation(at),
                            RigidBody::dynamic(),
                            Collider {
                                shape: ColliderShape::Box {
                                    half_extents: Vec3::splat(0.25),
                                },
                                ..Collider::default()
                            },
                            WindDrag { area: 0.25, cd: 1.05 },
                        ))
                        .id(),
                );
            }
            // Each stack settles and sleeps before the next one is spawned, so each sleeps in
            // its own rapier chunk and the gust wakes the chunks in the order it touches them.
            for _ in 0..300 {
                app.update();
            }
        }
        let asleep = |app: &App| {
            let physics = app.world().resource::<PhysicsWorld>();
            boxes
                .iter()
                .filter(|e| {
                    let h = app
                        .world()
                        .get::<RigidBodyHandle>(**e)
                        .expect("handle")
                        .to_handle();
                    physics.bodies.get(h).is_some_and(|rb| rb.is_sleeping())
                })
                .count()
        };
        assert_eq!(asleep(&app), boxes.len(), "the stacks fell asleep");
        if shuffle {
            // Moving every box into the marker's table in reverse order reverses the table
            // order the `WindDrag` query walks (a move out of a table swap-removes, so an insert
            // followed by a remove would only rotate it).
            for e in boxes.iter().rev() {
                app.world_mut().entity_mut(*e).insert(Shuffle);
            }
        }
        let query_order: Vec<u64> = app
            .world_mut()
            .query_filtered::<Entity, With<WindDrag>>()
            .iter(app.world())
            .map(Entity::to_bits)
            .collect();
        app.insert_resource(AirDensity(SEA_LEVEL_AIR_DENSITY));
        app.insert_resource(SceneWind::manual([9.0, 4.0]));
        app.update();
        let woke: Vec<(u32, u32)> = app
            .world()
            .resource::<PhysicsWorld>()
            .islands
            .active_bodies()
            .map(|h| h.into_raw_parts())
            .collect();
        assert_eq!(woke.len(), boxes.len(), "the gust woke every box");
        for _ in 0..179 {
            app.update();
        }
        let physics = app.world().resource::<PhysicsWorld>();
        let mut digest = 0xcbf2_9ce4_8422_2325u64;
        for e in &boxes {
            let h = app
                .world()
                .get::<RigidBodyHandle>(*e)
                .expect("handle")
                .to_handle();
            let rb = physics.bodies.get(h).expect("body");
            let (p, r, v) = (rb.translation(), rb.rotation(), rb.linvel());
            for x in [p.x, p.y, p.z, r.x, r.y, r.z, r.w, v.x, v.y, v.z] {
                digest = (digest ^ u64::from(x.to_bits())).wrapping_mul(0x0100_0000_01b3);
            }
        }
        (query_order, woke, digest)
    }

    /// Review rev-kimi-20 finding 2: the drag reaches rapier in a fixed body order (rapier
    /// handle order), so the order sleeping bodies wake in (rapier's awake-body order) does not
    /// depend on the ECS table layout (a replay, a net client or a loaded scene may have
    /// another component-insertion history than the live run). The final state is compared
    /// too; in rapier 0.36.0 it did not differ even when the wake order did (ADR-5899).
    #[test]
    fn the_gust_result_does_not_depend_on_the_table_order() {
        let (query, woke, digest) = gust_digest(false);
        assert_eq!(
            (query.clone(), woke.clone(), digest),
            gust_digest(false),
            "the run repeats"
        );
        let (query_shuffled, woke_shuffled, digest_shuffled) = gust_digest(true);
        assert_ne!(query, query_shuffled, "the shuffle changes the query order");
        assert_eq!(
            woke, woke_shuffled,
            "the same bodies in another table order wake in another order"
        );
        assert_eq!(
            digest, digest_shuffled,
            "the same bodies in another table order end elsewhere"
        );
    }
}
