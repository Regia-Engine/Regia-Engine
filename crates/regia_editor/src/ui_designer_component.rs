// SPDX-License-Identifier: MPL-2.0
//! UI components in the designer (`docs/specs/111-ui-components.md` § 6, ADR-3147): make a
//! component from the selected subtree (one undo step), set an instance's parameters, list the
//! project's components for the palette and the read-only nodes an instance inherits.
//!
//! Headless like [`crate::ui_designer`]: the panel and the `editor.ui_designer_*` agent methods
//! call the same functions.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regia_ui::authoring::UiNodeDef;
use regia_ui::ui_component::{
    ComponentDecl, ComponentDiagnostic, ComponentInfo, ParamDecl, ParamProperty, ParamTarget, ParamValue,
    check_component, check_use_path, parse_component,
};
use regia_ui::{UI_RON_VERSION, UiFile, UiMigrators, write_ui_ron};
use serde_json::Value;

use crate::ui_designer::{
    DesignOp, DesignerError, NodePath, UiDesign, node_at, path_string, resolve_node, walk, write_atomic,
};

/// One property of the selection exposed as a parameter of the new component.
#[derive(Debug, Clone, PartialEq)]
pub struct Exposed {
    /// Parameter name.
    pub name: String,
    /// The node it targets, as a path of the open document (inside the selection).
    pub node: NodePath,
    /// The property.
    pub property: ParamProperty,
}

/// What [`make_component`] wrote.
#[derive(Debug, Clone, PartialEq)]
pub struct MadeComponent {
    /// The component's path under `assets/` (the instance's `use`).
    pub use_path: String,
    /// The file written.
    pub file: PathBuf,
    /// The new component document.
    pub component: UiFile,
    /// The instance node's path in the open document.
    pub instance: NodePath,
}

/// The project's `assets/` directory for the open document: the preview's root, else the nearest
/// ancestor of its path named `assets`.
pub fn assets_root(doc: &UiDesign) -> Option<PathBuf> {
    doc.preview
        .assets_root
        .clone()
        .or_else(|| doc.path.as_deref().and_then(crate::ui_designer::assets_root_of))
}

fn need_assets(doc: &UiDesign) -> Result<PathBuf, DesignerError> {
    assets_root(doc).ok_or_else(|| {
        DesignerError::new(
            "no_assets",
            "the document is not under a project's assets/ directory: save it there first",
        )
    })
}

/// Writes the subtree at `node` to the new component `use_path` (under the document's `assets/`)
/// with `exposed` as its parameters (their current values become the defaults), and replaces the
/// subtree with an instance of it, as one undo step. The component file is written before the
/// document changes; it is never overwritten (`exists`).
pub fn make_component(
    doc: &mut UiDesign,
    node: &[usize],
    use_path: &str,
    exposed: &[Exposed],
) -> Result<MadeComponent, DesignerError> {
    let _span = tracing::debug_span!("ui_designer_make_component").entered();
    check_use_path(use_path).map_err(|why| DesignerError::new("bad_value", why))?;
    if node.is_empty() {
        return Err(DesignerError::new(
            "bad_node",
            "the root cannot become a component: select a node under it",
        ));
    }
    let subtree = node_at(&doc.file().root, node)
        .cloned()
        .ok_or_else(|| DesignerError::new("bad_node", format!("no node at {}", path_string(node))))?;
    if subtree.repeat.is_some() {
        return Err(DesignerError::new(
            "bad_node",
            "a repeat template cannot become a component itself: make its content one",
        ));
    }
    let assets = need_assets(doc)?;
    let file = assets.join(use_path);
    if file.exists() {
        return Err(DesignerError::new(
            "exists",
            format!("{} exists: pick another path", file.display()),
        ));
    }

    let mut root = subtree.clone();
    let mut params: Vec<ParamDecl> = Vec::new();
    for e in exposed {
        if e.name.is_empty() || params.iter().any(|p| p.name == e.name) {
            return Err(DesignerError::new(
                "bad_value",
                format!("parameter name {:?} is empty or used twice", e.name),
            ));
        }
        let Some(rel) = e.node.strip_prefix(node) else {
            return Err(DesignerError::new(
                "bad_node",
                format!("{} is outside the selection", path_string(&e.node)),
            ));
        };
        let target = node_at(&root, rel)
            .cloned()
            .ok_or_else(|| DesignerError::new("bad_node", format!("no node at {}", path_string(&e.node))))?;
        let target_name = if rel.is_empty() {
            String::new()
        } else if let Some(name) = &target.name {
            name.clone()
        } else {
            // An unnamed target gets a name the component's declaration can point at.
            let name = unique_name(&root, &e.name);
            if let Some(n) = node_at_mut(&mut root, rel) {
                n.name = Some(name.clone());
            }
            name
        };
        params.push(ParamDecl {
            name: e.name.clone(),
            ty: e.property.value_type(),
            default: e.property.current(&target),
            targets: vec![ParamTarget {
                node: target_name,
                property: e.property,
            }],
        });
    }
    let component = UiFile {
        version: UI_RON_VERSION,
        component: Some(ComponentDecl {
            params,
            slots: Vec::new(),
        }),
        root,
        variants: Vec::new(),
        back: None,
    };
    check_component(&component).map_err(|why| DesignerError::new("bad_value", why))?;
    let text = write_ui_ron(&component).map_err(|e| DesignerError::new("parse", e.to_string()))?;
    // The written text must load back as this component before anything is touched.
    parse_component(&text, use_path, &UiMigrators::default())
        .map_err(|e| DesignerError::new("parse", format!("written component does not load: {e:?}")))?;
    write_atomic(&file, &text)?;

    let instance = UiNodeDef {
        use_component: Some(use_path.to_owned()),
        name: subtree.name.clone(),
        ..UiNodeDef::default()
    };
    let mut next = doc.file().clone();
    if let Some(slot) = node_at_mut(&mut next.root, node) {
        *slot = instance;
    }
    doc.replace_as_step(next, node.to_vec());
    Ok(MadeComponent {
        use_path: use_path.to_owned(),
        file,
        component,
        instance: node.to_vec(),
    })
}

