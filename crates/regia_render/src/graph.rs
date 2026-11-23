// SPDX-License-Identifier: MPL-2.0
//! Render graph: versioned resources, transient textures from a pool keyed by [`TextureDesc`],
//! pipeline keys, and topological execution (`specs/13` § 4, ADR-2018).
//!
//! A graph made with [`RenderGraph::new`] orders nodes by resource name: each name has one
//! writer, and a second writer is an error. A graph made with [`RenderGraph::versioned`] takes
//! its nodes in registration order and gives every write a new version of its resource
//! (`color#1`, `color#2`, …): a read binds to the latest version registered before the reader,
//! so read-modify-write passes over one target (the frame's colour chain) are legal, and a pass
//! that overwrites a version before its readers ran is ordered after them. Edges then always
//! point from an earlier node to a later one, so a versioned graph has no cycles and runs in
//! registration order minus the nodes it culls.

mod alias;
mod transient;

pub use alias::{AliasPlan, TransientLifetime};
pub use transient::{TransientEntryInfo, TransientPool, TransientPoolStats, TransientTexture};

use std::collections::{BTreeSet, HashMap, HashSet};
use tracing::info_span;

use crate::TrackedDevice;

/// Named slot where plugins may insert extra passes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GraphSlot {
    /// After the opaque pass.
    AfterOpaque,
    /// After transparent geometry.
    AfterTransparent,
    /// Before post-processing.
    BeforePost,
    /// After post-processing.
    AfterPost,
    /// Final overlay (UI, debug).
    Overlay,
}

/// Descriptor for a transient colour or depth texture: the key of the [`TransientPool`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TextureDesc {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// wgpu format.
    pub format: wgpu::TextureFormat,
    /// Sample count (1 = no MSAA).
    pub sample_count: u32,
    /// Array layers (1 = a plain 2D texture).
    pub layers: u32,
    /// How the texture is used.
    pub usage: wgpu::TextureUsages,
}

impl TextureDesc {
    /// A single-sample, single-layer `width` × `height` texture of `format` used as a render
    /// attachment and sampled.
    #[must_use]
    pub const fn new(width: u32, height: u32, format: wgpu::TextureFormat) -> Self {
        Self {
            width,
            height,
            format,
            sample_count: 1,
            layers: 1,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT.union(wgpu::TextureUsages::TEXTURE_BINDING),
        }
    }

    /// The same description with `usage`.
    #[must_use]
    pub const fn with_usage(mut self, usage: wgpu::TextureUsages) -> Self {
        self.usage = usage;
        self
    }

    /// The same description with `layers` array layers.
    #[must_use]
    pub const fn with_layers(mut self, layers: u32) -> Self {
        self.layers = layers;
        self
    }

    /// The same description with `sample_count` samples.
    #[must_use]
    pub const fn with_samples(mut self, sample_count: u32) -> Self {
        self.sample_count = sample_count;
        self
    }

    /// The wgpu descriptor (one mip, 2D).
    #[must_use]
    pub fn descriptor<'a>(&self, label: Option<&'a str>) -> wgpu::TextureDescriptor<'a> {
        wgpu::TextureDescriptor {
            label,
            size: wgpu::Extent3d {
                width: self.width.max(1),
                height: self.height.max(1),
                depth_or_array_layers: self.layers.max(1),
            },
            mip_level_count: 1,
            sample_count: self.sample_count.max(1),
            dimension: wgpu::TextureDimension::D2,
            format: self.format,
            usage: self.usage,
            view_formats: &[],
        }
    }

    /// Bytes of the texture (every layer and sample).
    #[must_use]
    pub fn bytes(&self) -> u64 {
        crate::frame_stats::texture_bytes(&self.descriptor(None))
    }
}

/// A graph resource.
#[derive(Debug, Clone)]
pub enum GraphResource {
    /// A transient colour or depth texture: [`RenderGraph::execute`] takes it from the
    /// [`TransientPool`] by this description when its first writer runs.
    Texture(TextureDesc),
    /// A resource the caller owns (an imported target, a buffer): the graph orders and culls
    /// by it and never allocates it; [`GraphContext::texture`] finds an imported texture under
    /// its name.
    Imported,
}

/// Output edge: node produces a named resource.
#[derive(Debug, Clone)]
pub struct GraphOutput {
    /// Resource name in the graph.
    pub name: String,
    /// What the resource is.
    pub desc: GraphResource,
}

/// Input edge: node reads a named resource produced earlier.
#[derive(Debug, Clone)]
pub struct GraphInput {
    /// Resource name.
    pub name: String,
}

/// One version of a resource in a compiled graph: `name#version`. Version 0 is the resource
/// as it was before the graph ran (no node of the graph wrote it).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ResourceVersion {
    /// Resource name.
    pub name: String,
    /// Version: the number of writes before (and including) the one that made it.
    pub version: u32,
}

impl std::fmt::Display for ResourceVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}#{}", self.name, self.version)
    }
}

/// What a node's [`RenderGraphNode::record`] may use: the frame's encoder and device, and the
/// textures the graph bound for this run.
pub struct GraphContext<'a> {
    /// The command encoder (do not submit; the caller submits once).
    pub encoder: &'a mut wgpu::CommandEncoder,
    /// The tracked device: creates count in the hitch gate.
    pub device: &'a TrackedDevice,
    /// The caller's frame index.
    pub frame_index: u64,
    /// The node being recorded.
    pub node: &'a str,
    textures: &'a [(String, wgpu::TextureView)],
    imports: &'a [(&'a str, &'a wgpu::TextureView)],
}

