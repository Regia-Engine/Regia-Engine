// SPDX-License-Identifier: MPL-2.0
//! Shock-arrival camera shake: the shake of a distant blast, arriving when its pressure front
//! does.
//!
//! A blast shakes a camera `distance / speed` seconds after it goes off (sound speed, 343 m/s),
//! and the harder the farther it is the weaker. [`ShockShake`] holds that law as pure functions:
//! [`arrival_delay_s`](ShockShake::arrival_delay_s), [`amplitude`](ShockShake::amplitude),
//! [`to_shake`](ShockShake::to_shake) (a [`Shake`] for the
//! [`CameraDirector`]) and
//! [`arrival_frame`](ShockShake::arrival_frame) (the fixed step it lands on).
//!
//! [`ShockShakePlugin`] runs a [`ShockShakeSchedule`] on the fixed step: blasts scheduled with
//! their blast frame fire into the director on their arrival frame, so a sequence replays the
//! same shake on the same frame every time.
//!
//! The amplitude law is an inverse-distance falloff chosen for the look (far-field blast
//! pressure also falls roughly with the reciprocal of distance); it is not a physical
//! simulation. See `docs/provenance.md`.

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_scene::Transform;
use regia_scene::components::EntityGuid;

use crate::camera::{CameraDirector, CameraRig, Shake};
use crate::ground_shock::{GroundShock, MountShock, WaveKind};

/// Speed of sound in air at about 20 °C (m/s): the crash effects' blast-front speed, so a
/// scheduled shock shake lands on the frame the visible front and its gust reach the camera
/// (ADR-1497).
pub const SPEED_OF_SOUND_M_S: f32 = regia_vfx::crash::SHOCK_FRONT_SPEED;

/// Wobble frequency of a shock shake (hertz).
const SHAKE_FREQUENCY_HZ: f32 = 7.0;

/// Amplitude below which a shock is not worth a shake (metres).
const MIN_AMPLITUDE: f32 = 1.0e-4;

/// One blast's shake law.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShockShake {
    /// Blast strength, as the unclamped shake amplitude (metres) at `reference_distance_m`.
    pub energy: f32,
    /// Speed the shock travels (m/s). Non-positive or non-finite reads as 343.
    pub speed_m_s: f32,
    /// Distance at which the amplitude equals `energy` (metres); closer is clamped to
    /// `max_amplitude`. Non-positive or non-finite reads as 1.
    pub reference_distance_m: f32,
    /// Largest amplitude any distance produces (metres), so a close blast cannot throw the
    /// camera away. Negative or non-finite reads as 0.
    pub max_amplitude: f32,
    /// How long the shake lasts (seconds).
    pub duration_s: f32,
}

impl Default for ShockShake {
    fn default() -> Self {
        Self {
            energy: 1.5,
            speed_m_s: SPEED_OF_SOUND_M_S,
            reference_distance_m: 1000.0,
            max_amplitude: 0.5,
            duration_s: 3.0,
        }
    }
}

impl ShockShake {
    /// The showcase's blast (about 0.3 kt, seen from 3 to 4 km): a gentle shake, 0.1 m at 3.5 km,
    /// never above 0.2 m, lasting 3.5 s.
    pub fn showcase() -> Self {
        Self {
            energy: 0.35,
            speed_m_s: SPEED_OF_SOUND_M_S,
            reference_distance_m: 1000.0,
            max_amplitude: 0.2,
            duration_s: 3.5,
        }
    }

    fn speed(&self) -> f64 {
        if self.speed_m_s.is_finite() && self.speed_m_s > 0.0 {
            f64::from(self.speed_m_s)
        } else {
            f64::from(SPEED_OF_SOUND_M_S)
        }
    }

    /// Seconds from the blast to the shock reaching `distance_m`. `0` for a non-positive or NaN
    /// distance.
    pub fn arrival_delay_s(&self, distance_m: f32) -> f32 {
        self.delay_f64(distance_m) as f32
    }

    pub(crate) fn delay_f64(&self, distance_m: f32) -> f64 {
        if distance_m.is_nan() || distance_m <= 0.0 {
            return 0.0;
        }
        f64::from(distance_m) / self.speed()
    }

