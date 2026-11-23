// SPDX-License-Identifier: MPL-2.0
//! Graph assets (`.pgraph.ron`, `.pgvar.ron`), parameters and compilation (spec 62 § 4).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::data::{AttrType, DataKind, KindSet, Selector};
use crate::nodes::{
    Domain, EDITOR_AIDS, NodeKind, PinSpec, ProcNodeRegistry, SettingDesc, SettingType, Settings,
};
use crate::value::SettingValue;

/// `format_version` of `.pgraph.ron` files this build reads and writes.
pub const PGRAPH_FORMAT_VERSION: u32 = 1;
/// `format_version` of `.pgvar.ron` files.
pub const PGVAR_FORMAT_VERSION: u32 = 1;
/// Longest variant parent chain (spec 62 § 4).
pub const MAX_VARIANT_CHAIN: usize = 8;

/// The shape fingerprint of `pgraph` for `cargo xtask check-formats`: the hand-written reader's
/// field list, value types, kinds and domains. Edit it together with [`ProcGraph::from_ron`].
pub fn pgraph_schema_fingerprint() -> String {
    let types: Vec<&str> = AttrType::ALL.iter().map(|t| t.name()).collect();
    let kinds: Vec<&str> = DataKind::ALL.iter().map(|k| k.name()).collect();
    format!(
        "ProcGraph(format_version,description,params[(name,ty,default,min,max,doc)],inputs[(name,kinds)],\
         outputs[(name,kinds)],nodes{{u32:(ty,settings{{name:value}},at(x,y),enabled?,seed_salt?,v?)}},\
         edges[(from(node,pin),to(node,pin))],flags(starter,tool,domain));types={};kinds={};domains=World|Plant",
        types.join("|"),
        kinds.join("|")
    )
}

/// `pgraph` migration sources (none: version 1 is the first).
pub fn pgraph_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The shape fingerprint of `pgvar`.
pub fn pgvar_schema_fingerprint() -> String {
    "ProcGraphVariant(format_version,parent,overrides{name:value})".to_string()
}

/// `pgvar` migration sources (none).
pub fn pgvar_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// One compile or load problem: the node it names (if any), a stable code and a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphError {
    /// The node, when the problem belongs to one.
    pub node: Option<u32>,
    /// Stable lowercase code (`unknown_node_type`, `graph_cycle`, ...).
    pub code: String,
    /// Human-readable detail.
    pub message: String,
}

impl GraphError {
    pub(crate) fn new(node: Option<u32>, code: &str, message: impl Into<String>) -> Self {
        Self {
            node,
            code: code.to_string(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for GraphError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.node {
            Some(n) => write!(f, "node {n}: {}: {}", self.code, self.message),
            None => write!(f, "{}: {}", self.code, self.message),
        }
    }
}

/// Every problem a compile found (spec 62 § 4.2: all of them, not just the first).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{} graph error(s), first: {}", .0.len(), .0.first().map(ToString::to_string).unwrap_or_default())]
pub struct GraphErrors(pub Vec<GraphError>);

impl GraphErrors {
    /// Whether any error has `code`.
    pub fn has(&self, code: &str) -> bool {
        self.0.iter().any(|e| e.code == code)
    }
}

/// A declared graph parameter (spec 62 § 4.1).
#[derive(Clone, Debug, PartialEq)]
pub struct ParamDecl {
    /// `[a-z][a-z0-9_]{0,63}`.
    pub name: String,
    /// Value type.
    pub ty: AttrType,
    /// Default value.
    pub default: SettingValue,
    /// Inclusive minimum (numbers).
    pub min: Option<f64>,
    /// Inclusive maximum.
    pub max: Option<f64>,
    /// Doc line.
    pub doc: String,
}

/// A graph-level input or output pin.
#[derive(Clone, Debug, PartialEq)]
pub struct GraphPin {
    /// Pin name.
    pub name: String,
    /// Accepted kinds.
    pub kinds: KindSet,
}

/// One node entry of a graph file.
#[derive(Clone, Debug, PartialEq)]
pub struct NodeEntry {
    /// Node type id.
    pub ty: String,
    /// Untyped settings (spec 62 § 4).
    pub settings: BTreeMap<String, SettingValue>,
    /// Canvas position (not part of the hash).
    pub at: (f64, f64),
    /// `false` bypasses the node.
    pub enabled: bool,
    /// Explicit seed salt (default: the node id).
    pub seed_salt: Option<u32>,
    /// Settings version the settings were written with (`None` = current).
    pub v: Option<u32>,
}

impl NodeEntry {
    /// A node of type `ty` with no settings at the origin.
    pub fn new(ty: &str) -> Self {
        Self {
            ty: ty.to_string(),
            settings: BTreeMap::new(),
            at: (0.0, 0.0),
            enabled: true,
            seed_salt: None,
            v: None,
        }
    }

    /// Builder: sets one setting.
    pub fn with(mut self, name: &str, value: SettingValue) -> Self {
        self.settings.insert(name.to_string(), value);
        self
    }

    /// Builder: sets one setting from RON text (panics are avoided: bad text is stored as a string).
    pub fn with_ron(self, name: &str, ron: &str) -> Self {
        let v = SettingValue::parse(ron).unwrap_or_else(|_| SettingValue::Str(ron.to_string()));
        self.with(name, v)
    }
}

/// An edge from an output pin to an input pin.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Edge {
    /// `(node, output pin)`.
    pub from: (u32, String),
    /// `(node, input pin)`.
    pub to: (u32, String),
}

impl Edge {
    /// An edge.
    pub fn new(from: u32, from_pin: &str, to: u32, to_pin: &str) -> Self {
        Self {
            from: (from, from_pin.to_string()),
            to: (to, to_pin.to_string()),
        }
    }
}

/// Graph flags (spec 62 § 4).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GraphFlags {
    /// Listed in the editor's starter picker.
    pub starter: bool,
    /// Interactive tool kind the graph serves.
    pub tool: Option<String>,
    /// Domain.
    pub domain: Domain,
}

/// A procedural graph (`<name>.pgraph.ron`, format `pgraph` v1).
#[derive(Clone, Debug, PartialEq)]
pub struct ProcGraph {
    /// Always [`PGRAPH_FORMAT_VERSION`] after load.
    pub format_version: u32,
    /// Free text.
    pub description: String,
    /// Declared parameters.
    pub params: Vec<ParamDecl>,
    /// Graph input pins (subgraph use).
    pub inputs: Vec<GraphPin>,
    /// Graph output pins.
    pub outputs: Vec<GraphPin>,
    /// Nodes by id.
    pub nodes: BTreeMap<u32, NodeEntry>,
    /// Edges.
    pub edges: Vec<Edge>,
    /// Flags.
    pub flags: GraphFlags,
}

impl Default for ProcGraph {
    fn default() -> Self {
        Self {
            format_version: PGRAPH_FORMAT_VERSION,
            description: String::new(),
            params: Vec::new(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            nodes: BTreeMap::new(),
            edges: Vec::new(),
            flags: GraphFlags::default(),
        }
    }
}

/// A load failure of a graph or variant file.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct LoadError {
    /// `pgraph_parse`, `pgraph_version_newer`, `pgraph_shape`, `pgvar_parse`, ...
    pub code: String,
    /// Detail.
    pub message: String,
}

impl LoadError {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
        }
    }
}

fn fields_of<'a>(
    v: &'a SettingValue,
    expected: &str,
    prefix: &str,
) -> Result<&'a [(String, SettingValue)], LoadError> {
    match v {
        SettingValue::Record(name, fields) if name.as_deref().is_none_or(|n| n == expected) => Ok(fields),
        _ => Err(LoadError::new(
            &format!("{prefix}_shape"),
            format!("expected `{expected}( ... )`"),
        )),
    }
}

fn shape(prefix: &str, msg: impl Into<String>) -> LoadError {
    LoadError::new(&format!("{prefix}_shape"), msg)
}

fn opt_f64(v: &SettingValue) -> Option<f64> {
    match v {
        SettingValue::Call(n, a) if n == "Some" && a.len() == 1 => a[0].as_f64(),
        _ => v.as_f64(),
    }
}

fn opt_text(v: &SettingValue) -> Option<String> {
    match v {
        SettingValue::Call(n, a) if n == "Some" && a.len() == 1 => a[0].as_text().map(str::to_string),
        SettingValue::Ident(n) if n == "None" => None,
        _ => v.as_text().map(str::to_string),
    }
}

fn kinds_of(v: &SettingValue) -> Result<KindSet, LoadError> {
    let items = v
        .as_items()
        .ok_or_else(|| shape("pgraph", "pin kinds must be a list"))?;
    let mut kinds = Vec::new();
    for k in items {
        let name = k.as_text().unwrap_or("");
        kinds.push(
            DataKind::from_name(name)
                .ok_or_else(|| shape("pgraph", format!("unknown data kind `{name}`")))?,
        );
    }
    Ok(KindSet::of(&kinds))
}

fn pins_of(v: &SettingValue) -> Result<Vec<GraphPin>, LoadError> {
    let items = v
        .as_items()
        .ok_or_else(|| shape("pgraph", "graph pins must be a list"))?;
    items
        .iter()
        .map(|p| {
            Ok(GraphPin {
                name: p
                    .field("name")
                    .and_then(SettingValue::as_text)
                    .ok_or_else(|| shape("pgraph", "graph pin needs `name`"))?
                    .to_string(),
                kinds: match p.field("kinds") {
                    Some(k) => kinds_of(k)?,
                    None => KindSet::ANY,
                },
            })
        })
        .collect()
}

