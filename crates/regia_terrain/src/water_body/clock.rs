// SPDX-License-Identifier: MPL-2.0
//! The deterministic water clock: every animated water term is a pure function of the fixed-step
//! tick count, so the same tick renders the same image on any run, at any frame rate, and after
//! any number of hours (the seconds are reduced in `f64` before they reach `f32`).

use regia_app::prelude::*;

/// Lap of the wrapped seconds the shader sees (s). A multiple of every flow cycle the engine uses
/// (1.6 s and the 6.4 s coarse cycle) and of the 2 s standing-wave shimmer, so the wrap is invisible.
pub const WATER_CLOCK_LAP_SECS: f64 = 640.0;

/// Seconds per flow-map cycle: the ripples are advected this long before the layer fades out and
/// restarts (the other layer is half a cycle apart).
pub const FLOW_CYCLE_SECS: f64 = 1.6;

/// The fixed-step tick the water animates by. [`super::WaterPlugin`] advances it once per
/// `FixedUpdate`; a host driving [`super::WaterRenderer`] itself sets it.
#[derive(Resource, Debug, Clone, Copy, PartialEq)]
pub struct WaterClock {
    /// Fixed steps taken.
    pub tick: u64,
    /// Seconds per step.
    pub step_seconds: f64,
    /// `false` freezes the water (the tick is not advanced).
    pub running: bool,
}

impl Default for WaterClock {
    fn default() -> Self {
        Self {
            tick: 0,
            step_seconds: 1.0 / 60.0,
            running: true,
        }
    }
}

impl WaterClock {
    /// Seconds since tick 0, exact in `f64`.
    #[must_use]
    pub fn seconds(&self) -> f64 {
        self.tick as f64 * self.step_seconds
    }

    /// The seconds the shader sees: [`Self::seconds`] modulo [`WATER_CLOCK_LAP_SECS`].
    #[must_use]
    pub fn wrapped_seconds(&self) -> f32 {
        wrap_seconds(self.seconds())
    }

    /// The flow-map phases of this tick ([`flow_phases`]).
    #[must_use]
    pub fn phases(&self) -> FlowPhases {
        flow_phases(self.seconds())
    }
}

/// `seconds` modulo [`WATER_CLOCK_LAP_SECS`], as `f32`. Non-finite or negative input gives 0.
#[must_use]
pub fn wrap_seconds(seconds: f64) -> f32 {
    if !seconds.is_finite() || seconds < 0.0 {
        return 0.0;
    }
    (seconds % WATER_CLOCK_LAP_SECS) as f32
}

/// The two flow-map layers of one instant (Vlachos 2010): each layer's ripples are advected by
/// `flow × phase × cycle` and its weight is a triangle that is zero when the phase restarts.
/// Layer B is half a cycle behind layer A, so the weights always sum to 1.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FlowPhases {
    /// Phase of layer A in 0..1.
    pub a: f32,
    /// Phase of layer B in 0..1.
    pub b: f32,
    /// Weight of layer A (`1 − |2a − 1|`); layer B's is `1 − weight_a`.
    pub weight_a: f32,
    /// Phase of the slow, coarse layer (four times the cycle) in 0..1.
    pub coarse: f32,
    /// Weight of the coarse layer's first copy (its second copy is half a slow cycle behind).
    pub coarse_weight: f32,
}

/// The flow phases at `seconds` (exact: reduced in `f64`).
#[must_use]
pub fn flow_phases(seconds: f64) -> FlowPhases {
    let s = if seconds.is_finite() && seconds >= 0.0 {
        seconds
    } else {
        0.0
    };
    let a = (s / FLOW_CYCLE_SECS).fract();
    let b = (a + 0.5).fract();
    let coarse = (s / (4.0 * FLOW_CYCLE_SECS)).fract();
    FlowPhases {
        a: a as f32,
        b: b as f32,
        weight_a: (1.0 - (2.0 * a - 1.0).abs()) as f32,
        coarse: coarse as f32,
        coarse_weight: (1.0 - (2.0 * coarse - 1.0).abs()) as f32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phases_are_a_pure_function_of_the_tick() {
        let a = WaterClock {
            tick: 12_345,
            ..WaterClock::default()
        };
        let b = WaterClock {
            tick: 12_345,
            ..WaterClock::default()
        };
        assert_eq!(a.phases(), b.phases());
        assert_eq!(a.wrapped_seconds(), b.wrapped_seconds());
        let next = WaterClock { tick: 12_346, ..a };
        assert_ne!(a.phases(), next.phases(), "the water moves every tick");
    }

    #[test]
    fn layer_weights_sum_to_one_and_vanish_at_restart() {
        for i in 0..400 {
            let p = flow_phases(f64::from(i) * 0.013);
            let wb = 1.0 - (2.0 * p.b - 1.0).abs();
            assert!((p.weight_a + wb - 1.0).abs() < 1.0e-5, "{p:?}");
        }
        let restart = flow_phases(FLOW_CYCLE_SECS * 3.0);
        assert!(restart.a < 1.0e-6 && restart.weight_a < 1.0e-5, "{restart:?}");
    }

    /// After 30 days at 60 Hz the phases still step by one tick's worth (f32 seconds would have
    /// been 0.25 s apart by then).
    #[test]
    fn long_sessions_keep_resolution() {
        let tick = 30 * 24 * 3600 * 60;
        let a = WaterClock {
            tick,
            ..WaterClock::default()
        };
        let b = WaterClock { tick: tick + 1, ..a };
        let step = (b.phases().a - a.phases().a).rem_euclid(1.0);
        assert!(
            (step - (1.0 / 60.0 / FLOW_CYCLE_SECS) as f32).abs() < 1.0e-4,
            "{step}"
        );
        assert!(a.wrapped_seconds() < WATER_CLOCK_LAP_SECS as f32);
        assert!((WATER_CLOCK_LAP_SECS / FLOW_CYCLE_SECS).fract().abs() < 1.0e-9);
        assert!((WATER_CLOCK_LAP_SECS / (4.0 * FLOW_CYCLE_SECS)).fract().abs() < 1.0e-9);
    }

    #[test]
    fn bad_seconds_wrap_to_zero() {
        assert_eq!(wrap_seconds(f64::NAN), 0.0);
        assert_eq!(wrap_seconds(-3.0), 0.0);
        assert_eq!(flow_phases(f64::INFINITY), flow_phases(0.0));
    }
}