    /// Shake amplitude in metres at `distance_m`: `energy · reference / distance`, clamped to
    /// `0..=max_amplitude`, so it never grows with distance. A NaN distance gives `0` (an unknown
    /// place is not shaken); a non-positive one is at the source and gives the clamped energy.
    pub fn amplitude(&self, distance_m: f32) -> f32 {
        if distance_m.is_nan() {
            return 0.0;
        }
        let max = if self.max_amplitude.is_finite() {
            self.max_amplitude.max(0.0)
        } else {
            0.0
        };
        let energy = if self.energy.is_finite() {
            self.energy.max(0.0)
        } else {
            0.0
        };
        let reference = if self.reference_distance_m.is_finite() && self.reference_distance_m > 0.0 {
            self.reference_distance_m
        } else {
            1.0
        };
        // Inside the reference distance the law is flat at `energy` (no blow-up at 0).
        let d = distance_m.max(reference);
        let amplitude = energy * (reference / d);
        if amplitude.is_finite() {
            amplitude.clamp(0.0, max)
        } else {
            0.0
        }
    }

    /// The delayed shake for a camera `distance_m` from the blast: `(delay_s, shake)`, or `None`
    /// when the distance is NaN, the duration is not positive, or the amplitude is negligible.
    pub fn to_shake(&self, distance_m: f32) -> Option<(f32, Shake)> {
        if distance_m.is_nan() || !(self.duration_s.is_finite() && self.duration_s > 0.0) {
            return None;
        }
        let amplitude = self.amplitude(distance_m);
        if amplitude < MIN_AMPLITUDE {
            return None;
        }
        Some((
            self.arrival_delay_s(distance_m),
            Shake {
                amplitude,
                frequency_hz: SHAKE_FREQUENCY_HZ,
                duration_secs: self.duration_s,
                falloff: 2.0,
            },
        ))
    }

    /// The fixed step the shock arrives on: `start_frame` plus the delay in steps of
    /// `fixed_hz`, rounded up so it never arrives early. A non-positive or non-finite
    /// `fixed_hz` returns `start_frame`.
    pub fn arrival_frame(&self, distance_m: f32, fixed_hz: f32, start_frame: u64) -> u64 {
        if !(fixed_hz.is_finite() && fixed_hz > 0.0) {
            return start_frame;
        }
        let steps = self.delay_f64(distance_m) * f64::from(fixed_hz);
        // The tolerance keeps an exact multiple (343 m at 60 Hz = 60 steps) from rounding up.
        let whole = (steps - 1.0e-9).ceil().max(0.0);
        start_frame.saturating_add(whole as u64)
    }
}

/// The fixed step `delay_s` after `start_frame`, rounded up so it never arrives early.
pub(crate) fn frame_after(delay_s: f64, fixed_hz: f32, start_frame: u64) -> u64 {
    if !(fixed_hz.is_finite() && fixed_hz > 0.0) {
        return start_frame;
    }
    let steps = delay_s * f64::from(fixed_hz);
    start_frame.saturating_add((steps - 1.0e-9).ceil().max(0.0) as u64)
}

/// A shock waiting for its arrival frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PendingShock {
    /// Frame the blast went off on.
    pub blast_frame: u64,
    /// Distance from the blast to the camera (metres).
    pub distance_m: f32,
    /// The frame the shake fires on.
    pub arrival_frame: u64,
    /// The shake to fire.
    pub shake: Shake,
}

/// A mount oscillation waiting for its arrival frame (the seismic wave or the airblast of
/// [`GroundShock`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PendingGround {
    /// Which wave.
    pub kind: WaveKind,
    /// Frame the blast went off on.
    pub blast_frame: u64,
    /// Distance from the blast to the camera (metres).
    pub distance_m: f32,
    /// The frame the oscillation starts on.
    pub arrival_frame: u64,
    /// The oscillation.
    pub shock: MountShock,
}

/// Shocks scheduled on the fixed step, and the step counter they run against.
#[derive(Resource, Debug, Clone)]
pub struct ShockShakeSchedule {
    frame: u64,
    fixed_hz: f32,
    pending: Vec<PendingShock>,
    fired: u64,
    ground_pending: Vec<PendingGround>,
    ground_fired: u64,
    next_seed: u64,
}

impl Default for ShockShakeSchedule {
    fn default() -> Self {
        Self {
            frame: 0,
            fixed_hz: 60.0,
            pending: Vec::new(),
            fired: 0,
            ground_pending: Vec::new(),
            ground_fired: 0,
            next_seed: 1,
        }
    }
}

