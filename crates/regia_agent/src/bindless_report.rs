// SPDX-License-Identifier: MPL-2.0
//! `render.bindless_report`: one call that says which binding path the renderer's standard
//! materials take (per-material buffers, the material constants arena, or the texture table),
//! the level that was asked for, and for every path that is off the stable reason code with a
//! sentence and a fix (ADR-1111; `book/agents/bindless.md`). It joins what
//! `render.get_capabilities` and `render.get_frame_stats` report separately, so an agent can
//! answer "why is bindless off here" with one call.

use regia_app::prelude::*;
use regia_render::{BindlessCaps, BindlessTier, RenderDevice, RuntimeRenderer, TableSupport};
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// Reason code: the material constants stay in per-material buffers because the platform keeps
/// them there until the storage path is measured (Android).
pub(crate) const REASON_MOBILE_UNMEASURED: &str = "mobile_unmeasured";
/// Reason code: the device's storage-buffer limits are below the material arena's minimum.
pub(crate) const REASON_STORAGE_LIMITS: &str = "storage_limits";

/// Everything the report reads, so the JSON is a pure function of these.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ReportInputs {
    /// The device's tier.
    pub tier: BindlessTier,
    /// The tier the adapter could reach if the renderer requested its features.
    pub adapter_tier: BindlessTier,
    /// Whether this device keeps material constants in the arena.
    pub material_ssbo: bool,
    /// Whether the platform forbids the arena (Android).
    pub android: bool,
    /// Whether the device draws through the texture table.
    pub table: TableSupport,
    /// The level `REGIA_BINDLESS_LEVEL` asks for now (the device read it at creation).
    pub requested_level: u32,
}

/// The renderer's live counters the report quotes (absent without a `RuntimeRenderer`).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct LiveCounters {
    /// Draws of the opaque pass in the last main-view frame.
    pub opaque_draws: u64,
    /// Material batches of the opaque pass (1 on the texture table path).
    pub opaque_material_batches: u64,
    /// Materials holding a slot in the arena, on the arena path.
    pub arena_live: Option<u64>,
    /// Textures holding a table index, on the table path.
    pub table_live: Option<u64>,
    /// Texture slots that fell back to the neutral view, on the table path.
    pub table_overflows: Option<u64>,
}

/// The path name of `inputs`: `texture_table`, `material_arena` or `per_material`.
fn path_name(inputs: &ReportInputs) -> &'static str {
    if inputs.table.capacity().is_some() {
        "texture_table"
    } else if inputs.material_ssbo {
        "material_arena"
    } else {
        "per_material"
    }
}

/// The level the device's renderer actually runs at: 3 on the table, 1 on the arena, 0 on
/// per-material buffers.
fn effective_level(inputs: &ReportInputs) -> u32 {
    match path_name(inputs) {
        "texture_table" => 3,
        "material_arena" => 1,
        _ => 0,
    }
}

/// The reason the material arena is off, as `(code, message, hint)`; `None` when it is on.
fn arena_reason(inputs: &ReportInputs) -> Option<(&'static str, &'static str, Option<&'static str>)> {
    if inputs.material_ssbo {
        None
    } else if inputs.android {
        Some((
            REASON_MOBILE_UNMEASURED,
            "Android keeps per-material uniform buffers until the storage path is measured on phones",
            None,
        ))
    } else {
        Some((
            REASON_STORAGE_LIMITS,
            "the device has fewer than 11 storage buffers per shader stage or a storage binding under 1 MiB",
            None,
        ))
    }
}

/// The `reasons` entry for a feature that is off.
fn reason_json(feature: &str, code: &str, message: &str, hint: Option<&str>) -> Value {
    json!({ "feature": feature, "code": code, "message": message, "hint": hint })
}

