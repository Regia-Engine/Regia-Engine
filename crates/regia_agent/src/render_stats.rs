// SPDX-License-Identifier: MPL-2.0
//! Draw-call and binding counters and the bindless capability tier on the agent protocol
//! (bindless step 1, ADR-316; `book/agents/renderer-excellence.md` § Bindless step 1):
//! `render.get_frame_stats` reads the runtime renderer's per-pass counters
//! ([`regia_render::FrameRenderer::draw_counts`]), its material constants arena and, on the
//! texture table path (bindless level 3, ADR-863), its texture table; `render.get_capabilities`
//! reports the device's capabilities with its [`BindlessCaps`] tier and whether it draws through
//! the texture table (or the fallback code that keeps it off).

use regia_app::prelude::*;
use regia_render::{
    BindlessCaps, FrameDrawCounts, PassDrawCounts, RenderDevice, RuntimeRenderer, TableSupport,
    TextureTableStats,
};
use serde_json::{Value, json};

use crate::protocol::RpcError;

fn no_renderer() -> RpcError {
    RpcError::not_found(
        "no RuntimeRenderer resource in this app",
        "add regia_render::RenderPlugin (a host driving FrameRenderer itself reads \
         FrameRenderer::draw_counts)",
    )
}

fn no_device() -> RpcError {
    RpcError::not_found(
        "no RenderDevice resource in this app",
        "add regia_render::RenderPlugin and let it create the device (headless apps have none)",
    )
}

fn pass_json(pass: &PassDrawCounts) -> Value {
    json!({
        "draws": pass.draws,
        "pipeline_switches": pass.pipeline_switches,
        "bind_group_switches": pass.bind_group_switches,
        "bind_group_switches_total": pass.bind_group_switches_total(),
        "material_batches": pass.material_batches,
    })
}

/// The `texture_table` field of `render.get_frame_stats`: `null` off the table path.
fn texture_table_json(table: Option<TextureTableStats>) -> Value {
    match table {
        Some(table) => json!({
            "capacity": table.capacity,
            "live": table.live,
            "overflows": table.overflows,
            "rebuilds": table.rebuilds,
            "refreshes": table.refreshes,
            "last_rebuild_us": table.last_rebuild_us,
            "record_groups": table.record_groups,
        }),
        None => Value::Null,
    }
}

/// The `render.get_frame_stats` result for `counts`, the arena state and the texture table.
fn frame_stats_json(
    frame: u64,
    counts: &FrameDrawCounts,
    arena: Option<regia_render::MaterialArenaStats>,
    table: Option<TextureTableStats>,
) -> Value {
    let mut passes = serde_json::Map::new();
    for (name, pass) in counts.passes() {
        passes.insert(name.to_string(), pass_json(&pass));
    }
    json!({
        "frame": frame,
        "passes": passes,
        "total": pass_json(&counts.total()),
        "material_constants": match arena {
            Some(arena) => json!({
                "path": "storage_arena",
                "live": arena.live,
                "capacity": arena.capacity,
                "stride": arena.stride,
                "buffer_creates": arena.buffer_creates,
                "epoch": arena.epoch,
            }),
            None => json!({ "path": "per_material" }),
        },
        "texture_table": texture_table_json(table),
    })
}

/// `render.get_frame_stats`: `{ frame, passes: { shadow, depth_prepass, visbuffer_resolve,
/// opaque, translucent }, total, material_constants, texture_table }` of the runtime renderer's
/// last main-view frame. Each pass is `{ draws, pipeline_switches, bind_group_switches: [g0, g1,
/// g2, g3], bind_group_switches_total, material_batches }`; `texture_table` is `null` off the
/// texture table path.
pub(crate) fn get_frame_stats(world: &mut World) -> Result<Value, RpcError> {
    let renderer = world.get_resource::<RuntimeRenderer>().ok_or_else(no_renderer)?;
    let renderer = &renderer.0;
    Ok(frame_stats_json(
        renderer.frame_index(),
        &renderer.draw_counts(),
        renderer.material_arena_stats(),
        renderer.texture_table_stats(),
    ))
}

fn bindless_json(caps: &BindlessCaps) -> Value {
    json!({
        "tier": caps.tier.as_str(),
        "binding_arrays": caps.binding_arrays,
        "non_uniform_indexing": caps.non_uniform_indexing,
        "partially_bound": caps.partially_bound,
        "max_binding_array_elements": caps.max_binding_array_elements,
        "storage_buffers_per_stage": caps.storage_buffers_per_stage,
        "max_storage_buffer_binding_size": caps.max_storage_buffer_binding_size,
        "storage_offset_alignment": caps.storage_offset_alignment,
    })
}

