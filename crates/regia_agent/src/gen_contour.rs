// SPDX-License-Identifier: MPL-2.0
//! `gen.contour` (`book/agents/gen-mesh.md`, ADR-8044): marching-squares polylines of a scalar
//! grid on the XZ plane. Pure: needs no world, no renderer and no project. The same grid and
//! levels give the same points on every platform.
//!
//! Errors carry a lowercase `data.code`: `gen_invalid_recipe` (the grid or the levels do not
//! parse), `gen_invalid_input` (a non-finite value, a bad cell, an empty level list),
//! `gen_too_large` (a grid side, the level count, the smooth passes or the point count).

use regia_gen::{ContourOptions, ScalarGrid, contours};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, RpcError};

const HINT: &str = "see agents/gen-mesh in the book for the contour grid, levels and limits";

fn coded(code: &str, message: impl Into<String>) -> RpcError {
    RpcError::new(INVALID_PARAMS, code, message, HINT)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GridIn {
    origin: [f64; 2],
    cell: f64,
    width: u32,
    height: u32,
    samples: Vec<f64>,
}

/// `gen.contour { grid, levels, smooth?, min_length? }`.
pub(crate) fn gen_contour(params: &Value) -> Result<Value, RpcError> {
    let Some(raw_grid) = params.get("grid") else {
        return Err(RpcError::invalid_params("`grid` is required"));
    };
    if !raw_grid.is_object() {
        return Err(RpcError::wrong_type("grid", "an object"));
    }
    let Some(raw_levels) = params.get("levels") else {
        return Err(RpcError::invalid_params("`levels` is required"));
    };
    if !raw_levels.is_array() {
        return Err(RpcError::wrong_type("levels", "an array"));
    }
    let grid_in: GridIn = serde_json::from_value(raw_grid.clone())
        .map_err(|e| coded("gen_invalid_recipe", format!("gen.contour: grid: {e}")))?;
    let levels_in: Vec<f64> = serde_json::from_value(raw_levels.clone())
        .map_err(|e| coded("gen_invalid_recipe", format!("gen.contour: levels: {e}")))?;
    let smooth = crate::params::opt_u64(params, "smooth")?.unwrap_or(2);
    let min_length = crate::params::opt_f64(params, "min_length")?.unwrap_or(0.0);
    // A JSON integer above `u32::MAX` cannot be a pass count; the library then reports
    // `gen_too_large` because the count is above 8.
    let smooth_passes = u32::try_from(smooth).unwrap_or(u32::MAX);
    let found = contours(
        &ScalarGrid {
            origin: [f32_from_json(grid_in.origin[0]), f32_from_json(grid_in.origin[1])],
            cell: f32_from_json(grid_in.cell),
            width: grid_in.width,
            height: grid_in.height,
            samples: grid_in.samples.iter().copied().map(f32_from_json).collect(),
        },
        &levels_in.iter().copied().map(f32_from_json).collect::<Vec<_>>(),
        &ContourOptions {
            smooth_passes,
            min_length: f32_from_json(min_length),
        },
    )
    .map_err(|e| coded(e.code(), format!("gen.contour: {e}")))?;
    Ok(json!({
        "count": found.len(),
        "contours": found.iter().map(|c| json!({
            "level": c.level,
            "closed": c.closed,
            "points": c.points,
        })).collect::<Vec<_>>(),
    }))
}

/// JSON numbers are `f64`. The contour grid is `f32`, the same width as the rest of `regia_gen`.
fn f32_from_json(value: f64) -> f32 {
    value as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::{assert_listed, assert_result_named};

    fn code(err: &RpcError) -> String {
        err.data
            .as_ref()
            .and_then(|d| d["code"].as_str())
            .unwrap_or("")
            .to_string()
    }

    fn step() -> Value {
        json!({
            "grid": {
                "origin": [0.0, 0.0],
                "cell": 1.0,
                "width": 1,
                "height": 1,
                "samples": [0.0, 0.0, 1.0, 1.0]
            },
            "levels": [0.5],
            "smooth": 0
        })
    }

    #[test]
    fn a_step_returns_one_open_contour() {
        let out = gen_contour(&step()).expect("contour");
        assert_result_named("gen.contour", &out);
        assert_eq!(out["count"], 1);
        assert_eq!(out["contours"][0]["closed"], false);
        assert_eq!(out["contours"][0]["level"], 0.5);
        assert!(
            out["contours"][0]["points"]
                .as_array()
                .is_some_and(|p| p.len() >= 2)
        );
    }

    #[test]
    fn missing_fields_are_f0002_and_bad_values_use_the_gen_codes() {
        assert_eq!(code(&gen_contour(&json!({})).expect_err("empty")), "F0002");
        assert_eq!(
            code(&gen_contour(&json!({ "grid": {} })).expect_err("no levels")),
            "F0002"
        );
        let extra = json!({
            "grid": {
                "origin": [0.0, 0.0], "cell": 1.0, "width": 1, "height": 1,
                "samples": [0.0, 0.0, 1.0, 1.0], "extra": 1
            },
            "levels": [0.5]
        });
        assert_eq!(
            code(&gen_contour(&extra).expect_err("extra")),
            "gen_invalid_recipe"
        );
        assert_listed("gen.contour", "gen_invalid_recipe");
        let mut zero_cell = step();
        zero_cell["grid"]["cell"] = json!(0.0);
        assert_eq!(
            code(&gen_contour(&zero_cell).expect_err("cell")),
            "gen_invalid_input"
        );
        assert_listed("gen.contour", "gen_invalid_input");
        let mut too_smooth = step();
        too_smooth["smooth"] = json!(9);
        assert_eq!(
            code(&gen_contour(&too_smooth).expect_err("smooth")),
            "gen_too_large"
        );
        assert_listed("gen.contour", "gen_too_large");
    }
}
