// SPDX-License-Identifier: MPL-2.0
//! Graph → WGSL: lowering, validation, type inference, topological emission of
//! `material_surface` and (with a world position offset) `material_vertex_offset`.
//!
//! Static switches and material functions are lowered first (the `lower` module). Only nodes
//! reachable from the outputs are emitted, each as one `let` in dependency order, so the
//! generated functions are straight-line code (implicit-derivative samples stay in uniform
//! control flow). A node feeding both stages is emitted in both. Errors carry the node name
//! they belong to, so an editor or agent can point at the node. The emitted source is
//! validated by composing it into every host module
//! ([`regia_render::surface_graph::validate_graph_code`]); a failure there is a compiler bug
//! and is reported without a node.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::sync::Arc;

use regia_render::surface_graph::{
    GRAPH_PARAM_SLOTS, GRAPH_SAMPLER, GRAPH_TEXTURE_BINDINGS, GRAPH_TEXTURE_SLOTS, GraphCode,
};

use crate::format::{
    ColorSpace, GraphAlphaMode, MATGRAPH_FORMAT_VERSION, MaterialGraph, Node, NormalBlend, NormalSpace,
    ParamValue, ValueType,
};
use crate::lower::{Lowered, lower};

/// Parameters a graph may declare (slot 0 is the built-ins).
pub const MAX_PARAMS: usize = GRAPH_PARAM_SLOTS - 1;

/// Most layers a `Noise` node may sum.
pub const MAX_NOISE_OCTAVES: u32 = 8;

/// Most static switches one graph may declare (ADR-4901): at most 2^16 code permutations of
/// one graph, of which the cook compiles only the ones its instances pick.
pub const MAX_STATIC_SWITCHES: usize = 16;

/// Most flat nodes (plus switch, call and argument aliases) one graph may lower to after its
/// material functions expand (ADR-4901). Passing it is an error on the authored call node.
pub const MAX_LOWERED_NODES: usize = 4096;

/// Message prefix of the one diagnostic no authored node maps to: the generated WGSL failed
/// validation in a host shader ([`CompileError::code`] `generated_code_invalid`).
pub const GENERATED_CODE_INVALID_PREFIX: &str = "generated WGSL failed validation (compiler bug)";

/// Every [`CompileError::code`], for the agent protocol and the book page.
pub const COMPILE_ERROR_CODES: [&str; 8] = [
    "node",
    "param",
    "texture",
    "switch",
    "function",
    "output",
    "graph",
    "generated_code_invalid",
];

/// One diagnostic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompileError {
    /// The node (or `param:<name>`, `texture:<name>`, `output:<slot>`) the error belongs to;
    /// `None` for whole-graph problems.
    pub node: Option<String>,
    /// What is wrong.
    pub message: String,
}

impl CompileError {
    /// The stable lowercase code of the diagnostic (one of [`COMPILE_ERROR_CODES`]): what
    /// [`Self::node`] names (`node`, `param`, `texture`, `switch`, `function`, `output`), `graph`
    /// for a whole-graph problem, and `generated_code_invalid` for generated code a host shader
    /// rejected, the one failure no authored node maps to (never silently dropped: the compile
    /// and the cook fail on it).
    #[must_use]
    pub fn code(&self) -> &'static str {
        let Some(node) = self.node.as_deref() else {
            return if self.message.starts_with(GENERATED_CODE_INVALID_PREFIX) {
                "generated_code_invalid"
            } else {
                "graph"
            };
        };
        for (prefix, code) in [
            ("param:", "param"),
            ("texture:", "texture"),
            ("switch:", "switch"),
            ("function:", "function"),
            ("output:", "output"),
        ] {
            if node.starts_with(prefix) {
                return code;
            }
        }
        "node"
    }

    /// The authored node the diagnostic points at: for a node inside an expanded function
    /// (`call/inner`), the call node `call` the graph holds; `None` for anything that is not a
    /// node ([`Self::code`] other than `node`).
    #[must_use]
    pub fn authored_node(&self) -> Option<&str> {
        let node = self.node.as_deref()?;
        if self.code() != "node" {
            return None;
        }
        node.split('/').next()
    }
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.node {
            Some(node) => write!(f, "{node}: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

/// Every diagnostic of a failed compile.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("material graph has {} error(s): {}", .0.len(), .0.iter().map(ToString::to_string).collect::<Vec<_>>().join("; "))]
pub struct CompileErrors(pub Vec<CompileError>);

/// Where a parameter lives in the uniform block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParamSlot {
    /// Parameter name.
    pub name: String,
    /// `vec4` slot (1..16).
    pub slot: usize,
    /// Declared type.
    pub ty: ValueType,
}

/// Where a texture is bound.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextureSlot {
    /// Texture name.
    pub name: String,
    /// Slot index into [`GRAPH_TEXTURE_BINDINGS`].
    pub slot: usize,
    /// Colour space it is uploaded in.
    pub color_space: ColorSpace,
}

/// The code half of a compiled graph: shared by every material and instance with the same
/// structure and switch values (parameter values and images are not part of it).
#[derive(Clone, Debug, PartialEq)]
pub struct CompiledGraph {
    /// The generated `material_surface` function.
    pub wgsl: Arc<str>,
    /// The generated `material_vertex_offset` function (world position offset), if any.
    pub vertex_wgsl: Option<Arc<str>>,
    /// The graph reads the second UV set.
    pub uses_uv1: bool,
    /// The graph reads `Time` or `Wind`: the renderer writes graph parameter slot 0 (the
    /// material clock in `xy`, the scene wind in `zw`) only into graphs that set this.
    pub uses_time: bool,
    /// Pipeline cache key ([`GraphCode::key`] of the code fields).
    pub key: u64,
    /// Structural hash of the source graph and switch values (the CPU compile-cache key).
    pub graph_hash: u64,
    /// Effective static switch values this permutation was compiled with.
    pub switches: BTreeMap<String, bool>,
    /// Parameter layout.
    pub params: Vec<ParamSlot>,
    /// Texture layout.
    pub textures: Vec<TextureSlot>,
    /// Inferred type of every emitted node.
    pub node_types: BTreeMap<String, ValueType>,
    /// Flat nodes and aliases after switches and functions lowered (at most
    /// [`MAX_LOWERED_NODES`], ADR-4901).
    pub lowered_nodes: usize,
    /// Function calls expanded, every nesting level counted (ADR-4901).
    pub function_calls: usize,
}

impl CompiledGraph {
    /// The slot of parameter `name`.
    #[must_use]
    pub fn param(&self, name: &str) -> Option<&ParamSlot> {
        self.params.iter().find(|p| p.name == name)
    }

    /// The parameter slot the renderer writes the floating origin into for this graph, or `None`
    /// when the graph reads no world or object position
    /// ([`regia_render::surface_graph::graph_origin_slot`], ADR-2435). A graph that reads a
    /// position and every parameter slot does not compile. The generated code does not add the
    /// origin itself.
    #[must_use]
    pub fn origin_slot(&self) -> Option<usize> {
        regia_render::surface_graph::graph_origin_slot(&self.code())
    }

    /// The code fields as the renderer keys them.
    #[must_use]
    pub fn code(&self) -> GraphCode {
        GraphCode {
            surface: self.wgsl.clone(),
            vertex: self.vertex_wgsl.clone(),
            uv1: self.uses_uv1,
        }
    }
}

/// FNV-1a 64 over bytes.
pub(crate) fn fnv64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Structural hash with the declared switch defaults ([`permutation_hash`]).
#[must_use]
pub fn graph_hash(graph: &MaterialGraph) -> u64 {
    permutation_hash(graph, &BTreeMap::new())
}

/// Structural hash of one permutation: nodes, functions, outputs, the parameter and texture
/// *layout* and the effective switch values. Default values, texture paths, the name and the
/// shading block do not change the code and are left out.
#[must_use]
pub fn permutation_hash(graph: &MaterialGraph, switches: &BTreeMap<String, bool>) -> u64 {
    let params: Vec<(&str, ValueType)> = graph
        .params
        .iter()
        .map(|p| (p.name.as_str(), p.value.value_type()))
        .collect();
    let textures: Vec<(&str, ColorSpace)> = graph
        .textures
        .iter()
        .map(|t| (t.name.as_str(), t.color_space))
        .collect();
    let effective: Vec<(&str, bool)> = graph
        .switches
        .iter()
        .map(|s| {
            (
                s.name.as_str(),
                switches.get(&s.name).copied().unwrap_or(s.default),
            )
        })
        .collect();
    let text = ron::to_string(&(
        &graph.nodes,
        &graph.output,
        params,
        textures,
        effective,
        &graph.functions,
    ))
    .unwrap_or_default();
    fnv64(text.as_bytes())
}

/// Which generated function a node is emitted into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    /// `material_surface` (per pixel).
    Surface,
    /// `material_vertex_offset` (per vertex, every pass).
    Vertex,
}

struct Ctx<'a> {
    graph: &'a MaterialGraph,
    /// The lowered nodes (switches and calls resolved).
    nodes: &'a BTreeMap<String, Node>,
    errors: Vec<CompileError>,
    params: HashMap<&'a str, (usize, ValueType)>,
    textures: HashMap<&'a str, usize>,
}

impl Ctx<'_> {
    fn err(&mut self, node: impl Into<String>, message: impl Into<String>) {
        self.errors.push(CompileError {
            node: Some(node.into()),
            message: message.into(),
        });
    }
}

pub(crate) fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

static COMPILES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Graph compiles ([`compile_unvalidated_with`] and everything built on it) in this process
/// (all threads, background builders included) since start. A shipped game loads cooked
/// graphs and never moves it.
#[must_use]
pub fn compile_count() -> u64 {
    COMPILES.load(std::sync::atomic::Ordering::Relaxed)
}

