// SPDX-License-Identifier: MPL-2.0
//! The MaterialX document model the importer reads: top-level nodes (shaders and materials),
//! node graphs with their interface inputs and outputs, and node definitions (MaterialX
//! Specification, "MaterialX Document Structure" and "Custom Nodes").
//!
//! The XML is parsed with `roxmltree` with DTDs refused and a node limit, so a hostile file
//! costs bounded memory. Scoped attributes (`fileprefix`, `colorspace`) are resolved to their
//! effective value on every input while the tree is walked, so the translator never looks back
//! up the tree.

use std::collections::BTreeMap;

use super::{MAX_XML_NODES, MaterialXError};

/// One `<input>` (or nodegraph interface input, or nodedef input).
#[derive(Clone, Debug, Default)]
pub(crate) struct Input {
    /// `name`.
    pub name: String,
    /// `type` (`float`, `color3`, `vector2`, `filename`, ...).
    pub ty: String,
    /// `value`, when the input is a constant.
    pub value: Option<String>,
    /// `nodename`: the node (in the same scope) it connects to.
    pub nodename: Option<String>,
    /// `nodegraph`: the top-level node graph it connects to (with `output`).
    pub nodegraph: Option<String>,
    /// `output`: the output of a multi-output node or of a node graph.
    pub output: Option<String>,
    /// `interfacename`: the enclosing node graph's interface input it reads.
    pub interfacename: Option<String>,
    /// Effective colour space (the input's own, else the nearest enclosing element's).
    pub colorspace: Option<String>,
    /// Effective file prefix (concatenated from the enclosing elements).
    pub fileprefix: String,
}

impl Input {
    /// True when the input is wired to something rather than holding a value.
    pub fn is_connected(&self) -> bool {
        self.nodename.is_some() || self.nodegraph.is_some() || self.interfacename.is_some()
    }
}

/// One node element (`<image>`, `<multiply>`, `<standard_surface>`, `<surfacematerial>`, ...).
#[derive(Clone, Debug, Default)]
pub(crate) struct NodeElem {
    /// The element name: the node's category.
    pub category: String,
    /// `name`.
    pub name: String,
    /// `type` of its output (`multioutput` for several).
    pub ty: String,
    /// Its inputs in document order.
    pub inputs: Vec<Input>,
    /// 1-based source line (diagnostics).
    pub line: u32,
}

impl NodeElem {
    /// The input named `name`.
    pub fn input(&self, name: &str) -> Option<&Input> {
        self.inputs.iter().find(|i| i.name == name)
    }
}

/// One `<output>` of a node graph.
#[derive(Clone, Debug, Default)]
pub(crate) struct GraphOutput {
    /// `name`.
    pub name: String,
    /// `type`.
    pub ty: String,
    /// `nodename` inside the graph.
    pub nodename: Option<String>,
    /// `output` of a multi-output node.
    pub output: Option<String>,
    /// `interfacename`: a pass-through of an interface input.
    pub interfacename: Option<String>,
    /// `value`: a constant output.
    pub value: Option<String>,
}

/// One `<nodegraph>`: a compound graph, or the implementation of a [`NodeDef`].
#[derive(Clone, Debug, Default)]
pub(crate) struct NodeGraph {
    /// `nodedef`: set when the graph implements a custom node.
    pub nodedef: Option<String>,
    /// Nodes by name.
    pub nodes: BTreeMap<String, NodeElem>,
    /// Outputs in document order.
    pub outputs: Vec<GraphOutput>,
    /// Interface inputs declared on the graph itself.
    pub interface: Vec<Input>,
}

/// One `<nodedef>`: a custom node's interface.
#[derive(Clone, Debug, Default)]
pub(crate) struct NodeDef {
    /// `node`: the category instances use.
    pub node: String,
    /// Inputs with their default values.
    pub inputs: Vec<Input>,
    /// Outputs `(name, type)`.
    pub outputs: Vec<(String, String)>,
}

