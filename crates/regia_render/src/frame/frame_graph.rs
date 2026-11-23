// SPDX-License-Identifier: MPL-2.0
//! The frame's render graph (ADR-2018; `specs/13` § 4).
//!
//! [`FRAME_STEPS`] and the passes registered at its slots ([`super::FrameRenderer::register_pass`])
//! form one versioned [`RenderGraph`]: every built-in pass and every registered pass is a node,
//! every write makes a new version of its resource (`color#1` … `color#9`), and the graph
//! compiles against the frame's `output`. The frame takes from it:
//!
//! - the order registered passes record in at each slot;
//! - the pipeline keys of the live nodes, which the game-frame warm builds
//!   (`FrameRenderer::prewarm_mid_play`);
//! - the versioned listing `render.graph` publishes (what each node reads and writes, which
//!   nodes nothing downstream reads).
//!
//! The graph changes only when a pass is registered or removed ([`FRAME_STEPS`] is constant),
//! so it is rebuilt then and every frame reads the same compiled graph: a steady frame builds
//! nothing and allocates nothing for it.
//!
//! A registered pass whose writes nothing downstream reads is still recorded (as before the
//! graph existed); `render.graph` lists it with `live: false`.

use crate::{
    CompiledGraph, GraphCompileError, GraphInput, GraphOutput, GraphResource, GraphSlot, RenderGraph,
    RenderGraphNode, TransientPoolStats,
};

use super::extension::{FRAME_STEPS, FrameStep};
use super::hdr_scratch::HdrScratch;

/// The resource the frame graph compiles against: the display image.
pub(crate) const FRAME_OUTPUT: &str = "output";

/// Built-in writes the frame graph declares as graph transients (ADR-7877), each as
/// (node, resource), appended to the node's [`FrameStep::Builtin`] writes: the two HDR scratch
/// images, which the frame takes from the view's transient pool under their slot keys
/// (`frame::hdr_scratch`). The outdoor copy is taken by whichever of the three outdoor nodes
/// records first and drawn into by all three.
pub(crate) const BUILTIN_TRANSIENT_WRITES: &[(&str, &str)] = &[
    ("atmosphere", HdrScratch::Outdoor.resource()),
    ("clouds", HdrScratch::Outdoor.resource()),
    ("fog", HdrScratch::Outdoor.resource()),
    ("translucent_hdr", HdrScratch::Translucent.resource()),
];

/// Built-in reads of [`BUILTIN_TRANSIENT_WRITES`], each as (node, resource), put ahead of the
/// node's [`FrameStep::Builtin`] reads: the temporal resolve and the translucent pass read the
/// outdoor copy as the frame's colour, and post samples whichever copy the frame hands it
/// (and prewarms its groups for both, ADR-1361).
pub(crate) const BUILTIN_TRANSIENT_READS: &[(&str, &str)] = &[
    ("taau", HdrScratch::Outdoor.resource()),
    ("translucent_hdr", HdrScratch::Outdoor.resource()),
    ("post_stack", HdrScratch::Outdoor.resource()),
    ("post_stack", HdrScratch::Translucent.resource()),
];

/// The description the frame graph declares for a built-in transient: the usage the pool takes
/// it with, at a nominal 1 × 1 extent (the graph is built once, before any size is known; the
/// frame acquires the real size from the view's pool, and the graph only compares descriptions
/// between writers of one name).
fn builtin_transient_desc() -> crate::TextureDesc {
    crate::TextureDesc::new(1, 1, wgpu::TextureFormat::Rgba16Float)
        .with_usage(super::hdr_scratch::HDR_SCRATCH_USAGE)
}

/// The resource names of [`BUILTIN_TRANSIENT_WRITES`], each once, in table order.
pub(crate) fn builtin_transient_resources() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = Vec::new();
    for (_, resource) in BUILTIN_TRANSIENT_WRITES {
        if !names.contains(resource) {
            names.push(resource);
        }
    }
    names
}

/// What the frame graph needs of one registered pass.
pub(crate) struct PassDecl<'a> {
    pub(crate) name: &'a str,
    pub(crate) slot: GraphSlot,
    pub(crate) reads: &'a [&'static str],
    pub(crate) writes: &'a [&'static str],
    pub(crate) pipelines: &'a [&'static str],
}

/// Index of `slot` in [`FrameGraph::slot_order`].
pub(crate) fn slot_index(slot: GraphSlot) -> usize {
    match slot {
        GraphSlot::AfterOpaque => 0,
        GraphSlot::AfterTransparent => 1,
        GraphSlot::BeforePost => 2,
        GraphSlot::AfterPost => 3,
        GraphSlot::Overlay => 4,
    }
}

/// The compiled frame graph.
pub(crate) struct FrameGraph {
    graph: RenderGraph,
    compiled: Result<CompiledGraph, GraphCompileError>,
    /// Per node: the registered pass it stands for (index into the pass list).
    pass_of_node: Vec<Option<usize>>,
    /// Per slot ([`slot_index`]): the registered passes in the order the graph runs them.
    slot_order: [Vec<usize>; 5],
    /// The live nodes' pipeline keys, each once, in execution order.
    warm_keys: Vec<&'static str>,
    /// The held built-in nodes and the slots in node order (ADR-4945): what
    /// [`FrameDispatch::sites`] hands the frame.
    chain: Vec<ChainStep>,
    /// Per registered pass (index into the pass list): its breadcrumb tag, interned when the
    /// graph compiles (spec 54 § 2.2, ADR-9070).
    pass_tags: Vec<crate::breadcrumbs::PassTag>,
}

/// One step of the frame graph's chain in node order: a held built-in node, or a slot's place
/// (where its registered passes record, none when nothing is registered there).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChainStep {
    /// A built-in node the graph holds.
    Node(BuiltinNode),
    /// A slot's place in the chain.
    Slot(GraphSlot),
}