impl<'a> GraphContext<'a> {
    /// The texture bound to resource `name` for this run: an imported texture, or the transient
    /// the graph took from its pool. `None` for a name the graph does not hold as a texture.
    /// The view outlives the borrow of the context, so a node can hold it while it records into
    /// [`Self::encoder`].
    #[must_use]
    pub fn texture(&self, name: &str) -> Option<&'a wgpu::TextureView> {
        self.imports
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, view)| *view)
            .or_else(|| {
                self.textures
                    .iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, view)| view)
            })
    }
}

/// A node's recording callback ([`RenderGraphNode::record`]).
pub type GraphRecord = Box<dyn FnMut(&mut GraphContext<'_>) -> Result<(), String> + Send + Sync>;

/// One render-graph node (`specs/13` § 4).
pub struct RenderGraphNode {
    /// Stable node id.
    pub id: String,
    /// Resources this node reads.
    pub inputs: Vec<GraphInput>,
    /// Resources this node writes.
    pub outputs: Vec<GraphOutput>,
    /// Optional plugin slot (for ordering hints).
    pub slot: Option<GraphSlot>,
    /// Keys of the pipelines the node records with: [`RenderGraph::pipeline_keys`] lists the
    /// live nodes' keys so a prewarm builds them before the first frame that runs the node.
    pub pipelines: Vec<&'static str>,
    /// Recording callback. An `Err` skips the node for this run; [`RenderGraph::execute`]
    /// logs and returns it.
    pub record: GraphRecord,
}

/// Why [`RenderGraph::try_compile`] refused a graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphCompileError {
    /// Two nodes write the same resource name, so a reader's producer is ambiguous (a graph
    /// made with [`RenderGraph::new`]; a [`RenderGraph::versioned`] graph versions it).
    DuplicateOutput {
        /// The resource name.
        name: String,
        /// Ids of the nodes that write it, in registration order.
        nodes: Vec<String>,
    },
    /// Live nodes depend on each other in a cycle, so no order runs them all.
    Cycle {
        /// Ids of the live nodes left unordered (the cycle and everything downstream of it), in
        /// registration order.
        nodes: Vec<String>,
    },
    /// Writers of one transient texture declare different descriptions (a versioned graph keeps
    /// one texture per name).
    DescMismatch {
        /// The resource name.
        name: String,
        /// Ids of the nodes that write it, in registration order.
        nodes: Vec<String>,
    },
    /// A declared buffer is read without a preceding writer or an initialized owner import.
    ReadBeforeWrite {
        /// Buffer resource name.
        name: String,
        /// Node that attempted the read.
        node: String,
        /// Owner responsible for allocating and retaining the buffer.
        owner: String,
    },
}

impl std::fmt::Display for GraphCompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReadBeforeWrite { name, node, owner } => write!(
                f,
                "render graph: node `{node}` reads buffer `{name}` owned by `{owner}` before its writer"
            ),
            Self::DuplicateOutput { name, nodes } => {
                write!(
                    f,
                    "render graph: resource `{name}` is written by {}",
                    nodes.join(", ")
                )
            }
            Self::Cycle { nodes } => write!(f, "render graph: dependency cycle among {}", nodes.join(", ")),
            Self::DescMismatch { name, nodes } => write!(
                f,
                "render graph: writers of `{name}` declare different textures: {}",
                nodes.join(", ")
            ),
        }
    }
}

impl std::error::Error for GraphCompileError {}

impl GraphCompileError {
    /// The agent-protocol code: `graph_duplicate_output`, `graph_cycle` or
    /// `graph_desc_mismatch` or `graph_read_before_write`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::ReadBeforeWrite { .. } => "graph_read_before_write",
            Self::DuplicateOutput { .. } => "graph_duplicate_output",
            Self::Cycle { .. } => "graph_cycle",
            Self::DescMismatch { .. } => "graph_desc_mismatch",
        }
    }
}

/// A compiled graph ([`RenderGraph::resolve`]): the order and the resource versions each node
/// reads and writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledGraph {
    /// Live node indices in execution order.
    pub order: Vec<usize>,
    /// Culled node indices (nothing kept reads what they write), in registration order.
    pub culled: Vec<usize>,
    /// Per node (by index), the versions it reads.
    pub reads: Vec<Vec<ResourceVersion>>,
    /// Per node (by index), the versions it writes.
    pub writes: Vec<Vec<ResourceVersion>>,
}

/// What [`RenderGraph::execute`] needs besides the graph.
pub struct GraphExecution<'a> {
    /// The tracked device (transient creates count in the hitch gate).
    pub device: &'a TrackedDevice,
    /// The command encoder every node records into.
    pub encoder: &'a mut wgpu::CommandEncoder,
    /// Where transient textures come from. The caller starts the pool frame
    /// ([`TransientPool::begin_frame`]).
    pub pool: &'a mut TransientPool,
    /// Textures the caller owns, by resource name ([`GraphResource::Imported`]).
    pub imports: &'a [(&'a str, &'a wgpu::TextureView)],
    /// The caller's frame index.
    pub frame_index: u64,
}

/// [`RenderGraph::execute`]'s result. The run records its live nodes in compiled order on the
/// caller's one command encoder. Each node is wrapped in the CPU span `render_graph_node`.
/// The graph writes no GPU timestamp.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GraphRun {
    /// Ids of the nodes that recorded, in order.
    pub recorded: Vec<String>,
    /// Ids and messages of the nodes whose `record` returned an error (skipped).
    pub failed: Vec<(String, String)>,
    /// Transient textures the run bound.
    pub transients: usize,
}

