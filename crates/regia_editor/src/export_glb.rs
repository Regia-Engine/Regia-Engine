// SPDX-License-Identifier: MPL-2.0
//! `asset.export_glb` and File → Export selection to glTF…: a level, a prefab or the selected
//! entities (and their descendants) as one `.glb` (`regia_asset_pipeline::scene_export`, ADR-2554,
//! `book/agents/export-glb.md`).

use std::path::Path;

use regia_asset_pipeline::scene_export::{
    ExportError, ExportOptions, ExportReport, export_scene, read_scene, write_glb,
};
use regia_ecs::prelude::*;
use regia_scene::components::EntityGuid;

use regia_agent::protocol::{self, RpcError};
use regia_agent::req_path;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_asset_pipeline::Forward;
use regia_core::id::Guid;
use regia_scene::spawn::{SnapshotFilter, snapshot_scene};
use serde_json::{Value, json};

use crate::level_io::OpenLevel;
use crate::selection::Selection;

/// Exports the selection of the open level to `out`. `project_root` resolves the `assets/...`
/// paths the entities name.
///
/// # Errors
///
/// A message when nothing is selected, the level cannot be snapshotted, or the export or the
/// write fails.
pub fn export_selection(world: &mut World, project_root: &Path, out: &Path) -> Result<ExportReport, String> {
    let guids: Vec<_> = world
        .get_resource::<Selection>()
        .map(|s| s.entities().to_vec())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|e| world.get::<EntityGuid>(e).map(|g| g.0))
        .collect();
    if guids.is_empty() {
        return Err("select at least one entity first".into());
    }
    let id = world
        .get_resource::<OpenLevel>()
        .map(|open| open.id)
        .ok_or_else(|| "no level is open".to_string())?;
    // Like a save: a sequence preview's values are put back for the snapshot and re-applied after.
    let level = crate::seq_tracks::with_preview_lifted(world, |world| {
        crate::level_io::snapshot_saved_level(world, id)
    })
    .map_err(|e| e.to_string())?;
    let options = ExportOptions {
        entities: Some(guids),
        ..ExportOptions::default()
    };
    let (glb, report) = export_scene(&level.scene, project_root, &options).map_err(|e| e.to_string())?;
    write_glb(out, &glb).map_err(|e| e.to_string())?;
    Ok(report)
}

/// The menu entry; returns the toast text once it was used.
pub fn menu_entry(
    ui: &mut egui::Ui,
    world: &mut World,
    project_root: Option<std::path::PathBuf>,
) -> Option<String> {
    let has_selection = world.get_resource::<Selection>().is_some_and(|s| !s.is_empty());
    if !crate::agent_parity::action_enabled(ui, has_selection, &crate::agent_parity::FILE_EXPORT_GLB, None)
        .clicked()
    {
        return None;
    }
    ui.close();
    let out = crate::file_dialog::FileDialog::new()
        .add_filter("glTF binary", &["glb"])
        .set_file_name("selection.glb")
        .save_file()?;
    let root = project_root.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    Some(match export_selection(world, &root, &out) {
        Ok(report) => format!(
            "Exported {} ({} nodes, {} meshes, {} warnings)",
            out.display(),
            report.nodes,
            report.meshes,
            report.warnings.len()
        ),
        Err(error) => format!("Export failed: {error}"),
    })
}

/// An editor agent method's handler.
pub type MethodHandler = fn(&mut World, &Value) -> Result<Value, RpcError>;

