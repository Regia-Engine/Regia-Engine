// SPDX-License-Identifier: MPL-2.0
//! Several selected nodes and the clipboard in the UI designer (ADR-9730,
//! `docs/specs/81-ui-designer.md` § 4.8): shared fields edited together, delete, duplicate,
//! group into a new panel, align and distribute, and copy, cut and paste of subtrees within a
//! document and across documents. Every edit is one undo step. The clipboard text is the format's
//! node JSON (a JSON array of nodes, the shape `editor.ui_designer_edit {op: "add", node}` takes);
//! pasted names are made unique in the target document and bindings are kept as they are.

use std::collections::{BTreeMap, HashSet};

use regia_ui::UiFile;
use regia_ui::authoring::UiNodeDef;
use serde_json::Value;

use crate::ui_designer::{
    DesignOp, DesignerError, NodePath, UiDesign, node_at, node_json, path_string, walk,
};
use crate::ui_designer_canvas::{AlignKind, GroupMovePlan, align_ops};
use crate::ui_designer_preview::PreviewFrame;

/// `nodes` without the root, duplicates and nodes inside another listed node, in document order.
pub fn top_level(nodes: &[NodePath]) -> Vec<NodePath> {
    let mut out: Vec<NodePath> = nodes.iter().filter(|p| !p.is_empty()).cloned().collect();
    out.sort();
    out.dedup();
    let all = out.clone();
    out.retain(|p| !all.iter().any(|a| a != p && p.starts_with(a)));
    out
}

fn check(root: &UiNodeDef, nodes: &[NodePath]) -> Result<(), DesignerError> {
    match nodes.iter().find(|p| node_at(root, p).is_none()) {
        Some(p) => Err(DesignerError::new(
            "bad_node",
            format!("no node at {}", path_string(p)),
        )),
        None => Ok(()),
    }
}

fn need_some(nodes: &[NodePath]) -> Result<(), DesignerError> {
    if nodes.is_empty() {
        return Err(DesignerError::new(
            "bad_node",
            "no node is selected (the root cannot be)",
        ));
    }
    Ok(())
}

/// The fields of `nodes` as dotted keys (`name`, `opacity`, `style.size`, …; `children` left out)
/// with their value where every node agrees, and the keys whose values differ (shown as mixed).
pub fn shared_fields(root: &UiNodeDef, nodes: &[NodePath]) -> (BTreeMap<String, Value>, Vec<String>) {
    let flat = |node: &UiNodeDef| {
        let mut out = BTreeMap::new();
        if let Value::Object(map) = node_json(node) {
            for (k, v) in map {
                match (k.as_str(), v) {
                    ("children", _) => {}
                    ("style", Value::Object(style)) => {
                        for (sk, sv) in style {
                            out.insert(format!("style.{sk}"), sv);
                        }
                    }
                    (_, v) => {
                        out.insert(k, v);
                    }
                }
            }
        }
        out
    };
    let maps: Vec<BTreeMap<String, Value>> =
        nodes.iter().filter_map(|p| node_at(root, p)).map(flat).collect();
    let keys: std::collections::BTreeSet<&String> = maps.iter().flat_map(|m| m.keys()).collect();
    let mut shared = BTreeMap::new();
    let mut mixed = Vec::new();
    for key in keys {
        let first = maps.first().and_then(|m| m.get(key));
        if maps.iter().all(|m| m.get(key) == first) {
            if let Some(v) = first {
                shared.insert(key.clone(), v.clone());
            }
        } else {
            mixed.push(key.clone());
        }
    }
    (shared, mixed)
}

/// Where each node ends up after every node of `dups` (top-level, document order) got a copy right
/// after itself.
fn after_duplicates(path: &[usize], dups: &[NodePath]) -> NodePath {
    let mut out = path.to_vec();
    for depth in 0..path.len() {
        let before = dups
            .iter()
            .filter(|d| d.len() == depth + 1 && d[..depth] == path[..depth] && d[depth] < path[depth])
            .count();
        out[depth] += before;
    }
    out
}