fn pin_ref(v: &SettingValue) -> Result<(u32, String), LoadError> {
    let items = v
        .as_items()
        .ok_or_else(|| shape("pgraph", "edge ends are `(node, \"pin\")`"))?;
    let node = items
        .first()
        .and_then(SettingValue::as_i64)
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| shape("pgraph", "edge end needs a node id"))?;
    let pin = items
        .get(1)
        .and_then(SettingValue::as_text)
        .ok_or_else(|| shape("pgraph", "edge end needs a pin name"))?;
    Ok((node, pin.to_string()))
}

fn version_of(fields: &[(String, SettingValue)], prefix: &str, current: u32) -> Result<u32, LoadError> {
    let v = fields
        .iter()
        .find(|(k, _)| k == "format_version")
        .and_then(|(_, v)| v.as_i64())
        .ok_or_else(|| shape(prefix, "missing `format_version`"))?;
    if v > i64::from(current) {
        return Err(LoadError::new(
            &format!("{prefix}_version_newer"),
            format!("format_version {v} is newer than this build reads ({current})"),
        ));
    }
    if v < 1 {
        return Err(shape(prefix, format!("format_version {v} is invalid")));
    }
    // Only version 1 exists; a future v2 adds its `VersionStep` migrators here (specs/12 § 4a).
    Ok(v as u32)
}

impl ProcGraph {
    /// Parses a `.pgraph.ron` file.
    ///
    /// # Errors
    ///
    /// [`LoadError`] with `pgraph_parse`, `pgraph_shape` or `pgraph_version_newer`. Unknown node types
    /// and settings still load (compile reports them), so the editor can open the file.
    pub fn from_ron(text: &str) -> Result<Self, LoadError> {
        let root = SettingValue::parse(text).map_err(|e| LoadError::new("pgraph_parse", e.to_string()))?;
        let fields = fields_of(&root, "ProcGraph", "pgraph")?;
        let format_version = version_of(fields, "pgraph", PGRAPH_FORMAT_VERSION)?;
        let mut g = ProcGraph {
            format_version,
            ..ProcGraph::default()
        };
        for (key, v) in fields {
            match key.as_str() {
                "format_version" => {}
                "description" => g.description = v.as_text().unwrap_or("").to_string(),
                "params" => {
                    let items = v
                        .as_items()
                        .ok_or_else(|| shape("pgraph", "`params` must be a list"))?;
                    for p in items {
                        let name = p
                            .field("name")
                            .and_then(SettingValue::as_text)
                            .unwrap_or("")
                            .to_string();
                        let ty_name = p.field("ty").and_then(SettingValue::as_text).unwrap_or("");
                        let ty = AttrType::from_name(ty_name).ok_or_else(|| {
                            shape("pgraph", format!("parameter `{name}`: unknown type `{ty_name}`"))
                        })?;
                        g.params.push(ParamDecl {
                            name,
                            ty,
                            default: p
                                .field("default")
                                .cloned()
                                .ok_or_else(|| shape("pgraph", "parameter needs `default`"))?,
                            min: p.field("min").and_then(opt_f64),
                            max: p.field("max").and_then(opt_f64),
                            doc: p
                                .field("doc")
                                .and_then(SettingValue::as_text)
                                .unwrap_or("")
                                .to_string(),
                        });
                    }
                }
                "inputs" => g.inputs = pins_of(v)?,
                "outputs" => g.outputs = pins_of(v)?,
                "nodes" => {
                    let SettingValue::Map(entries) = v else {
                        return Err(shape("pgraph", "`nodes` must be a map `{ id: (...) }`"));
                    };
                    for (k, n) in entries {
                        let id = k
                            .as_i64()
                            .and_then(|i| u32::try_from(i).ok())
                            .ok_or_else(|| shape("pgraph", "node ids are u32"))?;
                        let ty = n
                            .field("ty")
                            .and_then(SettingValue::as_text)
                            .ok_or_else(|| shape("pgraph", format!("node {id} needs `ty`")))?;
                        let mut entry = NodeEntry::new(ty);
                        if let Some(SettingValue::Map(s)) = n.field("settings") {
                            for (sk, sv) in s {
                                let name = sk.as_text().ok_or_else(|| {
                                    shape("pgraph", format!("node {id}: setting names are strings"))
                                })?;
                                entry.settings.insert(name.to_string(), sv.clone());
                            }
                        }
                        if let Some(at) = n.field("at").and_then(SettingValue::as_items) {
                            entry.at = (
                                at.first().and_then(SettingValue::as_f64).unwrap_or(0.0),
                                at.get(1).and_then(SettingValue::as_f64).unwrap_or(0.0),
                            );
                        }
                        entry.enabled = n.field("enabled").and_then(SettingValue::as_bool).unwrap_or(true);
                        entry.seed_salt = n
                            .field("seed_salt")
                            .and_then(SettingValue::as_i64)
                            .and_then(|s| u32::try_from(s).ok());
                        entry.v = n
                            .field("v")
                            .and_then(SettingValue::as_i64)
                            .and_then(|s| u32::try_from(s).ok());
                        if g.nodes.insert(id, entry).is_some() {
                            return Err(shape("pgraph", format!("node id {id} appears twice")));
                        }
                    }
                }
                "edges" => {
                    let items = v
                        .as_items()
                        .ok_or_else(|| shape("pgraph", "`edges` must be a list"))?;
                    for e in items {
                        let from = pin_ref(
                            e.field("from")
                                .ok_or_else(|| shape("pgraph", "edge needs `from`"))?,
                        )?;
                        let to = pin_ref(e.field("to").ok_or_else(|| shape("pgraph", "edge needs `to`"))?)?;
                        g.edges.push(Edge { from, to });
                    }
                }
                "flags" => {
                    g.flags.starter = v
                        .field("starter")
                        .and_then(SettingValue::as_bool)
                        .unwrap_or(false);
                    g.flags.tool = v.field("tool").and_then(opt_text);
                    let d = v
                        .field("domain")
                        .and_then(SettingValue::as_text)
                        .unwrap_or("World");
                    g.flags.domain = Domain::from_name(d)
                        .ok_or_else(|| shape("pgraph", format!("unknown domain `{d}`")))?;
                }
                other => return Err(shape("pgraph", format!("unknown field `{other}`"))),
            }
        }
        Ok(g)
    }

    /// The canonical file text (write → read → write is byte-identical).
    pub fn to_ron(&self) -> String {
        let mut o = String::from("ProcGraph(\n");
        o.push_str(&format!("    format_version: {},\n", self.format_version));
        o.push_str(&format!(
            "    description: {},\n",
            SettingValue::Str(self.description.clone()).write()
        ));
        o.push_str("    params: [\n");
        for p in &self.params {
            let opt = |v: Option<f64>| match v {
                Some(x) => SettingValue::Call("Some".into(), vec![SettingValue::Float(x)]).write(),
                None => "None".to_string(),
            };
            o.push_str(&format!(
                "        (name: {}, ty: {}, default: {}, min: {}, max: {}, doc: {}),\n",
                SettingValue::Str(p.name.clone()).write(),
                p.ty.name(),
                p.default.write(),
                opt(p.min),
                opt(p.max),
                SettingValue::Str(p.doc.clone()).write()
            ));
        }
        o.push_str("    ],\n");
        for (label, pins) in [("inputs", &self.inputs), ("outputs", &self.outputs)] {
            o.push_str(&format!("    {label}: ["));
            for (k, p) in pins.iter().enumerate() {
                if k > 0 {
                    o.push_str(", ");
                }
                let kinds: Vec<String> = p
                    .kinds
                    .names()
                    .into_iter()
                    .map(|n| SettingValue::Str(n.to_string()).write())
                    .collect();
                o.push_str(&format!(
                    "(name: {}, kinds: [{}])",
                    SettingValue::Str(p.name.clone()).write(),
                    kinds.join(", ")
                ));
            }
            o.push_str("],\n");
        }
        o.push_str("    nodes: {\n");
        for (id, n) in &self.nodes {
            let settings = SettingValue::Map(
                n.settings
                    .iter()
                    .map(|(k, v)| (SettingValue::Str(k.clone()), v.clone()))
                    .collect(),
            );
            o.push_str(&format!(
                "        {id}: (ty: {}, settings: {}, at: {}",
                SettingValue::Str(n.ty.clone()).write(),
                settings.write(),
                SettingValue::Tuple(vec![SettingValue::Float(n.at.0), SettingValue::Float(n.at.1)]).write()
            ));
            if !n.enabled {
                o.push_str(", enabled: false");
            }
            if let Some(s) = n.seed_salt {
                o.push_str(&format!(", seed_salt: {s}"));
            }
            if let Some(v) = n.v {
                o.push_str(&format!(", v: {v}"));
            }
            o.push_str("),\n");
        }
        o.push_str("    },\n    edges: [\n");
        for e in &self.edges {
            o.push_str(&format!(
                "        (from: ({}, {}), to: ({}, {})),\n",
                e.from.0,
                SettingValue::Str(e.from.1.clone()).write(),
                e.to.0,
                SettingValue::Str(e.to.1.clone()).write()
            ));
        }
        o.push_str("    ],\n");
        let tool = match &self.flags.tool {
            Some(t) => format!("Some({})", SettingValue::Str(t.clone()).write()),
            None => "None".to_string(),
        };
        o.push_str(&format!(
            "    flags: (starter: {}, tool: {tool}, domain: {}),\n)\n",
            self.flags.starter,
            self.flags.domain.name()
        ));
        o
    }

