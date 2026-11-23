// SPDX-License-Identifier: MPL-2.0
//! CPU kinematic probe: plays an effect's spawn schedule on the CPU and integrates every
//! particle with the GPU update kernel's force model, so an effect's size, speed and timing can
//! be measured in metres and seconds without a GPU (unit tests, `vfx.crash_profile`).
//!
//! The spawn jobs are the real ones ([`VfxInstance::schedule`], the same seeded sampling the
//! GPU receives). Each particle then follows the kernel's update (`gpu/shaders.rs`, `update`):
//! `v = (v + g·dt) / (1 + k·dt)`, `p += v·dt`, the world `kill_below` plane and the v7 kill
//! volume ([`crate::kill_volume`]), and a flat ground
//! plane standing in for depth collision (a particle more than the offset under it slides: it
//! is put back at the offset above it with `v × 0.3`; or it dies). Curves are read through the same 8-sample LUT the GPU bakes
//! ([`crate::curves::CURVE_SAMPLES`]). What the probe leaves out: curl noise, vortex and
//! attractor forces (bounded, zero-mean wobble on the effects measured here), the screen-size
//! floor and the soft-particle fade. The numbers are therefore a kinematic envelope, not
//! pixels; the offscreen GPU tests cover the pixels.
//!
//! It is also the CPU reference of the v6 scene wind and event spawns: the wind term is the
//! kernel's `v = (v + (g + c·w)·dt) / (1 + (k + c)·dt)`, and a bounded event queue and a child ring
//! follow the kernels' order and hashes ([`crate::events`]). [`probe_steps`] returns what the
//! GPU pool holds after the same frames, so the two can be compared (`tests/events_gpu.rs`).

use regia_core::math::Vec3;

use crate::curves::{CURVE_SAMPLES, sample_color, sample_scalar};
use crate::events;
use crate::format::{ColorKey, FloatKey, VfxSystemDef};
use crate::sim::{LodInput, SpawnJob, VfxInstance};

/// Alpha below which a particle counts as invisible in the probe's visible-set metrics.
pub const PROBE_VISIBLE_ALPHA: f32 = 0.05;

/// How a probe run is set up.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProbeSettings {
    /// Fixed timestep, seconds.
    pub dt: f32,
    /// Instance seed.
    pub seed: u64,
    /// Instance spatial scale ([`VfxInstance::set_scale`]).
    pub scale: f32,
    /// World height of the flat ground that stands in for the depth buffer, or `None` for no
    /// ground (depth-collision emitters then fly on).
    pub ground: Option<f32>,
    /// The scene wind, m/s (what [`crate::scene::VfxScene::set_wind`] gives the GPU).
    pub wind: Vec3,
    /// Event budget ([`crate::VfxProfile::event_capacity`]): the GPU and this probe both keep
    /// the first `event_capacity` fired events in slot order and count the rest lost, so the
    /// probe's event queue mirrors the pool's keep-set exactly — including a sparse frame
    /// whose few firing slots sit late in the window (fixrev-22-k, Muse review finding 4).
    pub event_capacity: u32,
    /// Slots of the child ring ([`crate::VfxProfile::child_capacity`]).
    pub child_capacity: u32,
    /// Slots of the main ring ([`crate::VfxProfile::particle_capacity`]). `u32::MAX` (the
    /// default) models no pool: every spawn keeps its own slot and events rank by spawn order,
    /// as before ADR-7431. A real capacity makes the probe follow the GPU ring: spawn `n` lands
    /// in slot `n % capacity` (a live particle there is overwritten), a particle's event
    /// identity is its slot, and events rank by their slot's ordinal in the frame's update
    /// window ([`crate::gpu::VfxGpu::update_window`]): oldest first while the window covers
    /// less than the pool, slot order from slot 0 when it covers the whole pool.
    pub particle_capacity: u32,
}

impl Default for ProbeSettings {
    fn default() -> Self {
        Self {
            dt: 1.0 / 60.0,
            seed: 7,
            scale: 1.0,
            ground: Some(0.0),
            wind: Vec3::ZERO,
            event_capacity: 8192,
            child_capacity: 16_384,
            particle_capacity: u32::MAX,
        }
    }
}

/// One emitter's particles at one probe time.
#[derive(Debug, Clone, PartialEq)]
pub struct EmitterProbe {
    /// Emitter name.
    pub name: String,
    /// Particles alive.
    pub live: u32,
    /// Particles alive with alpha above [`PROBE_VISIBLE_ALPHA`].
    pub visible: u32,
    /// Alpha-weighted centroid of the visible particles (world).
    pub centroid: Vec3,
    /// Radius of the sphere about [`Self::centroid`] that holds every visible particle's disc
    /// (centre distance plus half its current size), metres.
    pub envelope_radius: f32,
    /// 90th percentile of the visible particles' horizontal distance from the instance origin
    /// (centres), metres: the front of a ring that runs out along the ground.
    pub front_p90: f32,
    /// Highest visible disc top (centre plus half size) above the instance origin, metres.
    pub top: f32,
    /// 90th percentile of the visible centres' height above the instance origin, metres.
    pub height_p90: f32,
    /// Mean speed of the visible particles, m/s.
    pub mean_speed: f32,
    /// Mean horizontal speed of the visible particles, m/s.
    pub mean_flat_speed: f32,
    /// Mean vertical (world +Y) speed of the visible particles, m/s.
    pub mean_rise: f32,
    /// Mean alpha of the visible particles.
    pub mean_alpha: f32,
    /// Alpha-weighted mean linear Rec. 709 luma of the visible particles' colour (before any
    /// lighting or emissive scale).
    pub mean_luma: f32,
    /// Largest current size (full quad width) of a visible particle, metres.
    pub max_size: f32,
    /// Horizontal offset of [`Self::centroid`] from the instance origin, metres (wind drift).
    pub drift: Vec3,
}