/// The names in `root`, depth first.
fn names(root: &UiNodeDef) -> HashSet<String> {
    let mut out = HashSet::new();
    walk(root, &mut Vec::new(), &mut |node, _| {
        out.extend(node.name.clone());
    });
    out
}

/// `name` made unique against `taken` (`hp` -> `hp_2`, `hp_2` -> `hp_3`), then taken.
fn unique_name(name: &str, taken: &mut HashSet<String>) -> String {
    if taken.insert(name.to_owned()) {
        return name.to_owned();
    }
    let base = match name.rsplit_once('_') {
        Some((base, n)) if !base.is_empty() && n.parse::<u32>().is_ok() => base,
        _ => name,
    };
    let mut n = 2u32;
    loop {
        let candidate = format!("{base}_{n}");
        if taken.insert(candidate.clone()) {
            return candidate;
        }
        n = n.saturating_add(1);
    }
}

fn rename_unique(node: &mut UiNodeDef, taken: &mut HashSet<String>) {
    if let Some(name) = &node.name {
        node.name = Some(unique_name(name, taken));
    }
    for child in &mut node.children {
        rename_unique(child, taken);
    }
}

/// The clipboard text of `nodes` (top level, document order): the format's node JSON, a JSON array.
pub fn copy_text(root: &UiNodeDef, nodes: &[NodePath]) -> Result<String, DesignerError> {
    let nodes = top_level(nodes);
    need_some(&nodes)?;
    check(root, &nodes)?;
    let values: Vec<Value> = nodes
        .iter()
        .filter_map(|p| node_at(root, p))
        .map(node_json)
        .collect();
    serde_json::to_string_pretty(&values).map_err(|e| DesignerError::new("bad_value", e.to_string()))
}

/// Nodes from clipboard text: a JSON array of nodes or one node, decoded by the format's types.
pub fn parse_clipboard(text: &str) -> Result<Vec<UiNodeDef>, DesignerError> {
    let value: Value = serde_json::from_str(text.trim())
        .map_err(|e| DesignerError::new("bad_value", format!("the clipboard is not node JSON: {e}")))?;
    let nodes = match value {
        Value::Array(items) => items,
        one @ Value::Object(_) => vec![one],
        _ => return Err(DesignerError::new("bad_value", "the clipboard holds no UI nodes")),
    };
    if nodes.is_empty() {
        return Err(DesignerError::new("bad_value", "the clipboard holds no UI nodes"));
    }
    nodes
        .into_iter()
        .map(|v| {
            serde_json::from_value::<UiNodeDef>(v)
                .map_err(|e| DesignerError::new("bad_value", format!("node: {e}")))
        })
        .collect()
}

/// Where a paste lands by default: after the primary selection among its siblings, or at the end of
/// the root when the root (or nothing) is selected.
pub fn paste_target(doc: &UiDesign) -> (NodePath, Option<usize>) {
    match doc.selection.as_deref().and_then(|p| p.split_last()) {
        Some((&last, parent)) => (parent.to_vec(), Some(last + 1)),
        None => (Vec::new(), None),
    }
}

impl UiDesign {
    /// Sets one field on every node of `nodes` as one undo step (a drag merges while `merge` is
    /// set, as [`UiDesign::apply_merged`] does for one node).
    pub fn set_many(
        &mut self,
        nodes: &[NodePath],
        key: &str,
        value: &Value,
        merge: bool,
    ) -> Result<Vec<NodePath>, DesignerError> {
        let mut seen = HashSet::new();
        let nodes: Vec<NodePath> = nodes
            .iter()
            .filter(|p| seen.insert((*p).clone()))
            .cloned()
            .collect();
        if nodes.is_empty() {
            return Err(DesignerError::new("bad_node", "no node is selected"));
        }
        let ops = nodes
            .iter()
            .map(|p| DesignOp::Set {
                node: p.clone(),
                key: key.to_owned(),
                value: value.clone(),
            })
            .collect();
        if merge {
            let token = format!(
                "{}|{key}",
                nodes.iter().map(|p| path_string(p)).collect::<Vec<_>>().join(",")
            );
            self.apply_batch_merged(ops, &token)
        } else {
            self.apply_batch(ops)
        }
    }

