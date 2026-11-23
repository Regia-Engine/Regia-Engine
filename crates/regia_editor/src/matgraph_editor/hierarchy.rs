// SPDX-License-Identifier: MPL-2.0
//! Material instance reparenting and the inheritance view (w12 P-170, ADR-7851).
//!
//! Reparent: an open instance gets another parent graph in one undo step. Overrides the new
//! parent still declares (a parameter of the same type, a texture, a switch) are kept; the
//! others are dropped and named in the toast and the agent's answer. The document's graph and
//! layout become the new parent's; only the `.matinst.ron` is saved.
//!
//! Hierarchy: every `.matgraph.ron` of the project with the instances whose `parent` resolves
//! to it (the format's chain: an instance's parent is a graph, `regia_matgraph::instance`);
//! instances whose parent is missing are listed apart. Dragging an instance row onto a graph
//! row opens the instance in a tab and reparents it there (save to keep it).

use std::path::{Path, PathBuf};

use regia_ecs::prelude::*;
use regia_matgraph::{MaterialGraph, MaterialInstance};
use serde_json::{Value, json};

use super::{DocMode, MatGraphDoc, MatGraphEditor};

/// One instance in the view.
#[derive(Clone, Debug, PartialEq)]
pub struct InstanceEntry {
    /// The `.matinst.ron`.
    pub path: PathBuf,
    /// Its `parent` field.
    pub parent: String,
}

/// One graph and its instances.
#[derive(Clone, Debug, PartialEq)]
pub struct GraphEntry {
    /// The `.matgraph.ron`.
    pub path: PathBuf,
    /// Its display name.
    pub name: String,
    /// Instances whose parent resolves to it, sorted by path.
    pub instances: Vec<InstanceEntry>,
}

/// The project's graphs with their instances.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Hierarchy {
    /// Graphs, sorted by path (unparsable ones skipped).
    pub graphs: Vec<GraphEntry>,
    /// Instances whose parent is missing or unreadable.
    pub orphans: Vec<InstanceEntry>,
}

