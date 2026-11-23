// SPDX-License-Identifier: MPL-2.0
// Source: strip-theory flight model built from textbook aerodynamics, derived in ADR-3374: finite-wing
// lift slope 2·pi·AR/(AR + 2) (lifting-line result for an elliptic load with a 2·pi section slope),
// induced drag CL²/(pi·e·AR), thin-airfoil flap effectiveness and flap pitching moment (Glauert
// integrals, worked in the ADR), a flat-plate normal-force model past the stall, the ISA troposphere
// and isothermal lower stratosphere (barometric formula), constant-power propeller thrust and rigid-body
// point velocity v + w x r. The blend across the stall and the gear's velocity-capped friction are this
// crate's own design; no third-party code.

//! Fixed-wing aircraft: aerodynamic surfaces, thrust and landing gear on a dynamic body (ADR-3374).
//!
//! An [`Aircraft`] is one dynamic body (a box fuselage collider with explicit mass, center of mass
//! and inertia) plus:
//!
//! - **Surfaces** ([`AeroSurface`]): wing panels, tailplanes and fins. Each one is a flat lifting
//!   panel at its aerodynamic center (quarter chord) with a span, a chord, a normal (the lift
//!   direction at zero angle of attack) and an incidence. Every fixed step the air-relative velocity
//!   at the panel (`v + w × r − wind`) gives its angle of attack; the lift, drag and pitching-moment
//!   coefficients come from [`AeroSurface::coefficients`] and the forces act at the panel.
//! - **Controls**: each surface lists [`ControlLink`]s that turn an input channel (pitch, roll, yaw,
//!   flaps) into a trailing-edge deflection, which shifts the panel's lift (an effective angle of
//!   attack) and its pitching moment.
//! - **Engines** ([`EngineConfig`]): a thrust along an axis at a point, scaled by throttle and air
//!   density, with a speed lapse ([`ThrustModel`]).
//! - **Landing gear** ([`GearConfig`]): one spring-damper ray per leg down the body's −Y, with
//!   rolling friction, wheel brakes, side grip and nose-wheel steering.
//!
//! Air density follows the ISA ([`isa_density`]) at the center of mass's height above
//! [`AeroEnvironment::sea_level_y`]; [`AeroEnvironment::wind`] is the air's velocity.
//!
//! Axes are the engine's (`docs/02-architecture.md` § 3): forward −Z, right +X, up +Y. Positive
//! [`AircraftInput::pitch`] raises the nose, positive [`AircraftInput::roll`] lowers the right
//! wing (a right roll), positive [`AircraftInput::yaw`] turns the nose right.
//!
//! Limits (ADR-3374): no downwash or wing-tail interference, no ground effect, no propeller wash,
//! torque or gyroscopic effects, no compressibility, no unsteady aerodynamics (stall has no
//! hysteresis), spanwise flow is ignored per panel, and the stall model is a blend to a flat plate.
//!
//! [`trim`] solves level flight and glides offline; [`asset`] is the `.aircraft.ron` format. The ECS
//! side is [`AircraftController`], [`AircraftInput`] and [`drive_aircraft`]; the framework's
//! `AircraftPlugin` schedules it.

use rapier3d::dynamics::{MassProperties, RigidBodyBuilder};
use rapier3d::geometry::{ColliderBuilder, Ray};
use rapier3d::pipeline::QueryFilter;
use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_scene::Transform;
use regia_scene::layers::LayerParked;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::debug_span;

use crate::components::{ColliderHandle, RigidBodyHandle};
use crate::convert::{from_rapier_pose, from_rapier_vec3, to_rapier_pose, to_rapier_vec3};
use crate::layers::PhysicsLayers;
use crate::vehicle::{checked_pose, ray_groups};
use crate::world::PhysicsWorld;

pub mod asset;
pub mod trim;

/// The most aerodynamic surfaces one aircraft may have.
pub const MAX_SURFACES: usize = 16;
/// The most engines one aircraft may have.
pub const MAX_ENGINES: usize = 8;
/// The most landing-gear legs one aircraft may have.
pub const MAX_GEAR: usize = 8;
/// The largest control deflection a link may ask for, degrees.
pub const MAX_DEFLECTION_DEG: f32 = 45.0;

/// ISA sea-level temperature, kelvin.
pub const ISA_SEA_LEVEL_TEMPERATURE: f32 = 288.15;
/// ISA sea-level pressure, pascals.
pub const ISA_SEA_LEVEL_PRESSURE: f32 = 101_325.0;
/// ISA sea-level density, kg/m³.
pub const ISA_SEA_LEVEL_DENSITY: f32 = 1.225;
/// ISA troposphere temperature lapse rate, K/m.
pub const ISA_LAPSE_RATE: f32 = 0.0065;
/// ISA tropopause altitude, meters; above it the model is isothermal.
pub const ISA_TROPOPAUSE: f32 = 11_000.0;
/// Highest altitude the density model reads, meters (the isothermal layer's top); higher reads this.
pub const ISA_CEILING: f32 = 20_000.0;
/// Lowest altitude the density model reads, meters; lower reads this.
pub const ISA_FLOOR: f32 = -2_000.0;
/// Specific gas constant of dry air, J/(kg·K).
pub const ISA_GAS_CONSTANT: f32 = 287.052_87;
/// Standard gravity, m/s².
pub const ISA_G0: f32 = 9.806_65;
/// ISA speed of sound at sea level, m/s (the jet lapse's reference speed).
pub const ISA_SEA_LEVEL_SPEED_OF_SOUND: f32 = 340.294;

/// Below this air-relative speed (m/s) a surface, the parasite drag and the air data read as still air.
const STILL_AIR: f32 = 1.0e-3;
/// Below this air speed at a wing panel (m/s) its stall does not set the aircraft's stall flag: a plane
/// parked or creeping has no meaningful angle of attack.
pub const STALL_MIN_SPEED: f32 = 5.0;
/// Fraction of a wheel's slip speed its friction removes per step (before the friction cap).
const GRIP_RELAX: f32 = 0.5;

/// The body's local forward axis.
const FORWARD: Vec3 = Vec3::NEG_Z;
/// The body's local up axis.
const UP: Vec3 = Vec3::Y;
/// The body's local right axis.
const RIGHT: Vec3 = Vec3::X;

/// Why an aircraft could not be created, updated, read, loaded or trimmed. [`AeroError::code`] gives
/// a stable lowercase identifier.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum AeroError {
    /// The configuration lists no aerodynamic surfaces.
    #[error("aircraft config has no aerodynamic surfaces")]
    NoSurfaces,
    /// More surfaces, engines or gear legs than the limits allow.
    #[error("aircraft config has {count} {part}; the most is {max}")]
    TooManyParts {
        /// `surfaces`, `engines` or `gear`.
        part: &'static str,
        /// How many the configuration listed.
        count: usize,
        /// The limit.
        max: usize,
    },
    /// A field is out of range or not finite.
    #[error("aircraft config: `{field}` is out of range or not finite")]
    InvalidConfig {
        /// The offending field (`surfaces[i].span` style for list items).
        field: String,
    },
    /// A spawn pose or velocity is not finite, or the rotation is degenerate.
    #[error("aircraft pose must be a finite translation, a non-zero finite rotation and a finite velocity")]
    InvalidPose,
    /// The time step is zero, negative or not finite.
    #[error("aircraft time step must be finite and positive")]
    InvalidTimeStep,
    /// The body is gone, not dynamic, or has no mass.
    #[error("the aircraft's body is missing, not dynamic or massless")]
    ChassisMissing,
    /// The app has no `PhysicsWorld` resource.
    #[error("no PhysicsWorld in this app; add regia_physics::PhysicsPlugin")]
    NoPhysicsWorld,
    /// The `.aircraft.ron` text does not parse.
    #[error("aircraft file does not parse: {0}")]
    Parse(String),
    /// The `.aircraft.ron` has another format version.
    #[error("aircraft file has format_version {found}; this engine reads {expected}")]
    Version {
        /// The file's version.
        found: u32,
        /// The version this build reads.
        expected: u32,
    },
    /// A trim target is out of range (speed, altitude or a fixed throttle).
    #[error("trim target: `{field}` is out of range or not finite")]
    TrimInvalidTarget {
        /// The offending target field.
        field: &'static str,
    },
    /// The trim's Newton iteration did not converge.
    #[error("trim did not converge (residual {residual})")]
    TrimFailed {
        /// Normalized residual after the last iteration.
        residual: f32,
    },
    /// The trim converged on controls outside their range (pitch outside ±1 or throttle outside 0..1).
    #[error("trim needs {control} = {value}, outside its range")]
    TrimUnreachable {
        /// `pitch` or `throttle`.
        control: &'static str,
        /// The value the solve needed.
        value: f32,
    },
}

impl AeroError {
    /// A stable identifier: lowercase ASCII, digits and `_`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NoSurfaces => "aircraft_no_surfaces",
            Self::TooManyParts { .. } => "aircraft_too_many_parts",
            Self::InvalidConfig { .. } => "aircraft_invalid_config",
            Self::InvalidPose => "aircraft_invalid_pose",
            Self::InvalidTimeStep => "aircraft_invalid_time_step",
            Self::ChassisMissing => "aircraft_chassis_missing",
            Self::NoPhysicsWorld => "aircraft_no_physics_world",
            Self::Parse(_) => "aircraft_parse",
            Self::Version { .. } => "aircraft_version",
            Self::TrimInvalidTarget { .. } => "aircraft_trim_invalid_target",
            Self::TrimFailed { .. } => "aircraft_trim_failed",
            Self::TrimUnreachable { .. } => "aircraft_trim_unreachable",
        }
    }
}

// ---------------------------------------------------------------------------
// Atmosphere
// ---------------------------------------------------------------------------

