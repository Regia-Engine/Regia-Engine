// SPDX-License-Identifier: MPL-2.0
//! Vehicle audio in the ECS (ADR-3271, `book/agents/vehicle-audio.md`): cars that sound like cars.
//!
//! Put a [`VehicleAudio`] (a `.vehicleaudio.ron` definition, [`regia_audio::VehicleAudioDef`]) on an
//! entity with a `regia_physics` [`VehicleController`] and add [`VehicleAudioPlugin`]. Each fixed
//! step [`vehicle_audio_step`] reads the vehicle (engine speed, throttle, torque and shift counters
//! from the geared drivetrain; per-wheel slip angle, slip ratio, contact speed, compression and the
//! ground's [`VehicleAudioSurface`]) into the pure model ([`regia_audio::VehicleAudioModel`]). Each
//! frame [`vehicle_audio_apply`] ranks the vehicles by distance to the nearest listener (the
//! listener's own vehicle, `ListenerCoupling::vehicle`, first), gives the first
//! [`VehicleAudioBudget::full_vehicles`] every layer and the next
//! [`VehicleAudioBudget::reduced_vehicles`] their loudest engine layer, and drives the voices on
//! emitter entities at the engine, the exhaust and every wheel through the existing emitter pipeline
//! (distance, Doppler, occlusion, and the direct cockpit path while the listener sits in that car).
//!
//! A vehicle with the `Simple` drivetrain has no engine: its rpm is derived from its forward speed
//! (`SIMPLE_IDLE_RPM` plus `SIMPLE_RPM_PER_MPS` per m/s, up to the definition's redline) and its
//! load from the throttle input. Sounds named `placeholder:...` are generated in code
//! ([`regia_audio::placeholder_sound`]); every other name resolves through
//! [`regia_audio::SoundAssets`].

use std::collections::BTreeMap;

use regia_app::prelude::*;
use regia_audio::{
    Audio, AudioEmitter, AudioPropagation, ListenerCoupling, MAX_AUDIO_WHEELS, ONE_SHOT_CAPACITY,
    SoundAssets, VehicleAudioDef, VehicleAudioInputs, VehicleAudioModel, VehicleEmitterEntities,
    VehicleEmitterSlot, VehicleOneShot, VehicleSoundBank, VehicleVoiceMode, VehicleVoiceSet,
    placeholder_sound,
};
use regia_core::math::{Quat, Vec3};
use regia_physics::vehicle::{VehicleController, VehicleInput, WheelState};
use regia_physics::{PhysicsStep, PhysicsWorld};
use regia_scene::layers::LayerParked;
use regia_scene::{GlobalTransform, Transform};

/// Engine speed of a `Simple`-drivetrain vehicle at rest, rpm.
pub const SIMPLE_IDLE_RPM: f32 = 900.0;

/// Engine speed a `Simple`-drivetrain vehicle gains per m/s of forward speed, rpm.
pub const SIMPLE_RPM_PER_MPS: f32 = 160.0;

/// Most one-shots a vehicle keeps waiting between frames; more are dropped (and counted).
pub const PENDING_ONE_SHOTS: usize = 4 * ONE_SHOT_CAPACITY;

/// The sound of a vehicle: put it on an entity with a [`VehicleController`].
#[derive(Component, Clone, Debug, PartialEq)]
pub struct VehicleAudio {
    /// The definition (`.vehicleaudio.ron`).
    pub def: VehicleAudioDef,
}

impl VehicleAudio {
    /// The placeholder car ([`VehicleAudioDef::placeholder`]): generated sounds, no recordings.
    #[must_use]
    pub fn placeholder() -> Self {
        Self {
            def: VehicleAudioDef::placeholder(),
        }
    }
}

/// Names the surface of a ground entity for rolling noise (`"gravel"`); entities without one
/// roll on the definition's `default` surface.
#[derive(Component, Reflect, Clone, Debug, Default, PartialEq, Eq)]
#[reflect(Component, Default)]
pub struct VehicleAudioSurface(pub String);

/// On the emitter entities the plugin spawns: whose sound they carry and where.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct VehicleAudioEmitter {
    /// The vehicle entity.
    pub vehicle: Entity,
    /// The slot it plays.
    pub slot: VehicleEmitterSlot,
}

