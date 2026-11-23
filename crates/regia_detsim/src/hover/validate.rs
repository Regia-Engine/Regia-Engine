// SPDX-License-Identifier: MPL-2.0
//! Valid ranges of the hover tuning numbers, [`HoverParams::validate`] and [`HoverParams::sanitized`]
//! (and the same for [`ClimbTuning`] and [`FlightRules`]).
//!
//! Hull files are user content (mods, map files): a host validates a hull when it loads it and steps
//! `sanitized()` numbers. The step itself never validates (it runs every tick and must stay
//! bit-identical for valid hulls); its own guards against a bad `dt`, input or floor are in
//! [`super::try_step_tuned`].
//!
//! The ranges are generous (a hull far outside any sensible tuning still passes) but bounded: the
//! upper bounds of the spring and damper numbers keep the semi-implicit spring stable at tick rates
//! of 30 Hz and above, so a sanitized hull's state stays finite for every input (book page
//! `det-hover.md`, "Bad input").

use core::fmt;

use super::{ClimbTuning, FlightRules, HoverParams};

/// One tunable `f32` field of `T` and its valid range: the table that `validate`, `sanitized` and
/// the tests read, so the three never disagree.
#[derive(Debug, Clone, Copy)]
pub struct FieldRange<T> {
    /// The field's name (`"flight.ride_max_m"` for a nested field), as errors report it.
    pub name: &'static str,
    /// Smallest valid value. Fields with `min < 0` may be negative (lean gains).
    pub min: f32,
    /// Largest valid value.
    pub max: f32,
    /// A rate or time constant the step divides by or needs above 0: 0 is `zero_rate`, and `min` is
    /// the smallest positive value `sanitized` puts in.
    pub rate: bool,
    /// Read the field.
    pub get: fn(&T) -> f32,
    /// Write the field.
    pub set: fn(&mut T, f32),
}

/// Why a tuning number is invalid. [`HoverParamsErrorKind::code`] is the protocol code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HoverParamsErrorKind {
    /// NaN or infinite (`not_finite`).
    NotFinite,
    /// Below 0 where the field's range starts at 0 (`negative`).
    Negative,
    /// 0 for a rate or time constant that must be above 0 (`zero_rate`).
    ZeroRate,
    /// Outside the field's documented range (`out_of_range`).
    OutOfRange,
    /// Two fields that form a range are the wrong way round (`inverted_range`): a `yaw_deadband`
    /// above `yaw_rate_at_rest` leaves full lock at full speed with no turn at all.
    InvertedRange,
}

impl HoverParamsErrorKind {
    /// Every kind, in declaration order.
    pub const ALL: [HoverParamsErrorKind; 5] = [
        HoverParamsErrorKind::NotFinite,
        HoverParamsErrorKind::Negative,
        HoverParamsErrorKind::ZeroRate,
        HoverParamsErrorKind::OutOfRange,
        HoverParamsErrorKind::InvertedRange,
    ];

    /// The error code: lowercase ascii, documented in `book/agents/det-hover.md`.
    pub const fn code(self) -> &'static str {
        match self {
            HoverParamsErrorKind::NotFinite => "not_finite",
            HoverParamsErrorKind::Negative => "negative",
            HoverParamsErrorKind::ZeroRate => "zero_rate",
            HoverParamsErrorKind::OutOfRange => "out_of_range",
            HoverParamsErrorKind::InvertedRange => "inverted_range",
        }
    }
}

/// The first invalid field [`HoverParams::validate`] (or [`ClimbTuning::validate`],
/// [`FlightRules::validate`]) found, in field-table order.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HoverParamsError {
    /// The field's name, as in the field table (`"flight.ride_max_m"` for a nested field).
    pub field: &'static str,
    /// What is wrong with it.
    pub kind: HoverParamsErrorKind,
    /// The value it holds.
    pub value: f32,
    /// The valid range, `[min, max]`.
    pub range: [f32; 2],
}

impl fmt::Display for HoverParamsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [min, max] = self.range;
        match self.kind {
            HoverParamsErrorKind::InvertedRange => write!(
                f,
                "{}: hover field `{}` = {} is above the field that bounds it ({max})",
                self.kind.code(),
                self.field,
                self.value
            ),
            _ => write!(
                f,
                "{}: hover field `{}` = {} (valid {min} to {max})",
                self.kind.code(),
                self.field,
                self.value
            ),
        }
    }
}

impl std::error::Error for HoverParamsError {}