impl FrameGraph {
    /// Builds and compiles the graph of [`FRAME_STEPS`] with `passes` (registration order) at
    /// their slots and the optional built-in nodes `options` switches on.
    pub(crate) fn build(passes: &[PassDecl<'_>], options: FrameGraphOptions) -> Self {
        let _span = tracing::info_span!("frame_graph_build").entered();
        let mut graph = RenderGraph::versioned();
        super::graph_buffers::declare(&mut graph);
        let mut pass_of_node = Vec::new();
        let mut chain = Vec::new();
        let imported = |names: &[&'static str]| -> Vec<GraphOutput> {
            names
                .iter()
                .map(|name| GraphOutput {
                    name: (*name).to_string(),
                    desc: GraphResource::Imported,
                })
                .collect()
        };
        let inputs = |names: &[&'static str]| -> Vec<GraphInput> {
            names
                .iter()
                .map(|name| GraphInput {
                    name: (*name).to_string(),
                })
                .collect()
        };
        for step in FRAME_STEPS {
            match step {
                FrameStep::Builtin {
                    name,
                    reads,
                    writes,
                    pipelines,
                    ..
                } => {
                    if !options.holds(name) {
                        continue;
                    }
                    if let Some(node) = BuiltinNode::from_name(name) {
                        chain.push(ChainStep::Node(BUILTIN_NODE_LIST[node]));
                    }
                    // The writes the frame owns stay imported; the HDR scratch images are graph
                    // transients (ADR-7877).
                    // The transient reads go first, so the declared reads keep their places.
                    let mut node_inputs: Vec<GraphInput> = BUILTIN_TRANSIENT_READS
                        .iter()
                        .filter(|(node, _)| node == name)
                        .map(|(_, resource)| GraphInput {
                            name: (*resource).to_string(),
                        })
                        .collect();
                    node_inputs.extend(super::graph_buffers::node_reads(name));
                    node_inputs.extend(inputs(reads));
                    let mut node_outputs = imported(writes);
                    node_outputs.extend(
                        BUILTIN_TRANSIENT_WRITES
                            .iter()
                            .filter(|(node, _)| node == name)
                            .map(|(_, resource)| GraphOutput {
                                name: (*resource).to_string(),
                                desc: GraphResource::Texture(builtin_transient_desc()),
                            }),
                    );
                    node_outputs.extend(super::graph_buffers::node_writes(name));
                    graph.add_node(RenderGraphNode {
                        id: (*name).to_string(),
                        inputs: node_inputs,
                        outputs: node_outputs,
                        slot: None,
                        pipelines: pipelines.to_vec(),
                        // A built-in node records through the frame's dispatch
                        // (`FrameDispatch`, ADR-4905), which borrows the frame's state; the frame
                        // graph is never run through `RenderGraph::execute`.
                        record: Box::new(|ctx| {
                            Err(format!(
                                "frame_builtin_dispatch: `{}` records through FrameRenderer::render_into",
                                ctx.node
                            ))
                        }),
                    });
                    pass_of_node.push(None);
                }
                FrameStep::Slot(slot) => {
                    chain.push(ChainStep::Slot(*slot));
                    for (index, pass) in passes.iter().enumerate().filter(|(_, p)| p.slot == *slot) {
                        graph.add_node(RenderGraphNode {
                            id: pass.name.to_string(),
                            inputs: inputs(pass.reads),
                            outputs: imported(pass.writes),
                            slot: Some(*slot),
                            pipelines: pass.pipelines.to_vec(),
                            // Registered passes record through `FramePass::record`.
                            record: Box::new(|_| Ok(())),
                        });
                        pass_of_node.push(Some(index));
                    }
                }
            }
        }
        let compiled = graph.resolve(&[FRAME_OUTPUT]);
        if let Err(err) = &compiled {
            tracing::error!(target: "regia_render", "frame graph: {err}");
        }
        let mut slot_order: [Vec<usize>; 5] = Default::default();
        // A versioned graph runs in registration order, so every node keeps its place: the
        // slot lists follow the node order (culled registered passes included, see the module
        // docs).
        for (node, pass) in graph.nodes().iter().zip(&pass_of_node) {
            if let (Some(slot), Some(pass)) = (node.slot, pass) {
                slot_order[slot_index(slot)].push(*pass);
            }
        }
        let warm_keys = graph.pipeline_keys(&[FRAME_OUTPUT]);
        let pass_tags = passes
            .iter()
            .map(|p| crate::breadcrumbs::intern(p.name))
            .collect();
        Self {
            graph,
            compiled,
            pass_of_node,
            slot_order,
            warm_keys,
            chain,
            pass_tags,
        }
    }

    /// The breadcrumb tag of registered pass `index` (its name, interned at build).
    pub(crate) fn pass_tag(&self, index: usize) -> Option<crate::breadcrumbs::PassTag> {
        self.pass_tags.get(index).copied()
    }

    /// The registered passes of `slot` in the order they record.
    pub(crate) fn slot_order(&self, slot: GraphSlot) -> &[usize] {
        &self.slot_order[slot_index(slot)]
    }

    /// The live nodes' pipeline keys, each once, in execution order.
    pub(crate) fn warm_keys(&self) -> &[&'static str] {
        &self.warm_keys
    }

    /// Nodes declaring a buffer read/write, including registered host passes.
    pub(super) fn buffer_accesses(&self, name: &str, local_readers: &[&str]) -> (Vec<String>, Vec<String>) {
        let writers = self
            .graph
            .nodes()
            .iter()
            .filter(|node| node.outputs.iter().any(|out| out.name == name))
            .map(|node| node.id.clone())
            .collect();
        let readers = self
            .graph
            .nodes()
            .iter()
            .filter(|node| {
                node.inputs.iter().any(|input| input.name == name)
                    || (local_readers.contains(&node.id.as_str())
                        && node.outputs.iter().any(|out| out.name == name))
            })
            .map(|node| node.id.clone())
            .collect();
        (writers, readers)
    }

    /// The listing `render.graph` publishes.
    pub(crate) fn info(&self, transients: TransientPoolStats) -> FrameGraphInfo {
        let (order, culled, reads, writes, error) = match &self.compiled {
            Ok(compiled) => (
                compiled.order.clone(),
                compiled.culled.clone(),
                compiled.reads.clone(),
                compiled.writes.clone(),
                None,
            ),
            Err(err) => (
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Some(FrameGraphError {
                    code: err.code(),
                    message: err.to_string(),
                }),
            ),
        };
        let names = |list: Option<&Vec<crate::ResourceVersion>>, buffers: bool| -> Vec<String> {
            list.map(|versions| {
                versions
                    .iter()
                    .filter(|v| super::graph_buffers::is_buffer(&v.name) == buffers)
                    .map(ToString::to_string)
                    .collect()
            })
            .unwrap_or_default()
        };
        let nodes = self
            .graph
            .nodes()
            .iter()
            .enumerate()
            .map(|(i, node)| FrameGraphNode {
                name: node.id.clone(),
                registered: self.pass_of_node[i].is_some(),
                slot: node.slot,
                reads: names(reads.get(i), false),
                buffer_reads: names(reads.get(i), true),
                writes: names(writes.get(i), false),
                buffer_writes: names(writes.get(i), true),
                pipelines: node.pipelines.clone(),
                live: !culled.contains(&i) && error.is_none(),
            })
            .collect();
        FrameGraphInfo {
            nodes,
            order: order.iter().map(|&i| self.graph.nodes()[i].id.clone()).collect(),
            error,
            warm_keys: self.warm_keys.clone(),
            transients,
            transient_writes: builtin_transient_resources(),
            keyed: TransientPoolStats::default(),
            pooled: Vec::new(),
            buffers: Vec::new(),
        }
    }
}

/// Which optional built-in nodes the frame graph holds (ADR-4905). A node left out is not in
/// [`FrameGraphInfo::nodes`], the frame does not record it, and it creates nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct FrameGraphOptions {
    /// `prepass_occlusion`: phase 1 of the depth prepass's two-phase occlusion
    /// ([`super::FrameRenderer::set_two_phase_occlusion`]).
    pub(crate) two_phase_occlusion: bool,
    /// `shadow_occlusion`: phase 1 of the shadow casters' two-phase occlusion
    /// ([`super::FrameRenderer::set_shadow_occlusion`], ADR-5178).
    pub(crate) shadow_occlusion: bool,
}

impl FrameGraphOptions {
    /// The graph holds `name`'s built-in node.
    fn holds(self, name: &str) -> bool {
        if name == BuiltinNode::PrepassOcclusion.name() {
            self.two_phase_occlusion
        } else if name == BuiltinNode::ShadowOcclusion.name() {
            self.shadow_occlusion
        } else {
            true
        }
    }
}

/// Number of [`FrameStep::Builtin`] rows in [`FRAME_STEPS`].
pub(crate) const BUILTIN_NODES: usize = 30;

/// Stamp-log tag of a timestamp written outside every node's record site (ADR-5386): before the
/// frame's first node, or after the frame closed its dispatch. 0 is a stamp never written.
pub(crate) const STAMP_OUTSIDE: u8 = 1;

/// Stamp-log tag of the first node: node `n` is `STAMP_NODE_TAG + n`.
pub(crate) const STAMP_NODE_TAG: u8 = 2;

// The recorded mask is a `u32` by node number.
const _: () = assert!(BUILTIN_NODES <= 32);

/// A built-in node of the frame graph: one [`FrameStep::Builtin`] row, numbered in [`FRAME_STEPS`]
/// order (`frame_graph` unit tests check the names).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum BuiltinNode {
    ShadowOcclusion,
    Shadows,
    ClusterLights,
    GiProbes,
    PrepassOcclusion,
    DepthPrepass,
    GpuCull,
    Ssao,
    ContactShadows,
    SunShadowTrace,
    ManyLightSampling,
    VisbufferResolve,
    SubsurfaceDraws,
    SubsurfaceBlur,
    Forward,
    OitAccum,
    OitComposite,
    RadianceCache,
    Ssgi,
    Ssr,
    Atmosphere,
    Clouds,
    Fog,
    Velocity,
    Taau,
    TranslucentHdr,
    PostStack,
    TranslucentLayers,
    ScalarOverlays,
    Gizmos,
}

/// The built-in step names in [`FRAME_STEPS`] order.
const fn builtin_names() -> [&'static str; BUILTIN_NODES] {
    let mut names = [""; BUILTIN_NODES];
    let mut step = 0;
    let mut node = 0;
    while step < FRAME_STEPS.len() {
        if let FrameStep::Builtin { name, .. } = FRAME_STEPS[step] {
            names[node] = name;
            node += 1;
        }
        step += 1;
    }
    // Every row named: `BUILTIN_NODES` matches the table (a longer table fails to index above).
    assert!(node == BUILTIN_NODES, "BUILTIN_NODES does not match FRAME_STEPS");
    names
}

/// [`builtin_names`], evaluated at compile time.
const BUILTIN_NAMES: [&str; BUILTIN_NODES] = builtin_names();