/// CPU-side render graph builder and executor.
pub struct RenderGraph {
    nodes: Vec<RenderGraphNode>,
    versioned: bool,
    /// Breadcrumb tags of the node ids by node index (spec 54 § 2.2), interned when the graph
    /// compiles for its first run with breadcrumbs on; nodes are only ever appended.
    tags: Vec<crate::breadcrumbs::PassTag>,
    /// Explicit buffer owners and whether version zero is supplied by that owner.
    buffers: HashMap<String, (String, bool)>,
}

impl RenderGraph {
    /// Creates an empty graph ordered by resource name (one writer per name).
    #[must_use]
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            versioned: false,
            tags: Vec::new(),
            buffers: HashMap::new(),
        }
    }

    /// Creates an empty versioned graph: every write makes a new version of its resource and a
    /// read binds to the latest version registered before the reader (module docs).
    #[must_use]
    pub fn versioned() -> Self {
        Self {
            nodes: Vec::new(),
            versioned: true,
            tags: Vec::new(),
            buffers: HashMap::new(),
        }
    }

    /// The graph versions its resources ([`Self::versioned`]).
    #[must_use]
    pub fn is_versioned(&self) -> bool {
        self.versioned
    }

    /// Declares an externally owned persistent buffer (including CPU lists).
    ///
    /// `initialized` permits version-zero reads: the owner supplies uploads or previous-frame
    /// history before graph execution. Otherwise a node must write the buffer before any read
    /// in a versioned graph. Non-versioned graphs order the writer before readers as usual.
    /// Buffer allocation and binding remain with `owner`; this does not allocate a texture or
    /// make the buffer eligible for texture aliasing. Redeclaring a name replaces its contract.
    pub fn declare_buffer(&mut self, name: &str, owner: &str, initialized: bool) {
        self.buffers
            .insert(name.to_owned(), (owner.to_owned(), initialized));
    }

    /// Registers a node. Returns the node id.
    pub fn add_node(&mut self, node: RenderGraphNode) -> String {
        let id = node.id.clone();
        self.nodes.push(node);
        id
    }

    /// The registered nodes in registration order.
    #[must_use]
    pub fn nodes(&self) -> &[RenderGraphNode] {
        &self.nodes
    }

    /// The final version of resource `name`: the number of nodes that write it in a versioned
    /// graph; 1 in an unversioned graph when a node writes it. 0 when no node writes it.
    #[must_use]
    pub fn latest_version(&self, name: &str) -> u32 {
        let writes = self
            .nodes
            .iter()
            .flat_map(|node| node.outputs.iter())
            .filter(|out| out.name == name)
            .count();
        if self.versioned {
            u32::try_from(writes).unwrap_or(u32::MAX)
        } else {
            u32::from(writes > 0)
        }
    }

    /// Topologically sorts nodes; culls nodes whose outputs are never consumed unless they are
    /// marked as graph outputs via `keep_outputs`.
    ///
    /// Returns node indices in execution order. Independent nodes run in registration order,
    /// so the order is the same on every call. A graph [`Self::try_compile`] refuses logs an
    /// error and returns the nodes that can be ordered: a node inside a dependency cycle (or
    /// downstream of one) is left out, and a resource written twice takes its last writer.
    #[must_use]
    pub fn compile(&self, keep_outputs: &[&str]) -> Vec<usize> {
        let (order, error) = self.order(keep_outputs);
        if let Some(err) = error {
            tracing::error!(target: "regia_render", "{err}");
        }
        order
    }

    /// [`Self::compile`], refusing a graph whose order is ambiguous or impossible: a resource
    /// written by two nodes, or live nodes in a dependency cycle.
    ///
    /// # Errors
    ///
    /// [`GraphCompileError::DuplicateOutput`] or [`GraphCompileError::Cycle`] (unversioned
    /// graphs), [`GraphCompileError::DescMismatch`] (versioned graphs), or
    /// [`GraphCompileError::ReadBeforeWrite`] for a declared buffer without initialized contents.
    pub fn try_compile(&self, keep_outputs: &[&str]) -> Result<Vec<usize>, GraphCompileError> {
        match self.order(keep_outputs) {
            (order, None) => Ok(order),
            (_, Some(err)) => Err(err),
        }
    }

    /// [`Self::try_compile`] with the culled nodes and the resource versions every node reads
    /// and writes (`render.graph` lists them).
    ///
    /// # Errors
    ///
    /// As [`Self::try_compile`].
    pub fn resolve(&self, keep_outputs: &[&str]) -> Result<CompiledGraph, GraphCompileError> {
        let order = self.try_compile(keep_outputs)?;
        let live: HashSet<usize> = order.iter().copied().collect();
        let culled = (0..self.nodes.len()).filter(|i| !live.contains(i)).collect();
        let (reads, writes) = self.versions();
        Ok(CompiledGraph {
            order,
            culled,
            reads,
            writes,
        })
    }

    /// Per node, the resource versions it reads and writes (registration order; an
    /// unversioned graph reads and writes version 1, or 0 for a name no node writes).
    fn versions(&self) -> (Vec<Vec<ResourceVersion>>, Vec<Vec<ResourceVersion>>) {
        let mut latest: HashMap<&str, u32> = HashMap::new();
        if !self.versioned {
            for node in &self.nodes {
                for out in &node.outputs {
                    latest.insert(out.name.as_str(), 1);
                }
            }
        }
        let mut reads = Vec::with_capacity(self.nodes.len());
        let mut writes = Vec::with_capacity(self.nodes.len());
        for node in &self.nodes {
            reads.push(
                node.inputs
                    .iter()
                    .map(|input| ResourceVersion {
                        name: input.name.clone(),
                        version: latest.get(input.name.as_str()).copied().unwrap_or(0),
                    })
                    .collect(),
            );
            writes.push(
                node.outputs
                    .iter()
                    .map(|out| {
                        let version = if self.versioned {
                            let v = latest.entry(out.name.as_str()).or_insert(0);
                            *v += 1;
                            *v
                        } else {
                            1
                        };
                        ResourceVersion {
                            name: out.name.clone(),
                            version,
                        }
                    })
                    .collect(),
            );
        }
        (reads, writes)
    }

    /// The pipeline keys of the live nodes in execution order, each once (the prewarm builds
    /// them; [`RenderGraphNode::pipelines`]).
    #[must_use]
    pub fn pipeline_keys(&self, keep_outputs: &[&str]) -> Vec<&'static str> {
        let mut keys: Vec<&'static str> = Vec::new();
        for idx in self.compile(keep_outputs) {
            for key in &self.nodes[idx].pipelines {
                if !keys.contains(key) {
                    keys.push(key);
                }
            }
        }
        keys
    }

    /// The execution order and the first problem found.
    fn order(&self, keep_outputs: &[&str]) -> (Vec<usize>, Option<GraphCompileError>) {
        if !self.buffers.is_empty() {
            let mut written: HashSet<&str> = HashSet::new();
            if !self.versioned {
                written.extend(
                    self.nodes
                        .iter()
                        .flat_map(|node| node.outputs.iter().map(|out| out.name.as_str())),
                );
            }
            for node in &self.nodes {
                for input in &node.inputs {
                    if let Some((owner, false)) = self.buffers.get(&input.name)
                        && !written.contains(input.name.as_str())
                    {
                        return (
                            Vec::new(),
                            Some(GraphCompileError::ReadBeforeWrite {
                                name: input.name.clone(),
                                node: node.id.clone(),
                                owner: owner.clone(),
                            }),
                        );
                    }
                }
                written.extend(node.outputs.iter().map(|out| out.name.as_str()));
            }
        }
        if self.versioned {
            self.order_versioned(keep_outputs)
        } else {
            self.order_by_name(keep_outputs)
        }
    }

    /// A versioned graph's order: registration order minus the culled nodes (every edge points
    /// forward). A node is live when it writes a version a live node or `keep_outputs` (the
    /// final versions) reads, or when it writes nothing.
    fn order_versioned(&self, keep_outputs: &[&str]) -> (Vec<usize>, Option<GraphCompileError>) {
        // Producer of each (name, version); the transient description of each name.
        let mut latest: HashMap<&str, (usize, u32)> = HashMap::new();
        let mut descs: HashMap<&str, (&TextureDesc, usize)> = HashMap::new();
        let mut error: Option<GraphCompileError> = None;
        // Per node: the producers of the versions it reads.
        let mut sources: Vec<Vec<usize>> = Vec::with_capacity(self.nodes.len());
        for (i, node) in self.nodes.iter().enumerate() {
            let mut from = Vec::new();
            for input in &node.inputs {
                if let Some(&(producer, _)) = latest.get(input.name.as_str()) {
                    from.push(producer);
                }
            }
            sources.push(from);
            for out in &node.outputs {
                let version = latest.get(out.name.as_str()).map_or(1, |&(_, v)| v + 1);
                latest.insert(out.name.as_str(), (i, version));
                if let GraphResource::Texture(desc) = &out.desc {
                    match descs.get(out.name.as_str()) {
                        Some((first, _)) if *first != desc && error.is_none() => {
                            let nodes = self
                                .nodes
                                .iter()
                                .filter(|n| n.outputs.iter().any(|o| o.name == out.name))
                                .map(|n| n.id.clone())
                                .collect();
                            error = Some(GraphCompileError::DescMismatch {
                                name: out.name.clone(),
                                nodes,
                            });
                        }
                        Some(_) => {}
                        None => {
                            descs.insert(out.name.as_str(), (desc, i));
                        }
                    }
                }
            }
        }
        let mut live = vec![false; self.nodes.len()];
        let mut stack: Vec<usize> = keep_outputs
            .iter()
            .filter_map(|name| latest.get(name).map(|&(producer, _)| producer))
            .collect();
        stack.extend(
            self.nodes
                .iter()
                .enumerate()
                .filter(|(_, n)| n.outputs.is_empty())
                .map(|(i, _)| i),
        );
        while let Some(i) = stack.pop() {
            if live[i] {
                continue;
            }
            live[i] = true;
            stack.extend(sources[i].iter().copied());
        }
        let order = (0..self.nodes.len()).filter(|&i| live[i]).collect();
        (order, error)
    }

    /// An unversioned graph's order (one writer per name; registration order breaks ties).
    fn order_by_name(&self, keep_outputs: &[&str]) -> (Vec<usize>, Option<GraphCompileError>) {
        let mut producers: HashMap<String, usize> = HashMap::new();
        let mut duplicate: Option<GraphCompileError> = None;
        for (i, node) in self.nodes.iter().enumerate() {
            for out in &node.outputs {
                if let Some(previous) = producers.insert(out.name.clone(), i)
                    && previous != i
                    && duplicate.is_none()
                {
                    let nodes = self
                        .nodes
                        .iter()
                        .filter(|n| n.outputs.iter().any(|o| o.name == out.name))
                        .map(|n| n.id.clone())
                        .collect();
                    duplicate = Some(GraphCompileError::DuplicateOutput {
                        name: out.name.clone(),
                        nodes,
                    });
                }
            }
        }

        let mut needed: HashSet<String> = keep_outputs.iter().map(|s| (*s).to_string()).collect();
        let mut changed = true;
        while changed {
            changed = false;
            let snapshot = needed.clone();
            for name in snapshot {
                if let Some(&idx) = producers.get(&name) {
                    for input in &self.nodes[idx].inputs {
                        if needed.insert(input.name.clone()) {
                            changed = true;
                        }
                    }
                }
            }
        }

        let live: HashSet<usize> = self
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| n.outputs.iter().any(|o| needed.contains(&o.name)) || n.outputs.is_empty())
            .map(|(i, _)| i)
            .collect();

        let mut in_degree: HashMap<usize, u32> = HashMap::new();
        let mut edges: HashMap<usize, Vec<usize>> = HashMap::new();
        for &consumer in &live {
            for input in &self.nodes[consumer].inputs {
                if let Some(&producer) = producers.get(&input.name)
                    && live.contains(&producer)
                {
                    edges.entry(producer).or_default().push(consumer);
                    in_degree.entry(consumer).and_modify(|d| *d += 1).or_insert(1);
                }
            }
            in_degree.entry(consumer).or_insert(0);
        }

        // Kahn's algorithm with the ready set ordered by node index: `HashMap` iteration order
        // differs between runs, and independent nodes must not swap places from frame to frame.
        let mut ready: BTreeSet<usize> = in_degree
            .iter()
            .filter(|(_, d)| **d == 0)
            .map(|(&i, _)| i)
            .collect();
        let mut order = Vec::new();
        while let Some(n) = ready.pop_first() {
            order.push(n);
            if let Some(children) = edges.get(&n) {
                for &c in children {
                    if let Some(deg) = in_degree.get_mut(&c) {
                        *deg = deg.saturating_sub(1);
                        if *deg == 0 {
                            ready.insert(c);
                        }
                    }
                }
            }
        }
        if duplicate.is_some() {
            return (order, duplicate);
        }
        if order.len() < live.len() {
            let ordered: HashSet<usize> = order.iter().copied().collect();
            let mut left: Vec<usize> = live.iter().copied().filter(|i| !ordered.contains(i)).collect();
            left.sort_unstable();
            let nodes = left.into_iter().map(|i| self.nodes[i].id.clone()).collect();
            return (order, Some(GraphCompileError::Cycle { nodes }));
        }
        (order, None)
    }

    /// Records the live nodes in compiled order ([`Self::compile`]: a node in a dependency
    /// cycle is skipped and an error is logged). Each transient texture
    /// ([`GraphResource::Texture`]) of a live node is taken from `exec.pool` before its first
    /// writer records, one texture per resource name, and stays bound for the rest of the run;
    /// imported names resolve to `exec.imports`.
    ///
    /// With [`TransientPool::set_aliasing`] on, a transient is released after its last reader and
    /// the next transient of that description reuses its texture ([`Self::alias_plan`]; a released
    /// name no longer resolves in [`GraphContext::texture`]). A node that fails releases what it was
    /// the first to write, so its readers find no texture instead of another resource's pixels.
    ///
    /// Each live node runs inside one GPU breadcrumb scope named by its id (spec 54 § 3,
    /// ADR-9070), on the calling thread's list: the next tracked submit (or
    /// [`crate::breadcrumbs::note_untracked_submit`]) publishes it. The ids are interned once,
    /// when the graph compiles for its first run with breadcrumbs on; a node that fails still
    /// closes its scope. With breadcrumbs off a node costs one relaxed load.
    pub fn execute(&mut self, exec: GraphExecution<'_>, keep_outputs: &[&str]) -> GraphRun {
        let order = self.compile(keep_outputs);
        let crumbs = crate::breadcrumbs::enabled();
        if crumbs {
            self.intern_node_tags();
        }
        let GraphExecution {
            device,
            encoder,
            pool,
            imports,
            frame_index,
        } = exec;
        let plan = pool
            .aliasing()
            .then(|| self.alias_plan(keep_outputs).ok())
            .flatten();
        let mut textures: Vec<(String, wgpu::TextureView)> = Vec::new();
        // With aliasing on: the generation of each bound transient, and the textures of the ones
        // that keep their own. Those are taken first, so a released texture is never handed to
        // them and they find what the last frame left; they bind at their first writer, as with
        // aliasing off.
        let mut generations: Vec<(String, u64)> = Vec::new();
        let mut reserved: Vec<(String, TransientTexture)> = Vec::new();
        if let Some(plan) = plan.as_ref() {
            for held in plan.lifetimes.iter().filter(|l| !l.aliasable) {
                if !imports.iter().any(|(name, _)| *name == held.name) {
                    let texture = pool.acquire(device, &held.desc, &held.name);
                    reserved.push((held.name.clone(), texture));
                }
            }
        }
        let mut run = GraphRun::default();
        for (step, idx) in order.into_iter().enumerate() {
            let tag = if crumbs { self.tags.get(idx).copied() } else { None };
            let node = &mut self.nodes[idx];
            for out in &node.outputs {
                if let GraphResource::Texture(desc) = &out.desc
                    && !imports.iter().any(|(name, _)| *name == out.name)
                    && !textures.iter().any(|(name, _)| *name == out.name)
                {
                    let texture = match reserved.iter().position(|(name, _)| *name == out.name) {
                        Some(at) => reserved.swap_remove(at).1,
                        None => pool.acquire(device, desc, &out.name),
                    };
                    if plan.is_some() {
                        generations.push((out.name.clone(), texture.generation));
                    }
                    textures.push((out.name.clone(), texture.view));
                    run.transients += 1;
                }
            }
            let span = info_span!("render_graph_node", node = %node.id);
            let _guard = span.enter();
            let _crumb = tag.map(crate::breadcrumbs::frame_pass_tag);
            let mut ctx = GraphContext {
                encoder: &mut *encoder,
                device,
                frame_index,
                node: &node.id,
                textures: &textures,
                imports,
            };
            let failed = match (node.record)(&mut ctx) {
                Ok(()) => {
                    run.recorded.push(node.id.clone());
                    false
                }
                Err(err) => {
                    tracing::warn!(target: "regia_render", "render graph node `{}` skipped: {err}", node.id);
                    run.failed.push((node.id.clone(), err));
                    true
                }
            };
            if let Some(plan) = plan.as_ref() {
                let over = plan
                    .lifetimes
                    .iter()
                    .filter(|l| l.aliasable && (l.last == step || (failed && l.first == step)));
                for over in over {
                    if let Some(at) = generations.iter().position(|(name, _)| *name == over.name) {
                        pool.release(generations.swap_remove(at).1);
                        textures.retain(|(name, _)| *name != over.name);
                    }
                }
            }
        }
        run
    }
}