/// One field's check.
fn check<T>(field: &FieldRange<T>, value: f32) -> Result<(), HoverParamsError> {
    let kind = if !value.is_finite() {
        Some(HoverParamsErrorKind::NotFinite)
    } else if field.rate && value == 0.0 {
        Some(HoverParamsErrorKind::ZeroRate)
    } else if value < 0.0 && field.min >= 0.0 {
        Some(HoverParamsErrorKind::Negative)
    } else if value < field.min || value > field.max {
        Some(HoverParamsErrorKind::OutOfRange)
    } else {
        None
    };
    match kind {
        Some(kind) => Err(HoverParamsError {
            field: field.name,
            kind,
            value,
            range: [field.min, field.max],
        }),
        None => Ok(()),
    }
}

/// Every field of `value` against `table`, in table order.
pub(super) fn check_all<T>(table: &[FieldRange<T>], value: &T) -> Result<(), HoverParamsError> {
    table
        .iter()
        .try_for_each(|field| check(field, (field.get)(value)))
}

/// `value` with every field brought into range: a non-finite field takes `fallback`'s value, then
/// a value outside `[min, max]` is clamped to the nearer bound. A field already in range keeps its
/// bits (a `-0` stays `-0`), so a valid value sanitizes to itself.
pub(super) fn sanitize_all<T: Copy>(table: &[FieldRange<T>], value: &T, fallback: &T) -> T {
    let mut out = *value;
    for field in table {
        let v = (field.get)(value);
        let v = if v.is_finite() { v } else { (field.get)(fallback) };
        let v = if v < field.min {
            field.min
        } else if v > field.max {
            field.max
        } else {
            v
        };
        (field.set)(&mut out, v);
    }
    out
}

/// A table row: `field_range!(Type, "name", path.to.field, min, max)`, with `rate` for a rate field.
macro_rules! field_range {
    ($t:ty, $name:literal, $($path:ident).+, $min:expr, $max:expr) => {
        field_range!($t, $name, $($path).+, $min, $max, false)
    };
    ($t:ty, $name:literal, $($path:ident).+, $min:expr, $max:expr, rate) => {
        field_range!($t, $name, $($path).+, $min, $max, true)
    };
    ($t:ty, $name:literal, $($path:ident).+, $min:expr, $max:expr, $rate:expr) => {
        FieldRange::<$t> {
            name: $name,
            min: $min,
            max: $max,
            rate: $rate,
            get: |p: &$t| p.$($path).+,
            set: |p: &mut $t, v: f32| p.$($path).+ = v,
        }
    };
}

/// The smallest value `sanitized` gives a rate or time constant (and the smallest valid one).
pub(super) const RATE_MIN: f32 = 1.0e-3;

