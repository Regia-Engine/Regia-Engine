// SPDX-License-Identifier: MPL-2.0
//! `level.get_baked_gi`: the open level's baked GI volume and what the framework did with it
//! (`book/agents/baked-gi-maps.md`, ADR-283). The level names its `.fgiv` in
//! `LevelSettings.look.baked_gi`; `regia_framework::baked_gi_level` loads and installs it.

use regia_app::prelude::*;
use regia_framework::baked_gi_level::LevelBakedGiState;
use regia_framework::baked_gi_stream::BakedGiStreamState;
use regia_render::RuntimeRenderer;
use regia_render::baked_gi::memory::volumes_report;
use regia_scene::level::LevelSettings;
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// `level.get_baked_gi`: what the level names, whether the renderer holds it, and why not.
///
/// `source` is `"level"` when the volume in the renderer is the one the level installed,
/// `"host"` when the host installed another one, `"none"` when no volume is installed. `parts` is
/// the split set the level named (`count`, `lit`, and each part that did not load), `host_layers`
/// the layers in the renderer that are neither its parts nor a streamed level's, and `memory` what
/// the volumes in the renderer take against every tier's budget (ADR-3006).
pub(crate) fn get_baked_gi(world: &mut World) -> Result<Value, RpcError> {
    let named = world
        .get_resource::<LevelSettings>()
        .and_then(|l| l.look.baked_gi.as_ref())
        .and_then(|b| serde_json::to_value(b).ok());
    let state = world.get_resource::<LevelBakedGiState>();
    let renderer = world.get_resource::<RuntimeRenderer>().map(|r| &r.0);
    let installed = renderer.and_then(|r| r.baked_gi());
    let source = match (installed, state) {
        (None, _) => "none",
        (Some(_), Some(s)) if s.owns(installed) => "level",
        (Some(_), _) => "host",
    };
    let status = renderer.map(|r| {
        let s = r.baked_gi_status();
        json!({
            "installed": s.installed,
            "active": s.active,
            "probes": s.probes,
            "gpu_bytes": s.gpu_bytes,
            "error": s.error,
        })
    });
    let mut out = json!({
        "plugin": state.is_some(),
        "level": named,
        "volume": state.and_then(|s| s.volume.clone()),
        "installed_by_level": state.is_some_and(|s| s.installed),
        "source": source,
        "error": state.and_then(|s| s.error.clone()),
        "error_code": state.and_then(|s| s.error_code),
        "probes": state.map_or(0, |s| s.probes),
        "renderer": status,
    });
    // The bake input against its stamp (book/agents/baked-gi-export.md).
    let bake_input = crate::bake_export::bake_input_status(world);
    out["bake_input_stale"] = bake_input.as_ref().map_or(Value::Null, |b| b["stale"].clone());
    out["bake_input_hash"] = bake_input.as_ref().map_or(Value::Null, |b| b["hash"].clone());
    out["bake_input"] = bake_input.unwrap_or(Value::Null);
    // ADR-2656: the installed lightmap and its receivers (book/agents/lightmaps.md).
    out["lightmaps"] = crate::lightmaps::gi_summary(world);
    // ADR-3006: what the set lights, what failed, whose layers these are, and the bytes per tier.
    out["parts"] = parts_json(world);
    out["host_layers"] = json!(host_layer_count(world));
    out["memory"] = memory_json(world);
    Ok(out)
}

