// SPDX-License-Identifier: MPL-2.0
//! Vehicle feedback (ADR-3276): what a driver feels through the pad, from the tires.
//!
//! A [`VehicleFeedback`] on a vehicle entity (one with a `VehicleController`) turns the tires'
//! state after every fixed step into two channels, each in `[0, 1]`:
//!
//! - **low frequency**: the steering torque, the sum of the steered wheels' self-aligning torques
//!   (`regia_physics::vehicle_tire::total_steering_torque`), through a dead band and a full-scale
//!   torque: `clamp((|torque| − deadband) / (full_scale − deadband), 0, 1)`. It needs slip tires
//!   with a `pneumatic_trail`; damper tires report none.
//! - **high frequency**: curb strikes, landings and impacts, read as the largest step-to-step
//!   change of a wheel's load over its static share of the weight (`chassis_mass · 9.81 / wheels`),
//!   through a threshold and a full scale; each jolt then fades with the time constant
//!   `jolt_decay`. A wheel that leaves the ground and lands is a jolt (its load goes 0 → load).
//!
//! The device API has one motor strength per request ([`RumbleRequest::strength`]), so the request
//! carries `max(low, high) · strength_scale`; both channels stay readable on the component (and over
//! the agent: `physics.vehicle_telemetry` reports the raw steering torque per vehicle and per steered
//! axle for a force-feedback wheel; this crate drives no wheel device). With `device` set, a request
//! goes out every fixed step whose strength is above 0, lasting `request_duration` seconds, so the
//! rumble stops by itself shortly after the feel does. Driving straight on flat ground gives 0 on
//! both channels.
//!
//! [`VehicleFeedbackPlugin`] runs [`update_vehicle_feedback`] in `FixedUpdate` after the vehicles
//! are driven. Fixed steps and the vehicle's own state only: no wall clock. The buffers are kept on
//! the component, so a steady state allocates nothing.

use regia_app::prelude::*;
use regia_physics::vehicle::{MAX_WHEELS, VehicleController, drive_vehicles};
use regia_physics::vehicle_tire::{
    AxleSteeringTorque, TireForces, steering_torques_into, total_steering_torque,
};
use regia_platform::RumbleRequest;

/// Standard gravity for the static wheel load (m/s²).
const GRAVITY: f32 = 9.81;

/// The documented map from tire state to the two rumble channels. Every field is clamped to a sane
/// range when read, so a bad value cannot make a channel leave `[0, 1]`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VehicleFeedbackMap {
    /// Steering torque below which the low channel is 0, newton-meters (≥ 0).
    pub torque_deadband: f32,
    /// Steering torque at which the low channel is 1, newton-meters (above the dead band).
    pub torque_full_scale: f32,
    /// Load change per step, as a share of the static wheel load, below which nothing jolts (≥ 0).
    pub jolt_threshold: f32,
    /// Load change share at which the high channel is 1 (above the threshold).
    pub jolt_full_scale: f32,
    /// How fast a jolt fades, seconds (> 0): the high channel falls by `e` in this time.
    pub jolt_decay: f32,
    /// Scale of the requested strength, in `[0, 1]`.
    pub strength_scale: f32,
    /// How long each request lasts, seconds (> 0): a little longer than a fixed step, so the
    /// rumble is continuous while the feel lasts and stops soon after.
    pub request_duration: f32,
}

impl Default for VehicleFeedbackMap {
    /// A road car: 5 N·m dead band, full at 150 N·m (two front tires near their peak with a 3 to
    /// 4 cm trail); jolts from a quarter of the static load per step, full at 1.5; 0.12 s fade;
    /// full strength; 0.1 s requests.
    fn default() -> Self {
        Self {
            torque_deadband: 5.0,
            torque_full_scale: 150.0,
            jolt_threshold: 0.25,
            jolt_full_scale: 1.5,
            jolt_decay: 0.12,
            strength_scale: 1.0,
            request_duration: 0.1,
        }
    }
}