    /// The next free node id (`max + 1`; ids are never reused within a file).
    pub fn next_node_id(&self) -> u32 {
        self.nodes.keys().next_back().map_or(1, |k| k.saturating_add(1))
    }

    /// The declared parameter `name`.
    pub fn param(&self, name: &str) -> Option<&ParamDecl> {
        self.params.iter().find(|p| p.name == name)
    }

    /// The graph hash (spec 62 § 6.4): parameters, nodes in id order (type, settings version,
    /// settings, enabled, seed salt), edges sorted after reroutes are collapsed, graph pins.
    /// Canvas positions, notes and the description are excluded.
    pub fn graph_hash(&self, registry: &ProcNodeRegistry) -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        h.update(b"regia.pgraph.v1");
        for p in &self.params {
            h.update(p.name.as_bytes());
            h.update(p.ty.name().as_bytes());
            h.update(p.default.write().as_bytes());
            h.update(&p.min.unwrap_or(f64::NAN).to_bits().to_le_bytes());
            h.update(&p.max.unwrap_or(f64::NAN).to_bits().to_le_bytes());
        }
        for (id, n) in &self.nodes {
            if EDITOR_AIDS.contains(&n.ty.as_str()) {
                continue;
            }
            h.update(&id.to_le_bytes());
            h.update(n.ty.as_bytes());
            let version = registry.get(&n.ty).map_or(0, |k| k.settings_version());
            h.update(&n.v.unwrap_or(version).to_le_bytes());
            for (k, v) in &n.settings {
                h.update(k.as_bytes());
                h.update(v.write().as_bytes());
            }
            h.update(&[u8::from(n.enabled)]);
            h.update(&n.seed_salt.unwrap_or(*id).to_le_bytes());
        }
        let mut edges = collapse_reroutes(self);
        edges.sort();
        for e in &edges {
            h.update(&e.from.0.to_le_bytes());
            h.update(e.from.1.as_bytes());
            h.update(&e.to.0.to_le_bytes());
            h.update(e.to.1.as_bytes());
        }
        for (tag, pins) in [(b"in", &self.inputs), (b"ou", &self.outputs)] {
            for p in pins {
                h.update(tag);
                h.update(p.name.as_bytes());
                h.update(&p.kinds.0.to_le_bytes());
            }
        }
        *h.finalize().as_bytes()
    }

    /// Compiles with the graph's defaults (no overrides).
    ///
    /// # Errors
    ///
    /// Every problem found, see [`ProcGraph::compile_with`].
    ///
    /// ```
    /// use regia_procgen::{NodeEntry, ProcGraph, ProcNodeRegistry};
    ///
    /// let registry = ProcNodeRegistry::with_builtins();
    ///
    /// // An unknown node type is reported at compile time, with a code.
    /// let mut bad = ProcGraph::default();
    /// bad.nodes.insert(1, NodeEntry::new("no.such.node"));
    /// let errors = bad.compile(&registry).err().expect("unknown node type");
    /// assert!(!errors.0.is_empty());
    ///
    /// // A generator feeding the output compiles.
    /// let mut good = ProcGraph::default();
    /// good.nodes.insert(1, NodeEntry::new("gen.grid"));
    /// assert!(good.compile(&registry).is_ok());
    /// ```
    pub fn compile(&self, registry: &ProcNodeRegistry) -> Result<CompiledGraph, GraphErrors> {
        self.compile_with(registry, &BTreeMap::new())
    }

    /// Validates the graph and builds its evaluation order (spec 62 § 4.2), with parameter
    /// overrides (generator over variant chain, already merged by the caller).
    ///
    /// # Errors
    ///
    /// [`GraphErrors`] listing every problem (unknown node types, pins, settings, parameters, kind
    /// mismatches, cycles, unconnected required pins).
    pub fn compile_with(
        &self,
        registry: &ProcNodeRegistry,
        overrides: &BTreeMap<String, SettingValue>,
    ) -> Result<CompiledGraph, GraphErrors> {
        self.compile_full(registry, overrides, &|_| None)
    }

    /// As [`ProcGraph::compile_with`], reading the graphs that `graph.call` / `graph.loop` nodes name
    /// through `load` (their declared pins become the node's pins).
    ///
    /// # Errors
    ///
    /// As [`ProcGraph::compile_with`], plus `procgen_unknown_graph` for a called graph `load` cannot read.
    pub fn compile_full(
        &self,
        registry: &ProcNodeRegistry,
        overrides: &BTreeMap<String, SettingValue>,
        load: &dyn Fn(&str) -> Option<String>,
    ) -> Result<CompiledGraph, GraphErrors> {
        // Without the pipeline feature no plant node type exists and plant graphs never run
        // (spec 68 § 2).
        if !cfg!(feature = "pipeline") && self.flags.domain == Domain::Plant {
            return Err(GraphErrors(vec![GraphError::new(
                None,
                "plant_runtime_forbidden",
                "Plant graphs run only in pipeline builds (editor, CLI)",
            )]));
        }
        let mut errors = Vec::new();
        let mut warnings = Vec::new();

        // Parameters: names, types, effective values (override → default), clamping.
        let mut values: BTreeMap<String, SettingValue> = BTreeMap::new();
        for p in &self.params {
            if !valid_param_name(&p.name) {
                errors.push(GraphError::new(
                    None,
                    "param_name",
                    format!("`{}` is not a valid parameter name", p.name),
                ));
                continue;
            }
            let raw = overrides.get(&p.name).unwrap_or(&p.default);
            match coerce_param(p, raw) {
                Ok((v, clamped)) => {
                    if clamped {
                        warnings.push(GraphError::new(
                            None,
                            "param_clamped",
                            format!(
                                "parameter `{}` = {} clamped to {}",
                                p.name,
                                raw.write(),
                                v.write()
                            ),
                        ));
                    }
                    values.insert(p.name.clone(), v);
                }
                Err(m) => errors.push(GraphError::new(None, "param_type_mismatch", m)),
            }
        }
        for name in overrides.keys() {
            if self.param(name).is_none() {
                warnings.push(GraphError::new(
                    None,
                    "variant_unknown_param",
                    format!("override of unknown parameter `{name}` ignored"),
                ));
            }
        }

        // Nodes: types, domains, settings.
        let mut nodes: BTreeMap<u32, (Arc<dyn NodeKind>, Settings, PinSpec)> = BTreeMap::new();
        for (&id, n) in &self.nodes {
            if EDITOR_AIDS.contains(&n.ty.as_str()) {
                continue;
            }
            let Some(kind) = registry.get(&n.ty) else {
                errors.push(GraphError::new(
                    Some(id),
                    "unknown_node_type",
                    format!("unknown node type `{}`", n.ty),
                ));
                continue;
            };
            if !kind.domains().contains(&self.flags.domain) {
                errors.push(GraphError::new(
                    Some(id),
                    "node_wrong_domain",
                    format!(
                        "`{}` is not allowed in a {} graph",
                        n.ty,
                        self.flags.domain.name()
                    ),
                ));
            }
            let mut raw = n.settings.clone();
            // Settings a newer build wrote cannot be read under this build's schema (audit L3).
            if let Some(v) = n.v
                && v > kind.settings_version()
            {
                errors.push(GraphError::new(
                    Some(id),
                    "setting_version_newer",
                    format!(
                        "`{}` settings are version {v}; this build reads up to {}",
                        n.ty,
                        kind.settings_version()
                    ),
                ));
                continue;
            }
            if let Some(v) = n.v
                && v < kind.settings_version()
                && let Err(m) = kind.migrate_settings(v, &mut raw)
            {
                errors.push(GraphError::new(Some(id), "setting_migration", m));
                continue;
            }
            match resolve_settings(id, kind.as_ref(), &raw, &values) {
                Ok(s) => {
                    let pins = if CALL_NODES.contains(&kind.id()) {
                        match call_pins(&s, load) {
                            Ok(p) => p,
                            Err(m) => {
                                errors.push(GraphError::new(Some(id), "procgen_unknown_graph", m));
                                continue;
                            }
                        }
                    } else {
                        kind.pins(&s)
                    };
                    let pins = with_override_pins(self, id, kind.as_ref(), pins);
                    nodes.insert(id, (kind.clone(), s, pins));
                }
                Err(mut e) => errors.append(&mut e),
            }
        }

        // Edges (reroutes collapsed), pins and kinds.
        let edges = collapse_reroutes(self);
        let mut incoming: BTreeMap<(u32, usize), Vec<EdgeSource>> = BTreeMap::new();
        let mut valid_edges: Vec<(u32, u32)> = Vec::new();
        // Input pins that had an edge, valid or not: a rejected edge is reported once, not again as
        // an unconnected pin.
        let mut attempted: BTreeSet<(u32, String)> = BTreeSet::new();
        for e in &edges {
            attempted.insert(e.to.clone());
            let (Some(src), Some(dst)) = (nodes.get(&e.from.0), nodes.get(&e.to.0)) else {
                let missing = if !self.nodes.contains_key(&e.from.0) {
                    Some(e.from.0)
                } else if !self.nodes.contains_key(&e.to.0) {
                    Some(e.to.0)
                } else {
                    None
                };
                if let Some(m) = missing {
                    errors.push(GraphError::new(
                        Some(m),
                        "unknown_pin",
                        format!("edge names missing node {m}"),
                    ));
                }
                continue;
            };
            let Some(op) = src.2.output(&e.from.1) else {
                errors.push(GraphError::new(
                    Some(e.from.0),
                    "unknown_pin",
                    format!("node {} has no output pin `{}`", e.from.0, e.from.1),
                ));
                continue;
            };
            let Some(ip) = dst.2.input(&e.to.1) else {
                errors.push(GraphError::new(
                    Some(e.to.0),
                    "unknown_pin",
                    format!("node {} has no input pin `{}`", e.to.0, e.to.1),
                ));
                continue;
            };
            let (okinds, ikinds) = (src.2.outputs[op].kinds, dst.2.inputs[ip].kinds);
            if !okinds.intersects(ikinds) {
                errors.push(GraphError::new(
                    Some(e.to.0),
                    "pin_kind_mismatch",
                    format!(
                        "{}.{} produces {:?} but {}.{} accepts {:?}",
                        e.from.0,
                        e.from.1,
                        okinds.names(),
                        e.to.0,
                        e.to.1,
                        ikinds.names()
                    ),
                ));
                continue;
            }
            incoming
                .entry((e.to.0, ip))
                .or_default()
                .push((e.from.0, e.from.1.clone(), op));
            valid_edges.push((e.from.0, e.to.0));
        }
        for ((node, ip), srcs) in &mut incoming {
            srcs.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
            if srcs.len() > 1
                && let Some((_, _, pins)) = nodes.get(node)
                && !pins.inputs[*ip].multi
            {
                errors.push(GraphError::new(
                    Some(*node),
                    "pin_too_many_edges",
                    format!(
                        "input pin `{}` takes one edge, has {}",
                        pins.inputs[*ip].name,
                        srcs.len()
                    ),
                ));
            }
        }
        for (&id, (_, _, pins)) in &nodes {
            for (ip, p) in pins.inputs.iter().enumerate() {
                if p.required
                    && !incoming.contains_key(&(id, ip))
                    && !attempted.contains(&(id, p.name.clone()))
                {
                    errors.push(GraphError::new(
                        Some(id),
                        "pin_unconnected",
                        format!("required input pin `{}` is not connected", p.name),
                    ));
                }
            }
        }

        // Topological order (Kahn 1962), ties by ascending node id.
        let order = match topo_order(nodes.keys().copied().collect(), &valid_edges) {
            Ok(o) => o,
            Err(on_cycle) => {
                errors.push(GraphError::new(
                    Some(on_cycle),
                    "graph_cycle",
                    format!("node {on_cycle} is on a cycle"),
                ));
                Vec::new()
            }
        };

        // Declared graph outputs need a `graph.output` node.
        for out in &self.outputs {
            let fed = nodes
                .values()
                .any(|(k, s, _)| k.id() == "graph.output" && s.text("pin") == out.name);
            if !fed {
                warnings.push(GraphError::new(
                    None,
                    "output_unreachable",
                    format!("graph output `{}` has no `graph.output` node", out.name),
                ));
            }
        }

        if !errors.is_empty() {
            return Err(GraphErrors(errors));
        }

        let index: BTreeMap<u32, usize> = order.iter().enumerate().map(|(k, id)| (*id, k)).collect();
        let mut compiled = Vec::with_capacity(order.len());
        for &id in &order {
            let Some((kind, settings, pins)) = nodes.remove(&id) else {
                continue;
            };
            let entry = &self.nodes[&id];
            let inputs = (0..pins.inputs.len())
                .map(|ip| {
                    incoming
                        .get(&(id, ip))
                        .map(|srcs| srcs.iter().map(|(n, _, op)| (index[n], *op)).collect())
                        .unwrap_or_default()
                })
                .collect();
            let mut settings_text = String::new();
            for (k, v) in &settings.0 {
                settings_text.push_str(k);
                settings_text.push('=');
                v.write_into(&mut settings_text);
                settings_text.push(';');
            }
            let overrides = pins
                .inputs
                .iter()
                .filter_map(|p| p.name.strip_prefix('@').map(str::to_string))
                .collect();
            compiled.push(CompiledNode {
                id,
                kind,
                settings,
                settings_text,
                enabled: entry.enabled,
                seed_salt: entry.seed_salt.unwrap_or(id),
                pins,
                inputs,
                overrides,
            });
        }
        let mut ph = blake3::Hasher::new();
        for (k, v) in &values {
            ph.update(k.as_bytes());
            ph.update(v.write().as_bytes());
        }
        Ok(CompiledGraph {
            registry: Arc::new(registry.clone()),
            nodes: compiled,
            graph_hash: self.graph_hash(registry),
            param_hash: *ph.finalize().as_bytes(),
            params: values,
            warnings,
            domain: self.flags.domain,
        })
    }
}

