// SPDX-License-Identifier: MPL-2.0
//! Bindless step 5 on the agent protocol (`book/agents/texture-residency.md`, ADR-1110):
//! `render.texture_residency` reads the texture table's growth policy (how much of the declared
//! table each rebuild binds, and how residency swaps kept their indices,
//! [`regia_render::FrameRenderer::texture_table_stats`]) and the GPU residency feedback
//! ([`regia_render::FrameRenderer::residency_feedback_stats`]).

use regia_app::prelude::*;
use regia_render::{MaterialResidency, ResidencyFeedbackStats, RuntimeRenderer, TextureTableStats};
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// Materials listed when the call names no `limit`.
const DEFAULT_LIMIT: u64 = 16;

fn no_renderer() -> RpcError {
    RpcError::not_found(
        "no RuntimeRenderer resource in this app",
        "add regia_render::RenderPlugin (a host driving FrameRenderer itself reads \
         FrameRenderer::texture_table_stats and FrameRenderer::residency_feedback_stats)",
    )
}

/// The `table` field: the growth policy's state, `null` off the table path.
fn table_json(table: Option<TextureTableStats>) -> Value {
    match table {
        Some(t) => json!({
            "capacity": t.capacity,
            "partially_bound": t.partially_bound,
            "bound": t.bound,
            "high_water": t.high_water,
            "live": t.live,
            "overflows": t.overflows,
            "rebuilds": t.rebuilds,
            "bound_changes": t.bound_changes,
            "views_written": t.views_written,
            "in_place_swaps": t.in_place_swaps,
            "moved_swaps": t.moved_swaps,
        }),
        None => Value::Null,
    }
}

fn material_json(m: &MaterialResidency) -> Value {
    json!({
        // Keys are 64-bit hashes; a string keeps every bit in JSON clients.
        "material_key": format!("{:#018x}", m.material_key),
        "gpu_need_px": m.gpu_need_px,
        "cpu_need_px": m.cpu_need_px,
        "resident_px": m.resident_px,
        "mismatch_levels": m.mismatch_levels(),
    })
}

/// The `feedback` field, `null` while feedback is off; at most `limit` materials.
fn feedback_json(feedback: Option<&ResidencyFeedbackStats>, limit: usize) -> Value {
    match feedback {
        Some(f) => json!({
            "mode": f.mode.as_str(),
            "copies": f.copies,
            "skipped_copies": f.skipped_copies,
            "readouts": f.readouts,
            "superseded": f.superseded,
            "map_failures": f.map_failures,
            "stale_dropped": f.stale_dropped,
            "last_latency": f.last_latency,
            "textures_reported": f.textures_reported,
            "materials_reported": f.materials_reported,
            "materials_driven": f.materials_driven,
            "materials": f.materials.iter().take(limit).map(material_json).collect::<Vec<_>>(),
        }),
        None => Value::Null,
    }
}

/// The optional `limit` (1-64 materials, default 16).
fn limit_param(params: &Value) -> Result<usize, RpcError> {
    let Some(value) = params.get("limit") else {
        return Ok(DEFAULT_LIMIT as usize);
    };
    let limit = value
        .as_u64()
        .filter(|n| (1..=regia_render::residency_feedback::REPORT_MATERIALS as u64).contains(n))
        .ok_or_else(|| RpcError::invalid_params("`limit` must be a whole number from 1 to 64"))?;
    Ok(limit as usize)
}

/// `render.texture_residency {limit?}`: `{ table: { capacity, partially_bound, bound, high_water,
/// live, overflows, rebuilds, bound_changes, views_written, in_place_swaps, moved_swaps } | null,
/// feedback: { mode, copies, skipped_copies, readouts, superseded, map_failures, stale_dropped,
/// last_latency, textures_reported, materials_reported, materials_driven, materials: [{
/// material_key, gpu_need_px, cpu_need_px, resident_px, mismatch_levels }] } | null }`.
pub(crate) fn get(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let limit = limit_param(params)?;
    let renderer = world.get_resource::<RuntimeRenderer>().ok_or_else(no_renderer)?;
    let feedback = renderer.0.residency_feedback_stats();
    Ok(json!({
        "table": table_json(renderer.0.texture_table_stats()),
        "feedback": feedback_json(feedback.as_ref(), limit),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_a_renderer_it_says_what_to_add() {
        let mut world = World::new();
        let err = get(&mut world, &json!({})).expect_err("no renderer");
        assert!(err.message.contains("RuntimeRenderer"), "{err:?}");
    }

    #[test]
    fn off_the_table_both_blocks_are_null() {
        let mut world = World::new();
        world.insert_resource(RuntimeRenderer(regia_render::FrameRenderer::new()));
        let got = get(&mut world, &json!({})).expect("a renderer");
        assert!(got["table"].is_null(), "{got}");
        assert!(got["feedback"].is_null(), "{got}");
    }

    #[test]
    fn the_table_block_reports_the_growth_policy() {
        let mut table = TextureTableStats::default();
        table.capacity = 4032;
        table.partially_bound = true;
        table.bound = 512;
        table.high_water = 300;
        table.live = 296;
        table.rebuilds = 3;
        table.bound_changes = 1;
        table.views_written = 256 + 512 + 512;
        table.in_place_swaps = 9;
        let got = table_json(Some(table));
        assert_eq!(got["capacity"], 4032);
        assert_eq!(got["partially_bound"], true);
        assert_eq!(got["bound"], 512);
        assert_eq!(got["high_water"], 300);
        assert_eq!(got["views_written"], 1280);
        assert_eq!(got["in_place_swaps"], 9);
        assert_eq!(got["moved_swaps"], 0);
        assert!(table_json(None).is_null());
    }

    #[test]
    fn the_feedback_block_lists_materials_up_to_the_limit() {
        let stats = ResidencyFeedbackStats::default();
        let got = feedback_json(Some(&stats), 16);
        assert_eq!(got["mode"], "off");
        assert_eq!(got["readouts"], 0);
        assert_eq!(got["materials"].as_array().map(Vec::len), Some(0));
        assert!(feedback_json(None, 16).is_null());
    }

    #[test]
    fn limit_is_checked() {
        assert_eq!(limit_param(&json!({})).ok(), Some(16));
        assert_eq!(limit_param(&json!({ "limit": 3 })).ok(), Some(3));
        assert_eq!(limit_param(&json!({ "limit": 64 })).ok(), Some(64));
        for bad in [json!(0), json!(65), json!(-1), json!("8"), json!(2.5)] {
            assert!(limit_param(&json!({ "limit": bad })).is_err(), "{bad}");
        }
    }
}
