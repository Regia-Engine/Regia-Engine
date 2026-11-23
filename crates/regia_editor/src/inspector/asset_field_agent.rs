// SPDX-License-Identifier: MPL-2.0
//! `editor.asset_field_choices` (ADR-7472): what an inspector asset field (`@AssetFilter`) offers
//! and whether it takes a given asset — the agent's view of the picker and of a Content Browser
//! drop. An agent then writes the field with `editor.set_property`, as the drop does.

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use regia_ecs::reflect::{AppTypeRegistry, TypeInfo};
use serde_json::{Value, json};

use super::impls::{asset_choices, asset_drop_verdict};
use crate::data_panels::PanelError;
use crate::viewport_agent::Handler;

const HINT: &str = "see book/agents/inspector.md";

/// Most choices one answer lists (the rest are counted in `total`).
pub const MAX_CHOICES: usize = 500;

fn err(code: &str, message: impl Into<String>) -> RpcError {
    PanelError::new(code, message).rpc(HINT)
}

/// The `@AssetFilter` of field `field` (a dotted path through struct fields) of component
/// `component` (type path or short name).
///
/// # Errors
///
/// `asset_field_unknown` when the component or field does not resolve, `asset_field_no_filter`
/// when the field has no `@AssetFilter`.
pub fn field_filter(
    registry: &regia_ecs::reflect::TypeRegistry,
    component: &str,
    field: &str,
) -> Result<&'static str, PanelError> {
    let registration = registry
        .get_with_type_path(component)
        .or_else(|| registry.get_with_short_type_path(component))
        .ok_or_else(|| {
            PanelError::new(
                "asset_field_unknown",
                format!("no registered component `{component}`"),
            )
        })?;
    let mut info: &TypeInfo = registration.type_info();
    let mut filter = None;
    for name in field.split('.') {
        let TypeInfo::Struct(fields) = info else {
            return Err(PanelError::new(
                "asset_field_unknown",
                format!("`{component}.{field}`: `{name}` is not inside a struct"),
            ));
        };
        let named = fields.field(name).ok_or_else(|| {
            PanelError::new(
                "asset_field_unknown",
                format!("`{component}` has no field `{field}`"),
            )
        })?;
        filter = regia_ecs::attrs::EditorAttrs::from_custom(named.custom_attributes()).asset_filter;
        info = registry
            .get(named.type_id())
            .map(|r| r.type_info())
            .unwrap_or(info);
    }
    filter.ok_or_else(|| {
        PanelError::new(
            "asset_field_no_filter",
            format!("`{component}.{field}` is not an asset field (no @AssetFilter)"),
        )
    })
}

/// `editor.asset_field_choices`.
fn asset_field_choices(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = crate::undo_agent::live_entity(world, params, "entity")?;
    let text = |key: &str| params.get(key).and_then(Value::as_str);
    let component = text("component").ok_or_else(|| RpcError::wrong_type("component", "a string"))?;
    let field = text("field").ok_or_else(|| RpcError::wrong_type("field", "a string"))?;
    let search = text("search").unwrap_or_default();
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let filter = field_filter(&registry, component, field).map_err(|e| e.rpc(HINT))?;
    drop(registry);
    let current = crate::command::read_property(world, entity, component, field)
        .map_err(|e| err("asset_field_unknown", e.to_string()))?;
    let current = current.try_downcast_ref::<String>().cloned().unwrap_or_default();
    let assets = world
        .get_resource::<super::AssetCatalog>()
        .map(|catalog| std::sync::Arc::clone(&catalog.entries))
        .unwrap_or_default();
    let choices = asset_choices(filter, search, &assets);
    let verdict = text("path").map(|path| {
        let kind = crate::dnd::kind_of_path(path);
        match asset_drop_verdict(filter, path, kind) {
            Ok(()) => json!({ "accepted": true, "reason": null }),
            Err(reason) => json!({ "accepted": false, "reason": reason }),
        }
    });
    Ok(json!({
        "filter": filter,
        "current": current,
        "total": choices.len(),
        "choices": choices.iter().take(MAX_CHOICES).collect::<Vec<_>>(),
        "verdict": verdict,
    }))
}