/// Every emitter of the effect at one probe time.
#[derive(Debug, Clone, PartialEq)]
pub struct ProbeSnapshot {
    /// Seconds since the effect fired.
    pub time: f32,
    /// One entry per emitter, in definition order.
    pub emitters: Vec<EmitterProbe>,
}

impl ProbeSnapshot {
    /// The entry of emitter `name`.
    #[must_use]
    pub fn emitter(&self, name: &str) -> Option<&EmitterProbe> {
        self.emitters.iter().find(|e| e.name == name)
    }

    /// Visible particles across every emitter.
    #[must_use]
    pub fn visible(&self) -> u32 {
        self.emitters.iter().map(|e| e.visible).sum()
    }
}

#[derive(Debug, Clone, Copy)]
struct Particle {
    emitter: usize,
    pos: Vec3,
    vel: Vec3,
    age: f32,
    life: f32,
    size: f32,
    color: [f32; 4],
    /// Identity of its event seeds ([`events::event_seed`]): the main ring's slot (its spawn
    /// number without a pool capacity), or the child's stamped lifetime.
    identity: u32,
    /// Child-ring slot, for a child.
    ring_slot: Option<u32>,
}

/// One fired event in the CPU queue.
#[derive(Debug, Clone, Copy)]
struct Fired {
    trigger: u32,
    parent: usize,
    seed: u32,
    pos: Vec3,
    vel: Vec3,
}

/// What the event path did in a probe run (the CPU side of [`crate::gpu::VfxPoolStats`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProbeEventStats {
    /// Events fired, overflow included.
    pub fired: u32,
    /// Events lost to a full queue.
    pub overflow: u32,
    /// Child particles spawned.
    pub children_spawned: u32,
    /// Child particles lost: a live child overwritten, or past one ring in one frame.
    pub children_dropped: u32,
}

/// The bounded event queue and the child ring's cursor.
struct EventQueue {
    /// This step's fired events with their GPU compaction key `(slot ordinal, n)`, sorted and
    /// cut to `capacity` by [`EventQueue::keep_first`]; then the events `spawn_children`
    /// consumes next step.
    fired: Vec<(u64, u32, Fired)>,
    capacity: usize,
    /// The main ring's update window this step, `(first slot, capacity)`; `None` without a pool
    /// capacity (the ordinal is the spawn number).
    window: Option<(u32, u32)>,
    ring: u32,
    cursor: u32,
    step: u32,
    stats: ProbeEventStats,
}

impl EventQueue {
    /// `fire_event`: stages the event under the key `event_prepare` compacts by on the GPU —
    /// the slot's ordinal in the update window, then the event's index `n` within the slot.
    /// The window is the main ring oldest first (the probe's spawn counter, `identity`), then
    /// the child ring by ring slot. The probe's `particles` order is not that order (a step's
    /// children are pushed before its new parents, and `swap_remove` reorders the vector on a
    /// ring overwrite), so the cut to `capacity` waits for [`Self::keep_first`]
    /// (rev-kimi-19, fixrev-22-k finding 4). With a pool capacity the main-ring ordinal is the
    /// slot's place in the update window, `(slot - first) mod capacity`, so a whole-pool window
    /// (`first` 0) ranks by slot from slot 0 as `pool_slot` does (ADR-7431).
    fn fire(&mut self, trigger: u32, p: &Particle, n: &mut u32, vel: Vec3) {
        self.stats.fired += 1;
        let ordinal = match (p.ring_slot, self.window) {
            (None, None) => u64::from(p.identity),
            (None, Some((first, capacity))) => {
                (u64::from(p.identity) + u64::from(capacity) - u64::from(first)) % u64::from(capacity.max(1))
            }
            (Some(slot), _) => (1 << 32) | u64::from(slot),
        };
        self.fired.push((
            ordinal,
            *n,
            Fired {
                trigger,
                parent: p.emitter,
                seed: events::event_seed(p.identity, self.step, trigger),
                pos: p.pos,
                vel,
            },
        ));
        *n += 1;
    }