/// `value` if finite, else `fallback`.
fn finite_or(value: f32, fallback: f32) -> f32 {
    if value.is_finite() { value } else { fallback }
}

/// `(value − low) / (high − low)` in `[0, 1]`; 0 for anything not finite.
fn ramp(value: f32, low: f32, high: f32) -> f32 {
    let span = (high - low).max(1.0e-6);
    finite_or((value - low) / span, 0.0).clamp(0.0, 1.0)
}

impl VehicleFeedbackMap {
    /// The low channel for a steering torque (newton-meters), in `[0, 1]`.
    #[must_use]
    pub fn low(&self, steering_torque: f32) -> f32 {
        let deadband = finite_or(self.torque_deadband, 0.0).max(0.0);
        let full = finite_or(self.torque_full_scale, deadband + 1.0).max(deadband + 1.0e-3);
        ramp(steering_torque.abs(), deadband, full)
    }

    /// The high channel's new jolt for a load change share (`|ΔFz| / static load`), in `[0, 1]`.
    #[must_use]
    pub fn jolt(&self, load_change_share: f32) -> f32 {
        let threshold = finite_or(self.jolt_threshold, 0.0).max(0.0);
        let full = finite_or(self.jolt_full_scale, threshold + 1.0).max(threshold + 1.0e-3);
        ramp(load_change_share, threshold, full)
    }

    /// How much of the high channel is left after `dt` seconds, in `[0, 1]`.
    #[must_use]
    pub fn fade(&self, dt: f32) -> f32 {
        let decay = finite_or(self.jolt_decay, 0.12).max(1.0e-3);
        finite_or((-dt.max(0.0) / decay).exp(), 0.0).clamp(0.0, 1.0)
    }
}

/// The feedback of one vehicle: its map, the gamepad it rumbles, and the channels of the last
/// fixed step.
#[derive(Component, Debug, Clone, PartialEq)]
pub struct VehicleFeedback {
    /// The map from tire state to the channels.
    pub map: VehicleFeedbackMap,
    /// The gamepad (backend device id) that rumbles; `None` computes the channels only.
    pub device: Option<u64>,
    /// The low-frequency channel of the last step, in `[0, 1]`.
    pub low: f32,
    /// The high-frequency channel of the last step, in `[0, 1]`.
    pub high: f32,
    /// The strength of the last request, `max(low, high) · strength_scale`, in `[0, 1]`.
    pub strength: f32,
    /// The steering torque of the last step, newton-meters, positive turning the wheels left.
    pub steering_torque: f32,
    /// The steering torque per steered axle of the last step.
    pub axles: Vec<AxleSteeringTorque>,
    /// Requests sent since the component was added.
    pub requests: u64,
    loads: [f32; MAX_WHEELS],
    primed: bool,
    forces: Vec<TireForces>,
}

impl Default for VehicleFeedback {
    fn default() -> Self {
        Self::new(VehicleFeedbackMap::default(), None)
    }
}

impl VehicleFeedback {
    /// Feedback with `map`, rumbling `device` (or computing only, with `None`).
    #[must_use]
    pub fn new(map: VehicleFeedbackMap, device: Option<u64>) -> Self {
        Self {
            map,
            device,
            low: 0.0,
            high: 0.0,
            strength: 0.0,
            steering_torque: 0.0,
            axles: Vec::new(),
            requests: 0,
            loads: [0.0; MAX_WHEELS],
            primed: false,
            forces: Vec::new(),
        }
    }