thread_local! {
    static THREAD_COMPILES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Graph compiles run by the calling thread since it started: unlike [`compile_count`] it
/// ignores compiles on other threads, so a test can assert "this thread did not compile"
/// while other tests compile in parallel.
#[must_use]
pub fn thread_compile_count() -> u64 {
    THREAD_COMPILES.with(std::cell::Cell::get)
}

/// Compiles a graph with its declared switch defaults.
///
/// # Errors
///
/// [`CompileErrors`] with every problem found (unknown references, cycles, type mismatches,
/// too many parameters or textures, non-finite constants, bad switches or calls, nodes the
/// vertex stage cannot evaluate, a position read that fills every parameter slot).
pub fn compile(graph: &MaterialGraph) -> Result<CompiledGraph, CompileErrors> {
    compile_with(graph, &BTreeMap::new())
}

/// Compiles one permutation: `switches` overrides the declared switch defaults (an instance's
/// [`crate::MaterialInstance::switches`]).
///
/// # Errors
///
/// As [`compile`].
pub fn compile_with(
    graph: &MaterialGraph,
    switches: &BTreeMap<String, bool>,
) -> Result<CompiledGraph, CompileErrors> {
    let compiled = compile_unvalidated_with(graph, switches)?;
    if let Err(err) = regia_render::surface_graph::validate_graph_code(&compiled.code()) {
        // Code nodes passed their standalone check; what fails only once composed (a name an
        // include shares with the host shader, a derivative per vertex) is theirs, not a
        // compiler bug (ADR-7852).
        let codes: Vec<&String> = graph
            .nodes
            .iter()
            .filter(|(_, n)| matches!(n, Node::Code { .. }))
            .map(|(k, _)| k)
            .collect();
        if !codes.is_empty() {
            return Err(CompileErrors(
                codes
                    .into_iter()
                    .map(|name| CompileError {
                        node: Some(name.clone()),
                        message: format!("code: the graph's code failed in a host shader: {err}"),
                    })
                    .collect(),
            ));
        }
        return Err(CompileErrors(vec![CompileError {
            node: None,
            message: format!("{GENERATED_CODE_INVALID_PREFIX}: {err}"),
        }]));
    }
    Ok(compiled)
}

/// [`compile`] without composing the result into the host shaders (fast; for tests and
/// editors that validate on a background thread).
///
/// # Errors
///
/// As [`compile`], minus the WGSL validation step.
pub fn compile_unvalidated(graph: &MaterialGraph) -> Result<CompiledGraph, CompileErrors> {
    compile_unvalidated_with(graph, &BTreeMap::new())
}

/// Rejects NaN or infinite [`crate::Shading`] values (they reach the lighting and the vertex
/// bounds unchecked otherwise).
fn check_shading_finite(ctx: &mut Ctx<'_>) {
    let sh = ctx.graph.shading;
    let mut values = vec![
        ("clearcoat", sh.clearcoat),
        ("clearcoat_roughness", sh.clearcoat_roughness),
        ("anisotropy", sh.anisotropy),
        ("anisotropy_rotation", sh.anisotropy_rotation),
        ("max_vertex_offset", sh.max_vertex_offset),
    ];
    if let GraphAlphaMode::Mask(cutoff) = sh.alpha_mode {
        values.push(("alpha cutoff", cutoff));
    }
    for (what, value) in values {
        if !value.is_finite() {
            ctx.errors.push(CompileError {
                node: None,
                message: format!("shading `{what}` must be finite"),
            });
        }
    }
    if sh.max_vertex_offset < 0.0 {
        ctx.errors.push(CompileError {
            node: None,
            message: "shading `max_vertex_offset` must not be negative (give the amplitude)".into(),
        });
    }
}

/// [`compile_with`] without the WGSL validation step.
///
/// # Errors
///
/// As [`compile`], minus the WGSL validation step.
pub fn compile_unvalidated_with(
    graph: &MaterialGraph,
    switches: &BTreeMap<String, bool>,
) -> Result<CompiledGraph, CompileErrors> {
    COMPILES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    THREAD_COMPILES.with(|c| c.set(c.get() + 1));
    let mut errors = Vec::new();
    if graph.format_version != MATGRAPH_FORMAT_VERSION {
        errors.push(CompileError {
            node: None,
            message: format!(
                "format_version {} (migrate to v{MATGRAPH_FORMAT_VERSION} first)",
                graph.format_version
            ),
        });
    }
    for name in graph.nodes.keys() {
        if !valid_name(name) {
            errors.push(CompileError {
                node: Some(name.clone()),
                message: "node names are 1–64 characters of [A-Za-z0-9_]".into(),
            });
        }
    }
    let lowered = lower(graph, switches, &mut errors);
    let mut ctx = Ctx {
        graph,
        nodes: &lowered.nodes,
        errors,
        params: HashMap::new(),
        textures: HashMap::new(),
    };
    declare(&mut ctx);
    check_shading_finite(&mut ctx);
    let slots = outputs(graph, &lowered);
    let mut all_roots: Vec<(&'static str, &str)> = slots
        .iter()
        .filter_map(|o| Some((o.slot, o.node.as_deref()?)))
        .collect();
    // A function argument the chosen output never reads is still type-checked.
    all_roots.extend(
        lowered
            .arg_checks
            .iter()
            .map(|c| ("function argument", c.arg.as_str())),
    );
    let order_all = order(&mut ctx, &all_roots, true);
    let types = infer(&mut ctx, &order_all);
    check_outputs(&mut ctx, &slots, &types);
    check_args(&mut ctx, &lowered, &types);
    let surface_roots: Vec<(&'static str, &str)> = slots
        .iter()
        .filter(|o| !o.vertex)
        .filter_map(|o| Some((o.slot, o.node.as_deref()?)))
        .collect();
    let vertex_roots: Vec<(&'static str, &str)> = slots
        .iter()
        .filter(|o| o.vertex)
        .filter_map(|o| Some((o.slot, o.node.as_deref()?)))
        .collect();
    let surface_order = order(&mut ctx, &surface_roots, false);
    let vertex_order = order(&mut ctx, &vertex_roots, false);
    check_vertex_stage(&mut ctx, &vertex_order);
    if !ctx.errors.is_empty() {
        return Err(CompileErrors(ctx.errors));
    }
    let output_of = |slot: &str| slots.iter().find(|o| o.slot == slot).and_then(|o| o.node.clone());
    let wgsl: Arc<str> = emit(&ctx, &surface_order, &types, &output_of).into();
    let vertex_wgsl: Option<Arc<str>> =
        output_of("world_position_offset").map(|root| emit_vertex(&ctx, &vertex_order, &types, &root).into());
    let reads = |pred: fn(&Node) -> bool| order_all.iter().any(|name| ctx.nodes.get(name).is_some_and(pred));
    let uses_uv1 = reads(|node| matches!(node, Node::TexCoord1));
    // Slot 0 carries the clock and the wind together; the renderer fills it only for graphs that
    // read either.
    let uses_time = reads(|node| matches!(node, Node::Time | Node::Wind));
    let params = graph
        .params
        .iter()
        .enumerate()
        .map(|(i, p)| ParamSlot {
            name: p.name.clone(),
            slot: i + 1,
            ty: p.value.value_type(),
        })
        .collect();
    let textures = graph
        .textures
        .iter()
        .enumerate()
        .map(|(slot, t)| TextureSlot {
            name: t.name.clone(),
            slot,
            color_space: t.color_space,
        })
        .collect();
    let code = GraphCode {
        surface: wgsl.clone(),
        vertex: vertex_wgsl.clone(),
        uv1: uses_uv1,
    };
    // A position read needs one free parameter slot for the floating origin (spec 78 § 12,
    // ADR-2435, ADR-5005). Filling every slot used to compile and then shade local positions.
    let reads_position = |text: &str| text.contains(".world_position") || text.contains(".object_position");
    if (reads_position(&wgsl) || vertex_wgsl.as_deref().is_some_and(reads_position))
        && regia_render::surface_graph::graph_origin_slot(&code).is_none()
    {
        return Err(CompileErrors(vec![CompileError {
            node: None,
            message: format!(
                "this graph reads a world or object position and every parameter slot; \
                 the floating origin has no free slot (at most {} parameters may be read \
                 together with a position)",
                MAX_PARAMS - 1
            ),
        }]));
    }
    Ok(CompiledGraph {
        key: code.key(),
        wgsl,
        vertex_wgsl,
        uses_uv1,
        uses_time,
        graph_hash: permutation_hash(graph, switches),
        switches: lowered.switches.clone(),
        params,
        textures,
        node_types: order_all
            .iter()
            .filter_map(|n| Some((n.to_string(), *types.get(n.as_str())?)))
            .collect(),
        lowered_nodes: lowered.size(),
        function_calls: lowered.calls,
    })
}

fn declare(ctx: &mut Ctx<'_>) {
    let graph = ctx.graph;
    if graph.params.len() > MAX_PARAMS {
        ctx.errors.push(CompileError {
            node: None,
            message: format!("{} parameters; at most {MAX_PARAMS}", graph.params.len()),
        });
    }
    for (i, p) in graph.params.iter().enumerate() {
        let key = format!("param:{}", p.name);
        if !valid_name(&p.name) {
            ctx.err(key.clone(), "parameter names are 1–64 characters of [A-Za-z0-9_]");
        }
        if !p.value.is_finite() {
            ctx.err(key.clone(), "default is not finite");
        }
        if let Some(why) = p.metadata_error() {
            ctx.err(key.clone(), why);
        }
        if ctx
            .params
            .insert(p.name.as_str(), (i + 1, p.value.value_type()))
            .is_some()
        {
            ctx.err(key, "declared twice");
        }
    }
    if graph.textures.len() > GRAPH_TEXTURE_SLOTS {
        ctx.errors.push(CompileError {
            node: None,
            message: format!("{} textures; at most {GRAPH_TEXTURE_SLOTS}", graph.textures.len()),
        });
    }
    for (slot, t) in graph.textures.iter().enumerate() {
        let key = format!("texture:{}", t.name);
        if !valid_name(&t.name) {
            ctx.err(key.clone(), "texture names are 1–64 characters of [A-Za-z0-9_]");
        }
        if ctx.textures.insert(t.name.as_str(), slot).is_some() {
            ctx.err(key, "declared twice");
        }
    }
}

/// One surface output and the (lowered) node driving it.
struct OutputSlot {
    slot: &'static str,
    node: Option<String>,
    ty: ValueType,
    /// Evaluated per vertex.
    vertex: bool,
}

fn outputs(graph: &MaterialGraph, lowered: &Lowered) -> Vec<OutputSlot> {
    let o = &graph.output;
    let slot = |slot, node: Option<&String>, ty, vertex| OutputSlot {
        slot,
        // An output may name a switch or call node: point it at what that resolves to.
        node: node.map(|name| lowered.resolve(name).unwrap_or_else(|| name.clone())),
        ty,
        vertex,
    };
    vec![
        slot("base_color", o.base_color.as_ref(), ValueType::Vec3, false),
        slot("alpha", o.alpha.as_ref(), ValueType::Float, false),
        slot("metallic", o.metallic.as_ref(), ValueType::Float, false),
        slot("roughness", o.roughness.as_ref(), ValueType::Float, false),
        slot("normal", o.normal.as_ref(), ValueType::Vec3, false),
        slot("occlusion", o.occlusion.as_ref(), ValueType::Float, false),
        slot("emissive", o.emissive.as_ref(), ValueType::Vec3, false),
        slot(
            "world_position_offset",
            o.world_position_offset.as_ref(),
            ValueType::Vec3,
            true,
        ),
    ]
}

/// Post-order DFS from `roots` (output slot, node): dependencies first. With `report`,
/// reports unknown references and cycles.
fn order(ctx: &mut Ctx<'_>, roots: &[(&'static str, &str)], report: bool) -> Vec<String> {
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        Visiting,
        Done,
    }
    let nodes = ctx.nodes;
    let mut marks: HashMap<&str, Mark> = HashMap::new();
    let mut out = Vec::new();
    let mut starts = Vec::new();
    for &(slot, name) in roots {
        if let Some((key, _)) = nodes.get_key_value(name) {
            starts.push(key.as_str());
        } else if report {
            ctx.err(format!("output:{slot}"), format!("unknown node `{name}`"));
        }
    }
    let roots = starts;
    // Iterative DFS: (node, next input index).
    for root in roots {
        let mut stack: Vec<(&str, usize)> = vec![(root, 0)];
        while let Some(&mut (name, ref mut next)) = stack.last_mut() {
            if *next == 0 {
                match marks.get(name) {
                    Some(Mark::Done) => {
                        stack.pop();
                        continue;
                    }
                    Some(Mark::Visiting) => {
                        stack.pop();
                        continue;
                    }
                    None => {
                        marks.insert(name, Mark::Visiting);
                    }
                }
            }
            let Some(node) = nodes.get(name) else {
                stack.pop();
                continue;
            };
            let inputs = node.inputs();
            if *next < inputs.len() {
                let input = inputs[*next];
                *next += 1;
                match (nodes.get_key_value(input), marks.get(input)) {
                    (None, _) => {
                        if report && input.is_empty() {
                            ctx.err(name, "an input is not connected");
                        } else if report {
                            ctx.err(name, format!("input `{input}` is not a node"));
                        }
                    }
                    (Some(_), Some(Mark::Visiting)) => {
                        if report {
                            ctx.err(name, format!("cycle through `{input}`"));
                        }
                    }
                    (Some(_), Some(Mark::Done)) => {}
                    (Some((key, _)), None) => stack.push((key.as_str(), 0)),
                }
            } else {
                marks.insert(name, Mark::Done);
                out.push(name.to_string());
                stack.pop();
            }
        }
    }
    out
}

fn same_or_scalar(ctx: &mut Ctx<'_>, node: &str, a: ValueType, b: ValueType) -> Option<ValueType> {
    if a == b {
        Some(a)
    } else if a == ValueType::Float {
        Some(b)
    } else if b == ValueType::Float {
        Some(a)
    } else {
        ctx.err(
            node,
            format!("operands are {a:?} and {b:?}; widths must match or one side must be Float"),
        );
        None
    }
}

fn infer(ctx: &mut Ctx<'_>, order: &[String]) -> HashMap<String, ValueType> {
    let mut types: HashMap<String, ValueType> = HashMap::new();
    let nodes = ctx.nodes;
    for name in order {
        let Some(node) = nodes.get(name) else {
            continue;
        };
        // Skip nodes whose inputs failed (their own error was already reported).
        let ins: Option<Vec<ValueType>> = node.inputs().iter().map(|i| types.get(*i).copied()).collect();
        let Some(ins) = ins else {
            continue;
        };
        if let Some(texture) = node.texture()
            && !ctx.textures.contains_key(texture)
        {
            ctx.err(name.clone(), format!("unknown texture `{texture}`"));
            continue;
        }
        let ty = node_type(ctx, name, node, &ins);
        if let Some(ty) = ty {
            types.insert(name.clone(), ty);
        }
    }
    types
}

fn expect(ctx: &mut Ctx<'_>, node: &str, what: &str, got: ValueType, want: ValueType) -> bool {
    if got == want {
        true
    } else {
        ctx.err(node, format!("{what} must be {want:?}, got {got:?}"));
        false
    }
}

fn finite(ctx: &mut Ctx<'_>, node: &str, values: &[f32]) -> bool {
    if values.iter().all(|v| v.is_finite()) {
        true
    } else {
        ctx.err(node, "constant is not finite");
        false
    }
}

fn swizzle_indices(channels: &str) -> Option<Vec<usize>> {
    let xyzw = channels.chars().all(|c| "xyzw".contains(c));
    let rgba = channels.chars().all(|c| "rgba".contains(c));
    if channels.is_empty() || channels.len() > 4 || !(xyzw || rgba) {
        return None;
    }
    Some(
        channels
            .chars()
            .map(|c| match c {
                'x' | 'r' => 0,
                'y' | 'g' => 1,
                'z' | 'b' => 2,
                _ => 3,
            })
            .collect(),
    )
}

fn node_type(ctx: &mut Ctx<'_>, name: &str, node: &Node, ins: &[ValueType]) -> Option<ValueType> {
    use ValueType::{Float, Vec2, Vec3, Vec4};
    match node {
        Node::TexCoord | Node::TexCoord1 | Node::Wind => Some(Vec2),
        Node::VertexColor => Some(Vec4),
        Node::WorldPosition | Node::WorldNormal | Node::ViewDirection | Node::ObjectPosition => Some(Vec3),
        Node::Sin(_) | Node::Cos(_) | Node::Frac(_) => Some(ins[0]),
        Node::Noise { octaves, .. } => {
            let mut ok = true;
            if !(1..=MAX_NOISE_OCTAVES).contains(octaves) {
                ctx.err(
                    name,
                    format!("octaves must be 1–{MAX_NOISE_OCTAVES}, got {octaves}"),
                );
                ok = false;
            }
            if !matches!(ins[0], Float | Vec2 | Vec3) {
                ctx.err(
                    name,
                    format!("input must be Float, Vec2 or Vec3, got {:?}", ins[0]),
                );
                ok = false;
            }
            ok.then_some(Float)
        }
        Node::Switch { .. } | Node::Call { .. } | Node::Input(_) => {
            // Lowering replaces these; one left over is a lowering error already reported.
            None
        }
        Node::Code { inputs, output, .. } => {
            let mut ok = true;
            for (input, got) in inputs.iter().zip(ins) {
                if *got != input.ty {
                    ctx.err(
                        name,
                        format!(
                            "input `{}` is {got:?}; the code declares {:?} (use Swizzle or Append)",
                            input.name, input.ty
                        ),
                    );
                    ok = false;
                }
            }
            for diag in crate::code::check_node(ctx.graph, node) {
                ctx.err(name, diag.to_string());
                ok = false;
            }
            ok.then_some(*output)
        }
        Node::CameraDistance | Node::Time => Some(Float),
        Node::Param(param) => match ctx.params.get(param.as_str()) {
            Some(&(_, ty)) => Some(ty),
            None => {
                ctx.err(name, format!("unknown parameter `{param}`"));
                None
            }
        },
        Node::Float(v) => finite(ctx, name, &[*v]).then_some(Float),
        Node::Vec2(v) => finite(ctx, name, v).then_some(Vec2),
        Node::Vec3(v) => finite(ctx, name, v).then_some(Vec3),
        Node::Vec4(v) => finite(ctx, name, v).then_some(Vec4),
        Node::Sample { lod, .. } => {
            let mut ok = expect(ctx, name, "uv", ins[0], Vec2);
            if lod.is_some() {
                ok &= expect(ctx, name, "lod", ins[1], Float);
            }
            ok.then_some(Vec4)
        }
        Node::Triplanar { sharpness, .. } => {
            let mut ok = expect(ctx, name, "scale", ins[0], Float);
            if !(sharpness.is_finite() && *sharpness >= 1.0) {
                ctx.err(name, "sharpness must be ≥ 1");
                ok = false;
            }
            ok.then_some(Vec4)
        }
        Node::UvTransform {
            scale,
            offset,
            rotation,
            ..
        } => {
            let ok = expect(ctx, name, "uv", ins[0], Vec2)
                && finite(ctx, name, &[scale[0], scale[1], offset[0], offset[1], *rotation]);
            ok.then_some(Vec2)
        }
        Node::Parallax {
            min_steps, max_steps, ..
        } => {
            let mut ok = expect(ctx, name, "uv", ins[0], Vec2) & expect(ctx, name, "scale", ins[1], Float);
            if !(min_steps.is_finite() && max_steps.is_finite() && *min_steps >= 1.0 && *max_steps >= 1.0) {
                ctx.err(name, "steps must be ≥ 1");
                ok = false;
            } else if min_steps > max_steps {
                ctx.err(name, "min_steps must not exceed max_steps");
                ok = false;
            }
            ok.then_some(Vec2)
        }
        Node::Add(..) | Node::Sub(..) | Node::Mul(..) | Node::Div(..) | Node::Min(..) | Node::Max(..) => {
            same_or_scalar(ctx, name, ins[0], ins[1])
        }
        Node::Pow(..) => same_or_scalar(ctx, name, ins[0], ins[1]),
        Node::Dot(..) => {
            if ins[0] == ins[1] && ins[0] != Float {
                Some(Float)
            } else {
                ctx.err(
                    name,
                    format!(
                        "Dot needs two vectors of one width, got {:?} and {:?}",
                        ins[0], ins[1]
                    ),
                );
                None
            }
        }
        Node::Lerp { .. } => {
            let ty = same_or_scalar(ctx, name, ins[0], ins[1])?;
            if ins[2] == Float || ins[2] == ty {
                Some(ty)
            } else {
                ctx.err(name, format!("t must be Float or {ty:?}, got {:?}", ins[2]));
                None
            }
        }
        Node::Smoothstep { .. } => {
            let edges = same_or_scalar(ctx, name, ins[0], ins[1])?;
            same_or_scalar(ctx, name, edges, ins[2])
        }
        Node::Saturate(_) | Node::OneMinus(_) | Node::Abs(_) => Some(ins[0]),
        Node::Normalize(_) | Node::Length(_) => {
            if ins[0] == Float {
                ctx.err(name, "needs a vector");
                return None;
            }
            Some(if matches!(node, Node::Length(_)) {
                Float
            } else {
                ins[0]
            })
        }
        Node::Swizzle { channels, .. } => {
            let Some(indices) = swizzle_indices(channels) else {
                ctx.err(name, format!("bad swizzle `{channels}` (1–4 of xyzw or of rgba)"));
                return None;
            };
            if indices.iter().any(|&i| i >= ins[0].width()) {
                ctx.err(name, format!("swizzle `{channels}` reads past a {:?}", ins[0]));
                return None;
            }
            ValueType::of_width(indices.len())
        }
        Node::Append(..) => {
            let width = ins[0].width() + ins[1].width();
            let ty = ValueType::of_width(width);
            if ty.is_none() {
                ctx.err(name, format!("{:?} + {:?} is wider than Vec4", ins[0], ins[1]));
            }
            ty
        }
        Node::Fresnel { .. } => expect(ctx, name, "power", ins[0], Float).then_some(Float),
        Node::HeightBlend { depth, .. } => {
            let ty = same_or_scalar(ctx, name, ins[0], ins[1])?;
            let mut ok = expect(ctx, name, "height_a", ins[2], Float)
                & expect(ctx, name, "height_b", ins[3], Float)
                & expect(ctx, name, "t", ins[4], Float);
            if !(depth.is_finite() && *depth > 0.0) {
                ctx.err(name, "depth must be > 0");
                ok = false;
            }
            ok.then_some(ty)
        }
        Node::UnpackNormal(_) => {
            if ins[0] == Vec3 || ins[0] == Vec4 {
                Some(Vec3)
            } else {
                ctx.err(name, format!("needs a Vec3 or Vec4 sample, got {:?}", ins[0]));
                None
            }
        }
        Node::NormalStrength { .. } => {
            let ok = expect(ctx, name, "normal", ins[0], Vec3) & expect(ctx, name, "strength", ins[1], Float);
            ok.then_some(Vec3)
        }
        Node::BlendNormals { .. } => {
            let ok = expect(ctx, name, "base", ins[0], Vec3) & expect(ctx, name, "detail", ins[1], Vec3);
            ok.then_some(Vec3)
        }
    }
}

fn check_outputs(ctx: &mut Ctx<'_>, slots: &[OutputSlot], types: &HashMap<String, ValueType>) {
    for out in slots {
        let Some(node) = &out.node else {
            continue;
        };
        if let Some(&got) = types.get(node.as_str())
            && got != out.ty
        {
            ctx.err(
                format!("output:{}", out.slot),
                format!(
                    "`{node}` is {got:?}; {} takes {:?} (use Swizzle)",
                    out.slot, out.ty
                ),
            );
        }
    }
}

/// Every function argument must have the declared input type.
fn check_args(ctx: &mut Ctx<'_>, lowered: &Lowered, types: &HashMap<String, ValueType>) {
    for check in &lowered.arg_checks {
        if let Some(&got) = types.get(check.arg.as_str())
            && got != check.ty
        {
            ctx.err(
                check.call.clone(),
                format!(
                    "argument `{}` is {got:?}; the function takes {:?}",
                    check.input, check.ty
                ),
            );
        }
    }
}

/// Nodes feeding `world_position_offset` run per vertex in every pass, shadows included: no
/// eye-dependent inputs (a shadow pass has another eye) and no parallax (a pixel-stage march).
fn check_vertex_stage(ctx: &mut Ctx<'_>, vertex_order: &[String]) {
    let nodes = ctx.nodes;
    for name in vertex_order {
        let Some(node) = nodes.get(name) else {
            continue;
        };
        let why = match node {
            Node::ViewDirection | Node::CameraDistance | Node::Fresnel { .. } => Some(
                "is eye-dependent; world_position_offset must match in every pass (shadows have another eye)",
            ),
            Node::Parallax { .. } => Some("is a per-pixel march; it cannot feed world_position_offset"),
            Node::Code { textures, .. } if !textures.is_empty() => Some(
                "has texture inputs; a Code node feeding world_position_offset cannot sample (no texture inputs per vertex)",
            ),
            _ => None,
        };
        if let Some(why) = why {
            ctx.err(name.clone(), format!("{} {why}", node.kind()));
        }
    }
}

/// WGSL literal of an `f32` that parses back to the same value.
/// `(sin, cos)` of `angle` from IEEE `+ - * /` only, so every platform and Rust version bakes
/// the same literal (`f32::sin` is documented as non-deterministic across platforms).
fn det_sin_cos(angle: f32) -> (f32, f32) {
    use std::f64::consts::FRAC_PI_2;
    // `%` is an exact fmod, so huge angles reduce without rounding error before the quarter split.
    let x = f64::from(angle) % std::f64::consts::TAU;
    let quarter = (x / FRAC_PI_2).round();
    let r = x - quarter * FRAC_PI_2;
    let r2 = r * r;
    let (mut sin, mut cos) = (r, 1.0_f64);
    let (mut s_term, mut c_term) = (r, 1.0_f64);
    for k in 1..=10_u32 {
        let (a, b) = (f64::from(2 * k), f64::from(2 * k + 1));
        c_term *= -r2 / ((a - 1.0) * a);
        s_term *= -r2 / (a * b);
        cos += c_term;
        sin += s_term;
    }
    // Quadrant of the reduction, from the exact integer value of `quarter`.
    let q = (quarter % 4.0 + 4.0) % 4.0;
    let (s, c) = match q as u8 {
        0 => (sin, cos),
        1 => (cos, -sin),
        2 => (-sin, -cos),
        _ => (-cos, sin),
    };
    (s as f32, c as f32)
}

fn lit(v: f32) -> String {
    let text = format!("{v:?}");
    if text.contains('.') || text.contains('e') || text.contains("inf") || text.contains("NaN") {
        text
    } else {
        format!("{text}.0")
    }
}

fn vec_lit(values: &[f32]) -> String {
    let parts: Vec<String> = values.iter().map(|v| lit(*v)).collect();
    format!("vec{}<f32>({})", values.len(), parts.join(", "))
}

/// `x` widened to `ty` when it is a scalar (for builtins that do not broadcast).
fn splat(x: &str, from: ValueType, ty: ValueType) -> String {
    if from == ty || from != ValueType::Float {
        x.to_string()
    } else {
        format!("{}({x})", ty.wgsl())
    }
}

/// The `let` statements of one stage, plus the helpers they call.
struct Emitted {
    helpers: String,
    body: String,
    var: HashMap<String, String>,
}

fn emit_nodes(ctx: &Ctx<'_>, order: &[String], types: &HashMap<String, ValueType>, stage: Stage) -> Emitted {
    let nodes = ctx.nodes;
    let var: HashMap<String, String> = order
        .iter()
        .enumerate()
        .map(|(i, n)| (n.clone(), format!("v{i}")))
        .collect();
    let v = |name: &str| var.get(name).cloned().unwrap_or_default();
    let ty = |name: &str| types.get(name).copied().unwrap_or(ValueType::Float);
    let tex = |name: &str| {
        ctx.textures
            .get(name)
            .map_or(GRAPH_TEXTURE_BINDINGS[0], |&slot| GRAPH_TEXTURE_BINDINGS[slot])
    };
    let vertex = stage == Stage::Vertex;

    let mut helpers = String::new();
    let mut body = String::new();
    let mut need_rnm = false;
    let mut need_triplanar = false;
    let mut need_noise = false;
    // Include files already emitted into this stage's module (each once).
    let mut included = std::collections::BTreeSet::new();
    for (index, name) in order.iter().enumerate() {
        let Some(node) = nodes.get(name) else {
            continue;
        };
        let out_ty = ty(name);
        let ins = node.inputs();
        let a = ins.first().map(|n| v(n)).unwrap_or_default();
        let b = ins.get(1).map(|n| v(n)).unwrap_or_default();
        let ta = ins.first().map_or(ValueType::Float, |n| ty(n));
        let tb = ins.get(1).map_or(ValueType::Float, |n| ty(n));
        let own = v(name);
        let expr = match node {
            Node::TexCoord => "in.uv".to_string(),
            Node::TexCoord1 => "in.second_uv".to_string(),
            Node::ObjectPosition => "in.object_position".to_string(),
            Node::VertexColor => "in.vertex_color".to_string(),
            Node::WorldPosition => "in.world_position".to_string(),
            Node::WorldNormal => "in.world_normal".to_string(),
            Node::ViewDirection => "in.view_dir".to_string(),
            Node::CameraDistance => "in.camera_distance".to_string(),
            // The vertex stage takes time as an input: the velocity pass evaluates the offset
            // at this frame's and last frame's time.
            Node::Time if vertex => "in.time".to_string(),
            Node::Time => "material.graph[0].x".to_string(),
            // The scene wind (m/s on world X and Z) beside the clock in slot 0, in both stages:
            // the vertex stage reads the material uniform too. One value per frame, so the
            // velocity pass's previous-frame offset uses this frame's wind.
            Node::Wind => "material.graph[0].zw".to_string(),
            Node::Param(param) => {
                let (slot, pty) = ctx
                    .params
                    .get(param.as_str())
                    .copied()
                    .unwrap_or((1, ValueType::Vec4));
                let swz = match pty {
                    ValueType::Float => ".x",
                    ValueType::Vec2 => ".xy",
                    ValueType::Vec3 => ".xyz",
                    ValueType::Vec4 => "",
                };
                format!("material.graph[{slot}]{swz}")
            }
            Node::Float(x) => lit(*x),
            Node::Vec2(x) => vec_lit(x),
            Node::Vec3(x) => vec_lit(x),
            Node::Vec4(x) => vec_lit(x),
            Node::Sample { texture, lod, .. } => match lod {
                Some(_) => format!("textureSampleLevel({}, {GRAPH_SAMPLER}, {a}, {b})", tex(texture)),
                // No derivatives per vertex: the top level.
                None if vertex => format!("textureSampleLevel({}, {GRAPH_SAMPLER}, {a}, 0.0)", tex(texture)),
                None => format!("textureSample({}, {GRAPH_SAMPLER}, {a})", tex(texture)),
            },
            Node::Triplanar {
                texture, sharpness, ..
            } => {
                need_triplanar = true;
                format!(
                    "{}({}, {GRAPH_SAMPLER}, in.world_position * {a}, in.world_normal, {})",
                    if vertex { "mgv_triplanar" } else { "mg_triplanar" },
                    tex(texture),
                    lit(*sharpness)
                )
            }
            Node::UvTransform {
                scale,
                offset,
                rotation,
                ..
            } => {
                let scaled = format!("({a} * {})", vec_lit(scale));
                if *rotation == 0.0 {
                    format!("{scaled} + {}", vec_lit(offset))
                } else {
                    let (s, c) = det_sin_cos(*rotation);
                    format!(
                        "mat2x2<f32>({}, {}, {}, {}) * {scaled} + {}",
                        lit(c),
                        lit(-s),
                        lit(s),
                        lit(c),
                        vec_lit(offset)
                    )
                }
            }
            Node::Parallax {
                texture,
                channel,
                min_steps,
                max_steps,
                ..
            } => {
                let _ = write!(
                    helpers,
                    "{}",
                    parallax_helper(index, tex(texture), channel.letter(), *min_steps, *max_steps)
                );
                format!("mg_parallax_{index}(in, {a}, {b})")
            }
            Node::Add(..) => format!("{a} + {b}"),
            Node::Sub(..) => format!("{a} - {b}"),
            Node::Mul(..) => format!("{a} * {b}"),
            // A zero divisor would write Inf or NaN; the guard keeps it finite.
            Node::Div(..) => {
                let w = tb.wgsl();
                format!("{a} / select({w}(1e-6), {b}, abs({b}) > {w}(1e-6))")
            }
            Node::Min(..) => format!("min({}, {})", splat(&a, ta, out_ty), splat(&b, tb, out_ty)),
            Node::Max(..) => format!("max({}, {})", splat(&a, ta, out_ty), splat(&b, tb, out_ty)),
            // `pow` of a negative base is undefined in WGSL; `abs` keeps every device identical,
            // and the epsilon keeps `pow(0, 0)` and `pow(0, negative)` finite.
            Node::Pow(..) => format!(
                "pow(abs({}) + {}(1e-6), {})",
                splat(&a, ta, out_ty),
                out_ty.wgsl(),
                splat(&b, tb, out_ty)
            ),
            Node::Dot(..) => format!("dot({a}, {b})"),
            Node::Lerp { t, .. } => {
                let tv = v(t);
                format!("mix({}, {}, {tv})", splat(&a, ta, out_ty), splat(&b, tb, out_ty))
            }
            Node::Smoothstep { x, .. } => {
                let (xv, tx) = (v(x), ty(x));
                // WGSL leaves `smoothstep` undefined when edge0 >= edge1, and `select` evaluates
                // both arms, so the Hermite curve is written out with a guarded divisor and the
                // reversed-edge case falls back to a step.
                let (e0, e1, xs) = (
                    splat(&a, ta, out_ty),
                    splat(&b, tb, out_ty),
                    splat(&xv, tx, out_ty),
                );
                let w = out_ty.wgsl();
                let t = format!(
                    "clamp(({xs} - {e0}) / select({w}(1.0), {e1} - {e0}, {e0} < {e1}), {w}(0.0), {w}(1.0))"
                );
                format!("select(step({e0}, {xs}), {t} * {t} * ({w}(3.0) - {w}(2.0) * {t}), {e0} < {e1})")
            }
            Node::Saturate(_) => format!("saturate({a})"),
            Node::OneMinus(_) => format!("1.0 - {a}"),
            Node::Abs(_) => format!("abs({a})"),
            Node::Normalize(_) => {
                let w = ta.wgsl();
                format!("select({w}(0.0), normalize({a}), dot({a}, {a}) > 1e-8)")
            }
            Node::Length(_) => format!("length({a})"),
            Node::Sin(_) => format!("sin({a})"),
            Node::Cos(_) => format!("cos({a})"),
            Node::Frac(_) => format!("fract({a})"),
            Node::Noise { octaves, .. } => {
                need_noise = true;
                let p = match ta {
                    ValueType::Float => format!("vec3<f32>({a}, 0.0, 0.0)"),
                    ValueType::Vec2 => format!("vec3<f32>({a}, 0.0)"),
                    _ => a.clone(),
                };
                format!(
                    "{}({p}, {}u)",
                    if vertex { "mgv_noise" } else { "mg_noise" },
                    (*octaves).clamp(1, MAX_NOISE_OCTAVES)
                )
            }
            Node::Swizzle { channels, .. } => {
                if ta == ValueType::Float {
                    // A scalar has no swizzle; widen first.
                    let idx = swizzle_indices(channels).map_or(1, |i| i.len());
                    if idx == 1 {
                        a.clone()
                    } else {
                        format!("vec{idx}<f32>({a})")
                    }
                } else {
                    format!("{a}.{channels}")
                }
            }
            Node::Append(..) => format!("{}({a}, {b})", out_ty.wgsl()),
            Node::Fresnel { .. } => {
                // A facing pixel has base 0: keep `pow` away from `pow(0, 0)` and negative powers.
                format!("pow(max(1.0 - saturate(dot(in.world_normal, in.view_dir)), 1e-4), max({a}, 0.0))")
            }
            Node::HeightBlend { t, depth, .. } => {
                let (ha, hb, tv) = (v(ins[2]), v(ins[3]), v(t));
                let _ = writeln!(body, "    let {own}_ha = {ha} + (1.0 - {tv});");
                let _ = writeln!(body, "    let {own}_hb = {hb} + {tv};");
                let _ = writeln!(
                    body,
                    "    let {own}_m = max({own}_ha, {own}_hb) - {};",
                    lit(*depth)
                );
                let _ = writeln!(body, "    let {own}_wa = max({own}_ha - {own}_m, 0.0);");
                let _ = writeln!(body, "    let {own}_wb = max({own}_hb - {own}_m, 0.0);");
                format!(
                    "({0} * {1}_wa + {2} * {1}_wb) / ({1}_wa + {1}_wb)",
                    splat(&a, ta, out_ty),
                    own,
                    splat(&b, tb, out_ty)
                )
            }
            Node::UnpackNormal(_) => format!("{a}.xyz * 2.0 - vec3<f32>(1.0)"),
            Node::NormalStrength { .. } => format!("vec3<f32>({a}.x * {b}, {a}.y * {b}, {a}.z)"),
            Node::BlendNormals { mode, .. } => match mode {
                NormalBlend::Whiteout => {
                    format!(
                        "normalize(vec3<f32>({a}.x + {b}.x, {a}.y + {b}.y, {a}.z * {b}.z) + vec3<f32>(0.0, 0.0, 1e-6))"
                    )
                }
                NormalBlend::Udn => format!(
                    "normalize(vec3<f32>({a}.x + {b}.x, {a}.y + {b}.y, {a}.z) + vec3<f32>(0.0, 0.0, 1e-6))"
                ),
                NormalBlend::Rnm => {
                    need_rnm = true;
                    format!("{}({a}, {b})", if vertex { "mgv_rnm" } else { "mg_rnm" })
                }
            },
            Node::Code {
                inputs,
                textures,
                output,
                code,
            } => {
                let (parts, _) = crate::code::parse_code(code);
                for include in &parts.includes {
                    if let Some(text) = parts.resolved.get(&include.name)
                        && included.insert(include.name.clone())
                    {
                        let _ = writeln!(
                            helpers,
                            "// include {}\n{}",
                            include.name,
                            text.trim_end_matches('\n')
                        );
                    }
                }
                let function = format!("{}_code_{index}", if vertex { "mgv" } else { "mg" });
                helpers.push_str(&crate::code::function_source(
                    &function,
                    inputs,
                    textures,
                    *output,
                    &parts.body,
                ));
                let mut args: Vec<String> = ins.iter().map(|n| v(n)).collect();
                args.extend(textures.iter().map(|t| tex(t).to_string()));
                if !textures.is_empty() {
                    args.push(GRAPH_SAMPLER.to_string());
                }
                format!("{function}({})", args.join(", "))
            }
            // Lowered away before emission.
            Node::Switch { .. } | Node::Call { .. } | Node::Input(_) => format!("{}()", out_ty.wgsl()),
        };
        let _ = writeln!(body, "    // {} `{}`", node.kind(), name);
        let _ = writeln!(body, "    let {own}: {} = {expr};", out_ty.wgsl());
    }
    let mut prelude = String::new();
    if need_triplanar {
        prelude.push_str(if vertex {
            TRIPLANAR_HELPER_VERTEX
        } else {
            TRIPLANAR_HELPER
        });
    }
    if need_rnm {
        prelude.push_str(&if vertex {
            RNM_HELPER.replace("fn mg_rnm", "fn mgv_rnm")
        } else {
            RNM_HELPER.to_string()
        });
    }
    if need_noise {
        prelude.push_str(&if vertex {
            NOISE_HELPER.replace("mg_noise", "mgv_noise")
        } else {
            NOISE_HELPER.to_string()
        });
    }
    prelude.push_str(&helpers);
    Emitted {
        helpers: prelude,
        body,
        var,
    }
}

/// `material_surface`. For a graph without v2 features the text is the v1 compiler's, byte
/// for byte (same pipeline keys).
fn emit(
    ctx: &Ctx<'_>,
    order: &[String],
    types: &HashMap<String, ValueType>,
    output_of: &dyn Fn(&str) -> Option<String>,
) -> String {
    let Emitted { helpers, body, var } = emit_nodes(ctx, order, types, Stage::Surface);
    let v = |name: &str| var.get(name).cloned().unwrap_or_default();
    let out = |slot: &str, default: &str| output_of(slot).map_or_else(|| default.to_string(), |n| v(&n));
    let base = out("base_color", "vec3<f32>(0.8)");
    let alpha = out("alpha", "1.0");
    let metallic = out("metallic", "0.0");
    let roughness = out("roughness", "0.5");
    let occlusion = format!("saturate({})", out("occlusion", "1.0"));
    let emissive = out("emissive", "vec3<f32>(0.0)");
    let normal = match (output_of("normal"), ctx.graph.output.normal_space) {
        (None, _) => "    let mg_normal = in.world_normal;\n".to_string(),
        (Some(n), NormalSpace::World) => format!(
            "    let mg_raw = {};\n    var mg_normal = in.world_normal;\n    \
             if (dot(mg_raw, mg_raw) > 1e-8) {{\n        mg_normal = normalize(mg_raw);\n    }}\n",
            v(&n)
        ),
        (Some(n), NormalSpace::Tangent) => format!(
            "    var mg_normal = in.world_normal;\n    \
             if (length(in.world_tangent.xyz) > 1e-5) {{\n        \
             // `facing` is 1 on a front face and -1 on a back face. The bitangent is built\n        \
             // from the unflipped tangent, so a back face's frame is the front frame negated.\n        \
             let mg_t_front = normalize(in.world_tangent.xyz);\n        \
             let mg_b = cross(in.world_normal, mg_t_front) * in.world_tangent.w;\n        \
             let mg_t = mg_t_front * in.facing;\n        \
             let mg_raw = mat3x3<f32>(mg_t, mg_b, in.world_normal) * {};\n        \
             if (dot(mg_raw, mg_raw) > 1e-8) {{\n            mg_normal = normalize(mg_raw);\n        }}\n    }}\n",
            v(&n)
        ),
    };

    let mut src = String::new();
    // No graph name here: the source must depend only on the structure (the cache key).
    src.push_str("// Generated by regia_matgraph. Do not edit.\n");
    src.push_str(&helpers);
    src.push_str(
        "fn material_surface(in: regia::pbr::surface::SurfaceInput) -> regia::pbr::surface::Surface {\n",
    );
    src.push_str(&body);
    src.push_str(&normal);
    let _ = writeln!(
        src,
        "    return regia::pbr::surface::Surface({base}, {alpha}, {metallic}, {roughness}, mg_normal, {occlusion}, {emissive}, 0.0);"
    );
    src.push_str("}\n");
    src
}

/// `material_vertex_offset`: the world position offset of the vertex, metres.
fn emit_vertex(ctx: &Ctx<'_>, order: &[String], types: &HashMap<String, ValueType>, root: &str) -> String {
    let Emitted { helpers, body, var } = emit_nodes(ctx, order, types, Stage::Vertex);
    let result = var
        .get(root)
        .cloned()
        .unwrap_or_else(|| "vec3<f32>(0.0)".to_string());
    let mut src = String::new();
    src.push_str("// Generated by regia_matgraph (vertex stage). Do not edit.\n");
    src.push_str(&helpers);
    src.push_str("fn material_vertex_offset(in: regia::pbr::surface::VertexOffsetInput) -> vec3<f32> {\n");
    src.push_str(&body);
    let _ = writeln!(src, "    return {result};");
    src.push_str("}\n");
    src
}

/// Triplanar projection (Geiss, "Generating Complex Procedural Terrains Using the GPU", GPU
/// Gems 3 ch. 1, 2007): three planar samples blended by `|n|^sharpness`.
const TRIPLANAR_HELPER: &str = "fn mg_triplanar(t: texture_2d<f32>, s: sampler, p: vec3<f32>, n: vec3<f32>, sharpness: f32) -> vec4<f32> {
    let w0 = pow(abs(n), vec3<f32>(sharpness));
    let w = w0 / max(w0.x + w0.y + w0.z, 1e-5);
    let x = textureSample(t, s, p.zy);
    let y = textureSample(t, s, p.xz);
    let z = textureSample(t, s, p.xy);
    return x * w.x + y * w.y + z * w.z;
}
";