    /// `event_prepare`'s keep-set: the first `capacity` of this step's fired events in slot
    /// order (ordinal, then `n`); the rest are counted lost. The GPU counts them one frame
    /// later, in the prepare that drops them; the probe counts them on the step they fired.
    fn keep_first(&mut self) {
        self.fired.sort_unstable_by_key(|&(ordinal, n, _)| (ordinal, n));
        if self.fired.len() > self.capacity {
            let lost = self.fired.len() - self.capacity;
            self.stats.overflow += u32::try_from(lost).unwrap_or(u32::MAX);
            self.fired.truncate(self.capacity);
        }
    }

    /// `child_emit`: last step's events become particles.
    fn spawn_children(&mut self, def: &VfxSystemDef, instance: &VfxInstance, particles: &mut Vec<Particle>) {
        let mut used = 0u32;
        for (_, _, fired) in std::mem::take(&mut self.fired) {
            let Some(parent) = def.emitters.get(fired.parent) else {
                continue;
            };
            for (l, event) in parent.update.events.iter().enumerate() {
                if events::trigger_bit(event.on) != fired.trigger {
                    continue;
                }
                let link = events::link_seed(fired.seed, l as u32);
                if !events::link_fires(link, event.probability) {
                    continue;
                }
                let Some(child) = def.emitter_index(&event.child) else {
                    continue;
                };
                let emitter = &def.emitters[child];
                let lod = instance.lod_scale();
                let count = events::scaled_count(event.count, emitter, lod, instance.intensity());
                if count == 0 {
                    continue;
                }
                // `event_prepare`: first fit in event order; a link that does not fit is
                // rolled back (it consumes no ring budget), so a later smaller link can spawn.
                if used.saturating_add(count) > self.ring {
                    self.stats.children_dropped += count;
                    continue;
                }
                used = used.saturating_add(count);
                self.stats.children_spawned += count;
                let init = events::child_init_for(emitter, instance, lod);
                for c in 0..count {
                    let seed = events::child_seed(link, c);
                    let sample =
                        events::sample_child(&init, seed, fired.pos, fired.vel, event.inherit_velocity);
                    let slot = self.cursor % self.ring;
                    self.cursor = self.cursor.wrapping_add(1);
                    if let Some(i) = particles.iter().position(|p| p.ring_slot == Some(slot)) {
                        particles.swap_remove(i);
                        self.stats.children_dropped += 1;
                    }
                    particles.push(Particle {
                        emitter: child,
                        pos: sample.position,
                        vel: sample.velocity,
                        age: 0.0,
                        life: sample.life,
                        size: sample.size,
                        color: sample.color,
                        identity: events::child_identity(sample.life),
                        ring_slot: Some(slot),
                    });
                }
            }
        }
    }
}

/// One step's world for [`integrate`].
#[derive(Clone, Copy)]
struct StepWorld {
    dt: f32,
    ground: Option<f32>,
    wind: Vec3,
}

/// A curve read the way the GPU reads it: baked to [`CURVE_SAMPLES`] points, then linear.
fn lut_scalar(keys: &[FloatKey], t: f32, default_value: f32) -> f32 {
    let n = CURVE_SAMPLES as usize;
    let x = t.clamp(0.0, 1.0) * (n - 1) as f32;
    let i = (x as usize).min(n - 2);
    let f = x - i as f32;
    let at = |k: usize| sample_scalar(keys, k as f32 / (n - 1) as f32, default_value);
    let (a, b) = (at(i), at(i + 1));
    a + (b - a) * f
}

fn lut_color(keys: &[ColorKey], t: f32) -> [f32; 4] {
    let n = CURVE_SAMPLES as usize;
    let x = t.clamp(0.0, 1.0) * (n - 1) as f32;
    let i = (x as usize).min(n - 2);
    let f = x - i as f32;
    let a = sample_color(keys, i as f32 / (n - 1) as f32);
    let b = sample_color(keys, (i + 1) as f32 / (n - 1) as f32);
    let mut out = [0.0; 4];
    for (o, (x, y)) in out.iter_mut().zip(a.iter().zip(&b)) {
        *o = x + (y - x) * f;
    }
    out
}

fn percentile(values: &mut [f32], q: f32) -> f32 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f32::total_cmp);
    let i = ((values.len() - 1) as f32 * q.clamp(0.0, 1.0)).round() as usize;
    values[i.min(values.len() - 1)]
}

/// Probes `def` fired at `origin` with local +Z along `forward`, held still, and snapshots it
/// at each of `times` (seconds, any order; each is reported at the first step at or past it).
#[must_use]
pub fn probe_effect(
    def: &VfxSystemDef,
    origin: Vec3,
    forward: Vec3,
    settings: ProbeSettings,
    times: &[f32],
) -> Vec<ProbeSnapshot> {
    probe_effect_moving(def, settings, times, |_| (origin, forward))
}

/// [`probe_effect`] for an instance that moves: `transform(t)` gives the origin and forward at
/// `t` seconds (a spray carried along a path). The first call is at `t = 0`.
#[must_use]
pub fn probe_effect_moving(
    def: &VfxSystemDef,
    settings: ProbeSettings,
    times: &[f32],
    transform: impl FnMut(f32) -> (Vec3, Vec3),
) -> Vec<ProbeSnapshot> {
    let mut out = Vec::with_capacity(times.len());
    drive(def, settings, times, transform, |particles, origin, t| {
        out.push(snapshot(def, particles, origin, t));
    });
    out
}