impl BuiltinNode {
    /// Its [`FrameStep::Builtin::name`].
    pub(crate) fn name(self) -> &'static str {
        BUILTIN_NAMES[self as usize]
    }

    /// The node numbered `index` ([`FRAME_STEPS`] order).
    pub(crate) fn from_index(index: usize) -> Option<Self> {
        BUILTIN_NODE_LIST.get(index).copied()
    }

    /// The number of the built-in step `name` (tests).
    #[cfg(test)]
    pub(crate) fn from_name_index(name: &str) -> Option<usize> {
        Self::from_name(name)
    }

    /// The node of the built-in step `name`.
    fn from_name(name: &str) -> Option<usize> {
        BUILTIN_NAMES.iter().position(|n| *n == name)
    }

    /// The record site that records it (ADR-4945).
    pub(crate) const fn site(self) -> FrameSite {
        match self {
            Self::ShadowOcclusion | Self::Shadows => FrameSite::Shadows,
            Self::ClusterLights | Self::GiProbes => FrameSite::ClusterProbes,
            Self::PrepassOcclusion | Self::DepthPrepass | Self::GpuCull => FrameSite::DepthPasses,
            Self::Ssao | Self::ContactShadows | Self::SunShadowTrace => FrameSite::ScreenOcclusion,
            Self::ManyLightSampling => FrameSite::ManyLightSampling,
            Self::VisbufferResolve => FrameSite::VisbufferResolve,
            Self::SubsurfaceDraws | Self::SubsurfaceBlur => FrameSite::Subsurface,
            Self::Forward | Self::OitAccum | Self::OitComposite => FrameSite::Forward,
            Self::RadianceCache | Self::Ssgi | Self::Ssr => FrameSite::ScreenSpace,
            Self::Atmosphere | Self::Clouds | Self::Fog => FrameSite::Outdoor,
            Self::Velocity | Self::Taau => FrameSite::Temporal,
            Self::TranslucentHdr => FrameSite::TranslucentHdr,
            Self::PostStack => FrameSite::PostStack,
            Self::TranslucentLayers => FrameSite::TranslucentLayers,
            Self::ScalarOverlays | Self::Gizmos => FrameSite::Overlays,
        }
    }
}

/// A record site of `FrameRenderer::render_into` (ADR-4945): one record function, which records
/// one built-in node or several consecutive ones (each entered through [`FrameDispatch::enter`]),
/// or one slot's registered passes. The frame graph hands the frame its sites in node order
/// ([`FrameDispatch::sites`]) and the frame records them in that order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameSite {
    /// `shadow_occlusion` (phase 1 of the casters' two-phase cull, recorded inside the shadow
    /// passes before any of them draws) and `shadows`: the visibility-buffer shadow cull, the
    /// shadow passes and the translucent casters.
    Shadows,
    /// `cluster_lights` and `gi_probes`.
    ClusterProbes,
    /// `prepass_occlusion`, `depth_prepass` and `gpu_cull` (`steps::depth_passes`).
    DepthPasses,
    /// `ssao`, `contact_shadows` and `sun_shadow_trace`.
    ScreenOcclusion,
    /// `many_light_sampling`.
    ManyLightSampling,
    /// `visbuffer_resolve`.
    VisbufferResolve,
    /// `subsurface_draws` and `subsurface_blur`.
    Subsurface,
    /// `forward`, `oit_accum` (in its middle) and `oit_composite`.
    Forward,
    /// `radiance_cache`, `ssgi` and `ssr`.
    ScreenSpace,
    /// `atmosphere`, `clouds` and `fog`.
    Outdoor,
    /// `velocity` and `taau`.
    Temporal,
    /// `translucent_hdr`.
    TranslucentHdr,
    /// `post_stack` (the `before_post` slot records inside it).
    PostStack,
    /// `translucent_layers`.
    TranslucentLayers,
    /// `scalar_overlays` and `gizmos`.
    Overlays,
    /// A slot's registered passes.
    Slot(GraphSlot),
}

/// Most record sites a frame runs: one per [`FrameSite`] kind, each slot once.
pub(crate) const MAX_SITES: usize = 20;

/// What the frame dispatch saw of one built-in node this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeState {
    /// The graph does not hold it (an optional node switched off).
    Absent,
    /// Not reached yet.
    Pending,
    /// A later node was recorded first: this one was passed over.
    Passed,
    /// Recorded, in dependency order.
    Recorded,
    /// Recorded after a later node (out of order).
    Late,
    /// Its per-frame switch left it out this frame.
    Inactive,
}

/// What the dispatch made of one entry of a node (`FrameDispatch::admit`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Admission {
    /// The graph does not hold the node (or did not when the frame began): nothing to record,
    /// nothing counted.
    NotHeld,
    /// Entered again in one frame: refused and counted as a violation.
    Refused,
    /// Taken: recorded when its switch is on, listed as inactive when it is off.
    Taken,
}

/// The frame's built-in pass dispatch (ADR-4905): the compiled frame graph decides which built-in
/// nodes the frame records and in which order, and every built-in record site of
/// `FrameRenderer::render_into` enters its node here. A node the graph does not hold is not
/// recorded; a node entered twice is refused (it records once); a node passed over, recorded out
/// of dependency order or never reached is counted as a violation and named in
/// [`FrameDispatchInfo`]. Fixed-size: a frame allocates nothing for it.
pub(crate) struct FrameDispatch {
    /// Per node: its rank in the compiled graph's order, `None` when the graph does not hold it.
    rank: [Option<u8>; BUILTIN_NODES],
    /// The nodes by rank.
    by_rank: [u8; BUILTIN_NODES],
    ranked: usize,
    /// The record sites in the graph's node order (ADR-4945).
    sites: [FrameSite; MAX_SITES],
    site_count: usize,
    state: [NodeState; BUILTIN_NODES],
    /// Rank of the next node in dependency order.
    cursor: usize,
    /// The nodes this frame recorded, in recording order.
    trace: [u8; BUILTIN_NODES],
    trace_len: usize,
    open: bool,
    main_view: bool,
    frame: u64,
    /// The last closed main-view frame.
    last_trace: [u8; BUILTIN_NODES],
    last_trace_len: usize,
    last_state: [NodeState; BUILTIN_NODES],
    last_frame: Option<u64>,
    frames: u64,
    violations: u64,
    last_violation: Option<FrameDispatchViolation>,
    /// The node whose record site the frame is in, for the GPU timer's stamp log (ADR-5386):
    /// `STAMP_OUTSIDE` outside every node, `STAMP_NODE_TAG` + the node's number after the
    /// frame entered a held node (shared with the frame's `FrameStamps`; allocated once).
    stamp_node: std::sync::Arc<std::sync::atomic::AtomicU8>,
}

impl Default for FrameDispatch {
    fn default() -> Self {
        Self {
            rank: [None; BUILTIN_NODES],
            by_rank: [0; BUILTIN_NODES],
            ranked: 0,
            sites: [FrameSite::Shadows; MAX_SITES],
            site_count: 0,
            state: [NodeState::Absent; BUILTIN_NODES],
            cursor: 0,
            trace: [0; BUILTIN_NODES],
            trace_len: 0,
            open: false,
            main_view: false,
            frame: 0,
            last_trace: [0; BUILTIN_NODES],
            last_trace_len: 0,
            last_state: [NodeState::Absent; BUILTIN_NODES],
            last_frame: None,
            frames: 0,
            violations: 0,
            last_violation: None,
            stamp_node: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(STAMP_OUTSIDE)),
        }
    }
}

impl FrameDispatch {
    /// Takes the built-in nodes the graph holds and their order from `graph` (after every
    /// rebuild). A versioned graph runs in registration order (`CompiledGraph::order` is the node
    /// order with the culled nodes left out), so the ranks follow the node order. A built-in the
    /// graph culls (a host pass wrote over its output without reading it) keeps its rank and
    /// still records, as before the dispatch: only a node the graph does not hold is left out.
    pub(crate) fn bind(&mut self, graph: &FrameGraph) {
        self.rank = [None; BUILTIN_NODES];
        self.ranked = 0;
        for (node, pass) in graph.graph.nodes().iter().zip(&graph.pass_of_node) {
            if pass.is_none()
                && let Some(node) = BuiltinNode::from_name(&node.id)
            {
                self.place(node);
            }
        }
        if let Err(node) = self.place_sites(&graph.chain) {
            tracing::error!(
                target: "regia_render",
                "frame graph dispatch: `{}` is not next to the rest of its record site; the frame records no built-in pass",
                node.name()
            );
        }
    }

    /// Folds the graph's chain into record sites: consecutive nodes of one site are one site, each
    /// slot is its own. A node whose site already ran earlier in the chain cannot be recorded by
    /// its record function in that order: the sites are refused (none kept) and `Err` names it.
    fn place_sites(&mut self, chain: &[ChainStep]) -> Result<(), BuiltinNode> {
        self.site_count = 0;
        for step in chain {
            let (site, node) = match *step {
                ChainStep::Node(node) => (node.site(), Some(node)),
                ChainStep::Slot(slot) => (FrameSite::Slot(slot), None),
            };
            if node.is_some() && self.site_count > 0 && self.sites[self.site_count - 1] == site {
                continue;
            }
            let placed = &self.sites[..self.site_count];
            if placed.contains(&site) || self.site_count == MAX_SITES {
                self.site_count = 0;
                return Err(node.unwrap_or(BuiltinNode::Shadows));
            }
            self.sites[self.site_count] = site;
            self.site_count += 1;
        }
        Ok(())
    }