/// [`TRIPLANAR_HELPER`] for the vertex stage: the top mip level (no derivatives per vertex).
const TRIPLANAR_HELPER_VERTEX: &str = "fn mgv_triplanar(t: texture_2d<f32>, s: sampler, p: vec3<f32>, n: vec3<f32>, sharpness: f32) -> vec4<f32> {
    let w0 = pow(abs(n), vec3<f32>(sharpness));
    let w = w0 / max(w0.x + w0.y + w0.z, 1e-5);
    let x = textureSampleLevel(t, s, p.zy, 0.0);
    let y = textureSampleLevel(t, s, p.xz, 0.0);
    let z = textureSampleLevel(t, s, p.xy, 0.0);
    return x * w.x + y * w.y + z * w.z;
}
";

/// Fractal gradient noise for the `Noise` node (`docs/provenance.md`): a lattice of random
/// gradients (Perlin, "Improving Noise", SIGGRAPH 2002: quintic fade, gradient dot offset per
/// cube corner, trilinear blend), with the gradient taken from an integer hash of the lattice
/// point instead of a permutation table (the `pcg3d` hash of Jarzynski and Olano, "Hash
/// Functions for GPU Rendering", JCGT 9(3), 2020), so every device picks the same gradients.
/// Octaves double the frequency and halve the amplitude; the sum is normalized and mapped to
/// 0..1.
const NOISE_HELPER: &str = "fn mg_noise_hash(c: vec3<i32>) -> vec3<f32> {
    var v = bitcast<vec3<u32>>(c) * 1664525u + vec3<u32>(1013904223u);
    v.x = v.x + v.y * v.z;
    v.y = v.y + v.z * v.x;
    v.z = v.z + v.x * v.y;
    v = v ^ (v >> vec3<u32>(16u));
    v.x = v.x + v.y * v.z;
    v.y = v.y + v.z * v.x;
    v.z = v.z + v.x * v.y;
    return vec3<f32>(v & vec3<u32>(65535u)) * (2.0 / 65535.0) - vec3<f32>(1.0);
}
fn mg_noise_corner(i: vec3<i32>, f: vec3<f32>, o: vec3<i32>) -> f32 {
    return dot(mg_noise_hash(i + o), f - vec3<f32>(o));
}
fn mg_noise_cell(p: vec3<f32>) -> f32 {
    let fl = floor(p);
    let i = vec3<i32>(fl);
    let f = p - fl;
    let u = f * f * f * (f * (f * 6.0 - 15.0) + 10.0);
    let x00 = mix(mg_noise_corner(i, f, vec3<i32>(0, 0, 0)), mg_noise_corner(i, f, vec3<i32>(1, 0, 0)), u.x);
    let x10 = mix(mg_noise_corner(i, f, vec3<i32>(0, 1, 0)), mg_noise_corner(i, f, vec3<i32>(1, 1, 0)), u.x);
    let x01 = mix(mg_noise_corner(i, f, vec3<i32>(0, 0, 1)), mg_noise_corner(i, f, vec3<i32>(1, 0, 1)), u.x);
    let x11 = mix(mg_noise_corner(i, f, vec3<i32>(0, 1, 1)), mg_noise_corner(i, f, vec3<i32>(1, 1, 1)), u.x);
    return mix(mix(x00, x10, u.y), mix(x01, x11, u.y), u.z);
}
fn mg_noise(p: vec3<f32>, octaves: u32) -> f32 {
    var sum = 0.0;
    var amp = 1.0;
    var norm = 0.0;
    var q = p;
    for (var o = 0u; o < octaves; o = o + 1u) {
        sum = sum + amp * mg_noise_cell(q);
        norm = norm + amp;
        amp = amp * 0.5;
        q = q * 2.0;
    }
    return saturate(0.5 + 0.5 * sum / max(norm, 1e-6));
}
";