    /// Removes every node of `nodes` (and what they hold) as one undo step; the parent of the first
    /// is selected afterwards.
    pub fn remove_many(&mut self, nodes: &[NodePath]) -> Result<NodePath, DesignerError> {
        let nodes = top_level(nodes);
        need_some(&nodes)?;
        check(&self.file().root, &nodes)?;
        let parent = nodes[0][..nodes[0].len() - 1].to_vec();
        // Last first: a removal never shifts a path that sorts before it.
        let ops = nodes.iter().rev().cloned().map(DesignOp::Remove).collect();
        self.apply_batch(ops)?;
        self.select(Some(parent.clone()))?;
        Ok(parent)
    }

    /// Copies every node of `nodes` right after itself as one undo step; the copies are selected.
    pub fn duplicate_many(&mut self, nodes: &[NodePath]) -> Result<Vec<NodePath>, DesignerError> {
        let nodes = top_level(nodes);
        need_some(&nodes)?;
        check(&self.file().root, &nodes)?;
        let ops = nodes.iter().rev().cloned().map(DesignOp::Duplicate).collect();
        self.apply_batch(ops)?;
        let copies: Vec<NodePath> = nodes
            .iter()
            .map(|p| {
                let mut c = after_duplicates(p, &nodes);
                if let Some(last) = c.last_mut() {
                    *last += 1;
                }
                c
            })
            .collect();
        self.select_many(copies.clone())?;
        Ok(copies)
    }

    /// Wraps `nodes` (siblings of one parent) in a new panel at the first one's place, keeping
    /// their order, as one undo step; the new panel is selected.
    pub fn group(&mut self, nodes: &[NodePath], name: Option<&str>) -> Result<NodePath, DesignerError> {
        let nodes = top_level(nodes);
        need_some(&nodes)?;
        check(&self.file().root, &nodes)?;
        let parent = nodes[0][..nodes[0].len() - 1].to_vec();
        if nodes.iter().any(|p| p[..p.len() - 1] != parent[..]) {
            return Err(DesignerError::new(
                "bad_move",
                "only siblings (nodes of one parent) group into a panel",
            ));
        }
        let mut file: UiFile = self.file().clone();
        let mut taken = names(&file.root);
        let Some(host) = node_path_mut(&mut file.root, &parent) else {
            return Err(DesignerError::new(
                "bad_node",
                format!("no node at {}", path_string(&parent)),
            ));
        };
        let indices: Vec<usize> = nodes.iter().filter_map(|p| p.last().copied()).collect();
        let at = indices[0];
        let mut group = UiNodeDef {
            name: name.map(|n| unique_name(n, &mut taken)),
            ..UiNodeDef::default()
        };
        for &i in indices.iter().rev() {
            group.children.insert(0, host.children.remove(i));
        }
        host.children.insert(at, group);
        let mut path = parent;
        path.push(at);
        self.replace_selecting(file, vec![path.clone()]);
        Ok(path)
    }

    /// Pastes clipboard `text` under `parent` at `index` (`None` appends) as one undo step: names
    /// that exist in this document (or earlier in the paste) get a `_2`, `_3` suffix; bindings are
    /// kept. The pasted nodes are selected.
    pub fn paste(
        &mut self,
        text: &str,
        parent: &[usize],
        index: Option<usize>,
    ) -> Result<Vec<NodePath>, DesignerError> {
        let mut pasted = parse_clipboard(text)?;
        let mut file: UiFile = self.file().clone();
        let mut taken = names(&file.root);
        for node in &mut pasted {
            rename_unique(node, &mut taken);
        }
        let Some(host) = node_path_mut(&mut file.root, parent) else {
            return Err(DesignerError::new(
                "bad_node",
                format!("no node at {}", path_string(parent)),
            ));
        };
        let at = index.unwrap_or(host.children.len());
        if at > host.children.len() {
            return Err(DesignerError::new(
                "bad_value",
                format!(
                    "index {at} is past the {} children of {}",
                    host.children.len(),
                    path_string(parent)
                ),
            ));
        }
        let count = pasted.len();
        for (i, node) in pasted.into_iter().enumerate() {
            host.children.insert(at + i, node);
        }
        let paths: Vec<NodePath> = (at..at + count)
            .map(|i| {
                let mut p = parent.to_vec();
                p.push(i);
                p
            })
            .collect();
        self.replace_selecting(file, paths.clone());
        Ok(paths)
    }