impl ProcGraph {
    /// The pins of node `id` as the compiler resolves them (its settings over the type's defaults,
    /// parameters at their defaults; a `graph.call` / `graph.loop` reads the called graph through
    /// `load`). `None` for a missing node, an unknown type, an editor aid (`note`, `reroute`) or
    /// settings that do not resolve. The editor's canvas draws these.
    pub fn node_pins(
        &self,
        id: u32,
        registry: &ProcNodeRegistry,
        load: &dyn Fn(&str) -> Option<String>,
    ) -> Option<crate::nodes::PinSpec> {
        let n = self.nodes.get(&id)?;
        if EDITOR_AIDS.contains(&n.ty.as_str()) {
            return None;
        }
        let kind = registry.get(&n.ty)?;
        let params: BTreeMap<String, SettingValue> = self
            .params
            .iter()
            .map(|p| (p.name.clone(), p.default.clone()))
            .collect();
        let s = resolve_settings(id, kind.as_ref(), &n.settings, &params).ok()?;
        let pins = if CALL_NODES.contains(&kind.id()) {
            call_pins(&s, load).ok()?
        } else {
            kind.pins(&s)
        };
        Some(with_override_pins(self, id, kind.as_ref(), pins))
    }
}

/// Appends an override input pin `@<setting>` (one `Table`, spec 62 § 4.1) for every setting of
/// node `id` that an edge targets, in setting-name order. An `@` name that is not a setting gets
/// no pin (the edge then fails as `unknown_pin`).
fn with_override_pins(g: &ProcGraph, id: u32, kind: &dyn NodeKind, mut pins: PinSpec) -> PinSpec {
    let wanted: BTreeSet<&str> = g
        .edges
        .iter()
        .filter(|e| e.to.0 == id)
        .filter_map(|e| e.to.1.strip_prefix('@'))
        .filter(|name| kind.settings().iter().any(|d| d.name == *name))
        .collect();
    for name in wanted {
        pins.inputs.push(crate::nodes::PinDesc {
            name: format!("@{name}"),
            kinds: KindSet::TABLE,
            multi: false,
            required: false,
        });
    }
    pins
}

/// The value an override table gives a setting of type `ty`: column `setting` (or the sole
/// column) at row 0, converted (spec 62 § 4.1). `Ok(None)` when the bundle holds no table.
///
/// A float converts to `Int` only when it is a finite integer inside `i64`. `2^63` is not
/// (the cast would saturate), and neither is a non-integer.
///
/// # Errors
///
/// A message when the column is missing, the table is empty or the type does not convert.
pub fn override_value(
    setting: &str,
    ty: SettingType,
    bundle: &crate::Bundle,
) -> Result<Option<SettingValue>, String> {
    use crate::data::{AttrValue, Payload};
    let Some(table) = bundle.0.iter().find_map(|i| match &i.payload {
        Payload::Table(t) => Some(t),
        _ => None,
    }) else {
        return Ok(None);
    };
    if table.rows == 0 {
        return Err(format!("override table for `{setting}` has no rows"));
    }
    let sole = match table.columns.as_slice() {
        [(_, only)] => Some(only),
        _ => None,
    };
    let col = table.column(setting).or(sole);
    let col = col.ok_or_else(|| format!("override table has no column `{setting}`"))?;
    let v = col
        .get(0)
        .ok_or_else(|| format!("override table for `{setting}` has no row 0"))?;
    let shown = format!("{v:?}");
    let bad = || {
        format!(
            "override value {shown} does not convert to `{setting}`'s type {}",
            ty.name()
        )
    };
    Ok(Some(match (ty, v) {
        (SettingType::Float, AttrValue::Float(f)) => SettingValue::Float(f),
        (SettingType::Float, AttrValue::Int(i)) => SettingValue::Float(i as f64),
        (SettingType::Int, AttrValue::Int(i)) => SettingValue::Int(i),
        (SettingType::Int, AttrValue::Float(f)) => match f64_exact_i64(f) {
            Some(i) => SettingValue::Int(i),
            None => return Err(bad()),
        },
        (SettingType::Bool, AttrValue::Bool(b)) => SettingValue::Bool(b),
        (SettingType::Vec3, AttrValue::Vec3(a)) => {
            SettingValue::Tuple(a.iter().map(|c| SettingValue::Float(*c)).collect())
        }
        (SettingType::Vec3, AttrValue::Float(f)) => SettingValue::Tuple(vec![SettingValue::Float(f); 3]),
        (
            SettingType::Name | SettingType::Asset | SettingType::Any,
            AttrValue::Name(n) | AttrValue::Asset(n),
        ) => SettingValue::Str(n),
        (SettingType::Enum(options), AttrValue::Name(n)) if options.contains(&n.as_str()) => {
            SettingValue::Ident(n)
        }
        (SettingType::Any, AttrValue::Float(f)) => SettingValue::Float(f),
        (SettingType::Any, AttrValue::Int(i)) => SettingValue::Int(i),
        (SettingType::Any, AttrValue::Bool(b)) => SettingValue::Bool(b),
        _ => return Err(bad()),
    }))
}