/// The report for `inputs`, with the live counters when a renderer is present.
pub(crate) fn report_json(inputs: &ReportInputs, live: Option<LiveCounters>) -> Value {
    let mut reasons = Vec::new();
    if let Some((code, message, hint)) = arena_reason(inputs) {
        reasons.push(reason_json("material_arena", code, message, hint));
    }
    if let TableSupport::Fallback(reason) = inputs.table {
        reasons.push(reason_json(
            "texture_table",
            reason.as_str(),
            reason.message(),
            reason.hint(),
        ));
    }
    if live.is_some_and(|live| live.table_overflows.unwrap_or(0) > 0) {
        reasons.push(reason_json(
            "texture_table",
            regia_render::texture_table::PROBLEM_TEXTURE_TABLE_FULL,
            "some texture slots sample their neutral view because the table is full",
            Some("share or atlas textures, or raise REGIA_TEXTURE_TABLE_VIEWS"),
        ));
    }
    let texture_table = match inputs.table {
        TableSupport::Table { capacity } => json!({
            "path": "table", "capacity": capacity, "fallback": Value::Null,
        }),
        TableSupport::Fallback(reason) => json!({
            "path": "per_material", "capacity": Value::Null, "fallback": reason.as_str(),
        }),
    };
    let live = match live {
        Some(live) => json!({
            "opaque_material_batches": live.opaque_material_batches,
            "opaque_draws": live.opaque_draws,
            "material_arena_live": live.arena_live,
            "texture_table_live": live.table_live,
            "texture_table_overflows": live.table_overflows,
        }),
        None => Value::Null,
    };
    json!({
        "path": path_name(inputs),
        "level": { "requested": inputs.requested_level, "effective": effective_level(inputs) },
        "tier": inputs.tier.as_str(),
        "adapter_tier": inputs.adapter_tier.as_str(),
        "material_arena": inputs.material_ssbo,
        "texture_table": texture_table,
        "shared_samplers": {
            "set": regia_render::MaterialSampler::ALL.map(regia_render::MaterialSampler::as_str),
            "honored": path_name(inputs) == "texture_table",
        },
        "reasons": reasons,
        "live": live,
    })
}

/// Every reason code the report can name, for the book page and its test.
#[cfg(test)]
pub(crate) fn reason_codes() -> Vec<&'static str> {
    let mut codes = vec![REASON_MOBILE_UNMEASURED, REASON_STORAGE_LIMITS];
    codes.extend(
        regia_render::TableFallback::ALL
            .iter()
            .map(|reason| reason.as_str()),
    );
    codes.push(regia_render::texture_table::PROBLEM_TEXTURE_TABLE_FULL);
    codes
}