/// Reoriented normal mapping (Barré-Brisebois & Hill, \"Blending in Detail\", 2012), on
/// unpacked `[-1, 1]` tangent-space normals.
const RNM_HELPER: &str = "fn mg_rnm(n1: vec3<f32>, n2: vec3<f32>) -> vec3<f32> {
    let t = n1 + vec3<f32>(0.0, 0.0, 1.0);
    let u = n2 * vec3<f32>(-1.0, -1.0, 1.0);
    return normalize(t * dot(t, u) / max(t.z, 1e-4) - u);
}
";

/// Parallax occlusion mapping specialised to one texture channel (Tatarchuk, \"Practical
/// Dynamic Parallax Occlusion Mapping\", SIGGRAPH 2005: linear search + secant refinement).
/// Same march and profile gate as the hand-written forward path, so a graph rebuild of a
/// parallax material matches it.
fn parallax_helper(index: usize, texture: &str, channel: char, min_steps: f32, max_steps: f32) -> String {
    format!(
        "fn mg_parallax_{index}(in: regia::pbr::surface::SurfaceInput, uv: vec2<f32>, scale: f32) -> vec2<f32> {{
    if (frame.pcss.z <= 0.5 || scale <= 1e-5) {{
        return uv;
    }}
    let geo = in.world_normal;
    let raw = in.world_tangent.xyz;
    if (length(raw) < 1e-5) {{
        return uv;
    }}
    let t = normalize(raw - geo * dot(geo, raw));
    if (length(t) < 1e-5) {{
        return uv;
    }}
    let b = cross(geo, t) * in.world_tangent.w;
    let view_ts = vec3<f32>(dot(in.view_dir, t), dot(in.view_dir, b), dot(in.view_dir, geo));
    let vz = max(abs(view_ts.z), 0.15);
    let facing = saturate(abs(view_ts.z));
    let max_steps = clamp({max}, 1.0, 16.0);
    let min_steps = clamp({min}, 1.0, max_steps);
    let steps_f = clamp(mix(max_steps, min_steps, facing), 1.0, 16.0);
    let steps = u32(steps_f + 0.5);
    // Divide by the rounded step count, as the hand-written path does (render6 audit 4).
    let count = f32(steps);
    let layer = 1.0 / count;
    let delta = view_ts.xy / vz * scale / count;
    var current_uv = uv;
    var current_depth = 0.0;
    var prev_uv = uv;
    var prev_depth = 0.0;
    var hit = 0.0;
    for (var i: u32 = 0u; i < 16u; i = i + 1u) {{
        if (i < steps && hit < 0.5) {{
            let h = textureSampleLevel({texture}, {GRAPH_SAMPLER}, current_uv, 0.0).{channel};
            if (h >= 1.0 - current_depth) {{
                hit = 1.0;
            }} else {{
                prev_uv = current_uv;
                prev_depth = current_depth;
                current_uv = current_uv - delta;
                current_depth = current_depth + layer;
            }}
        }}
    }}
    if (hit < 0.5) {{
        return current_uv;
    }}
    let h_after = textureSampleLevel({texture}, {GRAPH_SAMPLER}, current_uv, 0.0).{channel};
    let h_before = textureSampleLevel({texture}, {GRAPH_SAMPLER}, prev_uv, 0.0).{channel};
    let da = prev_depth - (1.0 - h_before);
    let db = current_depth - (1.0 - h_after);
    let denom = da - db;
    var r = 0.0;
    if (abs(denom) > 1e-5) {{
        r = clamp(da / denom, 0.0, 1.0);
    }}
    return mix(prev_uv, current_uv, r);
}}
",
        min = lit(min_steps),
        max = lit(max_steps),
    )
}