/// Node types whose pins are the called graph's declared pins.
pub const CALL_NODES: [&str; 2] = ["graph.call", "graph.loop"];

/// The pins of a `graph.call` / `graph.loop`: the called graph's declared inputs (multi) and outputs.
fn call_pins(s: &Settings, load: &dyn Fn(&str) -> Option<String>) -> Result<PinSpec, String> {
    let path = s.text("graph");
    let text = load(path).ok_or_else(|| format!("cannot read called graph `{path}`"))?;
    let g = ProcGraph::from_ron(&text).map_err(|e| format!("called graph `{path}`: {e}"))?;
    Ok(PinSpec::new(
        g.inputs
            .iter()
            .map(|p| crate::nodes::PinDesc {
                name: p.name.clone(),
                kinds: p.kinds,
                multi: true,
                required: false,
            })
            .collect(),
        g.outputs
            .iter()
            .map(|p| crate::nodes::PinDesc {
                name: p.name.clone(),
                kinds: p.kinds,
                multi: false,
                required: false,
            })
            .collect(),
    ))
}

/// Every graph reachable from `root` through `graph.call` / `graph.loop` paths, read through `load`
/// (what a host captures into [`crate::exec::RunInputs::graphs`]). Parameters take their declared
/// defaults: [`collect_subgraphs_with`] is the walk for a run with parameter values.
pub fn collect_subgraphs(
    root: &ProcGraph,
    load: &dyn Fn(&str) -> Option<String>,
) -> BTreeMap<String, String> {
    collect_subgraphs_with(root, &BTreeMap::new(), load)
}

/// [`collect_subgraphs`] with `params` binding `root`'s settings, the values compile resolves: a
/// `graph: Bind("body")` compiled, then its run failed with `unknown_graph`, because the capture
/// walked the raw settings. Each callee is walked with the parameters its caller passes.
pub fn collect_subgraphs_with(
    root: &ProcGraph,
    params: &BTreeMap<String, SettingValue>,
    load: &dyn Fn(&str) -> Option<String>,
) -> BTreeMap<String, String> {
    /// Visits of one walk: a path may be reached with several parameter sets.
    const MAX_VISITS: usize = 1024;
    type Call = (String, BTreeMap<String, SettingValue>);
    let mut found: BTreeMap<String, String> = BTreeMap::new();
    let mut visited: BTreeSet<String> = BTreeSet::new();
    let mut stack: Vec<Call> = Vec::new();
    let scan = |g: &ProcGraph, values: &BTreeMap<String, SettingValue>, stack: &mut Vec<Call>| {
        let lookup = |name: &str| {
            values
                .get(name)
                .cloned()
                .or_else(|| g.param(name).map(|p| p.default.clone()))
        };
        for n in g.nodes.values() {
            if !CALL_NODES.contains(&n.ty.as_str()) {
                continue;
            }
            let mut unknown = Vec::new();
            if let Some(SettingValue::Str(p)) = n
                .settings
                .get("graph")
                .map(|v| v.resolve_binds(&lookup, &mut unknown))
            {
                let passed = n
                    .settings
                    .get("params")
                    .map(|v| call_params(&v.resolve_binds(&lookup, &mut unknown)))
                    .unwrap_or_default();
                // Candidates before the declared path. The stack is LIFO, and the visit
                // and capture caps must not drop the graph the call names (ADR-3027).
                if n.ty == "graph.call" {
                    let graphs = n
                        .settings
                        .get("graphs")
                        .map(|v| v.resolve_binds(&lookup, &mut unknown));
                    let texts: Vec<String> = match &graphs {
                        Some(v) => match v.as_items() {
                            Some(items) => items
                                .iter()
                                .filter_map(|c| c.as_text().map(str::to_string))
                                .collect(),
                            None => v.as_text().map(|c| vec![c.to_string()]).unwrap_or_default(),
                        },
                        None => Vec::new(),
                    };
                    for c in texts {
                        if !c.is_empty() && c != p {
                            stack.push((c, passed.clone()));
                        }
                    }
                }
                stack.push((p, passed));
            }
        }
    };
    scan(root, params, &mut stack);
    while let Some((p, values)) = stack.pop() {
        let visit = format!("{p}\0{values:?}");
        if !visited.insert(visit) || visited.len() > MAX_VISITS {
            continue;
        }
        if !found.contains_key(&p) && found.len() >= 256 {
            continue;
        }
        let Some(text) = found.get(&p).cloned().or_else(|| load(&p)) else {
            continue;
        };
        if let Ok(g) = ProcGraph::from_ron(&text) {
            scan(&g, &values, &mut stack);
        }
        found.insert(p, text);
    }
    found
}

/// The parameter values a `params` setting of `graph.call` / `graph.loop` passes.
pub(crate) fn call_params(v: &SettingValue) -> BTreeMap<String, SettingValue> {
    match v {
        SettingValue::Record(_, fields) => fields.iter().cloned().collect(),
        SettingValue::Map(entries) => entries
            .iter()
            .filter_map(|(k, v)| k.as_text().map(|k| (k.to_string(), v.clone())))
            .collect(),
        _ => BTreeMap::new(),
    }
}

/// An edge into an input pin: `(source node, source pin name, source pin index)`.
type EdgeSource = (u32, String, usize);

/// Whether a parameter name follows `[a-z][a-z0-9_]{0,63}`.
pub fn valid_param_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
}

/// A finite `f64` that is an integer inside `i64`.
///
/// `i64::MAX as f64` is `2^63`, which does not fit: casting it saturates to `i64::MAX`.
/// `i64::MIN as f64` is `-2^63` and does fit. The half-open range keeps that lower end and
/// drops `2^63`. Every other finite value in the range with a zero fraction is exact.
fn f64_exact_i64(f: f64) -> Option<i64> {
    if f.is_finite() && f.fract() == 0.0 && (i64::MIN as f64..i64::MAX as f64).contains(&f) {
        Some(f as i64)
    } else {
        None
    }
}

/// Converts a parameter value to the declared type (spec 62 § 3.5 widening) and clamps it.
fn coerce_param(p: &ParamDecl, v: &SettingValue) -> Result<(SettingValue, bool), String> {
    let mismatch = || {
        format!(
            "parameter `{}` expects {}, got {}",
            p.name,
            p.ty.name(),
            v.write()
        )
    };
    let clamp = |x: f64| -> (f64, bool) {
        let mut y = x;
        if let Some(lo) = p.min {
            y = y.max(lo);
        }
        if let Some(hi) = p.max {
            y = y.min(hi);
        }
        (y, y != x)
    };
    Ok(match p.ty {
        AttrType::Bool => (SettingValue::Bool(v.as_bool().ok_or_else(mismatch)?), false),
        AttrType::Int => {
            let i = match v {
                SettingValue::Int(i) => *i,
                SettingValue::Bool(b) => i64::from(*b),
                _ => return Err(mismatch()),
            };
            let bound = |which: &str, x: f64| -> Result<i64, String> {
                f64_exact_i64(x).ok_or_else(|| {
                    format!(
                        "parameter `{}` {which} {x} is not an integer in i64 range",
                        p.name
                    )
                })
            };
            let mut y = i;
            if let Some(lo) = p.min {
                let lo = bound("min", lo)?;
                if y < lo {
                    y = lo;
                }
            }
            if let Some(hi) = p.max {
                let hi = bound("max", hi)?;
                if y > hi {
                    y = hi;
                }
            }
            (SettingValue::Int(y), y != i)
        }
        AttrType::Float => {
            let x = match v {
                SettingValue::Bool(b) => f64::from(u8::from(*b)),
                _ => v.as_f64().ok_or_else(mismatch)?,
            };
            if !x.is_finite() {
                return Err(mismatch());
            }
            let (y, c) = clamp(x);
            (SettingValue::Float(y), c)
        }
        AttrType::Name | AttrType::Asset => (
            SettingValue::Str(v.as_text().ok_or_else(mismatch)?.to_string()),
            false,
        ),
        AttrType::Vec2 | AttrType::Vec3 | AttrType::Vec4 | AttrType::Quat => {
            let n = p.ty.width().unwrap_or(0);
            let items: Vec<SettingValue> = match (v.as_f64(), v.as_items()) {
                (Some(s), _) => vec![SettingValue::Float(s); n],
                (None, Some(items)) if items.len() == n && items.iter().all(|x| x.as_f64().is_some()) => {
                    items
                        .iter()
                        .map(|x| SettingValue::Float(x.as_f64().unwrap_or(0.0)))
                        .collect()
                }
                _ => return Err(mismatch()),
            };
            (SettingValue::Tuple(items), false)
        }
        AttrType::Transform => (v.clone(), false),
    })
}