/// ISA air density (kg/m³) at a geopotential altitude in meters: the troposphere's linear lapse to
/// 11 km, then the isothermal layer to [`ISA_CEILING`]. Altitudes are clamped to
/// [`ISA_FLOOR`]..=[`ISA_CEILING`]; a non-finite altitude reads as sea level.
#[must_use]
pub fn isa_density(altitude: f32) -> f32 {
    let (pressure, temperature) = isa_pressure_temperature(altitude);
    pressure / (ISA_GAS_CONSTANT * temperature)
}

/// ISA pressure (Pa) and temperature (K) at a geopotential altitude in meters, clamped like
/// [`isa_density`].
#[must_use]
pub fn isa_pressure_temperature(altitude: f32) -> (f32, f32) {
    let h = if altitude.is_finite() {
        f64::from(altitude.clamp(ISA_FLOOR, ISA_CEILING))
    } else {
        0.0
    };
    let t0 = f64::from(ISA_SEA_LEVEL_TEMPERATURE);
    let p0 = f64::from(ISA_SEA_LEVEL_PRESSURE);
    let lapse = f64::from(ISA_LAPSE_RATE);
    let r = f64::from(ISA_GAS_CONSTANT);
    let g = f64::from(ISA_G0);
    let tropopause = f64::from(ISA_TROPOPAUSE);
    let exponent = g / (r * lapse);
    let troposphere = h.min(tropopause);
    let temperature = t0 - lapse * troposphere;
    let mut pressure = p0 * (temperature / t0).powf(exponent);
    if h > tropopause {
        pressure *= (-g * (h - tropopause) / (r * temperature)).exp();
    }
    (pressure as f32, temperature as f32)
}

/// The air an aircraft flies through: the wind and where sea level is. A resource; every aircraft
/// reads it.
#[derive(Resource, Debug, Clone, Copy, PartialEq)]
pub struct AeroEnvironment {
    /// The air's velocity in world space, m/s (a steady wind; gusts are the caller's to vary).
    pub wind: Vec3,
    /// World Y of sea level, meters: altitude for the density is `y − sea_level_y`.
    pub sea_level_y: f32,
}

impl Default for AeroEnvironment {
    /// Still air, sea level at y = 0.
    fn default() -> Self {
        Self {
            wind: Vec3::ZERO,
            sea_level_y: 0.0,
        }
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// What a surface is. Only [`SurfaceKind::Wing`] surfaces set [`AircraftState::stalled`] and give
/// the reference area of the aircraft's lift coefficient.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SurfaceKind {
    /// A main wing panel.
    #[default]
    Wing,
    /// A horizontal tail.
    Tailplane,
    /// A vertical fin.
    Fin,
}

impl SurfaceKind {
    /// Lowercase name for reports.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Wing => "wing",
            Self::Tailplane => "tailplane",
            Self::Fin => "fin",
        }
    }
}

/// An input channel a control surface follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ControlChannel {
    /// [`AircraftInput::pitch`] (−1..1, + nose up).
    #[default]
    Pitch,
    /// [`AircraftInput::roll`] (−1..1, + right wing down).
    Roll,
    /// [`AircraftInput::yaw`] (−1..1, + nose right).
    Yaw,
    /// [`AircraftInput::flaps`] (0..1).
    Flaps,
}

/// One input channel driving a surface's trailing edge.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ControlLink {
    /// The input channel.
    pub channel: ControlChannel,
    /// Deflection at a channel value of +1, degrees, signed: positive moves the trailing edge
    /// against the surface normal (more lift along the normal). At most [`MAX_DEFLECTION_DEG`].
    pub max_deflection_deg: f32,
}

/// One lifting panel. Its lift acts along `normal` at zero angle of attack, its chord points
/// forward (−Z, made perpendicular to `normal`), and the span axis is chord × normal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AeroSurface {
    /// A name for reports.
    pub name: String,
    /// Wing, tailplane or fin.
    pub kind: SurfaceKind,
    /// Aerodynamic center (quarter chord) in body space, meters.
    pub position: Vec3,
    /// Lift direction at zero angle of attack, body space (normalized; must not be parallel to −Z).
    /// `(0, 1, 0)` for a flat wing, `(1, 0, 0)` for a fin, tilted inward for dihedral.
    pub normal: Vec3,
    /// Rotation about the span axis, degrees; positive raises the leading edge.
    pub incidence_deg: f32,
    /// Span of this panel, meters (> 0).
    pub span: f32,
    /// Mean chord, meters (> 0). Area is `span · chord`.
    pub chord: f32,
    /// Aspect ratio for the lift slope and induced drag; `None` reads `span / chord`. A wing split into
    /// two panels gives each panel the whole wing's aspect ratio.
    pub aspect_ratio: Option<f32>,
    /// Lift coefficient at zero angle of attack (camber), −1..=1.
    pub cl0: f32,
    /// Zero-lift drag coefficient, 0..=0.5.
    pub cd0: f32,
    /// Pitching-moment coefficient about the quarter chord, −0.5..=0.5 (negative is nose down).
    pub cm0: f32,
    /// Oswald span efficiency for the induced drag, 0.3..=1.
    pub oswald: f32,
    /// Effective angle of attack where the attached-flow lift peaks, degrees, 2..=40.
    pub stall_angle_deg: f32,
    /// Width of the blend from attached flow to the flat plate past the stall, degrees, 0.5..=30.
    pub stall_blend_deg: f32,
    /// A flat plate's normal-force coefficient at 90°, 0.5..=2.5 (about 1.2 for a finite panel).
    pub flat_plate_cn: f32,
    /// Control-surface chord as a fraction of the chord, 0.05..=0.9.
    pub control_chord_fraction: f32,
    /// Input channels driving the trailing edge (at most 4).
    pub controls: Vec<ControlLink>,
}

impl Default for AeroSurface {
    /// A 10 m × 1.5 m flat wing at the origin with a 15° stall.
    fn default() -> Self {
        Self {
            name: "wing".into(),
            kind: SurfaceKind::Wing,
            position: Vec3::ZERO,
            normal: Vec3::Y,
            incidence_deg: 0.0,
            span: 10.0,
            chord: 1.5,
            aspect_ratio: None,
            cl0: 0.0,
            cd0: 0.008,
            cm0: 0.0,
            oswald: 0.8,
            stall_angle_deg: 15.0,
            stall_blend_deg: 4.0,
            flat_plate_cn: 1.2,
            control_chord_fraction: 0.25,
            controls: Vec::new(),
        }
    }
}

/// The coefficients of one surface at one effective angle of attack.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct SurfaceCoefficients {
    /// Lift coefficient.
    pub cl: f32,
    /// Drag coefficient.
    pub cd: f32,
    /// Pitching-moment coefficient about the quarter chord.
    pub cm: f32,
    /// Effective angle of attack (geometric plus the control's shift), radians, in −π..π.
    pub alpha_effective: f32,
    /// The effective angle of attack is past the stall angle.
    pub stalled: bool,
}

/// Wraps an angle into −π..π.
fn wrap_angle(angle: f32) -> f32 {
    let tau = std::f32::consts::TAU;
    let wrapped = (angle + std::f32::consts::PI).rem_euclid(tau) - std::f32::consts::PI;
    if wrapped.is_finite() { wrapped } else { 0.0 }
}

impl AeroSurface {
    /// Planform area, m².
    #[must_use]
    pub fn area(&self) -> f32 {
        self.span * self.chord
    }

    /// The aspect ratio the lift slope and induced drag use.
    #[must_use]
    pub fn effective_aspect_ratio(&self) -> f32 {
        self.aspect_ratio.unwrap_or(self.span / self.chord)
    }

    /// Attached-flow lift slope per radian: `2π · AR / (AR + 2)`.
    #[must_use]
    pub fn lift_slope(&self) -> f32 {
        let ar = self.effective_aspect_ratio();
        std::f32::consts::TAU * ar / (ar + 2.0)
    }

    /// Thin-airfoil flap effectiveness `τ = 1 − (θf − sin θf)/π`, `θf = acos(2·cf/c − 1)`: the
    /// change in effective angle of attack per radian of deflection.
    #[must_use]
    pub fn flap_effectiveness(&self) -> f32 {
        let theta = (2.0 * self.control_chord_fraction - 1.0).clamp(-1.0, 1.0).acos();
        1.0 - (theta - theta.sin()) / std::f32::consts::PI
    }

    /// Thin-airfoil quarter-chord moment per radian of deflection: `−½ sin θf (1 − cos θf)`.
    #[must_use]
    pub fn flap_moment_slope(&self) -> f32 {
        let theta = (2.0 * self.control_chord_fraction - 1.0).clamp(-1.0, 1.0).acos();
        -0.5 * theta.sin() * (1.0 - theta.cos())
    }