    /// Aligns or distributes `nodes` by their rects in `frame` (a preview of this document) as one
    /// undo step.
    pub fn align(
        &mut self,
        frame: &PreviewFrame,
        nodes: &[NodePath],
        kind: AlignKind,
    ) -> Result<(), DesignerError> {
        check(&self.file().root, nodes)?;
        let ops = align_ops(&self.file().root, frame, nodes, kind)?;
        let selection = nodes.to_vec();
        self.apply_batch(ops)?;
        self.select_many(selection)
    }

    /// Applies a planned group move ([`crate::ui_designer_canvas::plan_group_move`], ADR-9980) as
    /// one undo step and keeps `selection` selected. With `merge`, every call of the same token until
    /// [`UiDesign::end_merge`] shares that step (one step for a whole drag in the panel).
    pub fn move_many(
        &mut self,
        plan: GroupMovePlan,
        selection: &[NodePath],
        merge: Option<&str>,
    ) -> Result<(), DesignerError> {
        if plan.ops.is_empty() {
            return Ok(());
        }
        match merge {
            Some(token) => self.apply_batch_merged(plan.ops, token)?,
            None => self.apply_batch(plan.ops)?,
        };
        self.select_many(selection.to_vec())
    }
}

fn node_path_mut<'a>(root: &'a mut UiNodeDef, path: &[usize]) -> Option<&'a mut UiNodeDef> {
    path.iter().try_fold(root, |node, &i| node.children.get_mut(i))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_designer::{UiDesigner, preset};
    use regia_ui::{UiMigrators, parse_ui_ron};
    use serde_json::json;

    fn designer_with(kinds: &[&str]) -> UiDesigner {
        let mut designer = UiDesigner::default();
        designer.new_document();
        let doc = designer.doc_mut().expect("doc");
        for kind in kinds {
            doc.apply(DesignOp::Add {
                parent: Vec::new(),
                index: None,
                node: Box::new(preset(kind).expect("preset")),
            })
            .expect("add");
        }
        designer
    }

    /// Spec 81 § 4.8 acceptance: an `opacity` set on three selected nodes is one undo step, and
    /// one undo restores all three.
    #[test]
    fn a_shared_field_set_on_three_nodes_is_one_undo_step() {
        let mut designer = designer_with(&["panel", "text", "button"]);
        let doc = designer.doc_mut().expect("doc");
        let before = doc.text().expect("text");
        doc.select_many(vec![vec![0], vec![1], vec![2]]).expect("select");
        let undo = doc.undo_len();
        let nodes = doc.selected_nodes();
        assert_eq!(nodes.len(), 3);
        doc.set_many(&nodes, "opacity", &json!(0.5), false).expect("set");
        assert_eq!(doc.undo_len(), undo + 1, "one step");
        for i in 0..3 {
            assert_eq!(doc.file().root.children[i].opacity, Some(0.5));
        }
        let (shared, mixed) = shared_fields(&doc.file().root, &nodes);
        assert_eq!(shared.get("opacity"), Some(&json!(0.5)));
        assert!(
            mixed.contains(&"widget".to_owned()) || mixed.contains(&"text".to_owned()),
            "{mixed:?}"
        );
        assert_eq!(doc.selected_nodes().len(), 3, "the selection stays");
        doc.undo().expect("undo");
        assert_eq!(doc.text().expect("text"), before);
    }

    /// Delete, duplicate and group of several nodes are one step each; a group keeps the order.
    #[test]
    fn delete_duplicate_and_group_many() {
        let mut designer = designer_with(&["panel", "text", "button", "slider"]);
        let doc = designer.doc_mut().expect("doc");
        let undo = doc.undo_len();
        let copies = doc.duplicate_many(&[vec![1], vec![3]]).expect("duplicate");
        assert_eq!(copies, vec![vec![2], vec![5]]);
        assert_eq!(doc.undo_len(), undo + 1);
        let kinds: Vec<&str> = doc
            .file()
            .root
            .children
            .iter()
            .map(crate::ui_designer::node_kind)
            .collect();
        assert_eq!(kinds, ["panel", "text", "text", "button", "slider", "slider"]);
        doc.remove_many(&[vec![2], vec![5]]).expect("remove");
        assert_eq!(doc.file().root.children.len(), 4);
        let group = doc.group(&[vec![3], vec![1]], Some("box")).expect("group");
        assert_eq!(group, vec![1]);
        let g = &doc.file().root.children[1];
        assert_eq!(g.name.as_deref(), Some("box"));
        let inner: Vec<&str> = g.children.iter().map(crate::ui_designer::node_kind).collect();
        assert_eq!(inner, ["text", "slider"]);
        assert_eq!(doc.file().root.children.len(), 3);
        let e = doc.group(&[vec![0], vec![1, 0]], None).expect_err("not siblings");
        assert_eq!(e.code, "bad_move");
        assert_eq!(doc.undo_len(), undo + 3);
    }

    /// Spec 81 § 4.8 acceptance: a subtree copied in one document pastes into a second one with its
    /// names made unique and its bindings kept, saves, and the saved file loads with the runtime
    /// loader.
    #[test]
    fn a_subtree_pasted_into_a_second_document_loads() {
        let mut designer = designer_with(&["panel"]);
        let doc = designer.doc_mut().expect("doc");
        doc.apply(DesignOp::Set {
            node: vec![0],
            key: "name".to_owned(),
            value: json!("hp"),
        })
        .expect("name");
        doc.apply(DesignOp::Add {
            parent: vec![0],
            index: None,
            node: Box::new(
                serde_json::from_value(json!({
                    "name": "bar",
                    "widget": { "progress": 0.5 },
                    "bindings": [{ "source": "HudVm.health", "target": "progress" }],
                }))
                .expect("node"),
            ),
        })
        .expect("child");
        let text = copy_text(&doc.file().root, &[vec![0]]).expect("copy");
        designer.clipboard = Some(text.clone());

        designer.new_document();
        let doc = designer.doc_mut().expect("doc");
        doc.apply(DesignOp::Add {
            parent: Vec::new(),
            index: None,
            node: Box::new(UiNodeDef {
                name: Some("hp".to_owned()),
                ..UiNodeDef::default()
            }),
        })
        .expect("taken name");
        let undo = doc.undo_len();
        let pasted = doc.paste(&text, &[], None).expect("paste");
        assert_eq!(pasted, vec![vec![1]]);
        assert_eq!(doc.undo_len(), undo + 1);
        let node = &doc.file().root.children[1];
        assert_eq!(node.name.as_deref(), Some("hp_2"));
        assert_eq!(node.children[0].name.as_deref(), Some("bar"));
        assert_eq!(node.children[0].bindings[0].source, "HudVm.health");
        let dir = regia_test_utils::unique_temp_dir("ui-designer-paste");
        let path = dir.join("second.ui.ron");
        doc.save(Some(path.clone())).expect("save");
        let saved = std::fs::read_to_string(&path).expect("read");
        let loaded = parse_ui_ron(&saved, "second.ui.ron", &UiMigrators::default())
            .expect("the runtime loader reads it");
        assert_eq!(
            loaded.root.children[1].children[0].bindings[0].source,
            "HudVm.health"
        );
        let frame =
            crate::ui_designer_preview::preview(&loaded, &doc.preview, None).expect("spawns and lays out");
        assert!(frame.nodes.iter().any(|n| n.name.as_deref() == Some("hp_2")));
        assert!(parse_clipboard("[]").is_err());
        assert!(parse_clipboard("{\"widget\": \"rocket\"}").is_err());
    }
}