impl Hierarchy {
    /// As protocol JSON (paths as given by the scan).
    #[must_use]
    pub fn to_json(&self) -> Value {
        let inst = |i: &InstanceEntry| json!({ "path": i.path.display().to_string(), "parent": i.parent });
        json!({
            "graphs": self.graphs.iter().map(|g| json!({
                "path": g.path.display().to_string(),
                "name": g.name,
                "instances": g.instances.iter().map(inst).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "orphans": self.orphans.iter().map(inst).collect::<Vec<_>>(),
        })
    }
}

fn canonical(path: PathBuf) -> PathBuf {
    std::fs::canonicalize(&path).unwrap_or(path)
}

/// Scans `root` (hidden directories and `target` skipped) for graphs and instances.
#[must_use]
pub fn scan(root: &Path) -> Hierarchy {
    let _span = tracing::info_span!(target: "regia_editor", "matgraph_hierarchy").entered();
    let files = super::usages::project_files(root, &[".matgraph.ron", ".matinst.ron"]);
    let mut graphs = Vec::new();
    for path in files
        .iter()
        .filter(|p| p.to_string_lossy().ends_with(".matgraph.ron"))
    {
        let Some(graph) = std::fs::read_to_string(path)
            .ok()
            .and_then(|t| MaterialGraph::from_ron(&t).ok())
        else {
            continue;
        };
        graphs.push(GraphEntry {
            path: path.clone(),
            name: graph.name,
            instances: Vec::new(),
        });
    }
    let keys: Vec<PathBuf> = graphs.iter().map(|g| canonical(g.path.clone())).collect();
    let mut orphans = Vec::new();
    for path in files
        .iter()
        .filter(|p| p.to_string_lossy().ends_with(".matinst.ron"))
    {
        let Some(instance) = std::fs::read_to_string(path)
            .ok()
            .and_then(|t| MaterialInstance::from_ron(&t).ok())
        else {
            continue;
        };
        let entry = InstanceEntry {
            path: path.clone(),
            parent: instance.parent.clone(),
        };
        let parent = super::instance_parent_path(Some(path), Some(root), &instance.parent)
            .ok()
            .map(canonical);
        match parent.and_then(|p| keys.iter().position(|k| *k == p)) {
            Some(i) => graphs[i].instances.push(entry),
            None => orphans.push(entry),
        }
    }
    Hierarchy { graphs, orphans }
}

/// The `parent` reference that resolves from an instance at `instance` to `parent` the way the
/// editor and the cook resolve it (next to the instance, else under the project): the file
/// name when they share a folder, else the path under `assets/`, else under the project.
///
/// # Errors
///
/// No reference resolves back to `parent` (it is outside the project).
pub fn parent_reference(
    instance: Option<&Path>,
    root: Option<&Path>,
    parent: &Path,
) -> Result<String, String> {
    let target = canonical(parent.to_path_buf());
    let mut candidates = Vec::new();
    if let (Some(dir), Some(name)) = (instance.and_then(Path::parent), parent.file_name())
        && canonical(dir.to_path_buf()) == canonical(parent.parent().unwrap_or(Path::new("")).to_path_buf())
    {
        candidates.push(name.to_string_lossy().into_owned());
    }
    if let Some(root) = root {
        let root = canonical(root.to_path_buf());
        for base in [root.join("assets"), root.clone()] {
            if let Ok(rel) = target.strip_prefix(&base) {
                candidates.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    candidates
        .into_iter()
        .find(|c| {
            super::instance_parent_path(instance, root, c)
                .map(canonical)
                .is_ok_and(|p| p == target)
        })
        .ok_or_else(|| {
            format!(
                "{} is outside the project: an instance cannot name it",
                parent.display()
            )
        })
}

/// What a reparent kept and dropped.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReparentReport {
    /// The new `parent` field.
    pub parent: String,
    /// Overrides kept (`param tint`, `texture albedo`, `switch wet`).
    pub kept: Vec<String>,
    /// Overrides dropped, each with why.
    pub dropped: Vec<String>,
}

impl ReparentReport {
    /// As protocol JSON.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({ "parent": self.parent, "kept": self.kept, "dropped": self.dropped })
    }
}

/// Moves the instance document `doc` onto `parent` (its graph and layout), named `reference` in
/// the instance. Overrides the new parent does not declare, or declares with another type, are
/// dropped.
///
/// # Errors
///
/// The document is not an instance.
pub fn reparent(
    doc: &mut MatGraphDoc,
    parent: MaterialGraph,
    layout: super::GraphLayoutFile,
    reference: &str,
) -> Result<ReparentReport, String> {
    let DocMode::Instance(old) = &doc.mode else {
        return Err("only a material instance has a parent".into());
    };
    let mut next = MaterialInstance::new(reference);
    let mut report = ReparentReport {
        parent: reference.to_string(),
        ..ReparentReport::default()
    };
    for (name, value) in &old.params {
        match parent.params.iter().find(|p| &p.name == name) {
            Some(decl) if decl.value.value_type() == value.value_type() => {
                next.params
                    .insert(name.clone(), super::model::as_declared(decl.value, *value));
                report.kept.push(format!("param {name}"));
            }
            Some(_) => report
                .dropped
                .push(format!("param {name}: the new parent declares another type")),
            None => report
                .dropped
                .push(format!("param {name}: not declared by the new parent")),
        }
    }
    for (name, path) in &old.textures {
        if parent.textures.iter().any(|t| &t.name == name) {
            next.textures.insert(name.clone(), path.clone());
            report.kept.push(format!("texture {name}"));
        } else {
            report
                .dropped
                .push(format!("texture {name}: not declared by the new parent"));
        }
    }
    for (name, on) in &old.switches {
        if parent.switches.iter().any(|s| &s.name == name) {
            next.switches.insert(name.clone(), *on);
            report.kept.push(format!("switch {name}"));
        } else {
            report
                .dropped
                .push(format!("switch {name}: not declared by the new parent"));
        }
    }
    let fresh = super::with_layout(MatGraphDoc::instance(parent, Default::default(), next), layout);
    *doc = fresh;
    Ok(report)
}

/// Reparents the open instance onto the graph file `parent` (one undo step) and points the tab's
/// parent at it.
///
/// # Errors
///
/// No instance open, the parent cannot be read, or it is outside the project.
pub fn reparent_open(world: &mut World, parent: &Path) -> Result<ReparentReport, String> {
    let (instance_path, root) = {
        let editor = world.get_resource_or_insert_with(MatGraphEditor::default);
        if editor.doc.instance_meta().is_none() {
            return Err("the open document is not a material instance".into());
        }
        (editor.path.clone(), editor.root.clone())
    };
    let text = std::fs::read_to_string(parent)
        .map_err(|e| format!("cannot read the parent {}: {e}", parent.display()))?;
    let graph = MaterialGraph::from_ron(&text)
        .map_err(|e| format!("cannot load the parent {}: {e}", parent.display()))?;
    let reference = parent_reference(instance_path.as_deref(), root.as_deref(), parent)?;
    let layout = super::read_layout_file(parent);
    let report = super::edit(world, "reparent instance", None, move |d| {
        reparent(d, graph, layout, &reference)
    })?;
    // The edit's apply pointed the tab's parent at the new graph (and its undo points it back).
    let mut editor = world.resource_mut::<MatGraphEditor>();
    editor.ui.parity.hierarchy = None;
    editor.toast = if report.dropped.is_empty() {
        format!(
            "reparented onto {} (kept {} override(s))",
            report.parent,
            report.kept.len()
        )
    } else {
        format!(
            "reparented onto {}: dropped {}",
            report.parent,
            report.dropped.join("; ")
        )
    };
    Ok(report)
}

/// What the Hierarchy view asks the panel to do after drawing.
#[derive(Clone, Debug, PartialEq)]
pub enum HierarchyAction {
    /// Open a graph or an instance.
    Open(PathBuf),
    /// Open instance `.0` in a tab and reparent it onto graph `.1`.
    Reparent(PathBuf, PathBuf),
    /// Rescan the project.
    Refresh,
}

/// The Hierarchy section: graphs with their instances; click opens, an instance dragged onto a
/// graph reparents it; the open instance's reparent picker (keyboard route) is
/// [`reparent_picker_ui`].
pub fn hierarchy_ui(ui: &mut egui::Ui, editor: &mut MatGraphEditor, out: &mut Vec<HierarchyAction>) {
    egui::CollapsingHeader::new("Hierarchy")
        .id_salt("matgraph-hierarchy")
        .default_open(false)
        .show(ui, |ui| {
            let refresh = crate::a11y_audit::name(
                ui.small_button("Scan project"),
                egui::WidgetType::Button,
                "Scan the project's graphs and instances",
            );
            if refresh.clicked() || (editor.ui.parity.hierarchy.is_none() && editor.root.is_some()) {
                out.push(HierarchyAction::Refresh);
            }
            let Some(Ok(tree)) = &editor.ui.parity.hierarchy else {
                if let Some(Err(e)) = &editor.ui.parity.hierarchy {
                    ui.colored_label(crate::theme::ui_tokens(ui.ctx()).accents.error_text, e);
                }
                return;
            };
            for graph in &tree.graphs {
                let (_, dropped) = ui.dnd_drop_zone::<PathBuf, _>(egui::Frame::new(), |ui| {
                    let label = format!("{} ({})", graph.name, graph.path.display());
                    if ui.link(label).clicked() {
                        out.push(HierarchyAction::Open(graph.path.clone()));
                    }
                    ui.indent(("matgraph-hier", graph.path.display().to_string()), |ui| {
                        for inst in &graph.instances {
                            let id = egui::Id::new(("matgraph-hier-inst", inst.path.display().to_string()));
                            ui.dnd_drag_source(id, inst.path.clone(), |ui| {
                                ui.label(format!("instance {}", inst.path.display()));
                            });
                        }
                    });
                });
                if let Some(instance) = dropped {
                    out.push(HierarchyAction::Reparent((*instance).clone(), graph.path.clone()));
                }
            }
            if !tree.orphans.is_empty() {
                ui.weak("Instances whose parent is missing:");
                for inst in &tree.orphans {
                    if ui
                        .link(format!("{} (parent `{}`)", inst.path.display(), inst.parent))
                        .clicked()
                    {
                        out.push(HierarchyAction::Open(inst.path.clone()));
                    }
                }
            }
        });
}

/// The open instance's parent picker: a combo box of the project's graphs and a Reparent
/// button (the keyboard route of a drag in the Hierarchy).
pub fn reparent_picker_ui(ui: &mut egui::Ui, editor: &mut MatGraphEditor, out: &mut Vec<HierarchyAction>) {
    if editor.doc.instance_meta().is_none() {
        return;
    }
    let graphs: Vec<PathBuf> = match &editor.ui.parity.hierarchy {
        Some(Ok(tree)) => tree.graphs.iter().map(|g| g.path.clone()).collect(),
        _ => {
            if editor.root.is_some() {
                out.push(HierarchyAction::Refresh);
            }
            Vec::new()
        }
    };
    ui.horizontal(|ui| {
        ui.label("New parent");
        let shown = if editor.ui.parity.reparent_pick.is_empty() {
            "(pick a graph)".to_string()
        } else {
            editor.ui.parity.reparent_pick.clone()
        };
        let combo = egui::ComboBox::from_id_salt("matgraph-reparent-pick")
            .selected_text(shown)
            .show_ui(ui, |ui| {
                for g in &graphs {
                    let text = g.display().to_string();
                    ui.selectable_value(&mut editor.ui.parity.reparent_pick, text.clone(), text);
                }
            })
            .response;
        crate::a11y_audit::name(combo, egui::WidgetType::ComboBox, "New parent graph");
        let go = ui.add_enabled(
            !editor.ui.parity.reparent_pick.is_empty(),
            egui::Button::new("Reparent"),
        );
        if go.clicked() {
            // An unsaved instance (no path) is the open one: an empty path means "the open tab".
            out.push(HierarchyAction::Reparent(
                editor.path.clone().unwrap_or_default(),
                PathBuf::from(editor.ui.parity.reparent_pick.clone()),
            ));
        }
    });
}

/// Runs the Hierarchy's actions after the panel drew (the panel borrows the editor while it
/// draws).
pub fn perform(world: &mut World, actions: Vec<HierarchyAction>) {
    for action in actions {
        let result = match action {
            HierarchyAction::Refresh => {
                let mut editor = world.resource_mut::<MatGraphEditor>();
                let tree = editor
                    .root
                    .clone()
                    .map(|root| scan(&root))
                    .ok_or_else(|| "no project is open".to_string());
                editor.ui.parity.hierarchy = Some(tree);
                Ok(())
            }
            HierarchyAction::Open(path) => super::panel::open_path(world, &path.display().to_string()),
            HierarchyAction::Reparent(instance, parent) => {
                let open = world.resource::<MatGraphEditor>().path.clone();
                let opened =
                    if open.as_ref().is_some_and(|p| *p == instance) || instance.as_os_str().is_empty() {
                        Ok(())
                    } else {
                        super::panel::open_path(world, &instance.display().to_string())
                    };
                opened.and_then(|()| reparent_open(world, &parent).map(|_| ()))
            }
        };
        if let Err(e) = result {
            world.resource_mut::<MatGraphEditor>().toast = e;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).expect("dir");
        }
        std::fs::write(path, text).expect("write");
    }

    pub(super) const RED: &str = r#"(
        format_version: 4,
        name: "red",
        params: [(name: "tint", value: Color((1.0, 0.0, 0.0))), (name: "rough", value: Float(0.5))],
        textures: [(name: "albedo", path: "a.png", color_space: Srgb)],
        nodes: { "tint": Param("tint"), "r": Param("rough") },
        output: (base_color: Some("tint"), roughness: Some("r")),
    )"#;
    pub(super) const BLUE: &str = r#"(
        format_version: 4,
        name: "blue",
        params: [(name: "tint", value: Color((0.0, 0.0, 1.0))), (name: "rough", value: Vec2((0.5, 0.5)))],
        switches: [(name: "wet", default: false)],
        nodes: { "tint": Param("tint"), "r": Param("rough"), "rx": Swizzle(input: "r", channels: "x") },
        output: (base_color: Some("tint"), roughness: Some("rx")),
    )"#;

    pub(super) fn project() -> PathBuf {
        let root = regia_test_utils::unique_temp_dir("matgraph-hierarchy");
        write(&root.join("assets/red.matgraph.ron"), RED);
        write(&root.join("assets/sub/blue.matgraph.ron"), BLUE);
        write(
            &root.join("assets/red_dark.matinst.ron"),
            r#"(format_version: 2, parent: "red.matgraph.ron",
                params: { "tint": Color((0.2, 0.0, 0.0)), "rough": Float(0.9) },
                textures: { "albedo": "b.png" })"#,
        );
        write(
            &root.join("assets/lost.matinst.ron"),
            r#"(format_version: 2, parent: "gone.matgraph.ron")"#,
        );
        root
    }