    /// Lift, drag and moment coefficients at geometric angle of attack `alpha` (radians) with the
    /// trailing edge deflected by `deflection` (radians, positive adds lift along the normal).
    ///
    /// Attached flow (|effective α| ≤ stall): `CL = cl0 + a·α_eff`, `CD = cd0 + CL²/(π e AR)`,
    /// `CM = cm0 + cm_δ·δ`, with `α_eff = α + τ·δ`. Past the stall the attached values are held at the
    /// stall angle and blended (smoothstep over `stall_blend_deg`) into a flat plate: normal force
    /// `CN = cn90·sin α_eff`, `CL = CN cos α_eff`, `CD = cd0 + CN sin α_eff`, center of pressure
    /// moving from the quarter chord to mid chord (`CM = −CN · ¼(1 − |cos α_eff|)`). Never NaN for
    /// finite input; a non-finite angle reads as 0.
    #[must_use]
    pub fn coefficients(&self, alpha: f32, deflection: f32) -> SurfaceCoefficients {
        let alpha = if alpha.is_finite() { alpha } else { 0.0 };
        let deflection = if deflection.is_finite() { deflection } else { 0.0 };
        let slope = self.lift_slope();
        let ar = self.effective_aspect_ratio();
        let alpha_eff = wrap_angle(alpha + self.flap_effectiveness() * deflection);
        let stall = self.stall_angle_deg.to_radians();
        let blend = self.stall_blend_deg.to_radians();
        let magnitude = alpha_eff.abs();
        let induced = |cl: f32| cl * cl / (std::f32::consts::PI * self.oswald * ar);
        let cm_attached = self.cm0 + self.flap_moment_slope() * deflection;
        if magnitude <= stall {
            let cl = self.cl0 + slope * alpha_eff;
            return SurfaceCoefficients {
                cl,
                cd: self.cd0 + induced(cl),
                cm: cm_attached,
                alpha_effective: alpha_eff,
                stalled: false,
            };
        }
        let held_cl = self.cl0 + slope * stall * alpha_eff.signum();
        let held_cd = self.cd0 + induced(held_cl);
        let (sin, cos) = alpha_eff.sin_cos();
        let cn = self.flat_plate_cn * sin;
        let plate_cl = cn * cos;
        let plate_cd = self.cd0 + cn * sin;
        let plate_cm = -cn * 0.25 * (1.0 - cos.abs());
        let x = ((magnitude - stall) / blend).clamp(0.0, 1.0);
        let t = x * x * (3.0 - 2.0 * x);
        SurfaceCoefficients {
            cl: held_cl + (plate_cl - held_cl) * t,
            cd: held_cd + (plate_cd - held_cd) * t,
            cm: cm_attached + (plate_cm - cm_attached) * t,
            alpha_effective: alpha_eff,
            stalled: true,
        }
    }

    /// The surface's unit chord (forward), normal and span axes in body space, incidence applied.
    #[must_use]
    pub fn axes(&self) -> (Vec3, Vec3, Vec3) {
        let normal = self.normal.normalize_or_zero();
        let chord = (FORWARD - normal * FORWARD.dot(normal)).normalize_or_zero();
        let span = chord.cross(normal);
        let (sin, cos) = self.incidence_deg.to_radians().sin_cos();
        (chord * cos + normal * sin, normal * cos - chord * sin, span)
    }

    /// The deflection (radians) this surface's links give for `input`, clamped to ±[`MAX_DEFLECTION_DEG`].
    #[must_use]
    pub fn deflection(&self, input: &AircraftInput) -> f32 {
        let degrees: f32 = self
            .controls
            .iter()
            .map(|link| link.max_deflection_deg * input.channel(link.channel))
            .sum();
        degrees
            .clamp(-MAX_DEFLECTION_DEG, MAX_DEFLECTION_DEG)
            .to_radians()
    }

    fn validate(&self, index: usize) -> Result<(), AeroError> {
        let field = |name: &str| AeroError::InvalidConfig {
            field: format!("surfaces[{index}].{name}"),
        };
        let normal = self.normal.normalize_or_zero();
        if !self.position.is_finite() {
            return Err(field("position"));
        }
        if normal == Vec3::ZERO || normal.dot(FORWARD).abs() > 0.99 {
            return Err(field("normal"));
        }
        let range = |v: f32, lo: f32, hi: f32| v.is_finite() && (lo..=hi).contains(&v);
        let checks: [(bool, &str); 12] = [
            (range(self.incidence_deg, -30.0, 30.0), "incidence_deg"),
            (range(self.span, 0.01, 200.0), "span"),
            (range(self.chord, 0.01, 50.0), "chord"),
            (
                self.aspect_ratio.is_none_or(|ar| range(ar, 0.1, 50.0)),
                "aspect_ratio",
            ),
            (range(self.cl0, -1.0, 1.0), "cl0"),
            (range(self.cd0, 0.0, 0.5), "cd0"),
            (range(self.cm0, -0.5, 0.5), "cm0"),
            (range(self.oswald, 0.3, 1.0), "oswald"),
            (range(self.stall_angle_deg, 2.0, 40.0), "stall_angle_deg"),
            (range(self.stall_blend_deg, 0.5, 30.0), "stall_blend_deg"),
            (range(self.flat_plate_cn, 0.5, 2.5), "flat_plate_cn"),
            (
                range(self.control_chord_fraction, 0.05, 0.9),
                "control_chord_fraction",
            ),
        ];
        if let Some((_, name)) = checks.into_iter().find(|(ok, _)| !ok) {
            return Err(field(name));
        }
        if self.controls.len() > 4 {
            return Err(field("controls"));
        }
        if self
            .controls
            .iter()
            .any(|link| !range(link.max_deflection_deg, -MAX_DEFLECTION_DEG, MAX_DEFLECTION_DEG))
        {
            return Err(field("controls.max_deflection_deg"));
        }
        Ok(())
    }
}

/// How an engine's thrust falls with airspeed.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum ThrustModel {
    /// A propeller at constant shaft power: `T = min(static, efficiency · power / V)`.
    Propeller {
        /// Shaft power at full throttle, watts (> 0).
        power: f32,
        /// Propulsive efficiency, 0.1..=1.
        efficiency: f32,
    },
    /// A jet: `T = static · max(0, 1 − speed_lapse · V / a0)`, `a0` the sea-level speed of sound.
    Jet {
        /// Fraction of static thrust lost per Mach of airspeed, 0..=1.
        speed_lapse: f32,
    },
}

impl Default for ThrustModel {
    fn default() -> Self {
        Self::Propeller {
            power: 100_000.0,
            efficiency: 0.8,
        }
    }
}

/// One engine: thrust along `direction` at `position`, scaled by throttle and by density relative to
/// sea level.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EngineConfig {
    /// A name for reports.
    pub name: String,
    /// Where the thrust acts, body space, meters.
    pub position: Vec3,
    /// Thrust direction, body space (normalized; default forward −Z).
    pub direction: Vec3,
    /// Thrust at full throttle, zero airspeed and sea-level density, newtons (> 0).
    pub static_thrust: f32,
    /// The speed lapse.
    pub model: ThrustModel,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            name: "engine".into(),
            position: Vec3::ZERO,
            direction: FORWARD,
            static_thrust: 2000.0,
            model: ThrustModel::default(),
        }
    }
}

impl EngineConfig {
    /// Thrust in newtons at `throttle` (0..1), airspeed along the thrust axis `speed` (m/s) and
    /// density `density` (kg/m³). Never NaN for finite input.
    #[must_use]
    pub fn thrust(&self, throttle: f32, speed: f32, density: f32) -> f32 {
        let speed = speed.max(0.0);
        let available = match self.model {
            ThrustModel::Propeller { power, efficiency } => {
                if speed * self.static_thrust > efficiency * power {
                    efficiency * power / speed
                } else {
                    self.static_thrust
                }
            }
            ThrustModel::Jet { speed_lapse } => {
                self.static_thrust * (1.0 - speed_lapse * speed / ISA_SEA_LEVEL_SPEED_OF_SOUND).max(0.0)
            }
        };
        throttle.clamp(0.0, 1.0) * available * (density / ISA_SEA_LEVEL_DENSITY)
    }

    fn validate(&self, index: usize) -> Result<(), AeroError> {
        let field = |name: &str| AeroError::InvalidConfig {
            field: format!("engines[{index}].{name}"),
        };
        if !self.position.is_finite() {
            return Err(field("position"));
        }
        if !self.direction.is_finite() || self.direction.length_squared() < 1.0e-6 {
            return Err(field("direction"));
        }
        if !(self.static_thrust.is_finite() && self.static_thrust > 0.0 && self.static_thrust <= 1.0e7) {
            return Err(field("static_thrust"));
        }
        match self.model {
            ThrustModel::Propeller { power, efficiency } => {
                if !(power.is_finite() && power > 0.0 && power <= 1.0e9) {
                    return Err(field("model.power"));
                }
                if !(efficiency.is_finite() && (0.1..=1.0).contains(&efficiency)) {
                    return Err(field("model.efficiency"));
                }
            }
            ThrustModel::Jet { speed_lapse } => {
                if !(speed_lapse.is_finite() && (0.0..=1.0).contains(&speed_lapse)) {
                    return Err(field("model.speed_lapse"));
                }
            }
        }
        Ok(())
    }
}

/// One landing-gear leg: a spring-damper ray down the body's −Y from `position`, with a wheel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GearConfig {
    /// A name for reports.
    pub name: String,
    /// Strut mount point, body space, meters.
    pub position: Vec3,
    /// Strut length at zero load, meters (> 0).
    pub rest_length: f32,
    /// Wheel radius, meters (> 0). The ray reaches `rest_length + wheel_radius`.
    pub wheel_radius: f32,
    /// Spring rate, N/m (> 0).
    pub stiffness: f32,
    /// Damping on the contact's speed along the ground normal, N·s/m (≥ 0).
    pub damping: f32,
    /// The wheel turns with [`AircraftInput::yaw`].
    pub steerable: bool,
    /// Steering angle at full yaw input, degrees, 0..=80.
    pub max_steer_deg: f32,
    /// The wheel brakes with [`AircraftInput::brakes`].
    pub brakes: bool,
    /// Rolling-resistance coefficient, 0..=1.
    pub rolling_friction: f32,
    /// Braking friction coefficient at full brake, 0..=2.
    pub brake_friction: f32,
    /// Side-grip friction coefficient, 0..=3.
    pub lateral_friction: f32,
}

impl Default for GearConfig {
    fn default() -> Self {
        Self {
            name: "gear".into(),
            position: Vec3::new(0.0, -0.5, 0.0),
            rest_length: 0.5,
            wheel_radius: 0.3,
            stiffness: 40_000.0,
            damping: 6_000.0,
            steerable: false,
            max_steer_deg: 0.0,
            brakes: true,
            rolling_friction: 0.02,
            brake_friction: 0.6,
            lateral_friction: 0.9,
        }
    }
}