fn node_at_mut<'a>(root: &'a mut UiNodeDef, path: &[usize]) -> Option<&'a mut UiNodeDef> {
    path.iter().try_fold(root, |node, &i| node.children.get_mut(i))
}

/// `base`, or `base_2`, `base_3`, … : a node name not used under `root`.
fn unique_name(root: &UiNodeDef, base: &str) -> String {
    let mut taken = Vec::new();
    walk(root, &mut Vec::new(), &mut |n, _| taken.extend(n.name.clone()));
    if !taken.iter().any(|t| t == base) {
        return base.to_owned();
    }
    (2..)
        .map(|i| format!("{base}_{i}"))
        .find(|c| !taken.contains(c))
        .unwrap_or_else(|| base.to_owned())
}

/// The component an instance node uses, loaded and checked.
pub fn load_component(doc: &UiDesign, instance: &UiNodeDef) -> Result<(String, UiFile), DesignerError> {
    let use_path = instance
        .use_component
        .clone()
        .ok_or_else(|| DesignerError::new("not_instance", "the node is not a component instance"))?;
    check_use_path(&use_path).map_err(|why| DesignerError::new("bad_value", why))?;
    let assets = need_assets(doc)?;
    let text = std::fs::read_to_string(assets.join(&use_path))
        .map_err(|e| DesignerError::new("component_missing", format!("{use_path}: {e}")))?;
    let file = parse_component(&text, &use_path, &UiMigrators::default())
        .map_err(|e| DesignerError::new("component_invalid", format!("{e:?}")))?;
    Ok((use_path, file))
}

/// Sets instance parameter `name` of the instance at `node` to `value` (`null` resets it to the
/// component's default) as one undo step, checked against the component's declaration.
pub fn set_instance_param(
    doc: &mut UiDesign,
    node: &[usize],
    name: &str,
    value: Value,
) -> Result<NodePath, DesignerError> {
    let inst = node_at(&doc.file().root, node)
        .cloned()
        .ok_or_else(|| DesignerError::new("bad_node", format!("no node at {}", path_string(node))))?;
    let (use_path, comp) = load_component(doc, &inst)?;
    let decl = comp.component.unwrap_or_default();
    let Some(param) = decl.param(name) else {
        let known: Vec<&str> = decl.params.iter().map(|p| p.name.as_str()).collect();
        return Err(DesignerError::new(
            "unknown_param",
            format!("{use_path} declares no parameter {name:?} (declared: {known:?})"),
        ));
    };
    if !value.is_null() {
        let parsed: ParamValue = serde_json::from_value(value.clone())
            .map_err(|e| DesignerError::new("param_type", format!("{name}: {e}")))?;
        if !parsed.fits(param.ty) {
            return Err(DesignerError::new(
                "param_type",
                format!(
                    "parameter {name:?} is a {}, got a {}",
                    param.ty.name(),
                    parsed.kind()
                ),
            ));
        }
    }
    doc.apply(DesignOp::Set {
        node: node.to_vec(),
        key: format!("params.{name}"),
        value,
    })
}