    #[test]
    fn the_scan_puts_instances_under_their_graphs() {
        let root = project();
        let tree = scan(&root);
        let graphs: Vec<_> = tree
            .graphs
            .iter()
            .map(|g| (g.name.as_str(), g.instances.len()))
            .collect();
        assert_eq!(graphs, vec![("red", 1), ("blue", 0)]);
        assert_eq!(tree.orphans.len(), 1);
        assert_eq!(tree.orphans[0].parent, "gone.matgraph.ron");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_reference_resolves_back_to_the_parent() {
        let root = project();
        let inst = root.join("assets/red_dark.matinst.ron");
        assert_eq!(
            parent_reference(Some(&inst), Some(&root), &root.join("assets/red.matgraph.ron")),
            Ok("red.matgraph.ron".to_string())
        );
        assert_eq!(
            parent_reference(
                Some(&inst),
                Some(&root),
                &root.join("assets/sub/blue.matgraph.ron")
            ),
            Ok("sub/blue.matgraph.ron".to_string())
        );
        let outside = regia_test_utils::unique_temp_dir("matgraph-hierarchy-outside").join("x.matgraph.ron");
        write(&outside, RED);
        assert!(parent_reference(Some(&inst), Some(&root), &outside).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn reparent_keeps_fitting_overrides_drops_the_rest_and_undoes_in_one_step() {
        let root = project();
        let mut world = World::new();
        world.insert_resource(crate::command::UndoStack::new());
        let mut editor = MatGraphEditor {
            root: Some(root.clone()),
            ..MatGraphEditor::default()
        };
        editor
            .open_instance(
                &root.join("assets/red_dark.matinst.ron"),
                &root.join("assets/red.matgraph.ron"),
            )
            .expect("open");
        let before = editor.doc.clone();
        world.insert_resource(editor);
        let report = reparent_open(&mut world, &root.join("assets/sub/blue.matgraph.ron")).expect("reparent");
        assert_eq!(report.parent, "sub/blue.matgraph.ron");
        assert_eq!(report.kept, vec!["param tint".to_string()]);
        assert_eq!(report.dropped.len(), 2, "{:?}", report.dropped);
        assert!(report.dropped[0].starts_with("param rough: the new parent declares another type"));
        assert!(report.dropped[1].starts_with("texture albedo"));
        {
            let editor = world.resource::<MatGraphEditor>();
            assert_eq!(editor.doc.graph.name, "blue");
            let inst = editor.doc.instance_meta().expect("still an instance");
            assert_eq!(inst.parent, "sub/blue.matgraph.ron");
            assert!(inst.validate(&editor.doc.graph).is_ok());
            assert!(editor.toast.contains("dropped"), "{}", editor.toast);
        }
        let blue = root.join("assets/sub/blue.matgraph.ron");
        let red = root.join("assets/red.matgraph.ron");
        let parent_path = |world: &World| world.resource::<MatGraphEditor>().parent_path.clone();
        assert_eq!(
            parent_path(&world),
            Some(blue.clone()),
            "the tab's parent is the new graph"
        );
        crate::command::undo_world(&mut world);
        assert_eq!(
            world.resource::<MatGraphEditor>().doc,
            before,
            "one undo step restores it"
        );
        // Review g16 row 8: the undo points the tab's parent back too (the autosave stores it).
        assert_eq!(parent_path(&world), Some(red), "undo restores the old parent");
        crate::command::redo_world(&mut world);
        assert_eq!(parent_path(&world), Some(blue), "redo moves it again");
        crate::command::undo_world(&mut world);
        // An instance's graph still cannot be edited by any other change.
        let refused = super::super::edit(&mut world, "x", None, |d| {
            d.graph.name = "changed".into();
            Ok(())
        });
        assert!(refused.is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