/// The `asset.export_glb` schema row and handler, registered by `agent_ext::install` (the
/// runtime-safe agent crate cannot depend on the asset pipeline).
#[must_use]
pub fn agent_method() -> (MethodSchema, MethodHandler) {
    const fn p(
        name: &'static str,
        kind: &'static str,
        required: bool,
        description: &'static str,
    ) -> ParamSchema {
        ParamSchema {
            name,
            kind,
            required,
            description,
        }
    }
    const PARAMS: &[ParamSchema] = &[
        p(
            "level",
            "string",
            false,
            "Project-relative .level.ron or .level.d path.",
        ),
        p("prefab", "string", false, "Project-relative .prefab.ron path."),
        p(
            "entities",
            "array",
            false,
            "Entity guids to export with their descendants (default: every entity).",
        ),
        p(
            "out",
            "string",
            true,
            "Project-relative output path ending in .glb, without `..`.",
        ),
        p(
            "forward",
            "string",
            false,
            "`convert` (default; adds the Regia_to_glTF root node) or `keep`.",
        ),
        p(
            "skins",
            "boolean",
            false,
            "Export skins of skinned source meshes (default false).",
        ),
        p(
            "animations",
            "boolean",
            false,
            "Export animation clips of the source meshes (default false).",
        ),
    ];
    (
        MethodSchema {
            name: "asset.export_glb",
            tool: "asset_export_glb",
            description: "Exports a level, a prefab or a list of entities as one .glb (glTF 2.0 binary) for a DCC tool: the node hierarchy with transforms, meshes (source glTF/GLB and built-in primitives) with normals, tangents, UV0/UV1 and vertex colors, metallic-roughness materials with PNG/JPEG textures embedded, KHR_lights_punctual lights and cameras. A root node Regia_to_glTF turns Regia's -Z forward into glTF's +Z (forward: keep writes Regia axes). Name `level` or `prefab` (project-relative); with neither, the open level is exported. Left-out parts are listed in warnings. Errors carry data.reason: export_params, export_out_invalid, export_no_level, glb_read_failed, glb_parse_failed, glb_unknown_entity, glb_nothing_to_export, glb_write_failed, glb_non_finite, path_outside_project.",
            params: PARAMS,
            result: "{ ok, out, bytes, nodes, meshes, triangles, materials, images, lights, cameras, skins, animations, warnings }",
            mutating: true,
            permission: Permission::Assets,
        },
        export_glb,
    )
}

/// `F0002` (or the path refusal) with `data.reason`.
fn refused(reason: &str, message: impl Into<String>, hint: &str) -> RpcError {
    let mut error = RpcError::new(protocol::INVALID_PARAMS, "F0002", message, hint);
    if let Some(Value::Object(data)) = error.data.as_mut() {
        data.insert("reason".into(), json!(reason));
    }
    error
}

fn export_error(error: &ExportError) -> RpcError {
    let hint = "see book/agents/export-glb.md";
    let code = error.code();
    match error {
        ExportError::Read { .. } | ExportError::UnknownEntity(_) => {
            let mut e = RpcError::not_found(error.to_string(), hint);
            if let Some(Value::Object(data)) = e.data.as_mut() {
                data.insert("reason".into(), json!(code));
            }
            e
        }
        _ => refused(code, error.to_string(), hint),
    }
}