/// One live particle after [`probe_steps`], in the terms of [`crate::PoolParticle`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProbeState {
    /// Index of its emitter in the definition.
    pub emitter: usize,
    /// World position.
    pub position: Vec3,
    /// Velocity.
    pub velocity: Vec3,
    /// Age, seconds.
    pub age: f32,
    /// Lifetime, seconds.
    pub life: f32,
}

/// Runs `def` for exactly `steps` frames of the probe's clock and returns every live particle and
/// the event counts, in the same order of work as one GPU frame: last frame's events become
/// children, this frame's jobs land, every particle updates and fires.
#[must_use]
pub fn probe_steps(
    def: &VfxSystemDef,
    origin: Vec3,
    forward: Vec3,
    settings: ProbeSettings,
    steps: u32,
) -> (Vec<ProbeState>, ProbeEventStats) {
    let mut run = ProbeRun::for_def(def, settings, origin, forward);
    for _ in 0..steps {
        run.step(def, settings, origin, forward);
    }
    let states = run
        .particles
        .iter()
        .map(|p| ProbeState {
            emitter: p.emitter,
            position: p.pos,
            velocity: p.vel,
            age: p.age,
            life: p.life,
        })
        .collect();
    (states, run.events.stats)
}

/// One live particle as drawn after [`probe_steps_drawn`]: centre, velocity and current size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct DrawnState {
    /// Index of its emitter in the definition.
    pub(crate) emitter: usize,
    /// World position.
    pub(crate) position: Vec3,
    /// Velocity.
    pub(crate) velocity: Vec3,
    /// Current size (base size × size over life, the curve read as the GPU reads it).
    pub(crate) size: f32,
}

/// [`probe_steps`] with every motion module of the update kernel (curl, vortex, attractor
/// included) and each particle's current size: the run [`crate::bounds::measured_bounds`] reads.
pub(crate) fn probe_steps_drawn(
    def: &VfxSystemDef,
    origin: Vec3,
    forward: Vec3,
    settings: ProbeSettings,
    steps: u32,
) -> Vec<DrawnState> {
    let mut run = ProbeRun::for_def(def, settings, origin, forward);
    run.forces = true;
    for _ in 0..steps {
        run.step(def, settings, origin, forward);
    }
    run.particles
        .iter()
        .filter_map(|p| {
            let u = &def.emitters.get(p.emitter)?.update;
            let life = (p.age / p.life).clamp(0.0, 1.0);
            Some(DrawnState {
                emitter: p.emitter,
                position: p.pos,
                velocity: p.vel,
                size: p.size * lut_scalar(&u.size_over_life, life, 1.0),
            })
        })
        .collect()
}

/// One particle as [`probe_particles`] reports it: where it is and how it draws at that moment
/// (its size, alpha and colour after the emitter's curves).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProbeParticle {
    /// Index of its emitter in the definition.
    pub emitter: usize,
    /// World position, metres.
    pub position: Vec3,
    /// Current size (full quad width), metres.
    pub size: f32,
    /// Current alpha (spawn alpha × alpha over life × the emitter's alpha scale).
    pub alpha: f32,
    /// Current linear colour (spawn colour × colour over life), before lighting or emissive.
    pub rgb: [f32; 3],
}

/// Probes `def` like [`probe_effect`] and returns every live particle at each of `times`
/// (seconds, any order; each reported at the first step at or past it), as `(time, particles)`:
/// for measures that need the particles themselves (the crash fireball's projected footprint,
/// `crash::fireball_footprint`).
#[must_use]
pub fn probe_particles(
    def: &VfxSystemDef,
    origin: Vec3,
    forward: Vec3,
    settings: ProbeSettings,
    times: &[f32],
) -> Vec<(f32, Vec<ProbeParticle>)> {
    let mut out = Vec::with_capacity(times.len());
    drive(
        def,
        settings,
        times,
        |_| (origin, forward),
        |particles, _, t| {
            let list = particles
                .iter()
                .filter_map(|p| {
                    let emitter = def.emitters.get(p.emitter)?;
                    let u = &emitter.update;
                    let life = (p.age / p.life).clamp(0.0, 1.0);
                    let tint = lut_color(&u.color_over_life, life);
                    Some(ProbeParticle {
                        emitter: p.emitter,
                        position: p.pos,
                        size: p.size * lut_scalar(&u.size_over_life, life, 1.0),
                        alpha: p.color[3]
                            * lut_scalar(&u.alpha_over_life, life, 1.0)
                            * emitter.render.alpha_scale,
                        rgb: [p.color[0] * tint[0], p.color[1] * tint[1], p.color[2] * tint[2]],
                    })
                })
                .collect();
            out.push((t, list));
        },
    );
    out
}

