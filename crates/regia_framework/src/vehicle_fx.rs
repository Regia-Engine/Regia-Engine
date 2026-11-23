// SPDX-License-Identifier: MPL-2.0
//! Wheel surface effects on a running vehicle: [`VehicleFxPlugin`].
//!
//! The pure logic is [`regia_vfx::vehicle_fx`]: slip, smoke, dust and spray intensities, skid marks
//! and ruts, the tier-capped stamp pool. This plugin is the bridge. Every fixed step, after the
//! physics step, it reads each [`VehicleController`]'s wheels ([`regia_physics::vehicle::WheelState`]:
//! grounded, contact point, normal, ground entity, spin angle), builds a
//! [`WheelSample`] and feeds [`VehicleFx::step`]. **It does not touch the vehicle code**: the slip
//! comes from the contact point moving between fixed steps and the wheel's heading and spin, not from
//! the tyre model, so it works with every drivetrain (including `Simple`, whose
//! `WheelState::slip_ratio` is 0).
//!
//! **Surface** of a wheel, first match wins:
//! 1. a [`WheelSurfaceOverride`] on the ground entity;
//! 2. the ground entity's [`PhysicsMaterialRef`] name (`"gravel_rough"` is gravel, see
//!    [`WheelSurface::from_material_name`]);
//! 3. the host's terrain probe ([`VehicleFxState::terrain_probe`]: a function from a world position
//!    to a surface, which a game backs with its terrain layers);
//! 4. [`VehicleFxState::default_surface`] (asphalt).
//!
//! **Output.** One effect instance per wheel and effect (`tire_smoke`, `wheel_dust`,
//! `wheel_spray`, registered on first use) follows the contact point with the intensity from the
//! logic; each stamp becomes a decal in the [`DecalPool`] (capped by the quality tier and by half the
//! pool, retired oldest first, fading out over the last 30 % of its life).
//!
//! **Tier.** The stamp cap and the effect density come from the app's `RenderSettings` preset
//! ([`VehicleFxLimits::for_tier`]); a limit the agent sets sticks until the tier moves. With no
//! `RenderSettings` (a headless app) the High numbers stand.
//!
//! Needs `PhysicsPlugin` and `VehiclePlugin`; the decal pool and the effect scene come from
//! `VfxPlugin` (without them the logic still runs and the report works). Add it after those.

use std::collections::VecDeque;
use std::sync::Arc;

use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_physics::vehicle::VehicleController;
use regia_physics::{PhysicsMaterialRef, PhysicsWorld, step_physics};
use regia_vfx::decal::{DecalId, DecalPool};
use regia_vfx::scene::VfxScene;
use regia_vfx::vehicle_fx::{
    TIRE_SMOKE, VehicleFx, VehicleFxConfig, VehicleFxLimits, WHEEL_DUST, WHEEL_SPRAY, WheelOutput,
    WheelSample, WheelSurface, ensure_vehicle_effect,
};

/// Forces the surface of the wheels that touch this entity (put it next to the ground's collider).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct WheelSurfaceOverride(pub WheelSurface);

/// A host function from a world position to the surface there (terrain layers).
pub type TerrainSurfaceProbe = Arc<dyn Fn(Vec3) -> Option<WheelSurface> + Send + Sync>;

/// The wheel width as a fraction of the wheel radius (a vehicle config has no tyre width).
const HALF_WIDTH_PER_RADIUS: f32 = 0.4;

/// What the report says about one wheel.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WheelFxReport {
    /// The wheel's key (see [`wheel_key`]).
    pub key: u64,
    /// The surface it rolled on at the last step.
    pub surface: WheelSurface,
    /// The logic's output at the last step.
    pub output: WheelOutput,
    /// Effects playing for the wheel now, as names.
    pub active: [Option<&'static str>; 3],
}

/// The plugin's state: the logic, the bookkeeping that maps it onto decals and effect instances,
/// and the settings.
#[derive(Resource)]
pub struct VehicleFxState {
    /// Switches the whole feature; off stops the effects and clears the marks.
    pub enabled: bool,
    /// The surface when nothing else names one.
    pub default_surface: WheelSurface,
    /// The terrain-layer hook (step 3 of the surface lookup).
    pub terrain_probe: Option<TerrainSurfaceProbe>,
    fx: VehicleFx,
    samples: Vec<WheelSample>,
    outputs: Vec<WheelOutput>,
    stamps: VecDeque<(u64, DecalId)>,
    instances: Vec<WheelInstances>,
    spin: Vec<(u64, f32)>,
    tier_seen: Option<regia_render::QualityTier>,
    last_dt: f32,
}

#[derive(Clone, Copy, Debug)]
struct WheelInstances {
    key: u64,
    ids: [Option<u64>; 3],
}

const EFFECTS: [&str; 3] = [TIRE_SMOKE, WHEEL_DUST, WHEEL_SPRAY];