impl ShockShakeSchedule {
    /// The current fixed step.
    pub fn frame(&self) -> u64 {
        self.frame
    }

    /// Sets the step counter (a seek). Shocks whose arrival is already past fire on the next step.
    pub fn set_frame(&mut self, frame: u64) {
        self.frame = frame;
    }

    /// The fixed rate the delays are converted at.
    pub fn fixed_hz(&self) -> f32 {
        self.fixed_hz
    }

    /// Shocks that have not arrived yet, in scheduling order.
    pub fn pending(&self) -> &[PendingShock] {
        &self.pending
    }

    /// How many shakes this schedule has fired into the camera director.
    pub fn fired(&self) -> u64 {
        self.fired
    }

    /// Schedules the shake of a blast at `blast_frame`, `distance_m` from the camera. Returns
    /// the arrival frame, or `None` when the shock is negligible (see [`ShockShake::to_shake`]).
    pub fn schedule(&mut self, blast: &ShockShake, distance_m: f32, blast_frame: u64) -> Option<u64> {
        let (_, shake) = blast.to_shake(distance_m)?;
        let arrival_frame = blast.arrival_frame(distance_m, self.fixed_hz, blast_frame);
        self.pending.push(PendingShock {
            blast_frame,
            distance_m,
            arrival_frame,
            shake,
        });
        Some(arrival_frame)
    }

    /// Seismic and airblast oscillations that have not arrived yet, in scheduling order.
    pub fn ground_pending(&self) -> &[PendingGround] {
        &self.ground_pending
    }

    /// How many mount oscillations this schedule has fired into the camera director.
    pub fn ground_fired(&self) -> u64 {
        self.ground_fired
    }

    /// Schedules the seismic wave and the airblast of a blast at `blast_frame`, `distance_m`
    /// from the camera, each on its own arrival frame (the seismic wave first). Returns
    /// `(seismic_arrival, airblast_arrival)`; a wave that is negligible at that distance is
    /// `None`. The oscillations' frequencies and phases come from a counter of the schedule, so
    /// the same sequence of calls replays the same shake.
    pub fn schedule_ground(
        &mut self,
        blast: &GroundShock,
        distance_m: f32,
        blast_frame: u64,
    ) -> (Option<u64>, Option<u64>) {
        let mut arrivals = [None, None];
        for (slot, (kind, delay)) in [
            (WaveKind::Seismic, blast.seismic_delay_f64(distance_m)),
            (WaveKind::Airblast, blast.air_delay_f64(distance_m)),
        ]
        .into_iter()
        .enumerate()
        {
            let Some(shock) = blast.mount_shock(kind, distance_m, self.next_seed) else {
                continue;
            };
            self.next_seed += 1;
            let arrival_frame = frame_after(delay, self.fixed_hz, blast_frame);
            self.ground_pending.push(PendingGround {
                kind,
                blast_frame,
                distance_m,
                arrival_frame,
                shock,
            });
            arrivals[slot] = Some(arrival_frame);
        }
        (arrivals[0], arrivals[1])
    }

    /// Removes every pending shock.
    pub fn clear(&mut self) {
        self.pending.clear();
        self.ground_pending.clear();
    }

    /// Fires the shocks due at the current step into `director`, then advances the step.
    fn step(&mut self, director: &mut CameraDirector) {
        let frame = self.frame;
        let mut fired = 0u64;
        self.pending.retain(|shock| {
            if shock.arrival_frame <= frame {
                director.shake(shock.shake);
                fired += 1;
                false
            } else {
                true
            }
        });
        self.fired += fired;
        let mut ground = 0u64;
        self.ground_pending.retain(|wave| {
            if wave.arrival_frame <= frame {
                director.mount_shock(wave.shock);
                ground += 1;
                false
            } else {
                true
            }
        });
        self.ground_fired += ground;
        self.frame = frame.saturating_add(1);
    }
}

/// A blast at a world position, going off now. The plugin measures the distance to the
/// [`CameraRig`] with the lowest `EntityGuid` (guid-less rigs sort last), breaking ties by entity
/// bits, and schedules the shake for its arrival frame.
#[derive(Message, Debug, Clone, Copy, PartialEq)]
pub struct BlastShock {
    /// Where the blast goes off.
    pub position: Vec3,
    /// The shake law of this blast.
    pub shock: ShockShake,
}

