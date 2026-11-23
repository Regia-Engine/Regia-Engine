// SPDX-License-Identifier: MPL-2.0
//! Material functions in the editor (ADR-2490, `docs/specs/80-matgraph-remainder.md` § 11):
//! compiling a function on its own canvas, signatures and stale calls, collapsing a selection
//! into a function and expanding a call inline, and finding the graphs that use a function file.
//!
//! Pure data: no egui, no world (the world-level edits live in [`super`]).
//!
//! A function compiles inside a *harness*: a generated graph that borrows the declarations of
//! a calling graph (its parameters, textures, switches and other functions, when one is known),
//! feeds every input a zero constant of its type, calls the function once per output and reads
//! each output through a swizzle into one float. Errors inside the expansion
//! (`regia_out_0/<node>`) map back to the function's own nodes.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use regia_matgraph::{FunctionInput, FunctionSource, MaterialFunction, MaterialGraph, Node, ValueType};

use super::model::{Diagnostic, Diagnostics, DocMode, MatGraphDoc, Target, valid_node_name};

/// Prefix of the harness's call nodes (`regia_out_<i>`); errors under it belong to the function.
const OUT: &str = HARNESS_OUTPUT_PREFIX;

/// The name prefix of a [`harness`]'s call nodes, one per wired output in output order
/// (`regia_out_0`, …).
pub const HARNESS_OUTPUT_PREFIX: &str = "regia_out_";

/// A function's signature: what its `Call` nodes' pins follow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    /// Inputs in order.
    pub inputs: Vec<FunctionInput>,
    /// Output names in order.
    pub outputs: Vec<String>,
}

impl Signature {
    /// The signature of `function`.
    #[must_use]
    pub fn of(function: &MaterialFunction) -> Self {
        Self {
            inputs: function.inputs.clone(),
            outputs: function.outputs.keys().cloned().collect(),
        }
    }
}

fn zero_of(ty: ValueType) -> Node {
    match ty {
        ValueType::Float => Node::Float(0.0),
        ValueType::Vec2 => Node::Vec2([0.0; 2]),
        ValueType::Vec3 => Node::Vec3([0.0; 3]),
        ValueType::Vec4 => Node::Vec4([0.0; 4]),
    }
}

/// The harness graph that compiles `function` (named `name`) with the declarations of
/// `context` (a calling graph with its function files resolved), or with none.
#[must_use]
pub fn harness(function: &MaterialFunction, name: &str, context: Option<&MaterialGraph>) -> MaterialGraph {
    let mut graph = MaterialGraph::new("function harness");
    if let Some(context) = context {
        graph.params.clone_from(&context.params);
        graph.textures.clone_from(&context.textures);
        graph.switches.clone_from(&context.switches);
        graph.functions.clone_from(&context.functions);
    }
    graph
        .functions
        .insert(name.to_string(), FunctionSource::Inline(function.clone()));
    let args: BTreeMap<String, String> = function
        .inputs
        .iter()
        .enumerate()
        .map(|(i, input)| (input.name.clone(), format!("regia_arg_{i}")))
        .collect();
    for (i, input) in function.inputs.iter().enumerate() {
        graph.nodes.insert(format!("regia_arg_{i}"), zero_of(input.ty));
    }
    let mut sum: Option<String> = None;
    for (i, (output, node)) in function.outputs.iter().enumerate() {
        // An unwired or dangling output is reported on the output, not through the harness.
        if node.is_empty() || !function.nodes.contains_key(node) {
            continue;
        }
        let call = format!("{OUT}{i}");
        graph.nodes.insert(
            call.clone(),
            Node::Call {
                function: name.to_string(),
                args: args.clone(),
                output: Some(output.clone()),
            },
        );
        let x = format!("regia_x_{i}");
        graph.nodes.insert(
            x.clone(),
            Node::Swizzle {
                input: call,
                channels: "x".into(),
            },
        );
        sum = Some(match sum {
            None => x,
            Some(prev) => {
                let s = format!("regia_sum_{i}");
                graph.nodes.insert(s.clone(), Node::Add(prev, x));
                s
            }
        });
    }
    graph.output.roughness = sum;
    graph
}