/// `render.bindless_report`: `{ path, level: { requested, effective }, tier, adapter_tier,
/// material_arena, texture_table: { path, capacity, fallback }, reasons: [{ feature, code,
/// message, hint }], live }`. Needs a `RenderDevice`; `live` is `null` without a
/// `RuntimeRenderer`.
pub(crate) fn get(world: &mut World) -> Result<Value, RpcError> {
    let device = world.get_resource::<RenderDevice>().ok_or_else(|| {
        RpcError::not_found(
            "no RenderDevice resource in this app",
            "add regia_render::RenderPlugin and let it create the device (headless apps have none)",
        )
    })?;
    let limits = device.device.limits();
    let inputs = ReportInputs {
        tier: BindlessCaps::from_device(&device.device).tier,
        adapter_tier: BindlessCaps::from_adapter(
            device.adapter.features(),
            &limits,
            device.capabilities.backend,
        )
        .tier,
        material_ssbo: regia_render::bindless::material_ssbo(&device.device),
        android: cfg!(target_os = "android"),
        table: regia_render::texture_table::device_support_with_adapter(
            &device.device,
            device.adapter.features(),
        ),
        requested_level: regia_render::texture_table::requested_level(
            regia_core::brand::env_var(regia_render::texture_table::BINDLESS_LEVEL_ENV)
                .ok()
                .as_deref(),
        ),
    };
    let value = match world.get_resource::<RuntimeRenderer>() {
        Some(renderer) => {
            let renderer = &renderer.0;
            let opaque = renderer.draw_counts().opaque;
            let table = renderer.texture_table_stats();
            report_json(
                &inputs,
                Some(LiveCounters {
                    opaque_draws: opaque.draws,
                    opaque_material_batches: opaque.material_batches,
                    arena_live: renderer.material_arena_stats().map(|arena| arena.live),
                    table_live: table.map(|table| table.live),
                    table_overflows: table.map(|table| table.overflows),
                }),
            )
        }
        None => report_json(&inputs, None),
    };
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_render::TableFallback;

    fn inputs(tier: BindlessTier, ssbo: bool, table: TableSupport) -> ReportInputs {
        ReportInputs {
            tier,
            adapter_tier: BindlessTier::TextureArrays,
            material_ssbo: ssbo,
            android: false,
            table,
            requested_level: 1,
        }
    }

    #[test]
    fn the_table_path_has_no_reasons() {
        let report = report_json(
            &inputs(
                BindlessTier::TextureArrays,
                true,
                TableSupport::Table { capacity: 4032 },
            ),
            None,
        );
        assert_eq!(report["path"], "texture_table");
        assert_eq!(report["level"]["effective"], 3);
        assert_eq!(report["texture_table"]["capacity"], 4032);
        assert_eq!(report["reasons"].as_array().map(Vec::len), Some(0));
        assert!(report["live"].is_null());
    }

    #[test]
    fn the_arena_path_names_why_the_table_is_off() {
        let report = report_json(
            &inputs(
                BindlessTier::TextureArrays,
                true,
                TableSupport::Fallback(TableFallback::NotRequested),
            ),
            None,
        );
        assert_eq!(report["path"], "material_arena");
        assert_eq!(report["level"]["effective"], 1);
        let reasons = report["reasons"].as_array().expect("array");
        assert_eq!(reasons.len(), 1);
        assert_eq!(reasons[0]["feature"], "texture_table");
        assert_eq!(reasons[0]["code"], "not_requested");
        assert!(
            reasons[0]["hint"]
                .as_str()
                .is_some_and(|h| h.contains("REGIA_BINDLESS_LEVEL=3"))
        );
    }

    #[test]
    fn per_material_names_both_reasons() {
        let report = report_json(
            &inputs(
                BindlessTier::None,
                false,
                TableSupport::Fallback(TableFallback::UniformMaterials),
            ),
            None,
        );
        assert_eq!(report["path"], "per_material");
        assert_eq!(report["level"]["effective"], 0);
        let codes: Vec<&str> = report["reasons"]
            .as_array()
            .expect("array")
            .iter()
            .filter_map(|reason| reason["code"].as_str())
            .collect();
        assert_eq!(codes, [REASON_STORAGE_LIMITS, "uniform_materials"]);
        let mut android = inputs(
            BindlessTier::MaterialSsbo,
            false,
            TableSupport::Fallback(TableFallback::UniformMaterials),
        );
        android.android = true;
        let report = report_json(&android, None);
        assert_eq!(report["reasons"][0]["code"], REASON_MOBILE_UNMEASURED);
    }

    #[test]
    fn live_counters_and_a_full_table_show_up() {
        let live = LiveCounters {
            table_live: Some(100),
            table_overflows: Some(3),
            ..LiveCounters::default()
        };
        let report = report_json(
            &inputs(
                BindlessTier::TextureArrays,
                true,
                TableSupport::Table { capacity: 100 },
            ),
            Some(live),
        );
        assert_eq!(report["live"]["opaque_material_batches"], 0);
        assert_eq!(report["live"]["texture_table_overflows"], 3);
        assert_eq!(report["reasons"][0]["code"], "texture_table_full");
    }

    #[test]
    fn every_reason_code_is_a_protocol_code_and_unique() {
        let codes = reason_codes();
        let unique: std::collections::HashSet<_> = codes.iter().collect();
        assert_eq!(unique.len(), codes.len());
        for code in codes {
            assert!(
                code.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{code}"
            );
        }
    }

    #[test]
    fn without_a_device_it_says_what_to_add() {
        let mut world = World::new();
        assert!(get(&mut world).is_err());
    }

    #[test]
    fn a_headless_device_reports_a_known_path() {
        let Ok(device) = crate::seeing::headless_render_device() else {
            eprintln!("skipping: no GPU adapter (lavapipe) available");
            return;
        };
        let mut world = World::new();
        world.insert_resource(device);
        let got = get(&mut world).expect("report");
        assert!(
            ["texture_table", "material_arena", "per_material"].contains(&got["path"].as_str().unwrap_or("")),
            "{got}"
        );
        assert!(got["live"].is_null(), "no renderer resource");
        assert!(got["level"]["requested"].is_u64());
    }
}