impl GearConfig {
    fn validate(&self, index: usize) -> Result<(), AeroError> {
        let field = |name: &str| AeroError::InvalidConfig {
            field: format!("gear[{index}].{name}"),
        };
        if !self.position.is_finite() {
            return Err(field("position"));
        }
        let range = |v: f32, lo: f32, hi: f32| v.is_finite() && (lo..=hi).contains(&v);
        let checks: [(bool, &str); 8] = [
            (range(self.rest_length, 0.01, 20.0), "rest_length"),
            (range(self.wheel_radius, 0.01, 5.0), "wheel_radius"),
            (range(self.stiffness, 1.0, 1.0e9), "stiffness"),
            (range(self.damping, 0.0, 1.0e8), "damping"),
            (range(self.max_steer_deg, 0.0, 80.0), "max_steer_deg"),
            (range(self.rolling_friction, 0.0, 1.0), "rolling_friction"),
            (range(self.brake_friction, 0.0, 2.0), "brake_friction"),
            (range(self.lateral_friction, 0.0, 3.0), "lateral_friction"),
        ];
        if let Some((_, name)) = checks.into_iter().find(|(ok, _)| !ok) {
            return Err(field(name));
        }
        Ok(())
    }
}

/// A fixed-wing aircraft: body, surfaces, engines and gear.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AircraftConfig {
    /// Mass, kilograms (> 0).
    pub mass: f32,
    /// Center of mass in body space, meters.
    pub center_of_mass: Vec3,
    /// Principal moments of inertia about the body's X (pitch), Y (yaw) and Z (roll) axes through the
    /// center of mass, kg·m² (each > 0).
    pub inertia: Vec3,
    /// Half extents of the fuselage collider box, meters (each > 0).
    pub fuselage_half_extents: Vec3,
    /// Drag area of everything that is not a surface (fuselage, gear, struts), `CD·A` in m² (≥ 0),
    /// acting at the center of mass against the airflow.
    pub parasite_drag_area: f32,
    /// Lifting surfaces, 1 to [`MAX_SURFACES`].
    pub surfaces: Vec<AeroSurface>,
    /// Engines, 0 to [`MAX_ENGINES`] (none for a glider).
    pub engines: Vec<EngineConfig>,
    /// Landing-gear legs, 0 to [`MAX_GEAR`].
    pub gear: Vec<GearConfig>,
    /// Layers the gear rays can hit (`1 << index` bitmask); `None` hits all.
    pub ground_layers: Option<u32>,
    /// The layer the gear rays belong to (a row of the query matrix); `None` for no layer.
    pub query_layer: Option<u16>,
}

impl Default for AircraftConfig {
    /// The sample trainer: a 1000 kg high-wing single-engine plane on tricycle gear.
    fn default() -> Self {
        trainer()
    }
}

/// The reference light aircraft (`samples/flight_lab`'s plane): 1000 kg, an 11 m wing in two panels
/// with 3° dihedral and ailerons, a tailplane with an elevator, a fin with a rudder, one 120 kW
/// propeller engine and tricycle gear with a steerable nose wheel.
#[must_use]
pub fn trainer() -> AircraftConfig {
    let dihedral = 3.0_f32.to_radians();
    let (sin, cos) = dihedral.sin_cos();
    let wing = |name: &str, side: f32| AeroSurface {
        name: name.into(),
        kind: SurfaceKind::Wing,
        position: Vec3::new(side * 2.75, 0.8, 0.15),
        normal: Vec3::new(-side * sin, cos, 0.0),
        incidence_deg: 1.0,
        span: 5.5,
        chord: 1.5,
        aspect_ratio: Some(11.0 / 1.5),
        cl0: 0.25,
        cd0: 0.008,
        cm0: -0.05,
        oswald: 0.8,
        stall_angle_deg: 15.0,
        stall_blend_deg: 4.0,
        flat_plate_cn: 1.2,
        control_chord_fraction: 0.25,
        controls: vec![
            ControlLink {
                channel: ControlChannel::Roll,
                max_deflection_deg: -15.0 * side,
            },
            ControlLink {
                channel: ControlChannel::Flaps,
                max_deflection_deg: 10.0,
            },
        ],
    };
    AircraftConfig {
        mass: 1000.0,
        center_of_mass: Vec3::ZERO,
        inertia: Vec3::new(1800.0, 2700.0, 1300.0),
        fuselage_half_extents: Vec3::new(0.6, 0.6, 2.5),
        parasite_drag_area: 0.45,
        surfaces: vec![
            wing("left wing", -1.0),
            wing("right wing", 1.0),
            AeroSurface {
                name: "tailplane".into(),
                kind: SurfaceKind::Tailplane,
                position: Vec3::new(0.0, 0.6, 4.6),
                span: 3.4,
                chord: 0.9,
                incidence_deg: -1.0,
                control_chord_fraction: 0.35,
                controls: vec![ControlLink {
                    channel: ControlChannel::Pitch,
                    max_deflection_deg: -20.0,
                }],
                ..AeroSurface::default()
            },
            AeroSurface {
                name: "fin".into(),
                kind: SurfaceKind::Fin,
                position: Vec3::new(0.0, 1.2, 4.7),
                normal: Vec3::X,
                span: 1.5,
                chord: 1.1,
                control_chord_fraction: 0.4,
                controls: vec![ControlLink {
                    channel: ControlChannel::Yaw,
                    max_deflection_deg: -25.0,
                }],
                ..AeroSurface::default()
            },
        ],
        engines: vec![EngineConfig {
            name: "propeller".into(),
            position: Vec3::new(0.0, 0.0, -2.6),
            direction: FORWARD,
            static_thrust: 2600.0,
            model: ThrustModel::Propeller {
                power: 120_000.0,
                efficiency: 0.8,
            },
        }],
        gear: vec![
            GearConfig {
                name: "nose".into(),
                position: Vec3::new(0.0, -0.5, -1.8),
                stiffness: 20_000.0,
                damping: 3_000.0,
                steerable: true,
                max_steer_deg: 25.0,
                brakes: false,
                ..GearConfig::default()
            },
            GearConfig {
                name: "left main".into(),
                position: Vec3::new(-1.2, -0.5, 0.3),
                ..GearConfig::default()
            },
            GearConfig {
                name: "right main".into(),
                position: Vec3::new(1.2, -0.5, 0.3),
                ..GearConfig::default()
            },
        ],
        ground_layers: None,
        query_layer: None,
    }
}

impl AircraftConfig {
    /// Checks every field; the first problem is reported.
    ///
    /// # Errors
    /// An [`AeroError`] naming the field, or a part count.
    pub fn validate(&self) -> Result<(), AeroError> {
        let field = |name: &str| AeroError::InvalidConfig { field: name.into() };
        if !(self.mass.is_finite() && self.mass > 0.0 && self.mass <= 1.0e7) {
            return Err(field("mass"));
        }
        if !self.center_of_mass.is_finite() {
            return Err(field("center_of_mass"));
        }
        if !(self.inertia.is_finite() && self.inertia.min_element() > 0.0) {
            return Err(field("inertia"));
        }
        let half = self.fuselage_half_extents;
        if !(half.is_finite() && half.min_element() > 0.0) {
            return Err(field("fuselage_half_extents"));
        }
        if !(self.parasite_drag_area.is_finite() && (0.0..=1000.0).contains(&self.parasite_drag_area)) {
            return Err(field("parasite_drag_area"));
        }
        if self
            .query_layer
            .is_some_and(|layer| usize::from(layer) >= crate::layers::MAX_LAYERS)
        {
            return Err(field("query_layer"));
        }
        if self.surfaces.is_empty() {
            return Err(AeroError::NoSurfaces);
        }
        for (part, count, max) in [
            ("surfaces", self.surfaces.len(), MAX_SURFACES),
            ("engines", self.engines.len(), MAX_ENGINES),
            ("gear", self.gear.len(), MAX_GEAR),
        ] {
            if count > max {
                return Err(AeroError::TooManyParts { part, count, max });
            }
        }
        for (index, surface) in self.surfaces.iter().enumerate() {
            surface.validate(index)?;
        }
        for (index, engine) in self.engines.iter().enumerate() {
            engine.validate(index)?;
        }
        for (index, gear) in self.gear.iter().enumerate() {
            gear.validate(index)?;
        }
        Ok(())
    }

    /// Reference area of the lift and drag coefficients: the wing surfaces' area, or every surface's
    /// when none is a wing, m².
    #[must_use]
    pub fn reference_area(&self) -> f32 {
        let wings: f32 = self
            .surfaces
            .iter()
            .filter(|s| s.kind == SurfaceKind::Wing)
            .map(AeroSurface::area)
            .sum();
        if wings > 0.0 {
            wings
        } else {
            self.surfaces.iter().map(AeroSurface::area).sum()
        }
    }
}

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

/// Pilot input for one step. Also a component: [`drive_aircraft`] reads it from the aircraft's
/// entity every fixed step.
#[derive(Component, Reflect, Debug, Clone, Copy, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct AircraftInput {
    /// Stick fore-aft: +1 full nose up (pull), −1 full nose down.
    pub pitch: f32,
    /// Stick sideways: +1 full right roll (right wing down), −1 full left.
    pub roll: f32,
    /// Pedals: +1 full nose right (also the nose wheel), −1 full left.
    pub yaw: f32,
    /// Throttle: 0 idle to 1 full.
    pub throttle: f32,
    /// Flaps: 0 up to 1 fully down.
    pub flaps: f32,
    /// Wheel brakes: 0 off to 1 full.
    pub brakes: f32,
}

impl AircraftInput {
    /// The input clamped to its ranges, with non-finite values read as 0.
    #[must_use]
    pub fn sanitized(self) -> Self {
        let clean = |value: f32, lo: f32, hi: f32| {
            if value.is_finite() {
                value.clamp(lo, hi)
            } else {
                0.0
            }
        };
        Self {
            pitch: clean(self.pitch, -1.0, 1.0),
            roll: clean(self.roll, -1.0, 1.0),
            yaw: clean(self.yaw, -1.0, 1.0),
            throttle: clean(self.throttle, 0.0, 1.0),
            flaps: clean(self.flaps, 0.0, 1.0),
            brakes: clean(self.brakes, 0.0, 1.0),
        }
    }

