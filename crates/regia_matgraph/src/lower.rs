// SPDX-License-Identifier: MPL-2.0
//! Lowering: static switches and material functions resolved into one flat node map.
//!
//! The compiler's type inference and emission only know plain nodes. Lowering runs first:
//!
//! - a `Switch` node becomes an alias of the branch its switch selects (the instance override
//!   or the declared default), so the other branch is unreachable and never emitted;
//! - a `Call` node expands the function's nodes in place under the call node's name (`worn/out`
//!   for a call node `worn`), its `Input`
//!   nodes become aliases of the call's arguments, and the call becomes an alias of the chosen
//!   output node;
//! - every input is then rewritten through the aliases.
//!
//! The result of a graph without switches or calls is the graph itself, so its generated code
//! (and pipeline key) is unchanged by this pass.
//!
//! Both open axes are bounded (ADR-4901): a graph declares at most
//! [`crate::compile::MAX_STATIC_SWITCHES`] switches (so at most 2^N code permutations), and the
//! expansion stops at [`crate::compile::MAX_LOWERED_NODES`] flat nodes and aliases, with an error
//! on the authored call that passed it. Without the second bound a function that calls another
//! function many times, nested a few levels, grows exponentially before any check runs.

use std::collections::{BTreeMap, HashMap};

use crate::compile::{CompileError, MAX_LOWERED_NODES, MAX_STATIC_SWITCHES};
use crate::format::{FunctionSource, MaterialFunction, MaterialGraph, Node, ValueType};

/// Deepest function nesting.
const MAX_DEPTH: usize = 8;

/// A call argument whose type must match the function input (checked after inference).
#[derive(Clone, Debug)]
pub(crate) struct ArgCheck {
    /// The call node (error key).
    pub call: String,
    /// Input name.
    pub input: String,
    /// The flat argument node.
    pub arg: String,
    /// Declared input type.
    pub ty: ValueType,
}

/// A graph with switches and calls resolved.
#[derive(Clone, Debug, Default)]
pub(crate) struct Lowered {
    /// Plain nodes only (no `Switch`, `Call` or `Input`).
    pub nodes: BTreeMap<String, Node>,
    /// Call argument type checks.
    pub arg_checks: Vec<ArgCheck>,
    /// Effective switch values.
    pub switches: BTreeMap<String, bool>,
    /// Node name → node it stands for (switch branch, call output, function argument).
    aliases: HashMap<String, String>,
    /// Function calls expanded (every nesting level counts).
    pub calls: usize,
}

impl Lowered {
    /// Flat nodes plus aliases: what [`MAX_LOWERED_NODES`] bounds.
    pub(crate) fn size(&self) -> usize {
        self.nodes.len() + self.aliases.len()
    }
}

impl Lowered {
    /// The node `name` stands for after alias resolution (`name` itself when it is no alias).
    /// `None` on an alias cycle.
    pub(crate) fn resolve(&self, name: &str) -> Option<String> {
        let mut current = name;
        for _ in 0..=self.aliases.len() {
            match self.aliases.get(current) {
                Some(next) => current = next,
                None => return Some(current.to_string()),
            }
        }
        None
    }
}

struct Lowering<'a> {
    graph: &'a MaterialGraph,
    out: Lowered,
    errors: &'a mut Vec<CompileError>,
    stack: Vec<String>,
    /// [`MAX_LOWERED_NODES`] was passed: nothing more is expanded (one error, on the authored node).
    over: bool,
    /// Calls already expanded, by (function, flat arguments in input order): a later call of the
    /// same function with the same arguments reads that expansion (ADR-7730).
    expanded: HashMap<(String, Vec<String>), String>,
}