/// Fills defaults, replaces `Bind`s and checks every setting against the node's schema.
fn resolve_settings(
    id: u32,
    kind: &dyn NodeKind,
    raw: &BTreeMap<String, SettingValue>,
    params: &BTreeMap<String, SettingValue>,
) -> Result<Settings, Vec<GraphError>> {
    let mut errors = Vec::new();
    let descs = kind.settings();
    for name in raw.keys() {
        if !descs.iter().any(|d| d.name == name) {
            errors.push(GraphError::new(
                Some(id),
                "unknown_setting",
                format!("`{}` has no setting `{name}`", kind.id()),
            ));
        }
    }
    let mut out = BTreeMap::new();
    for d in descs {
        let value = match raw.get(d.name) {
            Some(v) => {
                let mut unknown = Vec::new();
                let r = v.resolve_binds(&|n| params.get(n).cloned(), &mut unknown);
                for u in unknown {
                    errors.push(GraphError::new(
                        Some(id),
                        "unknown_param",
                        format!("setting `{}` binds undeclared parameter `{u}`", d.name),
                    ));
                }
                r
            }
            None => match SettingValue::parse(d.default) {
                Ok(v) => v,
                Err(e) => {
                    errors.push(GraphError::new(
                        Some(id),
                        "setting_type",
                        format!("bad default of `{}`: {e}", d.name),
                    ));
                    continue;
                }
            },
        };
        let value = normalize(d, value);
        if let Err((code, m)) = check_setting(d, &value) {
            errors.push(GraphError::new(
                Some(id),
                code,
                format!("`{}`.{}: {m}", kind.id(), d.name),
            ));
        }
        out.insert(d.name.to_string(), value);
    }
    let settings = Settings(out);
    for (code, m) in kind.validate(&settings) {
        errors.push(GraphError::new(Some(id), &code, m));
    }
    if errors.is_empty() {
        Ok(settings)
    } else {
        Err(errors)
    }
}

/// Canonical forms: enum settings store identifiers; vectors store tuples.
fn normalize(d: &SettingDesc, v: SettingValue) -> SettingValue {
    match (d.ty, v) {
        (SettingType::Enum(_), SettingValue::Str(s)) => SettingValue::Ident(s),
        (SettingType::Vec3, v) => match v.as_vec3() {
            Some(a) => SettingValue::Tuple(a.iter().map(|x| SettingValue::Float(*x)).collect()),
            None => v,
        },
        (_, v) => v,
    }
}

fn in_range(d: &SettingDesc, x: f64) -> bool {
    d.min.is_none_or(|lo| x >= lo) && d.max.is_none_or(|hi| x <= hi)
}

fn range_text(d: &SettingDesc) -> String {
    format!(
        "{}..={}",
        d.min.map_or("-inf".to_string(), |v| v.to_string()),
        d.max.map_or("inf".to_string(), |v| v.to_string())
    )
}

fn check_setting(d: &SettingDesc, v: &SettingValue) -> Result<(), (&'static str, String)> {
    let ty_err = |what: &str| Err(("setting_type", format!("expected {what}, got {}", v.write())));
    let range_err = |x: f64| {
        Err((
            "setting_out_of_range",
            format!("{x} is outside {}", range_text(d)),
        ))
    };
    let check_num = |x: f64| -> Result<(), (&'static str, String)> {
        if !x.is_finite() {
            return Err(("setting_type", format!("{x} is not finite")));
        }
        if !in_range(d, x) {
            return range_err(x);
        }
        Ok(())
    };
    match d.ty {
        SettingType::Float => match v.as_f64() {
            Some(x) => check_num(x),
            None => ty_err("a number"),
        },
        SettingType::Int => match v.as_i64() {
            Some(x) => check_num(x as f64),
            None => ty_err("an integer"),
        },
        SettingType::Bool => match v.as_bool() {
            Some(_) => Ok(()),
            None => ty_err("a boolean"),
        },
        SettingType::Vec3 => match v.as_vec3() {
            Some(a) => a.iter().try_for_each(|x| check_num(*x)),
            None => ty_err("a 3-vector (x, y, z)"),
        },
        SettingType::Range2 => match v.as_items() {
            Some(items) if items.len() == 2 && items.iter().all(|x| x.as_f64().is_some()) => items
                .iter()
                .try_for_each(|x| check_num(x.as_f64().unwrap_or(0.0))),
            _ => ty_err("[lo, hi]"),
        },
        SettingType::Vec3Range => match v.as_items() {
            Some(items) if items.len() == 2 && items.iter().all(|x| x.as_vec3().is_some()) => items
                .iter()
                .filter_map(SettingValue::as_vec3)
                .flatten()
                .try_for_each(check_num),
            _ => ty_err("[(x, y, z), (x, y, z)]"),
        },
        SettingType::Name | SettingType::Asset => match v {
            SettingValue::Str(s) if s.len() <= 256 => Ok(()),
            _ => ty_err("a string of at most 256 bytes"),
        },
        SettingType::Selector => match v {
            SettingValue::Str(s) => Selector::parse(s).map(|_| ()).map_err(|m| ("setting_type", m)),
            _ => ty_err("a selector string"),
        },
        SettingType::Enum(options) => match v.as_text() {
            Some(s) if options.contains(&s) => Ok(()),
            _ => Err((
                "setting_type",
                format!("expected one of {options:?}, got {}", v.write()),
            )),
        },
        SettingType::Curve => {
            let Some(items) = v.as_items() else {
                return ty_err("a curve [(x, y), ...]");
            };
            let mut last = f64::NEG_INFINITY;
            for p in items {
                let xy = p.as_items().filter(|xy| xy.len() == 2);
                let Some((x, y)) = xy.and_then(|xy| Some((xy[0].as_f64()?, xy[1].as_f64()?))) else {
                    return ty_err("a curve [(x, y), ...]");
                };
                if !x.is_finite() || !y.is_finite() || x < last {
                    return Err((
                        "setting_type",
                        "curve keys must be finite and sorted by x".to_string(),
                    ));
                }
                last = x;
            }
            Ok(())
        }
        SettingType::NameList => match v.as_items() {
            Some(items) if items.iter().all(|x| x.as_text().is_some()) => Ok(()),
            _ => ty_err("a list of names"),
        },
        SettingType::Any => Ok(()),
    }
}

/// Edges with reroute nodes collapsed: each edge into a reroute is joined to every edge out of it
/// (transitively); notes have no pins and edges touching them are dropped.
fn collapse_reroutes(g: &ProcGraph) -> Vec<Edge> {
    let is_aid = |id: u32| {
        g.nodes
            .get(&id)
            .is_some_and(|n| EDITOR_AIDS.contains(&n.ty.as_str()))
    };
    let is_reroute = |id: u32| g.nodes.get(&id).is_some_and(|n| n.ty == "reroute");
    let mut out = Vec::new();
    for e in &g.edges {
        if is_aid(e.from.0) {
            continue;
        }
        // Follow reroutes forward from this edge's target.
        let mut frontier = vec![e.to.clone()];
        let mut seen = BTreeSet::new();
        while let Some(to) = frontier.pop() {
            if is_reroute(to.0) {
                if !seen.insert(to.0) {
                    continue;
                }
                for next in g.edges.iter().filter(|x| x.from.0 == to.0) {
                    frontier.push(next.to.clone());
                }
            } else if !is_aid(to.0) {
                out.push(Edge {
                    from: e.from.clone(),
                    to,
                });
            }
        }
    }
    out
}

/// Kahn's algorithm with a min-ordered ready set. On a cycle, returns the smallest node id that
/// lies on one.
fn topo_order(nodes: Vec<u32>, edges: &[(u32, u32)]) -> Result<Vec<u32>, u32> {
    let mut indeg: BTreeMap<u32, usize> = nodes.iter().map(|n| (*n, 0)).collect();
    let mut succ: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for &(a, b) in edges {
        if indeg.contains_key(&a) && indeg.contains_key(&b) {
            *indeg.entry(b).or_default() += 1;
            succ.entry(a).or_default().push(b);
        }
    }
    let mut ready: BTreeSet<u32> = indeg.iter().filter(|(_, d)| **d == 0).map(|(n, _)| *n).collect();
    let mut order = Vec::with_capacity(nodes.len());
    while let Some(n) = ready.pop_first() {
        order.push(n);
        for s in succ.get(&n).into_iter().flatten() {
            if let Some(d) = indeg.get_mut(s) {
                *d -= 1;
                if *d == 0 {
                    ready.insert(*s);
                }
            }
        }
    }
    if order.len() == nodes.len() {
        return Ok(order);
    }
    // Remaining nodes are on or behind a cycle; find one on it: a remaining node reachable from
    // itself through remaining nodes.
    let remaining: BTreeSet<u32> = indeg.iter().filter(|(_, d)| **d > 0).map(|(n, _)| *n).collect();
    for &start in &remaining {
        let mut stack: Vec<u32> = succ.get(&start).cloned().unwrap_or_default();
        let mut seen = BTreeSet::new();
        while let Some(n) = stack.pop() {
            if n == start {
                return Err(start);
            }
            if remaining.contains(&n) && seen.insert(n) {
                stack.extend(succ.get(&n).into_iter().flatten().copied());
            }
        }
    }
    Err(remaining.first().copied().unwrap_or(0))
}