impl RenderGraph {
    /// Interns the ids of the nodes added since the last call (spec 54 § 2.2: graph node ids are
    /// interned when the graph compiles), so a steady run takes no name-table lock.
    fn intern_node_tags(&mut self) {
        for node in &self.nodes[self.tags.len()..] {
            self.tags.push(crate::breadcrumbs::intern(&node.id));
        }
    }
}

impl Default for RenderGraph {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graph_buffers_require_explicit_imports_and_preserve_history_versions() {
        let mut graph = RenderGraph::versioned();
        graph.declare_buffer("history", "sampler", true);
        graph.declare_buffer("current", "sampler", false);
        graph.add_node(keyed("sample", &["history", "legacy_input"], &["current"], &[]));
        graph.add_node(keyed("shade", &["current"], &["output"], &[]));
        let compiled = graph.resolve(&["output"]).expect("buffer audit");
        assert_eq!(compiled.order, vec![0, 1]);
        assert_eq!(compiled.reads[0], vec![v("history", 0), v("legacy_input", 0)]);
        assert_eq!(compiled.reads[1], vec![v("current", 1)]);
        graph.declare_buffer("history", "sampler", false);
        assert_eq!(
            graph.resolve(&["output"]).expect_err("invalid buffer order"),
            GraphCompileError::ReadBeforeWrite {
                name: "history".into(),
                node: "sample".into(),
                owner: "sampler".into(),
            }
        );
    }