/// The `texture_table` field of `render.get_capabilities`' `bindless`: `{ path: "table",
/// capacity, fallback: null }` or `{ path: "per_material", capacity: null, fallback: code }`.
fn table_support_json(support: TableSupport) -> Value {
    match support {
        TableSupport::Table { capacity } => json!({
            "path": "table",
            "capacity": capacity,
            "fallback": Value::Null,
        }),
        TableSupport::Fallback(reason) => json!({
            "path": "per_material",
            "capacity": Value::Null,
            "fallback": reason.as_str(),
        }),
    }
}

/// `render.get_capabilities`: `{ adapter, backend, features: {...}, limits: {...},
/// fallbacks: [code], bindless: { tier, adapter_tier, material_path, texture_table, binding_arrays,
/// non_uniform_indexing, partially_bound, max_binding_array_elements, storage_buffers_per_stage,
/// max_storage_buffer_binding_size, storage_offset_alignment }, texture_paths: { bc, astc, etc2 },
/// material_anisotropy_lod_offset }` of the render device (`texture_paths`: `native`, `cpu_decode` or
/// `unbound`, ADR-2063; `material_anisotropy_lod_offset`: the mips the material sampler adds to the
/// texture bias after its per-device calibration, 0.25 on Mesa RADV, 0 on NVIDIA and lavapipe, null
/// before a renderer built its pipelines, ADR-5670).
pub(crate) fn get_capabilities(world: &mut World) -> Result<Value, RpcError> {
    let device = world.get_resource::<RenderDevice>().ok_or_else(no_device)?;
    let caps = &device.capabilities;
    let bindless = BindlessCaps::from_device(&device.device);
    // What the adapter could do once the renderer requests the binding-array features (step 2).
    let adapter_tier =
        BindlessCaps::from_adapter(device.adapter.features(), &device.device.limits(), caps.backend).tier;
    let mut bindless_value = bindless_json(&bindless);
    bindless_value["adapter_tier"] = json!(adapter_tier.as_str());
    // The path this device's renderer takes (Android keeps per-material buffers on every tier).
    bindless_value["material_path"] = json!(if regia_render::bindless::material_ssbo(&device.device) {
        "storage_arena"
    } else {
        "per_material"
    });
    // The texture table (bindless level 3, ADR-863): on, or the first requirement that keeps it
    // off (`not_requested` when the adapter could carry it but `REGIA_BINDLESS_LEVEL` is below 3).
    bindless_value["texture_table"] = table_support_json(
        regia_render::texture_table::device_support_with_adapter(&device.device, device.adapter.features()),
    );
    let fallbacks: Vec<&str> = caps.fallback_notes().iter().map(|note| note.code).collect();
    // How each cooked block family loads on this device (ADR-2063): native upload, CPU decode to
    // RGBA8, or unbound.
    let texture_paths: serde_json::Map<String, Value> =
        regia_render::compressed_texture::family_paths(device.device.features())
            .into_iter()
            .map(|(family, path)| (family.to_string(), json!(path.label())))
            .collect();
    Ok(json!({
        "adapter": caps.adapter_name,
        "backend": format!("{:?}", caps.backend),
        "features": {
            "timestamp_query": caps.timestamp_query,
            "multi_draw_indirect": caps.multi_draw_indirect,
            "multi_draw_indirect_count": caps.multi_draw_indirect_count,
            "indirect_first_instance": caps.indirect_first_instance,
            "rg11b10_renderable": caps.rg11b10_renderable,
            "texture_compression_bc": caps.texture_compression_bc,
            "texture_compression_astc": caps.texture_compression_astc,
            "texture_compression_etc2": caps.texture_compression_etc2,
            "early_depth_test": caps.early_depth_test,
            "float32_filterable": caps.float32_filterable,
        },
        "limits": {
            "max_storage_buffer_binding_size": caps.max_storage_buffer_binding_size,
            "max_storage_buffers_per_shader_stage": caps.max_storage_buffers_per_shader_stage,
            "max_storage_textures_per_shader_stage": caps.max_storage_textures_per_shader_stage,
            "max_uniform_buffers_per_shader_stage": caps.max_uniform_buffers_per_shader_stage,
            "max_compute_workgroup_storage_size": caps.max_compute_workgroup_storage_size,
            "max_bind_groups": caps.max_bind_groups,
            "max_texture_dimension_2d": caps.max_texture_dimension_2d,
        },
        "fallbacks": fallbacks,
        "bindless": bindless_value,
        "texture_paths": texture_paths,
        "upscalers": crate::upscalers::capability_rows(world),
        "material_anisotropy_lod_offset": regia_render::last_material_anisotropy_lod_offset(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_render::FrameRenderer;

    #[test]
    fn frame_stats_name_every_pass_before_the_first_frame() {
        let mut world = World::new();
        world.insert_resource(RuntimeRenderer(FrameRenderer::new()));
        let got = get_frame_stats(&mut world).expect("read");
        for pass in [
            "shadow",
            "depth_prepass",
            "visbuffer_resolve",
            "opaque",
            "translucent",
        ] {
            assert_eq!(got["passes"][pass]["draws"], 0, "{pass}: {got}");
            assert_eq!(
                got["passes"][pass]["bind_group_switches"]
                    .as_array()
                    .map(Vec::len),
                Some(4)
            );
        }
        assert_eq!(got["total"]["material_batches"], 0);
        assert_eq!(got["material_constants"]["path"], "per_material");
    }

    #[test]
    fn frame_stats_json_totals_the_passes_and_reports_the_arena() {
        let pass = PassDrawCounts {
            draws: 5,
            pipeline_switches: 2,
            bind_group_switches: [1, 5, 3, 0],
            material_batches: 3,
        };
        let counts = FrameDrawCounts {
            opaque: pass,
            shadow: pass,
            ..FrameDrawCounts::default()
        };
        let arena = regia_render::MaterialArenaStats {
            live: 12,
            capacity: 256,
            stride: 352,
            buffer_creates: 1,
            epoch: 0,
        };
        let got = frame_stats_json(9, &counts, Some(arena), None);
        assert_eq!(got["frame"], 9);
        assert_eq!(got["passes"]["opaque"]["draws"], 5);
        assert_eq!(got["passes"]["opaque"]["bind_group_switches_total"], 9);
        assert_eq!(got["total"]["draws"], 10);
        assert_eq!(got["total"]["bind_group_switches"], json!([2, 10, 6, 0]));
        assert_eq!(got["material_constants"]["path"], "storage_arena");
        assert_eq!(got["material_constants"]["live"], 12);
        assert_eq!(got["material_constants"]["stride"], 352);
        assert!(got["texture_table"].is_null(), "off the table path: {got}");
    }

    #[test]
    fn frame_stats_json_reports_the_texture_table() {
        let mut table = TextureTableStats::default();
        assert_eq!(table.record_groups, 0, "a default table has built nothing");
        table.capacity = 4096;
        table.live = 37;
        table.rebuilds = 2;
        table.refreshes = 120;
        table.record_groups = 1;
        let got = frame_stats_json(3, &FrameDrawCounts::default(), None, Some(table));
        assert_eq!(got["texture_table"]["capacity"], 4096);
        assert_eq!(got["texture_table"]["live"], 37);
        assert_eq!(got["texture_table"]["overflows"], 0);
        assert_eq!(got["texture_table"]["rebuilds"], 2);
        assert_eq!(got["texture_table"]["refreshes"], 120);
        assert_eq!(got["texture_table"]["record_groups"], 1);
        assert!(got["texture_table"]["last_rebuild_us"].is_u64());
    }

    #[test]
    fn table_support_names_the_path_and_the_fallback_code() {
        let on = table_support_json(TableSupport::Table { capacity: 4032 });
        assert_eq!(on["path"], "table");
        assert_eq!(on["capacity"], 4032);
        assert!(on["fallback"].is_null());
        let off = table_support_json(TableSupport::Fallback(regia_render::TableFallback::NotRequested));
        assert_eq!(off["path"], "per_material");
        assert!(off["capacity"].is_null());
        assert_eq!(off["fallback"], "not_requested");
    }

    #[test]
    fn without_a_renderer_or_device_it_says_what_to_add() {
        let mut world = World::new();
        let err = get_frame_stats(&mut world).expect_err("no renderer");
        assert!(format!("{err:?}").contains("RenderPlugin"));
        let err = get_capabilities(&mut world).expect_err("no device");
        assert!(format!("{err:?}").contains("RenderPlugin"));
    }

    #[test]
    fn capabilities_report_the_bindless_tier() {
        let Ok(device) = crate::seeing::headless_render_device() else {
            eprintln!("skipping: no GPU adapter (lavapipe) available");
            return;
        };
        let mut world = World::new();
        world.insert_resource(device);
        let got = get_capabilities(&mut world).expect("read");
        let tier = got["bindless"]["tier"].as_str().unwrap_or("");
        assert!(
            ["none", "material_ssbo", "texture_arrays"].contains(&tier),
            "{got}"
        );
        assert!(got["bindless"]["adapter_tier"].is_string());
        let table_path = got["bindless"]["texture_table"]["path"].as_str().unwrap_or("");
        assert!(["table", "per_material"].contains(&table_path), "{got}");
        assert!(
            ["storage_arena", "per_material"]
                .contains(&got["bindless"]["material_path"].as_str().unwrap_or(""))
        );
        assert!(got["limits"]["max_storage_buffers_per_shader_stage"].is_u64());
        assert!(got["fallbacks"].is_array());
        let xess = got["upscalers"]
            .as_array()
            .expect("upscaler capability rows")
            .iter()
            .find(|row| row["backend"] == "xess")
            .expect("XeSS capability row");
        assert!(xess["runtime"]["binding_built"].is_boolean());
        assert!(xess["runtime"]["sdk_version"].is_string());
        if !regia_render::upscaler::vendor::xess::BINDING_BUILT {
            assert_eq!(xess["available"], false);
            assert_eq!(xess["runtime"]["state"], "not_bundled");
        }
    }
}