/// One node ready to evaluate.
#[derive(Clone)]
pub struct CompiledNode {
    /// Node id in the file.
    pub id: u32,
    /// The node type.
    pub kind: Arc<dyn NodeKind>,
    /// Resolved settings.
    pub settings: Settings,
    /// Canonical settings text (cache key part).
    pub settings_text: String,
    /// `false` bypasses the node.
    pub enabled: bool,
    /// Seed salt (default: the id).
    pub seed_salt: u32,
    /// The node's pins.
    pub pins: PinSpec,
    /// Per input pin: `(source index in the evaluation order, source output pin)` in edge order.
    pub inputs: Vec<Vec<(usize, usize)>>,
    /// Settings fed by override pins (spec 62 § 4.1), in order: they are the last
    /// `overrides.len()` entries of `pins.inputs` (named `@<setting>`).
    pub overrides: Vec<String>,
}

/// A validated graph in evaluation order (spec 62 § 4.2).
#[derive(Clone)]
pub struct CompiledGraph {
    /// The registry it was compiled with (subgraphs compile with it at run time).
    pub registry: Arc<ProcNodeRegistry>,
    /// Nodes in topological order, ties by ascending id.
    pub nodes: Vec<CompiledNode>,
    /// The graph hash (spec 62 § 6.4).
    pub graph_hash: [u8; 32],
    /// Hash of the effective parameter values.
    pub param_hash: [u8; 32],
    /// Effective parameter values.
    pub params: BTreeMap<String, SettingValue>,
    /// Compile warnings (`param_clamped`, `variant_unknown_param`, `output_unreachable`).
    pub warnings: Vec<GraphError>,
    /// The graph's domain.
    pub domain: Domain,
}

impl std::fmt::Debug for CompiledNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompiledNode")
            .field("id", &self.id)
            .field("ty", &self.kind.id())
            .field("enabled", &self.enabled)
            .field("inputs", &self.inputs)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for CompiledGraph {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompiledGraph")
            .field("nodes", &self.nodes)
            .field("warnings", &self.warnings)
            .field("domain", &self.domain)
            .finish_non_exhaustive()
    }
}

/// A graph variant (`<name>.pgvar.ron`, format `pgvar` v1): a parent graph plus overrides.
#[derive(Clone, Debug, PartialEq)]
pub struct ProcGraphVariant {
    /// Always [`PGVAR_FORMAT_VERSION`] after load.
    pub format_version: u32,
    /// Parent graph or variant path.
    pub parent: String,
    /// Parameter overrides (unknown names are kept and warned about).
    pub overrides: BTreeMap<String, SettingValue>,
}

impl ProcGraphVariant {
    /// Parses a `.pgvar.ron` file.
    ///
    /// # Errors
    ///
    /// [`LoadError`] with `pgvar_parse`, `pgvar_shape` or `pgvar_version_newer`.
    pub fn from_ron(text: &str) -> Result<Self, LoadError> {
        let root = SettingValue::parse(text).map_err(|e| LoadError::new("pgvar_parse", e.to_string()))?;
        let fields = fields_of(&root, "ProcGraphVariant", "pgvar")?;
        let format_version = version_of(fields, "pgvar", PGVAR_FORMAT_VERSION)?;
        let mut v = ProcGraphVariant {
            format_version,
            parent: String::new(),
            overrides: BTreeMap::new(),
        };
        for (key, val) in fields {
            match key.as_str() {
                "format_version" => {}
                "parent" => v.parent = val.as_text().unwrap_or("").to_string(),
                "overrides" => {
                    let SettingValue::Map(entries) = val else {
                        return Err(shape("pgvar", "`overrides` must be a map"));
                    };
                    for (k, x) in entries {
                        let name = k
                            .as_text()
                            .ok_or_else(|| shape("pgvar", "override names are strings"))?;
                        v.overrides.insert(name.to_string(), x.clone());
                    }
                }
                other => return Err(shape("pgvar", format!("unknown field `{other}`"))),
            }
        }
        if v.parent.is_empty() {
            return Err(shape("pgvar", "missing `parent`"));
        }
        Ok(v)
    }

    /// The canonical file text.
    pub fn to_ron(&self) -> String {
        let overrides = SettingValue::Map(
            self.overrides
                .iter()
                .map(|(k, v)| (SettingValue::Str(k.clone()), v.clone()))
                .collect(),
        );
        format!(
            "ProcGraphVariant(format_version: {}, parent: {}, overrides: {})\n",
            self.format_version,
            SettingValue::Str(self.parent.clone()).write(),
            overrides.write()
        )
    }
}

/// A graph or a variant: the one asset type both extensions load as.
#[derive(Clone, Debug, PartialEq)]
pub enum ProcGraphAsset {
    /// A `.pgraph.ron`.
    Graph(ProcGraph),
    /// A `.pgvar.ron`.
    Variant(ProcGraphVariant),
}

impl ProcGraphAsset {
    /// Parses by extension (`.pgvar.ron` → variant, anything else → graph).
    ///
    /// # Errors
    ///
    /// The underlying [`LoadError`].
    pub fn from_ron(path: &str, text: &str) -> Result<Self, LoadError> {
        if path.ends_with(".pgvar.ron") {
            ProcGraphVariant::from_ron(text).map(Self::Variant)
        } else {
            ProcGraph::from_ron(text).map(Self::Graph)
        }
    }
}

/// Where a parameter's effective value came from (`procgen.get`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParamSource {
    /// The generator's own override.
    Generator,
    /// A variant in the chain (its path).
    Variant(String),
    /// The graph default.
    Default,
}

/// A graph with the variant chain's overrides merged, nearest variant first.
#[derive(Clone, Debug)]
pub struct ResolvedGraph {
    /// The root graph.
    pub graph: ProcGraph,
    /// Merged overrides and their sources.
    pub overrides: BTreeMap<String, (SettingValue, ParamSource)>,
    /// Paths of the files read (graph and variants), dependencies of the run.
    pub files: Vec<String>,
}

impl ResolvedGraph {
    /// The overrides as plain values, with the generator's own overrides on top.
    pub fn merged_with(&self, generator: &BTreeMap<String, SettingValue>) -> BTreeMap<String, SettingValue> {
        let mut m: BTreeMap<String, SettingValue> = self
            .overrides
            .iter()
            .map(|(k, (v, _))| (k.clone(), v.clone()))
            .collect();
        for (k, v) in generator {
            m.insert(k.clone(), v.clone());
        }
        m
    }
}

/// Follows a variant chain to its graph (spec 62 § 4): nearest variant's overrides win, chain ≤ 8,
/// cycles refused (`variant_cycle`).
///
/// # Errors
///
/// `procgen_unknown_graph` (a file the loader cannot read), `variant_cycle`, `variant_chain_too_long`,
/// or the file's [`LoadError`].
pub fn resolve_graph(path: &str, load: &dyn Fn(&str) -> Option<String>) -> Result<ResolvedGraph, LoadError> {
    let mut overrides: BTreeMap<String, (SettingValue, ParamSource)> = BTreeMap::new();
    let mut files = Vec::new();
    let mut current = path.to_string();
    loop {
        if files.contains(&current) {
            return Err(LoadError::new(
                "variant_cycle",
                format!("variant chain revisits `{current}`"),
            ));
        }
        if files.len() > MAX_VARIANT_CHAIN {
            return Err(LoadError::new(
                "variant_chain_too_long",
                format!("more than {MAX_VARIANT_CHAIN} variants above `{path}`"),
            ));
        }
        let text = load(&current)
            .ok_or_else(|| LoadError::new("procgen_unknown_graph", format!("cannot read `{current}`")))?;
        files.push(current.clone());
        match ProcGraphAsset::from_ron(&current, &text)? {
            ProcGraphAsset::Graph(graph) => {
                return Ok(ResolvedGraph {
                    graph,
                    overrides,
                    files,
                });
            }
            ProcGraphAsset::Variant(v) => {
                for (k, val) in v.overrides {
                    overrides
                        .entry(k)
                        .or_insert_with(|| (val, ParamSource::Variant(current.clone())));
                }
                current = v.parent;
            }
        }
    }
}

#[cfg(test)]
mod int_exact_tests {
    use super::{ProcGraph, override_value};
    use crate::data::{AttrValue, Bundle, DataItem, Payload, Table};
    use crate::nodes::{ProcNodeRegistry, SettingType};
    use crate::value::SettingValue;

    /// `2^53 + 1`. Exact as `i64`, not as `f64`.
    const PAST_MANTISSA: i64 = 9_007_199_254_740_993;