    #[test]
    fn graph_buffers_validate_dead_readers_and_unversioned_writers() {
        let mut graph = RenderGraph::versioned();
        graph.declare_buffer("missing", "renderer", false);
        graph.add_node(keyed("dead", &["missing"], &["unused"], &[]));
        assert_eq!(
            graph.resolve(&[]).expect_err("invalid buffer order").code(),
            "graph_read_before_write"
        );
        let mut graph = RenderGraph::new();
        graph.declare_buffer("buffer", "renderer", false);
        graph.add_node(keyed("reader", &["buffer"], &["output"], &[]));
        graph.add_node(keyed("writer", &[], &["buffer"], &[]));
        assert_eq!(
            graph.resolve(&["output"]).expect("buffer audit").order,
            vec![1, 0]
        );
    }

    #[test]
    fn culls_unused_branch() {
        let mut g = RenderGraph::new();
        g.add_node(RenderGraphNode {
            id: "shadow".into(),
            inputs: vec![],
            outputs: vec![GraphOutput {
                name: "shadow".into(),
                desc: GraphResource::Texture(TextureDesc::new(512, 512, wgpu::TextureFormat::Depth32Float)),
            }],
            slot: None,
            pipelines: vec![],
            record: Box::new(|_| Ok(())),
        });
        g.add_node(RenderGraphNode {
            id: "color".into(),
            inputs: vec![GraphInput {
                name: "shadow".into(),
            }],
            outputs: vec![GraphOutput {
                name: "hdr".into(),
                desc: GraphResource::Texture(TextureDesc::new(64, 64, wgpu::TextureFormat::Rgba16Float)),
            }],
            slot: None,
            pipelines: vec![],
            record: Box::new(|_| Ok(())),
        });
        g.add_node(RenderGraphNode {
            id: "dead".into(),
            inputs: vec![],
            outputs: vec![GraphOutput {
                name: "unused".into(),
                desc: GraphResource::Texture(TextureDesc::new(1, 1, wgpu::TextureFormat::Rgba8Unorm)),
            }],
            slot: None,
            pipelines: vec![],
            record: Box::new(|_| Ok(())),
        });
        let order = g.compile(&["hdr"]);
        assert_eq!(order.len(), 2);
        assert_eq!(g.nodes[order[0]].id, "shadow");
        assert_eq!(g.nodes[order[1]].id, "color");
        assert_eq!(g.try_compile(&["hdr"]), Ok(order));
    }