/// Turns [`BlastShock`] messages into scheduled shocks, measured to the lowest stable camera key.
fn schedule_blast_shocks(
    mut blasts: MessageReader<BlastShock>,
    mut schedule: ResMut<ShockShakeSchedule>,
    cameras: Query<(Entity, &Transform, Option<&EntityGuid>), With<CameraRig>>,
) {
    let Some((_, camera, _)) = cameras
        .iter()
        .min_by_key(|(entity, _, guid)| (guid.is_none(), guid.map_or(0, |guid| guid.0.0), entity.to_bits()))
    else {
        // Drain: a blast with nobody to shake is dropped, not kept for later.
        blasts.read().for_each(drop);
        return;
    };
    for blast in blasts.read() {
        let distance = (camera.translation - blast.position).length();
        let now = schedule.frame;
        schedule.schedule(&blast.shock, distance, now);
    }
}

/// Fires due shocks on every fixed step.
fn fire_shock_shakes(
    fixed: Res<FixedTime>,
    mut schedule: ResMut<ShockShakeSchedule>,
    director: Option<ResMut<CameraDirector>>,
) {
    let step = fixed.step().as_secs_f64();
    if step > 0.0 {
        schedule.fixed_hz = (1.0 / step) as f32;
    }
    match director {
        Some(mut director) => schedule.step(&mut director),
        None => schedule.frame = schedule.frame.saturating_add(1),
    }
}

/// Installs the [`ShockShakeSchedule`] and the fixed-step system that fires it, and the
/// [`BlastGustGrassPlugin`](crate::blast_gust::BlastGustGrassPlugin) that bends the grass to the
/// same blast front. Needs the [`CameraPlugin`](crate::camera::CameraPlugin)'s director to have
/// an effect.
pub struct ShockShakePlugin;