/// Compiles the function a [`MatGraphDoc`] holds (function mode) inside its [`harness`] and maps
/// every error to the function's canvas: its nodes, its outputs, or the whole function.
/// `context` is a calling graph (function files resolved) whose declarations the function
/// reads; without one, `Param` and texture nodes have nothing to name.
#[must_use]
pub fn diagnose_function(doc: &MatGraphDoc, context: Option<&MaterialGraph>) -> Diagnostics {
    let Some(meta) = doc.function_meta() else {
        return Diagnostics::default();
    };
    let function = meta.to_function(&doc.graph.nodes);
    let name = if valid_node_name(&meta.name) {
        meta.name.clone()
    } else {
        "function".to_string()
    };
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    for (output, node) in &function.outputs {
        let message = if node.is_empty() {
            Some("the output is not connected".to_string())
        } else if !function.nodes.contains_key(node) {
            Some(format!("unknown node `{node}`"))
        } else {
            None
        };
        if let Some(message) = message {
            errors.push(Diagnostic {
                target: Target::Output(output.clone()),
                source: Some(format!("output:{output}")),
                message,
            });
        }
    }
    if function.outputs.is_empty() {
        errors.push(Diagnostic {
            target: Target::Graph,
            source: None,
            message: "the function has no outputs: add one to its signature".into(),
        });
    }
    let mut seen = BTreeSet::new();
    for input in &function.inputs {
        if !seen.insert(&input.name) {
            errors.push(Diagnostic {
                target: Target::Graph,
                source: None,
                message: format!("input `{}` is declared twice", input.name),
            });
        }
    }
    let reads_declarations = function.nodes.values().any(|n| {
        matches!(n, Node::Param(_) | Node::Switch { .. } | Node::Call { .. }) || n.texture().is_some()
    });
    if context.is_none() && reads_declarations {
        warnings.push(
            "no calling graph: parameters, textures, switches and other functions are checked against none (open the function from a graph that calls it, or pass `context`)".to_string(),
        );
    }
    let graph = harness(&function, &name, context);
    let mut key = None;
    let mut graph_hash = None;
    match regia_matgraph::compile_unvalidated(&graph) {
        Ok(compiled) => {
            key = Some(compiled.key);
            graph_hash = Some(compiled.graph_hash);
        }
        Err(found) => {
            let mut dedupe = BTreeSet::new();
            for e in found.0 {
                let (target, source) = map_harness_error(e.node.as_deref(), &name);
                if dedupe.insert((target.clone(), e.message.clone())) {
                    errors.push(Diagnostic {
                        target,
                        source,
                        message: e.message,
                    });
                }
            }
        }
    }
    Diagnostics {
        ok: errors.is_empty(),
        errors,
        key,
        graph_hash,
        warnings,
        ..Diagnostics::default()
    }
}

/// Maps a [`harness`] error target to the function's canvas: `regia_out_0/<node>/…` → the node,
/// `function:<name>/<node>` → the node, the calling graph's declarations → the whole function.
pub fn map_harness_error(node: Option<&str>, name: &str) -> (Target, Option<String>) {
    let Some(node) = node else {
        return (Target::Graph, None);
    };
    if let Some(rest) = node.strip_prefix(OUT)
        && let Some((_, inner)) = rest.split_once('/')
    {
        return (Target::of(Some(inner)), Some(inner.to_string()));
    }
    if let Some(inner) = node
        .strip_prefix("function:")
        .and_then(|r| r.strip_prefix(name))
        .and_then(|r| r.strip_prefix('/'))
    {
        return (Target::Node(inner.to_string()), Some(inner.to_string()));
    }
    (Target::Graph, Some(node.to_string()))
}

/// How a `Call` node no longer fits its function's signature.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StaleCall {
    /// The `Call` node.
    pub node: String,
    /// The function it calls.
    pub function: String,
    /// Arguments the function has no input for.
    pub unknown_args: Vec<String>,
    /// Inputs the call passes nothing to.
    pub missing_args: Vec<String>,
    /// The output it names that the function does not have (`""`: it names none and the
    /// function has more than one).
    pub bad_output: Option<String>,
}

impl StaleCall {
    /// One line for the canvas and the agent.
    #[must_use]
    pub fn message(&self) -> String {
        let mut parts = Vec::new();
        if !self.unknown_args.is_empty() {
            parts.push(format!("no input {}", quote_list(&self.unknown_args)));
        }
        if !self.missing_args.is_empty() {
            parts.push(format!("unwired {}", quote_list(&self.missing_args)));
        }
        match self.bad_output.as_deref() {
            Some("") => parts.push("names no output and the function has several".into()),
            Some(o) => parts.push(format!("no output `{o}`")),
            None => {}
        }
        format!(
            "stale signature: `{}` changed ({}); use Fix signature",
            self.function,
            parts.join("; ")
        )
    }
}