impl HoverParams {
    /// Every `f32` field with its valid range, in declaration order. The ranges are also in each
    /// field's doc comment.
    pub const FIELD_RANGES: &'static [FieldRange<HoverParams>] = &[
        field_range!(HoverParams, "ride_height", ride_height, 0.0, 50.0),
        field_range!(HoverParams, "coast_drag", coast_drag, 0.0, 1000.0),
        field_range!(HoverParams, "thrust_drag", thrust_drag, 0.0, 1000.0),
        field_range!(HoverParams, "height_spring", height_spring, 0.0, 400.0),
        field_range!(HoverParams, "height_damper", height_damper, 0.0, 50.0),
        field_range!(
            HoverParams,
            "lean_pitch_per_throttle",
            lean_pitch_per_throttle,
            -10.0,
            10.0
        ),
        field_range!(
            HoverParams,
            "lean_pitch_per_speed",
            lean_pitch_per_speed,
            -10.0,
            10.0
        ),
        field_range!(
            HoverParams,
            "lean_roll_per_strafe",
            lean_roll_per_strafe,
            -10.0,
            10.0
        ),
        field_range!(
            HoverParams,
            "lean_roll_per_yaw_rate",
            lean_roll_per_yaw_rate,
            -10.0,
            10.0
        ),
        field_range!(HoverParams, "max_forward_speed", max_forward_speed, 0.0, 1000.0),
        field_range!(HoverParams, "max_reverse_speed", max_reverse_speed, 0.0, 1000.0),
        field_range!(HoverParams, "max_strafe_speed", max_strafe_speed, 0.0, 1000.0),
        field_range!(HoverParams, "thrust_accel", thrust_accel, 0.0, 1000.0),
        field_range!(HoverParams, "brake_accel", brake_accel, 0.0, 1000.0),
        field_range!(HoverParams, "yaw_rate_at_rest", yaw_rate_at_rest, 0.0, 50.0),
        field_range!(HoverParams, "yaw_rate_at_speed", yaw_rate_at_speed, 0.0, 50.0),
        field_range!(HoverParams, "yaw_response", yaw_response, 0.0, 1000.0),
        field_range!(HoverParams, "jump_speed", jump_speed, 0.0, 1000.0),
        field_range!(HoverParams, "lateral_drag", lateral_drag, 0.0, 1000.0),
        field_range!(HoverParams, "thrust_taper", thrust_taper, 0.0, 1.0),
        field_range!(
            HoverParams,
            "yaw_release_response",
            yaw_release_response,
            0.0,
            1000.0
        ),
        field_range!(
            HoverParams,
            "yaw_response_at_speed",
            yaw_response_at_speed,
            0.0,
            1000.0
        ),
        field_range!(HoverParams, "yaw_deadband", yaw_deadband, 0.0, 50.0),
        field_range!(HoverParams, "roll_slip", roll_slip, -10.0, 10.0),
        field_range!(HoverParams, "hover_lead", hover_lead, 0.0, 10.0),
        field_range!(HoverParams, "slope_pull", slope_pull, 0.0, 4.0),
        field_range!(HoverParams, "accel_drag_high", accel_drag_high, 0.0, 1000.0),
        field_range!(HoverParams, "drag_knee", drag_knee, 0.0, 1000.0),
        field_range!(HoverParams, "turbo_scale", turbo_scale, 0.0, 100.0),
        field_range!(HoverParams, "lat_grip", lat_grip, 0.0, 1000.0),
        field_range!(HoverParams, "turn_bleed", turn_bleed, 0.0, 100.0),
        field_range!(HoverParams, "strafe_thrust_share", strafe_thrust_share, 0.0, 10.0),
        field_range!(
            HoverParams,
            "reverse_thrust_share",
            reverse_thrust_share,
            0.0,
            10.0
        ),
        field_range!(HoverParams, "coast_drop", coast_drop, 0.0, 1000.0),
        field_range!(HoverParams, "coast_drop_share", coast_drop_share, 0.0, 10.0),
        field_range!(HoverParams, "roll_bank", roll_bank, -10.0, 10.0),
        field_range!(
            HoverParams,
            "coast_drop_tau",
            coast_drop_tau,
            RATE_MIN,
            100.0,
            rate
        ),
        field_range!(
            HoverParams,
            "roll_spring_omega",
            roll_spring_omega,
            RATE_MIN,
            20.0,
            rate
        ),
        field_range!(HoverParams, "roll_spring_zeta", roll_spring_zeta, 0.0, 1.25),
        field_range!(
            HoverParams,
            "cushion_compress_damp_share",
            cushion_compress_damp_share,
            0.0,
            1.0
        ),
        field_range!(
            HoverParams,
            "cushion_terrain_share",
            cushion_terrain_share,
            0.0,
            1.0
        ),
        field_range!(HoverParams, "cushion_pitch_omega", cushion_pitch_omega, 0.0, 20.0),
        field_range!(HoverParams, "cushion_pitch_zeta", cushion_pitch_zeta, 0.0, 1.25),
        field_range!(
            HoverParams,
            "cushion_reach_full_m",
            cushion_reach_full_m,
            0.0,
            100.0
        ),
        field_range!(
            HoverParams,
            "cushion_reach_band_m",
            cushion_reach_band_m,
            RATE_MIN,
            100.0,
            rate
        ),
    ];

    /// Byte length of [`HoverParams::to_le_bytes`]: four bytes for each [`HoverParams::FIELD_RANGES`] field.
    pub const BYTES: usize = Self::FIELD_RANGES.len() * 4;

    /// The hull as little-endian raw `f32` bits, one word per [`HoverParams::FIELD_RANGES`] entry, in
    /// that table's order.
    ///
    /// These are the `params` part (the first 180 bytes) of the hull's lockstep handshake,
    /// [`hull_handshake_bytes`](super::hull_handshake_bytes), which adds the flight mode and the
    /// limits. Compare that, not these bytes alone: a peer with the same params and other limits or
    /// another flight mode steps differently. Never compare the text of a hull file: text is parsed
    /// on the host, two hosts can turn one float token into different bits, and raw bits cannot.
    /// `-0.0` stays `-0.0`. The layout is fixed; a change to it is a sim-version change (ADR-8019).
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::BYTES] {
        let mut out = [0_u8; Self::BYTES];
        for (i, field) in Self::FIELD_RANGES.iter().enumerate() {
            let at = i * 4;
            out[at..at + 4].copy_from_slice(&(field.get)(self).to_bits().to_le_bytes());
        }
        out
    }

    /// `Ok` when every field is finite and inside its documented range ([`HoverParams::FIELD_RANGES`])
    /// and `yaw_deadband` is at most `yaw_rate_at_rest`; otherwise the first bad field, in
    /// declaration order. Call it when a hull is loaded (a mod, a map file, a settings message),
    /// never per tick. [`HoverParams::DEFAULT_HULL`] is valid.
    pub fn validate(&self) -> Result<(), HoverParamsError> {
        check_all(Self::FIELD_RANGES, self)?;
        if self.yaw_deadband > self.yaw_rate_at_rest {
            return Err(HoverParamsError {
                field: "yaw_deadband",
                kind: HoverParamsErrorKind::InvertedRange,
                value: self.yaw_deadband,
                range: [0.0, self.yaw_rate_at_rest],
            });
        }
        Ok(())
    }

    /// The hull with every field made valid: a non-finite field takes the
    /// [`HoverParams::DEFAULT_HULL`] value, a value outside its range is clamped to the nearer bound
    /// (a zero or negative rate to its smallest valid value), and `yaw_deadband` is clamped to at most
    /// `yaw_rate_at_rest`. A valid hull comes back bit for bit, so stepping `sanitized()` numbers
    /// never moves a valid hull's run. The result always passes [`HoverParams::validate`].
    pub fn sanitized(&self) -> HoverParams {
        let mut out = sanitize_all(Self::FIELD_RANGES, self, &HoverParams::DEFAULT_HULL);
        if out.yaw_deadband > out.yaw_rate_at_rest {
            out.yaw_deadband = out.yaw_rate_at_rest;
        }
        out
    }
}