    fn int_graph(default: i64, min: &str, max: &str) -> ProcGraph {
        let text = format!(
            r#"ProcGraph(
                format_version: 1,
                description: "",
                params: [
                    (name: "token", ty: Int, default: {default}, min: {min}, max: {max}, doc: ""),
                ],
                inputs: [],
                outputs: [],
                nodes: {{}},
                edges: [],
                flags: (starter: false, tool: None, domain: World),
            )"#
        );
        ProcGraph::from_ron(&text).expect("graph text")
    }

    fn compile(g: &ProcGraph) -> Result<super::CompiledGraph, super::GraphErrors> {
        g.compile(&ProcNodeRegistry::with_builtins())
    }

    fn count_override(cell: AttrValue) -> Result<Option<SettingValue>, String> {
        let bundle = Bundle::one(DataItem::new(Payload::Table(Table::one_row([(
            "count".to_string(),
            cell,
        )]))));
        override_value("count", SettingType::Int, &bundle)
    }

    #[test]
    fn int_param_past_f64_mantissa_keeps_every_bit() {
        let g = int_graph(PAST_MANTISSA, "None", "None");
        assert_eq!(
            g.params[0].default,
            SettingValue::Int(PAST_MANTISSA),
            "the file stores the integer exactly"
        );
        let c = compile(&g).expect("compile");
        assert_eq!(c.params["token"], SettingValue::Int(PAST_MANTISSA));
        assert!(
            c.warnings.iter().all(|w| w.code != "param_clamped"),
            "no clamp when min and max are absent: {:?}",
            c.warnings
        );
    }

    #[test]
    fn int_param_bound_outside_i64_is_a_type_mismatch() {
        let g = int_graph(1, "Some(1e20)", "None");
        let err = compile(&g).expect_err("1e20 is not an i64 bound");
        assert!(
            err.has("param_type_mismatch"),
            "expected param_type_mismatch, got {err}"
        );
    }

    #[test]
    fn int_param_still_clamps_inside_i64() {
        let g = int_graph(20, "Some(0.0)", "Some(10.0)");
        let c = compile(&g).expect("compile");
        assert_eq!(c.params["token"], SettingValue::Int(10));
        assert_eq!(
            c.warnings.iter().filter(|w| w.code == "param_clamped").count(),
            1,
            "{:?}",
            c.warnings
        );
    }

    #[test]
    fn override_float_outside_i64_is_rejected_and_exact_values_convert() {
        assert!(
            count_override(AttrValue::Float(1e19)).is_err(),
            "1e19 is not an i64"
        );
        assert!(
            count_override(AttrValue::Float(9_223_372_036_854_775_808.0)).is_err(),
            "2^63 saturates if cast"
        );
        assert_eq!(
            count_override(AttrValue::Float(4.0)).expect("4.0"),
            Some(SettingValue::Int(4))
        );
        assert_eq!(
            count_override(AttrValue::Int(PAST_MANTISSA)).expect("wide int"),
            Some(SettingValue::Int(PAST_MANTISSA))
        );
        assert_eq!(
            count_override(AttrValue::Float(i64::MIN as f64)).expect("i64::MIN"),
            Some(SettingValue::Int(i64::MIN))
        );
        // The last exact `f64` integer below `2^63` (ulp at that magnitude is 2048).
        let just_under = (i64::MAX as f64) - 2048.0;
        assert_eq!(
            count_override(AttrValue::Float(just_under)).expect("just under 2^63"),
            Some(SettingValue::Int(just_under as i64))
        );
        assert!(count_override(AttrValue::Float(-1e19)).is_err(), "-1e19");
        assert!(count_override(AttrValue::Float(f64::NAN)).is_err(), "NaN");
        assert!(
            count_override(AttrValue::Float(f64::INFINITY)).is_err(),
            "infinity"
        );
    }

    #[test]
    fn int_param_fractional_bound_is_a_type_mismatch() {
        let g = int_graph(1, "Some(1.5)", "None");
        let err = compile(&g).expect_err("1.5 is not an integer bound");
        assert!(err.has("param_type_mismatch"), "{err}");
    }

    #[test]
    fn float_param_bound_still_clamps_in_f64() {
        let g = ProcGraph::from_ron(
            r#"ProcGraph(
                format_version: 1,
                description: "",
                params: [
                    (name: "scale", ty: Float, default: 1.0, min: Some(1e20), max: None, doc: ""),
                ],
                inputs: [],
                outputs: [],
                nodes: {},
                edges: [],
                flags: (starter: false, tool: None, domain: World),
            )"#,
        )
        .expect("graph");
        let c = compile(&g).expect("float bounds stay f64");
        assert_eq!(c.params["scale"], SettingValue::Float(1e20));
        assert!(c.warnings.iter().any(|w| w.code == "param_clamped"));
    }
}

#[cfg(test)]
mod int_exact_audfix_tests {
    use std::collections::BTreeMap;

    use super::{GraphErrors, ParamDecl, ProcGraph, override_value};
    use crate::data::{AttrType, AttrValue, Bundle, DataItem, Payload, Table};
    use crate::nodes::{ProcNodeRegistry, SettingType};
    use crate::value::SettingValue;

    fn int_param(default: i64, min: Option<f64>, max: Option<f64>) -> ParamDecl {
        ParamDecl {
            name: "n".into(),
            ty: AttrType::Int,
            default: SettingValue::Int(default),
            min,
            max,
            doc: String::new(),
        }
    }

    fn compiled(
        p: ParamDecl,
        replacement: Option<SettingValue>,
    ) -> Result<super::CompiledGraph, GraphErrors> {
        let mut g = ProcGraph::default();
        g.params.push(p);
        let mut overrides = BTreeMap::new();
        if let Some(v) = replacement {
            overrides.insert("n".to_string(), v);
        }
        g.compile_with(&ProcNodeRegistry::empty(), &overrides)
    }

    #[test]
    fn int_past_2_53_keeps_every_bit() {
        let exact = (1_i64 << 53) + 1;
        let c = compiled(int_param(exact, None, None), None).expect("compile");
        assert_eq!(c.params["n"], SettingValue::Int(exact), "rounded through f64");
        assert!(c.warnings.iter().all(|w| w.code != "param_clamped"));
    }

    #[test]
    fn i64_min_default_survives() {
        let c = compiled(int_param(i64::MIN, None, None), None).expect("compile");
        assert_eq!(c.params["n"], SettingValue::Int(i64::MIN));
    }

    #[test]
    fn int_range_with_no_integer_is_a_type_error() {
        let e = compiled(int_param(0, Some(0.5), Some(0.6)), None).expect_err("empty range");
        assert!(e.has("param_type_mismatch"), "{e}");
    }

    fn one_float(f: f64) -> Bundle {
        Bundle::one(DataItem::new(Payload::Table(Table::one_row([(
            "n".to_string(),
            AttrValue::Float(f),
        )]))))
    }

    #[test]
    fn override_float_must_be_an_exact_i64() {
        assert_eq!(
            override_value("n", SettingType::Int, &one_float(4.0)).expect("4"),
            Some(SettingValue::Int(4))
        );
        let huge = override_value("n", SettingType::Int, &one_float(1.0e20));
        assert!(huge.is_err(), "1e20 saturated: {huge:?}");
        let top = 9223372036854775808.0_f64;
        let sat = override_value("n", SettingType::Int, &one_float(top));
        assert!(sat.is_err(), "2^63 accepted as i64::MAX: {sat:?}");
        let nan = override_value("n", SettingType::Int, &one_float(f64::NAN));
        assert!(nan.is_err(), "{nan:?}");
    }
}

#[cfg(test)]
mod subgraph_capture_tests {
    use super::{NodeEntry, ParamDecl, ProcGraph, collect_subgraphs_with};
    use crate::data::AttrType;
    use crate::value::SettingValue;
    use std::collections::BTreeMap;

    fn leaf() -> String {
        "ProcGraph(format_version: 1)".to_string()
    }

    fn call_node(graph: &str, graphs: SettingValue) -> ProcGraph {
        let mut g = ProcGraph::default();
        let mut node = NodeEntry::new("graph.call");
        node.settings
            .insert("graph".to_string(), SettingValue::Str(graph.to_string()));
        node.settings.insert("graphs".to_string(), graphs);
        g.nodes.insert(1, node);
        g
    }

    #[test]
    fn a_bound_graph_candidate_is_captured() {
        let mut g = call_node(
            "a.pgraph.ron",
            SettingValue::List(vec![SettingValue::Call(
                "Bind".to_string(),
                vec![SettingValue::Str("alt".to_string())],
            )]),
        );
        g.params.push(ParamDecl {
            name: "alt".to_string(),
            ty: AttrType::Asset,
            default: SettingValue::Str("b.pgraph.ron".to_string()),
            min: None,
            max: None,
            doc: String::new(),
        });
        let found = collect_subgraphs_with(&g, &BTreeMap::new(), &|p| {
            (p == "a.pgraph.ron" || p == "b.pgraph.ron").then(leaf)
        });
        assert!(
            found.contains_key("b.pgraph.ron"),
            "bound candidate missing: {:?}",
            found.keys().collect::<Vec<_>>()
        );
        assert!(found.contains_key("a.pgraph.ron"));
    }

    #[test]
    fn the_fallback_graph_is_captured_ahead_of_the_candidate_cap() {
        let graphs = SettingValue::List(
            (0..256)
                .map(|i| SettingValue::Str(format!("c{i}.pgraph.ron")))
                .collect(),
        );
        let g = call_node("fallback.pgraph.ron", graphs);
        let found = collect_subgraphs_with(&g, &BTreeMap::new(), &|_p| Some(leaf()));
        assert!(
            found.contains_key("fallback.pgraph.ron"),
            "fallback dropped, captured {}",
            found.len()
        );
    }
}
