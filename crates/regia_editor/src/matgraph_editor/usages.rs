// SPDX-License-Identifier: MPL-2.0
//! Find usages (w12 P-173, P-183, P-029; ADR-7851): from a parameter, texture, switch,
//! function-call or constant node, the project's graphs, functions and instances that use the
//! same parameter name, texture, switch, function or constant. The node context menu's "Find
//! usages" lists them in the side column (a click opens the file); the agent twin is
//! `editor.matgraph_find_usages`.

use std::path::{Path, PathBuf};

use regia_matgraph::{FunctionSource, MaterialFunction, MaterialGraph, MaterialInstance, Node};
use serde_json::{Value, json};

use super::params_panel::{DeclKind, reads};

/// Most directory entries a project scan visits (the function users scan's guard).
pub const SCAN_LIMIT: usize = super::function::USERS_SCAN_LIMIT;

/// What a usage search looks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsageKind {
    /// A parameter name (`Param` nodes, declarations, instance overrides).
    Param,
    /// A texture name (sampling nodes, declarations, instance overrides).
    Texture,
    /// A static switch name.
    Switch,
    /// A material function: by its name in the graph, or by its `.matfunc.ron` file.
    Function,
    /// A constant node's value (`Float(0.5)`, as RON).
    Constant,
}

impl UsageKind {
    /// Every kind.
    pub const ALL: [Self; 5] = [
        Self::Param,
        Self::Texture,
        Self::Switch,
        Self::Function,
        Self::Constant,
    ];

    /// The protocol name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Param => "param",
            Self::Texture => "texture",
            Self::Switch => "switch",
            Self::Function => "function",
            Self::Constant => "constant",
        }
    }

    /// Parses [`Self::name`].
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.name() == text)
    }

    fn decl(self) -> Option<DeclKind> {
        match self {
            Self::Param => Some(DeclKind::Param),
            Self::Texture => Some(DeclKind::Texture),
            Self::Switch => Some(DeclKind::Switch),
            _ => None,
        }
    }
}

/// A usage search.
#[derive(Clone, Debug, PartialEq)]
pub struct UsageQuery {
    /// What is searched.
    pub kind: UsageKind,
    /// The name (a constant: its node RON).
    pub name: String,
    /// A function's `.matfunc.ron` (absolute), when the searched call reads a file: graphs that
    /// name the same file under another name match too.
    pub file: Option<PathBuf>,
}

/// One file that uses the searched item.
#[derive(Clone, Debug, PartialEq)]
pub struct Usage {
    /// The file.
    pub path: PathBuf,
    /// `graph`, `function` or `instance`.
    pub doc: &'static str,
    /// The nodes that use it (graphs and functions).
    pub nodes: Vec<String>,
    /// How: `declares`, `reads`, `calls`, `overrides`, `inherits`.
    pub how: Vec<&'static str>,
}

impl Usage {
    /// As protocol JSON.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "path": self.path.display().to_string(),
            "doc": self.doc,
            "nodes": self.nodes,
            "how": self.how,
        })
    }
}

/// The query a node's "Find usages" runs, or `None` for a node that names nothing searchable.
/// `dir` and `root` resolve a function file.
#[must_use]
pub fn query_for_node(
    graph: &MaterialGraph,
    node: &str,
    dir: Option<&Path>,
    root: Option<&Path>,
) -> Option<UsageQuery> {
    let n = graph.nodes.get(node)?;
    let simple = |kind, name: &str| UsageQuery {
        kind,
        name: name.to_string(),
        file: None,
    };
    Some(match n {
        Node::Param(p) => simple(UsageKind::Param, p),
        Node::Sample { texture, .. } | Node::Triplanar { texture, .. } | Node::Parallax { texture, .. } => {
            simple(UsageKind::Texture, texture)
        }
        Node::Switch { switch, .. } => simple(UsageKind::Switch, switch),
        Node::Call { function, .. } => UsageQuery {
            kind: UsageKind::Function,
            name: function.clone(),
            file: match graph.functions.get(function) {
                Some(FunctionSource::File(rel)) => super::model::graph_file_path(dir, root, rel)
                    .ok()
                    .map(|p| std::fs::canonicalize(&p).unwrap_or(p)),
                _ => None,
            },
        },
        Node::Float(_) | Node::Vec2(_) | Node::Vec3(_) | Node::Vec4(_) => {
            simple(UsageKind::Constant, &constant_key(n)?)
        }
        _ => return None,
    })
}

/// A constant node as compact RON (the constant search's key).
#[must_use]
pub fn constant_key(node: &Node) -> Option<String> {
    matches!(
        node,
        Node::Float(_) | Node::Vec2(_) | Node::Vec3(_) | Node::Vec4(_)
    )
    .then(|| ron::ser::to_string(node).ok())
    .flatten()
}