    /// The record sites in the order the frame records them (the graph's node order), and how
    /// many there are. Fixed-size: a copy, so the frame can record while it walks them.
    pub(crate) fn sites(&self) -> ([FrameSite; MAX_SITES], usize) {
        (self.sites, self.site_count)
    }

    /// Gives `node` the next rank (once).
    fn place(&mut self, node: usize) {
        if self.rank[node].is_none() && self.ranked < BUILTIN_NODES {
            self.rank[node] = u8::try_from(self.ranked).ok();
            self.by_rank[self.ranked] = node as u8;
            self.ranked += 1;
        }
    }

    /// Opens frame `frame` (`main_view`: not a texture view or a probe capture).
    pub(crate) fn begin(&mut self, frame: u64, main_view: bool) {
        for (state, rank) in self.state.iter_mut().zip(&self.rank) {
            *state = if rank.is_some() {
                NodeState::Pending
            } else {
                NodeState::Absent
            };
        }
        self.cursor = 0;
        self.trace_len = 0;
        self.open = true;
        self.main_view = main_view;
        self.frame = frame;
        self.set_stamp_node(None);
    }

    /// The handle the frame's GPU timer reads, when it writes a stamp, to tag it with the node
    /// whose record site is open (ADR-5386). A clone of one `Arc`: no allocation.
    pub(crate) fn stamp_node(&self) -> std::sync::Arc<std::sync::atomic::AtomicU8> {
        std::sync::Arc::clone(&self.stamp_node)
    }

    /// The nodes this frame (open or just closed) recorded, as a mask by node number (bit `n` for
    /// node `n`): what a GPU timing sample of the frame keeps (ADR-5386).
    pub(crate) fn frame_recorded_mask(&self) -> u32 {
        self.state
            .iter()
            .enumerate()
            .filter(|(_, state)| matches!(state, NodeState::Recorded | NodeState::Late))
            .fold(0, |mask, (node, _)| mask | (1 << node))
    }

    fn set_stamp_node(&self, node: Option<BuiltinNode>) {
        let tag = node.map_or(STAMP_OUTSIDE, |node| STAMP_NODE_TAG + node as u8);
        self.stamp_node.store(tag, std::sync::atomic::Ordering::Relaxed);
    }

    /// Passes over the pending nodes ranked before `rank` and moves the cursor past it.
    fn advance_to(&mut self, rank: usize) {
        for &node in &self.by_rank[self.cursor.min(rank)..rank] {
            if self.state[usize::from(node)] == NodeState::Pending {
                self.state[usize::from(node)] = NodeState::Passed;
            }
        }
        self.cursor = self.cursor.max(rank + 1);
    }

    /// The frame reaches `node`'s record site with its per-frame switch `active`. Returns whether
    /// to record it: `false` when the graph does not hold it, when `active` is `false` (it is
    /// listed as inactive) and when it was already entered this frame (refused, counted).
    /// Outside a frame (no [`Self::begin`]) a held node records and nothing is counted.
    pub(crate) fn enter(&mut self, node: BuiltinNode, active: bool) -> bool {
        self.admit(node, active) == Admission::Taken && active
    }

    /// Takes one entry of `node` with its per-frame switch `active`: the one place the dispatch
    /// decides whether a node is held and whether an entry is refused, so [`Self::enter`] and
    /// [`Self::enter_shadows`] cannot disagree (review fixrev4-graph-dispatch finding 2).
    fn admit(&mut self, node: BuiltinNode, active: bool) -> Admission {
        let index = node as usize;
        let Some(rank) = self.rank[index] else {
            return Admission::NotHeld;
        };
        if !self.open {
            return Admission::Taken;
        }
        let rank = usize::from(rank);
        // The frame is in this node's record site now, whatever it records (ADR-5386).
        self.set_stamp_node(Some(node));
        match self.state[index] {
            NodeState::Absent => Admission::NotHeld,
            NodeState::Recorded | NodeState::Late | NodeState::Inactive => {
                self.violate(FrameDispatchViolation::REPEATED, node);
                Admission::Refused
            }
            NodeState::Passed => {
                if active {
                    self.state[index] = NodeState::Late;
                    self.push(node);
                } else {
                    self.state[index] = NodeState::Inactive;
                }
                self.violate(FrameDispatchViolation::OUT_OF_ORDER, node);
                Admission::Taken
            }
            NodeState::Pending => {
                self.advance_to(rank);
                if active {
                    self.state[index] = NodeState::Recorded;
                    self.push(node);
                } else {
                    self.state[index] = NodeState::Inactive;
                }
                Admission::Taken
            }
        }
    }

    /// Enters the `shadows` site's nodes in their order: the optional `shadow_occlusion` and
    /// `shadows`. `active` says whether phase 1 can cull this frame (a main view with shadow jobs
    /// and GPU objects, ADR-5626); an inactive held node is listed as inactive, not recorded. The
    /// few-casters skip stays inside the record function. Returns whether to call that record
    /// function: the dispatch took the entry (the same decision as [`Self::enter`], from
    /// `admit`). It runs for an inactive node too, so the cull's report still says why it
    /// skipped.
    pub(crate) fn enter_shadows(&mut self, active: bool) -> bool {
        let record = self.admit(BuiltinNode::ShadowOcclusion, active) == Admission::Taken;
        self.enter(BuiltinNode::Shadows, true);
        record
    }

    /// [`Self::enter`] for the always-on nodes one record site records, in their order. Returns
    /// whether every one of them is to record.
    pub(crate) fn enter_all(&mut self, nodes: &[BuiltinNode]) -> bool {
        let mut all = true;
        for &node in nodes {
            all &= self.enter(node, true);
        }
        all
    }

    /// The open (or last) frame is a main view: not a texture view or a probe capture.
    pub(crate) fn main_view(&self) -> bool {
        self.main_view
    }

    /// How many nodes this frame has recorded so far: a record site's start mark for
    /// [`Self::recorded_since`] (pass-boundary copies, ADR-7860).
    pub(crate) fn recorded_len(&self) -> usize {
        self.trace_len
    }

    /// The names of the nodes this frame recorded from mark `start` on, in recording order.
    pub(crate) fn recorded_since(&self, start: usize) -> impl Iterator<Item = &'static str> + '_ {
        self.trace[start.min(self.trace_len)..self.trace_len]
            .iter()
            .map(|&node| BUILTIN_NAMES[usize::from(node)])
    }

    fn push(&mut self, node: BuiltinNode) {
        if let Some(slot) = self.trace.get_mut(self.trace_len) {
            *slot = node as u8;
            self.trace_len += 1;
        }
    }

    /// Closes the frame: a held node it never entered (or passed over and never came back to) is
    /// a skipped node.
    pub(crate) fn finish(&mut self) {
        if !self.open {
            return;
        }
        self.open = false;
        self.set_stamp_node(None);
        let states = self.state;
        for (state, node) in states.iter().zip(BUILTIN_NODE_LIST) {
            if matches!(state, NodeState::Pending | NodeState::Passed) {
                self.violate(FrameDispatchViolation::SKIPPED, node);
            }
        }
        self.frames += 1;
        if self.main_view {
            self.last_trace = self.trace;
            self.last_trace_len = self.trace_len;
            self.last_state = self.state;
            self.last_frame = Some(self.frame);
        }
    }

    fn violate(&mut self, code: &'static str, node: BuiltinNode) {
        if self.violations == 0 {
            tracing::error!(
                target: "regia_render",
                "frame graph dispatch: built-in pass `{}` {code} in frame {}",
                node.name(),
                self.frame
            );
        } else {
            tracing::debug!(
                target: "regia_render",
                "frame graph dispatch: built-in pass `{}` {code} in frame {}",
                node.name(),
                self.frame
            );
        }
        self.violations += 1;
        self.last_violation = Some(FrameDispatchViolation {
            code,
            node: node.name(),
            frame: self.frame,
        });
    }

    /// The report `render.graph` publishes.
    pub(crate) fn info(&self) -> FrameDispatchInfo {
        let named = |want: &[NodeState]| -> Vec<&'static str> {
            (0..self.ranked)
                .map(|rank| usize::from(self.by_rank[rank]))
                .filter(|&node| self.last_frame.is_some() && want.contains(&self.last_state[node]))
                .map(|node| BUILTIN_NAMES[node])
                .collect()
        };
        FrameDispatchInfo {
            frames: self.frames,
            last_frame: self.last_frame,
            recorded: self.last_trace[..self.last_trace_len]
                .iter()
                .map(|&node| BUILTIN_NAMES[usize::from(node)])
                .collect(),
            inactive: named(&[NodeState::Inactive]),
            skipped: named(&[NodeState::Pending, NodeState::Passed]),
            late: named(&[NodeState::Late]),
            violations: self.violations,
            last_violation: self.last_violation,
        }
    }
}