fn quote_list(names: &[String]) -> String {
    names
        .iter()
        .map(|n| format!("`{n}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The signature of every function of `graph` that can be read: inline ones from the graph,
/// file ones through `read` (a function whose file cannot be read is left out).
pub fn signatures(
    graph: &MaterialGraph,
    read: &mut dyn FnMut(&str) -> Option<MaterialFunction>,
) -> BTreeMap<String, Signature> {
    graph
        .functions
        .iter()
        .filter_map(|(name, source)| {
            let function = match source {
                FunctionSource::Inline(f) => Some(f.clone()),
                FunctionSource::File(path) => read(path),
            }?;
            Some((name.clone(), Signature::of(&function)))
        })
        .collect()
}

/// The `Call` nodes of `graph` that do not fit their function's signature in `sigs`, by node name.
#[must_use]
pub fn stale_calls(graph: &MaterialGraph, sigs: &BTreeMap<String, Signature>) -> Vec<StaleCall> {
    graph
        .nodes
        .iter()
        .filter_map(|(name, node)| {
            let Node::Call {
                function,
                args,
                output,
            } = node
            else {
                return None;
            };
            let sig = sigs.get(function)?;
            stale_call(name, function, args, output.as_deref(), sig)
        })
        .collect()
}

fn stale_call(
    name: &str,
    function: &str,
    args: &BTreeMap<String, String>,
    output: Option<&str>,
    sig: &Signature,
) -> Option<StaleCall> {
    let unknown_args: Vec<String> = args
        .keys()
        .filter(|a| !sig.inputs.iter().any(|i| &i.name == *a))
        .cloned()
        .collect();
    let missing_args: Vec<String> = sig
        .inputs
        .iter()
        .filter(|i| args.get(&i.name).is_none_or(String::is_empty))
        .map(|i| i.name.clone())
        .collect();
    let bad_output = match output {
        Some(o) if !sig.outputs.iter().any(|x| x == o) => Some(o.to_string()),
        None if sig.outputs.len() != 1 => Some(String::new()),
        _ => None,
    };
    (!unknown_args.is_empty() || !missing_args.is_empty() || bad_output.is_some()).then(|| StaleCall {
        node: name.to_string(),
        function: function.to_string(),
        unknown_args,
        missing_args,
        bad_output,
    })
}

/// What [`fix_call`] changed on one node.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CallFix {
    /// The `Call` node.
    pub node: String,
    /// Arguments moved to a renamed input: (old argument, new input).
    pub rewired: Vec<(String, String)>,
    /// Arguments dropped (the function has no such input any more).
    pub dropped: Vec<String>,
    /// Inputs still unwired after the fix (nothing to wire them to).
    pub unwired: Vec<String>,
    /// The output the call now names (`None`: the function's only output).
    pub output: Option<Option<String>>,
}

/// Makes a `Call` node fit `sig`: arguments of inputs that still exist stay; when as many
/// arguments lost their input as inputs lost their argument, they pair up in order (old
/// arguments by name, new inputs in signature order: a rename keeps its wire); otherwise the
/// stale arguments are dropped. A stale output becomes the only output (`None`), or the first
/// output when there are several. `None` when `node` is not a `Call` or already fits.
pub fn fix_call(name: &str, node: &mut Node, sig: &Signature) -> Option<CallFix> {
    let Node::Call {
        function,
        args,
        output,
    } = node
    else {
        return None;
    };
    let stale = stale_call(name, function, args, output.as_deref(), sig)?;
    let mut fix = CallFix {
        node: name.to_string(),
        ..CallFix::default()
    };
    let missing: Vec<String> = sig
        .inputs
        .iter()
        .filter(|i| !args.contains_key(&i.name))
        .map(|i| i.name.clone())
        .collect();
    if !stale.unknown_args.is_empty() && stale.unknown_args.len() == missing.len() {
        for (old, new) in stale.unknown_args.iter().zip(&missing) {
            if let Some(wire) = args.remove(old) {
                args.insert(new.clone(), wire);
                fix.rewired.push((old.clone(), new.clone()));
            }
        }
    } else {
        for old in &stale.unknown_args {
            args.remove(old);
            fix.dropped.push(old.clone());
        }
    }
    fix.unwired = sig
        .inputs
        .iter()
        .filter(|i| args.get(&i.name).is_none_or(String::is_empty))
        .map(|i| i.name.clone())
        .collect();
    if stale.bad_output.is_some() {
        let next = if sig.outputs.len() == 1 {
            None
        } else {
            sig.outputs.first().cloned()
        };
        output.clone_from(&next);
        fix.output = Some(next);
    }
    Some(fix)
}

/// What [`collapse`] made.
#[derive(Clone, Debug, PartialEq)]
pub struct Collapsed {
    /// The new function.
    pub function: MaterialFunction,
    /// The `Call` nodes that replace the selection (one per output, each named after the node
    /// it stands for, so every reader keeps its wire).
    pub calls: Vec<String>,
}

/// The value type of each of `names` in `graph` (function files resolved), from a probe compile
/// that reads only them. `None` for a name whose type cannot be inferred (its inputs do not
/// compile).
fn probe_types(graph: &MaterialGraph, names: &[String]) -> Result<BTreeMap<String, ValueType>, String> {
    let mut probe = graph.clone();
    probe.output = regia_matgraph::SurfaceOutputs::default();
    let prefix = (0..)
        .map(|n| format!("regia_probe{n}_"))
        .find(|p| !graph.nodes.keys().any(|k| k.starts_with(p.as_str())))
        .unwrap_or_default();
    let mut sum: Option<String> = None;
    for (i, name) in names.iter().enumerate() {
        let t = format!("{prefix}t{i}");
        let x = format!("{prefix}x{i}");
        probe
            .nodes
            .insert(t.clone(), Node::Add(name.clone(), name.clone()));
        probe.nodes.insert(
            x.clone(),
            Node::Swizzle {
                input: t,
                channels: "x".into(),
            },
        );
        sum = Some(match sum {
            None => x,
            Some(prev) => {
                let s = format!("{prefix}s{i}");
                probe.nodes.insert(s.clone(), Node::Add(prev, x));
                s
            }
        });
    }
    probe.output.roughness = sum;
    let compiled = regia_matgraph::compile_unvalidated(&probe)
        .map_err(|e| format!("the selection's inputs must compile to find their types: {e}"))?;
    names
        .iter()
        .enumerate()
        .map(|(i, name)| {
            compiled
                .node_types
                .get(&format!("{prefix}t{i}"))
                .map(|ty| (name.clone(), *ty))
                .ok_or_else(|| format!("cannot infer the type of `{name}`"))
        })
        .collect()
}

/// Collapses the nodes `selection` of a graph document into material function `name`. The
/// function's inputs are the nodes outside the selection that it reads (each input, and the
/// `Input` node that reads it, is named after that node, so no wire inside changes); its outputs
/// are the selected nodes read from outside (by a node or a surface output). Each output becomes
/// a `Call` node with the output node's name and canvas position, so every reader keeps its wire.
/// `resolved` is the document's graph with its function files read (the input types come from a
/// compile of it). The caller stores the function (inline, or as a file).
///
/// The compiled code is the code before (only node names in comments differ), with one output or
/// several: calls of one function with the same arguments share one expansion in the compiler,
/// so nodes the outputs share are emitted once (ADR-7730).
///
/// # Errors
///
/// `matgraph_collapse_invalid` reasons: not a graph document, an empty selection, a missing
/// node, a bad or taken function name, a selection nothing outside reads, or inputs that do not
/// compile.
pub fn collapse(
    doc: &MatGraphDoc,
    resolved: &MaterialGraph,
    selection: &BTreeSet<String>,
    name: &str,
) -> Result<Collapsed, String> {
    if !matches!(doc.mode, DocMode::Graph) {
        return Err(
            "collapse works in a material graph (a function or an instance cannot hold new functions)".into(),
        );
    }
    if selection.is_empty() {
        return Err("select the nodes to collapse".into());
    }
    if !valid_node_name(name) {
        return Err(format!(
            "bad function name `{name}` (1–64 characters of A-Z, a-z, 0-9, _)"
        ));
    }
    if doc.graph.functions.contains_key(name) {
        return Err(format!("the graph already has a function `{name}`"));
    }
    let nodes = &doc.graph.nodes;
    for n in selection {
        match nodes.get(n) {
            None => return Err(format!("no node `{n}`")),
            Some(Node::Input(_)) => {
                return Err(format!("`{n}` is an Input node (only valid inside a function)"));
            }
            Some(_) => {}
        }
    }
    let mut externals = BTreeSet::new();
    for n in selection {
        for input in nodes[n].inputs() {
            if !input.is_empty() && !selection.contains(input) && nodes.contains_key(input) {
                externals.insert(input.to_string());
            }
        }
    }
    let mut outputs = BTreeSet::new();
    for (other, node) in nodes {
        if selection.contains(other) {
            continue;
        }
        for input in node.inputs() {
            if selection.contains(input) {
                outputs.insert(input.to_string());
            }
        }
    }
    for slot in regia_matgraph::SurfaceOutputs::SLOTS {
        if let Some(n) = doc.graph.output.get(slot)
            && selection.contains(n)
        {
            outputs.insert(n.to_string());
        }
    }
    if outputs.is_empty() {
        return Err("nothing outside the selection reads it: there is no output to call".into());
    }
    let externals: Vec<String> = externals.into_iter().collect();
    let types = probe_types(resolved, &externals)?;
    let mut fnodes: BTreeMap<String, Node> =
        selection.iter().map(|n| (n.clone(), nodes[n].clone())).collect();
    for e in &externals {
        fnodes.insert(e.clone(), Node::Input(e.clone()));
    }
    let function = MaterialFunction {
        format_version: regia_matgraph::MATFUNC_FORMAT_VERSION,
        name: name.to_string(),
        inputs: externals
            .iter()
            .map(|e| FunctionInput {
                name: e.clone(),
                ty: types[e],
            })
            .collect(),
        nodes: fnodes,
        outputs: outputs.iter().map(|o| (o.clone(), o.clone())).collect(),
    };
    Ok(Collapsed {
        function,
        calls: outputs.into_iter().collect(),
    })
}

/// Applies a [`collapse`] to the document: the selection goes, a `Call` per output comes (named
/// and placed like the node it stands for), and the graph gains function `name` from `source`.
pub fn apply_collapse(
    doc: &mut MatGraphDoc,
    selection: &BTreeSet<String>,
    collapsed: &Collapsed,
    source: FunctionSource,
) {
    let name = collapsed.function.name.clone();
    let args: BTreeMap<String, String> = collapsed
        .function
        .inputs
        .iter()
        .map(|i| (i.name.clone(), i.name.clone()))
        .collect();
    let single = collapsed.calls.len() == 1;
    let positions = doc.positions();
    for n in selection {
        doc.graph.nodes.remove(n);
        doc.layout.remove(n);
    }
    for call in &collapsed.calls {
        doc.graph.nodes.insert(
            call.clone(),
            Node::Call {
                function: name.clone(),
                args: args.clone(),
                output: (!single).then(|| call.clone()),
            },
        );
        if let Some(pos) = positions.get(call) {
            doc.layout.insert(call.clone(), *pos);
        }
    }
    doc.graph.functions.insert(name, source);
}

/// What [`expand`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Expanded {
    /// Function node name → graph node name, for the nodes inserted.
    pub nodes: BTreeMap<String, String>,
    /// The node that now stands where the call stood (the call's own name, unless the output
    /// is an input passed straight through: then the argument node).
    pub result: String,
}

/// Expands `Call` node `call` of a graph document inline with `function` (its definition, file
/// already read): the function nodes the chosen output reads are inserted (named as in the
/// function when free, else `<call>_<name>`, else a fresh name), `Input` nodes become the call's
/// arguments, and the output node takes the call's name and place, so readers keep their wires.
/// The function stays declared (other calls may use it).
///
/// # Errors
///
/// `matgraph_expand_invalid` reasons: not a graph document, no such `Call` node, or an output
/// the function does not have.
pub fn expand(doc: &mut MatGraphDoc, call: &str, function: &MaterialFunction) -> Result<Expanded, String> {
    if !matches!(doc.mode, DocMode::Graph) {
        return Err("expand works in a material graph".into());
    }
    let Some(Node::Call { args, output, .. }) = doc.graph.nodes.get(call).cloned() else {
        return Err(format!("`{call}` is not a Call node"));
    };
    let output = match output {
        Some(o) => o,
        None if function.outputs.len() == 1 => function.outputs.keys().next().cloned().unwrap_or_default(),
        None => return Err(format!("`{call}` names no output and the function has several")),
    };
    let Some(root) = function.outputs.get(&output).cloned() else {
        return Err(format!("the function has no output `{output}`"));
    };
    if !function.nodes.contains_key(&root) {
        return Err(format!(
            "output `{output}` reads `{root}`, which is not a node of the function"
        ));
    }
    // The function nodes the output reads.
    let mut reach = BTreeSet::new();
    let mut stack = vec![root.clone()];
    while let Some(n) = stack.pop() {
        if !reach.insert(n.clone()) {
            continue;
        }
        if let Some(node) = function.nodes.get(&n) {
            stack.extend(
                node.inputs()
                    .into_iter()
                    .filter(|i| function.nodes.contains_key(*i))
                    .map(str::to_string),
            );
        }
    }
    // Names: inputs map to arguments, the root takes the call's name, the rest stay when free.
    let mut map: BTreeMap<String, String> = BTreeMap::new();
    let mut taken: BTreeSet<String> = doc.graph.nodes.keys().cloned().collect();
    for n in &reach {
        if let Some(Node::Input(input)) = function.nodes.get(n) {
            map.insert(n.clone(), args.get(input).cloned().unwrap_or_default());
        }
    }
    let root_is_input = matches!(function.nodes.get(&root), Some(Node::Input(_)));
    if !root_is_input {
        map.insert(root.clone(), call.to_string());
    }
    for n in &reach {
        if map.contains_key(n) {
            continue;
        }
        let kind = function.nodes.get(n).map_or("node", Node::kind);
        let prefixed = format!("{call}_{n}");
        let pick = if !taken.contains(n) {
            n.clone()
        } else if valid_node_name(&prefixed) && !taken.contains(&prefixed) {
            prefixed
        } else {
            let base = doc.fresh_name(kind);
            (1..)
                .map(|k| format!("{base}{k}"))
                .find(|c| !taken.contains(c) && valid_node_name(c))
                .unwrap_or(base)
        };
        taken.insert(pick.clone());
        map.insert(n.clone(), pick);
    }
    let pos = doc.positions().get(call).copied();
    doc.graph.nodes.remove(call);
    let mut inserted = BTreeMap::new();
    for n in &reach {
        let Some(node) = function.nodes.get(n) else {
            continue;
        };
        if matches!(node, Node::Input(_)) {
            continue;
        }
        let mut node = node.clone();
        node.map_inputs(|input| {
            if let Some(to) = map.get(input.as_str()) {
                input.clone_from(to);
            }
        });
        let to = map[n].clone();
        doc.graph.nodes.insert(to.clone(), node);
        inserted.insert(n.clone(), to);
    }
    let result = if root_is_input {
        // A pass-through: readers of the call read the argument instead.
        let arg = map.get(&root).cloned().unwrap_or_default();
        for node in doc.graph.nodes.values_mut() {
            node.map_inputs(|input| {
                if input == call {
                    input.clone_from(&arg);
                }
            });
        }
        doc.graph
            .output
            .rename(call, (!arg.is_empty()).then_some(arg.as_str()));
        doc.layout.remove(call);
        arg
    } else {
        if let Some(pos) = pos {
            doc.layout.insert(call.to_string(), pos);
        }
        call.to_string()
    };
    Ok(Expanded {
        nodes: inserted,
        result,
    })
}

/// One graph that uses a function file.
#[derive(Clone, Debug, PartialEq)]
pub struct FunctionUser {
    /// The `.matgraph.ron`.
    pub path: PathBuf,
    /// The name the graph gives the function.
    pub function: String,
    /// Its `Call` nodes of the function.
    pub calls: Vec<String>,
    /// The calls that do not fit the signature.
    pub stale: Vec<StaleCall>,
}

/// Most directory entries [`function_users`] visits (a guard against a huge tree).
pub const USERS_SCAN_LIMIT: usize = 50_000;

/// Every `.matgraph.ron` under `root` (skipping hidden directories and `target`) that names the
/// function file `function_path` (compared after resolving each graph's reference the way the
/// editor resolves it), with its calls checked against `sig`. Sorted by path; unreadable or
/// unparsable graphs are skipped.
#[must_use]
pub fn function_users(
    root: &Path,
    project: Option<&Path>,
    function_path: &Path,
    sig: &Signature,
) -> Vec<FunctionUser> {
    let target = std::fs::canonicalize(function_path).unwrap_or_else(|_| function_path.to_path_buf());
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    let mut visited = 0usize;
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            visited += 1;
            if visited > USERS_SCAN_LIMIT {
                break;
            }
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if path.is_dir() {
                if !name.starts_with('.') && name != "target" {
                    stack.push(path);
                }
            } else if name.ends_with(".matgraph.ron") {
                files.push(path);
            }
        }
    }
    files.sort();
    let mut users = Vec::new();
    for path in files {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(graph) = MaterialGraph::from_ron(&text) else {
            continue;
        };
        let dir = path.parent();
        for (fname, source) in &graph.functions {
            let FunctionSource::File(rel) = source else {
                continue;
            };
            let Ok(full) = super::model::graph_file_path(dir, project, rel) else {
                continue;
            };
            let full = std::fs::canonicalize(&full).unwrap_or(full);
            if full != target {
                continue;
            }
            let sigs = BTreeMap::from([(fname.clone(), sig.clone())]);
            let calls = graph
                .nodes
                .iter()
                .filter(|(_, n)| matches!(n, Node::Call { function, .. } if function == fname))
                .map(|(n, _)| n.clone())
                .collect();
            users.push(FunctionUser {
                path: path.clone(),
                function: fname.clone(),
                calls,
                stale: stale_calls(&graph, &sigs),
            });
        }
    }
    users
}

