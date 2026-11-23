// SPDX-License-Identifier: MPL-2.0
//! `ui.components`: the UI components the running game knows (book `agents/ui-components.md`,
//! `docs/specs/111-ui-components.md` § 7, ADR-3147).
//!
//! Lists every component document in the [`UiLibrary`] (a `.ui.ron` with a `component:`
//! declaration) and, when an asset server gives the project directory, every component under
//! `assets/` too; for each one its parameters, slots and the templates that use it, plus every
//! template's expansion diagnostics (`ui_component_*` codes).

use std::collections::BTreeMap;

use regia_ecs::prelude::*;
use regia_ui::authoring::UiLibrary;
use regia_ui::ui_component::{COMPONENT_CODES, ComponentInfo, MAX_COMPONENT_DEPTH, scan_components};
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// `ui.components`.
pub(crate) fn components(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let mut found: BTreeMap<String, (ComponentInfo, Option<String>)> = BTreeMap::new();
    if let Some(server) = world.get_resource::<regia_asset::AssetServer>() {
        for info in scan_components(&server.project_root().join("assets")) {
            found.insert(info.path.clone(), (info, None));
        }
    }
    let Some(library) = world.get_resource::<UiLibrary>() else {
        return Ok(answer(found, &[], Vec::new()));
    };
    for (template, document, file) in library.components() {
        let info = ComponentInfo::of(document, file);
        found.insert(document.to_owned(), (info, Some(template.to_owned())));
    }
    let users: Vec<(String, Vec<String>)> = found
        .keys()
        .map(|path| (path.clone(), library.users_of(path)))
        .collect();
    let mut diagnostics = Vec::new();
    for name in library.names() {
        for d in library.diagnostics_of(name) {
            diagnostics.push(json!({
                "template": name,
                "code": d.code,
                "severity": d.severity,
                "document": d.document,
                "node": d.node,
                "component": d.component,
                "message": d.message,
            }));
        }
    }
    Ok(answer(found, &users, diagnostics))
}

fn answer(
    found: BTreeMap<String, (ComponentInfo, Option<String>)>,
    users: &[(String, Vec<String>)],
    diagnostics: Vec<Value>,
) -> Value {
    let rows: Vec<Value> = found
        .into_values()
        .map(|(info, template)| {
            let used_by = users
                .iter()
                .find(|(p, _)| *p == info.path)
                .map(|(_, u)| u.clone())
                .unwrap_or_default();
            json!({
                "path": info.path,
                "template": template,
                "loaded": template.is_some(),
                "params": info.params,
                "slots": info.slots,
                "users": used_by,
            })
        })
        .collect();
    json!({
        "components": rows,
        "diagnostics": diagnostics,
        "codes": COMPONENT_CODES,
        "max_depth": MAX_COMPONENT_DEPTH,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_ui::ui_component::text_loader;
    use regia_ui::{UiMigrators, parse_ui_ron};

    const BAR: &str = r#"(
    version: 9,
    component: (params: [(name: "label", type: string, default: "HP", targets: [(node: "L", property: text)])]),
    root: (name: "Bar", children: [(name: "L", text: (spans: ["HP"]))]),
)"#;

    #[test]
    fn lists_components_users_and_diagnostics() {
        let mut world = World::new();
        let empty = components(&mut world, &json!({})).expect("no library");
        assert_eq!(empty["components"], json!([]));
        assert_eq!(
            empty["codes"].as_array().map(Vec::len),
            Some(COMPONENT_CODES.len())
        );

        let migrators = UiMigrators::default();
        let mut load = text_loader(
            |p: &str| match p {
                "ui/bar.ui.ron" => Ok(BAR.to_owned()),
                other => Err(format!("{other}: not found")),
            },
            &migrators,
        );
        let mut library = UiLibrary::default();
        let bar = parse_ui_ron(BAR, "ui/bar.ui.ron", &migrators).expect("bar");
        library.insert_with("bar", "ui/bar.ui.ron", bar, &mut load);
        let hud = parse_ui_ron(
            r#"(version: 9, root: (children: [(use: "ui/bar.ui.ron"), (use: "ui/gone.ui.ron")]))"#,
            "ui/hud.ui.ron",
            &migrators,
        )
        .expect("hud");
        library.insert_with("hud", "ui/hud.ui.ron", hud, &mut load);
        world.insert_resource(library);

        let out = components(&mut world, &json!({})).expect("list");
        assert_eq!(out["components"][0]["path"], "ui/bar.ui.ron");
        assert_eq!(out["components"][0]["params"][0]["name"], "label");
        assert_eq!(out["components"][0]["users"], json!(["hud"]));
        assert_eq!(out["diagnostics"][0]["code"], "ui_component_missing");
        assert_eq!(out["diagnostics"][0]["template"], "hud");
    }
}