/// The split set the level named: how many parts it has, how many are lit, the ones that did not
/// load with their codes, and per lit part its bricks and how many are crops of a brick that crosses
/// a cut (`installed`, ADR-3237). `null` before a volume was read.
fn parts_json(world: &World) -> Value {
    let Some(state) = world
        .get_resource::<LevelBakedGiState>()
        .filter(|s| s.part_count > 0)
    else {
        return Value::Null;
    };
    let failed: Vec<Value> = state
        .failed_parts
        .iter()
        .map(|f| json!({ "index": f.index, "path": f.path, "code": f.code, "error": f.error }))
        .collect();
    let installed: Vec<Value> = state
        .installed_volumes()
        .filter_map(|v| {
            let split = v.split.as_ref()?;
            let layout = v.layout();
            Some(json!({
                "index": split.index,
                "bytes": layout.runtime_buffer_bytes(),
                "bricks": layout.bricks.len(),
                "cropped_bricks": split.cropped_bricks(&layout),
            }))
        })
        .collect();
    json!({
        "count": state.part_count,
        "lit": state.installed_volumes().count(),
        "failed": failed,
        "installed": installed,
    })
}

/// Renderer layers that are neither the level's parts nor a streamed level's: the host's.
fn host_layer_count(world: &World) -> usize {
    let Some(renderer) = world.get_resource::<RuntimeRenderer>() else {
        return 0;
    };
    let state = world.get_resource::<LevelBakedGiState>();
    let stream = world.get_resource::<BakedGiStreamState>();
    renderer
        .0
        .baked_gi_layers()
        .iter()
        .filter(|l| {
            !state.is_some_and(|s| s.holds_part(&l.volume))
                && !stream.is_some_and(|s| s.holds_volume(&l.volume))
        })
        .count()
}

