// SPDX-License-Identifier: MPL-2.0
//! `fluid.sleep_mask`: which cells of a speed grid to integrate this step (ADR-3949,
//! `book/agents/fluid.md`).
//!
//! The call is pure. It does not read or write a `FluidWorld`. The caller owns the speed slice
//! and passes the previous quiet counts back in `still`.

use regia_app::prelude::*;
use regia_fluid::sleep::{self, DEFAULT_EPSILON, DEFAULT_QUIET_STEPS};
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

const HINT: &str = "see book/agents/fluid.md (cell sleep) for the parameters and codes";

fn bad(reason: &str, message: impl Into<String>) -> RpcError {
    let mut error = RpcError::new(protocol::INVALID_PARAMS, "F0002", message, HINT);
    if let Some(Value::Object(data)) = error.data.as_mut() {
        data.insert("reason".into(), json!(reason));
    }
    error
}

fn required_dim(params: &Value, key: &str) -> Result<usize, RpcError> {
    match crate::params::opt_u64(params, key)? {
        None => Err(bad("fluid_bad_param", format!("`{key}` is required"))),
        Some(v) => usize::try_from(v).map_err(|_| bad("fluid_bad_param", format!("`{key}` is too large"))),
    }
}

fn finite_f32(x: f64) -> Option<f32> {
    if !x.is_finite() {
        return None;
    }
    let x = x as f32;
    x.is_finite().then_some(x)
}

fn speeds_of(values: &[Value]) -> Result<Vec<f32>, RpcError> {
    let mut out = Vec::with_capacity(values.len());
    for (i, value) in values.iter().enumerate() {
        let Some(x) = value.as_f64().and_then(finite_f32) else {
            return Err(bad(
                "fluid_bad_param",
                format!("speeds[{i}] must be a finite number"),
            ));
        };
        out.push(x);
    }
    Ok(out)
}

fn still_of(values: &[Value]) -> Result<Vec<u32>, RpcError> {
    let mut out = Vec::with_capacity(values.len());
    for (i, value) in values.iter().enumerate() {
        let Some(n) = value.as_u64() else {
            return Err(bad(
                "fluid_bad_param",
                format!("still[{i}] must be a non-negative integer"),
            ));
        };
        let Some(n) = u32::try_from(n).ok() else {
            return Err(bad("fluid_bad_param", format!("still[{i}] does not fit in u32")));
        };
        out.push(n);
    }
    Ok(out)
}