    fn node(id: &str, inputs: &[&str], outputs: &[&str]) -> RenderGraphNode {
        RenderGraphNode {
            id: id.into(),
            inputs: inputs
                .iter()
                .map(|name| GraphInput { name: (*name).into() })
                .collect(),
            outputs: outputs
                .iter()
                .map(|name| GraphOutput {
                    name: (*name).into(),
                    desc: GraphResource::Texture(TextureDesc::new(8, 8, wgpu::TextureFormat::Rgba8Unorm)),
                })
                .collect(),
            slot: None,
            pipelines: vec![],
            record: Box::new(|_| Ok(())),
        }
    }

    /// Two live nodes compile to one sequence. [`RenderGraph::execute`] records that sequence
    /// on the caller's one encoder and writes no GPU timestamp.
    #[test]
    fn two_live_nodes_compile_to_one_sequence() {
        let mut g = RenderGraph::new();
        g.add_node(node("first", &[], &["a"]));
        g.add_node(node("second", &["a"], &["b"]));
        let order = g.compile(&["b"]);
        assert_eq!(order.len(), 2);
        assert_eq!(g.nodes[order[0]].id, "first");
        assert_eq!(g.nodes[order[1]].id, "second");
    }

    /// A cycle used to drop its nodes with no error (even a node named in `keep_outputs`).
    #[test]
    fn a_cycle_is_an_error_naming_its_nodes() {
        let mut g = RenderGraph::new();
        g.add_node(node("source", &[], &["a"]));
        g.add_node(node("left", &["a", "c"], &["b"]));
        g.add_node(node("right", &["b"], &["c"]));
        g.add_node(node("final", &["c"], &["out"]));
        assert_eq!(
            g.try_compile(&["out"]),
            Err(GraphCompileError::Cycle {
                nodes: vec!["left".into(), "right".into(), "final".into()],
            })
        );
        // `compile` still returns what can run: the node outside the cycle.
        let order = g.compile(&["out"]);
        assert_eq!(order, vec![0]);
        let message = g.try_compile(&["out"]).expect_err("cycle").to_string();
        assert!(message.contains("cycle") && message.contains("left"), "{message}");
    }

