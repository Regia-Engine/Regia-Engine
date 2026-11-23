// SPDX-License-Identifier: MPL-2.0
//! The UI designer's component methods (`book/agents/ui-components.md`, spec 111 § 7, ADR-3147):
//! make a component from a node, set an instance's parameters, enter an instance's component and
//! return, and list the open document's instances with their expansion.

use regia_agent::protocol::{INTERNAL_ERROR, INVALID_PARAMS, NOT_FOUND, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use regia_ui::ui_component::ParamProperty;
use serde_json::{Value, json};

use crate::ui_designer::{DesignerError, UiDesign, UiDesigner, path_string, resolve_node};
use crate::ui_designer_component::{
    Exposed, assets_root, instances, make_component, palette_components, resolve_instance,
    set_instance_param, set_instance_slot,
};
use crate::viewport_agent::Handler;

/// The error codes only the component methods return (as `data.code`), besides the shared
/// `editor.ui_designer_*` ones.
pub const COMPONENT_ERROR_CODES: [&str; 10] = [
    "ui_designer_no_assets",
    "ui_designer_exists",
    "ui_designer_not_instance",
    "ui_designer_component_missing",
    "ui_designer_component_invalid",
    "ui_designer_unknown_param",
    "ui_designer_param_type",
    "ui_designer_not_entered",
    "ui_designer_dirty",
    "ui_designer_unknown_slot",
];

fn rpc(e: DesignerError) -> RpcError {
    let (code, hint) = match e.code {
        "no_document" => (NOT_FOUND, "call editor.ui_designer_open first"),
        "bad_node" | "not_instance" => (
            NOT_FOUND,
            "editor.ui_designer_instances lists the instances; editor.ui_designer_state lists node paths",
        ),
        "no_assets" => (
            INVALID_PARAMS,
            "open or save the document under the project's assets/ directory first",
        ),
        "exists" => (INVALID_PARAMS, "pick a component path that does not exist yet"),
        "component_missing" | "component_invalid" => (
            NOT_FOUND,
            "ui.components lists the components the project has and why one does not load",
        ),
        "unknown_param" | "param_type" => (
            INVALID_PARAMS,
            "editor.ui_designer_instances lists each instance's parameters with their types",
        ),
        "unknown_slot" => (
            INVALID_PARAMS,
            "editor.ui_designer_instances lists each instance's slots",
        ),
        "not_entered" => (
            INVALID_PARAMS,
            "enter a component with editor.ui_designer_enter_component first",
        ),
        "dirty" => (
            INVALID_PARAMS,
            "save the component with editor.ui_designer_save (or undo its edits) before returning",
        ),
        "io" => (INTERNAL_ERROR, "check the path and its permissions"),
        _ => (INVALID_PARAMS, "the message names the field; nothing was changed"),
    };
    RpcError::new(code, &format!("ui_designer_{}", e.code), e.message, hint)
}

fn designer(world: &mut World) -> &mut UiDesigner {
    world
        .get_resource_or_insert_with(UiDesigner::default)
        .into_inner()
}

fn doc(world: &mut World) -> Result<&mut UiDesign, RpcError> {
    designer(world).doc_mut().map_err(rpc)
}

fn str_param<'a>(params: &'a Value, key: &str) -> Result<&'a str, RpcError> {
    params
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params(format!("`{key}` (a string) is required")))
}

fn where_now(world: &mut World) -> Value {
    let d = designer(world);
    json!({
        "path": d.doc().and_then(|doc| doc.path.as_ref()).map(|p| p.display().to_string()),
        "depth": d.depth(),
        "parents": d
            .parent_paths()
            .iter()
            .map(|p| p.as_ref().map(|p| p.display().to_string()))
            .collect::<Vec<_>>(),
        "revision": d.doc().map(UiDesign::revision),
        "dirty": d.doc().map(UiDesign::dirty),
    })
}