/// The probe's state between steps.
struct ProbeRun {
    instance: VfxInstance,
    particles: Vec<Particle>,
    jobs: Vec<SpawnJob>,
    events: EventQueue,
    /// The main ring's cursor: job `j` of a frame is slot `cursor + j` on the GPU.
    cursor: u32,
    dt: f32,
    /// The pool model with a main-ring capacity (`None`: no pool, ADR-7431).
    pool: Option<ProbePool>,
    /// Integrate curl noise, vortex and attractor too ([`crate::bounds::measured_bounds`]);
    /// off for every other probe user, whose numbers stay the kinematic envelope.
    forces: bool,
}

/// The main ring as the GPU keeps it: its size, the clock and spawn history its update window
/// is cut from ([`crate::gpu::spawn_window`]).
struct ProbePool {
    capacity: u32,
    max_lifetime: Option<f32>,
    time: f32,
    history: std::collections::VecDeque<(f32, u32)>,
}

impl ProbeRun {
    fn new(settings: ProbeSettings, origin: Vec3, forward: Vec3) -> Self {
        let mut instance = VfxInstance::new(0, settings.seed, origin, forward);
        instance.set_scale(settings.scale);
        Self {
            instance,
            particles: Vec::new(),
            jobs: Vec::new(),
            events: EventQueue {
                fired: Vec::new(),
                capacity: settings.event_capacity as usize,
                window: None,
                ring: settings.child_capacity.max(1),
                cursor: 0,
                step: 0,
                stats: ProbeEventStats::default(),
            },
            cursor: 0,
            dt: probe_dt(settings),
            pool: None,
            forces: false,
        }
    }

    /// [`Self::new`] for `def`, with the main-ring pool when the settings give a capacity.
    fn for_def(def: &VfxSystemDef, settings: ProbeSettings, origin: Vec3, forward: Vec3) -> Self {
        let mut run = Self::new(settings, origin, forward);
        if settings.particle_capacity != u32::MAX {
            // The scene gives the GPU its library's longest lifetime (`VfxLibrary::max_lifetime`).
            let life = def
                .emitters
                .iter()
                .map(|e| e.spawn.lifetime.max.max(e.spawn.lifetime.min))
                .fold(0.0, f32::max);
            run.pool = Some(ProbePool {
                capacity: settings.particle_capacity.max(1),
                max_lifetime: (life.is_finite() && life > 0.0).then_some(life),
                time: 0.0,
                history: std::collections::VecDeque::new(),
            });
        }
        run
    }

    fn step(&mut self, def: &VfxSystemDef, settings: ProbeSettings, origin: Vec3, forward: Vec3) {
        let dt = self.dt;
        self.instance.set_transform(origin, forward, dt);
        self.instance
            .schedule(def, dt, &LodInput { camera_pos: origin }, u32::MAX);
        self.events.step = self.events.step.wrapping_add(1);
        self.events
            .spawn_children(def, &self.instance, &mut self.particles);
        let mut spawned = 0u32;
        for index in 0..def.emitters.len() {
            self.jobs.clear();
            self.instance.drain_emitter_jobs(index, &mut self.jobs);
            for job in &self.jobs {
                let identity = match &self.pool {
                    None => self.cursor,
                    Some(pool) => {
                        // The emit kernel writes job `j` over whatever slot `cursor + j` holds.
                        let slot = self.cursor % pool.capacity;
                        if let Some(i) = self
                            .particles
                            .iter()
                            .position(|p| p.ring_slot.is_none() && p.identity == slot)
                        {
                            self.particles.remove(i);
                        }
                        slot
                    }
                };
                spawned = spawned.wrapping_add(1);
                self.particles.push(Particle {
                    emitter: index,
                    pos: Vec3::from_array(job.position),
                    vel: Vec3::from_array(job.velocity),
                    age: 0.0,
                    life: job.lifetime,
                    size: job.size,
                    color: job.color,
                    identity,
                    ring_slot: None,
                });
                self.cursor = self.cursor.wrapping_add(1);
            }
        }
        if let Some(pool) = &mut self.pool {
            // `VfxGpu::set_frame`: the clock steps, then the window is cut for this frame.
            if dt.is_finite() {
                pool.time += dt;
            }
            let window = crate::gpu::spawn_window(
                &mut pool.history,
                crate::gpu::SpawnClock {
                    cursor: self.cursor,
                    time: pool.time,
                    max_lifetime: pool.max_lifetime,
                    capacity: pool.capacity,
                },
                spawned,
            );
            self.events.window = Some((window.0, pool.capacity));
        }
        let world = StepWorld {
            dt,
            ground: settings.ground,
            wind: settings.wind,
        };
        // The kill volumes as this frame's GPU emitter records carry them.
        let axes = self.instance.axes();
        let kills: Vec<crate::kill_volume::ResolvedKillVolume> = def
            .emitters
            .iter()
            .map(|e| {
                crate::kill_volume::resolve(
                    e.update.kill_volume.as_ref(),
                    self.instance.origin(),
                    axes,
                    self.instance.scale(),
                )
            })
            .collect();
        let forces: Vec<crate::bounds::MotionForces> = if self.forces {
            def.emitters
                .iter()
                .map(|e| crate::bounds::MotionForces::resolve(e, self.instance.origin(), axes))
                .collect()
        } else {
            Vec::new()
        };
        integrate(def, &mut self.particles, world, &mut self.events, &kills, &forces);
    }
}