/// `fluid.sleep_mask`.
pub fn sleep_mask(_world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let nx = required_dim(params, "nx")?;
    let ny = required_dim(params, "ny")?;
    let nz = required_dim(params, "nz")?;
    let epsilon = match crate::params::opt_f64(params, "epsilon")? {
        None => DEFAULT_EPSILON,
        Some(v) => finite_f32(v).ok_or_else(|| bad("fluid_bad_param", "`epsilon` must be finite"))?,
    };
    let k = match crate::params::opt_u64(params, "k")? {
        None => DEFAULT_QUIET_STEPS,
        Some(v) => u32::try_from(v).map_err(|_| bad("fluid_bad_param", "`k` does not fit in u32"))?,
    };
    let speeds_json = crate::params::opt_array(params, "speeds")?
        .ok_or_else(|| bad("fluid_bad_param", "`speeds` (one number per cell) is required"))?;
    let still_json = crate::params::opt_array(params, "still")?;
    let speeds = speeds_of(speeds_json)?;
    let still = match still_json {
        None => Vec::new(),
        Some(values) => still_of(values)?,
    };
    let step = sleep::cells_to_integrate(nx, ny, nz, &speeds, &still, epsilon, k)
        .map_err(|e| bad(e.code(), e.to_string()))?;
    let cells = step.still.len();
    let asleep = cells.saturating_sub(step.integrate.len());
    let digest = step.digest;
    Ok(json!({
        "nx": nx,
        "ny": ny,
        "nz": nz,
        "epsilon": epsilon,
        "k": k,
        "cells": cells,
        "asleep": asleep,
        "integrate": step.integrate,
        "still": step.still,
        "digest": format!("{digest:016x}"),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reason(e: &RpcError) -> Option<&str> {
        e.data.as_ref()?.get("reason")?.as_str()
    }

    fn grid(n: usize, speed: f32) -> Vec<f32> {
        vec![speed; n * n * n]
    }

    #[test]
    fn sleep_mask_bad_grid_names_its_code() {
        let mut world = World::new();
        let err = sleep_mask(
            &mut world,
            &json!({ "nx": 2, "ny": 2, "nz": 2, "speeds": [0.0, 0.0, 0.0] }),
        )
        .expect_err("length 3 is not 8");
        assert_eq!(reason(&err), Some("fluid_sleep_bad_grid"));
        assert_eq!(
            err.data
                .as_ref()
                .and_then(|d| d.get("code"))
                .and_then(Value::as_str),
            Some("F0002")
        );
    }

    #[test]
    fn sleep_mask_sleeps_a_still_grid_and_wakes_a_halo() {
        let mut world = World::new();
        let speeds = grid(4, 0.0);
        let mut still = Value::Null;
        let mut digest = String::new();
        for step in 1..=3 {
            let answer = sleep_mask(
                &mut world,
                &json!({
                    "nx": 4, "ny": 4, "nz": 4, "k": 3, "epsilon": 0.001,
                    "speeds": &speeds, "still": &still
                }),
            )
            .expect("step");
            digest = answer["digest"].as_str().expect("digest").to_string();
            if step < 3 {
                assert_eq!(answer["integrate"].as_array().map(Vec::len), Some(64));
                assert_eq!(answer["asleep"], 0);
            } else {
                assert_eq!(answer["integrate"].as_array().map(Vec::len), Some(0));
                assert_eq!(answer["asleep"], 64);
            }
            still = answer["still"].clone();
        }
        // A second run of the same three steps matches the digest.
        let mut prev = Value::Array(vec![]);
        let mut third = Value::Null;
        for _ in 0..3 {
            third = sleep_mask(
                &mut world,
                &json!({
                    "nx": 4, "ny": 4, "nz": 4, "k": 3, "epsilon": 0.001,
                    "speeds": grid(4, 0.0), "still": &prev
                }),
            )
            .expect("replay");
            prev = third["still"].clone();
        }
        assert_eq!(third["digest"].as_str(), Some(digest.as_str()));

        let mut fast = grid(4, 0.0);
        // Flat index of (1, 1, 1) on a 4³ grid: 1 + 4 * (1 + 4 * 1) = 21.
        fast[21] = 1.0;
        let woken = sleep_mask(
            &mut world,
            &json!({
                "nx": 4, "ny": 4, "nz": 4, "k": 3, "epsilon": 0.001,
                "speeds": fast, "still": &still
            }),
        )
        .expect("wake");
        let got: Vec<u64> = woken["integrate"]
            .as_array()
            .expect("list")
            .iter()
            .map(|v| v.as_u64().expect("index"))
            .collect();
        assert_eq!(got, vec![5, 17, 20, 21, 22, 25, 37]);
    }

    #[test]
    fn k_zero_and_two_runs_match() {
        let mut world = World::new();
        let speeds = grid(4, 0.0);
        let a = sleep_mask(
            &mut world,
            &json!({ "nx": 4, "ny": 4, "nz": 4, "k": 0, "speeds": speeds }),
        )
        .expect("a");
        let b = sleep_mask(
            &mut world,
            &json!({ "nx": 4, "ny": 4, "nz": 4, "k": 0, "speeds": grid(4, 0.0) }),
        )
        .expect("b");
        assert_eq!(a["digest"], b["digest"]);
        assert_eq!(a["integrate"].as_array().map(Vec::len), Some(64));
        assert_eq!(a["asleep"], 0);
        assert_eq!(a["k"], 0);
        let defaults =
            sleep_mask(&mut world, &json!({ "nx": 1, "ny": 1, "nz": 1, "speeds": [0.0] })).expect("defaults");
        assert_eq!(defaults["k"].as_u64(), Some(u64::from(DEFAULT_QUIET_STEPS)));
        assert_eq!(defaults["epsilon"].as_f64(), Some(f64::from(DEFAULT_EPSILON)));
    }
}