    /// The value of one channel.
    #[must_use]
    pub fn channel(&self, channel: ControlChannel) -> f32 {
        match channel {
            ControlChannel::Pitch => self.pitch,
            ControlChannel::Roll => self.roll,
            ControlChannel::Yaw => self.yaw,
            ControlChannel::Flaps => self.flaps,
        }
    }
}

// ---------------------------------------------------------------------------
// Loads
// ---------------------------------------------------------------------------

/// Pose and velocity of the center of mass, world space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Kinematics {
    /// Center of mass, world meters.
    pub center_of_mass: Vec3,
    /// Body rotation.
    pub rotation: Quat,
    /// Velocity of the center of mass, m/s.
    pub linear_velocity: Vec3,
    /// Angular velocity, world rad/s.
    pub angular_velocity: Vec3,
}

/// Air data at the center of mass.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct AirData {
    /// Air-relative speed, m/s.
    pub airspeed: f32,
    /// Angle of attack, radians (+ when the air comes from below the nose).
    pub alpha: f32,
    /// Sideslip, radians (+ when the air comes from the right).
    pub beta: f32,
    /// Height above sea level, meters.
    pub altitude: f32,
    /// Air density, kg/m³.
    pub density: f32,
    /// Dynamic pressure `½ ρ V²`, pascals.
    pub dynamic_pressure: f32,
}

/// One surface after the last evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct SurfaceState {
    /// Geometric angle of attack at the panel, radians.
    pub alpha: f32,
    /// Control deflection, radians.
    pub deflection: f32,
    /// Coefficients at the panel.
    pub coefficients: SurfaceCoefficients,
    /// Dynamic pressure at the panel, pascals.
    pub dynamic_pressure: f32,
    /// Lift, newtons (signed along the lift direction).
    pub lift: f32,
    /// Drag, newtons.
    pub drag: f32,
    /// Total force on the panel, world newtons.
    pub force: Vec3,
}

/// The summed loads of one evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Loads {
    /// Total aerodynamic force (surfaces and parasite drag), world newtons.
    pub aero_force: Vec3,
    /// Total thrust, world newtons.
    pub thrust_force: Vec3,
    /// Total torque about the center of mass (aerodynamic and thrust), world N·m.
    pub torque: Vec3,
    /// Air data at the center of mass.
    pub air: AirData,
    /// Aircraft lift coefficient (aero force across the airflow in the symmetry plane over `q·S_ref`).
    pub cl: f32,
    /// Aircraft drag coefficient (aero force against the airflow over `q·S_ref`).
    pub cd: f32,
    /// Some wing surface is past its stall angle.
    pub stalled: bool,
}

/// Air data from the center of mass's air-relative velocity.
fn air_data(kin: &Kinematics, env: &AeroEnvironment) -> AirData {
    let altitude = kin.center_of_mass.y - env.sea_level_y;
    let density = isa_density(altitude);
    let air = kin.linear_velocity - env.wind;
    let airspeed = air.length();
    let mut data = AirData {
        airspeed,
        alpha: 0.0,
        beta: 0.0,
        altitude,
        density,
        dynamic_pressure: 0.5 * density * airspeed * airspeed,
    };
    if airspeed > STILL_AIR && airspeed.is_finite() {
        let body = kin.rotation.inverse() * air;
        // Forward speed u along −Z, downward-relative speed w along −Y (air from below = +α).
        data.alpha = (-body.y).atan2(-body.z);
        data.beta = (body.x / airspeed).clamp(-1.0, 1.0).asin();
    }
    data
}

/// Evaluates every surface, engine and the parasite drag at `kin` without touching a world. Writes
/// one [`SurfaceState`] per surface into `surfaces` (which must hold `config.surfaces.len()`; extra
/// entries are left alone) and one thrust (newtons) per engine into `thrusts`. Allocation-free.
#[must_use]
pub fn evaluate_loads(
    config: &AircraftConfig,
    kin: &Kinematics,
    input: &AircraftInput,
    env: &AeroEnvironment,
    surfaces: &mut [SurfaceState],
    thrusts: &mut [f32],
) -> Loads {
    let air = air_data(kin, env);
    let rotation = kin.rotation;
    let com_local = config.center_of_mass;
    let mut aero_force = Vec3::ZERO;
    let mut thrust_force = Vec3::ZERO;
    let mut torque = Vec3::ZERO;
    let mut stalled = false;
    for (surface, out) in config.surfaces.iter().zip(surfaces.iter_mut()) {
        let (chord, normal, span) = surface.axes();
        let (chord, normal, span) = (rotation * chord, rotation * normal, rotation * span);
        let arm = rotation * (surface.position - com_local);
        let velocity = kin.linear_velocity + kin.angular_velocity.cross(arm) - env.wind;
        let vc = velocity.dot(chord);
        let vn = velocity.dot(normal);
        let speed_sq = vc * vc + vn * vn;
        let deflection = surface.deflection(input);
        if !(speed_sq > STILL_AIR * STILL_AIR && speed_sq.is_finite()) {
            *out = SurfaceState {
                deflection,
                coefficients: surface.coefficients(0.0, deflection),
                ..SurfaceState::default()
            };
            continue;
        }
        let speed = speed_sq.sqrt();
        let alpha = (-vn).atan2(vc);
        let coefficients = surface.coefficients(alpha, deflection);
        stalled |= coefficients.stalled
            && surface.kind == SurfaceKind::Wing
            && speed_sq > STALL_MIN_SPEED * STALL_MIN_SPEED;
        let q = 0.5 * air.density * speed_sq;
        let qs = q * surface.area();
        let lift_dir = (chord * -vn + normal * vc) / speed;
        let drag_dir = -(chord * vc + normal * vn) / speed;
        let lift = coefficients.cl * qs;
        let drag = coefficients.cd * qs;
        let force = lift_dir * lift + drag_dir * drag;
        let moment = span * (coefficients.cm * qs * surface.chord);
        aero_force += force;
        torque += arm.cross(force) + moment;
        *out = SurfaceState {
            alpha,
            deflection,
            coefficients,
            dynamic_pressure: q,
            lift,
            drag,
            force,
        };
    }
    let air_velocity = kin.linear_velocity - env.wind;
    if air.airspeed > STILL_AIR && air.airspeed.is_finite() {
        aero_force += -air_velocity / air.airspeed * (air.dynamic_pressure * config.parasite_drag_area);
    }
    for (engine, out) in config.engines.iter().zip(thrusts.iter_mut()) {
        let direction = rotation * engine.direction.normalize_or_zero();
        let arm = rotation * (engine.position - com_local);
        let at = kin.linear_velocity + kin.angular_velocity.cross(arm) - env.wind;
        let thrust = engine.thrust(input.throttle, at.dot(direction), air.density);
        *out = thrust;
        let force = direction * thrust;
        thrust_force += force;
        torque += arm.cross(force);
    }
    let (mut cl, mut cd) = (0.0, 0.0);
    let qs = air.dynamic_pressure * config.reference_area();
    if air.airspeed > STILL_AIR && qs > 0.0 && qs.is_finite() {
        let flow = air_velocity / air.airspeed;
        let right = rotation * RIGHT;
        let lift_dir = right.cross(flow).normalize_or(rotation * UP);
        cl = aero_force.dot(lift_dir) / qs;
        cd = -aero_force.dot(flow) / qs;
    }
    Loads {
        aero_force,
        thrust_force,
        torque,
        air,
        cl,
        cd,
        stalled,
    }
}

// ---------------------------------------------------------------------------
// The aircraft in a world
// ---------------------------------------------------------------------------

/// One gear leg after the last update.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct GearState {
    /// The leg found ground within its reach.
    pub grounded: bool,
    /// How far the strut is compressed, meters (0 when airborne).
    pub compression: f32,
    /// Spring and damper force along the ground normal, newtons.
    pub normal_force: f32,
    /// Friction force (rolling, braking and side grip), newtons.
    pub friction_force: f32,
    /// Steering angle, radians (+ turns the wheel right).
    pub steer_angle: f32,
    /// Speed of the contact along the wheel's rolling direction, m/s.
    pub rolling_speed: f32,
    /// World point of the contact, if grounded.
    pub contact_point: Option<Vec3>,
    /// Ground normal at the contact (internal: the friction pass reads it).
    normal: Vec3,
}

/// The aircraft's pose, velocity and air data after the last update.
#[derive(Debug, Clone, PartialEq)]
pub struct AircraftState {
    /// Body translation, world meters.
    pub translation: Vec3,
    /// Body rotation.
    pub rotation: Quat,
    /// Velocity of the center of mass, m/s.
    pub linear_velocity: Vec3,
    /// Body rates about the body's right (pitch, + nose up), up (yaw, + nose left) and back
    /// (roll, + left wing down) axes, rad/s: the world angular velocity in body axes.
    pub body_rates: Vec3,
    /// Air data at the center of mass (from the last update).
    pub air: AirData,
    /// Aircraft lift coefficient.
    pub cl: f32,
    /// Aircraft drag coefficient.
    pub cd: f32,
    /// Some wing surface is past its stall angle.
    pub stalled: bool,
    /// Total thrust, newtons.
    pub thrust: f32,
    /// Gear legs touching the ground.
    pub grounded_gear: usize,
    /// Heading, radians: 0 flying −Z, + toward +X (clockwise seen from above).
    pub heading: f32,
    /// Pitch attitude, radians (+ nose up).
    pub pitch: f32,
    /// Bank, radians (+ right wing down).
    pub bank: f32,
}