/// Files under `root` whose names end with one of `suffixes` (hidden directories and `target`
/// skipped, at most [`SCAN_LIMIT`] entries visited), sorted.
#[must_use]
pub fn project_files(root: &Path, suffixes: &[&str]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    let mut visited = 0usize;
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            visited += 1;
            if visited > SCAN_LIMIT {
                break;
            }
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if path.is_dir() {
                if !name.starts_with('.') && name != "target" {
                    stack.push(path);
                }
            } else if suffixes.iter().any(|s| name.ends_with(s)) {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

/// Nodes of `nodes` that use the searched item, and how.
fn node_hits(
    nodes: &std::collections::BTreeMap<String, Node>,
    functions: Option<&std::collections::BTreeMap<String, FunctionSource>>,
    query: &UsageQuery,
    dir: Option<&Path>,
    root: Option<&Path>,
) -> Vec<String> {
    nodes
        .iter()
        .filter(|(_, n)| match (query.kind, n) {
            (UsageKind::Function, Node::Call { function, .. }) => {
                if let (Some(file), Some(FunctionSource::File(rel))) =
                    (&query.file, functions.and_then(|f| f.get(function)))
                {
                    super::model::graph_file_path(dir, root, rel)
                        .map(|p| std::fs::canonicalize(&p).unwrap_or(p))
                        .is_ok_and(|p| &p == file)
                } else {
                    query.file.is_none() && function == &query.name
                }
            }
            (UsageKind::Constant, n) => constant_key(n).as_deref() == Some(query.name.as_str()),
            (kind, n) => kind.decl().is_some_and(|d| reads(n, d, &query.name)),
        })
        .map(|(k, _)| k.clone())
        .collect()
}

fn declares(graph: &MaterialGraph, query: &UsageQuery) -> bool {
    match query.kind {
        UsageKind::Param => graph.params.iter().any(|p| p.name == query.name),
        UsageKind::Texture => graph.textures.iter().any(|t| t.name == query.name),
        UsageKind::Switch => graph.switches.iter().any(|s| s.name == query.name),
        _ => false,
    }
}

/// Every graph, function file and instance under `root` that uses the searched item, sorted by
/// path. `project` is the project root (function files and parents resolve against it).
/// Unreadable or unparsable files are skipped.
#[must_use]
pub fn find_usages(root: &Path, project: Option<&Path>, query: &UsageQuery) -> Vec<Usage> {
    let _span = tracing::info_span!(target: "regia_editor", "matgraph_find_usages").entered();
    let files = project_files(root, &[".matgraph.ron", ".matfunc.ron", ".matinst.ron"]);
    let mut out = Vec::new();
    let mut graphs_using = Vec::new();
    for path in files
        .iter()
        .filter(|p| !p.to_string_lossy().ends_with(".matinst.ron"))
    {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let dir = path.parent();
        let is_graph = path.to_string_lossy().ends_with(".matgraph.ron");
        let (nodes, how_decl) = if is_graph {
            let Ok(graph) = MaterialGraph::from_ron(&text) else {
                continue;
            };
            let nodes = node_hits(&graph.nodes, Some(&graph.functions), query, dir, project);
            (nodes, declares(&graph, query))
        } else {
            let Ok(function) = MaterialFunction::from_ron(&text) else {
                continue;
            };
            (node_hits(&function.nodes, None, query, dir, project), false)
        };
        if nodes.is_empty() && !how_decl {
            continue;
        }
        let mut how = Vec::new();
        if how_decl {
            how.push("declares");
        }
        if !nodes.is_empty() {
            how.push(if query.kind == UsageKind::Function {
                "calls"
            } else {
                "reads"
            });
        }
        if is_graph {
            graphs_using.push(std::fs::canonicalize(path).unwrap_or_else(|_| path.clone()));
        }
        out.push(Usage {
            path: path.clone(),
            doc: if is_graph { "graph" } else { "function" },
            nodes,
            how,
        });
    }
    for path in files
        .iter()
        .filter(|p| p.to_string_lossy().ends_with(".matinst.ron"))
    {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let Ok(instance) = MaterialInstance::from_ron(&text) else {
            continue;
        };
        let overrides = match query.kind {
            UsageKind::Param => instance.params.contains_key(&query.name),
            UsageKind::Texture => instance.textures.contains_key(&query.name),
            UsageKind::Switch => instance.switches.contains_key(&query.name),
            _ => false,
        };
        let inherits = super::instance_parent_path(Some(path), project, &instance.parent)
            .ok()
            .map(|p| std::fs::canonicalize(&p).unwrap_or(p))
            .is_some_and(|p| graphs_using.contains(&p));
        if !overrides && !inherits {
            continue;
        }
        let mut how = Vec::new();
        if overrides {
            how.push("overrides");
        }
        if inherits {
            how.push("inherits");
        }
        out.push(Usage {
            path: path.clone(),
            doc: "instance",
            nodes: Vec::new(),
            how,
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// The side-column list of the last Find usages answer; a click on a row opens the file.
pub fn usages_ui(ui: &mut egui::Ui, editor: &mut super::MatGraphEditor, open: &mut Option<String>) {
    let Some((query, result)) = &editor.ui.parity.usages else {
        return;
    };
    let mut close = false;
    ui.horizontal(|ui| {
        ui.strong(format!("Usages of {} `{}`", query.kind.name(), query.name));
        close = ui.small_button("Close").clicked();
    });
    match result {
        Ok(list) if list.is_empty() => {
            ui.weak("No file in the project uses it.");
        }
        Ok(list) => {
            for usage in list {
                let label = format!(
                    "{} {} ({}{})",
                    usage.doc,
                    usage.path.display(),
                    usage.how.join(", "),
                    if usage.nodes.is_empty() {
                        String::new()
                    } else {
                        format!(": {}", usage.nodes.join(", "))
                    }
                );
                if ui.link(label).clicked() {
                    *open = Some(usage.path.display().to_string());
                }
            }
        }
        Err(e) => {
            ui.colored_label(crate::theme::ui_tokens(ui.ctx()).accents.error_text, e);
        }
    }
    if close {
        editor.ui.parity.usages = None;
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

    const A: &str = r#"(
        format_version: 4,
        params: [(name: "tint", value: Color((1.0, 0.0, 0.0)))],
        functions: { "wobble": File("fns/wobble.matfunc.ron") },
        nodes: { "tint": Param("tint"), "half": Float(0.5), "w": Call(function: "wobble", args: {}) },
        output: (base_color: Some("tint"), roughness: Some("half")),
    )"#;
    const B: &str = r#"(
        format_version: 4,
        params: [(name: "rough", value: Float(0.3))],
        functions: { "wiggle": File("fns/wobble.matfunc.ron") },
        nodes: { "r": Param("rough"), "half": Float(0.5), "c": Call(function: "wiggle", args: {}) },
        output: (roughness: Some("r")),
    )"#;
    const F: &str = r#"(
        format_version: 3,
        nodes: { "t": Param("tint"), "k": Float(2.0) },
        outputs: { "out": "t" },
    )"#;

    fn project() -> PathBuf {
        let root = regia_test_utils::unique_temp_dir("matgraph-usages");
        write(&root.join("assets/a.matgraph.ron"), A);
        write(&root.join("assets/b.matgraph.ron"), B);
        write(&root.join("assets/fns/wobble.matfunc.ron"), F);
        write(
            &root.join("assets/a_red.matinst.ron"),
            r#"(format_version: 2, parent: "a.matgraph.ron", params: { "tint": Color((0.0, 1.0, 0.0)) })"#,
        );
        write(
            &root.join("assets/b_plain.matinst.ron"),
            r#"(format_version: 2, parent: "b.matgraph.ron")"#,
        );
        write(&root.join("assets/broken.matgraph.ron"), "not ron");
        root
    }

    fn names(list: &[Usage], root: &Path) -> Vec<(String, &'static str, Vec<&'static str>)> {
        list.iter()
            .map(|u| {
                (
                    u.path.strip_prefix(root).unwrap_or(&u.path).display().to_string(),
                    u.doc,
                    u.how.clone(),
                )
            })
            .collect()
    }

    #[test]
    fn a_parameter_is_found_in_graphs_functions_and_instances() {
        let root = project();
        let graph = MaterialGraph::from_ron(A).expect("a");
        let assets = root.join("assets");
        let q = query_for_node(&graph, "tint", Some(&assets), Some(&root)).expect("param query");
        assert_eq!(q.kind, UsageKind::Param);
        let found = find_usages(&root, Some(&root), &q);
        assert_eq!(
            names(&found, &root),
            vec![
                ("assets/a.matgraph.ron".into(), "graph", vec!["declares", "reads"]),
                (
                    "assets/a_red.matinst.ron".into(),
                    "instance",
                    vec!["overrides", "inherits"]
                ),
                ("assets/fns/wobble.matfunc.ron".into(), "function", vec!["reads"]),
            ]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_function_file_is_found_under_any_name_and_a_constant_by_value() {
        let root = project();
        let graph = MaterialGraph::from_ron(A).expect("a");
        let assets = root.join("assets");
        let q = query_for_node(&graph, "w", Some(&assets), Some(&root)).expect("call query");
        assert_eq!(q.kind, UsageKind::Function);
        assert!(q.file.is_some());
        let found = find_usages(&root, Some(&root), &q);
        assert_eq!(
            names(&found, &root),
            vec![
                ("assets/a.matgraph.ron".into(), "graph", vec!["calls"]),
                ("assets/a_red.matinst.ron".into(), "instance", vec!["inherits"]),
                ("assets/b.matgraph.ron".into(), "graph", vec!["calls"]),
                ("assets/b_plain.matinst.ron".into(), "instance", vec!["inherits"]),
            ]
        );
        assert_eq!(found[2].nodes, vec!["c".to_string()]);
        let q = query_for_node(&graph, "half", None, None).expect("constant");
        assert_eq!((q.kind, q.name.as_str()), (UsageKind::Constant, "Float(0.5)"));
        let found = find_usages(&root, Some(&root), &q);
        assert_eq!(found.iter().filter(|u| u.doc == "graph").count(), 2);
        assert!(query_for_node(&graph, "nope", None, None).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }
}