/// `asset.export_glb { level? | prefab?, entities?, out, forward?, skins?, animations? }`.
fn export_glb(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let _span = tracing::info_span!("agent_export_glb").entered();
    let root = req_path::project_root(world);
    let text = |key: &str| params.get(key).and_then(Value::as_str);

    let out_text = text("out").ok_or_else(|| {
        refused(
            "export_params",
            "`out` (a project-relative .glb path) is required",
            "see book/agents/export-glb.md",
        )
    })?;
    if !out_text.to_ascii_lowercase().ends_with(".glb") {
        return Err(refused(
            "export_out_invalid",
            "`out` must end in .glb",
            "write the file as <name>.glb",
        ));
    }
    let out = req_path::project_path_in(&root, "out", out_text)?;
    req_path::check_derived(&root, "out", &out)?;

    let forward = match text("forward") {
        None | Some("convert") => Forward::Convert,
        Some("keep") => Forward::Keep,
        Some(other) => {
            return Err(refused(
                "export_params",
                format!("`forward` is `convert` or `keep`, not `{other}`"),
                "see book/agents/export-glb.md",
            ));
        }
    };
    let flag = |key: &str| params.get(key).and_then(Value::as_bool).unwrap_or(false);
    let entities = match params.get("entities") {
        None | Some(Value::Null) => None,
        Some(Value::Array(list)) => {
            let mut guids = Vec::with_capacity(list.len());
            for item in list {
                let guid: Guid = item.as_str().and_then(|s| s.parse().ok()).ok_or_else(|| {
                    refused(
                        "export_params",
                        format!("`entities` holds guid strings; {item} is not one"),
                        "list entity guids, e.g. from entity.list",
                    )
                })?;
                guids.push(guid);
            }
            (!guids.is_empty()).then_some(guids)
        }
        Some(_) => {
            return Err(refused(
                "export_params",
                "`entities` is an array of guid strings",
                "see book/agents/export-glb.md",
            ));
        }
    };
    let options = ExportOptions {
        entities,
        forward,
        skins: flag("skins"),
        animations: flag("animations"),
    };

    let scene = match (text("level"), text("prefab")) {
        (Some(_), Some(_)) => {
            return Err(refused(
                "export_params",
                "give `level` or `prefab`, not both",
                "omit both to export the open level",
            ));
        }
        (Some(rel), None) | (None, Some(rel)) => {
            let key = if params.get("level").is_some() {
                "level"
            } else {
                "prefab"
            };
            let path = req_path::project_path_in(&root, key, rel)?;
            req_path::check_derived(&root, key, &path)?;
            read_scene(&path).map_err(|e| export_error(&e))?
        }
        // The level as saved: without a sequence preview's values (see `export_selection`).
        (None, None) => crate::seq_tracks::with_preview_lifted(world, |world| {
            snapshot_scene(world, &SnapshotFilter::default())
        })
        .map_err(|e| {
            refused(
                "export_no_level",
                format!("the open level cannot be snapshotted: {e}"),
                "open a level, or name `level` or `prefab`",
            )
        })?,
    };

    let (glb, report) = export_scene(&scene, &root, &options).map_err(|e| export_error(&e))?;
    write_glb(&out, &glb).map_err(|e| export_error(&e))?;
    Ok(json!({
        "ok": true,
        "out": out_text,
        "bytes": report.bytes,
        "nodes": report.nodes,
        "meshes": report.meshes,
        "triangles": report.triangles,
        "materials": report.materials,
        "images": report.images,
        "lights": report.lights,
        "cameras": report.cameras,
        "skins": report.skins,
        "animations": report.animations,
        "warnings": report.warnings,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_selected_is_an_error() {
        let mut world = World::new();
        let dir = regia_test_utils::unique_temp_dir("editor_export_glb");
        let error = export_selection(&mut world, &dir, &dir.join("a.glb")).expect_err("empty selection");
        assert!(error.contains("select"));
    }
}

#[cfg(test)]
mod agent_tests {
    use super::*;

    fn world_in(root: &std::path::Path) -> World {
        let mut world = World::new();
        world.insert_resource(regia_agent::AgentSettings {
            project_dir: Some(root.to_path_buf()),
        });
        world
    }

    fn project() -> std::path::PathBuf {
        let dir = regia_test_utils::unique_temp_dir("agent_export_glb");
        std::fs::create_dir_all(dir.join("assets/maps")).expect("dir");
        std::fs::write(
            dir.join("assets/maps/a.level.ron"),
            r#"SceneFile(format_version: 1, entities: [
                (guid: "00000000-0000-0000-0000-000000000001", name: "Cube", components: {
                    "regia_render::runtime::MeshSource": (mesh: "builtin:cube", material: ""),
                }),
            ])"#,
        )
        .expect("level");
        dir
    }

    fn reason(error: &RpcError) -> String {
        error
            .data
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    }

    #[test]
    fn a_level_file_is_exported_inside_the_project() {
        let root = project();
        let mut world = world_in(&root);
        let result = export_glb(
            &mut world,
            &json!({ "level": "assets/maps/a.level.ron", "out": "export/a.glb" }),
        )
        .expect("export");
        assert_eq!(result["ok"], true);
        assert_eq!(result["meshes"], 1);
        let bytes = std::fs::read(root.join("export/a.glb")).expect("file");
        assert_eq!(&bytes[0..4], b"glTF");
        assert_eq!(result["bytes"], bytes.len());
    }

    #[test]
    fn errors_carry_their_codes() {
        let root = project();
        let mut world = world_in(&root);
        let call = |world: &mut World, p: Value| reason(&export_glb(world, &p).expect_err("error"));
        assert_eq!(call(&mut world, json!({})), "export_params");
        assert_eq!(call(&mut world, json!({ "out": "a.txt" })), "export_out_invalid");
        assert_eq!(
            call(
                &mut world,
                json!({ "level": "assets/maps/missing.level.ron", "out": "a.glb" })
            ),
            "glb_read_failed"
        );
        assert_eq!(
            call(
                &mut world,
                json!({ "level": "assets/maps/a.level.ron", "out": "a.glb", "entities": ["00000000-0000-0000-0000-0000000000ff"] })
            ),
            "glb_unknown_entity"
        );
        assert_eq!(
            call(
                &mut world,
                json!({ "level": "assets/maps/a.level.ron", "out": "a.glb", "entities": [7] })
            ),
            "export_params"
        );
        assert_eq!(
            call(
                &mut world,
                json!({ "level": "assets/maps/a.level.ron", "out": "a.glb", "forward": "up" })
            ),
            "export_params"
        );
        assert_eq!(
            call(
                &mut world,
                json!({ "level": "assets/maps/a.level.ron", "out": "../escape.glb" })
            ),
            "path_outside_project"
        );
        assert_eq!(
            call(&mut world, json!({ "level": "../a.level.ron", "out": "a.glb" })),
            "path_outside_project"
        );
    }
}

/// Encodes evaluated blockout triangles through the existing generated-mesh GLB writer,
/// then gives each contiguous material run its own glTF primitive and material slot.
pub(crate) fn blockout_glb(mesh: &crate::csg::Mesh) -> Result<Vec<u8>, String> {
    use std::borrow::Cow;
    use std::collections::BTreeMap;
    let mut grouped = BTreeMap::<u32, Vec<[u32; 3]>>::new();
    for (triangle, material) in mesh.triangles.iter().zip(&mesh.materials) {
        grouped.entry(*material).or_default().push(*triangle);
    }
    let mut generated = regia_procgen::GeneratedMesh::default();
    let mut runs = Vec::new();
    for (material, triangles) in grouped {
        let first = generated.indices.len();
        for triangle in triangles {
            let p = triangle.map(|i| mesh.positions[i as usize]);
            let normal = (p[1] - p[0])
                .cross(p[2] - p[0])
                .normalize_or_zero()
                .as_vec3()
                .to_array();
            for point in p {
                generated.indices.push(generated.positions.len() as u32);
                generated.positions.push(point.as_vec3().to_array());
                generated.normals.push(normal);
                generated.uvs.push([point.x as f32, point.z as f32]);
            }
        }
        runs.push((material, first, generated.indices.len() - first));
    }
    let bytes = regia_asset_pipeline::generated_mesh_export::generated_mesh_glb("Baked blockout", &generated)
        .map_err(|e| e.to_string())?;
    let parsed = gltf::binary::Glb::from_slice(&bytes).map_err(|e| e.to_string())?;
    let mut doc: Value = serde_json::from_slice(&parsed.json).map_err(|e| e.to_string())?;
    let original = doc["meshes"][0]["primitives"][0].clone();
    let index = original["indices"].as_u64().ok_or("GLB indices missing")? as usize;
    let accessor = doc["accessors"][index].clone();
    if accessor["componentType"] != 5125 {
        return Err("GLB writer did not emit u32 indices".into());
    }
    let offset = accessor["byteOffset"].as_u64().unwrap_or(0);
    let mut primitives = Vec::new();
    let mut materials = Vec::new();
    for (slot, (material, first, count)) in runs.into_iter().enumerate() {
        let mut next = accessor.clone();
        next["byteOffset"] = json!(offset + first as u64 * 4);
        next["count"] = json!(count);
        // The run has tighter index bounds than the original accessor; indices do not need bounds.
        if let Some(object) = next.as_object_mut() {
            object.remove("min");
            object.remove("max");
        }
        let accessors = doc["accessors"].as_array_mut().ok_or("GLB accessors missing")?;
        let i = accessors.len();
        accessors.push(next);
        let mut primitive = original.clone();
        primitive["indices"] = json!(i);
        primitive["material"] = json!(slot);
        primitives.push(primitive);
        materials.push(json!({"name":format!("Blockout material {material}"),"extras":{"blockout_material_id":material},"pbrMetallicRoughness":{"metallicFactor":0.0,"roughnessFactor":1.0}}));
    }
    doc["meshes"][0]["primitives"] = json!(primitives);
    doc["materials"] = json!(materials);
    let glb = gltf::binary::Glb {
        header: parsed.header,
        json: Cow::Owned(serde_json::to_vec(&doc).map_err(|e| e.to_string())?),
        bin: parsed.bin,
    };
    glb.to_vec().map_err(|e| e.to_string())
}