/// Sets the content of slot `slot` of the instance at `node` (ADR-7677, spec 111 § 2.2): `Some`
/// nodes replace the slot's content (they take the place of the outlet's default children),
/// `None` clears it so the component's defaults show again. One undo step, checked against the
/// component's declared slots. The inspector's slot rows and
/// `editor.ui_designer_set_instance_slot` both call this.
///
/// # Errors
///
/// `bad_node`, `not_instance`, `component_missing`, `component_invalid`, or `unknown_slot` when
/// the component declares no such slot.
pub fn set_instance_slot(
    doc: &mut UiDesign,
    node: &[usize],
    slot: &str,
    content: Option<Vec<UiNodeDef>>,
) -> Result<NodePath, DesignerError> {
    let inst = node_at(&doc.file().root, node)
        .cloned()
        .ok_or_else(|| DesignerError::new("bad_node", format!("no node at {}", path_string(node))))?;
    let (use_path, comp) = load_component(doc, &inst)?;
    let decl = comp.component.unwrap_or_default();
    if !decl.slots.iter().any(|s| s == slot) {
        return Err(DesignerError::new(
            "unknown_slot",
            format!(
                "{use_path} declares no slot {slot:?} (declared: {:?})",
                decl.slots
            ),
        ));
    }
    let value = match content {
        Some(nodes) => Value::Array(nodes.iter().map(crate::ui_designer::node_json).collect()),
        None => Value::Null,
    };
    doc.apply(DesignOp::Set {
        node: node.to_vec(),
        key: format!("slots.{slot}"),
        value,
    })
}

/// One parameter of an instance as the inspector shows it.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct InstanceParam {
    /// The declaration (name, type, default, targets).
    #[serde(flatten)]
    pub decl: ParamDecl,
    /// The instance's value, when it sets one.
    pub value: Option<ParamValue>,
}

/// An instance of the open document with its expansion.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct InstanceInfo {
    /// Node path in the open document.
    pub node: String,
    /// The component path.
    #[serde(rename = "use")]
    pub use_path: String,
    /// `bind_root`, when set.
    pub bind_root: Option<String>,
    /// The component's parameters with this instance's values.
    pub params: Vec<InstanceParam>,
    /// The component's slots.
    pub slots: Vec<String>,
    /// The inherited nodes, read only: `(depth, label)` in tree order (the instance's own node first).
    pub inherited: Vec<(usize, String)>,
    /// Expansion problems.
    pub diagnostics: Vec<ComponentDiagnostic>,
}

/// Every instance of the open document (outer ones only: an instance inside slot content is listed
/// as part of its parent's expansion), expanded through the document's `assets/` directory.
pub fn instances(doc: &UiDesign) -> Vec<InstanceInfo> {
    let mut found: Vec<(NodePath, UiNodeDef)> = Vec::new();
    walk(&doc.file().root, &mut Vec::new(), &mut |n, p| {
        if n.use_component.is_some() {
            found.push((p.to_vec(), n.clone()));
        }
    });
    let assets = assets_root(doc);
    found
        .into_iter()
        .map(|(path, inst)| instance_info(doc, &path, &inst, assets.as_deref()))
        .collect()
}

fn instance_info(doc: &UiDesign, path: &[usize], inst: &UiNodeDef, assets: Option<&Path>) -> InstanceInfo {
    let expansion = crate::ui_designer_preview::expand_instance(inst, assets);
    let decl = load_component(doc, inst)
        .ok()
        .and_then(|(_, f)| f.component)
        .unwrap_or_default();
    let mut inherited = Vec::new();
    walk(&expansion.root, &mut Vec::new(), &mut |n, p| {
        inherited.push((p.len(), crate::ui_designer::node_label(n)));
    });
    InstanceInfo {
        node: path_string(path),
        use_path: inst.use_component.clone().unwrap_or_default(),
        bind_root: inst.bind_root.clone(),
        params: decl
            .params
            .iter()
            .map(|p| InstanceParam {
                decl: p.clone(),
                value: inst.params.get(&p.name).cloned(),
            })
            .collect(),
        slots: decl.slots,
        inherited,
        diagnostics: expansion.diagnostics,
    }
}

/// The instance node `text` (a path or `#name`) of the open document.
pub fn resolve_instance(doc: &UiDesign, text: &str) -> Result<(NodePath, UiNodeDef), DesignerError> {
    let path = resolve_node(&doc.file().root, text)?;
    let node = node_at(&doc.file().root, &path)
        .cloned()
        .ok_or_else(|| DesignerError::new("bad_node", format!("no node at {text}")))?;
    if node.use_component.is_none() {
        return Err(DesignerError::new(
            "not_instance",
            format!("{} is not a component instance", path_string(&path)),
        ));
    }
    Ok((path, node))
}

/// The palette's components: every `.ui.ron` under the document's `assets/` with a `component:`
/// declaration, sorted by path (empty when the document is not in a project).
pub fn palette_components(doc: &UiDesign) -> Vec<ComponentInfo> {
    assets_root(doc)
        .map(|a| regia_ui::ui_component::scan_components(&a))
        .unwrap_or_default()
}

