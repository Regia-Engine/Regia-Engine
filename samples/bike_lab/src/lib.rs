// SPDX-License-Identifier: MIT-0
//! Headless bike lab (ADR-4017).
//!
//! [`run`] steps [`regia_physics::vehicle_bike::bike_step`] for a fixed count. The default is 600
//! steps of 1/60 s, ten seconds, with a steer of 0.08 rad and a speed of 8 m/s. Nothing here opens
//! a window or reads the clock. Two calls with the same [`LabInputs`] return the same digest.

use regia_physics::vehicle_bike;

pub use regia_physics::vehicle_bike::{
    BikeConfig, BikeError, BikeInput, BikeState, FIXED_STEP_S, bike_step, steady_lean,
};

/// What to run. Every field is an argument; there is no bike file format.
#[derive(Clone, Debug, PartialEq)]
pub struct LabInputs {
    /// How many fixed steps.
    pub steps: u32,
    /// Seconds per step.
    pub dt: f32,
    /// Front steer, radians.
    pub steer: f32,
    /// Travel speed, m/s.
    pub speed: f32,
    /// Drive command.
    pub drive: f32,
    /// Front wheel ray length, meters.
    pub front_ray: f32,
    /// Rear wheel ray length, meters.
    pub rear_ray: f32,
    /// Lengths and rates.
    pub config: BikeConfig,
}

impl Default for LabInputs {
    fn default() -> Self {
        Self {
            steps: 600,
            dt: FIXED_STEP_S,
            steer: 0.08,
            speed: 8.0,
            drive: 0.0,
            front_ray: 0.3,
            rear_ray: 0.3,
            config: BikeConfig::default(),
        }
    }
}

/// The bike after a run.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LabReport {
    /// Steps that ran.
    pub steps: u32,
    /// Final lean, radians.
    pub lean: f32,
    /// Final steer, radians.
    pub steer: f32,
    /// Final speed, m/s.
    pub speed: f32,
    /// Final drive command.
    pub drive: f32,
    /// Front wheel in contact.
    pub front_contact: bool,
    /// Rear wheel in contact.
    pub rear_contact: bool,
    /// The lean passed the maximum during the run.
    pub fallen: bool,
    /// [`BikeState::digest`] of the final state.
    pub digest: u64,
}

impl std::fmt::Display for LabReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "steps={} lean={:.6} steer={:.6} speed={:.6} drive={:.6} front_contact={} rear_contact={} fallen={} digest={:016x}",
            self.steps,
            self.lean,
            self.steer,
            self.speed,
            self.drive,
            self.front_contact,
            self.rear_contact,
            self.fallen,
            self.digest
        )
    }
}

/// Either a run or the usage text.
#[derive(Clone, Debug, PartialEq)]
pub enum Launch {
    /// Step the bike.
    Run(LabInputs),
    /// The caller asked for usage.
    Help,
}

/// Usage line for `--help`.
pub const USAGE: &str = "\
bike_lab [--steps N | --seconds S] [--dt S] [--steer RAD] [--speed M/S] [--drive N] \
[--front-ray M] [--rear-ray M] [--wheelbase M] [--max-lean RAD]
Default: 600 steps of 1/60 s (10 s), steer 0.08 rad, speed 8 m/s. No window.";

/// Parse arguments into a launch. Unknown flags and bad numbers are an error string.
///
/// # Errors
///
/// An unknown flag, a missing value, or a number outside the range the flag accepts.
pub fn launch_from_args<I, S>(args: I) -> Result<Launch, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut inputs = LabInputs::default();
    let mut seconds: Option<f32> = None;
    let mut steps_set = false;
    let mut iter = args.into_iter();
    while let Some(raw) = iter.next() {
        let flag = raw.as_ref();
        if flag == "--help" || flag == "-h" {
            return Ok(Launch::Help);
        }
        let value = iter
            .next()
            .map(|item| item.as_ref().to_string())
            .ok_or_else(|| format!("{flag} needs a value"))?;
        match flag {
            "--steps" => {
                let steps: u32 = value
                    .parse()
                    .map_err(|_| format!("--steps needs a positive integer, got {value}"))?;
                if steps == 0 || steps > 60_000 {
                    return Err("--steps must be from 1 to 60000".to_string());
                }
                inputs.steps = steps;
                steps_set = true;
            }
            "--seconds" => seconds = Some(parse_finite("--seconds", &value)?),
            "--dt" => inputs.dt = parse_finite("--dt", &value)?,
            "--steer" => inputs.steer = parse_finite("--steer", &value)?,
            "--speed" => inputs.speed = parse_finite("--speed", &value)?,
            "--drive" => inputs.drive = parse_finite("--drive", &value)?,
            "--front-ray" => inputs.front_ray = parse_finite("--front-ray", &value)?,
            "--rear-ray" => inputs.rear_ray = parse_finite("--rear-ray", &value)?,
            "--wheelbase" => inputs.config.wheelbase = parse_finite("--wheelbase", &value)?,
            "--max-lean" => inputs.config.max_lean = parse_finite("--max-lean", &value)?,
            other => return Err(format!("unknown argument {other}\n{USAGE}")),
        }
    }
    if let Some(seconds) = seconds
        && !steps_set
    {
        if seconds <= 0.0 {
            return Err("--seconds must be positive".to_string());
        }
        let steps = (seconds / inputs.dt).round();
        if !(1.0..=60_000.0).contains(&steps) {
            return Err("--seconds is outside 1 step ..= 60000 steps".to_string());
        }
        inputs.steps = steps as u32;
    }
    Ok(Launch::Run(inputs))
}

fn parse_finite(flag: &str, text: &str) -> Result<f32, String> {
    let value: f32 = text
        .parse()
        .map_err(|_| format!("{flag} needs a number, got {text}"))?;
    if value.is_finite() {
        Ok(value)
    } else {
        Err(format!("{flag} must be finite"))
    }
}

/// Run the lab. This is the only integration: a loop of [`bike_step`].
///
/// # Errors
///
/// [`BikeError`] when a step's inputs are outside the balance model's range. No step is reported
/// in that case.
pub fn run(inputs: &LabInputs) -> Result<LabReport, BikeError> {
    let mut state = BikeState::default();
    let command = BikeInput {
        dt: inputs.dt,
        steer: inputs.steer,
        speed: inputs.speed,
        drive: inputs.drive,
        front_ray: inputs.front_ray,
        rear_ray: inputs.rear_ray,
    };
    for _ in 0..inputs.steps {
        vehicle_bike::bike_step(&mut state, &inputs.config, &command)?;
    }
    Ok(LabReport {
        steps: inputs.steps,
        lean: state.lean,
        steer: state.steer,
        speed: state.speed,
        drive: state.drive,
        front_contact: state.front_contact,
        rear_contact: state.rear_contact,
        fallen: state.fallen,
        digest: state.digest(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ten_seconds_is_six_hundred_steps() {
        let Launch::Run(inputs) = launch_from_args(["--seconds", "10"]).expect("args") else {
            panic!("--seconds is a run");
        };
        assert_eq!(inputs.steps, 600);
        assert!((inputs.dt - FIXED_STEP_S).abs() < 1.0e-8);
    }
}