impl ClimbTuning {
    /// Every `f32` field with its valid range, the flight rules' fields included
    /// (`"flight.ride_max_m"`, ...), in declaration order.
    pub const FIELD_RANGES: &'static [FieldRange<ClimbTuning>] = &[
        field_range!(ClimbTuning, "slope_free_deg", slope_free_deg, 0.0, 90.0),
        field_range!(ClimbTuning, "slope_band_deg", slope_band_deg, 0.0, 90.0),
        field_range!(ClimbTuning, "fan_free_m", fan_free_m, 0.0, 100.0),
        field_range!(ClimbTuning, "fan_band_m", fan_band_m, 0.0, 100.0),
        field_range!(ClimbTuning, "air_decel", air_decel, 0.0, 1000.0),
        field_range!(ClimbTuning, "lead_climb_max", lead_climb_max, 0.0, 1000.0),
        field_range!(ClimbTuning, "lead_max_rise", lead_max_rise, 0.0, 100.0),
        field_range!(ClimbTuning, "flight.ride_max_m", flight.ride_max_m, 0.0, 100.0),
        field_range!(
            ClimbTuning,
            "flight.crest_down_force",
            flight.crest_down_force,
            0.0,
            10.0
        ),
        field_range!(
            ClimbTuning,
            "flight.lift_accel_max",
            flight.lift_accel_max,
            0.0,
            1000.0
        ),
    ];

    /// `Ok` when every field ([`ClimbTuning::FIELD_RANGES`]) is finite and in range; otherwise the
    /// first bad field. [`ClimbTuning::AIR_TIME`] and [`ClimbTuning::CLAMPED_1014`] are valid.
    pub fn validate(&self) -> Result<(), HoverParamsError> {
        check_all(Self::FIELD_RANGES, self)
    }

    /// The tuning with every field made valid (non-finite: the [`ClimbTuning::AIR_TIME`] value; out
    /// of range: the nearer bound). Valid tuning comes back bit for bit.
    pub fn sanitized(&self) -> ClimbTuning {
        sanitize_all(Self::FIELD_RANGES, self, &ClimbTuning::AIR_TIME)
    }
}

impl FlightRules {
    /// Every `f32` field with its valid range, in declaration order.
    pub const FIELD_RANGES: &'static [FieldRange<FlightRules>] = &[
        field_range!(FlightRules, "ride_max_m", ride_max_m, 0.0, 100.0),
        field_range!(FlightRules, "crest_down_force", crest_down_force, 0.0, 10.0),
        field_range!(FlightRules, "lift_accel_max", lift_accel_max, 0.0, 1000.0),
    ];

    /// `Ok` when every field ([`FlightRules::FIELD_RANGES`]) is finite and in range; otherwise the
    /// first bad field. [`FlightRules::AIR_TIME`] and [`FlightRules::CLAMPED_1014`] are valid.
    pub fn validate(&self) -> Result<(), HoverParamsError> {
        check_all(Self::FIELD_RANGES, self)
    }

    /// The rules with every field made valid (non-finite: the [`FlightRules::AIR_TIME`] value; out
    /// of range: the nearer bound). Valid rules come back bit for bit.
    pub fn sanitized(&self) -> FlightRules {
        sanitize_all(Self::FIELD_RANGES, self, &FlightRules::AIR_TIME)
    }
}