impl Plugin for ShockShakePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ShockShakeSchedule>()
            .add_message::<BlastShock>()
            .add_systems(FixedUpdate, (schedule_blast_shocks, fire_shock_shakes).chain());
        // The blast's gust bends the grass on the clock the shake uses (ADR-1497).
        if !app.has_plugin(std::any::type_name::<crate::blast_gust::BlastGustGrassPlugin>()) {
            app.add_plugins(crate::blast_gust::BlastGustGrassPlugin);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::id::Guid;

    fn blast() -> ShockShake {
        ShockShake::default()
    }

    #[test]
    fn three_hundred_forty_three_metres_is_one_second_and_sixty_frames() {
        let b = blast();
        assert_eq!(b.arrival_delay_s(343.0), 1.0);
        assert_eq!(b.arrival_frame(343.0, 60.0, 0), 60);
        assert_eq!(b.arrival_frame(343.0, 60.0, 100), 160);
        assert_eq!(b.arrival_frame(3430.0, 60.0, 0), 600);
    }

    #[test]
    fn arrival_frame_rounds_up_and_never_early() {
        let b = blast();
        // 344 m is 1.0029 s = 60.17 steps: the 61st.
        assert_eq!(b.arrival_frame(344.0, 60.0, 0), 61);
        assert_eq!(b.arrival_frame(1.0, 60.0, 0), 1);
        // The showcase range: 3 to 4 km is about 8.7 to 11.7 s.
        for km in [3000.0f32, 3500.0, 4000.0] {
            let frame = b.arrival_frame(km, 60.0, 0);
            let exact = f64::from(km) / 343.0 * 60.0;
            assert!(
                frame as f64 >= exact && (frame as f64) < exact + 1.0,
                "{km}: {frame} vs {exact}"
            );
        }
    }

    #[test]
    fn bad_distances_arrive_at_once() {
        let b = blast();
        for d in [0.0, -5.0, f32::NAN, f32::NEG_INFINITY] {
            assert_eq!(b.arrival_delay_s(d), 0.0, "{d}");
            assert_eq!(b.arrival_frame(d, 60.0, 7), 7, "{d}");
        }
        assert_eq!(b.arrival_frame(343.0, 0.0, 7), 7);
        assert_eq!(b.arrival_frame(343.0, f32::NAN, 7), 7);
        assert_eq!(
            b.arrival_frame(f32::INFINITY, 60.0, 7),
            u64::MAX,
            "saturates, no panic"
        );
    }

    #[test]
    fn bad_speed_reads_as_the_speed_of_sound() {
        for speed in [0.0, -1.0, f32::NAN] {
            let b = ShockShake {
                speed_m_s: speed,
                ..blast()
            };
            assert_eq!(b.arrival_delay_s(343.0), 1.0, "{speed}");
        }
        let fast = ShockShake {
            speed_m_s: 686.0,
            ..blast()
        };
        assert_eq!(fast.arrival_delay_s(343.0), 0.5);
    }

    #[test]
    fn amplitude_falls_with_distance_and_stays_in_bounds() {
        let b = blast();
        let mut last = f32::MAX;
        for d in [0.0, 1.0, 100.0, 1000.0, 2000.0, 3500.0, 10_000.0, 1.0e6] {
            let a = b.amplitude(d);
            assert!((0.0..=b.max_amplitude).contains(&a), "{d}: {a}");
            assert!(a <= last, "non-increasing at {d}");
            last = a;
        }
        assert_eq!(b.amplitude(0.0), 0.5, "clamped to the maximum at the source");
        assert_eq!(b.amplitude(1000.0), 0.5, "energy 1.5 clamps to the maximum");
        assert!((b.amplitude(3000.0) - 0.5).abs() < 1e-6, "1.5 * 1000 / 3000");
        assert!(
            (b.amplitude(6000.0) - 0.25).abs() < 1e-6,
            "half again at double the distance"
        );
    }

    #[test]
    fn amplitude_is_nan_safe() {
        let b = blast();
        assert_eq!(b.amplitude(f32::NAN), 0.0);
        assert_eq!(b.amplitude(f32::INFINITY), 0.0);
        for bad in [f32::NAN, f32::INFINITY, -1.0] {
            let broken = ShockShake {
                energy: bad,
                reference_distance_m: bad,
                max_amplitude: bad,
                ..blast()
            };
            for d in [0.0, 10.0, 5000.0, f32::NAN] {
                let a = broken.amplitude(d);
                assert!(a.is_finite() && a >= 0.0, "{bad} at {d}: {a}");
            }
        }
    }

    #[test]
    fn to_shake_carries_the_delay_and_the_amplitude() {
        let b = ShockShake {
            energy: 0.2,
            ..blast()
        };
        let (delay, shake) = b.to_shake(3430.0).expect("a shake");
        assert_eq!(delay, 10.0);
        assert!((shake.amplitude - 0.2 * 1000.0 / 3430.0).abs() < 1e-6);
        assert_eq!(shake.duration_secs, 3.0);
        assert!(b.to_shake(f32::NAN).is_none());
        assert!(b.to_shake(1.0e9).is_none(), "negligible far away");
        let instant = ShockShake {
            duration_s: 0.0,
            ..blast()
        };
        assert!(instant.to_shake(10.0).is_none());
        let silent = ShockShake {
            energy: 0.0,
            ..blast()
        };
        assert!(silent.to_shake(10.0).is_none());
    }

    #[test]
    fn the_showcase_blast_is_gentle_at_the_shot_four_range() {
        let b = ShockShake::showcase();
        for d in [3000.0f32, 3500.0, 4000.0] {
            let a = b.amplitude(d);
            assert!(a > 0.05 && a < 0.13, "{d}: {a}");
        }
        assert!(b.amplitude(3000.0) > b.amplitude(4000.0));
        assert!(b.amplitude(10.0) <= 0.2);
    }

    #[test]
    fn the_schedule_fires_on_the_arrival_frame() {
        let mut schedule = ShockShakeSchedule::default();
        let mut director = CameraDirector::default();
        let arrival = schedule.schedule(&blast(), 343.0, 0).expect("scheduled");
        assert_eq!(arrival, 60);
        for _ in 0..60 {
            schedule.step(&mut director);
        }
        assert!(!director.is_shaking(), "nothing before frame 60");
        assert_eq!(schedule.pending().len(), 1);
        schedule.step(&mut director);
        assert!(director.is_shaking(), "fired on frame 60");
        assert_eq!(schedule.fired(), 1);
        assert!(schedule.pending().is_empty());
        assert_eq!(schedule.frame(), 61);
    }

    #[test]
    fn a_far_blast_arrives_later_and_weaker() {
        let mut schedule = ShockShakeSchedule::default();
        let near = schedule.schedule(&blast(), 3000.0, 0).expect("near");
        let far = schedule.schedule(&blast(), 4000.0, 0).expect("far");
        assert!(far > near);
        assert!(schedule.pending()[1].shake.amplitude < schedule.pending()[0].shake.amplitude);
        assert!(schedule.schedule(&blast(), f32::NAN, 0).is_none());
        assert_eq!(schedule.pending().len(), 2);
    }

    #[test]
    fn blast_distance_uses_the_lowest_guid_camera_after_an_archetype_move() {
        #[derive(Component)]
        struct Marker;

        let mut app = App::new();
        app.add_plugins((MinimalPlugins, ShockShakePlugin));
        app.insert_resource(TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_millis(17),
        ));
        let near = app
            .world_mut()
            .spawn((CameraRig::default(), Transform::default(), EntityGuid(Guid(1))))
            .id();
        app.world_mut().spawn((
            CameraRig::default(),
            Transform::from_translation(Vec3::new(343.0, 0.0, 0.0)),
            EntityGuid(Guid(2)),
        ));
        app.world_mut().entity_mut(near).insert(Marker);
        app.world_mut().write_message(BlastShock {
            position: Vec3::ZERO,
            shock: blast(),
        });
        app.update();
        let pending = app.world().resource::<ShockShakeSchedule>();
        assert_eq!(pending.pending().len(), 1);
        assert_eq!(pending.pending()[0].distance_m, 0.0);
    }

    #[test]
    fn duplicate_camera_guids_tie_break_on_entity_bits() {
        #[derive(Component)]
        struct Marker;

        let mut app = App::new();
        app.add_plugins((MinimalPlugins, ShockShakePlugin));
        app.insert_resource(TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_millis(17),
        ));
        let first = app
            .world_mut()
            .spawn((CameraRig::default(), Transform::default(), EntityGuid(Guid(7))))
            .id();
        let second = app
            .world_mut()
            .spawn((CameraRig::default(), Transform::default(), EntityGuid(Guid(7))))
            .id();
        let (near, far) = if first.to_bits() < second.to_bits() {
            (first, second)
        } else {
            (second, first)
        };
        app.world_mut()
            .entity_mut(near)
            .insert((Transform::default(), Marker));
        app.world_mut()
            .entity_mut(far)
            .insert(Transform::from_translation(Vec3::new(343.0, 0.0, 0.0)));
        app.world_mut().write_message(BlastShock {
            position: Vec3::ZERO,
            shock: blast(),
        });
        app.update();
        let pending = app.world().resource::<ShockShakeSchedule>();
        assert_eq!(pending.pending().len(), 1);
        assert_eq!(pending.pending()[0].distance_m, 0.0);
    }

    #[test]
    fn the_ground_wave_fires_before_the_airblast_and_both_play_in_the_director() {
        let mut schedule = ShockShakeSchedule::default();
        let mut director = CameraDirector::default();
        let (seismic, air) = schedule.schedule_ground(&GroundShock::default(), 3300.0, 0);
        // 3.3 km: P wave at 0.6 s (36 steps), airblast at 9.62 s (578 steps).
        assert_eq!((seismic, air), (Some(36), Some(578)));
        assert_eq!(schedule.ground_pending().len(), 2);
        for _ in 0..36 {
            schedule.step(&mut director);
        }
        assert!(!director.is_shaking());
        schedule.step(&mut director);
        assert!(director.is_shaking());
        assert_eq!(schedule.ground_fired(), 1);
        assert_eq!(schedule.ground_pending().len(), 1);
        for _ in 0..542 {
            schedule.step(&mut director);
        }
        assert_eq!(schedule.ground_fired(), 2);
    }

    #[test]
    fn the_ground_schedule_is_deterministic_and_ignores_the_negligible() {
        let run = || {
            let mut schedule = ShockShakeSchedule::default();
            schedule.schedule_ground(&GroundShock::default(), 1000.0, 5);
            schedule.schedule_ground(&GroundShock::default(), 2000.0, 5);
            schedule.ground_pending().to_vec()
        };
        assert_eq!(run(), run());
        let seeds: Vec<u64> = run().iter().map(|w| w.shock.seed).collect();
        assert_eq!(seeds, vec![1, 2, 3, 4], "each wave has its own seed");
        let mut schedule = ShockShakeSchedule::default();
        assert_eq!(
            schedule.schedule_ground(&GroundShock::default(), 1.0e9, 0),
            (None, None)
        );
        assert_eq!(
            schedule.schedule_ground(&GroundShock::default(), f32::NAN, 0),
            (None, None)
        );
        assert!(schedule.ground_pending().is_empty());
    }
}