/// A parsed document.
#[derive(Clone, Debug, Default)]
pub(crate) struct Document {
    /// `version` of the root.
    pub version: String,
    /// Top-level nodes by name.
    pub nodes: BTreeMap<String, NodeElem>,
    /// Top-level node names in document order.
    pub order: Vec<String>,
    /// Node graphs by name.
    pub graphs: BTreeMap<String, NodeGraph>,
    /// Node definitions by name.
    pub nodedefs: BTreeMap<String, NodeDef>,
}

impl Document {
    /// The nodedef implementing category `category` with output type `ty`, if the document
    /// defines one (the first by name when several match the category).
    pub fn nodedef_for(&self, category: &str, ty: &str) -> Option<(&str, &NodeDef)> {
        let mut fallback = None;
        for (name, def) in &self.nodedefs {
            if def.node != category {
                continue;
            }
            if def.outputs.iter().any(|(_, t)| t == ty) {
                return Some((name, def));
            }
            fallback.get_or_insert((name.as_str(), def));
        }
        fallback
    }

    /// The node graph implementing nodedef `nodedef`.
    pub fn implementation(&self, nodedef: &str) -> Option<&NodeGraph> {
        self.graphs
            .values()
            .find(|g| g.nodedef.as_deref() == Some(nodedef))
    }
}

/// Scoped attributes inherited by child elements.
#[derive(Clone, Default)]
struct Scope {
    fileprefix: String,
    colorspace: Option<String>,
}

impl Scope {
    fn enter(&self, node: roxmltree::Node<'_, '_>) -> Self {
        let mut next = self.clone();
        if let Some(prefix) = node.attribute("fileprefix") {
            next.fileprefix.push_str(prefix);
        }
        if let Some(cs) = node.attribute("colorspace") {
            next.colorspace = Some(cs.to_string());
        }
        next
    }
}

fn attr(node: roxmltree::Node<'_, '_>, name: &str) -> Option<String> {
    node.attribute(name).map(str::to_string)
}

fn line_of(doc: &roxmltree::Document<'_>, node: roxmltree::Node<'_, '_>) -> u32 {
    doc.text_pos_at(node.range().start).row
}

fn read_input(node: roxmltree::Node<'_, '_>, scope: &Scope) -> Input {
    let scope = scope.enter(node);
    Input {
        name: attr(node, "name").unwrap_or_default(),
        ty: attr(node, "type").unwrap_or_default(),
        value: attr(node, "value"),
        nodename: attr(node, "nodename").filter(|s| !s.is_empty()),
        nodegraph: attr(node, "nodegraph").filter(|s| !s.is_empty()),
        output: attr(node, "output").filter(|s| !s.is_empty()),
        interfacename: attr(node, "interfacename").filter(|s| !s.is_empty()),
        colorspace: scope.colorspace,
        fileprefix: scope.fileprefix,
    }
}

fn read_node(
    doc: &roxmltree::Document<'_>,
    node: roxmltree::Node<'_, '_>,
    scope: &Scope,
) -> Result<NodeElem, MaterialXError> {
    let line = line_of(doc, node);
    let name = attr(node, "name").ok_or_else(|| MaterialXError::Structure {
        line,
        message: format!("<{}> has no name", node.tag_name().name()),
    })?;
    let scope = scope.enter(node);
    let inputs = node
        .children()
        .filter(|c| c.is_element() && c.tag_name().name() == "input")
        .map(|c| read_input(c, &scope))
        .collect();
    Ok(NodeElem {
        category: node.tag_name().name().to_string(),
        name,
        ty: attr(node, "type").unwrap_or_default(),
        inputs,
        line,
    })
}

/// Elements that are not nodes inside a document or graph.
fn is_structural(tag: &str) -> bool {
    matches!(
        tag,
        "input"
            | "output"
            | "nodedef"
            | "nodegraph"
            | "implementation"
            | "typedef"
            | "look"
            | "lookgroup"
            | "collection"
            | "geominfo"
            | "geompropdef"
            | "propertyset"
            | "variantset"
            | "unitdef"
            | "unittypedef"
            | "targetdef"
            | "attributedef"
            | "token"
            | "xi:include"
            | "include"
            | "backdrop"
            | "comment"
    )
}