/// Heading, pitch and bank of a rotation in the engine's axes.
#[must_use]
pub fn attitude(rotation: Quat) -> (f32, f32, f32) {
    let forward = rotation * FORWARD;
    let right = rotation * RIGHT;
    let heading = forward.x.atan2(-forward.z);
    let pitch = forward.y.clamp(-1.0, 1.0).asin();
    // Bank: the right wing's drop below the horizon, measured in the plane across the nose.
    let level_right = Vec3::new(-forward.z, 0.0, forward.x).normalize_or(Vec3::X);
    let up_across = level_right.cross(forward).normalize_or(Vec3::Y);
    let bank = (-right.dot(up_across)).atan2(right.dot(level_right));
    (heading, pitch, bank)
}

/// An aircraft in a [`PhysicsWorld`].
#[derive(Debug, Clone)]
pub struct Aircraft {
    config: AircraftConfig,
    chassis: RigidBodyHandle,
    chassis_collider: Option<rapier3d::geometry::ColliderHandle>,
    surfaces: Vec<SurfaceState>,
    thrusts: Vec<f32>,
    gear: Vec<GearState>,
    loads: Loads,
    /// The force and torque this aircraft left on its body's force accumulator (rapier keeps user forces
    /// between steps; each update replaces its own share).
    applied_force: Vec3,
    applied_torque: Vec3,
    input: AircraftInput,
    steps: u64,
}

impl Aircraft {
    /// Creates the aircraft: a dynamic body (never sleeps) with a box fuselage collider at the given
    /// pose, moving at `linear_velocity` (world m/s).
    ///
    /// # Errors
    /// An invalid configuration, pose or velocity; nothing is added to the world then.
    pub fn spawn(
        physics: &mut PhysicsWorld,
        config: AircraftConfig,
        translation: Vec3,
        rotation: Quat,
        linear_velocity: Vec3,
    ) -> Result<Self, AeroError> {
        config.validate()?;
        let pose = checked_pose(translation, rotation).map_err(|_| AeroError::InvalidPose)?;
        if !linear_velocity.is_finite() {
            return Err(AeroError::InvalidPose);
        }
        let body = RigidBodyBuilder::dynamic()
            .pose(to_rapier_pose(pose.0, pose.1))
            .linvel(to_rapier_vec3(linear_velocity))
            .can_sleep(false)
            .build();
        let chassis = physics.bodies.insert(body);
        let half = config.fuselage_half_extents;
        let mass = MassProperties::new(
            to_rapier_vec3(config.center_of_mass),
            config.mass,
            to_rapier_vec3(config.inertia),
        );
        let collider = ColliderBuilder::cuboid(half.x, half.y, half.z)
            .mass_properties(mass)
            .build();
        let handle = {
            let PhysicsWorld {
                bodies, colliders, ..
            } = physics;
            colliders.insert_with_parent(collider, chassis, bodies)
        };
        Ok(Self {
            surfaces: vec![SurfaceState::default(); config.surfaces.len()],
            thrusts: vec![0.0; config.engines.len()],
            gear: vec![GearState::default(); config.gear.len()],
            config,
            chassis: RigidBodyHandle::from_rapier(chassis),
            chassis_collider: Some(handle),
            loads: Loads::default(),
            applied_force: Vec3::ZERO,
            applied_torque: Vec3::ZERO,
            input: AircraftInput::default(),
            steps: 0,
        })
    }

    /// The body.
    #[must_use]
    pub fn chassis(&self) -> RigidBodyHandle {
        self.chassis
    }

    /// The fuselage collider.
    #[must_use]
    pub fn chassis_collider(&self) -> Option<ColliderHandle> {
        self.chassis_collider.map(ColliderHandle::from_rapier)
    }

    /// The configuration.
    #[must_use]
    pub fn config(&self) -> &AircraftConfig {
        &self.config
    }

    /// Fixed steps applied so far.
    #[must_use]
    pub fn steps(&self) -> u64 {
        self.steps
    }

    /// Each surface after the last update, in configuration order.
    #[must_use]
    pub fn surfaces(&self) -> &[SurfaceState] {
        &self.surfaces
    }

    /// Each gear leg after the last update, in configuration order.
    #[must_use]
    pub fn gear(&self) -> &[GearState] {
        &self.gear
    }

    /// Each engine's thrust after the last update, newtons.
    #[must_use]
    pub fn thrusts(&self) -> &[f32] {
        &self.thrusts
    }

    /// The loads of the last update.
    #[must_use]
    pub fn loads(&self) -> &Loads {
        &self.loads
    }

    /// The input the last update used (sanitized, assist included).
    #[must_use]
    pub fn last_input(&self) -> AircraftInput {
        self.input
    }

    /// Re-expresses the gear's last contact points after a floating-origin rebase.
    pub fn shift_origin(&mut self, shift: Vec3) {
        for gear in &mut self.gear {
            if let Some(point) = &mut gear.contact_point {
                *point -= shift;
            }
        }
    }

    /// Applies one step: aerodynamic loads, thrust and gear. Call once per fixed step before the
    /// world steps. `layers` is the matrix the gear rays' `query_layer` reads. This crate's own
    /// code on this path allocates nothing; the one residual per-step allocation is rapier 0.36
    /// regrowing its modified-bodies list on the first `get_mut` after each step (ADR-3374; the
    /// hover craft and the vehicles pay the same).
    ///
    /// # Errors
    /// A bad `dt` or a missing body; the world is unchanged then.
    pub fn update_with_layers(
        &mut self,
        physics: &mut PhysicsWorld,
        input: AircraftInput,
        env: &AeroEnvironment,
        dt: f32,
        layers: Option<&PhysicsLayers>,
    ) -> Result<(), AeroError> {
        if !(dt.is_finite() && dt > 0.0) {
            return Err(AeroError::InvalidTimeStep);
        }
        let handle = self.chassis.to_handle();
        let (pose, mass, linvel, angvel) = match physics.bodies.get(handle) {
            Some(b) if b.is_dynamic() && b.mass().is_finite() && b.mass() > 0.0 => (
                *b.position(),
                b.mass(),
                from_rapier_vec3(b.linvel()),
                from_rapier_vec3(b.angvel()),
            ),
            _ => return Err(AeroError::ChassisMissing),
        };
        let input = input.sanitized();
        let (centre, rotation) = from_rapier_pose(&pose);
        let com = centre + rotation * self.config.center_of_mass;
        let kin = Kinematics {
            center_of_mass: com,
            rotation,
            linear_velocity: linvel,
            angular_velocity: angvel,
        };
        let loads = evaluate_loads(
            &self.config,
            &kin,
            &input,
            env,
            &mut self.surfaces,
            &mut self.thrusts,
        );
        self.update_gear(physics, &kin, &input, layers);
        let Some(body) = physics.bodies.get_mut(handle) else {
            return Err(AeroError::ChassisMissing);
        };
        // Loads are forces, not impulses: rapier integrates them over its substeps exactly like gravity,
        // so a trimmed state stays in equilibrium (a start-of-step impulse against substepped gravity
        // drifts by g·dt·(n − 1)/(2n) per step). Only this aircraft's own contribution is replaced, so
        // another force on the body (an `ExternalForce`) survives.
        let mut force = loads.aero_force + loads.thrust_force;
        let mut torque = loads.torque;
        for gear in &self.gear {
            if let Some(point) = gear.contact_point
                && gear.grounded
            {
                let push = gear.normal * gear.normal_force;
                force += push;
                torque += (point - com).cross(push);
            }
        }
        if !(force.is_finite() && torque.is_finite()) {
            force = Vec3::ZERO;
            torque = Vec3::ZERO;
        }
        body.add_force(to_rapier_vec3(force - self.applied_force), true);
        body.add_torque(to_rapier_vec3(torque - self.applied_torque), true);
        self.applied_force = force;
        self.applied_torque = torque;
        self.apply_gear_friction(physics, &kin, &input, mass, dt);
        self.loads = loads;
        self.input = input;
        self.steps += 1;
        Ok(())
    }

    /// Casts every gear ray and records the spring-damper force.
    fn update_gear(
        &mut self,
        physics: &PhysicsWorld,
        kin: &Kinematics,
        input: &AircraftInput,
        layers: Option<&PhysicsLayers>,
    ) {
        if self.config.gear.is_empty() {
            return;
        }
        let handle = self.chassis.to_handle();
        let mut filter = QueryFilter::default()
            .exclude_sensors()
            .exclude_rigid_body(handle);
        if let Some(groups) = ray_groups(self.config.ground_layers, self.config.query_layer, layers) {
            filter = filter.groups(groups);
        }
        let pipeline = physics.query_pipeline(filter);
        let rotation = kin.rotation;
        let up = rotation * UP;
        let centre = kin.center_of_mass - rotation * self.config.center_of_mass;
        for (config, state) in self.config.gear.iter().zip(self.gear.iter_mut()) {
            let origin = centre + rotation * config.position;
            let reach = config.rest_length + config.wheel_radius;
            let steer_angle = if config.steerable {
                input.yaw * config.max_steer_deg.to_radians()
            } else {
                0.0
            };
            let ray = Ray::new(to_rapier_vec3(origin), to_rapier_vec3(-up));
            let Some((_, hit)) = pipeline.cast_ray_and_get_normal(&ray, reach, true) else {
                *state = GearState {
                    steer_angle,
                    ..GearState::default()
                };
                continue;
            };
            let distance = hit.time_of_impact;
            let normal = from_rapier_vec3(hit.normal).normalize_or(up);
            let compression = (reach - distance).max(0.0);
            let contact = origin - up * distance;
            let point_velocity =
                kin.linear_velocity + kin.angular_velocity.cross(contact - kin.center_of_mass);
            let along = point_velocity.dot(normal);
            let normal_force = (config.stiffness * compression - config.damping * along).max(0.0);
            *state = GearState {
                grounded: true,
                compression,
                normal_force,
                friction_force: 0.0,
                steer_angle,
                rolling_speed: 0.0,
                contact_point: Some(contact),
                normal,
            };
        }
    }