/// The voice budget: how many vehicles may sound fully, how many with one layer.
#[derive(Resource, Reflect, Clone, Copy, Debug, PartialEq)]
#[reflect(Resource)]
pub struct VehicleAudioBudget {
    /// The nearest vehicles that play every layer.
    pub full_vehicles: usize,
    /// The next vehicles that play their loudest engine layer only; the rest are silent.
    pub reduced_vehicles: usize,
    /// Vehicles farther than this from every listener are silent, meters.
    pub max_distance: f32,
}

impl Default for VehicleAudioBudget {
    fn default() -> Self {
        Self {
            full_vehicles: 3,
            reduced_vehicles: 12,
            max_distance: 250.0,
        }
    }
}

/// One vehicle's live sound state.
pub struct VehicleAudioState {
    /// The model.
    pub model: VehicleAudioModel,
    /// Its sounds.
    pub bank: VehicleSoundBank,
    /// Its live voices.
    pub voices: VehicleVoiceSet,
    /// Its emitter entities (spawned on the first frame after the vehicle is seen).
    pub emitters: Option<VehicleEmitterEntities>,
    /// The budget's verdict this frame.
    pub mode: VehicleVoiceMode,
    /// Distance to the nearest listener, meters.
    pub distance: f32,
    /// The inputs of the last step.
    pub inputs: VehicleAudioInputs,
    /// The chassis pose after the last step.
    pub pose: (Vec3, Quat),
    /// Wheel centers after the last step (world space).
    pub wheels: [Vec3; MAX_AUDIO_WHEELS],
    /// One-shots from fixed steps waiting for the next frame.
    pub pending: Vec<VehicleOneShot>,
    /// One-shots played (queued on the mixer).
    pub shots_played: u64,
    /// One-shots dropped because `pending` was full.
    pub shots_dropped: u64,
    /// The vehicle's readout failed (its error code), or a sound is missing.
    pub problem: Option<String>,
    /// The emitter settings every voice of this vehicle uses.
    pub emitter: AudioEmitter,
    /// Scratch for the physics wheel readout, reused every fixed step
    /// (`regia_physics::vehicle::Vehicle::wheel_states_into`).
    wheel_buf: Vec<WheelState>,
}

impl VehicleAudioState {
    /// A fresh state for `def` with sounds from `resolve`.
    fn new(
        def: VehicleAudioDef,
        wheels: usize,
        resolve: impl FnMut(&str) -> Option<regia_audio::SoundAsset>,
    ) -> Result<Self, String> {
        let model = VehicleAudioModel::new(def, wheels).map_err(|e| e.to_string())?;
        let bank = VehicleSoundBank::resolve(&model, resolve);
        let problem =
            (!bank.missing.is_empty()).then(|| format!("missing sounds: {}", bank.missing.join(", ")));
        let emitter = AudioEmitter {
            autoplay: false,
            min_distance: 3.0,
            max_distance: 250.0,
            doppler_factor: 1.0,
            bus: model.def().bus.clone(),
            ..AudioEmitter::default()
        };
        Ok(Self {
            model,
            bank,
            voices: VehicleVoiceSet::default(),
            emitters: None,
            mode: VehicleVoiceMode::Silent,
            distance: f32::INFINITY,
            inputs: VehicleAudioInputs::default(),
            pose: (Vec3::ZERO, Quat::IDENTITY),
            wheels: [Vec3::ZERO; MAX_AUDIO_WHEELS],
            pending: Vec::with_capacity(PENDING_ONE_SHOTS),
            shots_played: 0,
            shots_dropped: 0,
            problem,
            emitter,
            wheel_buf: Vec::new(),
        })
    }

    /// World position of a slot.
    #[must_use]
    pub fn slot_position(&self, slot: VehicleEmitterSlot) -> Option<Vec3> {
        let (translation, rotation) = self.pose;
        let offset = |p: [f32; 3]| translation + rotation * Vec3::from_array(p);
        match slot {
            VehicleEmitterSlot::Engine => Some(offset(self.model.def().emitters.engine)),
            VehicleEmitterSlot::Exhaust => Some(offset(self.model.def().emitters.exhaust)),
            VehicleEmitterSlot::Wheel(i) => {
                let i = usize::from(i);
                (i < self.inputs.wheel_count).then(|| self.wheels[i])
            }
        }
    }
}