/// Parses a MaterialX document.
pub(crate) fn parse(text: &str) -> Result<Document, MaterialXError> {
    let options = roxmltree::ParsingOptions {
        allow_dtd: false,
        nodes_limit: MAX_XML_NODES,
    };
    let xml = roxmltree::Document::parse_with_options(text, options)
        .map_err(|e| MaterialXError::Xml(e.to_string()))?;
    let root = xml.root_element();
    if root.tag_name().name() != "materialx" {
        return Err(MaterialXError::NotMaterialX(root.tag_name().name().to_string()));
    }
    let version = root.attribute("version").unwrap_or("").to_string();
    let major = version
        .split('.')
        .next()
        .and_then(|m| m.trim().parse::<u32>().ok());
    if major != Some(1) {
        return Err(MaterialXError::Version(version));
    }
    let top = Scope::default().enter(root);
    let mut out = Document {
        version,
        ..Document::default()
    };
    for child in root.children().filter(roxmltree::Node::is_element) {
        let tag = child.tag_name().name();
        let line = line_of(&xml, child);
        match tag {
            "nodegraph" => {
                let name = attr(child, "name").ok_or_else(|| MaterialXError::Structure {
                    line,
                    message: "<nodegraph> has no name".into(),
                })?;
                let scope = top.enter(child);
                let mut graph = NodeGraph {
                    nodedef: attr(child, "nodedef"),
                    ..NodeGraph::default()
                };
                for inner in child.children().filter(roxmltree::Node::is_element) {
                    let inner_tag = inner.tag_name().name();
                    match inner_tag {
                        "input" => graph.interface.push(read_input(inner, &scope)),
                        "output" => graph.outputs.push(GraphOutput {
                            name: attr(inner, "name").unwrap_or_default(),
                            ty: attr(inner, "type").unwrap_or_default(),
                            nodename: attr(inner, "nodename").filter(|s| !s.is_empty()),
                            output: attr(inner, "output").filter(|s| !s.is_empty()),
                            interfacename: attr(inner, "interfacename").filter(|s| !s.is_empty()),
                            value: attr(inner, "value"),
                        }),
                        t if is_structural(t) => {}
                        _ => {
                            let node = read_node(&xml, inner, &scope)?;
                            if graph.nodes.contains_key(&node.name) {
                                return Err(MaterialXError::Structure {
                                    line: node.line,
                                    message: format!(
                                        "node `{}` is declared twice in nodegraph `{name}`",
                                        node.name
                                    ),
                                });
                            }
                            graph.nodes.insert(node.name.clone(), node);
                        }
                    }
                }
                if out.graphs.insert(name.clone(), graph).is_some() {
                    return Err(MaterialXError::Structure {
                        line,
                        message: format!("nodegraph `{name}` is declared twice"),
                    });
                }
            }
            "nodedef" => {
                let name = attr(child, "name").ok_or_else(|| MaterialXError::Structure {
                    line,
                    message: "<nodedef> has no name".into(),
                })?;
                let scope = top.enter(child);
                let mut def = NodeDef {
                    node: attr(child, "node").unwrap_or_default(),
                    ..NodeDef::default()
                };
                for inner in child.children().filter(roxmltree::Node::is_element) {
                    match inner.tag_name().name() {
                        "input" => def.inputs.push(read_input(inner, &scope)),
                        "output" => def.outputs.push((
                            attr(inner, "name").unwrap_or_default(),
                            attr(inner, "type").unwrap_or_default(),
                        )),
                        _ => {}
                    }
                }
                // A nodedef without outputs has the one output `out` of its `type`.
                if def.outputs.is_empty()
                    && let Some(ty) = attr(child, "type")
                {
                    def.outputs.push(("out".into(), ty));
                }
                out.nodedefs.insert(name, def);
            }
            t if is_structural(t) => {}
            _ => {
                let node = read_node(&xml, child, &top)?;
                if out.nodes.contains_key(&node.name) {
                    return Err(MaterialXError::Structure {
                        line: node.line,
                        message: format!("node `{}` is declared twice", node.name),
                    });
                }
                out.order.push(node.name.clone());
                out.nodes.insert(node.name.clone(), node);
            }
        }
    }
    Ok(out)
}
