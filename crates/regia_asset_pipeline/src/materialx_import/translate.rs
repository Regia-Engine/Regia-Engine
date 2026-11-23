// SPDX-License-Identifier: MPL-2.0
//! MaterialX nodes → material graph nodes.
//!
//! The translator walks from the surface shader's inputs to the nodes they read, depth first,
//! and emits one or a few graph nodes per MaterialX node (memoised per node and output, so a
//! node read twice is emitted once). Node graphs are flattened: a compound graph is one scope,
//! a custom node's implementation graph is a new scope per use, with its interface inputs bound
//! to the instance's inputs. Values carry their constant when they have one, which folds
//! arithmetic on constants and turns `ifgreater`/`ifequal` with constant operands into static
//! choices.

use std::collections::{BTreeSet, HashMap, HashSet};

use regia_matgraph::{
    ColorSpace, MATGRAPH_FORMAT_VERSION, MAX_PARAMS, MaterialGraph, Node, ParamDecl, ParamValue, TextureDecl,
    ValueType,
};
use regia_render::surface_graph::GRAPH_TEXTURE_SLOTS;

use super::doc::{Document, Input, NodeElem, NodeGraph};
use super::{
    ImportedParam, ImportedTexture, MAX_DEPTH, MAX_EMITTED_NODES, MAX_EXPANSIONS, MappedNode, MaterialXError,
    MaterialXImport, MaterialXReport, MaterialXSettings, MaterialXWarning, WARN_BAD_VALUE, WARN_LOSSY_INPUT,
    WARN_MISSING_NODE, WARN_PARAM_LIMIT, WARN_TEXTURE_LIMIT, WARN_TEXTURE_PATH, WARN_UNSUPPORTED_NODE,
    value_type,
};

/// Steepness of the step that stands in for a non-constant `ifgreater`/`ifequal`.
const STEP_GAIN: f32 = 1.0e5;

/// What a value is, beyond its graph node.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Shape {
    /// An ordinary value.
    Plain,
    /// A MaterialX texture coordinate (origin bottom left) computed from the mesh UV node
    /// `raw` (origin top left): an image reading it unchanged samples `raw` directly.
    MxTexcoord {
        /// The mesh UV node.
        raw: String,
    },
    /// A `normalmap` result: the value node is a world-space stand-in, `tangent` the tangent-
    /// space normal the surface's normal output takes.
    NormalMap {
        /// The tangent-space normal node.
        tangent: String,
    },
}

/// A translated value: its graph node, type, constant (when known) and shape.
#[derive(Clone, Debug)]
pub(crate) struct Val {
    pub node: String,
    pub ty: ValueType,
    pub constant: Option<[f32; 4]>,
    pub shape: Shape,
}

/// An interface input of a scope, resolved on first use.
#[derive(Clone, Debug)]
enum Binding<'d> {
    /// Not resolved yet: the input, the scope it is evaluated in, and the parameter name it
    /// becomes when it holds a value (`None`: a constant).
    Pending {
        input: &'d Input,
        frame: usize,
        param: Option<(String, String)>,
    },
    /// A resolved value.
    Ready(Val),
    /// No value: the reader's own default applies.
    Missing,
    /// Being resolved: met again before it resolves, the binding is its own source (an output
    /// that passes an interface input bound back to its own graph), a cycle.
    Resolving,
}

/// One translation scope: the document's top level, a compound node graph, or one use of a
/// custom node.
struct Frame<'d> {
    graph: Option<&'d NodeGraph>,
    path: String,
    bindings: HashMap<String, Binding<'d>>,
}

type Key = (usize, String, String);

/// The translator state.
pub(crate) struct Translator<'d> {
    doc: &'d Document,
    pub settings: &'d MaterialXSettings,
    doc_dir: String,
    pub graph: MaterialGraph,
    pub report: MaterialXReport,
    frames: Vec<Frame<'d>>,
    compound: HashMap<String, usize>,
    memo: HashMap<Key, Option<Val>>,
    visiting: HashSet<Key>,
    constants: HashMap<String, String>,
    uv: [Option<String>; 2],
    textures: HashMap<(String, bool), String>,
    texture_limit_warned: bool,
    depth: usize,
    pub fatal: Option<MaterialXError>,
    gaps: BTreeSet<String>,
    /// The world-normal placeholder node of each tangent-space `normalmap` (its value; the map
    /// itself rides on the `Shape::NormalMap` tag, which any other node drops).
    normal_map_placeholders: HashSet<String>,
    /// Once the node limit is hit, every overflow node is wired to this one placeholder so the
    /// graph stays bounded while the fatal error propagates (`add`).
    overflow_name: String,
}

/// Imports the document's surface material.
pub(crate) fn import(
    doc: &Document,
    doc_dir: &str,
    settings: &MaterialXSettings,
) -> Result<MaterialXImport, MaterialXError> {
    let (material, shader) = super::surface::pick_material(doc, settings)?;
    let mut t = Translator::new(doc, settings, material, shader);
    t.doc_dir = doc_dir.trim_matches('/').to_string();
    for name in super::surface::extra_materials(doc, &t.report.material) {
        t.warn(
            super::WARN_EXTRA_MATERIAL,
            &name,
            "surfacematerial",
            "the document has more materials; one file imports one (set `material` in the .meta)",
        );
    }
    super::surface::map_surface(&mut t, shader);
    if let Some(err) = t.fatal.take() {
        return Err(err);
    }
    t.graph.format_version = MATGRAPH_FORMAT_VERSION;
    t.report.matgraph_gaps = t.gaps.iter().cloned().collect();
    t.report.emitted_nodes = t.graph.nodes.len();
    Ok(MaterialXImport {
        graph: t.graph,
        report: t.report,
    })
}

fn sanitize(name: &str, max: usize) -> String {
    let mut out: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .take(max)
        .collect();
    if out.is_empty() {
        out.push('n');
    }
    out
}

/// A parsed colour value honours its `colorspace` like an image file does ([`Translator::image`]):
/// `srgb*` (and the `g22_rec709` approximation) marks the value encoded, and the graph works
/// linear, so the channels decode. Non-colour inputs, and colours without the attribute, pass
/// through unchanged.
pub(crate) fn decode_value_colorspace(input: &Input, mut v: [f32; 4]) -> [f32; 4] {
    if input.ty.starts_with("color")
        && input
            .colorspace
            .as_deref()
            .is_some_and(|cs| cs.starts_with("srgb") || cs == "g22_rec709")
    {
        for c in &mut v {
            *c = regia_core::math::srgb_to_linear(*c);
        }
    }
    v
}

/// Parses a MaterialX value string (`"0.8, 0.8, 0.8"`, `"true"`) into `width` floats.
pub(crate) fn parse_value(text: &str, width: usize) -> Option<[f32; 4]> {
    let text = text.trim();
    let mut out = [0.0f32; 4];
    match text {
        "true" if width == 1 => {
            out[0] = 1.0;
            return Some(out);
        }
        "false" if width == 1 => return Some(out),
        _ => {}
    }
    let mut n = 0;
    for part in text
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|p| !p.is_empty())
    {
        if n >= width {
            return None;
        }
        let v: f32 = part.parse().ok()?;
        if !v.is_finite() {
            return None;
        }
        out[n] = v;
        n += 1;
    }
    (n == width).then_some(out)
}

pub(crate) fn constant_node(ty: ValueType, v: [f32; 4]) -> Node {
    match ty {
        ValueType::Float => Node::Float(v[0]),
        ValueType::Vec2 => Node::Vec2([v[0], v[1]]),
        ValueType::Vec3 => Node::Vec3([v[0], v[1], v[2]]),
        ValueType::Vec4 => Node::Vec4(v),
    }
}

/// `v` broadcast or cut to `ty` (constants follow [`Translator::convert`]'s rules).
fn convert_constant(v: [f32; 4], from: ValueType, to: ValueType) -> [f32; 4] {
    if from == to {
        return v;
    }
    match (from.width(), to.width()) {
        (1, _) => [v[0], v[0], v[0], v[0]],
        (2, 3) => [v[0], v[1], 0.0, 0.0],
        (2, 4) => [v[0], v[1], 0.0, 1.0],
        (3, 4) => [v[0], v[1], v[2], 1.0],
        (_, w) => {
            let mut out = [0.0; 4];
            out[..w].copy_from_slice(&v[..w]);
            out
        }
    }
}