/// Every vehicle's sound state, by entity (a `BTreeMap`: iteration order is entity order, so
/// nothing that feeds a digest depends on hashing).
#[derive(Resource, Default)]
pub struct VehicleAudioRuntime {
    /// The states.
    pub vehicles: BTreeMap<Entity, VehicleAudioState>,
    /// Definitions that did not build a model, by vehicle (the error text).
    pub rejected: BTreeMap<Entity, String>,
    ranking: Vec<(u8, f32, Entity)>,
    shots: Vec<VehicleOneShot>,
    retired: Vec<VehicleVoiceSet>,
}

impl VehicleAudioRuntime {
    /// Vehicles per budget mode: `(full, reduced, silent)`.
    #[must_use]
    pub fn mode_counts(&self) -> (usize, usize, usize) {
        let mut counts = (0, 0, 0);
        for state in self.vehicles.values() {
            match state.mode {
                VehicleVoiceMode::Full => counts.0 += 1,
                VehicleVoiceMode::Reduced => counts.1 += 1,
                VehicleVoiceMode::Silent => counts.2 += 1,
            }
        }
        counts
    }

    /// FNV-1a over every vehicle's parameter digest, in entity order.
    #[must_use]
    pub fn digest(&self) -> u64 {
        let mut h = 0xCBF2_9CE4_8422_2325_u64;
        for (entity, state) in &self.vehicles {
            for byte in entity
                .to_bits()
                .to_le_bytes()
                .into_iter()
                .chain(state.model.parameter_digest().to_le_bytes())
            {
                h = (h ^ u64::from(byte)).wrapping_mul(0x0100_0000_01B3);
            }
        }
        h
    }
}

/// Adds vehicle audio: [`vehicle_audio_step`] in `FixedUpdate` after the physics step and
/// [`vehicle_audio_apply`] in `PostUpdate` before the audio sync. Needs `regia_audio::AudioPlugin`
/// and `regia_physics::vehicle::VehiclePlugin`; does nothing without them.
#[derive(Default)]
pub struct VehicleAudioPlugin;

impl Plugin for VehicleAudioPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<VehicleAudioRuntime>()
            .init_resource::<VehicleAudioBudget>()
            .register_type::<VehicleAudioBudget>()
            .register_type::<VehicleAudioSurface>()
            .add_systems(FixedUpdate, vehicle_audio_step.after(PhysicsStep))
            .add_systems(
                PostUpdate,
                vehicle_audio_apply
                    .after(PostUpdateSet::TransformPropagate)
                    .before(PostUpdateSet::AudioSync),
            );
    }
}