    /// One fixed step of `controller`'s vehicle, `dt` seconds long: updates the channels and
    /// returns the strength to request (0 for none). The first step after the component is added
    /// only records the loads.
    pub fn step(&mut self, controller: &VehicleController, dt: f32) -> f32 {
        let vehicle = &controller.vehicle;
        let config = vehicle.config();
        vehicle.tire_forces_into(&mut self.forces);
        self.steering_torque = finite_or(total_steering_torque(config, &self.forces), 0.0);
        steering_torques_into(config, &self.forces, &mut self.axles);
        self.low = self.map.low(self.steering_torque);
        let wheels = self.forces.len().clamp(1, MAX_WHEELS) as f32;
        let static_load = (config.chassis_mass * GRAVITY / wheels).max(1.0);
        let mut jolt = 0.0_f32;
        for (last, force) in self.loads.iter_mut().zip(&self.forces) {
            let load = finite_or(force.load, 0.0).max(0.0);
            if self.primed {
                jolt = jolt.max(self.map.jolt((load - *last).abs() / static_load));
            }
            *last = load;
        }
        self.primed = true;
        self.high = (self.high * self.map.fade(dt)).max(jolt).clamp(0.0, 1.0);
        if self.high < 1.0e-3 {
            self.high = 0.0;
        }
        let scale = finite_or(self.map.strength_scale, 1.0).clamp(0.0, 1.0);
        self.strength = (self.low.max(self.high) * scale).clamp(0.0, 1.0);
        self.strength
    }
}

/// Updates every [`VehicleFeedback`] after the vehicles are driven and sends a [`RumbleRequest`]
/// for each one with a device and a strength above 0.
pub fn update_vehicle_feedback(
    fixed: Option<Res<FixedTime>>,
    mut vehicles: Query<(&VehicleController, &mut VehicleFeedback)>,
    mut rumble: MessageWriter<RumbleRequest>,
) {
    let Some(fixed) = fixed else {
        return;
    };
    let _span = tracing::debug_span!("vehicle_feedback").entered();
    let dt = fixed.step().as_secs_f32();
    for (controller, mut feedback) in &mut vehicles {
        let strength = feedback.step(controller, dt);
        let Some(device) = feedback.device else {
            continue;
        };
        if strength > 0.0 {
            let seconds = finite_or(feedback.map.request_duration, 0.1).clamp(1.0e-3, 10.0);
            rumble.write(RumbleRequest {
                device,
                strength,
                duration: std::time::Duration::from_secs_f32(seconds),
            });
            feedback.requests += 1;
        }
    }
}

/// Runs [`update_vehicle_feedback`] in `FixedUpdate` after `drive_vehicles`. Add it with
/// `VehiclePlugin`.
pub struct VehicleFeedbackPlugin;

impl Plugin for VehicleFeedbackPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<RumbleRequest>()
            .add_systems(FixedUpdate, update_vehicle_feedback.after(drive_vehicles));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_map_keeps_both_channels_in_range() {
        let map = VehicleFeedbackMap::default();
        assert_eq!(map.low(0.0), 0.0);
        assert_eq!(map.low(4.9), 0.0);
        assert_eq!(map.low(-1.0e9), 1.0);
        assert!((map.low(77.5) - 0.5).abs() < 1.0e-6);
        assert_eq!(map.low(f32::NAN), 0.0);
        assert_eq!(map.jolt(0.2), 0.0);
        assert_eq!(map.jolt(10.0), 1.0);
        assert_eq!(map.jolt(f32::INFINITY), 0.0);
        assert!((map.fade(0.12) - (-1.0_f32).exp()).abs() < 1.0e-6);
        let broken = VehicleFeedbackMap {
            torque_deadband: f32::NAN,
            torque_full_scale: -3.0,
            jolt_threshold: f32::INFINITY,
            jolt_full_scale: f32::NAN,
            jolt_decay: 0.0,
            strength_scale: 7.0,
            request_duration: f32::NAN,
        };
        for value in [-1.0e9_f32, -1.0, 0.0, 0.5, 3.0, 1.0e9, f32::NAN, f32::INFINITY] {
            for channel in [broken.low(value), broken.jolt(value), broken.fade(value)] {
                assert!((0.0..=1.0).contains(&channel), "{value}: {channel}");
            }
        }
    }
}