/// The step a probe runs at: the setting, or 60 Hz for a useless one.
fn probe_dt(settings: ProbeSettings) -> f32 {
    if settings.dt.is_finite() && settings.dt > 1e-4 {
        settings.dt
    } else {
        1.0 / 60.0
    }
}

/// The probe's clock: plays `def`'s spawn schedule at a fixed step, integrates every particle and
/// calls `visit(particles, origin, time)` at each wanted time (sorted, each at the first step at
/// or past it).
fn drive(
    def: &VfxSystemDef,
    settings: ProbeSettings,
    times: &[f32],
    mut transform: impl FnMut(f32) -> (Vec3, Vec3),
    mut visit: impl FnMut(&[Particle], Vec3, f32),
) {
    let dt = probe_dt(settings);
    let mut wanted: Vec<f32> = times
        .iter()
        .copied()
        .filter(|t| t.is_finite() && *t >= 0.0)
        .collect();
    wanted.sort_by(f32::total_cmp);
    let Some(&last) = wanted.last() else {
        return;
    };
    let (origin0, forward0) = transform(0.0);
    let mut run = ProbeRun::for_def(def, settings, origin0, forward0);
    let mut next = 0usize;
    let steps = (last / dt).ceil() as u32 + 1;
    for step in 1..=steps {
        let t = step as f32 * dt;
        let (origin, forward) = transform(t);
        run.step(def, settings, origin, forward);
        while next < wanted.len() && t + 1e-5 >= wanted[next] {
            visit(&run.particles, run.instance.origin(), t);
            next += 1;
        }
    }
}

/// One step of the GPU update kernel's force model and event firing (see the module docs).
fn integrate(
    def: &VfxSystemDef,
    particles: &mut Vec<Particle>,
    world: StepWorld,
    events: &mut EventQueue,
    kills: &[crate::kill_volume::ResolvedKillVolume],
    forces: &[crate::bounds::MotionForces],
) {
    let StepWorld { dt, ground, wind } = world;
    particles.retain_mut(|p| {
        let Some(emitter) = def.emitters.get(p.emitter) else {
            return false;
        };
        let u = &emitter.update;
        let triggers = events::trigger_mask(emitter);
        // This slot's event index (`fire_event`'s `n`): the rank within its slot.
        let mut n = 0u32;
        if triggers & events::TRIGGER_SPAWN != 0 && p.age <= 0.0 {
            events.fire(events::TRIGGER_SPAWN, p, &mut n, p.vel);
        }
        p.age += dt;
        if p.age >= p.life {
            if triggers & events::TRIGGER_DEATH != 0 {
                events.fire(events::TRIGGER_DEATH, p, &mut n, p.vel);
            }
            return false;
        }
        let g = Vec3::new(u.gravity.0, u.gravity.1, u.gravity.2);
        let c = u.wind_response * u.wind_drag;
        p.vel = (p.vel + (g + wind * c) * dt) / (1.0 + (u.drag.max(0.0) + c) * dt);
        if let Some(f) = forces.get(p.emitter) {
            p.vel = f.apply(p.pos, p.vel, dt);
        }
        p.pos += p.vel * dt;
        if u.kill_below.is_some_and(|y| p.pos.y < y) || kills.get(p.emitter).is_some_and(|k| k.kills(p.pos)) {
            if triggers & events::TRIGGER_DEATH != 0 {
                events.fire(events::TRIGGER_DEATH, p, &mut n, p.vel);
            }
            return false;
        }
        if let (Some(collide), Some(ground)) = (u.depth_collision, ground)
            && p.pos.y < ground - collide.offset
        {
            if triggers & events::TRIGGER_COLLISION != 0 {
                events.fire(events::TRIGGER_COLLISION, p, &mut n, p.vel);
            }
            if collide.kill {
                if triggers & events::TRIGGER_DEATH != 0 {
                    events.fire(events::TRIGGER_DEATH, p, &mut n, p.vel);
                }
                return false;
            }
            p.pos.y = ground + collide.offset;
            p.vel *= 0.3;
        }
        true
    });
    events.keep_first();
}

/// A visible particle as the snapshot metrics see it.
struct Seen {
    pos: Vec3,
    vel: Vec3,
    size: f32,
    alpha: f32,
    luma: f32,
}