impl Default for VehicleFxState {
    fn default() -> Self {
        Self {
            enabled: true,
            default_surface: WheelSurface::Asphalt,
            terrain_probe: None,
            fx: VehicleFx::new(VehicleFxConfig::default()),
            samples: Vec::new(),
            outputs: Vec::new(),
            stamps: VecDeque::new(),
            instances: Vec::new(),
            spin: Vec::new(),
            tier_seen: None,
            last_dt: 1.0 / 60.0,
        }
    }
}

/// A key that is stable for one wheel of one vehicle.
#[must_use]
pub fn wheel_key(vehicle: Entity, wheel: usize) -> u64 {
    vehicle.to_bits() ^ ((wheel as u64 & 0xff) << 56)
}

impl VehicleFxState {
    /// The logic (read-only).
    #[must_use]
    pub fn logic(&self) -> &VehicleFx {
        &self.fx
    }

    /// The tuning.
    #[must_use]
    pub fn config(&self) -> VehicleFxConfig {
        *self.fx.config()
    }

    /// Replaces the tuning (the stamp cap included).
    pub fn set_config(&mut self, config: VehicleFxConfig) {
        self.fx.set_config(config);
    }

    /// One report per tracked wheel, in first-seen order.
    #[must_use]
    pub fn wheels(&self) -> Vec<WheelFxReport> {
        self.fx
            .outputs()
            .map(|(output, surface)| {
                let levels = [output.smoke, output.dust, output.spray];
                WheelFxReport {
                    key: output.key,
                    surface,
                    output,
                    active: std::array::from_fn(|n| (levels[n] > 0.0).then_some(EFFECTS[n])),
                }
            })
            .collect()
    }

    /// Forgets every mark and stops every effect instance on the next step.
    pub fn clear_marks(&mut self) {
        self.fx.clear();
    }

    fn spin_of(&mut self, key: u64, angle: f32) -> Option<f32> {
        if let Some(entry) = self.spin.iter_mut().find(|(k, _)| *k == key) {
            let delta = angle - entry.1;
            entry.1 = angle;
            return Some(delta);
        }
        self.spin.push((key, angle));
        None
    }
}

fn surface_of(
    state: &VehicleFxState,
    grounds: &Query<(Option<&WheelSurfaceOverride>, Option<&PhysicsMaterialRef>)>,
    ground: Option<Entity>,
    contact: Vec3,
) -> WheelSurface {
    if let Some(entity) = ground
        && let Ok((forced, material)) = grounds.get(entity)
    {
        if let Some(forced) = forced {
            return forced.0;
        }
        if let Some(surface) = material.and_then(|m| WheelSurface::from_material_name(&m.0)) {
            return surface;
        }
    }
    state
        .terrain_probe
        .as_ref()
        .and_then(|probe| probe(contact))
        .unwrap_or(state.default_surface)
}

fn apply_tier(
    render: Option<Res<regia_render::RenderSettings>>,
    pool: Option<Res<DecalPool>>,
    mut state: ResMut<VehicleFxState>,
) {
    let tier = render
        .and_then(|r| r.preset())
        .map(regia_render::QualityTier::of_preset);
    let Some(tier) = tier else { return };
    if state.tier_seen == Some(tier) {
        return;
    }
    state.tier_seen = Some(tier);
    let mut config = state.config();
    config.limits = VehicleFxLimits::for_tier(tier);
    if let Some(pool) = pool {
        // The marks share the pool with every other decal: never more than half of it.
        config.limits.max_stamps = config
            .limits
            .max_stamps
            .min((pool.capacity() as usize / 2).max(1));
    }
    state.set_config(config);
}