    /// Rolling resistance, brakes and side grip at each grounded wheel, as impulses capped by the
    /// friction limit and by the slip speed (so friction never reverses a wheel's motion in a step).
    fn apply_gear_friction(
        &mut self,
        physics: &mut PhysicsWorld,
        kin: &Kinematics,
        input: &AircraftInput,
        mass: f32,
        dt: f32,
    ) {
        let grounded = self.gear.iter().filter(|g| g.grounded).count();
        if grounded == 0 {
            return;
        }
        let Some(body) = physics.bodies.get_mut(self.chassis.to_handle()) else {
            return;
        };
        let inertia = self.config.inertia;
        let rotation = kin.rotation;
        let up = rotation * UP;
        for (config, state) in self.config.gear.iter().zip(self.gear.iter_mut()) {
            let Some(contact) = state.contact_point else {
                continue;
            };
            if !state.grounded {
                continue;
            }
            let normal = state.normal;
            // The wheel's rolling direction: the body's forward turned by the steering angle about the
            // body's up, laid on the ground plane.
            let heading = Quat::from_axis_angle(up, -state.steer_angle) * (rotation * FORWARD);
            let roll_dir = (heading - normal * heading.dot(normal)).normalize_or_zero();
            if roll_dir == Vec3::ZERO {
                continue;
            }
            let side_dir = normal.cross(roll_dir);
            let velocity = kin.linear_velocity + kin.angular_velocity.cross(contact - kin.center_of_mass);
            let v_roll = velocity.dot(roll_dir);
            let v_side = velocity.dot(side_dir);
            let n = state.normal_force;
            let brake = if config.brakes {
                input.brakes * config.brake_friction
            } else {
                0.0
            };
            // The impulse that would stop the contact's slip on its own, shared between the grounded
            // wheels and relaxed, so the wheels together never overshoot (they act below the center of
            // mass, and the rotation they add moves the contacts too).
            let arm = contact - kin.center_of_mass;
            let stop = |direction: Vec3| {
                let local = rotation.inverse() * arm.cross(direction);
                let angular = local.x * local.x / inertia.x
                    + local.y * local.y / inertia.y
                    + local.z * local.z / inertia.z;
                GRIP_RELAX / ((1.0 / mass + angular) * grounded as f32)
            };
            let roll_cap = stop(roll_dir) * v_roll.abs();
            let roll_impulse = ((config.rolling_friction + brake) * n * dt).min(roll_cap) * -v_roll.signum();
            let side_cap = config.lateral_friction * n * dt;
            let side_impulse = (-v_side * stop(side_dir)).clamp(-side_cap, side_cap);
            let impulse = roll_dir * roll_impulse + side_dir * side_impulse;
            state.rolling_speed = v_roll;
            state.friction_force = impulse.length() / dt;
            if impulse.is_finite() {
                body.apply_impulse_at_point(to_rapier_vec3(impulse), to_rapier_vec3(contact), true);
            }
        }
    }

    /// The pose, velocity and air data after the last update.
    ///
    /// # Errors
    /// [`AeroError::ChassisMissing`] when the body is gone.
    pub fn state(&self, physics: &PhysicsWorld) -> Result<AircraftState, AeroError> {
        let Some(body) = physics.bodies.get(self.chassis.to_handle()) else {
            return Err(AeroError::ChassisMissing);
        };
        let (translation, rotation) = from_rapier_pose(body.position());
        let linear_velocity = from_rapier_vec3(body.linvel());
        let angular_velocity = from_rapier_vec3(body.angvel());
        let body_rates = rotation.inverse() * angular_velocity;
        let (heading, pitch, bank) = attitude(rotation);
        Ok(AircraftState {
            translation,
            rotation,
            linear_velocity,
            body_rates,
            air: self.loads.air,
            cl: self.loads.cl,
            cd: self.loads.cd,
            stalled: self.loads.stalled,
            thrust: self.thrusts.iter().sum(),
            grounded_gear: self.gear.iter().filter(|g| g.grounded).count(),
            heading,
            pitch,
            bank,
        })
    }

    /// Removes the body from the world; `false` when it was already gone.
    pub fn remove(self, physics: &mut PhysicsWorld) -> bool {
        let handle = self.chassis.to_handle();
        if !physics.bodies.contains(handle) {
            return false;
        }
        physics.remove_body(handle);
        true
    }
}

// ---------------------------------------------------------------------------
// ECS
// ---------------------------------------------------------------------------

/// An additive control term the physics adds to the pilot's [`AircraftInput`] before clamping (the
/// framework's rate-damping assist writes it every fixed step).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ControlAugment {
    /// Added to [`AircraftInput::pitch`].
    pub pitch: f32,
    /// Added to [`AircraftInput::roll`].
    pub roll: f32,
    /// Added to [`AircraftInput::yaw`].
    pub yaw: f32,
}

/// An aircraft on an ECS entity, driven by [`drive_aircraft`] from the entity's [`AircraftInput`].
#[derive(Component, Debug, Clone)]
pub struct AircraftController {
    /// The aircraft.
    pub aircraft: Aircraft,
    /// The error from the last fixed-step update, `None` when it succeeded.
    pub last_error: Option<AeroError>,
    /// Added to the pilot's input each step (zero unless an assist writes it).
    pub augment: ControlAugment,
}

/// Creates an aircraft in the app's `PhysicsWorld` and an entity that owns it (body and collider
/// handles, a `Transform`, an [`AircraftController`] and a released [`AircraftInput`]).
///
/// # Errors
/// [`AeroError::NoPhysicsWorld`] without a world; a configuration or pose error. Nothing is spawned
/// on error.
pub fn spawn_aircraft(
    world: &mut World,
    config: AircraftConfig,
    translation: Vec3,
    rotation: Quat,
    linear_velocity: Vec3,
) -> Result<Entity, AeroError> {
    let aircraft = {
        let Some(mut physics) = world.get_resource_mut::<PhysicsWorld>() else {
            return Err(AeroError::NoPhysicsWorld);
        };
        Aircraft::spawn(&mut physics, config, translation, rotation, linear_velocity)?
    };
    let chassis = aircraft.chassis();
    let collider = aircraft.chassis_collider;
    let pose = world.get_resource::<PhysicsWorld>().and_then(|p| {
        p.bodies
            .get(chassis.to_handle())
            .map(|b| from_rapier_pose(b.position()))
    });
    let (translation, rotation) = pose.unwrap_or((translation, rotation));
    let mut entity = world.spawn((
        chassis,
        Transform {
            translation,
            rotation,
            scale: Vec3::ONE,
        },
        AircraftController {
            aircraft,
            last_error: None,
            augment: ControlAugment::default(),
        },
        AircraftInput::default(),
    ));
    if let Some(collider) = collider {
        entity.insert(ColliderHandle::from_rapier(collider));
    }
    let entity = entity.id();
    if let Some(mut physics) = world.get_resource_mut::<PhysicsWorld>() {
        physics.register_body(chassis.to_handle(), entity);
        if let Some(collider) = collider {
            physics.register_collider(collider, entity);
        }
    }
    Ok(entity)
}

/// Updates every [`AircraftController`] from its entity's [`AircraftInput`] plus its
/// [`AircraftController::augment`], in spawn order (component arrival, then entity id). Errors are
/// stored in [`AircraftController::last_error`]. An aircraft of a parked data layer is not driven.
/// The order list is kept between runs, so a steady fleet allocates nothing.
pub fn drive_aircraft(
    fixed: Option<Res<FixedTime>>,
    physics: Option<ResMut<PhysicsWorld>>,
    layers: Option<Res<PhysicsLayers>>,
    env: Option<Res<AeroEnvironment>>,
    order: Option<Res<regia_ecs::spawn_order::SpawnOrder<AircraftController>>>,
    mut aircraft: Query<(Entity, &mut AircraftController, Option<&AircraftInput>), Without<LayerParked>>,
    mut drive: Local<Vec<Entity>>,
) {
    let (Some(fixed), Some(mut physics)) = (fixed, physics) else {
        return;
    };
    let _span = debug_span!("drive_aircraft").entered();
    let env = env.as_deref().copied().unwrap_or_default();
    let dt = fixed.step().as_secs_f32();
    drive.clear();
    drive.extend(aircraft.iter().map(|(entity, _, _)| entity));
    // Keys are unique (the entity bits are part of them), so the unstable sort is deterministic and
    // allocation-free.
    drive.sort_unstable_by_key(|entity| crate::step::spawn_key(order.as_deref(), *entity));
    for entity in drive.iter().copied() {
        let Ok((_, mut controller, input)) = aircraft.get_mut(entity) else {
            continue;
        };
        let mut input = input.copied().unwrap_or_default();
        let augment = controller.augment;
        input.pitch += augment.pitch;
        input.roll += augment.roll;
        input.yaw += augment.yaw;
        let error = controller
            .aircraft
            .update_with_layers(&mut physics, input, &env, dt, layers.as_deref())
            .err();
        if controller.last_error != error {
            controller.last_error = error;
        }
    }
}