fn snapshot(def: &VfxSystemDef, particles: &[Particle], origin: Vec3, time: f32) -> ProbeSnapshot {
    let flat = |v: Vec3| Vec3::new(v.x, 0.0, v.z);
    let emitters = def
        .emitters
        .iter()
        .enumerate()
        .map(|(index, emitter)| {
            let u = &emitter.update;
            let mut live = 0u32;
            let mut seen: Vec<Seen> = Vec::new();
            for p in particles.iter().filter(|p| p.emitter == index) {
                live += 1;
                let t = (p.age / p.life).clamp(0.0, 1.0);
                let alpha = p.color[3] * lut_scalar(&u.alpha_over_life, t, 1.0) * emitter.render.alpha_scale;
                if alpha <= PROBE_VISIBLE_ALPHA {
                    continue;
                }
                let tint = lut_color(&u.color_over_life, t);
                let rgb = [p.color[0] * tint[0], p.color[1] * tint[1], p.color[2] * tint[2]];
                seen.push(Seen {
                    pos: p.pos,
                    vel: p.vel,
                    size: p.size * lut_scalar(&u.size_over_life, t, 1.0),
                    alpha,
                    luma: 0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2],
                });
            }
            let visible = seen.len() as u32;
            let n = visible.max(1) as f32;
            let weight: f32 = seen.iter().map(|s| s.alpha).sum::<f32>().max(1e-6);
            let centroid = if seen.is_empty() {
                origin
            } else {
                seen.iter().fold(Vec3::ZERO, |acc, s| acc + s.pos * s.alpha) / weight
            };
            let mut fronts: Vec<f32> = seen.iter().map(|s| flat(s.pos - origin).length()).collect();
            let mut heights: Vec<f32> = seen.iter().map(|s| s.pos.y - origin.y).collect();
            EmitterProbe {
                name: emitter.name.clone(),
                live,
                visible,
                centroid,
                envelope_radius: seen
                    .iter()
                    .map(|s| (s.pos - centroid).length() + 0.5 * s.size)
                    .fold(0.0f32, f32::max),
                front_p90: percentile(&mut fronts, 0.9),
                top: seen
                    .iter()
                    .map(|s| s.pos.y - origin.y + 0.5 * s.size)
                    .fold(0.0f32, f32::max),
                height_p90: percentile(&mut heights, 0.9),
                mean_speed: seen.iter().map(|s| s.vel.length()).sum::<f32>() / n,
                mean_flat_speed: seen.iter().map(|s| flat(s.vel).length()).sum::<f32>() / n,
                mean_rise: seen.iter().map(|s| s.vel.y).sum::<f32>() / n,
                mean_alpha: seen.iter().map(|s| s.alpha).sum::<f32>() / n,
                mean_luma: seen.iter().map(|s| s.luma * s.alpha).sum::<f32>() / weight,
                max_size: seen.iter().map(|s| s.size).fold(0.0f32, f32::max),
                drift: flat(centroid - origin),
            }
        })
        .collect();
    ProbeSnapshot { time, emitters }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{
        EmitterDef, InitModule, RangeDef, RenderModule, ShapeDef, SpawnMode, SpawnModule, SystemModule,
        UpdateModule, VFX_FORMAT_VERSION, VelocityDef, VelocityMode,
    };

    fn one(update: UpdateModule, speed: f32, life: f32) -> VfxSystemDef {
        VfxSystemDef {
            version: VFX_FORMAT_VERSION,
            name: "probe".into(),
            system: SystemModule::default(),
            emitters: vec![EmitterDef {
                name: "e".into(),
                delay: 0.0,
                duration: None,
                spawn: SpawnModule {
                    mode: SpawnMode::Burst(1),
                    lifetime: RangeDef { min: life, max: life },
                    count_scale: 1.0,
                },
                init: InitModule {
                    shape: ShapeDef::Point,
                    velocity: VelocityDef {
                        mode: VelocityMode::Directional {
                            dir: (0.0, 0.0, 1.0),
                            spread: 0.0,
                            speed: RangeDef {
                                min: speed,
                                max: speed,
                            },
                        },
                        inherit: 0.0,
                    },
                    size: RangeDef { min: 2.0, max: 2.0 },
                    rotation: RangeDef::default(),
                    spin: RangeDef::default(),
                    color: Vec::new(),
                },
                update,
                render: RenderModule::default(),
            }],
        }
    }

    /// A particle launched straight up with no forces climbs at its launch speed; with drag
    /// and lift it relaxes to the terminal speed `lift / drag`, as the kernel integrates.
    #[test]
    fn probe_matches_the_kernel_force_model() {
        let free = one(UpdateModule::default(), 4.0, 10.0);
        let snaps = probe_effect(&free, Vec3::ZERO, Vec3::Y, ProbeSettings::default(), &[2.0]);
        let e = &snaps[0].emitters[0];
        assert_eq!(e.live, 1);
        assert!((e.height_p90 - 8.0).abs() < 0.1, "free climb {}", e.height_p90);
        assert!((e.top - 9.0).abs() < 0.1, "half size on top");
        let lifted = one(
            UpdateModule {
                gravity: (0.0, 2.0, 0.0),
                drag: 0.5,
                ..UpdateModule::default()
            },
            0.0,
            40.0,
        );
        let snaps = probe_effect(&lifted, Vec3::ZERO, Vec3::Y, ProbeSettings::default(), &[30.0]);
        let e = &snaps[0].emitters[0];
        assert!(
            (e.mean_speed - 4.0).abs() < 0.05,
            "terminal speed {}",
            e.mean_speed
        );
    }

    #[test]
    fn probe_is_deterministic_and_respects_lifetime_and_ground() {
        let mut def = one(
            UpdateModule {
                gravity: (0.0, -9.81, 0.0),
                depth_collision: Some(crate::format::DepthCollisionDef {
                    kill: false,
                    offset: 0.1,
                }),
                ..UpdateModule::default()
            },
            5.0,
            3.0,
        );
        let a = probe_effect(
            &def,
            Vec3::ZERO,
            Vec3::Y,
            ProbeSettings::default(),
            &[0.5, 2.5, 3.5],
        );
        let b = probe_effect(
            &def,
            Vec3::ZERO,
            Vec3::Y,
            ProbeSettings::default(),
            &[3.5, 0.5, 2.5],
        );
        assert_eq!(a, b, "same inputs, same snapshots, times in any order");
        assert!(
            a[1].emitters[0].height_p90 > -0.15,
            "rests on the ground (within the offset)"
        );
        assert!(a[1].emitters[0].height_p90 < 0.2);
        assert_eq!(a[2].emitters[0].live, 0, "dead after its lifetime");
        def.emitters[0].update.alpha_over_life = vec![FloatKey { t: 0.0, v: 0.0 }];
        let hidden = probe_effect(&def, Vec3::ZERO, Vec3::Y, ProbeSettings::default(), &[0.5]);
        assert_eq!(hidden[0].emitters[0].live, 1);
        assert_eq!(hidden[0].emitters[0].visible, 0, "zero alpha is not visible");
        assert!(probe_effect(&def, Vec3::ZERO, Vec3::Y, ProbeSettings::default(), &[]).is_empty());
    }

    /// A rate parent firing OnSpawn into a one-particle child, as `.vfx.ron`.
    fn spawn_chain(per_step: f32, dt: f32) -> VfxSystemDef {
        VfxSystemDef::from_ron(&format!(
            r#"(
    version: {VFX_FORMAT_VERSION},
    name: "chain",
    system: (budget: 100000),
    emitters: [
        (
            name: "parent",
            spawn: (mode: Rate({rate:?}), lifetime: (min: 0.02, max: 0.02)),
            init: (size: (min: 0.2, max: 0.4)),
            update: (events: [(on: Spawn, child: "child", count: 1)]),
            render: (mode: Billboard),
        ),
        (
            name: "child",
            spawn: (mode: Event, lifetime: (min: 1.0, max: 2.0)),
            init: (size: (min: 0.2, max: 0.4)),
            render: (mode: Billboard),
        ),
    ],
)"#,
            rate = per_step / dt
        ))
        .expect("chain parses")
    }

    /// ADR-7431 (4): with a pool capacity the probe keeps the GPU's keep-set on a whole-pool
    /// frame. 24 parents a step in a 64-slot ring: step 3's parents are spawns 48..72 in slots
    /// 48..64 and 0..8, the window (2.25 s of spawns) fills the pool, and the first 16 events in
    /// slot order are slots 0..8 then 48..56, each seeded by its slot (`particle_identity`). The
    /// probe without a pool keeps spawns 48..64.
    #[test]
    fn a_whole_pool_frame_keeps_events_in_slot_order() {
        let dt = 1.0 / 64.0;
        let def = spawn_chain(24.0, dt);
        let settings = ProbeSettings {
            dt,
            ground: None,
            event_capacity: 16,
            child_capacity: 64,
            particle_capacity: 64,
            ..ProbeSettings::default()
        };
        let kept = |settings: ProbeSettings| {
            let mut run = ProbeRun::for_def(&def, settings, Vec3::ZERO, Vec3::Y);
            for _ in 0..3 {
                run.step(&def, settings, Vec3::ZERO, Vec3::Y);
            }
            assert_eq!(run.cursor, 72);
            run.events
                .fired
                .iter()
                .map(|(_, _, f)| f.seed)
                .collect::<Vec<u32>>()
        };
        let seeds = |slots: &[u32]| -> Vec<u32> {
            slots
                .iter()
                .map(|&slot| events::event_seed(slot, 3, events::TRIGGER_SPAWN))
                .collect()
        };
        let slot_order: Vec<u32> = (0..8).chain(48..56).collect();
        assert_eq!(kept(settings), seeds(&slot_order), "slot order from slot 0");
        let spawn_order: Vec<u32> = (48..64).collect();
        assert_eq!(
            kept(ProbeSettings {
                particle_capacity: u32::MAX,
                ..settings
            }),
            seeds(&spawn_order),
            "without a pool: spawn order, as before ADR-7431"
        );
        // A window that covers less than the pool keeps the oldest first: step 2's 24 spawns in
        // a 256-slot ring keep spawns 24..40.
        let mut run = ProbeRun::for_def(
            &def,
            ProbeSettings {
                particle_capacity: 256,
                ..settings
            },
            Vec3::ZERO,
            Vec3::Y,
        );
        for _ in 0..2 {
            run.step(&def, settings, Vec3::ZERO, Vec3::Y);
        }
        let partial: Vec<u32> = run.events.fired.iter().map(|(_, _, f)| f.seed).collect();
        let oldest: Vec<u32> = (24..40)
            .map(|slot| events::event_seed(slot, 2, events::TRIGGER_SPAWN))
            .collect();
        assert_eq!(partial, oldest);
    }
}