/// The fixed-step system: samples every wheel, steps the logic, lays and retires decals, drives
/// the effect instances.
#[tracing::instrument(skip_all)]
pub fn drive_vehicle_fx(
    fixed: Res<FixedTime>,
    physics: Option<Res<PhysicsWorld>>,
    vehicles: Query<(Entity, &VehicleController)>,
    grounds: Query<(Option<&WheelSurfaceOverride>, Option<&PhysicsMaterialRef>)>,
    mut state: ResMut<VehicleFxState>,
    pool: Option<ResMut<DecalPool>>,
    scene: Option<ResMut<VfxScene>>,
) {
    let _span = tracing::debug_span!("vehicle_fx").entered();
    let dt = fixed.step().as_secs_f32();
    let state = &mut *state;
    state.last_dt = dt;
    let mut pool = pool;
    let mut scene = scene;
    if !state.enabled {
        if !state.stamps.is_empty() || state.fx.stats().live > 0 {
            state.fx.clear();
            retire(state, pool.as_deref_mut());
        }
        stop_all(state, scene.as_deref_mut());
        return;
    }
    state.samples.clear();
    if let Some(physics) = physics.as_deref() {
        // Spawn order is not needed: the logic keys wheels, and the pool is shared, so the
        // vehicles are visited in entity order for a stable stamp order.
        let mut order: Vec<Entity> = vehicles.iter().map(|(e, _)| e).collect();
        order.sort();
        for entity in order {
            let Ok((_, controller)) = vehicles.get(entity) else {
                continue;
            };
            let Ok(vs) = controller.vehicle.state(physics) else {
                continue;
            };
            let radii = &controller.vehicle.config().wheels;
            for wheel in &vs.wheels {
                let radius = radii.get(wheel.index).map_or(0.35, |w| w.radius);
                let key = wheel_key(entity, wheel.index);
                let heading = vs.rotation * Quat::from_rotation_y(wheel.steer_angle) * Vec3::NEG_Z;
                let delta = state.spin_of(key, wheel.spin_angle);
                let roll_speed = delta.map_or(0.0, |d| d / dt * radius);
                let (contact, normal) = match (wheel.grounded, wheel.contact_point, wheel.contact_normal) {
                    (true, Some(p), Some(n)) => (p, n),
                    _ => (wheel.world_translation, Vec3::Y),
                };
                let surface = surface_of(state, &grounds, wheel.ground_entity, contact);
                state.samples.push(WheelSample {
                    key,
                    grounded: wheel.grounded && wheel.contact_point.is_some(),
                    contact,
                    normal,
                    heading,
                    roll_speed,
                    surface,
                    half_width: radius * HALF_WIDTH_PER_RADIUS,
                });
            }
        }
    }
    let samples = std::mem::take(&mut state.samples);
    let mut outputs = std::mem::take(&mut state.outputs);
    state.fx.step(dt, &samples, &mut outputs);
    // Marks: new stamps become decals, retired serials remove them.
    if let Some(pool) = pool.as_deref_mut() {
        for stamp in state.fx.new_stamps() {
            let id = pool.spawn_with(stamp.decal, stamp.surface);
            state.stamps.push_back((stamp.serial, id));
        }
    }
    retire(state, pool.as_deref_mut());
    // Effects.
    if let Some(scene) = scene.as_deref_mut() {
        drive_instances(state, scene, &samples, &outputs, dt);
    }
    state.samples = samples;
    state.outputs = outputs;
}

fn retire(state: &mut VehicleFxState, pool: Option<&mut DecalPool>) {
    let mut pool = pool;
    for &serial in state.fx.retired() {
        while let Some(&(front, id)) = state.stamps.front() {
            if front > serial {
                break;
            }
            state.stamps.pop_front();
            if let Some(pool) = pool.as_deref_mut() {
                pool.remove(id);
            }
        }
    }
}

fn stop_all(state: &mut VehicleFxState, scene: Option<&mut VfxScene>) {
    if let Some(scene) = scene {
        for inst in &mut state.instances {
            for id in inst.ids.iter_mut().filter_map(Option::take) {
                scene.stop(id);
            }
        }
    }
    state.instances.clear();
}

fn drive_instances(
    state: &mut VehicleFxState,
    scene: &mut VfxScene,
    samples: &[WheelSample],
    outputs: &[WheelOutput],
    dt: f32,
) {
    // Wheels that left the scene (a despawned vehicle): stop their effects.
    let mut i = 0;
    while i < state.instances.len() {
        if samples.iter().any(|s| s.key == state.instances[i].key) {
            i += 1;
            continue;
        }
        let gone = state.instances.remove(i);
        for id in gone.ids.into_iter().flatten() {
            scene.stop(id);
        }
    }
    for (sample, out) in samples.iter().zip(outputs) {
        let levels = [out.smoke, out.dust, out.spray];
        let slot = match state.instances.iter().position(|w| w.key == sample.key) {
            Some(slot) => slot,
            None => {
                if levels.iter().all(|l| *l <= 0.0) {
                    continue;
                }
                state.instances.push(WheelInstances {
                    key: sample.key,
                    ids: [None; 3],
                });
                state.instances.len() - 1
            }
        };
        for (n, name) in EFFECTS.iter().enumerate() {
            let level = levels[n];
            let alive = state.instances[slot].ids[n].filter(|id| scene.instance(*id).is_some());
            let id = match alive {
                Some(id) => id,
                None if level > 0.0 && ensure_vehicle_effect(scene, name) => {
                    let seed = sample.key ^ (0x5645_4858 + n as u64);
                    match scene.spawn(name, sample.contact, sample.normal, seed) {
                        Some(id) => id,
                        None => continue,
                    }
                }
                None => {
                    state.instances[slot].ids[n] = None;
                    continue;
                }
            };
            state.instances[slot].ids[n] = Some(id);
            if let Some(instance) = scene.instance_mut(id) {
                instance.set_transform(sample.contact, sample.normal, dt);
                instance.set_intensity(level);
            }
        }
    }
}

/// Installs [`VehicleFxState`] and the fixed-step systems. Add it after `PhysicsPlugin`,
/// `VehiclePlugin` and `VfxPlugin`.
pub struct VehicleFxPlugin;

impl Plugin for VehicleFxPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<VehicleFxState>().add_systems(
            FixedUpdate,
            (apply_tier, drive_vehicle_fx).chain().after(step_physics),
        );
    }
}