/// The memory answer for every volume the renderer holds: the installed one and its layers, each
/// in its own buffer. `null` when it holds none.
fn memory_json(world: &World) -> Value {
    let Some(renderer) = world.get_resource::<RuntimeRenderer>() else {
        return Value::Null;
    };
    let mut volumes = Vec::new();
    if let Some(volume) = renderer.0.baked_gi() {
        volumes.push(("volume".to_string(), volume.layout()));
    }
    for (k, layer) in renderer.0.baked_gi_layers().iter().enumerate() {
        volumes.push((format!("layer {k}"), layer.volume.layout()));
    }
    if volumes.is_empty() {
        return Value::Null;
    }
    serde_json::to_value(volumes_report(&volumes)).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_nothing_in_a_bare_world() {
        let mut world = World::new();
        let got = get_baked_gi(&mut world).expect("answers");
        assert_eq!(got["source"], "none");
        assert_eq!(got["plugin"], false);
        assert!(got["level"].is_null());
        assert!(got["renderer"].is_null());
        assert_eq!(got["lightmaps"]["installed"], false);
        assert_eq!(got["lightmaps"]["instances"], 0);
    }

    #[test]
    fn a_level_without_a_plugin_is_reported_as_named_only() {
        let mut world = World::new();
        let mut settings = LevelSettings::default();
        settings.look.baked_gi = Some(regia_scene::level::LevelBakedGi {
            volume: "maps/a.fgiv".to_string(),
            ..Default::default()
        });
        world.insert_resource(settings);
        world.insert_resource(RuntimeRenderer::default());
        let got = get_baked_gi(&mut world).expect("answers");
        assert_eq!(got["level"]["volume"], "maps/a.fgiv");
        assert_eq!(got["source"], "none");
        assert_eq!(got["installed_by_level"], false);
        assert_eq!(got["renderer"]["installed"], false);
    }

    #[test]
    fn a_host_volume_has_source_host() {
        let mut world = World::new();
        world.insert_resource(LevelBakedGiState::default());
        let mut renderer = RuntimeRenderer::default();
        renderer.0.set_baked_gi(Some(std::sync::Arc::new(host_volume())));
        world.insert_resource(renderer);
        let got = get_baked_gi(&mut world).expect("answers");
        assert_eq!(got["source"], "host");
        assert_eq!(got["renderer"]["installed"], true);
    }

    /// A renderer's volumes are answered per tier, and a host's layer is counted as the host's.
    #[test]
    fn the_memory_answer_covers_what_the_renderer_holds() {
        let mut world = World::new();
        world.insert_resource(LevelBakedGiState::default());
        let mut renderer = RuntimeRenderer::default();
        renderer.0.set_baked_gi(Some(std::sync::Arc::new(host_volume())));
        renderer
            .0
            .set_baked_gi_layers(&[regia_render::baked_gi::BakedGiLayer::new(std::sync::Arc::new(
                host_volume(),
            ))]);
        world.insert_resource(renderer);
        let got = get_baked_gi(&mut world).expect("answers");
        assert_eq!(got["host_layers"], 1, "{got}");
        assert!(got["parts"].is_null(), "the level read nothing: {got}");
        let memory = &got["memory"];
        assert_eq!(memory["parts"].as_array().map(Vec::len), Some(2), "{memory}");
        let tiers = memory["tiers"].as_array().expect("tiers");
        // One entry per budget tier (ADR-8878 appended `integrated_low`).
        assert_eq!(tiers.len(), regia_render::BudgetTier::ALL.len(), "{memory}");
        for tier in tiers {
            assert_eq!(tier["fits"], true, "{tier}");
            assert_eq!(tier["bytes"], memory["bytes"]);
        }
        assert_eq!(tiers[1]["tier"], "deck");
        assert_eq!(tiers[1]["budget"], 384u64 << 20);
        assert_eq!(tiers[3]["tier"], "integrated_low");
        assert_eq!(tiers[3]["budget"], 192u64 << 20);
    }

    /// A part that did not load is named with its stable code (ADR-3006).
    #[test]
    fn a_failed_part_is_reported_with_its_code() {
        let mut world = World::new();
        let mut state = LevelBakedGiState::default();
        state.part_count = 3;
        state.error_code = Some("volume_not_found");
        state
            .failed_parts
            .push(regia_framework::baked_gi_level::FailedPart {
                index: 2,
                path: "maps/a.part2.fgiv".to_string(),
                error: "maps/a.part2.fgiv: file not found".to_string(),
                code: "volume_not_found",
            });
        world.insert_resource(state);
        let got = get_baked_gi(&mut world).expect("answers");
        assert_eq!(got["error_code"], "volume_not_found");
        assert_eq!(got["parts"]["count"], 3);
        assert_eq!(got["parts"]["lit"], 0);
        assert_eq!(got["parts"]["failed"][0]["index"], 2);
        assert_eq!(got["parts"]["failed"][0]["path"], "maps/a.part2.fgiv");
        assert_eq!(got["parts"]["failed"][0]["code"], "volume_not_found");
    }

    /// ADR-3237: the report names each lit part's bricks and how many are crops of a brick that
    /// crosses a cut, through the real level plugin and a written cropped split.
    #[test]
    fn the_parts_report_counts_cropped_bricks_per_part() {
        use regia_asset::baked_gi::{
            BakeInfo, BakeLights, BakedGiVolume, DetailBrick, GI_COMPONENTS, ProbeData, ProbeFlags,
            ProbeMeta, TerrainLayer, VolumeLayout,
        };
        use regia_asset::{AssetServer, AssetServerSettings};
        use regia_core::id::Guid;
        use regia_framework::baked_gi_level::BakedGiLevelPlugin;
        use regia_scene::level::{Level, LevelId, LevelLibrary, LoadMode, open_level};
        use regia_scene::prelude::ScenePlugin;

        let root = regia_test_utils::unique_temp_dir("agent-gi-cropped");
        let maps = root.join("assets").join("maps");
        std::fs::create_dir_all(&maps).expect("maps dir");
        let mut layout = VolumeLayout {
            terrain: Some(TerrainLayer {
                origin: [0.0, 0.0],
                spacing: [4.0, 4.0],
                counts: [24, 4],
                levels: vec![1.0],
                ground: vec![0.0; 96],
            }),
            bricks: vec![DetailBrick {
                origin: [2.0, 0.0, 2.0],
                spacing: [1.0; 3],
                counts: [85, 6, 6],
                first_probe: 0,
                flags: 0,
            }],
            moment_resolution: 4,
            max_distance: 16.0,
        };
        layout.assign_brick_offsets();
        let probes: Vec<ProbeData> = (0..layout.probe_count())
            .map(|_| ProbeData {
                sh: [[[0.5, 0.0, 0.0, 0.0]; 3]; GI_COMPONENTS],
                moments: vec![[16.0, 256.0]; 16],
                meta: ProbeMeta {
                    offset: [0.0; 3],
                    flags: ProbeFlags::VALID,
                },
            })
            .collect();
        let whole = BakedGiVolume::from_probes(&layout, BakeLights::default(), BakeInfo::default(), &probes)
            .expect("volume");
        let parts = whole
            .split_parts(layout.bricks[0].probe_count() as u64 * 136 * 7 / 10)
            .expect("cropped split");
        regia_asset::baked_gi_split::write_split_set(&parts, &maps.join("a.fgiv")).expect("write parts");

        let mut app = App::new();
        app.add_plugins((ScenePlugin, BakedGiLevelPlugin));
        app.insert_resource(
            AssetServer::open(AssetServerSettings {
                project_root: root.clone(),
                platform: "linux".to_string(),
                content_dir: None,
            })
            .expect("asset server"),
        );
        app.insert_resource(RuntimeRenderer::default());
        let id = LevelId(Guid::new_random());
        let text = r#"SceneFile(
    format_version: 10,
    entities: [],
    resources: {
        "regia_scene::level::LevelSettings": (look: (baked_gi: Some((volume: "maps/a.fgiv")))),
    },
)"#;
        let level = Level::from_ron_str(id, text).expect("level parses");
        app.world_mut().resource_mut::<LevelLibrary>().insert(level);
        open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
        app.update();

        let got = get_baked_gi(app.world_mut()).expect("answers");
        let installed = got["parts"]["installed"].as_array().expect("installed parts");
        assert_eq!(installed.len(), parts.len(), "{got}");
        let mut cropped = 0;
        for (entry, part) in installed.iter().zip(&parts) {
            let split = part.split.as_ref().expect("split part");
            assert_eq!(entry["index"], split.index);
            assert_eq!(entry["bricks"], part.bricks.len());
            assert_eq!(entry["bytes"], part.layout().runtime_buffer_bytes());
            cropped += entry["cropped_bricks"].as_u64().expect("count");
        }
        assert!(cropped >= 2, "the brick is cropped in both parts: {got}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn nothing_installed_has_no_memory_answer() {
        let mut world = World::new();
        world.insert_resource(RuntimeRenderer::default());
        let got = get_baked_gi(&mut world).expect("answers");
        assert!(got["memory"].is_null(), "{got}");
        assert_eq!(got["host_layers"], 0);
    }

    fn host_volume() -> regia_asset::baked_gi::BakedGiVolume {
        use regia_asset::baked_gi::{
            BakeInfo, BakeLights, BakedGiVolume, GI_COMPONENTS, ProbeData, ProbeFlags, ProbeMeta,
            TerrainLayer, VolumeLayout,
        };
        let layout = VolumeLayout {
            terrain: Some(TerrainLayer {
                origin: [0.0, 0.0],
                spacing: [4.0, 4.0],
                counts: [2, 2],
                levels: vec![1.0],
                ground: vec![0.0; 4],
            }),
            bricks: Vec::new(),
            moment_resolution: 4,
            max_distance: 16.0,
        };
        let probes: Vec<ProbeData> = (0..layout.probe_count())
            .map(|_| ProbeData {
                sh: [[[0.5, 0.0, 0.0, 0.0]; 3]; GI_COMPONENTS],
                moments: vec![[16.0, 256.0]; 16],
                meta: ProbeMeta {
                    offset: [0.0; 3],
                    flags: ProbeFlags::VALID,
                },
            })
            .collect();
        BakedGiVolume::from_probes(&layout, BakeLights::default(), BakeInfo::default(), &probes)
            .expect("volume")
    }
}