/// The default parameter block of a graph (slot 0 = built-ins with `time`).
#[must_use]
pub fn default_params(graph: &MaterialGraph, time: f32) -> [[f32; 4]; GRAPH_PARAM_SLOTS] {
    let mut block = [[0.0; 4]; GRAPH_PARAM_SLOTS];
    block[0][0] = time;
    for (i, p) in graph.params.iter().take(MAX_PARAMS).enumerate() {
        block[i + 1] = p.value.to_vec4();
    }
    block
}

/// The value of a parameter as declared (for instance type checks).
#[must_use]
pub fn declared(graph: &MaterialGraph, name: &str) -> Option<ParamValue> {
    graph.params.iter().find(|p| p.name == name).map(|p| p.value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn det_sin_cos_matches_std_closely_and_is_exact_at_zero() {
        assert_eq!(det_sin_cos(0.0), (0.0, 1.0));
        for k in -40..=40 {
            let angle = k as f32 * 0.37;
            let (s, c) = det_sin_cos(angle);
            assert!((s - angle.sin()).abs() < 1e-5, "sin({angle}) = {s}");
            assert!((c - angle.cos()).abs() < 1e-5, "cos({angle}) = {c}");
        }
        // A huge finite angle still yields finite, bounded values (no NaN baked into WGSL).
        let (s, c) = det_sin_cos(3.0e38);
        assert!(s.is_finite() && c.is_finite() && s.abs() <= 1.0 && c.abs() <= 1.0);
    }

    use crate::format::{ParamDecl, TextureDecl};

    fn graph(nodes: &[(&str, Node)]) -> MaterialGraph {
        let mut g = MaterialGraph::new("test");
        for (name, node) in nodes {
            g.nodes.insert((*name).to_string(), node.clone());
        }
        g
    }

    /// Column-major `mat3x3(t, b, n) * v`, the tangent frame the emitter writes.
    fn map_tangent(normal: [f32; 3], tangent: [f32; 4], ts: [f32; 3], facing: f32) -> [f32; 3] {
        let len = (tangent[0] * tangent[0] + tangent[1] * tangent[1] + tangent[2] * tangent[2]).sqrt();
        let t_front = [tangent[0] / len, tangent[1] / len, tangent[2] / len];
        let b = [
            (normal[1] * t_front[2] - normal[2] * t_front[1]) * tangent[3],
            (normal[2] * t_front[0] - normal[0] * t_front[2]) * tangent[3],
            (normal[0] * t_front[1] - normal[1] * t_front[0]) * tangent[3],
        ];
        let t = [t_front[0] * facing, t_front[1] * facing, t_front[2] * facing];
        [
            t[0] * ts[0] + b[0] * ts[1] + normal[0] * ts[2],
            t[1] * ts[0] + b[1] * ts[1] + normal[1] * ts[2],
            t[2] * ts[0] + b[2] * ts[1] + normal[2] * ts[2],
        ]
    }

    /// A tangent-space normal on a back face uses a negated tangent (`in.facing` is -1), so the
    /// frame is the front frame negated. The old frame, with the normal already flipped and the
    /// tangent left alone, mirrors the map across X.
    #[test]
    fn a_back_face_negates_the_tangent_of_a_tangent_space_normal() {
        let mut g = graph(&[("n", Node::Vec3([1.0, 0.0, 1.0]))]);
        g.output.normal = Some("n".into());
        let compiled = compile_unvalidated(&g).expect("compile");
        let wgsl = compiled.wgsl.as_ref();
        assert!(
            wgsl.contains("let mg_t_front = normalize(in.world_tangent.xyz);"),
            "{wgsl}"
        );
        assert!(
            wgsl.contains("let mg_b = cross(in.world_normal, mg_t_front) * in.world_tangent.w;"),
            "{wgsl}"
        );
        assert!(wgsl.contains("let mg_t = mg_t_front * in.facing;"), "{wgsl}");

        let len = (2.0f32).sqrt();
        let ts = [1.0 / len, 0.0, 1.0 / len];
        let tangent = [1.0, 0.0, 0.0, 1.0];
        let front = map_tangent([0.0, 0.0, 1.0], tangent, ts, 1.0);
        let back = map_tangent([0.0, 0.0, -1.0], tangent, ts, -1.0);
        for axis in 0..3 {
            assert!(
                (back[axis] + front[axis]).abs() < 1.0e-5,
                "back {back:?} is not -front {front:?}"
            );
        }
        // The formula without the facing multiply (normal already flipped) keeps +X.
        let mirrored = map_tangent([0.0, 0.0, -1.0], tangent, ts, 1.0);
        assert!((mirrored[0] - front[0]).abs() < 1.0e-5, "{mirrored:?}");
        assert!((mirrored[2] + front[2]).abs() < 1.0e-5, "{mirrored:?}");
        assert!((mirrored[0] - back[0]).abs() > 0.5);
    }

    #[test]
    fn a_graph_without_a_normal_output_does_not_read_facing() {
        let mut g = graph(&[("c", Node::Vec3([0.2, 0.3, 0.4]))]);
        g.output.base_color = Some("c".into());
        let compiled = compile_unvalidated(&g).expect("compile");
        assert!(!compiled.wgsl.contains("in.facing"));
    }

    #[test]
    fn non_finite_shading_is_rejected() {
        for edit in [
            (|g: &mut MaterialGraph| g.shading.clearcoat = f32::NAN) as fn(&mut MaterialGraph),
            |g| g.shading.anisotropy = f32::INFINITY,
            |g| g.shading.max_vertex_offset = f32::NAN,
            |g| g.shading.alpha_mode = GraphAlphaMode::Mask(f32::NAN),
        ] {
            let mut g = graph(&[("a", Node::Float(1.0))]);
            g.output.alpha = Some("a".into());
            edit(&mut g);
            let errors = compile_unvalidated(&g).expect_err("non-finite shading").0;
            assert!(
                errors.iter().any(|e| e.message.contains("must be finite")),
                "{errors:?}"
            );
        }
    }

    #[test]
    fn a_negative_vertex_offset_is_rejected() {
        let mut g = graph(&[("a", Node::Float(1.0))]);
        g.output.alpha = Some("a".into());
        g.shading.max_vertex_offset = -0.2;
        let errors = compile_unvalidated(&g).expect_err("negative offset").0;
        assert!(
            errors.iter().any(|e| e.message.contains("must not be negative")),
            "{errors:?}"
        );
    }

    #[test]
    fn pow_div_and_normalize_emit_zero_guards() {
        let mut g = graph(&[("a", Node::Float(0.0)), ("b", Node::Float(0.0))]);
        g.nodes.insert("p".into(), Node::Pow("a".into(), "b".into()));
        g.nodes.insert("d".into(), Node::Div("a".into(), "b".into()));
        g.nodes.insert("pd".into(), Node::Add("p".into(), "d".into()));
        g.output.roughness = Some("pd".into());
        let wgsl = compile_unvalidated(&g).expect("compiles").wgsl;
        assert!(wgsl.contains("+ f32(1e-6)"), "{wgsl}");
        assert!(wgsl.contains("select(f32(1e-6)"), "{wgsl}");

        let mut g = graph(&[("n", Node::WorldNormal)]);
        g.nodes.insert("u".into(), Node::Normalize("n".into()));
        g.output.base_color = Some("u".into());
        let wgsl = compile_unvalidated(&g).expect("compiles").wgsl;
        assert!(wgsl.contains("select(vec3<f32>(0.0), normalize("), "{wgsl}");
    }

    #[test]
    fn reoriented_blend_guards_a_downward_base_normal() {
        assert!(RNM_HELPER.contains("max(t.z, 1e-4)"));
        assert!(!RNM_HELPER.contains("/ t.z"));
    }

    #[test]
    fn smoothstep_never_calls_the_undefined_builtin() {
        let mut g = graph(&[
            ("x", Node::Float(0.5)),
            ("e0", Node::Float(1.0)),
            ("e1", Node::Float(0.0)),
        ]);
        g.nodes.insert(
            "s".into(),
            Node::Smoothstep {
                edge0: "e0".into(),
                edge1: "e1".into(),
                x: "x".into(),
            },
        );
        g.output.roughness = Some("s".into());
        let wgsl = compile(&g).expect("compiles").wgsl;
        assert!(!wgsl.contains("smoothstep("), "{wgsl}");
        assert!(wgsl.contains("select(step("), "{wgsl}");
        // The Hermite polynomial is still emitted for the ordered-edge branch.
        assert!(wgsl.contains("(3.0) - ") && wgsl.contains("(2.0) * "), "{wgsl}");
        assert!(wgsl.contains("< "), "the select is guarded by e0 < e1: {wgsl}");
    }

    #[test]
    fn parallax_march_is_predicated_without_continue_or_break() {
        let mut g = graph(&[
            ("uv", Node::TexCoord),
            ("depth", Node::Float(0.05)),
            (
                "pom",
                Node::Parallax {
                    texture: "h".into(),
                    channel: crate::format::Channel::R,
                    uv: "uv".into(),
                    scale: "depth".into(),
                    min_steps: 4.0,
                    max_steps: 16.0,
                },
            ),
            (
                "tex",
                Node::Sample {
                    texture: "h".into(),
                    uv: "pom".into(),
                    lod: None,
                },
            ),
        ]);
        g.textures.push(TextureDecl {
            name: "h".into(),
            path: String::new(),
            color_space: crate::format::ColorSpace::Linear,
        });
        g.nodes.insert(
            "rgb".into(),
            Node::Swizzle {
                input: "tex".into(),
                channels: "rgb".into(),
            },
        );
        g.output.base_color = Some("rgb".into());
        let wgsl = compile(&g).expect("compiles").wgsl;
        assert!(wgsl.contains("if (i < steps && hit < 0.5)"), "{wgsl}");
        assert!(!wgsl.contains("continue;"), "{wgsl}");
        assert!(!wgsl.contains("break;"), "{wgsl}");
    }

    #[test]
    fn literals_round_trip_and_always_have_a_point() {
        assert_eq!(lit(1.0), "1.0");
        assert_eq!(lit(-0.22), "-0.22");
        assert_eq!(lit(0.42), "0.42");
        for v in [0.1f32, 1.0e-5, 123_456.79, -7.25e-9] {
            assert_eq!(lit(v).parse::<f32>().expect("parse"), v);
        }
    }

    #[test]
    fn unknown_references_name_their_node() {
        let mut g = graph(&[
            ("m", Node::Mul("a".into(), "nope".into())),
            ("a", Node::Float(1.0)),
        ]);
        g.output.alpha = Some("m".into());
        g.output.roughness = Some("ghost".into());
        let errors = compile_unvalidated(&g).expect_err("should fail").0;
        assert!(
            errors
                .iter()
                .any(|e| e.node.as_deref() == Some("m") && e.message.contains("nope")),
            "{errors:?}"
        );
        assert!(
            errors
                .iter()
                .any(|e| e.node.as_deref() == Some("output:roughness")),
            "{errors:?}"
        );
    }

    #[test]
    fn cycles_are_reported() {
        let mut g = graph(&[
            ("a", Node::Add("b".into(), "c".into())),
            ("b", Node::Saturate("a".into())),
            ("c", Node::Float(1.0)),
        ]);
        g.output.alpha = Some("a".into());
        let errors = compile_unvalidated(&g).expect_err("should fail").0;
        assert!(errors.iter().any(|e| e.message.contains("cycle")), "{errors:?}");
    }

    #[test]
    fn type_errors_point_at_the_node_and_output() {
        let mut g = graph(&[
            ("uv", Node::TexCoord),
            ("c", Node::Vec3([1.0, 0.0, 0.0])),
            ("bad", Node::Add("uv".into(), "c".into())),
            ("col", Node::VertexColor),
        ]);
        g.output.base_color = Some("col".into());
        g.output.alpha = Some("bad".into());
        let errors = compile_unvalidated(&g).expect_err("should fail").0;
        assert!(
            errors.iter().any(|e| e.node.as_deref() == Some("bad")),
            "{errors:?}"
        );
        assert!(
            errors
                .iter()
                .any(|e| e.node.as_deref() == Some("output:base_color") && e.message.contains("Swizzle")),
            "{errors:?}"
        );
    }

    #[test]
    fn limits_and_names_are_checked() {
        let mut g = graph(&[("x", Node::Float(f32::NAN))]);
        g.output.alpha = Some("x".into());
        for i in 0..16 {
            g.params
                .push(ParamDecl::new(format!("p{i}"), ParamValue::Float(0.0)));
        }
        g.textures.push(TextureDecl {
            name: "bad name".into(),
            path: String::new(),
            color_space: ColorSpace::Linear,
        });
        let errors = compile_unvalidated(&g).expect_err("should fail").0;
        let text = CompileErrors(errors).to_string();
        assert!(text.contains("at most 15"), "{text}");
        assert!(text.contains("texture:bad name"), "{text}");
        assert!(text.contains("not finite"), "{text}");
    }

    #[test]
    fn unreachable_nodes_are_not_emitted_and_broadcasts_type() {
        let mut g = graph(&[
            ("unused", Node::Float(3.0)),
            ("c", Node::Vec3([0.5, 0.5, 0.5])),
            ("k", Node::Float(2.0)),
            ("scaled", Node::Mul("c".into(), "k".into())),
            ("lo", Node::Min("scaled".into(), "k".into())),
        ]);
        g.output.base_color = Some("lo".into());
        let compiled = compile_unvalidated(&g).expect("compile");
        assert!(!compiled.node_types.contains_key("unused"));
        assert_eq!(compiled.node_types["lo"], ValueType::Vec3);
        assert!(
            compiled.wgsl.contains("min(v2, vec3<f32>(v1))"),
            "{}",
            compiled.wgsl
        );
    }

    #[test]
    fn the_hash_ignores_values_but_not_structure() {
        let mut a = graph(&[("p", Node::Param("tint".into()))]);
        a.params
            .push(ParamDecl::new("tint", ParamValue::Color([1.0, 0.0, 0.0])));
        a.output.base_color = Some("p".into());
        let mut b = a.clone();
        b.params[0].value = ParamValue::Color([0.0, 1.0, 0.0]);
        b.name = "other".into();
        assert_eq!(graph_hash(&a), graph_hash(&b));
        let mut c = a.clone();
        c.output.emissive = Some("p".into());
        assert_ne!(graph_hash(&a), graph_hash(&c));
        let block = default_params(&b, 2.5);
        assert_eq!(block[0][0], 2.5);
        assert_eq!(block[1], [0.0, 1.0, 0.0, 0.0]);
    }

    #[test]
    fn a_wind_driven_vertex_offset_reads_slot_zero_zw_and_flags_time() {
        let mut g = graph(&[
            ("w", Node::Wind),
            (
                "wx",
                Node::Swizzle {
                    input: "w".into(),
                    channels: "x".into(),
                },
            ),
            (
                "wz",
                Node::Swizzle {
                    input: "w".into(),
                    channels: "y".into(),
                },
            ),
            ("zero", Node::Float(0.0)),
            ("xy", Node::Append("wx".into(), "zero".into())),
            ("dir", Node::Append("xy".into(), "wz".into())),
            ("k", Node::Float(0.02)),
            ("offset", Node::Mul("dir".into(), "k".into())),
        ]);
        g.output.world_position_offset = Some("offset".into());
        g.shading.max_vertex_offset = 0.8;
        // Validated: the vertex stage composes into every host shader (forward, depth, shadow,
        // velocity), each of which binds the material uniform.
        let compiled = compile(&g).unwrap_or_else(|e| panic!("{e}"));
        assert!(
            compiled.uses_time,
            "reading the wind makes the graph time-reading"
        );
        assert_eq!(compiled.node_types["w"], ValueType::Vec2);
        assert_eq!(compiled.node_types["dir"], ValueType::Vec3);
        let vertex = compiled.vertex_wgsl.as_deref().expect("a vertex stage");
        assert!(vertex.contains("material.graph[0].zw"), "{vertex}");
        assert!(
            !compiled.wgsl.contains("material.graph[0].zw"),
            "the surface stage does not read the wind: {}",
            compiled.wgsl
        );
    }

    #[test]
    fn a_surface_tinted_by_wind_speed_flags_time() {
        let mut g = graph(&[
            ("w", Node::Wind),
            ("speed", Node::Length("w".into())),
            ("per_speed", Node::Float(0.05)),
            ("scaled", Node::Mul("speed".into(), "per_speed".into())),
            ("t", Node::Saturate("scaled".into())),
            ("calm", Node::Vec3([0.2, 0.4, 0.1])),
            ("gusty", Node::Vec3([0.5, 0.6, 0.3])),
            (
                "base",
                Node::Lerp {
                    a: "calm".into(),
                    b: "gusty".into(),
                    t: "t".into(),
                },
            ),
        ]);
        g.output.base_color = Some("base".into());
        let compiled = compile(&g).unwrap_or_else(|e| panic!("{e}"));
        assert!(
            compiled.uses_time,
            "the renderer must write slot 0 into this graph"
        );
        assert!(compiled.vertex_wgsl.is_none());
        assert!(
            compiled.wgsl.contains("material.graph[0].zw"),
            "{}",
            compiled.wgsl
        );
    }

    #[test]
    fn a_graph_reading_neither_time_nor_wind_is_not_time_reading() {
        let mut g = graph(&[
            ("op", Node::ObjectPosition),
            ("c", Node::Vec3([0.5, 0.5, 0.5])),
            ("base", Node::Mul("op".into(), "c".into())),
        ]);
        g.output.base_color = Some("base".into());
        let compiled = compile_unvalidated(&g).expect("compile");
        assert!(!compiled.uses_time);
        // An unreachable Wind node is not emitted, so it does not count either.
        g.nodes.insert("w".into(), Node::Wind);
        assert!(!compile_unvalidated(&g).expect("compile").uses_time);
        // Time alone still flags it.
        g.nodes.insert("t".into(), Node::Time);
        g.nodes.insert("tb".into(), Node::Mul("base".into(), "t".into()));
        g.output.base_color = Some("tb".into());
        assert!(compile_unvalidated(&g).expect("compile").uses_time);
    }

    #[test]
    fn numeric_guards_are_emitted() {
        let mut g = graph(&[
            ("p", Node::Float(0.0)),
            ("f", Node::Fresnel { power: "p".into() }),
            ("n", Node::Vec3([0.0, 0.0, 1.0])),
            ("s", Node::Float(0.0)),
            (
                "ns",
                Node::NormalStrength {
                    normal: "n".into(),
                    strength: "s".into(),
                },
            ),
            (
                "w",
                Node::BlendNormals {
                    base: "ns".into(),
                    detail: "n".into(),
                    mode: NormalBlend::Whiteout,
                },
            ),
            (
                "u",
                Node::BlendNormals {
                    base: "ns".into(),
                    detail: "n".into(),
                    mode: NormalBlend::Udn,
                },
            ),
            ("o", Node::Sub("f".into(), "f".into())),
        ]);
        g.output.occlusion = Some("o".into());
        g.output.normal = Some("w".into());
        g.output.emissive = Some("u".into());
        let wgsl = compile_unvalidated(&g).expect("compile").wgsl;
        assert!(
            wgsl.contains("pow(max(1.0 - saturate(dot(in.world_normal, in.view_dir)), 1e-4), max("),
            "{wgsl}"
        );
        assert_eq!(wgsl.matches("vec3<f32>(0.0, 0.0, 1e-6))").count(), 2, "{wgsl}");
        assert!(wgsl.contains("dot(mg_raw, mg_raw) > 1e-8"), "{wgsl}");
        assert!(wgsl.contains("saturate("), "{wgsl}");
        assert!(wgsl.contains(", saturate(v"), "{wgsl}");
    }
}