/// A rebase moves the gear's last contact points.
pub fn shift_aircraft_origin(
    mut shifted: MessageReader<regia_scene::streaming::OriginShifted>,
    mut aircraft: Query<&mut AircraftController>,
) {
    for message in shifted.read() {
        let shift = Vec3::from_array(message.rebase.shift);
        for mut controller in &mut aircraft {
            controller.aircraft.shift_origin(shift);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wing() -> AeroSurface {
        AeroSurface {
            cl0: 0.2,
            ..AeroSurface::default()
        }
    }

    #[test]
    fn lift_slope_is_the_finite_wing_formula() {
        let s = AeroSurface {
            span: 8.0,
            chord: 1.0,
            ..AeroSurface::default()
        };
        let expected = std::f32::consts::TAU * 8.0 / 10.0;
        assert!((s.lift_slope() - expected).abs() < 1.0e-6);
        // An infinite wing tends to 2π.
        let long = AeroSurface {
            aspect_ratio: Some(1.0e4),
            ..AeroSurface::default()
        };
        assert!((long.lift_slope() - std::f32::consts::TAU).abs() < 2.0e-3);
    }

    #[test]
    fn flap_effectiveness_matches_thin_airfoil_limits() {
        let full = AeroSurface {
            control_chord_fraction: 0.9,
            ..AeroSurface::default()
        };
        let quarter = AeroSurface {
            control_chord_fraction: 0.25,
            ..AeroSurface::default()
        };
        // cf/c = 0.25: θf = acos(-0.5) = 2π/3, τ = 1 − (2π/3 − √3/2)/π ≈ 0.609.
        let expected =
            1.0 - (2.0 * std::f32::consts::FRAC_PI_3 - 3.0_f32.sqrt() / 2.0) / std::f32::consts::PI;
        assert!((quarter.flap_effectiveness() - expected).abs() < 1.0e-5);
        assert!(full.flap_effectiveness() > quarter.flap_effectiveness());
        // A trailing-edge-down flap pitches the nose down.
        assert!(quarter.flap_moment_slope() < 0.0);
    }

    #[test]
    fn attached_lift_is_linear_and_drag_has_the_induced_term() {
        let s = wing();
        let alpha = 5.0_f32.to_radians();
        let c = s.coefficients(alpha, 0.0);
        assert!(!c.stalled);
        assert!((c.cl - (0.2 + s.lift_slope() * alpha)).abs() < 1.0e-6);
        let ar = s.effective_aspect_ratio();
        let induced = c.cl * c.cl / (std::f32::consts::PI * 0.8 * ar);
        assert!((c.cd - (0.008 + induced)).abs() < 1.0e-6);
    }

    #[test]
    fn past_the_stall_angle_lift_drops_and_the_flag_is_set() {
        let s = wing();
        let stall = s.stall_angle_deg.to_radians();
        let at = s.coefficients(stall, 0.0);
        assert!(!at.stalled);
        let mut previous = at.cl;
        for degrees in 1..=40 {
            let c = s.coefficients(stall + (degrees as f32 * 0.5).to_radians(), 0.0);
            assert!(c.stalled, "{degrees}");
            assert!(c.cl < at.cl, "{degrees}: {} >= {}", c.cl, at.cl);
            if degrees <= 8 {
                assert!(c.cl <= previous + 1.0e-6, "{degrees}");
            }
            previous = c.cl;
        }
        // Negative stall too.
        assert!(s.coefficients(-stall - 0.1, 0.0).stalled);
    }

    #[test]
    fn deflection_shifts_lift_and_moment() {
        let s = wing();
        let plain = s.coefficients(0.0, 0.0);
        let down = s.coefficients(0.0, 10.0_f32.to_radians());
        assert!(down.cl > plain.cl);
        assert!(down.cm < plain.cm);
        let shift = s.flap_effectiveness() * 10.0_f32.to_radians() * s.lift_slope();
        assert!((down.cl - plain.cl - shift).abs() < 1.0e-5);
    }

    #[test]
    fn coefficients_are_finite_everywhere() {
        let s = wing();
        for i in -720..=720 {
            let c = s.coefficients(i as f32 * 0.5_f32.to_radians(), 0.3);
            assert!(c.cl.is_finite() && c.cd.is_finite() && c.cm.is_finite(), "{i}");
            assert!(c.cd >= 0.0, "{i}");
        }
        let c = s.coefficients(f32::NAN, f32::INFINITY);
        assert!(c.cl.is_finite() && c.cd.is_finite());
    }

    #[test]
    fn isa_matches_the_standard_table() {
        assert!((isa_density(0.0) - 1.225).abs() < 1.0e-3);
        // ISA table densities at geometric altitudes 1, 5, 11 and 15 km; the model reads geopotential
        // altitude, H = r0·h/(r0 + h) with r0 = 6 356 766 m.
        for (h, rho) in [
            (1000.0_f32, 1.1117),
            (5000.0, 0.7364),
            (11000.0, 0.3648),
            (15000.0, 0.1948),
        ] {
            let geopotential = 6_356_766.0 * h / (6_356_766.0 + h);
            let got = isa_density(geopotential);
            assert!((got - rho).abs() / rho < 1.0e-3, "{h}: {got} vs {rho}");
        }
        assert!(isa_density(f32::NAN) > 1.2);
        assert_eq!(isa_density(1.0e6), isa_density(ISA_CEILING));
    }

    #[test]
    fn surface_lift_equals_cl_q_s() {
        let config = AircraftConfig {
            surfaces: vec![AeroSurface {
                position: Vec3::ZERO,
                ..wing()
            }],
            engines: Vec::new(),
            gear: Vec::new(),
            parasite_drag_area: 0.0,
            ..trainer()
        };
        let alpha = 6.0_f32.to_radians();
        let speed = 50.0;
        let kin = Kinematics {
            center_of_mass: Vec3::new(0.0, 500.0, 0.0),
            rotation: Quat::from_rotation_x(alpha),
            linear_velocity: Vec3::new(0.0, 0.0, -speed),
            angular_velocity: Vec3::ZERO,
        };
        let mut surfaces = [SurfaceState::default()];
        let loads = evaluate_loads(
            &config,
            &kin,
            &AircraftInput::default(),
            &AeroEnvironment::default(),
            &mut surfaces,
            &mut [],
        );
        let s = &config.surfaces[0];
        let q = 0.5 * isa_density(500.0) * speed * speed;
        let expected = s.coefficients(alpha, 0.0).cl * q * s.area();
        assert!((surfaces[0].alpha - alpha).abs() < 1.0e-5);
        // Lift is the force across the flow (world +Y here).
        assert!(
            (loads.aero_force.y - expected).abs() / expected < 0.01,
            "{} vs {expected}",
            loads.aero_force.y
        );
        assert!((surfaces[0].lift - expected).abs() / expected < 0.01);
        assert!(loads.aero_force.z > 0.0, "drag points back (+Z)");
    }

    #[test]
    fn zero_airspeed_gives_finite_loads() {
        let config = trainer();
        let kin = Kinematics {
            center_of_mass: Vec3::ZERO,
            rotation: Quat::IDENTITY,
            linear_velocity: Vec3::ZERO,
            angular_velocity: Vec3::ZERO,
        };
        let mut surfaces = vec![SurfaceState::default(); config.surfaces.len()];
        let mut thrusts = vec![0.0; config.engines.len()];
        let input = AircraftInput {
            throttle: 1.0,
            pitch: 1.0,
            ..AircraftInput::default()
        };
        let loads = evaluate_loads(
            &config,
            &kin,
            &input,
            &AeroEnvironment::default(),
            &mut surfaces,
            &mut thrusts,
        );
        assert!(loads.aero_force.is_finite() && loads.torque.is_finite());
        assert_eq!(loads.air.alpha, 0.0);
        assert!((thrusts[0] - 2600.0).abs() < 1.0);
        assert!(surfaces.iter().all(|s| s.alpha == 0.0 && s.force == Vec3::ZERO));
    }

    #[test]
    fn propeller_and_jet_thrust_lapse() {
        let prop = EngineConfig::default();
        assert!((prop.thrust(1.0, 0.0, ISA_SEA_LEVEL_DENSITY) - 2000.0).abs() < 1.0e-3);
        assert!((prop.thrust(1.0, 80.0, ISA_SEA_LEVEL_DENSITY) - 1000.0).abs() < 1.0e-2);
        assert!((prop.thrust(0.5, 0.0, ISA_SEA_LEVEL_DENSITY * 0.5) - 500.0).abs() < 1.0e-2);
        let jet = EngineConfig {
            model: ThrustModel::Jet { speed_lapse: 0.5 },
            ..EngineConfig::default()
        };
        let at_mach_half = jet.thrust(1.0, 0.5 * ISA_SEA_LEVEL_SPEED_OF_SOUND, ISA_SEA_LEVEL_DENSITY);
        assert!((at_mach_half - 1500.0).abs() < 1.0e-2);
    }

    #[test]
    fn attitude_reads_heading_pitch_and_bank() {
        let (h, p, b) = attitude(Quat::IDENTITY);
        assert!(h.abs() < 1.0e-6 && p.abs() < 1.0e-6 && b.abs() < 1.0e-6);
        let (_, p, _) = attitude(Quat::from_rotation_x(0.2));
        assert!((p - 0.2).abs() < 1.0e-5);
        // A positive rotation about forward (−Z) lowers the right wing: bank +.
        let (_, _, b) = attitude(Quat::from_axis_angle(FORWARD, 0.3));
        assert!((b - 0.3).abs() < 1.0e-5, "{b}");
        let (h, _, _) = attitude(Quat::from_rotation_y(-0.4));
        assert!((h - 0.4).abs() < 1.0e-5, "{h}");
    }

    #[test]
    fn the_trainer_validates_and_bad_fields_are_named() {
        trainer().validate().expect("trainer");
        let mut bad = trainer();
        bad.surfaces[2].span = -1.0;
        assert_eq!(
            bad.validate(),
            Err(AeroError::InvalidConfig {
                field: "surfaces[2].span".into()
            })
        );
        let mut none = trainer();
        none.surfaces.clear();
        assert_eq!(none.validate().map_err(|e| e.code()), Err("aircraft_no_surfaces"));
        let mut many = trainer();
        many.gear = vec![GearConfig::default(); MAX_GEAR + 1];
        assert_eq!(
            many.validate().map_err(|e| e.code()),
            Err("aircraft_too_many_parts")
        );
        let mut along = trainer();
        along.surfaces[0].normal = Vec3::NEG_Z;
        assert_eq!(
            along.validate().map_err(|e| e.code()),
            Err("aircraft_invalid_config")
        );
    }
}