/// Every [`BuiltinNode`] by number.
const BUILTIN_NODE_LIST: [BuiltinNode; BUILTIN_NODES] = [
    BuiltinNode::ShadowOcclusion,
    BuiltinNode::Shadows,
    BuiltinNode::ClusterLights,
    BuiltinNode::GiProbes,
    BuiltinNode::PrepassOcclusion,
    BuiltinNode::DepthPrepass,
    BuiltinNode::GpuCull,
    BuiltinNode::Ssao,
    BuiltinNode::ContactShadows,
    BuiltinNode::SunShadowTrace,
    BuiltinNode::ManyLightSampling,
    BuiltinNode::VisbufferResolve,
    BuiltinNode::SubsurfaceDraws,
    BuiltinNode::SubsurfaceBlur,
    BuiltinNode::Forward,
    BuiltinNode::OitAccum,
    BuiltinNode::OitComposite,
    BuiltinNode::RadianceCache,
    BuiltinNode::Ssgi,
    BuiltinNode::Ssr,
    BuiltinNode::Atmosphere,
    BuiltinNode::Clouds,
    BuiltinNode::Fog,
    BuiltinNode::Velocity,
    BuiltinNode::Taau,
    BuiltinNode::TranslucentHdr,
    BuiltinNode::PostStack,
    BuiltinNode::TranslucentLayers,
    BuiltinNode::ScalarOverlays,
    BuiltinNode::Gizmos,
];

/// A built-in pass the frame did not record exactly once in dependency order
/// ([`FrameDispatchInfo::last_violation`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameDispatchViolation {
    /// `builtin_skipped` (the graph holds it, the frame never recorded it),
    /// `builtin_repeated` (entered a second time in one frame; the second recording was refused)
    /// or `builtin_out_of_order` (recorded after a node that depends on it).
    pub code: &'static str,
    /// The built-in pass name ([`FRAME_STEPS`]).
    pub node: &'static str,
    /// The renderer frame index.
    pub frame: u64,
}

impl FrameDispatchViolation {
    /// [`Self::code`] of a node the frame never recorded.
    pub const SKIPPED: &'static str = "builtin_skipped";
    /// [`Self::code`] of a node entered twice in one frame.
    pub const REPEATED: &'static str = "builtin_repeated";
    /// [`Self::code`] of a node recorded after a node ranked after it.
    pub const OUT_OF_ORDER: &'static str = "builtin_out_of_order";
}

/// How the frame recorded its built-in passes through the frame graph (ADR-4905), in
/// [`super::FrameGraphReport::dispatch`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FrameDispatchInfo {
    /// Frames the dispatch closed (every view: main, texture views, probe captures).
    pub frames: u64,
    /// The renderer frame index of the last closed main-view frame the lists below describe.
    pub last_frame: Option<u64>,
    /// The built-in passes that frame recorded, in recording order.
    pub recorded: Vec<&'static str>,
    /// Held nodes its per-frame switches left out (in graph order).
    pub inactive: Vec<&'static str>,
    /// Held nodes it never recorded (should stay empty).
    pub skipped: Vec<&'static str>,
    /// Nodes it recorded after a node ranked after them (should stay empty).
    pub late: Vec<&'static str>,
    /// Violations over all frames: skipped, repeated or out-of-order nodes (should stay 0).
    pub violations: u64,
    /// The last of them.
    pub last_violation: Option<FrameDispatchViolation>,
}

/// One node of the frame graph in [`FrameGraphInfo::nodes`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameGraphNode {
    /// The built-in pass name ([`FRAME_STEPS`]) or the registered pass name.
    pub name: String,
    /// A host registered it ([`super::FrameRenderer::register_pass`]).
    pub registered: bool,
    /// The slot a registered pass records at (`None` for a built-in pass).
    pub slot: Option<GraphSlot>,
    /// The resource versions it reads (`color#2`; version 0 is the frame's input).
    pub reads: Vec<String>,
    /// The resource versions it writes.
    pub writes: Vec<String>,
    /// Its pipeline keys ([`FrameStep::Builtin::pipelines`]).
    pub pipelines: Vec<&'static str>,
    /// Something downstream of it reaches the frame's `output`. A registered pass that is not
    /// live still records.
    pub live: bool,
    /// Versioned reads of audited persistent buffers; legacy `reads` stays unchanged.
    pub buffer_reads: Vec<String>,
    /// Versioned writes of audited persistent buffers; legacy `writes` stays unchanged.
    pub buffer_writes: Vec<String>,
}

/// Why the frame graph did not compile ([`GraphCompileError`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameGraphError {
    /// [`GraphCompileError::code`].
    pub code: &'static str,
    /// The message.
    pub message: String,
}