/// `editor.ui_designer_make_component`.
fn make(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = str_param(params, "path")?.to_owned();
    let doc = doc(world)?;
    let node_text = str_param(params, "node")?;
    let node = resolve_node(&doc.file().root, node_text).map_err(rpc)?;
    let mut exposed = Vec::new();
    for (i, p) in params
        .get("params")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .enumerate()
    {
        let name = p
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::invalid_params(format!("params[{i}].name (a string) is required")))?;
        let property_name = p.get("property").and_then(Value::as_str).ok_or_else(|| {
            RpcError::invalid_params(format!("params[{i}].property (a string) is required"))
        })?;
        let property = ParamProperty::parse(property_name).ok_or_else(|| {
            let known: Vec<&str> = ParamProperty::ALL.iter().map(|p| p.name()).collect();
            RpcError::invalid_params(format!(
                "params[{i}].property {property_name:?} is not one of {known:?}"
            ))
        })?;
        let target = match p.get("node").and_then(Value::as_str) {
            Some(t) => resolve_node(&doc.file().root, t).map_err(rpc)?,
            None => node.clone(),
        };
        exposed.push(Exposed {
            name: name.to_owned(),
            node: target,
            property,
        });
    }
    let made = make_component(doc, &node, &path, &exposed).map_err(rpc)?;
    Ok(json!({
        "use": made.use_path,
        "file": made.file.display().to_string(),
        "instance": path_string(&made.instance),
        "component": made.component.component,
        "revision": doc.revision(),
        "undo": doc.undo_len(),
    }))
}

/// `editor.ui_designer_set_instance_param`.
fn set_param(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let doc = doc(world)?;
    let (node, _) = resolve_instance(doc, str_param(params, "node")?).map_err(rpc)?;
    let name = str_param(params, "name")?;
    let value = params.get("value").cloned().unwrap_or(Value::Null);
    let touched = set_instance_param(doc, &node, name, value).map_err(rpc)?;
    let all = instances(doc);
    let info = all.into_iter().find(|i| i.node == path_string(&touched));
    Ok(json!({ "instance": info, "revision": doc.revision(), "undo": doc.undo_len() }))
}

/// `editor.ui_designer_set_instance_slot`.
fn set_slot(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let doc = doc(world)?;
    let (node, _) = resolve_instance(doc, str_param(params, "node")?).map_err(rpc)?;
    let slot = str_param(params, "slot")?;
    let content = match params.get("content") {
        None | Some(Value::Null) => None,
        Some(v @ Value::Array(_)) => Some(
            serde_json::from_value::<Vec<regia_ui::authoring::UiNodeDef>>(v.clone())
                .map_err(|e| rpc(DesignerError::new("bad_value", format!("content: {e}"))))?,
        ),
        Some(_) => {
            return Err(rpc(DesignerError::new(
                "bad_value",
                "content is an array of nodes, or null to clear the slot",
            )));
        }
    };
    let touched = set_instance_slot(doc, &node, slot, content).map_err(rpc)?;
    let all = instances(doc);
    let info = all.into_iter().find(|i| i.node == path_string(&touched));
    let filled = crate::ui_designer::node_at(&doc.file().root, &touched)
        .and_then(|n| n.slots.get(slot))
        .map_or(0, Vec::len);
    Ok(
        json!({ "instance": info, "slot": slot, "nodes": filled, "revision": doc.revision(), "undo": doc.undo_len() }),
    )
}

/// `editor.ui_designer_enter_component`.
fn enter(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let doc = doc(world)?;
    let (_, inst) = resolve_instance(doc, str_param(params, "node")?).map_err(rpc)?;
    let root = assets_root(doc).ok_or_else(|| {
        rpc(DesignerError::new(
            "no_assets",
            "the document is not under a project's assets/ directory",
        ))
    })?;
    let file = root.join(inst.use_component.unwrap_or_default());
    if !file.is_file() {
        return Err(rpc(DesignerError::new(
            "component_missing",
            format!("{} does not exist", file.display()),
        )));
    }
    designer(world).enter(&file).map_err(rpc)?;
    Ok(where_now(world))
}

/// `editor.ui_designer_return`.
fn leave(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    designer(world).leave().map_err(rpc)?;
    Ok(where_now(world))
}

/// `editor.ui_designer_instances`.
fn list(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let doc = doc(world)?;
    let palette = palette_components(doc);
    Ok(json!({
        "instances": instances(doc),
        "components": palette,
        "assets_root": assets_root(doc).map(|p| p.display().to_string()),
        "revision": doc.revision(),
    }))
}