/// Reads one vehicle into model inputs: the drivetrain readout, and per wheel the slip angle and
/// slip ratio `regia_physics` reports, the contact's speed over the ground (the chassis velocity at
/// the contact point, in the ground plane) and the surface `surface_of` names for the ground
/// entity. Returns the inputs and the chassis pose; `None` when the chassis is gone.
#[must_use]
pub fn vehicle_audio_inputs<'s>(
    controller: &VehicleController,
    input: Option<&VehicleInput>,
    physics: &PhysicsWorld,
    model: &VehicleAudioModel,
    wheels: &[WheelState],
    surface_of: impl Fn(Entity) -> Option<&'s str>,
) -> Option<(VehicleAudioInputs, (Vec3, Quat))> {
    use regia_physics::convert::{from_rapier_pose, from_rapier_vec3, to_rapier_vec3};
    let vehicle = &controller.vehicle;
    let body = physics.bodies.get(vehicle.chassis().to_handle())?;
    let (translation, rotation) = from_rapier_pose(body.position());
    let linear = from_rapier_vec3(body.linvel());
    let forward = rotation * Vec3::NEG_Z;
    let forward_speed = linear.dot(forward);
    let throttle_input = input.map_or(0.0, |i| i.throttle).clamp(-1.0, 1.0);
    let mut out = VehicleAudioInputs {
        speed: linear.length(),
        wheel_count: wheels.len().min(MAX_AUDIO_WHEELS),
        ..VehicleAudioInputs::default()
    };
    match vehicle.drivetrain() {
        Some(drivetrain) => {
            out.rpm = drivetrain.rpm();
            out.throttle = drivetrain.applied_throttle();
            out.engine_torque = drivetrain.engine_torque();
            out.upshifts = drivetrain.upshifts();
            out.downshifts = drivetrain.downshifts();
            out.shifting = drivetrain.is_shifting();
        }
        None => {
            let redline = model.def().engine.redline_rpm;
            out.rpm = (SIMPLE_IDLE_RPM + forward_speed.abs() * SIMPLE_RPM_PER_MPS).min(redline);
            out.throttle = throttle_input.abs();
            out.engine_torque = out.throttle * model.def().engine.peak_torque_nm;
        }
    }
    for (slot, wheel) in out.wheels.iter_mut().zip(wheels) {
        let contact = wheel.contact_point.unwrap_or(wheel.world_translation);
        let velocity = from_rapier_vec3(body.velocity_at_point(to_rapier_vec3(contact)));
        let normal = wheel.contact_normal.unwrap_or(Vec3::Y);
        let tangential = velocity - normal * velocity.dot(normal);
        slot.grounded = wheel.grounded;
        slot.slip_angle = wheel.slip_angle;
        slot.slip_ratio = wheel.slip_ratio;
        slot.contact_speed = tangential.length();
        slot.compression = wheel.compression;
        slot.surface = model.surface_index(wheel.ground_entity.and_then(&surface_of));
    }
    Some((out, (translation, rotation)))
}

type VehicleQuery<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static VehicleController,
        Option<&'static VehicleInput>,
        Ref<'static, VehicleAudio>,
    ),
    Without<LayerParked>,
>;

/// `FixedUpdate` (after the physics step): steps every vehicle's sound model by the fixed step.
pub fn vehicle_audio_step(
    fixed: Option<Res<FixedTime>>,
    physics: Option<Res<PhysicsWorld>>,
    sounds: Option<Res<SoundAssets>>,
    runtime: Option<ResMut<VehicleAudioRuntime>>,
    vehicles: VehicleQuery,
    surfaces: Query<&VehicleAudioSurface>,
) {
    let (Some(fixed), Some(physics), Some(mut runtime)) = (fixed, physics, runtime) else {
        return;
    };
    let _span = tracing::debug_span!("vehicle_audio_step").entered();
    let dt = fixed.step().as_secs_f32();
    let runtime = &mut *runtime;
    for (entity, controller, input, audio) in &vehicles {
        let wheel_count = controller.vehicle.config().wheels.len();
        let rebuild = audio.is_changed() || !runtime.vehicles.contains_key(&entity);
        if rebuild && !runtime.rejected.get(&entity).is_some_and(|_| !audio.is_changed()) {
            let resolve = |name: &str| match placeholder_sound(name) {
                Some(generated) => generated.ok(),
                None => sounds.as_deref().and_then(|s| s.by_name(name)).cloned(),
            };
            match VehicleAudioState::new(audio.def.clone(), wheel_count, resolve) {
                Ok(mut state) => {
                    if let Some(old) = runtime.vehicles.remove(&entity) {
                        // The old voices are stopped by the apply system (it owns `Audio`).
                        state.emitters = old.emitters;
                        runtime.retired.push(old.voices);
                    }
                    if let Some(problem) = &state.problem {
                        tracing::warn!(target: "regia_framework", "vehicle audio {entity:?}: {problem}");
                    }
                    runtime.rejected.remove(&entity);
                    runtime.vehicles.insert(entity, state);
                }
                Err(error) => {
                    tracing::warn!(target: "regia_framework", "vehicle audio {entity:?} rejected: {error}");
                    runtime.rejected.insert(entity, error);
                    continue;
                }
            }
        }
        let Some(state) = runtime.vehicles.get_mut(&entity) else {
            continue;
        };
        // `wheel_states_into` reuses the state's buffer: no per-step `Vec` (ADR-3271's readout
        // allocation is gone for this caller).
        if let Err(error) = controller
            .vehicle
            .wheel_states_into(&physics, &mut state.wheel_buf)
        {
            state.problem = Some(error.code().to_string());
            continue;
        }
        let wheels = &state.wheel_buf;
        let surface_of = |ground: Entity| surfaces.get(ground).ok().map(|s| s.0.as_str());
        let Some((inputs, pose)) =
            vehicle_audio_inputs(controller, input, &physics, &state.model, wheels, surface_of)
        else {
            continue;
        };
        for (slot, wheel) in state.wheels.iter_mut().zip(wheels) {
            *slot = wheel.world_translation;
        }
        state.inputs = inputs;
        state.pose = pose;
        state.model.step(&inputs, dt);
        state.model.drain_one_shots(&mut runtime.shots);
        for shot in runtime.shots.drain(..) {
            if state.pending.len() < PENDING_ONE_SHOTS {
                state.pending.push(shot);
            } else {
                state.shots_dropped += 1;
            }
        }
    }
}