impl Lowering<'_> {
    fn err(&mut self, node: impl Into<String>, message: impl Into<String>) {
        self.errors.push(CompileError {
            node: Some(node.into()),
            message: message.into(),
        });
    }

    /// Expands `nodes` under `prefix`. `args` maps function inputs to flat node names (inside
    /// a function).
    fn expand(
        &mut self,
        prefix: &str,
        nodes: &BTreeMap<String, Node>,
        args: Option<&HashMap<String, String>>,
    ) {
        for (name, node) in nodes {
            if self.over {
                return;
            }
            let flat = format!("{prefix}{name}");
            if self.out.size() >= MAX_LOWERED_NODES {
                self.over = true;
                // The authored node: the top-level call the expansion is inside, or the node.
                let authored = flat.split('/').next().unwrap_or(&flat).to_string();
                self.err(
                    authored,
                    format!(
                        "the graph expands to more than {MAX_LOWERED_NODES} nodes (function calls nest or repeat too much); \
                         call fewer functions or share one result"
                    ),
                );
                return;
            }
            match node {
                Node::Switch { switch, on, off } => match self.out.switches.get(switch) {
                    Some(&value) => {
                        let branch = if value { on } else { off };
                        self.out.aliases.insert(flat, format!("{prefix}{branch}"));
                    }
                    None => self.err(flat, format!("unknown switch `{switch}`")),
                },
                Node::Input(input) => match args {
                    Some(args) => match args.get(input) {
                        Some(arg) => {
                            self.out.aliases.insert(flat, arg.clone());
                        }
                        None => self.err(flat, format!("the function has no input `{input}`")),
                    },
                    None => self.err(flat, "Input is only valid inside a material function"),
                },
                Node::Call {
                    function,
                    args: call_args,
                    output,
                } => self.call(prefix, &flat, function, call_args, output.as_deref()),
                other => {
                    let mut node = other.clone();
                    node.map_inputs(|input| *input = format!("{prefix}{input}"));
                    self.out.nodes.insert(flat, node);
                }
            }
        }
    }

    fn call(
        &mut self,
        prefix: &str,
        flat: &str,
        function: &str,
        call_args: &BTreeMap<String, String>,
        output: Option<&str>,
    ) {
        let graph = self.graph;
        let definition: &MaterialFunction = match graph.functions.get(function) {
            Some(FunctionSource::Inline(definition)) => definition,
            Some(FunctionSource::File(path)) => {
                self.err(
                    flat,
                    format!(
                        "function `{function}` ({path}) was not resolved; load the graph through its loader"
                    ),
                );
                return;
            }
            None => {
                self.err(flat, format!("unknown function `{function}`"));
                return;
            }
        };
        if self.stack.iter().any(|f| f == function) || self.stack.len() >= MAX_DEPTH {
            self.err(
                flat,
                format!("function `{function}` calls itself (or nests deeper than {MAX_DEPTH})"),
            );
            return;
        }
        let mut args = HashMap::new();
        for input in &definition.inputs {
            match call_args.get(&input.name) {
                Some(arg) => {
                    let arg = format!("{prefix}{arg}");
                    self.out.arg_checks.push(ArgCheck {
                        call: flat.to_string(),
                        input: input.name.clone(),
                        arg: arg.clone(),
                        ty: input.ty,
                    });
                    args.insert(input.name.clone(), arg);
                }
                None => self.err(flat, format!("missing argument `{}`", input.name)),
            }
        }
        for name in call_args.keys() {
            if !definition.inputs.iter().any(|input| &input.name == name) {
                self.err(flat, format!("`{function}` has no input `{name}`"));
            }
        }
        let output_name = match output {
            Some(output) => output.to_string(),
            None if definition.outputs.len() == 1 => {
                definition.outputs.keys().next().cloned().unwrap_or_default()
            }
            None => {
                self.err(
                    flat,
                    format!("`{function}` has {} outputs; name one", definition.outputs.len()),
                );
                return;
            }
        };
        let Some(inner) = definition.outputs.get(&output_name) else {
            self.err(flat, format!("`{function}` has no output `{output_name}`"));
            return;
        };
        // The expansion is a function of the definition, the flat arguments and the graph's
        // switches (fixed for the whole lowering): a call that repeats an earlier one (another
        // output of a collapsed function, or the same call twice) aliases into that expansion,
        // so the nodes they share are emitted once (ADR-7730).
        let key = (
            function.to_string(),
            definition
                .inputs
                .iter()
                .map(|input| args.get(&input.name).cloned().unwrap_or_default())
                .collect::<Vec<_>>(),
        );
        if let Some(first) = self.expanded.get(&key) {
            let target = format!("{first}/{inner}");
            self.out.aliases.insert(flat.to_string(), target);
            return;
        }
        self.expanded.insert(key, flat.to_string());
        let inner_prefix = format!("{flat}/");
        self.out
            .aliases
            .insert(flat.to_string(), format!("{inner_prefix}{inner}"));
        self.out.calls += 1;
        self.stack.push(function.to_string());
        self.expand(&inner_prefix, &definition.nodes, Some(&args));
        self.stack.pop();
    }
}