/// Normalizes generated WGSL for comparing two compiles of one graph: drops the `//` comment
/// lines (they name nodes, which a collapse or an expand renames) and trailing spaces.
#[must_use]
pub fn normalized_wgsl(wgsl: &str) -> String {
    wgsl.lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// tint * (vcol.rgb), then a lerp to a constant by uv.x: a chain to collapse.
    fn graph() -> MaterialGraph {
        MaterialGraph::from_ron(
            r#"(
                format_version: 4,
                params: [(name: "tint", value: Color((0.8, 0.2, 0.1)))],
                nodes: {
                    "tint": Param("tint"),
                    "vcol": VertexColor,
                    "rgb": Swizzle(input: "vcol", channels: "rgb"),
                    "tinted": Mul("tint", "rgb"),
                    "uv": TexCoord,
                    "u": Swizzle(input: "uv", channels: "x"),
                    "white": Vec3((1.0, 1.0, 1.0)),
                    "base": Lerp(a: "tinted", b: "white", t: "u"),
                    "half": Float(0.5),
                    "rough": Mul("u", "half"),
                },
                output: (base_color: Some("base"), roughness: Some("rough")),
            )"#,
        )
        .expect("fixture")
    }

    fn wgsl(graph: &MaterialGraph) -> String {
        normalized_wgsl(&regia_matgraph::compile_unvalidated(graph).expect("compiles").wgsl)
    }

    fn sel(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn collapse_one_output_keeps_the_code_and_the_wires() {
        let g = graph();
        let before = wgsl(&g);
        let mut doc = MatGraphDoc::new(g.clone(), BTreeMap::from([("base".to_string(), [3.0, 4.0])]));
        let selection = sel(&["rgb", "tinted", "white", "base"]);
        let made = collapse(&doc, &g, &selection, "tint_blend").expect("collapse");
        assert_eq!(made.calls, ["base"]);
        let inputs: Vec<(&str, ValueType)> = made
            .function
            .inputs
            .iter()
            .map(|i| (i.name.as_str(), i.ty))
            .collect();
        assert_eq!(
            inputs,
            [
                ("tint", ValueType::Vec3),
                ("u", ValueType::Float),
                ("vcol", ValueType::Vec4)
            ]
        );
        assert_eq!(
            made.function.outputs,
            BTreeMap::from([("base".into(), "base".into())])
        );
        apply_collapse(
            &mut doc,
            &selection,
            &made,
            FunctionSource::Inline(made.function.clone()),
        );
        assert!(matches!(
            &doc.graph.nodes["base"],
            Node::Call { output: None, .. }
        ));
        assert_eq!(
            doc.layout["base"],
            [3.0, 4.0],
            "the call takes the output's place"
        );
        assert!(!doc.graph.nodes.contains_key("rgb"));
        assert_eq!(wgsl(&doc.graph), before, "same code after the collapse");
        // And back: expand restores the code.
        let f = made.function.clone();
        let out = expand(&mut doc, "base", &f).expect("expand");
        assert_eq!(out.result, "base");
        assert_eq!(wgsl(&doc.graph), before, "same code after the expand");
        assert!(matches!(doc.graph.nodes["base"], Node::Lerp { .. }));
    }

    #[test]
    fn collapse_refusals_name_the_reason() {
        let g = graph();
        let doc = MatGraphDoc::new(g.clone(), BTreeMap::new());
        assert!(collapse(&doc, &g, &BTreeSet::new(), "f").is_err());
        assert!(collapse(&doc, &g, &sel(&["nope"]), "f").is_err());
        assert!(collapse(&doc, &g, &sel(&["rgb"]), "bad name").is_err());
        // Everything selected: nothing outside reads... the outputs still do.
        let all: BTreeSet<String> = g.nodes.keys().cloned().collect();
        let made = collapse(&doc, &g, &all, "whole").expect("outputs read it");
        assert_eq!(made.calls, ["base", "rough"]);
        // A node nothing reads.
        let mut lone = g.clone();
        lone.nodes.insert("lone".into(), Node::Time);
        let d = MatGraphDoc::new(lone.clone(), BTreeMap::new());
        assert!(collapse(&d, &lone, &sel(&["lone"]), "f").is_err());
    }

    #[test]
    fn several_outputs_call_once_per_output() {
        let g = graph();
        let mut doc = MatGraphDoc::new(g.clone(), BTreeMap::new());
        let selection = sel(&["uv", "u"]);
        let made = collapse(&doc, &g, &selection, "coord").expect("collapse");
        // `u` is read by `base` and `rough`; `uv` only inside.
        assert_eq!(made.calls, ["u"]);
        apply_collapse(
            &mut doc,
            &selection,
            &made,
            FunctionSource::Inline(made.function.clone()),
        );
        assert_eq!(wgsl(&doc.graph), wgsl(&g));
        // Two outputs: tinted (read by base) and u (read by base and rough).
        let mut doc = MatGraphDoc::new(g.clone(), BTreeMap::new());
        let selection = sel(&["uv", "u", "rgb", "tinted"]);
        let made = collapse(&doc, &g, &selection, "two").expect("collapse");
        assert_eq!(made.calls, ["tinted", "u"]);
        apply_collapse(
            &mut doc,
            &selection,
            &made,
            FunctionSource::Inline(made.function.clone()),
        );
        assert!(matches!(&doc.graph.nodes["u"], Node::Call { output: Some(o), .. } if o == "u"));
        assert!(regia_matgraph::compile_unvalidated(&doc.graph).is_ok());
    }

    #[test]
    fn stale_calls_are_found_and_fixed() {
        let mut g = graph();
        g.functions.insert(
            "f".into(),
            FunctionSource::Inline(MaterialFunction {
                format_version: 3,
                name: "f".into(),
                inputs: vec![FunctionInput {
                    name: "coord".into(),
                    ty: ValueType::Float,
                }],
                nodes: BTreeMap::from([("c".into(), Node::Input("coord".into()))]),
                outputs: BTreeMap::from([("o".into(), "c".into())]),
            }),
        );
        g.nodes.insert(
            "call".into(),
            Node::Call {
                function: "f".into(),
                args: BTreeMap::from([("x".into(), "u".into())]),
                output: Some("old".into()),
            },
        );
        let sigs = signatures(&g, &mut |_| None);
        let stale = stale_calls(&g, &sigs);
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].unknown_args, ["x"]);
        assert_eq!(stale[0].missing_args, ["coord"]);
        assert_eq!(stale[0].bad_output.as_deref(), Some("old"));
        assert!(stale[0].message().contains("stale signature"));
        let mut node = g.nodes["call"].clone();
        let fix = fix_call("call", &mut node, &sigs["f"]).expect("fixes");
        assert_eq!(fix.rewired, [("x".to_string(), "coord".to_string())]);
        assert_eq!(fix.output, Some(None));
        assert!(fix.unwired.is_empty());
        g.nodes.insert("call".into(), node.clone());
        assert!(stale_calls(&g, &sigs).is_empty());
        assert!(fix_call("call", &mut node, &sigs["f"]).is_none(), "fits now");
    }

    #[test]
    fn a_function_compiles_in_its_harness_and_errors_map_to_its_nodes() {
        let function = MaterialFunction::from_ron(
            r#"(
                format_version: 3,
                inputs: [(name: "c", ty: Vec3)],
                nodes: { "c": Input("c"), "t": Param("tint"), "o": Mul("c", "t"), "bad": Normalize("k"), "k": Float(1.0) },
                outputs: { "out": "o", "worse": "bad" },
            )"#,
        )
        .expect("function");
        let doc = MatGraphDoc::function(function, BTreeMap::new());
        // No calling graph: the parameter is unknown, and Normalize(Float) is an error.
        let d = diagnose_function(&doc, None);
        assert!(!d.ok);
        assert!(
            d.for_target(&Target::Node("t".into()))
                .any(|m| m.contains("unknown parameter")),
            "{:?}",
            d.errors
        );
        assert!(
            d.for_target(&Target::Node("bad".into()))
                .any(|m| m.contains("needs a vector")),
            "{:?}",
            d.errors
        );
        assert!(!d.warnings.is_empty(), "no calling graph is a warning");
        // With a calling graph that declares the parameter, only `bad` is wrong.
        let context = graph();
        let d = diagnose_function(&doc, Some(&context));
        let targets: BTreeSet<Target> = d.errors.iter().map(|e| e.target.clone()).collect();
        assert_eq!(
            targets,
            BTreeSet::from([Target::Node("bad".into())]),
            "{:?}",
            d.errors
        );
        // Fixed: it compiles.
        let mut fixed = doc.clone();
        fixed.graph.nodes.insert("k".into(), Node::Vec2([1.0, 0.0]));
        assert!(diagnose_function(&fixed, Some(&context)).ok);
        // An unwired output is an error on the output.
        if let DocMode::Function(meta) = &mut fixed.mode {
            meta.outputs.insert("worse".into(), String::new());
        }
        let d = diagnose_function(&fixed, Some(&context));
        assert_eq!(
            d.for_target(&Target::Output("worse".into())).collect::<Vec<_>>(),
            ["the output is not connected"]
        );
    }

    #[test]
    fn expanding_a_pass_through_rewires_readers_to_the_argument() {
        let mut g = graph();
        let f = MaterialFunction {
            format_version: 3,
            name: "id".into(),
            inputs: vec![FunctionInput {
                name: "x".into(),
                ty: ValueType::Vec3,
            }],
            nodes: BTreeMap::from([("x".into(), Node::Input("x".into()))]),
            outputs: BTreeMap::from([("o".into(), "x".into())]),
        };
        g.functions.insert("id".into(), FunctionSource::Inline(f.clone()));
        g.nodes.insert(
            "pass".into(),
            Node::Call {
                function: "id".into(),
                args: BTreeMap::from([("x".into(), "white".into())]),
                output: None,
            },
        );
        g.output.emissive = Some("pass".into());
        let mut doc = MatGraphDoc::new(g, BTreeMap::new());
        let out = expand(&mut doc, "pass", &f).expect("expand");
        assert_eq!(out.result, "white");
        assert_eq!(doc.graph.output.emissive.as_deref(), Some("white"));
        assert!(!doc.graph.nodes.contains_key("pass"));
        assert!(expand(&mut doc, "base", &f).is_err(), "not a call");
    }

    #[test]
    fn normalization_drops_comments_only() {
        assert_eq!(normalized_wgsl("a  \n    // Mul `x`\nb"), "a\nb");
    }
}