const NODE: ParamSchema = ParamSchema {
    name: "node",
    kind: "string",
    required: true,
    description: "A node path (\"/0/2\") or \"#name\".",
};

/// The component methods, appended to the `editor.ui_designer_*` table.
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.ui_designer_make_component",
                tool: "ui_designer_make_component",
                description: "Make a UI component from a node of the open document: its subtree is written to a new \
                    .ui.ron under assets/ (never overwriting) with the chosen properties exposed as parameters (their \
                    current values become the defaults), and the node is replaced by an instance (`use`). One undo \
                    step; undo restores the document exactly (the component file stays).",
                params: &[
                    NODE,
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: true,
                        description: "The component's path under assets/, e.g. \"ui/health_bar.ui.ron\".",
                    },
                    ParamSchema {
                        name: "params",
                        kind: "array",
                        required: false,
                        description: "[{ name, property, node? }]: property is text, command, class, background, border_color, tint, opacity, width, height, progress, visible or disabled; node (inside the subtree) defaults to the subtree's root.",
                    },
                ],
                result: "{ use, file, instance, component: { params, slots }, revision, undo }",
                mutating: true,
                permission: Permission::Assets,
            },
            make as Handler,
        ),
        (
            MethodSchema {
                name: "editor.ui_designer_set_instance_param",
                tool: "ui_designer_set_instance_param",
                description: "Set one parameter of a component instance in the open document (checked against the \
                    component's declaration); null resets it to the component's default. One undo step.",
                params: &[
                    NODE,
                    ParamSchema {
                        name: "name",
                        kind: "string",
                        required: true,
                        description: "A parameter the component declares.",
                    },
                    ParamSchema {
                        name: "value",
                        kind: "any",
                        required: false,
                        description: "string, number, bool, or a colour (\"#RRGGBB[AA]\" or [r, g, b, a]); null or absent resets.",
                    },
                ],
                result: "{ instance: { node, use, bind_root, params, slots, inherited, diagnostics }, revision, undo }",
                mutating: true,
                permission: Permission::EditScene,
            },
            set_param as Handler,
        ),
        (
            MethodSchema {
                name: "editor.ui_designer_set_instance_slot",
                tool: "ui_designer_set_instance_slot",
                description: "Set the content of one slot of a component instance in the open document (checked \
                    against the component's declared slots): the nodes replace the outlet's default children; null \
                    clears the slot so the component's defaults show. The inspector's slot rows call the same \
                    function. One undo step.",
                params: &[
                    NODE,
                    ParamSchema {
                        name: "slot",
                        kind: "string",
                        required: true,
                        description: "A slot the component declares.",
                    },
                    ParamSchema {
                        name: "content",
                        kind: "array",
                        required: false,
                        description: "The slot's nodes, each in the node form editor.ui_designer_edit adds; null or absent clears the slot.",
                    },
                ],
                result: "{ instance: { node, use, bind_root, params, slots, inherited, diagnostics }, slot, nodes, revision, undo }",
                mutating: true,
                permission: Permission::EditScene,
            },
            set_slot as Handler,
        ),
        (
            MethodSchema {
                name: "editor.ui_designer_enter_component",
                tool: "ui_designer_enter_component",
                description: "Open the component an instance uses; the open document waits until \
                    editor.ui_designer_return (nested entries stack).",
                params: &[NODE],
                result: "{ path, depth, parents, revision, dirty }",
                mutating: true,
                permission: Permission::Assets,
            },
            enter as Handler,
        ),
        (
            MethodSchema {
                name: "editor.ui_designer_return",
                tool: "ui_designer_return",
                description: "Return from an entered component to the document that entered it (save the component \
                    first); that document's previews re-read its components.",
                params: &[],
                result: "{ path, depth, parents, revision, dirty }",
                mutating: true,
                permission: Permission::Assets,
            },
            leave as Handler,
        ),
        (
            MethodSchema {
                name: "editor.ui_designer_instances",
                tool: "ui_designer_instances",
                description: "The open document's component instances with their parameters (declared type, default, \
                    this instance's value), slots, the read-only nodes they inherit and their expansion \
                    diagnostics, plus the project's components (the palette).",
                params: &[],
                result: "{ instances: [{ node, use, bind_root, params: [{ name, type, default, targets, value }], slots, inherited: [[depth, label]], diagnostics: [{ code, severity, document, node, component, message }] }], components: [{ path, params, slots }], assets_root, revision }",
                mutating: false,
                permission: Permission::Read,
            },
            list as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(e: &RpcError) -> String {
        e.data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    }

    const BAR: &str = r##"(
    version: 11,
    component: (
        params: [
            (name: "label", type: string, default: "HP", targets: [(node: "Label", property: text)]),
        ],
    ),
    root: (name: "Bar", children: [(name: "Label", text: (spans: ["HP"]))]),
)
"##;

    const HUD: &str = r##"(
    version: 11,
    // The player's bars.
    root: (
        name: "Hud",
        children: [
            (use: "ui/bar.ui.ron", name: "P1"),
            (name: "Box", background: "#202020", children: [(name: "Caption", text: (spans: ["Ammo"]))]),
        ],
    ),
)
"##;

    fn project() -> std::path::PathBuf {
        let dir = regia_test_utils::unique_temp_dir("ui-designer-components");
        let ui = dir.join("assets/ui");
        std::fs::create_dir_all(&ui).expect("mkdir");
        std::fs::write(ui.join("bar.ui.ron"), BAR).expect("write");
        std::fs::write(ui.join("hud.ui.ron"), HUD).expect("write");
        dir
    }

    fn call(w: &mut World, name: &str, params: Value) -> Result<Value, RpcError> {
        let table = methods();
        let (_, handler) = table.iter().find(|(s, _)| s.name == name).expect(name);
        handler(w, &params)
    }

    #[test]
    fn schemas_are_well_formed() {
        for (schema, _) in methods() {
            assert!(schema.name.starts_with("editor.ui_designer_"));
            assert_eq!(
                schema.mutating,
                schema.permission != Permission::Read,
                "{}",
                schema.name
            );
        }
        for c in COMPONENT_ERROR_CODES {
            assert!(
                c.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{c}"
            );
        }
    }

    /// Instances, parameters, enter and return, make-component and its undo through the wire names.
    #[test]
    fn instances_params_enter_return_and_make() {
        let dir = project();
        let hud = dir.join("assets/ui/hud.ui.ron");
        let original = std::fs::read_to_string(&hud).expect("read");
        let mut app = regia_app::App::new();
        let w = app.world_mut();
        designer(w).open(&hud).expect("open");

        let listed = call(w, "editor.ui_designer_instances", json!({})).expect("instances");
        assert_eq!(listed["instances"][0]["use"], "ui/bar.ui.ron");
        assert_eq!(listed["instances"][0]["params"][0]["name"], "label");
        assert_eq!(listed["components"][0]["path"], "ui/bar.ui.ron");
        let inherited = listed["instances"][0]["inherited"].as_array().expect("inherited");
        assert_eq!(inherited.len(), 2, "the bar and its label: {inherited:?}");

        let set = call(
            w,
            "editor.ui_designer_set_instance_param",
            json!({ "node": "#P1", "name": "label", "value": "P1" }),
        )
        .expect("set");
        assert_eq!(set["instance"]["params"][0]["value"], "P1");
        let bad = call(
            w,
            "editor.ui_designer_set_instance_param",
            json!({ "node": "#P1", "name": "nope", "value": 1 }),
        )
        .expect_err("unknown");
        assert_eq!(code(&bad), "ui_designer_unknown_param");
        let bad = call(
            w,
            "editor.ui_designer_set_instance_param",
            json!({ "node": "#P1", "name": "label", "value": 3 }),
        )
        .expect_err("type");
        assert_eq!(code(&bad), "ui_designer_param_type");

        let entered = call(w, "editor.ui_designer_enter_component", json!({ "node": "#P1" })).expect("enter");
        assert_eq!(entered["depth"], 1);
        assert!(
            entered["path"]
                .as_str()
                .unwrap_or_default()
                .ends_with("bar.ui.ron")
        );
        let back = call(w, "editor.ui_designer_return", json!({})).expect("return");
        assert_eq!(back["depth"], 0);
        assert_eq!(
            designer(w).doc().map(|d| d.file().root.children[0].params.len()),
            Some(1),
            "the parameter edit survived the round trip"
        );
        designer(w)
            .doc_mut()
            .expect("doc")
            .undo()
            .expect("undo the param");

        let made = call(
            w,
            "editor.ui_designer_make_component",
            json!({
                "node": "#Box",
                "path": "ui/ammo_box.ui.ron",
                "params": [
                    { "name": "caption", "property": "text", "node": "#Caption" },
                    { "name": "fill", "property": "background" },
                ],
            }),
        )
        .expect("make");
        assert_eq!(made["use"], "ui/ammo_box.ui.ron");
        assert_eq!(made["component"]["params"][0]["default"], "Ammo");
        assert!(dir.join("assets/ui/ammo_box.ui.ron").is_file());
        let again = call(
            w,
            "editor.ui_designer_make_component",
            json!({ "node": "#Box", "path": "ui/ammo_box.ui.ron" }),
        )
        .expect_err("exists");
        assert_eq!(code(&again), "ui_designer_exists");

        // One undo step restores the document byte for byte.
        let doc = designer(w).doc_mut().expect("doc");
        doc.undo().expect("undo make");
        assert_eq!(doc.save_text().expect("text").0, original);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every component error code comes back from the method that owns it.
    #[test]
    fn every_component_error_code_is_returned() {
        let dir = project();
        let mut app = regia_app::App::new();
        let w = app.world_mut();
        let mut seen = std::collections::BTreeSet::new();
        let mut see = |result: Result<Value, RpcError>| {
            seen.insert(code(&result.expect_err("must fail")));
        };
        // A new document has no assets/ directory.
        designer(w).new_document();
        crate::ui_designer::UiDesigner::doc_mut(designer(w))
            .expect("doc")
            .apply(crate::ui_designer::DesignOp::Add {
                parent: Vec::new(),
                index: None,
                node: Box::new(crate::ui_designer_component::instance_node("ui/bar.ui.ron")),
            })
            .expect("add");
        see(call(
            w,
            "editor.ui_designer_make_component",
            json!({ "node": "/0", "path": "ui/x.ui.ron" }),
        ));
        see(call(w, "editor.ui_designer_return", json!({})));

        let hud = dir.join("assets/ui/hud.ui.ron");
        designer(w).open(&hud).expect("open");
        see(call(
            w,
            "editor.ui_designer_enter_component",
            json!({ "node": "#Box" }),
        ));
        see(call(
            w,
            "editor.ui_designer_make_component",
            json!({ "node": "#Box", "path": "ui/bar.ui.ron" }),
        ));
        see(call(
            w,
            "editor.ui_designer_set_instance_param",
            json!({ "node": "#P1", "name": "nope", "value": 1 }),
        ));
        see(call(
            w,
            "editor.ui_designer_set_instance_param",
            json!({ "node": "#P1", "name": "label", "value": false }),
        ));
        see(call(
            w,
            "editor.ui_designer_set_instance_slot",
            json!({ "node": "#P1", "slot": "nope", "content": [] }),
        ));
        std::fs::write(dir.join("assets/ui/bar.ui.ron"), "(version: 9, root: (").expect("break");
        see(call(
            w,
            "editor.ui_designer_set_instance_param",
            json!({ "node": "#P1", "name": "label", "value": "x" }),
        ));
        std::fs::remove_file(dir.join("assets/ui/bar.ui.ron")).expect("rm");
        see(call(
            w,
            "editor.ui_designer_enter_component",
            json!({ "node": "#P1" }),
        ));
        std::fs::write(dir.join("assets/ui/bar.ui.ron"), BAR).expect("restore");
        call(w, "editor.ui_designer_enter_component", json!({ "node": "#P1" })).expect("enter");
        designer(w)
            .doc_mut()
            .expect("doc")
            .apply(crate::ui_designer::DesignOp::Set {
                node: Vec::new(),
                key: "name".to_owned(),
                value: json!("Edited"),
            })
            .expect("edit");
        see(call(w, "editor.ui_designer_return", json!({})));

        let expected: std::collections::BTreeSet<String> =
            COMPONENT_ERROR_CODES.iter().map(|c| (*c).to_owned()).collect();
        assert_eq!(seen, expected);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