/// Lowers `graph` with the switch `overrides` (unknown names are errors).
pub(crate) fn lower(
    graph: &MaterialGraph,
    overrides: &BTreeMap<String, bool>,
    errors: &mut Vec<CompileError>,
) -> Lowered {
    let mut lowering = Lowering {
        graph,
        out: Lowered::default(),
        errors,
        stack: Vec::new(),
        over: false,
        expanded: HashMap::new(),
    };
    if let Some(extra) = graph.switches.get(MAX_STATIC_SWITCHES) {
        lowering.err(
            format!("switch:{}", extra.name),
            format!(
                "{} static switches; at most {MAX_STATIC_SWITCHES} (each one doubles the code permutations)",
                graph.switches.len()
            ),
        );
    }
    for decl in &graph.switches {
        let key = format!("switch:{}", decl.name);
        if !crate::compile::valid_name(&decl.name) {
            lowering.err(key.clone(), "switch names are 1–64 characters of [A-Za-z0-9_]");
        }
        let value = overrides.get(&decl.name).copied().unwrap_or(decl.default);
        if lowering.out.switches.insert(decl.name.clone(), value).is_some() {
            lowering.err(key, "declared twice");
        }
    }
    for name in overrides.keys() {
        if !graph.switches.iter().any(|decl| &decl.name == name) {
            lowering.err(format!("switch:{name}"), "not declared by the graph");
        }
    }
    for (name, source) in &graph.functions {
        if !crate::compile::valid_name(name) {
            lowering.err(
                format!("function:{name}"),
                "function names are 1–64 characters of [A-Za-z0-9_]",
            );
        }
        if let FunctionSource::Inline(definition) = source {
            for node in definition.nodes.keys() {
                if !crate::compile::valid_name(node) {
                    lowering.err(
                        format!("function:{name}/{node}"),
                        "node names are 1–64 characters of [A-Za-z0-9_]",
                    );
                }
            }
        }
    }
    lowering.expand("", &graph.nodes, None);
    let mut out = lowering.out;
    // Rewrite every input through the aliases (a switch branch, a call output, an argument).
    let mut cycles = Vec::new();
    // One alias walk per distinct input name, not per reader.
    let mut memo: HashMap<String, Option<String>> = HashMap::new();
    let resolved: Vec<(String, Node)> = out
        .nodes
        .iter()
        .map(|(name, node)| {
            let mut node = node.clone();
            node.map_inputs(|input| {
                let target = memo
                    .entry(input.clone())
                    .or_insert_with(|| out.resolve(input))
                    .clone();
                match target {
                    Some(target) => *input = target,
                    None => cycles.push((name.clone(), input.clone())),
                }
            });
            (name.clone(), node)
        })
        .collect();
    out.nodes = resolved.into_iter().collect();
    let args: Vec<Option<String>> = out
        .arg_checks
        .iter()
        .map(|check| out.resolve(&check.arg))
        .collect();
    for (check, target) in out.arg_checks.iter_mut().zip(args) {
        if let Some(target) = target {
            check.arg = target;
        }
    }
    for (node, input) in cycles {
        errors.push(CompileError {
            node: Some(node),
            message: format!("`{input}` resolves through a cycle of switches or calls"),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{FunctionInput, SwitchDecl};

    fn function() -> MaterialFunction {
        let mut nodes = BTreeMap::new();
        nodes.insert("x".to_string(), Node::Input("x".into()));
        nodes.insert("k".to_string(), Node::Float(0.5));
        nodes.insert("half".to_string(), Node::Mul("x".into(), "k".into()));
        let mut outputs = BTreeMap::new();
        outputs.insert("out".to_string(), "half".to_string());
        MaterialFunction {
            format_version: crate::format::MATFUNC_FORMAT_VERSION,
            name: "halve".into(),
            inputs: vec![FunctionInput {
                name: "x".into(),
                ty: ValueType::Vec3,
            }],
            nodes,
            outputs,
        }
    }

    #[test]
    fn switches_alias_the_chosen_branch_and_calls_expand_in_place() {
        let mut graph = MaterialGraph::new("g");
        graph.switches.push(SwitchDecl {
            name: "dim".into(),
            default: true,
        });
        graph
            .functions
            .insert("halve".into(), FunctionSource::Inline(function()));
        graph.nodes.insert("c".into(), Node::Vec3([1.0, 0.5, 0.25]));
        let mut args = BTreeMap::new();
        args.insert("x".to_string(), "c".to_string());
        graph.nodes.insert(
            "dimmed".into(),
            Node::Call {
                function: "halve".into(),
                args,
                output: None,
            },
        );
        graph.nodes.insert(
            "pick".into(),
            Node::Switch {
                switch: "dim".into(),
                on: "dimmed".into(),
                off: "c".into(),
            },
        );
        graph.nodes.insert("sat".into(), Node::Saturate("pick".into()));
        let mut errors = Vec::new();
        let lowered = lower(&graph, &BTreeMap::new(), &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(lowered.nodes["sat"], Node::Saturate("dimmed/half".into()));
        assert_eq!(
            lowered.nodes["dimmed/half"],
            Node::Mul("c".into(), "dimmed/k".into())
        );
        let mut off = BTreeMap::new();
        off.insert("dim".to_string(), false);
        let lowered = lower(&graph, &off, &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(lowered.nodes["sat"], Node::Saturate("c".into()));
    }

    /// w11 (ADR-7730): two calls of one function with the same arguments share one expansion
    /// (the second is an alias into the first), so nodes they share are emitted once; a call
    /// with other arguments expands on its own.
    #[test]
    fn calls_with_the_same_arguments_share_one_expansion() {
        let mut graph = MaterialGraph::new("g");
        graph
            .functions
            .insert("halve".into(), FunctionSource::Inline(function()));
        graph.nodes.insert("c".into(), Node::Vec3([1.0, 0.5, 0.25]));
        graph.nodes.insert("e".into(), Node::Vec3([0.0, 1.0, 0.0]));
        let call = |arg: &str| Node::Call {
            function: "halve".into(),
            args: BTreeMap::from([("x".to_string(), arg.to_string())]),
            output: Some("out".into()),
        };
        graph.nodes.insert("a".into(), call("c"));
        graph.nodes.insert("b".into(), call("c"));
        graph.nodes.insert("d".into(), call("e"));
        graph
            .nodes
            .insert("sum".into(), Node::Add("a".into(), "b".into()));
        graph
            .nodes
            .insert("all".into(), Node::Add("sum".into(), "d".into()));
        let mut errors = Vec::new();
        let lowered = lower(&graph, &BTreeMap::new(), &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(
            lowered.nodes["sum"],
            Node::Add("a/half".into(), "a/half".into()),
            "b reads a's expansion"
        );
        assert!(
            !lowered.nodes.keys().any(|n| n.starts_with("b/")),
            "{:?}",
            lowered.nodes.keys()
        );
        assert_eq!(lowered.nodes["d/half"], Node::Mul("e".into(), "d/k".into()));
        assert_eq!(lowered.calls, 2, "two expansions");
    }

    #[test]
    fn many_readers_of_one_switch_chain_resolve_to_the_same_target() {
        let mut graph = MaterialGraph::new("g");
        graph.switches.push(SwitchDecl {
            name: "s".into(),
            default: false,
        });
        graph.nodes.insert("a".into(), Node::Float(1.0));
        graph.nodes.insert("b".into(), Node::Float(2.0));
        graph.nodes.insert(
            "s0".into(),
            Node::Switch {
                switch: "s".into(),
                on: "a".into(),
                off: "b".into(),
            },
        );
        for i in 1..20 {
            graph.nodes.insert(
                format!("s{i}"),
                Node::Switch {
                    switch: "s".into(),
                    on: "a".into(),
                    off: format!("s{}", i - 1),
                },
            );
        }
        for i in 0..20 {
            graph.nodes.insert(format!("r{i}"), Node::Saturate("s19".into()));
        }
        let mut errors = Vec::new();
        let lowered = lower(&graph, &BTreeMap::new(), &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
        for i in 0..20 {
            assert_eq!(lowered.nodes[&format!("r{i}")], Node::Saturate("b".into()));
        }
    }

    #[test]
    fn bad_calls_and_switches_are_reported_at_the_node() {
        let mut graph = MaterialGraph::new("g");
        graph
            .functions
            .insert("halve".into(), FunctionSource::Inline(function()));
        graph.nodes.insert(
            "call".into(),
            Node::Call {
                function: "halve".into(),
                args: BTreeMap::new(),
                output: Some("nope".into()),
            },
        );
        graph.nodes.insert(
            "sw".into(),
            Node::Switch {
                switch: "missing".into(),
                on: "a".into(),
                off: "b".into(),
            },
        );
        graph.nodes.insert("stray".into(), Node::Input("x".into()));
        let mut overrides = BTreeMap::new();
        overrides.insert("ghost".to_string(), true);
        let mut errors = Vec::new();
        lower(&graph, &overrides, &mut errors);
        let text: Vec<String> = errors.iter().map(ToString::to_string).collect();
        let all = text.join("\n");
        assert!(all.contains("call: missing argument `x`"), "{all}");
        assert!(all.contains("call: `halve` has no output `nope`"), "{all}");
        assert!(all.contains("sw: unknown switch `missing`"), "{all}");
        assert!(all.contains("stray: Input is only valid"), "{all}");
        assert!(all.contains("switch:ghost: not declared"), "{all}");
    }

    #[test]
    fn recursion_is_an_error() {
        let mut graph = MaterialGraph::new("g");
        let mut recursive = function();
        let mut args = BTreeMap::new();
        args.insert("x".to_string(), "x".to_string());
        recursive.nodes.insert(
            "again".into(),
            Node::Call {
                function: "halve".into(),
                args: args.clone(),
                output: None,
            },
        );
        recursive.outputs.insert("out".into(), "again".into());
        graph
            .functions
            .insert("halve".into(), FunctionSource::Inline(recursive));
        graph.nodes.insert("c".into(), Node::Vec3([1.0; 3]));
        let mut top = BTreeMap::new();
        top.insert("x".to_string(), "c".to_string());
        graph.nodes.insert(
            "call".into(),
            Node::Call {
                function: "halve".into(),
                args: top,
                output: None,
            },
        );
        let mut errors = Vec::new();
        lower(&graph, &BTreeMap::new(), &mut errors);
        assert!(
            errors.iter().any(|e| e.message.contains("calls itself")),
            "{errors:?}"
        );
    }
}