type ListenerVehicleQuery<'w, 's> = Query<'w, 's, &'static ListenerCoupling>;
type AudibleVehicleQuery<'w, 's> =
    Query<'w, 's, Entity, (With<VehicleAudio>, With<VehicleController>, Without<LayerParked>)>;

/// `PostUpdate` (before the audio sync): ranks the vehicles for the voice budget, places their
/// emitter entities, applies the model to the voices and plays the pending one-shots.
pub fn vehicle_audio_apply(
    mut commands: Commands,
    audio: Option<ResMut<Audio>>,
    runtime: Option<ResMut<VehicleAudioRuntime>>,
    budget: Option<Res<VehicleAudioBudget>>,
    vehicles: AudibleVehicleQuery,
    coupling: ListenerVehicleQuery,
    mut emitters: Query<(&VehicleAudioEmitter, &mut Transform, &mut GlobalTransform)>,
) {
    let (Some(mut audio), Some(mut runtime)) = (audio, runtime) else {
        return;
    };
    let _span = tracing::debug_span!("vehicle_audio_apply").entered();
    let budget = budget.as_deref().copied().unwrap_or_default();
    let runtime = &mut *runtime;

    // Old voice sets of rebuilt definitions, and vehicles that are gone.
    for mut voices in runtime.retired.drain(..) {
        voices.stop_all(&mut audio);
    }
    let gone: Vec<Entity> = runtime
        .vehicles
        .keys()
        .copied()
        .filter(|e| !vehicles.contains(*e))
        .collect();
    for entity in gone {
        if let Some(mut state) = runtime.vehicles.remove(&entity) {
            state.voices.stop_all(&mut audio);
            if let Some(entities) = state.emitters {
                for e in [entities.engine, entities.exhaust]
                    .into_iter()
                    .chain(entities.wheels)
                {
                    if let Ok(mut e) = commands.get_entity(e) {
                        e.despawn();
                    }
                }
            }
        }
    }
    runtime.rejected.retain(|e, _| vehicles.contains(*e));

    // Rank: the listener's own vehicle first, then by distance to the nearest listener.
    let own: Option<Entity> = coupling.iter().find_map(|c| c.vehicle);
    runtime.ranking.clear();
    for (entity, state) in &mut runtime.vehicles {
        let at = state.pose.0;
        state.distance = audio
            .listeners()
            .iter()
            .map(|(_, l)| l.position.distance(at))
            .fold(f32::INFINITY, f32::min);
        let class = u8::from(own != Some(*entity));
        runtime.ranking.push((class, state.distance, *entity));
    }
    runtime
        .ranking
        .sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)).then(a.2.cmp(&b.2)));
    for (rank, &(class, distance, entity)) in runtime.ranking.iter().enumerate() {
        let Some(state) = runtime.vehicles.get_mut(&entity) else {
            continue;
        };
        state.mode = if class == 1 && distance > budget.max_distance {
            VehicleVoiceMode::Silent
        } else if rank < budget.full_vehicles {
            VehicleVoiceMode::Full
        } else if rank < budget.full_vehicles + budget.reduced_vehicles {
            VehicleVoiceMode::Reduced
        } else {
            VehicleVoiceMode::Silent
        };
    }

    // Place the emitters; spawn them for vehicles seen for the first time (voices start next
    // frame, once the sync knows where they are).
    for (&vehicle, state) in &mut runtime.vehicles {
        let Some(entities) = &state.emitters else {
            let mut spawn = |slot: VehicleEmitterSlot| {
                let at = state.slot_position(slot).unwrap_or(state.pose.0);
                commands
                    .spawn((
                        Transform::from_translation(at),
                        GlobalTransform::IDENTITY.mul_transform(&Transform::from_translation(at)),
                        VehicleAudioEmitter { vehicle, slot },
                        state.emitter.clone(),
                        AudioPropagation {
                            vehicle: Some(vehicle),
                            ..AudioPropagation::default()
                        },
                    ))
                    .id()
            };
            let engine = spawn(VehicleEmitterSlot::Engine);
            let exhaust = spawn(VehicleEmitterSlot::Exhaust);
            let wheels = (0..state.inputs.wheel_count as u8)
                .map(|i| spawn(VehicleEmitterSlot::Wheel(i)))
                .collect();
            state.emitters = Some(VehicleEmitterEntities {
                engine,
                exhaust,
                wheels,
            });
            continue;
        };
        for slot_entity in [entities.engine, entities.exhaust]
            .into_iter()
            .chain(entities.wheels.iter().copied())
        {
            let Ok((tag, mut transform, mut global)) = emitters.get_mut(slot_entity) else {
                continue;
            };
            if let Some(at) = state.slot_position(tag.slot)
                && transform.translation != at
            {
                transform.translation = at;
                *global = GlobalTransform::IDENTITY.mul_transform(&transform);
            }
        }
        if !emitters.contains(entities.engine) {
            // Spawned this frame by commands: wait one frame.
            continue;
        }
        state.voices.apply(
            &mut audio,
            &state.model,
            &state.bank,
            entities,
            &state.emitter,
            state.mode,
        );
        if state.mode == VehicleVoiceMode::Silent {
            state.pending.clear();
        } else if !state.pending.is_empty() {
            let played = VehicleVoiceSet::play_one_shots(
                &mut audio,
                &state.pending,
                &state.bank,
                &state.emitter,
                |slot| state.slot_position(slot),
            );
            state.shots_played += played as u64;
            state.pending.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use regia_audio::{AudioListener, AudioPlugin, AudioSettings};
    use regia_physics::vehicle::{VehicleConfig, VehiclePlugin, spawn_vehicle};
    use regia_physics::vehicle_drivetrain::{Drivetrain, DrivetrainConfig};
    use regia_physics::{BodyKind, Collider, ColliderShape, PhysicsPlugin, RigidBody};

    use super::*;

    fn app() -> App {
        let mut app = App::new();
        app.insert_resource(TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_secs_f64(1.0 / 60.0),
        ));
        app.insert_resource(FixedTime::default());
        app.insert_resource(AudioSettings {
            headless: true,
            ..AudioSettings::default()
        });
        app.add_plugins((PhysicsPlugin, VehiclePlugin, AudioPlugin, VehicleAudioPlugin));
        app.world_mut().spawn((
            Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
            RigidBody {
                kind: BodyKind::Static,
                ..RigidBody::default()
            },
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(2_000.0, 0.5, 2_000.0),
                },
                ..Collider::default()
            },
        ));
        app.world_mut().spawn((
            Transform::from_translation(Vec3::new(4.0, 1.5, 0.0)),
            AudioListener::default(),
        ));
        app
    }

    fn geared() -> VehicleConfig {
        VehicleConfig {
            drivetrain: Drivetrain::Geared(DrivetrainConfig::default()),
            ..VehicleConfig::default()
        }
    }

    fn car(app: &mut App, config: VehicleConfig, at: Vec3) -> Entity {
        let entity = spawn_vehicle(app.world_mut(), config, at, Quat::IDENTITY).expect("spawns");
        app.world_mut()
            .entity_mut(entity)
            .insert(VehicleAudio::placeholder());
        entity
    }

    fn throttle(app: &mut App, entity: Entity, value: f32) {
        if let Some(mut input) = app.world_mut().get_mut::<VehicleInput>(entity) {
            input.throttle = value;
        }
    }

    fn run(app: &mut App, frames: usize) {
        for _ in 0..frames {
            app.update();
        }
    }

    #[test]
    fn spawned_vehicle_slots_survive_the_audio_sync() {
        let mut app = app();
        let e = car(&mut app, geared(), Vec3::new(0.0, 1.0, 0.0));
        run(&mut app, 30);
        let state = &app.world().resource::<VehicleAudioRuntime>().vehicles[&e];
        let slots = state.emitters.as_ref().expect("slots spawned");
        for slot in [slots.engine, slots.exhaust]
            .into_iter()
            .chain(slots.wheels.iter().copied())
        {
            let emitter = app.world().get::<AudioEmitter>(slot).expect("registered emitter");
            assert!(!emitter.autoplay, "the vehicle model starts its layers");
            assert_eq!(emitter.max_distance, state.emitter.max_distance);
        }
        assert!(state.voices.live_count() > 0, "idle engine is playing");
        let audio = app.world().resource::<Audio>();
        for i in 0..state.model.voices().len() {
            if let Some(id) = state.voices.voice_id(i) {
                assert!(audio.voice_alive(id), "audio sync retired a live vehicle layer");
            }
        }
    }

    #[test]
    fn a_car_accelerating_from_rest_raises_the_rpm_and_plays_each_upshift_once() {
        let mut app = app();
        let e = car(&mut app, geared(), Vec3::new(0.0, 1.0, 0.0));
        // The driver's ears are in the car: it stays the first-ranked vehicle however far it drives.
        let listener = app
            .world_mut()
            .query_filtered::<Entity, With<AudioListener>>()
            .single(app.world())
            .expect("one listener");
        app.world_mut().entity_mut(listener).insert(ListenerCoupling {
            vehicle: Some(e),
            ..ListenerCoupling::default()
        });
        run(&mut app, 60);
        let idle = app.world().resource::<VehicleAudioRuntime>().vehicles[&e]
            .model
            .params()
            .rpm;
        throttle(&mut app, e, 1.0);
        let mut peak = idle;
        for _ in 0..900 {
            app.update();
            let rpm = app.world().resource::<VehicleAudioRuntime>().vehicles[&e]
                .model
                .params()
                .rpm;
            peak = peak.max(rpm);
        }
        throttle(&mut app, e, 0.0);
        run(&mut app, 2);
        let upshifts = app
            .world()
            .get::<VehicleController>(e)
            .and_then(|c| c.vehicle.drivetrain().map(|d| d.upshifts()))
            .expect("geared");
        let runtime = app.world().resource::<VehicleAudioRuntime>();
        let state = &runtime.vehicles[&e];
        assert!(peak > idle + 2000.0, "rpm rose from {idle} to {peak}");
        assert!(upshifts >= 1, "the automatic shifted up");
        assert_eq!(
            state.model.shifts_fired(),
            u64::from(upshifts),
            "one shot per upshift"
        );
        assert_eq!(
            state.shots_played + state.pending.len() as u64,
            u64::from(upshifts) + state.model.bumps_fired()
        );
        assert_eq!(state.mode, VehicleVoiceMode::Full);
        assert!(
            state.voices.live_count() > 3,
            "engine, wind and tires play: live={}, inputs={:?}, voices={:?}",
            state.voices.live_count(),
            state.inputs,
            state.model.voices()
        );
        assert!(state.problem.is_none(), "{:?}", state.problem);
    }

    #[test]
    fn fifty_vehicles_keep_at_most_the_budgeted_full_voices() {
        let mut app = app();
        let cars: Vec<Entity> = (0..50)
            .map(|i| {
                car(
                    &mut app,
                    VehicleConfig::default(),
                    Vec3::new(0.0, 1.0, -4.0 - 4.0 * i as f32),
                )
            })
            .collect();
        for &e in &cars {
            throttle(&mut app, e, 0.6);
        }
        run(&mut app, 30);
        let budget = *app.world().resource::<VehicleAudioBudget>();
        let runtime = app.world().resource::<VehicleAudioRuntime>();
        assert_eq!(runtime.vehicles.len(), 50);
        let (full, reduced, silent) = runtime.mode_counts();
        assert_eq!(full, budget.full_vehicles);
        assert_eq!(reduced, budget.reduced_vehicles);
        assert_eq!(silent, 50 - full - reduced);
        let many = runtime
            .vehicles
            .values()
            .filter(|s| s.voices.live_count() > 1)
            .count();
        assert!(
            many <= budget.full_vehicles,
            "{many} vehicles hold more than one voice"
        );
        for state in runtime.vehicles.values() {
            match state.mode {
                VehicleVoiceMode::Reduced => assert!(state.voices.live_count() <= 1),
                VehicleVoiceMode::Silent => assert_eq!(state.voices.live_count(), 0),
                VehicleVoiceMode::Full => {}
            }
        }
        // The nearest three are the full ones.
        for &e in &cars[..budget.full_vehicles] {
            assert_eq!(runtime.vehicles[&e].mode, VehicleVoiceMode::Full);
        }
    }

    #[test]
    fn the_listeners_own_vehicle_is_heard_first() {
        let mut app = app();
        let near = car(&mut app, VehicleConfig::default(), Vec3::new(0.0, 1.0, -5.0));
        let far = car(&mut app, VehicleConfig::default(), Vec3::new(0.0, 1.0, -120.0));
        app.insert_resource(VehicleAudioBudget {
            full_vehicles: 1,
            reduced_vehicles: 0,
            max_distance: 100.0,
        });
        app.world_mut().spawn(ListenerCoupling {
            vehicle: Some(far),
            ..ListenerCoupling::default()
        });
        run(&mut app, 5);
        let runtime = app.world().resource::<VehicleAudioRuntime>();
        assert_eq!(
            runtime.vehicles[&far].mode,
            VehicleVoiceMode::Full,
            "own vehicle, beyond max_distance"
        );
        assert_eq!(runtime.vehicles[&near].mode, VehicleVoiceMode::Silent);
    }

    #[test]
    fn the_same_drive_gives_the_same_parameter_digest() {
        let drive = |throttle_value: f32| {
            let mut app = app();
            let e = car(&mut app, geared(), Vec3::new(0.0, 1.0, 0.0));
            run(&mut app, 10);
            throttle(&mut app, e, throttle_value);
            run(&mut app, 240);
            app.world().resource::<VehicleAudioRuntime>().digest()
        };
        assert_eq!(drive(1.0), drive(1.0));
        assert_ne!(drive(1.0), drive(0.4));
    }

    #[test]
    fn a_despawned_vehicle_releases_its_voices_and_emitters() {
        let mut app = app();
        let e = car(&mut app, VehicleConfig::default(), Vec3::new(0.0, 1.0, -5.0));
        throttle(&mut app, e, 1.0);
        run(&mut app, 20);
        let emitters = app
            .world_mut()
            .query::<&VehicleAudioEmitter>()
            .iter(app.world())
            .count();
        assert_eq!(emitters, 6, "engine, exhaust and four wheels");
        app.world_mut().despawn(e);
        run(&mut app, 3);
        assert!(app.world().resource::<VehicleAudioRuntime>().vehicles.is_empty());
        let left = app
            .world_mut()
            .query::<&VehicleAudioEmitter>()
            .iter(app.world())
            .count();
        assert_eq!(left, 0);
    }

    #[test]
    fn a_bad_definition_is_rejected_with_its_reason() {
        let mut app = app();
        let e = spawn_vehicle(app.world_mut(), VehicleConfig::default(), Vec3::Y, Quat::IDENTITY)
            .expect("spawns");
        let mut def = VehicleAudioDef::placeholder();
        def.engine.on_load.clear();
        app.world_mut().entity_mut(e).insert(VehicleAudio { def });
        run(&mut app, 3);
        let runtime = app.world().resource::<VehicleAudioRuntime>();
        assert!(runtime.vehicles.is_empty());
        assert!(runtime.rejected[&e].contains("engine.on_load"));
    }
}