    #[test]
    fn two_writers_of_one_resource_are_an_error() {
        let mut g = RenderGraph::new();
        g.add_node(node("first", &[], &["hdr"]));
        g.add_node(node("second", &[], &["hdr"]));
        g.add_node(node("post", &["hdr"], &["ldr"]));
        assert_eq!(
            g.try_compile(&["ldr"]),
            Err(GraphCompileError::DuplicateOutput {
                name: "hdr".into(),
                nodes: vec!["first".into(), "second".into()],
            })
        );
    }

    /// Independent nodes run in registration order on every call (the ready set used to come
    /// from `HashMap` iteration, which differs between runs).
    #[test]
    fn independent_nodes_run_in_registration_order() {
        let mut g = RenderGraph::new();
        let names: Vec<String> = (0..24).map(|i| format!("n{i}")).collect();
        for name in &names {
            g.add_node(node(name, &[], &[name.as_str()]));
        }
        g.add_node(node(
            "join",
            &names.iter().map(String::as_str).collect::<Vec<_>>(),
            &["out"],
        ));
        let expected: Vec<usize> = (0..=24).collect();
        for _ in 0..8 {
            assert_eq!(g.try_compile(&["out"]), Ok(expected.clone()));
        }
    }

    fn keyed(id: &str, inputs: &[&str], outputs: &[&str], pipelines: &[&'static str]) -> RenderGraphNode {
        let mut n = node(id, inputs, outputs);
        for out in &mut n.outputs {
            out.desc = GraphResource::Imported;
        }
        n.pipelines = pipelines.to_vec();
        n
    }

    fn v(name: &str, version: u32) -> ResourceVersion {
        ResourceVersion {
            name: name.into(),
            version,
        }
    }

    /// A versioned graph runs a read-modify-write chain over one target in registration order
    /// and culls the pass whose version nothing reads (the frame's colour chain, ADR-2018).
    #[test]
    fn versioned_chain_runs_in_registration_order_and_culls_dead_versions() {
        let mut g = RenderGraph::versioned();
        g.add_node(keyed("forward", &["depth"], &["color"], &[]));
        g.add_node(keyed("debug_mask", &["color"], &["mask"], &[]));
        g.add_node(keyed("fog", &["color", "depth"], &["color"], &[]));
        g.add_node(keyed("taa", &["color"], &["color"], &[]));
        g.add_node(keyed("post", &["color"], &["output"], &[]));
        let compiled = g.resolve(&["output"]).expect("versioned chain compiles");
        assert_eq!(compiled.order, vec![0, 2, 3, 4]);
        assert_eq!(compiled.culled, vec![1]);
        assert_eq!(compiled.reads[2], vec![v("color", 1), v("depth", 0)]);
        assert_eq!(compiled.writes[2], vec![v("color", 2)]);
        assert_eq!(compiled.reads[4], vec![v("color", 3)]);
        assert_eq!(g.latest_version("color"), 3);
        assert_eq!(v("color", 3).to_string(), "color#3");
        // The same names in an unversioned graph are three writers of `color`.
        let mut flat = RenderGraph::new();
        flat.add_node(keyed("forward", &["depth"], &["color"], &[]));
        flat.add_node(keyed("fog", &["color"], &["color"], &[]));
        assert_eq!(
            flat.try_compile(&["color"]).map_err(|e| e.code()),
            Err("graph_duplicate_output")
        );
    }

    /// A read binds to the latest version registered before the reader, not to a later writer,
    /// and keeping an older version alive keeps its writer.
    #[test]
    fn versioned_reads_bind_to_the_version_before_the_reader() {
        let mut g = RenderGraph::versioned();
        g.add_node(keyed("a", &[], &["hdr"], &[]));
        g.add_node(keyed("probe", &["hdr"], &["probe_out"], &[]));
        g.add_node(keyed("b", &["hdr"], &["hdr"], &[]));
        let compiled = g.resolve(&["probe_out", "hdr"]).expect("compiles");
        assert_eq!(compiled.order, vec![0, 1, 2]);
        assert_eq!(compiled.reads[1], vec![v("hdr", 1)]);
        assert_eq!(compiled.writes[2], vec![v("hdr", 2)]);
        // Keeping only the probe output culls the later writer.
        assert_eq!(g.try_compile(&["probe_out"]), Ok(vec![0, 1]));
        // A node that writes nothing always runs and keeps what it reads.
        g.add_node(keyed("present", &["hdr"], &[], &[]));
        assert_eq!(g.try_compile(&[]), Ok(vec![0, 2, 3]));
    }

    #[test]
    fn versioned_writers_of_one_transient_must_agree_on_its_desc() {
        let mut g = RenderGraph::versioned();
        g.add_node(node("a", &[], &["hdr"]));
        let mut b = node("b", &["hdr"], &["hdr"]);
        b.outputs[0].desc = GraphResource::Texture(TextureDesc::new(16, 8, wgpu::TextureFormat::Rgba8Unorm));
        g.add_node(b);
        let err = g.try_compile(&["hdr"]).expect_err("two descs for one name");
        assert_eq!(err.code(), "graph_desc_mismatch");
        assert_eq!(
            err,
            GraphCompileError::DescMismatch {
                name: "hdr".into(),
                nodes: vec!["a".into(), "b".into()],
            }
        );
        assert!(err.to_string().contains("`hdr`"), "{err}");
    }

    /// The prewarm reads the live nodes' pipeline keys, each once, in execution order; a culled
    /// node's keys are not asked for.
    #[test]
    fn pipeline_keys_list_the_live_nodes_keys_once() {
        let mut g = RenderGraph::versioned();
        g.add_node(keyed("ssao", &["depth"], &["ao"], &["ssao", "ssao_blur"]));
        g.add_node(keyed("dead", &[], &["unused"], &["never"]));
        g.add_node(keyed("forward", &["ao"], &["color"], &["forward", "ssao_blur"]));
        assert_eq!(g.pipeline_keys(&["color"]), vec!["ssao", "ssao_blur", "forward"]);
    }

    /// Spec 54 § 3 (ADR-9070): `execute` opens one breadcrumb scope per live node, in compiled
    /// order (a failing node's attempt included), named by the node id interned when the graph
    /// compiles. A culled node opens none. The scopes go on the calling thread's list, which the
    /// next tracked submit publishes as one submission.
    #[test]
    fn execute_opens_one_breadcrumb_scope_per_node_in_compiled_order() {
        let device = match crate::RenderDevice::shared_headless() {
            Ok(device) => device,
            Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
        };
        let tracked = TrackedDevice::observe(&device.device);
        let mut g = RenderGraph::new();
        // Registered out of dependency order: the compiled order is a, b, c; `dead` is culled.
        g.add_node(keyed("crumb_node_c", &["crumb_b"], &["crumb_out"], &[]));
        g.add_node(keyed("crumb_node_a", &[], &["crumb_a"], &[]));
        g.add_node(keyed("crumb_node_dead", &[], &["crumb_unused"], &[]));
        g.add_node(keyed("crumb_node_b", &["crumb_a"], &["crumb_b"], &[]));
        g.nodes[3].record = Box::new(|_| Err("refused on purpose".into()));
        let out = device.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("crumb_out"),
            size: wgpu::Extent3d {
                width: 4,
                height: 4,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let out_view = out.create_view(&wgpu::TextureViewDescriptor::default());
        let imports = [
            ("crumb_out", &out_view),
            ("crumb_a", &out_view),
            ("crumb_b", &out_view),
            ("crumb_unused", &out_view),
        ];
        let mut pool = super::TransientPool::default();
        pool.begin_frame(1);
        crate::breadcrumbs::begin_frame(0x5eed_9070);
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        let run = g.execute(
            GraphExecution {
                device: &tracked,
                encoder: &mut encoder,
                pool: &mut pool,
                imports: &imports,
                frame_index: 1,
            },
            &["crumb_out"],
        );
        assert_eq!(
            run.recorded,
            vec!["crumb_node_a".to_string(), "crumb_node_c".to_string()]
        );
        assert_eq!(run.failed.len(), 1, "b failed on purpose");
        // The tracked queue's submit path: the thread's markers become one submission. It is read
        // back by its id, before the submit: the frame number is process-wide, so another test's
        // frame start between `begin_frame` above and here files it under that frame instead, and
        // the ring keeps only the newest frames (rev-claude-60, the pregate race).
        let (id, done) = crate::breadcrumbs::on_submit_with_id().expect("breadcrumbs are on in tests");
        let snapshot = crate::breadcrumbs::snapshot();
        drop(done);
        device.queue.submit([encoder.finish()]);
        let ours: Vec<(String, bool)> = snapshot
            .frames
            .iter()
            .find_map(|f| {
                let range = f.submissions.iter().find(|r| r.id == id)?;
                Some(
                    f.markers[range.start..range.end]
                        .iter()
                        .map(|m| (snapshot.names[usize::from(m.tag.0)].clone(), m.open))
                        .filter(|(name, _)| name.starts_with("crumb_node_"))
                        .collect(),
                )
            })
            .unwrap_or_default();
        let expected: Vec<(String, bool)> = ["crumb_node_a", "crumb_node_b", "crumb_node_c"]
            .iter()
            .flat_map(|n| [((*n).to_string(), true), ((*n).to_string(), false)])
            .collect();
        assert_eq!(
            ours, expected,
            "one open/close pair per live node, in compiled order"
        );
    }

    #[test]
    fn texture_desc_builders_and_bytes() {
        let desc = TextureDesc::new(4, 2, wgpu::TextureFormat::Rgba16Float)
            .with_layers(2)
            .with_samples(1)
            .with_usage(wgpu::TextureUsages::STORAGE_BINDING);
        assert_eq!(desc.bytes(), 4 * 2 * 8 * 2);
        let wgpu_desc = desc.descriptor(Some("t"));
        assert_eq!(wgpu_desc.size.depth_or_array_layers, 2);
        assert_eq!(wgpu_desc.usage, wgpu::TextureUsages::STORAGE_BINDING);
        assert_ne!(desc, TextureDesc::new(4, 2, wgpu::TextureFormat::Rgba16Float));
    }
}