/// Binary operations the translator folds on constants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Add,
    Sub,
    Mul,
    Div,
    Min,
    Max,
}

impl<'d> Translator<'d> {
    // -- diagnostics ------------------------------------------------------------------------

    pub(crate) fn warn(
        &mut self,
        code: &'static str,
        node: &str,
        category: &str,
        message: impl Into<String>,
    ) {
        self.report.warnings.push(MaterialXWarning {
            code,
            node: node.to_string(),
            category: category.to_string(),
            message: message.into(),
        });
    }

    pub(crate) fn lossy(&mut self, node: &str, message: impl Into<String>) {
        self.warn(WARN_LOSSY_INPUT, node, "", message);
    }

    fn mapped(&mut self, node: &str, category: &str, to: impl Into<String>) {
        self.report.mapped.push(MappedNode {
            node: node.to_string(),
            category: category.to_string(),
            mapped_to: to.into(),
        });
    }

    fn gap(&mut self, text: &str) {
        self.gaps.insert(text.to_string());
    }

    fn path_of(&self, frame: usize, name: &str) -> String {
        let prefix = &self.frames[frame].path;
        if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        }
    }

    // -- emission ---------------------------------------------------------------------------

    /// An empty translator for `doc` over the picked surface (`import` runs
    /// [`super::surface::pick_material`] first and errors there; tests pass a placeholder).
    pub(crate) fn new(
        doc: &'d Document,
        settings: &'d MaterialXSettings,
        material: String,
        shader: &'d super::doc::NodeElem,
    ) -> Self {
        Translator {
            doc,
            settings,
            doc_dir: String::new(),
            graph: MaterialGraph::new(material.clone()),
            report: MaterialXReport {
                version: doc.version.clone(),
                material,
                shader: shader.category.clone(),
                ..MaterialXReport::default()
            },
            frames: vec![Frame {
                graph: None,
                path: String::new(),
                bindings: HashMap::new(),
            }],
            compound: HashMap::new(),
            memo: HashMap::new(),
            visiting: HashSet::new(),
            constants: HashMap::new(),
            uv: [None, None],
            textures: HashMap::new(),
            texture_limit_warned: false,
            depth: 0,
            fatal: None,
            gaps: BTreeSet::new(),
            normal_map_placeholders: HashSet::new(),
            overflow_name: String::new(),
        }
    }

    /// Adds a graph node under a unique name derived from `hint`.
    pub(crate) fn add(&mut self, hint: &str, node: Node) -> String {
        if !self.overflow_name.is_empty() {
            // The limit is already hit and the import will fail: point every overflow node at the
            // one placeholder instead of growing the graph without end.
            return self.overflow_name.clone();
        }
        if self.graph.nodes.len() >= MAX_EMITTED_NODES {
            self.fatal.get_or_insert(MaterialXError::Limit(format!(
                "more than {MAX_EMITTED_NODES} graph nodes"
            )));
            let base = sanitize("limit_overflow", 48);
            let mut name = base.clone();
            let mut n = 1;
            while self.graph.nodes.contains_key(&name) {
                n += 1;
                name = format!("{base}_{n}");
            }
            self.graph.nodes.insert(name.clone(), node);
            self.overflow_name = name.clone();
            return name;
        }
        let base = sanitize(hint, 48);
        let mut name = base.clone();
        let mut n = 1;
        while self.graph.nodes.contains_key(&name) {
            n += 1;
            name = format!("{base}_{n}");
        }
        self.graph.nodes.insert(name.clone(), node);
        name
    }

    /// True when graph node `node` reads, directly or through other nodes, the placeholder of a
    /// tangent-space normal map: the map passed through another node, which drops it.
    pub(crate) fn reaches_normal_map(&self, node: &str) -> bool {
        if self.normal_map_placeholders.is_empty() {
            return false;
        }
        let mut seen: HashSet<&str> = HashSet::new();
        let mut stack = vec![node];
        while let Some(name) = stack.pop() {
            if !seen.insert(name) {
                continue;
            }
            if self.normal_map_placeholders.contains(name) {
                return true;
            }
            if let Some(n) = self.graph.nodes.get(name) {
                stack.extend(n.inputs());
            }
        }
        false
    }

    fn plain(node: String, ty: ValueType) -> Val {
        Val {
            node,
            ty,
            constant: None,
            shape: Shape::Plain,
        }
    }

    /// A constant value (one graph node per distinct constant).
    pub(crate) fn constant(&mut self, ty: ValueType, v: [f32; 4]) -> Val {
        let mut v = v;
        for c in v.iter_mut().skip(ty.width()) {
            *c = 0.0;
        }
        let key = format!("{ty:?}{v:?}");
        let node = match self.constants.get(&key) {
            Some(node) => node.clone(),
            None => {
                let node = self.add("c", constant_node(ty, v));
                self.constants.insert(key, node.clone());
                node
            }
        };
        Val {
            node,
            ty,
            constant: Some(v),
            shape: Shape::Plain,
        }
    }

    /// `val` as `ty`: a float broadcasts, a wider vector keeps its leading components, a
    /// narrower one is padded with 0 (and 1 for the fourth component).
    pub(crate) fn convert(&mut self, val: Val, ty: ValueType) -> Val {
        if val.ty == ty {
            return val;
        }
        if let Some(c) = val.constant {
            return self.constant(ty, convert_constant(c, val.ty, ty));
        }
        let (from, to) = (val.ty.width(), ty.width());
        let node = if from == 1 || to < from {
            let letters = if from == 1 {
                "x".repeat(to)
            } else {
                "xyzw"[..to].to_string()
            };
            self.add(
                "cvt",
                Node::Swizzle {
                    input: val.node,
                    channels: letters,
                },
            )
        } else {
            let pad = match (from, to) {
                (2, 3) => self.constant(ValueType::Float, [0.0; 4]),
                (2, 4) => self.constant(ValueType::Vec2, [0.0, 1.0, 0.0, 0.0]),
                _ => self.constant(ValueType::Float, [1.0, 0.0, 0.0, 0.0]),
            };
            self.add("cvt", Node::Append(val.node, pad.node))
        };
        Self::plain(node, ty)
    }

    fn binary(&mut self, op: Op, a: Val, b: Val, hint: &str) -> Val {
        let ty = if a.ty == ValueType::Float { b.ty } else { a.ty };
        if let (Some(x), Some(y)) = (a.constant, b.constant) {
            let x = convert_constant(x, a.ty, ty);
            let y = convert_constant(y, b.ty, ty);
            let mut out = [0.0f32; 4];
            for (i, o) in out.iter_mut().enumerate().take(ty.width()) {
                *o = match op {
                    Op::Add => x[i] + y[i],
                    Op::Sub => x[i] - y[i],
                    Op::Mul => x[i] * y[i],
                    Op::Div if y[i].abs() > 1e-6 => x[i] / y[i],
                    Op::Div => x[i],
                    Op::Min => x[i].min(y[i]),
                    Op::Max => x[i].max(y[i]),
                };
            }
            if out.iter().all(|v| v.is_finite()) {
                return self.constant(ty, out);
            }
        }
        // Identities keep the graph small: x * 1, x / 1, x + 0, x - 0.
        let is = |v: &Val, k: f32| {
            v.constant
                .is_some_and(|c| c[..v.ty.width()].iter().all(|&e| e == k))
        };
        match op {
            Op::Mul if is(&b, 1.0) && a.ty == ty => return a,
            Op::Mul if is(&a, 1.0) && b.ty == ty => return b,
            Op::Div if is(&b, 1.0) && a.ty == ty => return a,
            Op::Add if is(&b, 0.0) && a.ty == ty => return a,
            Op::Add if is(&a, 0.0) && b.ty == ty => return b,
            Op::Sub if is(&b, 0.0) && a.ty == ty => return a,
            _ => {}
        }
        // Widths must match or one side must be a float.
        let (a, b) = if a.ty != b.ty && a.ty != ValueType::Float && b.ty != ValueType::Float {
            let b = self.convert(b, a.ty);
            (a, b)
        } else {
            (a, b)
        };
        let node = match op {
            Op::Add => Node::Add(a.node, b.node),
            Op::Sub => Node::Sub(a.node, b.node),
            Op::Mul => Node::Mul(a.node, b.node),
            Op::Div => Node::Div(a.node, b.node),
            Op::Min => Node::Min(a.node, b.node),
            Op::Max => Node::Max(a.node, b.node),
        };
        Self::plain(self.add(hint, node), ty)
    }

    pub(crate) fn mul(&mut self, a: Val, b: Val) -> Val {
        self.binary(Op::Mul, a, b, "mul")
    }

    pub(crate) fn sub(&mut self, a: Val, b: Val) -> Val {
        self.binary(Op::Sub, a, b, "sub")
    }

    pub(crate) fn add_vals(&mut self, a: Val, b: Val) -> Val {
        self.binary(Op::Add, a, b, "add")
    }

    /// `mix(a, b, t)`.
    pub(crate) fn lerp(&mut self, a: Val, b: Val, t: Val) -> Val {
        if let Some(c) = t.constant {
            if c[..t.ty.width()].iter().all(|&e| e == 0.0) {
                return a;
            }
            if c[..t.ty.width()].iter().all(|&e| e == 1.0) {
                return b;
            }
        }
        let ty = if a.ty == ValueType::Float { b.ty } else { a.ty };
        let a = self.convert(a, ty);
        let b = self.convert(b, ty);
        let t = if t.ty == ValueType::Float {
            t
        } else {
            self.convert(t, ty)
        };
        let node = self.add(
            "mix",
            Node::Lerp {
                a: a.node,
                b: b.node,
                t: t.node,
            },
        );
        Self::plain(node, ty)
    }

    pub(crate) fn unary(&mut self, hint: &str, val: &Val, make: fn(String) -> Node) -> Val {
        let node = self.add(hint, make(val.node.clone()));
        Self::plain(node, val.ty)
    }

    pub(crate) fn swizzle(&mut self, val: &Val, channels: &str) -> Val {
        let ty = ValueType::of_width(channels.len()).unwrap_or(ValueType::Float);
        if let Some(c) = val.constant {
            let mut out = [0.0; 4];
            for (i, ch) in channels.chars().enumerate() {
                let idx = "xyzw".find(ch).unwrap_or(0).min(val.ty.width() - 1);
                out[i] = c[idx];
            }
            return self.constant(ty, out);
        }
        let node = self.add(
            "swz",
            Node::Swizzle {
                input: val.node.clone(),
                channels: channels.to_string(),
            },
        );
        Self::plain(node, ty)
    }

    /// The mesh UV set `index` (0 or 1) as Regia reads it.
    pub(crate) fn mesh_uv(&mut self, index: usize) -> Val {
        let slot = index.min(1);
        let node = match &self.uv[slot] {
            Some(node) => node.clone(),
            None => {
                let node = self.add(
                    if slot == 0 { "uv0" } else { "uv1" },
                    if slot == 0 {
                        Node::TexCoord
                    } else {
                        Node::TexCoord1
                    },
                );
                self.uv[slot] = Some(node.clone());
                node
            }
        };
        Self::plain(node, ValueType::Vec2)
    }

    /// `uv` with `v` flipped (`(u, 1 - v)`), when the settings flip.
    fn flip_v(&mut self, uv: &Val) -> Val {
        if !self.settings.flip_v {
            return uv.clone();
        }
        let u = self.swizzle(uv, "x");
        let v = self.swizzle(uv, "y");
        let one_minus = self.unary("flip", &v, Node::OneMinus);
        let node = self.add("uvf", Node::Append(u.node, one_minus.node));
        Self::plain(node, ValueType::Vec2)
    }

    // -- parameters and textures ------------------------------------------------------------

    /// A material parameter, or `None` when every slot is taken (the caller keeps a constant).
    pub(crate) fn param(
        &mut self,
        hint: &str,
        source: &str,
        ty: ValueType,
        value: [f32; 4],
        color: bool,
    ) -> Option<Val> {
        let name = self.param_decl(hint, source, ty, value, color)?;
        let node = self.add(&format!("p_{name}"), Node::Param(name));
        Some(Self::plain(node, ty))
    }

    /// Declares a material parameter and returns its name, or `None` (with a warning) when
    /// every slot is taken.
    pub(crate) fn param_decl(
        &mut self,
        hint: &str,
        source: &str,
        ty: ValueType,
        value: [f32; 4],
        color: bool,
    ) -> Option<String> {
        if self.graph.params.len() >= MAX_PARAMS {
            self.warn(
                WARN_PARAM_LIMIT,
                source,
                "",
                format!("{MAX_PARAMS} parameters already; this input stays a constant"),
            );
            return None;
        }
        let base = sanitize(hint, 60);
        let mut name = base.clone();
        let mut n = 1;
        while self.graph.params.iter().any(|p| p.name == name) {
            n += 1;
            name = format!("{base}_{n}");
        }
        let decl = match ty {
            ValueType::Float => ParamValue::Float(value[0]),
            ValueType::Vec2 => ParamValue::Vec2([value[0], value[1]]),
            ValueType::Vec3 if color => ParamValue::Color([value[0], value[1], value[2]]),
            ValueType::Vec3 => ParamValue::Vec3([value[0], value[1], value[2]]),
            ValueType::Vec4 => ParamValue::Vec4(value),
        };
        self.graph.params.push(ParamDecl::new(name.clone(), decl));
        self.report.params.push(ImportedParam {
            name: name.clone(),
            source: source.to_string(),
            value,
            width: ty.width(),
        });
        Some(name)
    }

    /// The project path of an image file value: the document directory, the effective file
    /// prefix and the value, folded lexically, then checked with `check_relative`. `None`
    /// (with a warning) for absolute paths, URIs, UDIM sets, paths that leave the project,
    /// and paths that are not portable (a colon, a Windows device name, a segment of only dots).
    fn image_path(&mut self, node_path: &str, category: &str, prefix: &str, value: &str) -> Option<String> {
        let value = value.replace('\\', "/");
        // An absolute value ignores the prefix (and is refused below).
        let absolute = value.starts_with('/') || value.contains("://") || value.get(1..3) == Some(":/");
        let joined = if absolute {
            value
        } else {
            format!("{prefix}{value}").replace('\\', "/")
        };
        let refuse = |why: &str| format!("image `{joined}`: {why}; the input's default is used");
        if joined.contains("<UDIM>") || joined.contains("<UVTILE>") {
            self.warn(
                WARN_TEXTURE_PATH,
                node_path,
                category,
                refuse("UDIM and UV-tile sets are not supported"),
            );
            return None;
        }
        if joined.starts_with('/') || joined.contains("://") || joined.get(1..3) == Some(":/") {
            self.warn(
                WARN_TEXTURE_PATH,
                node_path,
                category,
                refuse("absolute paths and URIs are not imported"),
            );
            return None;
        }
        let mut parts: Vec<&str> = self.doc_dir.split('/').filter(|p| !p.is_empty()).collect();
        for part in joined.split('/') {
            match part {
                "" | "." => {}
                ".." => {
                    if parts.pop().is_none() {
                        self.warn(
                            WARN_TEXTURE_PATH,
                            node_path,
                            category,
                            refuse("the path leaves the project"),
                        );
                        return None;
                    }
                }
                other => parts.push(other),
            }
        }
        if parts.is_empty() {
            self.warn(
                WARN_TEXTURE_PATH,
                node_path,
                category,
                refuse("the path is empty"),
            );
            return None;
        }
        let folded = parts.join("/");
        // The fold drops `..`. It still keeps `...`, `nul.png` and `C:name`, which Windows
        // reads as a parent, a device and a drive prefix (the same rule as generator paths).
        if let Err(e) = regia_core::project_path::check_relative(&folded) {
            self.warn(
                WARN_TEXTURE_PATH,
                node_path,
                category,
                refuse(&format!("not a portable project path ({})", e.code())),
            );
            return None;
        }
        Some(folded)
    }

    /// The graph texture for `path` in `srgb` or linear, declared on first use; `None` when the
    /// graph's texture slots are full.
    fn texture(&mut self, node_path: &str, path: &str, srgb: bool) -> Option<String> {
        if let Some(name) = self.textures.get(&(path.to_string(), srgb)) {
            return Some(name.clone());
        }
        if self.graph.textures.len() >= GRAPH_TEXTURE_SLOTS {
            if !self.texture_limit_warned {
                self.texture_limit_warned = true;
                self.gap("more than 7 images per material: graph texture slots are fixed at 7");
            }
            self.warn(
                WARN_TEXTURE_LIMIT,
                node_path,
                "image",
                format!("`{path}`: the graph samples at most {GRAPH_TEXTURE_SLOTS} images; the image's default is used"),
            );
            return None;
        }
        let stem = path.rsplit('/').next().unwrap_or(path);
        let stem = stem.split('.').next().unwrap_or(stem);
        let base = sanitize(stem, 56);
        let mut name = base.clone();
        let mut n = 1;
        while self.graph.textures.iter().any(|t| t.name == name) {
            n += 1;
            name = format!("{base}_{n}");
        }
        self.graph.textures.push(TextureDecl {
            name: name.clone(),
            path: path.to_string(),
            color_space: if srgb {
                ColorSpace::Srgb
            } else {
                ColorSpace::Linear
            },
        });
        self.report.textures.push(ImportedTexture {
            name: name.clone(),
            path: path.to_string(),
            color_space: if srgb { "srgb" } else { "linear" },
        });
        self.textures.insert((path.to_string(), srgb), name.clone());
        Some(name)
    }

    // -- inputs -----------------------------------------------------------------------------

    /// Resolves an interface binding of `frame`.
    fn binding(&mut self, frame: usize, name: &str) -> Option<Val> {
        let state = self.frames[frame].bindings.get(name).cloned()?;
        let val = match state {
            Binding::Ready(v) => return Some(v),
            Binding::Missing => return None,
            Binding::Resolving => {
                if self.fatal.is_none() {
                    let path = self.path_of(frame, name);
                    self.fatal = Some(MaterialXError::Cycle(path));
                }
                return None;
            }
            Binding::Pending {
                input,
                frame: at,
                param,
            } => {
                // A string or filename input is read by `text_binding`; leave it pending.
                let ty = value_type(&input.ty)?;
                // Marked before resolving, so a binding that reaches itself through node graph
                // outputs is a cycle, not unbounded recursion (a stack overflow aborts the cook).
                self.frames[frame]
                    .bindings
                    .insert(name.to_string(), Binding::Resolving);
                match (&param, &input.value, input.is_connected()) {
                    (Some((hint, source)), Some(text), false) => match parse_value(text, ty.width()) {
                        Some(v) => {
                            let v = decode_value_colorspace(input, v);
                            let color = input.ty.starts_with("color");
                            let p = self.param(hint, source, ty, v, color);
                            p.or_else(|| Some(self.constant(ty, v)))
                        }
                        None => {
                            self.warn(
                                WARN_BAD_VALUE,
                                source,
                                "",
                                format!("`{text}` is not a {}", input.ty),
                            );
                            None
                        }
                    },
                    _ => self.resolve(at, input, ty, ""),
                }
            }
        };
        let slot = match &val {
            Some(v) => Binding::Ready(v.clone()),
            None => Binding::Missing,
        };
        self.frames[frame].bindings.insert(name.to_string(), slot);
        val
    }

    /// A string or filename interface binding: `(value, colorspace, fileprefix)`.
    fn text_binding(&self, frame: usize, name: &str) -> Option<(String, Option<String>, String)> {
        match self.frames[frame].bindings.get(name)? {
            Binding::Pending { input, frame: at, .. } => {
                if let Some(iface) = &input.interfacename {
                    return self.text_binding(*at, iface);
                }
                input
                    .value
                    .clone()
                    .map(|v| (v, input.colorspace.clone(), input.fileprefix.clone()))
            }
            _ => None,
        }
    }

    /// Resolves an input to a value of `want`: an interface binding, a node, a node graph
    /// output, or its own value. `None` when it has none (the caller's default applies).
    pub(crate) fn resolve(
        &mut self,
        frame: usize,
        input: &'d Input,
        want: ValueType,
        owner: &str,
    ) -> Option<Val> {
        // An unbound interface input falls through to its own value, if any.
        if let Some(iface) = &input.interfacename
            && let Some(v) = self.binding(frame, iface)
        {
            return Some(self.convert(v, want));
        }
        if let Some(name) = &input.nodename {
            let v = self.node_output(frame, name, input.output.as_deref())?;
            return Some(self.convert_keep_shape(v, want));
        }
        if let Some(graph) = &input.nodegraph {
            let v = self.graph_output(graph, input.output.as_deref())?;
            return Some(self.convert_keep_shape(v, want));
        }
        let text = input.value.as_ref()?;
        match parse_value(text, want.width()) {
            Some(v) => Some(self.constant(want, decode_value_colorspace(input, v))),
            None => {
                let at = if owner.is_empty() {
                    input.name.clone()
                } else {
                    format!("{owner}.{}", input.name)
                };
                self.warn(WARN_BAD_VALUE, &at, "", format!("`{text}` is not a {}", input.ty));
                None
            }
        }
    }

    fn convert_keep_shape(&mut self, v: Val, want: ValueType) -> Val {
        if v.ty == want { v } else { self.convert(v, want) }
    }

    /// Input `name` of `node` as `want`, or the constant `default` when unset.
    pub(crate) fn input(
        &mut self,
        frame: usize,
        node: &'d NodeElem,
        name: &str,
        want: ValueType,
        default: [f32; 4],
    ) -> Val {
        let owner = self.path_of(frame, &node.name);
        match node
            .input(name)
            .and_then(|i| self.resolve(frame, i, want, &owner))
        {
            Some(v) => v,
            None => self.constant(want, default),
        }
    }

    /// Input `name` of `node` as `want`, or `None` when it is absent or resolves to nothing
    /// (an unbound interface input with a geometric default, a missing node).
    fn opt_input(&mut self, frame: usize, node: &'d NodeElem, name: &str, want: ValueType) -> Option<Val> {
        let owner = self.path_of(frame, &node.name);
        let input = node.input(name)?;
        self.resolve(frame, input, want, &owner)
    }

    /// Input `name` as its declared type (or `fallback` when it declares none), or `default`.
    fn input_own_type(
        &mut self,
        frame: usize,
        node: &'d NodeElem,
        name: &str,
        fallback: ValueType,
        default: [f32; 4],
    ) -> Val {
        let want = node
            .input(name)
            .and_then(|i| value_type(&i.ty))
            .unwrap_or(fallback);
        self.input(frame, node, name, want, default)
    }

    /// A string input's value (through interface bindings).
    fn string_input(
        &self,
        frame: usize,
        node: &NodeElem,
        name: &str,
    ) -> Option<(String, Option<String>, String)> {
        let input = node.input(name)?;
        if let Some(iface) = &input.interfacename
            && let Some(found) = self.text_binding(frame, iface)
        {
            return Some(found);
        }
        input
            .value
            .clone()
            .map(|v| (v, input.colorspace.clone(), input.fileprefix.clone()))
    }

    /// An integer input that must be constant (an index, an octave count).
    fn const_int(&mut self, frame: usize, node: &'d NodeElem, name: &str, default: i32) -> i32 {
        let v = self.input(
            frame,
            node,
            name,
            ValueType::Float,
            [default as f32, 0.0, 0.0, 0.0],
        );
        match v.constant {
            Some(c) => c[0].round() as i32,
            None => {
                let at = self.path_of(frame, &node.name);
                self.lossy(&at, format!("`{name}` must be constant here; {default} is used"));
                default
            }
        }
    }

    /// True when `node` sets input `name` to anything but `default`.
    pub(crate) fn non_default(
        &mut self,
        frame: usize,
        node: &'d NodeElem,
        name: &str,
        default: [f32; 4],
    ) -> bool {
        let Some(input) = node.input(name) else {
            return false;
        };
        let want = value_type(&input.ty).unwrap_or(ValueType::Float);
        if input.is_connected() {
            return match self
                .resolve(frame, input, want, &node.name)
                .and_then(|v| v.constant)
            {
                Some(c) => c[..want.width()] != default[..want.width()],
                None => true,
            };
        }
        match input.value.as_deref().and_then(|t| parse_value(t, want.width())) {
            Some(c) => c[..want.width()] != default[..want.width()],
            None => false,
        }
    }

    // -- nodes ------------------------------------------------------------------------------

    /// The compound node graph `name` as a scope (created on first use).
    fn compound_frame(&mut self, name: &str) -> Option<usize> {
        if let Some(&id) = self.compound.get(name) {
            return Some(id);
        }
        let graph = self.doc.graphs.get(name)?;
        let id = self.frames.len();
        let mut bindings = HashMap::new();
        for input in &graph.interface {
            let param = value_type(&input.ty)
                .filter(|_| !matches!(input.ty.as_str(), "integer" | "boolean"))
                .map(|_| (input.name.clone(), format!("{name}.{}", input.name)));
            bindings.insert(
                input.name.clone(),
                Binding::Pending {
                    input,
                    frame: 0,
                    param,
                },
            );
        }
        self.frames.push(Frame {
            graph: Some(graph),
            path: name.to_string(),
            bindings,
        });
        self.compound.insert(name.to_string(), id);
        Some(id)
    }

    /// Output `output` (or the first) of the top-level node graph `name`.
    fn graph_output(&mut self, name: &str, output: Option<&str>) -> Option<Val> {
        let Some(frame) = self.compound_frame(name) else {
            self.warn(
                WARN_MISSING_NODE,
                name,
                "nodegraph",
                "no such nodegraph; the input's default is used",
            );
            return None;
        };
        let graph = self.frames[frame].graph?;
        let out = match output {
            Some(o) => graph.outputs.iter().find(|x| x.name == o),
            None => graph.outputs.first(),
        };
        let Some(out) = out else {
            self.warn(
                WARN_MISSING_NODE,
                name,
                "nodegraph",
                format!(
                    "no output `{}`; the input's default is used",
                    output.unwrap_or("")
                ),
            );
            return None;
        };
        self.scope_output(frame, out)
    }

    fn scope_output(&mut self, frame: usize, out: &'d super::doc::GraphOutput) -> Option<Val> {
        let ty = value_type(&out.ty);
        if let Some(node) = &out.nodename {
            let v = self.node_output(frame, node, out.output.as_deref())?;
            return Some(match ty {
                Some(ty) => self.convert_keep_shape(v, ty),
                None => v,
            });
        }
        if let Some(iface) = &out.interfacename {
            return self.binding(frame, iface);
        }
        let ty = ty?;
        let v = parse_value(out.value.as_deref()?, ty.width())?;
        Some(self.constant(ty, v))
    }

    /// The value of output `output` of node `name` in `frame`.
    pub(crate) fn node_output(&mut self, frame: usize, name: &str, output: Option<&str>) -> Option<Val> {
        let doc: &'d Document = self.doc;
        let found: Option<&'d NodeElem> = match self.frames[frame].graph {
            Some(graph) => graph.nodes.get(name),
            None => doc.nodes.get(name),
        };
        let Some(elem) = found else {
            let path = self.path_of(frame, name);
            self.warn(
                WARN_MISSING_NODE,
                &path,
                "",
                "no such node; the input's default is used",
            );
            return None;
        };
        let key: Key = (frame, name.to_string(), output.unwrap_or("").to_string());
        if let Some(done) = self.memo.get(&key) {
            return done.clone();
        }
        if self.fatal.is_some() {
            return None;
        }
        if !self.visiting.insert(key.clone()) {
            self.fatal = Some(MaterialXError::Cycle(self.path_of(frame, name)));
            return None;
        }
        self.depth += 1;
        let result = if self.depth > MAX_DEPTH {
            self.fatal.get_or_insert(MaterialXError::Limit(format!(
                "node references deeper than {MAX_DEPTH} at `{}`",
                self.path_of(frame, name)
            )));
            None
        } else {
            self.translate(frame, elem, output)
        };
        self.depth -= 1;
        self.visiting.remove(&key);
        self.memo.insert(key, result.clone());
        result
    }

    /// The fallback for an unsupported node: one warning, a constant of the node's type
    /// (mid grey for colours, zero otherwise).
    fn unsupported(&mut self, frame: usize, elem: &NodeElem, why: &str) -> Option<Val> {
        let path = self.path_of(frame, &elem.name);
        let ty = value_type(&elem.ty)?;
        let grey = elem.ty.starts_with("color");
        let fallback = if grey { [0.5, 0.5, 0.5, 1.0] } else { [0.0; 4] };
        self.warn(
            WARN_UNSUPPORTED_NODE,
            &path,
            &elem.category,
            format!("{why}; a constant {fallback:?} stands in"),
        );
        Some(self.constant(ty, fallback))
    }

    fn translate(&mut self, frame: usize, elem: &'d NodeElem, output: Option<&str>) -> Option<Val> {
        use ValueType::{Float, Vec2, Vec3};
        let path = self.path_of(frame, &elem.name);
        let cat = elem.category.as_str();
        let ty = value_type(&elem.ty);
        // Multi-output nodes (`separateN`, custom nodes) and shaders have no single value type.
        if ty.is_none() && !cat.starts_with("separate") {
            if let Some((def_name, _)) = self.doc.nodedef_for(cat, &elem.ty)
                && self.doc.implementation(def_name).is_some()
            {
                return self.custom(frame, elem, def_name, output);
            }
            self.warn(
                WARN_UNSUPPORTED_NODE,
                &path,
                cat,
                format!(
                    "type `{}` is not a value the graph can carry; the reader's default is used",
                    elem.ty
                ),
            );
            return None;
        }
        let ty = ty.unwrap_or(Float);
        let zero = [0.0f32; 4];
        let one = [1.0f32; 4];
        let val = match cat {
            "constant" => {
                let v = self.input(frame, elem, "value", ty, zero);
                self.mapped(&path, cat, "constant");
                v
            }
            "dot" => {
                let v = self.input(frame, elem, "in", ty, zero);
                self.mapped(&path, cat, "pass-through");
                v
            }
            "image" | "tiledimage" => return self.image(frame, elem, ty),
            "texcoord" => {
                let index = self.const_int(frame, elem, "index", 0);
                if index > 1 {
                    self.lossy(
                        &path,
                        format!("UV set {index} read as UV set 1 (meshes carry two)"),
                    );
                }
                let raw = self.mesh_uv(usize::try_from(index.clamp(0, 1)).unwrap_or(0));
                let mut uv = self.flip_v(&raw);
                uv.shape = Shape::MxTexcoord { raw: raw.node };
                self.mapped(&path, cat, if index == 0 { "TexCoord" } else { "TexCoord1" });
                if ty == Vec2 { uv } else { self.convert(uv, ty) }
            }
            "position" => {
                let space = self
                    .string_input(frame, elem, "space")
                    .map_or("object".into(), |s| s.0);
                let world = self.add("pos", Node::WorldPosition);
                let world = Self::plain(world, Vec3);
                let v = if space == "world" {
                    self.mapped(&path, cat, "WorldPosition");
                    world
                } else {
                    let origin = self.add("origin", Node::ObjectPosition);
                    self.lossy(&path, "object-space position is the world position minus the object origin (rotation and scale are not undone)");
                    self.gap("object-space position and normal: no object transform node");
                    self.mapped(&path, cat, "WorldPosition - ObjectPosition");
                    let origin = Self::plain(origin, Vec3);
                    self.sub(world, origin)
                };
                self.convert(v, ty)
            }
            "normal" => {
                let space = self
                    .string_input(frame, elem, "space")
                    .map_or("object".into(), |s| s.0);
                if space != "world" {
                    self.lossy(&path, "object-space normal read as the world normal");
                    self.gap("object-space position and normal: no object transform node");
                }
                let n = self.add("nrm", Node::WorldNormal);
                self.mapped(&path, cat, "WorldNormal");
                let n = Self::plain(n, Vec3);
                self.convert(n, ty)
            }
            "geomcolor" => {
                let c = self.add("vcol", Node::VertexColor);
                self.mapped(&path, cat, "VertexColor");
                let c = Self::plain(c, ValueType::Vec4);
                self.convert(c, ty)
            }
            "time" => {
                let t = self.add("time", Node::Time);
                self.mapped(&path, cat, "Time");
                let t = Self::plain(t, Float);
                self.convert(t, ty)
            }
            "add" | "subtract" | "multiply" | "divide" | "min" | "max" => {
                let (op, d2) = match cat {
                    "add" => (Op::Add, zero),
                    "subtract" => (Op::Sub, zero),
                    "multiply" => (Op::Mul, one),
                    "divide" => (Op::Div, one),
                    "min" => (Op::Min, zero),
                    _ => (Op::Max, zero),
                };
                let a = self.input_own_type(frame, elem, "in1", ty, zero);
                let b = self.input_own_type(frame, elem, "in2", ty, d2);
                let a = self.convert(a, ty);
                let v = self.binary(op, a, b, cat);
                self.mapped(&path, cat, format!("{op:?}"));
                self.convert(v, ty)
            }
            "power" => {
                let a = self.input(frame, elem, "in1", ty, zero);
                let b = self.input_own_type(frame, elem, "in2", ty, one);
                let node = self.add("pow", Node::Pow(a.node, b.node));
                self.mapped(&path, cat, "Pow");
                Self::plain(node, ty)
            }
            "mix" => {
                let fg = self.input(frame, elem, "fg", ty, zero);
                let bg = self.input(frame, elem, "bg", ty, zero);
                let t = self.input_own_type(frame, elem, "mix", Float, zero);
                self.mapped(&path, cat, "Lerp(bg, fg, mix)");
                self.lerp(bg, fg, t)
            }
            "clamp" => {
                let x = self.input(frame, elem, "in", ty, zero);
                let lo = self.input_own_type(frame, elem, "low", ty, zero);
                let hi = self.input_own_type(frame, elem, "high", ty, one);
                let unit = lo
                    .constant
                    .is_some_and(|c| c[..lo.ty.width()].iter().all(|&e| e == 0.0))
                    && hi
                        .constant
                        .is_some_and(|c| c[..hi.ty.width()].iter().all(|&e| e == 1.0));
                self.mapped(
                    &path,
                    cat,
                    if unit {
                        "Saturate"
                    } else {
                        "Min(Max(in, low), high)"
                    },
                );
                if unit {
                    self.unary("sat", &x, Node::Saturate)
                } else {
                    let m = self.binary(Op::Max, x, lo, "max");
                    self.binary(Op::Min, m, hi, "min")
                }
            }
            "dotproduct" => {
                let in_ty = elem.input("in1").and_then(|i| value_type(&i.ty)).unwrap_or(Vec3);
                let a = self.input(frame, elem, "in1", in_ty, zero);
                let b = self.input(frame, elem, "in2", in_ty, zero);
                self.mapped(&path, cat, "Dot");
                if in_ty == Float {
                    self.mul(a, b)
                } else {
                    let node = self.add("dot", Node::Dot(a.node, b.node));
                    Self::plain(node, Float)
                }
            }
            "normalize" => {
                let x = self.input(frame, elem, "in", ty, zero);
                self.mapped(&path, cat, "Normalize");
                self.unary("norm", &x, Node::Normalize)
            }
            "magnitude" => {
                let in_ty = elem.input("in").and_then(|i| value_type(&i.ty)).unwrap_or(Vec3);
                let x = self.input(frame, elem, "in", in_ty, zero);
                self.mapped(&path, cat, "Length");
                let node = self.add("len", Node::Length(x.node));
                Self::plain(node, Float)
            }
            "absval" | "sin" | "cos" => {
                let x = self.input(frame, elem, "in", ty, zero);
                let (hint, make): (&str, fn(String) -> Node) = match cat {
                    "absval" => ("abs", Node::Abs),
                    "sin" => ("sin", Node::Sin),
                    _ => ("cos", Node::Cos),
                };
                self.mapped(&path, cat, hint);
                self.unary(hint, &x, make)
            }
            "invert" => {
                let amount = self.input_own_type(frame, elem, "amount", ty, one);
                let x = self.input(frame, elem, "in", ty, zero);
                self.mapped(&path, cat, "Sub(amount, in)");
                let v = self.sub(amount, x);
                self.convert(v, ty)
            }
            "modulo" => {
                let a = self.input(frame, elem, "in1", ty, zero);
                let b = self.input_own_type(frame, elem, "in2", ty, one);
                let q = self.binary(Op::Div, a, b.clone(), "div");
                let f = self.unary("frac", &q, Node::Frac);
                self.mapped(&path, cat, "Mul(Frac(Div(in1, in2)), in2)");
                self.mul(f, b)
            }
            "floor" => {
                let x = self.input(frame, elem, "in", ty, zero);
                let f = self.unary("frac", &x, Node::Frac);
                self.mapped(&path, cat, "Sub(in, Frac(in))");
                self.sub(x, f)
            }
            "smoothstep" => {
                let x = self.input(frame, elem, "in", ty, zero);
                let lo = self.input_own_type(frame, elem, "low", ty, zero);
                let hi = self.input_own_type(frame, elem, "high", ty, one);
                self.mapped(&path, cat, "Smoothstep");
                let node = self.add(
                    "sstep",
                    Node::Smoothstep {
                        edge0: lo.node,
                        edge1: hi.node,
                        x: x.node,
                    },
                );
                Self::plain(node, ty)
            }
            "normalmap" => {
                let space = self
                    .string_input(frame, elem, "space")
                    .map_or("tangent".into(), |s| s.0);
                let encoded = self.input(frame, elem, "in", Vec3, [0.5, 0.5, 1.0, 0.0]);
                let scale = self.input_own_type(frame, elem, "scale", Float, one);
                let scale = if scale.ty == Float {
                    scale
                } else {
                    self.lossy(&path, "a per-axis normal scale is read as its x component");
                    self.swizzle(&scale, "x")
                };
                for unused in ["normal", "tangent", "bitangent"] {
                    if elem.input(unused).is_some_and(Input::is_connected) {
                        self.lossy(
                            &path,
                            format!("`{unused}` is ignored: the mesh's own frame is used"),
                        );
                    }
                }
                let unpacked = self.add("nmap", Node::UnpackNormal(encoded.node));
                if space == "object" {
                    self.lossy(
                        &path,
                        "an object-space normal map is read as a world-space normal",
                    );
                    self.mapped(&path, cat, "UnpackNormal (world)");
                    let n = self.add("nmap_n", Node::Normalize(unpacked));
                    Self::plain(n, Vec3)
                } else {
                    let strong = self.add(
                        "nmap_s",
                        Node::NormalStrength {
                            normal: unpacked,
                            strength: scale.node,
                        },
                    );
                    let tangent = self.add("nmap_t", Node::Normalize(strong));
                    let world = self.add("nmap_w", Node::WorldNormal);
                    self.normal_map_placeholders.insert(world.clone());
                    self.mapped(
                        &path,
                        cat,
                        "UnpackNormal + NormalStrength (tangent-space normal output)",
                    );
                    Val {
                        node: world,
                        ty: Vec3,
                        constant: None,
                        shape: Shape::NormalMap { tangent },
                    }
                }
            }
            "extract" => {
                let in_ty = elem.input("in").and_then(|i| value_type(&i.ty)).unwrap_or(Vec3);
                let x = self.input(frame, elem, "in", in_ty, zero);
                let index = self.const_int(frame, elem, "index", 0);
                let i = usize::try_from(index).unwrap_or(0).min(in_ty.width() - 1);
                self.mapped(&path, cat, "Swizzle");
                self.swizzle(&x, &"xyzw"[i..=i])
            }
            "combine2" | "combine3" | "combine4" => {
                let count = usize::from(cat.as_bytes()[7] - b'0');
                let mut acc: Option<Val> = None;
                for i in 1..=count {
                    let name = format!("in{i}");
                    let part = self.input_own_type(frame, elem, &name, Float, zero);
                    acc = Some(match acc {
                        None => part,
                        Some(prev) => {
                            let width = prev.ty.width() + part.ty.width();
                            let Some(wide) = ValueType::of_width(width) else {
                                return self.unsupported(
                                    frame,
                                    elem,
                                    "the inputs are wider than four components",
                                );
                            };
                            let node = self.add("cmb", Node::Append(prev.node, part.node));
                            Self::plain(node, wide)
                        }
                    });
                }
                self.mapped(&path, cat, "Append");
                let v = acc.unwrap_or_else(|| self.constant(ty, zero));
                self.convert(v, ty)
            }
            "separate2" | "separate3" | "separate4" => {
                let in_ty = elem.input("in").and_then(|i| value_type(&i.ty)).unwrap_or(Vec3);
                let x = self.input(frame, elem, "in", in_ty, zero);
                let out = output.unwrap_or("outx");
                let letter = out.chars().last().unwrap_or('x');
                let index = "xyzw".find(letter).or_else(|| "rgba".find(letter)).unwrap_or(0);
                let index = index.min(in_ty.width() - 1);
                self.mapped(&path, cat, "Swizzle");
                self.swizzle(&x, &"xyzw"[index..=index])
            }
            "swizzle" => {
                let in_ty = elem.input("in").and_then(|i| value_type(&i.ty)).unwrap_or(Vec3);
                let x = self.input(frame, elem, "in", in_ty, zero);
                let channels = self
                    .string_input(frame, elem, "channels")
                    .map_or(String::new(), |s| s.0);
                self.mapped(&path, cat, "Swizzle/Append");
                return self.swizzle_channels(frame, elem, &x, &channels, ty);
            }
            "convert" => {
                let in_ty = elem.input("in").and_then(|i| value_type(&i.ty)).unwrap_or(Float);
                let x = self.input(frame, elem, "in", in_ty, zero);
                self.mapped(&path, cat, "Swizzle/Append");
                self.convert(x, ty)
            }
            "ifgreater" | "ifgreatereq" | "ifequal" => {
                let (d1, d2) = if cat == "ifequal" {
                    (zero, zero)
                } else {
                    (one, zero)
                };
                let v1 = self.input(frame, elem, "value1", Float, d1);
                let v2 = self.input(frame, elem, "value2", Float, d2);
                if let (Some(a), Some(b)) = (v1.constant, v2.constant) {
                    let take_in1 = match cat {
                        "ifgreater" => a[0] > b[0],
                        "ifgreatereq" => a[0] >= b[0],
                        _ => a[0] == b[0],
                    };
                    let pick = if take_in1 { "in1" } else { "in2" };
                    self.mapped(&path, cat, format!("static switch: {pick}"));
                    self.input(frame, elem, pick, ty, zero)
                } else {
                    let in1 = self.input(frame, elem, "in1", ty, zero);
                    let in2 = self.input(frame, elem, "in2", ty, zero);
                    let diff = self.sub(v1, v2);
                    let t = match cat {
                        "ifequal" => {
                            let a = self.unary("abs", &diff, Node::Abs);
                            let gain = self.constant(Float, [STEP_GAIN, 0.0, 0.0, 0.0]);
                            let s = self.mul(a, gain);
                            let s = self.unary("sat", &s, Node::Saturate);
                            self.unary("eq", &s, Node::OneMinus)
                        }
                        _ => {
                            let diff = if cat == "ifgreatereq" {
                                let eps = self.constant(Float, [0.5 / STEP_GAIN, 0.0, 0.0, 0.0]);
                                self.add_vals(diff, eps)
                            } else {
                                diff
                            };
                            let gain = self.constant(Float, [STEP_GAIN, 0.0, 0.0, 0.0]);
                            let s = self.mul(diff, gain);
                            self.unary("gt", &s, Node::Saturate)
                        }
                    };
                    self.lossy(
                        &path,
                        "the operands are not constant: the choice is a steep blend (exact outside a 1e-5 band)",
                    );
                    self.gap("ifgreater/ifequal with varying operands: no step or select node");
                    self.mapped(&path, cat, "Lerp(in2, in1, step)");
                    self.lerp(in2, in1, t)
                }
            }
            "noise2d" | "noise3d" | "fractal3d" => {
                let octaves = if cat == "fractal3d" {
                    let o = self.const_int(frame, elem, "octaves", 3);
                    let clamped = o.clamp(1, 8);
                    if clamped != o {
                        self.lossy(&path, format!("{o} octaves clamped to {clamped}"));
                    }
                    if self.non_default(frame, elem, "lacunarity", [2.0, 0.0, 0.0, 0.0])
                        || self.non_default(frame, elem, "diminish", [0.5, 0.0, 0.0, 0.0])
                    {
                        self.lossy(&path, "lacunarity and diminish are fixed at 2 and 0.5");
                        self.gap("fractal noise with custom lacunarity/diminish");
                    }
                    u32::try_from(clamped).unwrap_or(1)
                } else {
                    1
                };
                let pos = if cat == "noise2d" {
                    match self.opt_input(frame, elem, "texcoord", Vec2) {
                        Some(uv) => uv,
                        None => self.mesh_uv(0),
                    }
                } else {
                    match self.opt_input(frame, elem, "position", Vec3) {
                        Some(p) => p,
                        None => {
                            let world = self.add("pos", Node::WorldPosition);
                            let origin = self.add("origin", Node::ObjectPosition);
                            let (world, origin) = (Self::plain(world, Vec3), Self::plain(origin, Vec3));
                            self.sub(world, origin)
                        }
                    }
                };
                let noise = self.add(
                    "noise",
                    Node::Noise {
                        input: pos.node,
                        octaves,
                    },
                );
                let noise = Self::plain(noise, Float);
                // Regia's noise is 0..1; MaterialX's is signed around the pivot.
                let two = self.constant(Float, [2.0, 0.0, 0.0, 0.0]);
                let one_c = self.constant(Float, one);
                let scaled = self.mul(noise, two);
                let signed = self.sub(scaled, one_c);
                let amplitude = self.input_own_type(frame, elem, "amplitude", Float, one);
                let pivot = self.input(frame, elem, "pivot", Float, zero);
                let amp = self.mul(signed, amplitude);
                let v = self.add_vals(amp, pivot);
                if ty.width() > 1 {
                    self.lossy(&path, "a vector noise repeats one scalar noise in every channel");
                    self.gap("vector-valued noise");
                }
                self.mapped(&path, cat, "Noise");
                self.convert(v, ty)
            }
            _ => {
                if let Some((def_name, _)) = self.doc.nodedef_for(cat, &elem.ty)
                    && self.doc.implementation(def_name).is_some()
                {
                    return self.custom(frame, elem, def_name, output);
                }
                let why = match cat {
                    "standard_surface" | "open_pbr_surface" | "surfacematerial" => "a shader read as a value",
                    "place2d" => {
                        self.gap("place2d (pivot/scale/rotate/offset of a texture coordinate)");
                        "no mapping"
                    }
                    _ => "no mapping",
                };
                return self.unsupported(frame, elem, why);
            }
        };
        Some(val)
    }

    /// `swizzle` with its `channels` string (letters of `xyzw`/`rgba`, or `0`/`1`).
    fn swizzle_channels(
        &mut self,
        frame: usize,
        elem: &NodeElem,
        x: &Val,
        channels: &str,
        ty: ValueType,
    ) -> Option<Val> {
        let width = x.ty.width();
        let letters = !channels.is_empty()
            && channels.len() <= 4
            && width > 1
            && (channels.chars().all(|c| "xyzw".contains(c)) || channels.chars().all(|c| "rgba".contains(c)));
        if letters {
            let normalized: String = channels
                .chars()
                .map(|c| match c {
                    'r' => 'x',
                    'g' => 'y',
                    'b' => 'z',
                    'a' => 'w',
                    other => other,
                })
                .collect();
            if normalized
                .chars()
                .any(|c| "xyzw".find(c).is_none_or(|i| i >= width))
            {
                return self.unsupported(frame, elem, &format!("channels `{channels}` read past the input"));
            }
            let v = self.swizzle(x, &normalized);
            return Some(self.convert(v, ty));
        }
        // At most four parts: checked before emitting any, so a hostile channel string does not
        // emit a node per character (each a name search over the graph, quadratic in its length).
        if channels.is_empty() || channels.chars().nth(4).is_some() {
            return self.unsupported(frame, elem, &format!("bad channels `{channels}`"));
        }
        let mut parts: Vec<Val> = Vec::new();
        for ch in channels.chars() {
            let part = match ch {
                '0' => self.constant(ValueType::Float, [0.0; 4]),
                '1' => self.constant(ValueType::Float, [1.0, 0.0, 0.0, 0.0]),
                c => {
                    let Some(i) = "xyzw".find(c).or_else(|| "rgba".find(c)) else {
                        return self.unsupported(frame, elem, &format!("bad channels `{channels}`"));
                    };
                    if i >= width && width > 1 {
                        return self.unsupported(
                            frame,
                            elem,
                            &format!("channels `{channels}` read past the input"),
                        );
                    }
                    self.swizzle(x, &"xyzw"[i.min(width - 1)..=i.min(width - 1)])
                }
            };
            parts.push(part);
        }
        if parts.is_empty() || parts.len() > 4 {
            return self.unsupported(frame, elem, &format!("bad channels `{channels}`"));
        }
        let mut acc = parts.remove(0);
        for p in parts {
            let wide = ValueType::of_width(acc.ty.width() + 1).unwrap_or(ValueType::Vec4);
            let node = self.add("swzc", Node::Append(acc.node, p.node));
            acc = Self::plain(node, wide);
        }
        Some(self.convert(acc, ty))
    }

    /// Expands a custom node (a nodedef with a node graph implementation) in a new scope.
    fn custom(
        &mut self,
        frame: usize,
        elem: &'d NodeElem,
        def_name: &str,
        output: Option<&str>,
    ) -> Option<Val> {
        let def = self.doc.nodedefs.get(def_name)?;
        let graph = self.doc.implementation(def_name)?;
        if self.frames.len() > MAX_EXPANSIONS {
            self.fatal.get_or_insert(MaterialXError::Limit(format!(
                "more than {MAX_EXPANSIONS} custom node expansions"
            )));
            return None;
        }
        let path = self.path_of(frame, &elem.name);
        let mut bindings = HashMap::new();
        for input in &def.inputs {
            let paramable =
                value_type(&input.ty).is_some() && !matches!(input.ty.as_str(), "integer" | "boolean");
            let binding = match elem.input(&input.name) {
                Some(given) if given.is_connected() => Binding::Pending {
                    input: given,
                    frame,
                    param: None,
                },
                Some(given) => Binding::Pending {
                    input: given,
                    frame,
                    param: paramable.then(|| {
                        (
                            format!("{}_{}", elem.name, input.name),
                            format!("{path}.{}", input.name),
                        )
                    }),
                },
                None if input.value.is_some() || input.interfacename.is_some() => Binding::Pending {
                    input,
                    frame,
                    param: paramable.then(|| {
                        (
                            format!("{}_{}", elem.name, input.name),
                            format!("{path}.{}", input.name),
                        )
                    }),
                },
                None => Binding::Missing,
            };
            bindings.insert(input.name.clone(), binding);
        }
        let id = self.frames.len();
        self.frames.push(Frame {
            graph: Some(graph),
            path: path.clone(),
            bindings,
        });
        let out = match output {
            Some(o) => graph.outputs.iter().find(|x| x.name == o),
            None => graph.outputs.first(),
        };
        let Some(out) = out else {
            return self.unsupported(frame, elem, "its node graph has no such output");
        };
        self.mapped(
            &path,
            &elem.category,
            format!("custom node `{def_name}` (flattened)"),
        );
        self.scope_output(id, out)
    }

    /// `image` and `tiledimage`.
    fn image(&mut self, frame: usize, elem: &'d NodeElem, ty: ValueType) -> Option<Val> {
        let path = self.path_of(frame, &elem.name);
        let cat = elem.category.as_str();
        let default = |t: &mut Self| {
            let d = t.input(frame, elem, "default", ty, [0.0; 4]);
            Some(d)
        };
        let Some((file, colorspace, prefix)) = self
            .string_input(frame, elem, "file")
            .filter(|f| !f.0.trim().is_empty())
        else {
            self.warn(
                WARN_TEXTURE_PATH,
                &path,
                cat,
                "no `file`; the image's default is used",
            );
            return default(self);
        };
        let Some(project_path) = self.image_path(&path, cat, &prefix, file.trim()) else {
            return default(self);
        };
        // Colour images follow their colour space (`srgb_texture` decodes sRGB); data images
        // (float, vector) are read raw.
        let srgb = elem.ty.starts_with("color")
            && colorspace
                .as_deref()
                .is_some_and(|cs| cs.starts_with("srgb") || cs == "g22_rec709");
        let Some(texture) = self.texture(&path, &project_path, srgb) else {
            return default(self);
        };
        for mode in ["uaddressmode", "vaddressmode"] {
            if let Some((m, ..)) = self.string_input(frame, elem, mode)
                && m != "periodic"
            {
                self.lossy(&path, format!("{mode} `{m}`: the graph sampler repeats"));
            }
        }
        if let Some((f, ..)) = self.string_input(frame, elem, "filtertype")
            && f != "linear"
        {
            self.lossy(
                &path,
                format!("filtertype `{f}`: the graph sampler filters linearly"),
            );
        }
        // The texture coordinate: unchanged mesh UVs sample directly; anything computed in
        // MaterialX space is flipped back.
        let uv_in = match self.opt_input(frame, elem, "texcoord", ValueType::Vec2) {
            Some(uv) => uv,
            None => {
                let raw = self.mesh_uv(0);
                let mut uv = raw.clone();
                uv.shape = Shape::MxTexcoord { raw: raw.node };
                uv
            }
        };
        let uv = if cat == "tiledimage" {
            for unit in ["realworldimagesize", "realworldtilesize"] {
                if elem.input(unit).is_some() {
                    self.lossy(&path, format!("`{unit}` is ignored"));
                }
            }
            let tiling = self.input(frame, elem, "uvtiling", ValueType::Vec2, [1.0, 1.0, 0.0, 0.0]);
            let offset = self.input(frame, elem, "uvoffset", ValueType::Vec2, [0.0; 4]);
            match (&uv_in.shape, tiling.constant, offset.constant) {
                (Shape::MxTexcoord { raw }, Some(s), Some(o)) => {
                    // v' = v_mx * s + o with v_mx = 1 - v, sampled at 1 - v'.
                    let offset = if self.settings.flip_v {
                        [o[0], 1.0 - s[1] - o[1]]
                    } else {
                        [o[0], o[1]]
                    };
                    let node = self.add(
                        "tile",
                        Node::UvTransform {
                            uv: raw.clone(),
                            scale: [s[0], s[1]],
                            offset,
                            rotation: 0.0,
                        },
                    );
                    Self::plain(node, ValueType::Vec2)
                }
                _ => {
                    // The default texcoord carries the mesh UV itself (its MaterialX value is
                    // that flipped, which the branch above folds into the transform): flip it
                    // into MaterialX space before the arithmetic, as an explicit `texcoord` is.
                    let uv_in = match &uv_in.shape {
                        Shape::MxTexcoord { raw } if *raw == uv_in.node => self.flip_v(&uv_in),
                        _ => uv_in,
                    };
                    let scaled = self.mul(uv_in, tiling);
                    let mx = self.add_vals(scaled, offset);
                    self.flip_v(&mx)
                }
            }
        } else {
            match &uv_in.shape {
                Shape::MxTexcoord { raw } => Self::plain(raw.clone(), ValueType::Vec2),
                _ => self.flip_v(&uv_in),
            }
        };
        let sample = self.add(
            &format!("tex_{texture}"),
            Node::Sample {
                texture: texture.clone(),
                uv: uv.node,
                lod: None,
            },
        );
        let sample = Self::plain(sample, ValueType::Vec4);
        self.mapped(&path, cat, format!("Sample `{texture}` ({project_path})"));
        Some(match ty.width() {
            4 => sample,
            w => self.swizzle(&sample, &"xyzw"[..w]),
        })
    }
}