/// The frame's compiled render graph in [`super::FrameGraphReport::graph`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameGraphInfo {
    /// Every node in registration order (built-in passes, registered passes at their slots).
    pub nodes: Vec<FrameGraphNode>,
    /// The live nodes' names in execution order.
    pub order: Vec<String>,
    /// Set when the graph did not compile.
    pub error: Option<FrameGraphError>,
    /// The pipeline keys the game-frame warm builds, in execution order.
    pub warm_keys: Vec<&'static str>,
    /// The main view's transient texture pool (the HDR target, the refraction copy and glass
    /// front layer, the SSAO targets, the `before_post` copy and the output-sized depth copy).
    pub transients: TransientPoolStats,
    /// The resources built-in nodes declare as graph transients rather than imported
    /// (ADR-7877): `outdoor_hdr_scratch` and `translucent_hdr_scratch`.
    pub transient_writes: Vec<&'static str>,
    /// The main view's keyed pool (ADR-7877): the two HDR scratch images, each under its own
    /// slot key ([`crate::TransientPool::acquire_keyed`]). Zero from the graph alone;
    /// [`super::FrameRenderer::graph_report`] fills it.
    pub keyed: TransientPoolStats,
    /// Every texture of the main view's two pools with its slot key and label: the frame pool's
    /// (key 0) in creation order, then the keyed pool's (empty from the graph alone;
    /// [`super::FrameRenderer::graph_report`] fills it).
    pub pooled: Vec<crate::TransientEntryInfo>,
    /// Audited persistent buffers, their owners, allocation bytes and possible readers/writers.
    /// Filled by the renderer; empty in a graph-only listing. GPU and CPU bytes are separate.
    pub buffers: Vec<super::graph_buffers::FrameGraphBuffer>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decl<'a>(
        name: &'a str,
        slot: GraphSlot,
        reads: &'a [&'static str],
        writes: &'a [&'static str],
    ) -> PassDecl<'a> {
        PassDecl {
            name,
            slot,
            reads,
            writes,
            pipelines: &[],
        }
    }

    const TWO_PHASE: FrameGraphOptions = FrameGraphOptions {
        two_phase_occlusion: true,
        shadow_occlusion: false,
    };

    /// Two-phase shadow occlusion on (ADR-5178), the prepass's off.
    const SHADOW: FrameGraphOptions = FrameGraphOptions {
        two_phase_occlusion: false,
        shadow_occlusion: true,
    };

    /// Every optional node held.
    const ALL: FrameGraphOptions = FrameGraphOptions {
        two_phase_occlusion: true,
        shadow_occlusion: true,
    };

    #[test]
    fn graph_buffers_reject_reordered_and_missing_writers() {
        // Use the production graph's exact declarations, then corrupt registration order.
        let production = FrameGraph::build(&[], ALL);
        for writer in ["cluster_lights", "many_light_sampling"] {
            for remove in [false, true] {
                let mut graph = RenderGraph::versioned();
                super::super::graph_buffers::declare(&mut graph);
                let nodes = production.graph.nodes();
                let ordered = nodes
                    .iter()
                    .filter(|n| n.id != writer)
                    .chain(nodes.iter().filter(|n| n.id == writer && !remove));
                for node in ordered {
                    graph.add_node(RenderGraphNode {
                        id: node.id.clone(),
                        inputs: node.inputs.clone(),
                        outputs: node.outputs.clone(),
                        slot: node.slot,
                        pipelines: node.pipelines.clone(),
                        record: Box::new(|_| Ok(())),
                    });
                }
                let error = graph.resolve(&[FRAME_OUTPUT]).expect_err("missing prior writer");
                assert_eq!(error.code(), "graph_read_before_write", "{writer}: {error}");
            }
        }
    }

    /// With no registered pass every built-in pass is live, in [`FRAME_STEPS`] order, and the
    /// colour chain is versioned through the frame.
    #[test]
    fn the_built_in_graph_is_live_in_recording_order_with_versioned_colour() {
        let graph = FrameGraph::build(&[], ALL);
        let info = graph.info(TransientPoolStats::default());
        assert_eq!(info.error, None);
        let builtins: Vec<&str> = FRAME_STEPS
            .iter()
            .filter_map(|step| match step {
                FrameStep::Builtin { name, .. } => Some(*name),
                FrameStep::Slot(_) => None,
            })
            .collect();
        assert_eq!(info.order, builtins);
        assert!(info.nodes.iter().all(|node| node.live && !node.registered));
        let node = |name: &str| info.nodes.iter().find(|n| n.name == name).expect(name).clone();
        assert_eq!(node("visbuffer_resolve").writes, vec!["color#1"]);
        assert_eq!(node("subsurface_draws").reads[0], "color#1");
        assert_eq!(node("subsurface_blur").writes, vec!["color#3"]);
        assert_eq!(node("forward").reads[0], "color#3");
        assert_eq!(node("forward").writes, vec!["color#4"]);
        assert_eq!(node("oit_composite").reads[0], "color#4");
        assert_eq!(node("oit_composite").writes[0], "color#5");
        assert_eq!(
            node("taau").reads.last().map(String::as_str),
            Some("reactive_mask#1")
        );
        assert_eq!(node("post_stack").writes, vec!["output#1"]);
        assert_eq!(node("gizmos").writes, vec!["output#4"]);
        assert_eq!(node("shadows").reads, vec!["casters#0", "shadow_occlusion#1"]);
        assert_eq!(node("depth_prepass").reads, vec!["meshes#0", "occlusion#1"]);
    }

    /// The optional `prepass_occlusion` node (ADR-4905) is in the graph only while two-phase
    /// occlusion is on; without it the prepass reads the frame's (empty) `occlusion#0` and every
    /// other built-in pass stays live in the same order.
    #[test]
    fn two_phase_occlusion_is_a_graph_node_only_while_it_is_on() {
        let off = FrameGraph::build(&[], FrameGraphOptions::default()).info(TransientPoolStats::default());
        let on = FrameGraph::build(&[], TWO_PHASE).info(TransientPoolStats::default());
        assert_eq!(off.error, None);
        assert!(!off.nodes.iter().any(|n| n.name == "prepass_occlusion"));
        assert!(off.nodes.iter().all(|n| n.live));
        let on_order: Vec<&String> = on.order.iter().filter(|n| *n != "prepass_occlusion").collect();
        assert_eq!(off.order.iter().collect::<Vec<_>>(), on_order);
        let prepass = off
            .nodes
            .iter()
            .find(|n| n.name == "depth_prepass")
            .expect("prepass");
        assert_eq!(prepass.reads, vec!["meshes#0", "occlusion#0"]);
        let at = |info: &FrameGraphInfo, name: &str| info.order.iter().position(|n| n == name);
        assert_eq!(
            at(&on, "prepass_occlusion").map(|i| i + 1),
            at(&on, "depth_prepass")
        );
    }

    /// The optional `shadow_occlusion` node (ADR-5178) is in the graph only while two-phase
    /// shadow occlusion is on; it comes right before `shadows`, which reads what it writes, and
    /// without it `shadows` reads the frame's (empty) `shadow_occlusion#0` and every other
    /// built-in pass stays live in the same order. It is independent of the prepass's node.
    #[test]
    fn shadow_occlusion_is_a_graph_node_only_while_it_is_on() {
        let off = FrameGraph::build(&[], FrameGraphOptions::default()).info(TransientPoolStats::default());
        let on = FrameGraph::build(&[], SHADOW).info(TransientPoolStats::default());
        let all = FrameGraph::build(&[], ALL).info(TransientPoolStats::default());
        assert_eq!(on.error, None);
        assert!(!off.nodes.iter().any(|n| n.name == "shadow_occlusion"));
        assert!(!on.nodes.iter().any(|n| n.name == "prepass_occlusion"));
        assert!(on.nodes.iter().all(|n| n.live), "{:?}", on.nodes);
        let on_order: Vec<&String> = on.order.iter().filter(|n| *n != "shadow_occlusion").collect();
        assert_eq!(off.order.iter().collect::<Vec<_>>(), on_order);
        let node = |info: &FrameGraphInfo, name: &str| {
            info.nodes.iter().find(|n| n.name == name).expect(name).clone()
        };
        assert_eq!(
            node(&off, "shadows").reads,
            vec!["casters#0", "shadow_occlusion#0"]
        );
        assert_eq!(node(&on, "shadow_occlusion").reads, vec!["casters#0"]);
        assert_eq!(node(&on, "shadow_occlusion").writes, vec!["shadow_occlusion#1"]);
        assert_eq!(
            node(&on, "shadows").reads,
            vec!["casters#0", "shadow_occlusion#1"]
        );
        assert!(!node(&on, "shadow_occlusion").registered);
        assert_eq!(on.order.first().map(String::as_str), Some("shadow_occlusion"));
        assert_eq!(on.order.get(1).map(String::as_str), Some("shadows"));
        // Both optional nodes: each comes right before the pass that reads it.
        let at = |info: &FrameGraphInfo, name: &str| info.order.iter().position(|n| n == name);
        assert_eq!(at(&all, "shadow_occlusion").map(|i| i + 1), at(&all, "shadows"));
        assert_eq!(
            at(&all, "prepass_occlusion").map(|i| i + 1),
            at(&all, "depth_prepass")
        );
        // The warm keys do not change: the node has no pipeline key of its own.
        assert_eq!(
            FrameGraph::build(&[], SHADOW).warm_keys(),
            FrameGraph::build(&[], FrameGraphOptions::default()).warm_keys()
        );
    }

    /// The dispatch enters `shadow_occlusion` before `shadows` and records it only while the graph
    /// holds it (ADR-5178): absent, `enter_shadows` says not to record and nothing is counted.
    #[test]
    fn the_shadows_site_enters_shadow_occlusion_only_while_the_graph_holds_it() {
        for (options, held) in [(FrameGraphOptions::default(), false), (SHADOW, true)] {
            let mut dispatch = dispatch(options);
            dispatch.begin(9, true);
            assert_eq!(dispatch.enter_shadows(true), held, "{options:?}");
            for node in &BUILTIN_NODE_LIST[2..] {
                dispatch.enter(*node, true);
            }
            dispatch.finish();
            let info = dispatch.info();
            assert_eq!(info.violations, 0, "{options:?}: {:?}", info.last_violation);
            assert_eq!(info.recorded.contains(&"shadow_occlusion"), held, "{options:?}");
            let shadows = info.recorded.iter().position(|n| *n == "shadows");
            assert_eq!(shadows, Some(usize::from(held)), "{:?}", info.recorded);
        }
    }

    /// Review fixrev-graph-dispatch-g finding 1 (ADR-5626): a frame where phase 1 cannot cull
    /// (a CPU-culled frame, no shadow jobs, a texture view) enters `shadow_occlusion` inactive.
    /// It is listed as inactive and not recorded, the record function still runs (it reports
    /// why it skipped), `shadows` records, and the dispatch counts no violation. A second
    /// entry in one frame is refused as before.
    #[test]
    fn the_shadows_site_lists_shadow_occlusion_inactive_when_phase1_cannot_cull() {
        let mut site = dispatch(SHADOW);
        site.begin(9, true);
        assert!(
            site.enter_shadows(false),
            "the record function still reports the skip"
        );
        for node in &BUILTIN_NODE_LIST[2..] {
            site.enter(*node, true);
        }
        site.finish();
        let info = site.info();
        assert_eq!(info.violations, 0, "{:?}", info.last_violation);
        assert!(
            !info.recorded.contains(&"shadow_occlusion"),
            "{:?}",
            info.recorded
        );
        assert_eq!(info.inactive, ["shadow_occlusion"], "{info:?}");
        assert_eq!(info.recorded.first(), Some(&"shadows"), "{:?}", info.recorded);

        // Not held: nothing to record, active or not.
        let mut absent = dispatch(FrameGraphOptions::default());
        absent.begin(10, true);
        assert!(!absent.enter_shadows(false));
        assert!(!absent.enter_shadows(true));

        // Entered twice in one frame: refused (counted), whatever `active` says.
        let mut twice = dispatch(SHADOW);
        twice.begin(11, true);
        assert!(twice.enter_shadows(false));
        assert!(!twice.enter_shadows(false));
        assert!(!twice.enter_shadows(true));
        twice.finish();
        assert!(twice.info().violations > 0);
    }

    /// Review fixrev4-graph-dispatch finding 2: `enter_shadows` and `enter` decide from one rule.
    /// A graph that comes to hold `shadow_occlusion` after the frame began (the node was absent
    /// at `begin`) is not entered by `enter`; `enter_shadows` must not call the record function
    /// for it either. Its own copy of the refusal rule (held and not repeated) said to record.
    #[test]
    fn enter_shadows_takes_the_node_exactly_when_enter_does() {
        // `enter` on its own: a node absent at `begin` is not recorded this frame.
        let mut alone = dispatch(FrameGraphOptions::default());
        alone.begin(12, true);
        alone.bind(&FrameGraph::build(&[], SHADOW));
        assert!(!alone.enter(BuiltinNode::ShadowOcclusion, true));
        // The shadows site agrees, active or not, and the node is neither recorded nor inactive.
        for active in [true, false] {
            let mut site = dispatch(FrameGraphOptions::default());
            site.begin(12, true);
            site.bind(&FrameGraph::build(&[], SHADOW));
            assert!(
                !site.enter_shadows(active),
                "active {active}: the record function ran for a node the frame does not hold"
            );
            site.finish();
            let info = site.info();
            assert!(!info.recorded.contains(&"shadow_occlusion"), "{info:?}");
            assert!(!info.inactive.contains(&"shadow_occlusion"), "{info:?}");
        }
        // Outside a frame both take a held node.
        let mut outside = dispatch(SHADOW);
        assert!(outside.enter_shadows(false));
        assert!(outside.enter(BuiltinNode::ShadowOcclusion, true));
    }

    /// `BuiltinNode` numbers the built-in rows of [`FRAME_STEPS`] in order.
    #[test]
    fn builtin_nodes_name_the_frame_steps_in_order() {
        let builtins: Vec<&str> = FRAME_STEPS
            .iter()
            .filter_map(|step| match step {
                FrameStep::Builtin { name, .. } => Some(*name),
                FrameStep::Slot(_) => None,
            })
            .collect();
        let names: Vec<&str> = BUILTIN_NODE_LIST.iter().map(|n| n.name()).collect();
        assert_eq!(names, builtins);
        for (i, node) in BUILTIN_NODE_LIST.iter().enumerate() {
            assert_eq!(*node as usize, i);
        }
        assert_eq!(BuiltinNode::ShadowOcclusion.name(), "shadow_occlusion");
        assert_eq!(BuiltinNode::Shadows.name(), "shadows");
        assert_eq!(BuiltinNode::PrepassOcclusion.name(), "prepass_occlusion");
        assert_eq!(BuiltinNode::DepthPrepass.name(), "depth_prepass");
        assert_eq!(BuiltinNode::Forward.name(), "forward");
        assert_eq!(BuiltinNode::OitComposite.name(), "oit_composite");
        assert_eq!(BuiltinNode::TranslucentHdr.name(), "translucent_hdr");
        assert_eq!(BuiltinNode::Gizmos.name(), "gizmos");
    }

    fn dispatch(options: FrameGraphOptions) -> FrameDispatch {
        let mut dispatch = FrameDispatch::default();
        dispatch.bind(&FrameGraph::build(&[], options));
        dispatch
    }

    /// Enters every node in [`FRAME_STEPS`] order except `leave_out`; returns what each enter said.
    fn frame(dispatch: &mut FrameDispatch, leave_out: &[BuiltinNode]) -> Vec<bool> {
        dispatch.begin(7, true);
        let entered = BUILTIN_NODE_LIST
            .iter()
            .filter(|node| !leave_out.contains(node))
            .map(|&node| dispatch.enter(node, true))
            .collect();
        dispatch.finish();
        entered
    }

    /// The oracle: a frame that enters every held node once, in order, records them all and
    /// counts nothing; the optional node the graph does not hold is refused.
    #[test]
    fn a_frame_that_enters_every_node_once_in_order_is_clean() {
        let mut dispatch = dispatch(FrameGraphOptions::default());
        let entered = frame(&mut dispatch, &[]);
        let info = dispatch.info();
        assert_eq!(info.violations, 0, "{:?}", info.last_violation);
        assert_eq!(info.frames, 1);
        assert_eq!(info.last_frame, Some(7));
        let held: Vec<&str> = BUILTIN_NAMES
            .iter()
            .copied()
            .filter(|n| *n != "prepass_occlusion" && *n != "shadow_occlusion")
            .collect();
        assert_eq!(info.recorded, held);
        assert!(info.skipped.is_empty() && info.late.is_empty() && info.inactive.is_empty());
        assert!(
            !entered[BuiltinNode::PrepassOcclusion as usize],
            "not held: never records"
        );
        assert!(
            !entered[BuiltinNode::ShadowOcclusion as usize],
            "not held: never records"
        );
        assert_eq!(entered.iter().filter(|e| !**e).count(), 2);

        let mut dispatch = self::dispatch(ALL);
        frame(&mut dispatch, &[]);
        let info = dispatch.info();
        assert_eq!(info.violations, 0);
        assert_eq!(info.recorded, BUILTIN_NAMES.to_vec());
    }

    /// A built-in the graph culls (here a host overlay pass writes `output` without reading it, so
    /// nothing reads the gizmos' output) is still held: it keeps its rank and records, as before
    /// the dispatch existed.
    #[test]
    fn a_culled_built_in_still_records() {
        let passes = [decl("blind_overlay", GraphSlot::Overlay, &[], &["output"])];
        let graph = FrameGraph::build(&passes, FrameGraphOptions::default());
        let info = graph.info(TransientPoolStats::default());
        let gizmos = info.nodes.iter().find(|n| n.name == "gizmos").expect("gizmos");
        assert!(!gizmos.live, "the blind write culls the gizmos: {:?}", info.order);
        let mut dispatch = FrameDispatch::default();
        dispatch.bind(&graph);
        let entered = frame(&mut dispatch, &[]);
        assert!(entered[BuiltinNode::Gizmos as usize]);
        let info = dispatch.info();
        assert_eq!(info.violations, 0, "{:?}", info.last_violation);
        assert!(info.recorded.contains(&"gizmos") && info.recorded.contains(&"scalar_overlays"));
    }

    /// The oracle fails when a declared built-in is skipped.
    #[test]
    fn a_skipped_built_in_is_a_violation() {
        let mut dispatch = dispatch(FrameGraphOptions::default());
        frame(&mut dispatch, &[BuiltinNode::Ssao]);
        let info = dispatch.info();
        assert_eq!(info.violations, 1);
        assert_eq!(info.skipped, vec!["ssao"]);
        let violation = info.last_violation.expect("violation");
        assert_eq!((violation.code, violation.node), ("builtin_skipped", "ssao"));
        assert!(!info.recorded.contains(&"ssao"));
        // The last node never reached counts too.
        frame(&mut dispatch, &[BuiltinNode::Gizmos]);
        let info = dispatch.info();
        assert_eq!(info.violations, 2);
        assert_eq!(info.skipped, vec!["gizmos"]);
    }

    /// The oracle fails when a built-in is recorded twice, and the second recording is refused.
    #[test]
    fn a_built_in_entered_twice_records_once_and_is_a_violation() {
        let mut dispatch = dispatch(FrameGraphOptions::default());
        dispatch.begin(3, true);
        for node in BUILTIN_NODE_LIST {
            dispatch.enter(node, true);
            if node == BuiltinNode::Forward {
                assert!(
                    !dispatch.enter(BuiltinNode::Forward, true),
                    "second record refused"
                );
            }
        }
        dispatch.finish();
        let info = dispatch.info();
        assert_eq!(info.violations, 1);
        let violation = info.last_violation.expect("violation");
        assert_eq!(
            (violation.code, violation.node, violation.frame),
            ("builtin_repeated", "forward", 3)
        );
        assert_eq!(info.recorded.iter().filter(|n| **n == "forward").count(), 1);
    }

    /// The oracle fails when a built-in records after a node ranked after it.
    #[test]
    fn a_built_in_recorded_out_of_dependency_order_is_a_violation() {
        let mut dispatch = dispatch(FrameGraphOptions::default());
        dispatch.begin(1, true);
        for node in BUILTIN_NODE_LIST {
            match node {
                BuiltinNode::ScalarOverlays => {}
                BuiltinNode::Gizmos => {
                    assert!(dispatch.enter(BuiltinNode::Gizmos, true));
                    assert!(
                        dispatch.enter(BuiltinNode::ScalarOverlays, true),
                        "late, still records"
                    );
                }
                node => {
                    dispatch.enter(node, true);
                }
            }
        }
        dispatch.finish();
        let info = dispatch.info();
        assert_eq!(info.violations, 1);
        assert_eq!(info.late, vec!["scalar_overlays"]);
        assert!(info.skipped.is_empty());
        assert_eq!(info.last_violation.map(|v| v.code), Some("builtin_out_of_order"));
        assert_eq!(
            info.recorded[info.recorded.len() - 2..],
            ["gizmos", "scalar_overlays"]
        );
    }

    /// A node whose per-frame switch is off is listed as inactive, not recorded and not counted;
    /// a texture-view frame counts but leaves the main view's lists alone.
    #[test]
    fn inactive_nodes_and_texture_views() {
        let mut dispatch = dispatch(FrameGraphOptions::default());
        dispatch.begin(5, true);
        for node in BUILTIN_NODE_LIST {
            let active = node != BuiltinNode::PostStack;
            assert_eq!(
                dispatch.enter(node, active),
                active && node != BuiltinNode::PrepassOcclusion && node != BuiltinNode::ShadowOcclusion
            );
        }
        dispatch.finish();
        dispatch.begin(6, false);
        dispatch.enter(BuiltinNode::Shadows, true);
        dispatch.finish();
        let info = dispatch.info();
        assert_eq!(info.frames, 2);
        assert_eq!(info.last_frame, Some(5));
        assert_eq!(info.inactive, vec!["post_stack"]);
        assert!(!info.recorded.contains(&"post_stack"));
        // The texture view entered one node and skipped the rest (the two optional nodes are not
        // held).
        assert_eq!(
            info.last_violation.map(|v| (v.code, v.frame)),
            Some(("builtin_skipped", 6))
        );
        assert_eq!(info.violations, (BUILTIN_NODES - 3) as u64);
        // Outside a frame a held node records and nothing is counted.
        assert!(dispatch.enter(BuiltinNode::Forward, true));
        assert!(!dispatch.enter(BuiltinNode::PrepassOcclusion, true));
        assert!(!dispatch.enter(BuiltinNode::ShadowOcclusion, true));
        assert_eq!(dispatch.info().violations, (BUILTIN_NODES - 3) as u64);
    }

    /// Registered passes take their slot's place in the versioned chain, in registration order;
    /// one whose write nothing reads is listed as not live but still recorded.
    #[test]
    fn registered_passes_join_the_chain_at_their_slots() {
        let passes = [
            decl("outline", GraphSlot::AfterPost, &["output", "depth"], &["output"]),
            decl("tint", GraphSlot::AfterOpaque, &["color"], &["color"]),
            decl("mask_only", GraphSlot::AfterOpaque, &["depth"], &["outline_mask"]),
            decl("late_tint", GraphSlot::AfterOpaque, &["color"], &["color"]),
        ];
        let graph = FrameGraph::build(&passes, FrameGraphOptions::default());
        let info = graph.info(TransientPoolStats::default());
        assert_eq!(info.error, None);
        assert_eq!(graph.slot_order(GraphSlot::AfterOpaque), &[1, 2, 3]);
        assert_eq!(graph.slot_order(GraphSlot::AfterPost), &[0]);
        assert!(graph.slot_order(GraphSlot::Overlay).is_empty());
        let node = |name: &str| info.nodes.iter().find(|n| n.name == name).expect(name).clone();
        // forward wrote color#4, the transparency composite #5; the two tints make #6 and #7; ssgi
        // reads #7.
        assert_eq!(node("tint").reads, vec!["color#5"]);
        assert_eq!(node("late_tint").writes, vec!["color#7"]);
        assert_eq!(node("ssgi").reads[0], "color#7");
        assert_eq!(node("outline").slot, Some(GraphSlot::AfterPost));
        assert!(node("outline").live && node("outline").registered);
        assert!(!node("mask_only").live, "nothing reads outline_mask");
        assert!(!info.order.contains(&"mask_only".to_string()));
        let post = info.order.iter().position(|n| n == "post_stack").expect("post");
        let outline = info.order.iter().position(|n| n == "outline").expect("outline");
        let scalar = info
            .order
            .iter()
            .position(|n| n == "scalar_overlays")
            .expect("scalar");
        assert!(post < outline && outline < scalar, "{:?}", info.order);
    }

    /// Every pipeline key a built-in step declares has a warm (`WARM_KEYS`), and every warm is
    /// declared by a step: a pass that needs a pipeline built before play names it here.
    #[test]
    fn built_in_pipeline_keys_and_the_warm_table_name_each_other() {
        let declared: std::collections::BTreeSet<&str> = FRAME_STEPS
            .iter()
            .flat_map(|step| match step {
                FrameStep::Builtin { pipelines, .. } => pipelines.iter().copied(),
                FrameStep::Slot(_) => [].iter().copied(),
            })
            .collect();
        let warms: std::collections::BTreeSet<&str> =
            super::super::prewarm::WARM_KEYS.iter().copied().collect();
        assert_eq!(declared, warms);
    }

    /// The warm keys are the live built-in passes' keys, each once, in frame order.
    #[test]
    fn warm_keys_follow_the_frame_order() {
        let graph = FrameGraph::build(&[], FrameGraphOptions::default());
        assert_eq!(
            graph.warm_keys(),
            &[
                "blue_noise",
                "sun_shadow_trace",
                "restir_di",
                "visbuffer_path",
                "subsurface",
                "forward_sun_rt",
                "forward_studio",
                "oit",
                "ssgi_half_res",
                "radiance_cache",
                "outdoor",
                "tsr",
                "taau_resolve",
                "depth_match",
                "depth_match_msaa"
            ]
        );
    }

    /// The record sites the frame runs (ADR-4945), in the graph's node order: one per record
    /// function, each slot at its place in the chain.
    const SITES: [FrameSite; MAX_SITES] = [
        FrameSite::Shadows,
        FrameSite::ClusterProbes,
        FrameSite::DepthPasses,
        FrameSite::ScreenOcclusion,
        FrameSite::ManyLightSampling,
        FrameSite::VisbufferResolve,
        FrameSite::Subsurface,
        FrameSite::Forward,
        FrameSite::Slot(GraphSlot::AfterOpaque),
        FrameSite::ScreenSpace,
        FrameSite::Outdoor,
        FrameSite::Temporal,
        FrameSite::TranslucentHdr,
        FrameSite::Slot(GraphSlot::AfterTransparent),
        FrameSite::Slot(GraphSlot::BeforePost),
        FrameSite::PostStack,
        FrameSite::TranslucentLayers,
        FrameSite::Slot(GraphSlot::AfterPost),
        FrameSite::Overlays,
        FrameSite::Slot(GraphSlot::Overlay),
    ];

    /// The graph hands the frame its record sites in node order (ADR-4945): every built-in node
    /// the graph holds belongs to exactly one site, a site's nodes are consecutive in the graph,
    /// and the optional nodes do not add a site of their own (the depth passes hold
    /// `prepass_occlusion`, the shadows `shadow_occlusion`).
    #[test]
    fn the_graph_hands_the_frame_its_record_sites_in_node_order() {
        for options in [FrameGraphOptions::default(), TWO_PHASE, SHADOW, ALL] {
            let graph = FrameGraph::build(&[], options);
            let mut dispatch = FrameDispatch::default();
            dispatch.bind(&graph);
            let (sites, count) = dispatch.sites();
            assert_eq!(&sites[..count], &SITES[..], "{options:?}");
            // The built-in sites follow the graph's nodes: each node's site, consecutive repeats
            // folded, is the site list without the slots.
            let mut from_nodes: Vec<FrameSite> = Vec::new();
            for node in graph.graph.nodes() {
                let site = BuiltinNode::from_name(&node.id)
                    .map(|n| BUILTIN_NODE_LIST[n].site())
                    .expect("a built-in node");
                if from_nodes.last() != Some(&site) {
                    from_nodes.push(site);
                }
            }
            let built_in: Vec<FrameSite> = SITES
                .iter()
                .copied()
                .filter(|s| !matches!(s, FrameSite::Slot(_)))
                .collect();
            assert_eq!(from_nodes, built_in, "{options:?}");
        }
        for node in BUILTIN_NODE_LIST {
            assert_eq!(SITES.iter().filter(|s| **s == node.site()).count(), 1, "{node:?}");
        }
    }

    /// Registered passes do not change the sites: they record inside their slot's site, in the
    /// slot order the graph gives.
    #[test]
    fn registered_passes_record_inside_their_slot_site() {
        let passes = [
            decl("tint", GraphSlot::AfterOpaque, &["color"], &["color"]),
            decl("outline", GraphSlot::AfterPost, &["output"], &["output"]),
        ];
        let graph = FrameGraph::build(&passes, FrameGraphOptions::default());
        let mut dispatch = FrameDispatch::default();
        dispatch.bind(&graph);
        let (sites, count) = dispatch.sites();
        assert_eq!(&sites[..count], &SITES[..]);
        assert_eq!(graph.slot_order(GraphSlot::AfterOpaque), &[0]);
        assert_eq!(graph.slot_order(GraphSlot::AfterPost), &[1]);
    }

    /// A site list the frame cannot run (a node whose site is not next) is refused at bind: the
    /// dispatch keeps no sites, so the frame records no built-in and the oracle counts every held
    /// node as skipped.
    #[test]
    fn a_graph_whose_sites_are_not_contiguous_is_refused() {
        let mut dispatch = FrameDispatch::default();
        let order = [
            BuiltinNode::Shadows,
            BuiltinNode::ClusterLights,
            BuiltinNode::DepthPrepass,
            BuiltinNode::GiProbes,
        ];
        let chain = order.map(ChainStep::Node);
        assert_eq!(dispatch.place_sites(&chain), Err(BuiltinNode::GiProbes));
        let (_, count) = dispatch.sites();
        assert_eq!(count, 0);
    }
}