const PARAMS: &[ParamSchema] = &[
    ParamSchema {
        name: "entity",
        kind: "number",
        required: true,
        description: "The entity whose component holds the field.",
    },
    ParamSchema {
        name: "component",
        kind: "string",
        required: true,
        description: "Component type path or short name (`MeshSource`).",
    },
    ParamSchema {
        name: "field",
        kind: "string",
        required: true,
        description: "The field (`material`; nested struct fields as `a.b`).",
    },
    ParamSchema {
        name: "search",
        kind: "string",
        required: false,
        description: "Keep the choices whose path contains this (case-insensitive), as the picker's search box.",
    },
    ParamSchema {
        name: "path",
        kind: "string",
        required: false,
        description: "An asset path to check as a Content Browser drop would: `verdict` says whether the field \
            takes it and why not.",
    },
];

/// The method.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.asset_field_choices",
            tool: "asset_field_choices",
            description: "An inspector asset field (@AssetFilter): its filter, current value and the project assets \
                its picker lists (built-in primitives first for a mesh field), and whether it takes `path` as a \
                drop would (the refusal reason the Inspector shows). Write the field with editor.set_property.",
            params: PARAMS,
            result: "{ filter, current, total, choices: [path], verdict: { accepted, reason } | null }",
            mutating: false,
            permission: Permission::Read,
        },
        asset_field_choices as Handler,
    )]
}

#[cfg(test)]
mod tests {
    use regia_render::MeshSource;
    use regia_scene::prelude::{Name, Transform};

    use super::*;
    use crate::dnd::ContentKind;

    fn reason(e: &RpcError) -> String {
        e.data
            .as_ref()
            .and_then(|d| d["reason"].as_str())
            .unwrap_or_default()
            .to_owned()
    }

    /// Dispatched like the protocol does (the editor's method table), the method lists what the
    /// picker lists and gives the drop's verdict.
    #[test]
    fn the_method_lists_the_pickers_choices_and_the_drop_verdict() {
        let mut app = regia_app::App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        app.register_type::<MeshSource>();
        let entity = app
            .world_mut()
            .spawn((
                Name::new("Crate"),
                Transform::default(),
                MeshSource::new("builtin:cube", ""),
            ))
            .id();
        app.world_mut().insert_resource(super::super::AssetCatalog {
            entries: vec![
                ("assets/props/crate.gltf".to_owned(), ContentKind::Mesh),
                ("assets/materials/wet.matgraph.ron".to_owned(), ContentKind::Other),
                (
                    "assets/materials/wet_red.matinst.ron".to_owned(),
                    ContentKind::Other,
                ),
            ]
            .into(),
            scanned: None,
        });
        let handler = crate::agent_ext::all_methods()
            .into_iter()
            .find(|(schema, _)| schema.name == "editor.asset_field_choices")
            .map(|(_, handler)| handler)
            .expect("registered with the editor");
        let call = |app: &mut regia_app::App, params: Value| handler(app.world_mut(), &params);
        let v = call(
            &mut app,
            json!({ "entity": entity.to_bits(), "component": "MeshSource", "field": "material",
                    "path": "assets/props/crate.gltf" }),
        )
        .expect("material");
        assert_eq!(v["filter"], "material");
        assert_eq!(
            v["choices"],
            json!([
                "assets/materials/wet.matgraph.ron",
                "assets/materials/wet_red.matinst.ron"
            ])
        );
        assert_eq!(v["verdict"]["accepted"], false);
        assert_eq!(
            v["verdict"]["reason"],
            "assets/props/crate.gltf is not a material (material)"
        );
        let v = call(
            &mut app,
            json!({ "entity": entity.to_bits(), "component": "MeshSource", "field": "mesh", "search": "crate",
                    "path": "assets/props/crate.gltf" }),
        )
        .expect("mesh");
        assert_eq!(v["current"], "builtin:cube");
        assert_eq!(v["choices"], json!(["assets/props/crate.gltf"]));
        assert_eq!(v["verdict"]["accepted"], true);
        for (params, expected) in [
            (
                json!({ "entity": entity.to_bits(), "component": "MeshSource", "field": "nope" }),
                "asset_field_unknown",
            ),
            (
                json!({ "entity": entity.to_bits(), "component": "Nope", "field": "mesh" }),
                "asset_field_unknown",
            ),
            (
                json!({ "entity": entity.to_bits(), "component": "Transform", "field": "translation" }),
                "asset_field_no_filter",
            ),
        ] {
            let e = call(&mut app, params.clone()).expect_err("refused");
            assert_eq!(reason(&e), expected, "{params}");
        }
    }
}