/// A new instance node of the component `use_path` (the palette's "add").
pub fn instance_node(use_path: &str) -> UiNodeDef {
    let stem = use_path
        .rsplit('/')
        .next()
        .unwrap_or(use_path)
        .trim_end_matches(".ui.ron");
    UiNodeDef {
        use_component: Some(use_path.to_owned()),
        name: Some(stem.to_owned()),
        params: BTreeMap::new(),
        ..UiNodeDef::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_designer::UiDesigner;
    use crate::ui_designer_preview::preview;

    const BAR: &str = r##"(
    version: 11,
    component: (
        params: [(name: "label", type: string, default: "HP", targets: [(node: "Label", property: text)])],
    ),
    root: (name: "Bar", children: [(name: "Label", text: (spans: ["HP"]))]),
)
"##;

    fn project(hud: &str) -> (PathBuf, PathBuf) {
        let dir = regia_test_utils::unique_temp_dir("ui-designer-component");
        let ui = dir.join("assets/ui");
        std::fs::create_dir_all(&ui).expect("mkdir");
        std::fs::write(ui.join("bar.ui.ron"), BAR).expect("write");
        let path = ui.join("hud.ui.ron");
        std::fs::write(&path, hud).expect("write");
        (dir, path)
    }

    /// A save writes instances as references (`use` + parameters), never their expansion; the
    /// previews draw them expanded.
    #[test]
    fn a_save_keeps_instances_as_references_and_the_preview_expands_them() {
        let (dir, path) = project(
            "(\n    version: 11,\n    root: (name: \"Hud\", children: [(use: \"ui/bar.ui.ron\", name: \"P1\")]),\n)\n",
        );
        let mut designer = UiDesigner::default();
        designer.open(&path).expect("open");
        let doc = designer.doc_mut().expect("doc");
        set_instance_param(doc, &[0], "label", serde_json::json!("One")).expect("param");
        doc.save(None).expect("save");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("use: \"ui/bar.ui.ron\""), "{text}");
        assert!(text.contains("\"label\": \"One\""), "{text}");
        assert!(
            !text.contains("Label"),
            "the component's nodes stay in the component: {text}"
        );

        let frame = preview(doc.file(), &doc.preview, None).expect("preview");
        let label = frame
            .nodes
            .iter()
            .find(|n| n.name.as_deref() == Some("Label"))
            .expect("the instance draws its component's nodes");
        assert_eq!(label.text.as_deref(), Some("One"));
        assert_eq!(
            label.path, "/0",
            "inherited nodes report the instance as their origin"
        );
        let info = instances(doc);
        assert_eq!(info.len(), 1);
        assert_eq!(info[0].inherited.len(), 2);
        assert!(info[0].diagnostics.is_empty(), "{:?}", info[0].diagnostics);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Make component names an unnamed target, writes a loadable component and is one undo step
    /// that restores the document text byte for byte.
    #[test]
    fn make_component_is_one_undo_step_and_names_unnamed_targets() {
        let hud = "(\n    version: 11,\n    // kept\n    root: (name: \"Hud\", children: [\n        (name: \"Box\", children: [(text: (spans: [\"Ammo\"]))]),\n    ]),\n)\n";
        let (dir, path) = project(hud);
        let mut designer = UiDesigner::default();
        designer.open(&path).expect("open");
        let doc = designer.doc_mut().expect("doc");
        let undo_before = doc.undo_len();
        let made = make_component(
            doc,
            &[0],
            "ui/ammo.ui.ron",
            &[Exposed {
                name: "caption".into(),
                node: vec![0, 0],
                property: ParamProperty::Text,
            }],
        )
        .expect("make");
        assert_eq!(doc.undo_len(), undo_before + 1);
        let comp = std::fs::read_to_string(&made.file).expect("component written");
        let parsed = parse_component(&comp, "ui/ammo.ui.ron", &UiMigrators::default()).expect("loads");
        let decl = parsed.component.expect("declared");
        assert_eq!(
            decl.params[0].targets[0].node, "caption",
            "the unnamed text node got a name"
        );
        assert_eq!(decl.params[0].default, ParamValue::Text("Ammo".into()));
        assert_eq!(
            doc.file().root.children[0].use_component.as_deref(),
            Some("ui/ammo.ui.ron")
        );
        assert_eq!(doc.file().root.children[0].name.as_deref(), Some("Box"));
        assert!(
            make_component(doc, &[0], "ui/ammo.ui.ron", &[]).is_err(),
            "the component path is taken now"
        );
        doc.undo().expect("undo");
        assert_eq!(doc.save_text().expect("text").0, hud);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
