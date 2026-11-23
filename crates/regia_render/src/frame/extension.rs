// SPDX-License-Identifier: MPL-2.0
//! The frame's pass extension point (ADR-1552; `specs/13` § 4: plugins insert nodes at named
//! slots).
//!
//! A game or tool crate implements [`FramePass`] and registers it with
//! [`FrameRenderer::register_pass`]. [`FrameRenderer::render_into`] records every registered pass
//! of a slot, in registration order, into the frame's own command encoder when the frame reaches
//! that slot. The pass sees the slot's colour target, the scene depth, the frame's matrices and
//! the tracked device (so its creates count in the hitch gate), and never submits: the frame
//! still submits once.
//!
//! With no pass registered the frame does no extra work and allocates nothing, so every frame
//! without extensions is bit-identical to the frame before the extension point existed.
//!
//! [`FrameRenderer::graph_report`] lists the built-in passes in recording order, the slots
//! between them, the registered passes with their counters, and the last GPU timings; the agent
//! protocol publishes it as `render.graph`.

use regia_core::glam::Mat4;

use super::FrameRenderer;
use crate::{GpuTimings, GraphSlot, TrackedDevice};

/// One pass a host adds to the frame at a named [`GraphSlot`].
///
/// `record` runs on the frame thread inside [`FrameRenderer::render_into`], once per frame that
/// reaches the slot. It records into [`FramePassContext::encoder`] (render or compute passes,
/// copies) and must not submit. An `Err` skips the pass for this frame only: the frame goes on,
/// the error is logged and counted ([`FramePassInfo::failures`]).
pub trait FramePass: Send + Sync + 'static {
    /// Stable, unique name: lowercase ascii letters, digits and `_` (listed by `render.graph`).
    fn name(&self) -> &str;
    /// The slot this pass records at.
    fn slot(&self) -> GraphSlot;
    /// Names of the frame resources the pass reads, for the graph listing (for example
    /// `"color"`, `"depth"`). Informational: the frame does not reorder passes by it.
    fn reads(&self) -> &[&'static str] {
        &[]
    }
    /// Names of the frame resources the pass writes. The frame graph versions them (ADR-2018):
    /// a pass that writes `color` makes the next version every later reader binds to.
    fn writes(&self) -> &[&'static str] {
        &[]
    }
    /// Keys of the pipelines the pass records with, for the graph listing (`render.graph`
    /// `warm_keys`). Informational: the warm builds them through [`Self::prewarm`].
    fn pipelines(&self) -> &[&'static str] {
        &[]
    }
    /// Records the pass into the frame's encoder.
    ///
    /// # Errors
    /// Any error skips this pass for this frame; the message is kept for `render.graph`.
    fn record(&mut self, ctx: &mut FramePassContext<'_>) -> Result<(), String>;

    /// Whether the pass writes the slot's colour target this frame, given the view's post settings
    /// ([`FramePassContext::post`]; `None` outside `before_post`). At `before_post` the frame hands
    /// writing passes a per-frame copy of the HDR image, because the image the post stack reads
    /// may be a temporal history or the held-screen cache that the next frame reads again
    /// (ADR-1863); a pass that returns `false` records against the image itself and saves the
    /// copy, so it must not write it. The default is `true` when [`Self::writes`] lists anything.
    fn writes_color(&self, post: Option<&crate::PostSettings>) -> bool {
        let _ = post;
        !self.writes().is_empty()
    }

    /// Builds what the pass would otherwise create on its first recording frame (pipelines,
    /// fixed resources), during the renderer's game-frame warm, so play does not compile them on
    /// the frame thread (ADR-1863). Returns the pipelines it asked for. The default does nothing.
    ///
    /// # Errors
    /// The message is logged; the warm goes on and the pass builds on first use instead.
    fn prewarm(&mut self, device: &TrackedDevice) -> Result<usize, String> {
        let _ = device;
        Ok(0)
    }
}

/// What the colour target of a slot holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotColorSpace {
    /// Linear, scene-referred HDR radiance multiplied by [`FramePassContext::pre_exposure`]
    /// (before the post stack's exposure and tonemap).
    SceneLinearHdr,
    /// The display-referred output image (after the tonemap; the target's own format).
    Display,
}

impl SlotColorSpace {
    /// `"scene_linear_hdr"` or `"display"`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SceneLinearHdr => "scene_linear_hdr",
            Self::Display => "display",
        }
    }
}

/// Everything a [`FramePass`] may use while it records.
pub struct FramePassContext<'a> {
    /// The tracked device: pipeline, buffer and texture creates count like the frame's own.
    pub device: &'a TrackedDevice,
    /// The frame's command encoder (do not submit; the frame submits once).
    pub encoder: &'a mut wgpu::CommandEncoder,
    /// The slot being recorded.
    pub slot: GraphSlot,
    /// The slot's colour target.
    pub color: &'a wgpu::TextureView,
    /// Its format.
    pub color_format: wgpu::TextureFormat,
    /// Its size in pixels.
    pub color_size: (u32, u32),
    /// What `color` holds.
    pub color_space: SlotColorSpace,
    /// The view rectangle inside `color` (x, y, w, h in pixels).
    pub viewport: [f32; 4],
    /// The scene depth (standard depth 0..1; any size, sample it by UV inside the viewport).
    pub depth: &'a wgpu::TextureView,
    /// `depth` is multisampled.
    pub depth_multisampled: bool,
    /// Unjittered view-projection of the frame.
    pub clip_from_world: Mat4,
    /// Unjittered projection of the frame.
    pub clip_from_view: Mat4,
    /// Camera position (world space).
    pub eye: [f32; 3],
    /// Pre-exposure of a [`SlotColorSpace::SceneLinearHdr`] target (1 on display targets).
    pub pre_exposure: f32,
    /// The renderer's frame counter.
    pub frame_index: u64,
    /// The render-to-texture view being drawn ([`FrameRenderer::render_texture_view`]'s asset
    /// id), `None` for the main view. Reflection-probe captures run no registered pass.
    pub texture_view: Option<u64>,
    /// The view's resolved post settings (project, level look, camera, post volumes and the
    /// preset gate applied) at [`GraphSlot::BeforePost`], where the post stack runs next; `None`
    /// at the other slots (ADR-1863).
    pub post: Option<&'a crate::PostSettings>,
    /// The exposure the post stack applies to this view at [`GraphSlot::BeforePost`], `None` at
    /// the other slots (ADR-1863): a value `v` of the HDR target reads as `v / pre_exposure ×
    /// exposure` on the display, before the tonemap. Unlike [`Self::pre_exposure`], which moves
    /// in whole stops, it follows the auto exposure smoothly.
    pub exposure: Option<f32>,
    queue: &'a crate::device::TrackedQueue,
}

impl FramePassContext<'_> {
    /// Writes `data` into `buffer` at `offset` through the frame's queue (staged; it lands before
    /// the frame's submit).
    pub fn write_buffer(&self, buffer: &wgpu::Buffer, offset: u64, data: &[u8]) {
        self.queue.write_buffer(buffer, offset, data);
    }
}

/// Why [`FrameRenderer::register_pass`] refused a pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PassRegisterError {
    /// The name is empty or has characters other than lowercase ascii letters, digits and `_`.
    BadName(String),
    /// A built-in pass or another registered pass already has this name.
    DuplicateName(String),
}

impl PassRegisterError {
    /// The agent-protocol error code: `pass_bad_name` or `pass_duplicate_name`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::BadName(_) => "pass_bad_name",
            Self::DuplicateName(_) => "pass_duplicate_name",
        }
    }
}

impl std::fmt::Display for PassRegisterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadName(name) => write!(
                f,
                "frame pass name `{name}`: use lowercase ascii letters, digits and `_`"
            ),
            Self::DuplicateName(name) => write!(f, "frame pass `{name}` is already in the graph"),
        }
    }
}

impl std::error::Error for PassRegisterError {}

/// One step of the frame in recording order: a built-in pass or a slot where registered passes
/// record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameStep {
    /// A built-in pass: its name, the module that records it, the resources it reads and
    /// writes, and the [`GpuTimings::passes`] entry that times it (when one does).
    Builtin {
        /// Pass name.
        name: &'static str,
        /// The `regia_render` source module that records it (`frame::<module>`, one file under
        /// `src/frame/`).
        module: &'static str,
        /// Frame resources it reads.
        reads: &'static [&'static str],
        /// Frame resources it writes.
        writes: &'static [&'static str],
        /// Its [`GpuTimings::passes`] label, if it is timed.
        timing: Option<&'static str>,
        /// Keys of the pipelines play can switch the pass onto after a warm frame (a TSR tier,
        /// the depth match of a scaled or MSAA frame, a map's outdoor passes): the game-frame
        /// warm reads them from the frame graph and builds each (ADR-2018).
        pipelines: &'static [&'static str],
    },
    /// A slot where registered passes record.
    Slot(GraphSlot),
}

const fn builtin(
    name: &'static str,
    module: &'static str,
    reads: &'static [&'static str],
    writes: &'static [&'static str],
    timing: Option<&'static str>,
) -> FrameStep {
    FrameStep::Builtin {
        name,
        module,
        reads,
        writes,
        timing,
        pipelines: &[],
    }
}

/// `step` with the pipeline keys `keys` ([`FrameStep::Builtin::pipelines`]).
const fn warms(step: FrameStep, keys: &'static [&'static str]) -> FrameStep {
    match step {
        FrameStep::Builtin {
            name,
            module,
            reads,
            writes,
            timing,
            ..
        } => FrameStep::Builtin {
            name,
            module,
            reads,
            writes,
            timing,
            pipelines: keys,
        },
        FrameStep::Slot(slot) => FrameStep::Slot(slot),
    }
}

/// The frame's passes in recording order, with the slots between them (`specs/13` § 5). A pass
/// that a frame's settings leave out still has its row; `render.graph` reports which ones ran.
pub const FRAME_STEPS: &[FrameStep] = &[
    // Phase 1 of the shadow casters' two-phase occlusion (ADR-2072): each culled shadow-map kind's
    // cull against the pyramid its last redraw left, which the shadow passes draw the survivors
    // of. An optional node (ADR-5178): the frame graph holds it only while
    // `FrameRenderer::set_shadow_occlusion` is on. Its dispatches record inside the shadows'
    // record function, before any shadow pass draws.
    builtin(
        "shadow_occlusion",
        "shadow_two_phase",
        &["casters"],
        &["shadow_occlusion"],
        None,
    ),
    builtin(
        "shadows",
        "shadow_passes",
        &["casters", "shadow_occlusion"],
        &["shadow_atlas"],
        Some("shadows"),
    ),
    builtin(
        "cluster_lights",
        "cluster_probes",
        &["lights"],
        &["clusters"],
        Some("clusters"),
    ),
    builtin(
        "gi_probes",
        "cluster_probes",
        &["shadow_atlas"],
        &["probes"],
        Some("gi probes"),
    ),
    // Phase 1 of the depth prepass's two-phase occlusion (ADR-1887): the cull against last
    // frame's pyramid that the prepass draws the survivors of. An optional node (ADR-4905): the
    // frame graph holds it only while `FrameRenderer::set_two_phase_occlusion` is on.
    builtin(
        "prepass_occlusion",
        "prepass_two_phase",
        &["meshes"],
        &["occlusion"],
        None,
    ),
    builtin(
        "depth_prepass",
        "prepass",
        &["meshes", "occlusion"],
        &["depth"],
        Some("prepass"),
    ),
    builtin("gpu_cull", "gpu_cull", &["depth"], &["draw_lists"], Some("cull")),
    // The blue-noise variants (ADR-2075) of this step, SSGI and stochastic SSR.
    warms(
        builtin("ssao", "screen_occlusion", &["depth"], &["ao"], Some("ssao")),
        &["blue_noise"],
    ),
    builtin(
        "contact_shadows",
        "screen_occlusion",
        &["depth"],
        &["contact"],
        Some("contact shadows"),
    ),
    // In the contact shadows' place when the profile traces the sun (ADR-2035): one texture, so
    // this reads what it replaces and the chain keeps both passes live.
    warms(
        builtin(
            "sun_shadow_trace",
            "sun_trace",
            &["depth", "contact"],
            &["contact"],
            Some("contact shadows"),
        ),
        &["sun_shadow_trace"],
    ),
    // Stochastic many-light sampling (ADR-2470, ADR-2652): one resampled local light per pixel
    // for the opaque forward pass. Nothing is recorded unless the profile asks.
    warms(
        builtin(
            "many_light_sampling",
            "many_light",
            &["depth", "clusters"],
            &["light_reservoirs"],
            None,
        ),
        &["restir_di"],
    ),
    warms(
        builtin(
            "visbuffer_resolve",
            "visbuffer_resolve",
            &["depth", "light_reservoirs"],
            &["color"],
            None,
        ),
        &["visbuffer_path"],
    ),
    // A scattering material's draws and their blur, ahead of the forward pass that loads the
    // image: blended surfaces must see the finished skin, and the `after_opaque` slot comes after
    // them (ADR-2049). Nothing is recorded when no such material is visible.
    warms(
        builtin(
            "subsurface_draws",
            "subsurface_gpu",
            &[
                "color",
                "depth",
                "clusters",
                "shadow_atlas",
                "ao",
                "contact",
                "probes",
                "light_reservoirs",
            ],
            &["color", "subsurface_light", "subsurface_albedo"],
            None,
        ),
        &["subsurface"],
    ),
    builtin(
        "subsurface_blur",
        "subsurface_gpu",
        &["color", "subsurface_light", "subsurface_albedo", "depth"],
        &["color"],
        None,
    ),
    warms(
        builtin(
            "forward",
            "forward_pass",
            &[
                "color",
                "depth",
                "draw_lists",
                "clusters",
                "shadow_atlas",
                "ao",
                "contact",
                "probes",
                "light_reservoirs",
            ],
            &["color"],
            Some("forward"),
        ),
        &["forward_sun_rt", "forward_studio"],
    ),
    // Translucent draws of a profile that asks (ADR-2065). They record in the middle of `forward`,
    // ahead of its blended run: glass draws over the composite and refraction sees it. Nothing is
    // recorded when no such material is visible.
    warms(
        builtin(
            "oit_accum",
            "oit_gpu",
            &[
                "depth",
                "draw_lists",
                "clusters",
                "shadow_atlas",
                "ao",
                "contact",
                "probes",
            ],
            &["oit_accum", "oit_revealage"],
            None,
        ),
        &["oit"],
    ),
    builtin(
        "oit_composite",
        "oit_gpu",
        &["color", "oit_accum", "oit_revealage"],
        &["color", "reactive_mask"],
        None,
    ),
    FrameStep::Slot(GraphSlot::AfterOpaque),
    builtin(
        "radiance_cache",
        "screen_space",
        &["probes"],
        &["radiance_cache"],
        None,
    ),
    warms(
        builtin(
            "ssgi",
            "screen_space",
            &["color", "depth", "radiance_cache"],
            &["color"],
            Some("ssgi"),
        ),
        &["ssgi_half_res", "radiance_cache"],
    ),
    builtin(
        "ssr",
        "screen_space",
        &["color", "depth"],
        &["color"],
        Some("ssr"),
    ),
    warms(
        builtin(
            "atmosphere",
            "outdoor_passes",
            &["color", "depth"],
            &["color"],
            Some("atmosphere"),
        ),
        &["outdoor"],
    ),
    warms(
        builtin(
            "clouds",
            "outdoor_passes",
            &["color", "depth"],
            &["color"],
            Some("clouds"),
        ),
        &["outdoor"],
    ),
    warms(
        builtin(
            "fog",
            "outdoor_passes",
            &["color", "depth", "shadow_atlas"],
            &["color"],
            Some("fog"),
        ),
        &["outdoor"],
    ),
    builtin("velocity", "temporal", &["meshes", "depth"], &["velocity"], None),
    warms(
        builtin(
            "taau",
            "temporal",
            &["color", "depth", "velocity", "reactive_mask"],
            &["color"],
            Some("taau"),
        ),
        &["tsr", "taau_resolve"],
    ),
    builtin(
        "translucent_hdr",
        "display_passes",
        &["color", "depth"],
        &["color"],
        None,
    ),
    FrameStep::Slot(GraphSlot::AfterTransparent),
    FrameStep::Slot(GraphSlot::BeforePost),
    builtin("post_stack", "post_stack", &["color"], &["output"], Some("post")),
    warms(
        builtin(
            "translucent_layers",
            "display_passes",
            &["output", "depth"],
            &["output"],
            None,
        ),
        &["depth_match", "depth_match_msaa"],
    ),
    FrameStep::Slot(GraphSlot::AfterPost),
    builtin(
        "scalar_overlays",
        "display_passes",
        &["output", "depth"],
        &["output"],
        None,
    ),
    builtin(
        "gizmos",
        "display_passes",
        &["output", "depth"],
        &["output"],
        None,
    ),
    FrameStep::Slot(GraphSlot::Overlay),
];

/// What the colour target of each slot holds; `None` for a slot [`FrameRenderer::render_into`]
/// does not record registered passes at (every slot is wired today). The three scene-referred
/// slots run only on frames with a linear HDR target.
#[must_use]
pub fn slot_color_space(slot: GraphSlot) -> Option<SlotColorSpace> {
    match slot {
        GraphSlot::AfterOpaque | GraphSlot::AfterTransparent | GraphSlot::BeforePost => {
            Some(SlotColorSpace::SceneLinearHdr)
        }
        GraphSlot::AfterPost | GraphSlot::Overlay => Some(SlotColorSpace::Display),
    }
}

/// `"after_opaque"`, `"after_transparent"`, `"before_post"`, `"after_post"` or `"overlay"`.
#[must_use]
pub fn slot_name(slot: GraphSlot) -> &'static str {
    match slot {
        GraphSlot::AfterOpaque => "after_opaque",
        GraphSlot::AfterTransparent => "after_transparent",
        GraphSlot::BeforePost => "before_post",
        GraphSlot::AfterPost => "after_post",
        GraphSlot::Overlay => "overlay",
    }
}

/// The slot named `name` ([`slot_name`]'s inverse).
#[must_use]
pub fn slot_from_name(name: &str) -> Option<GraphSlot> {
    [
        GraphSlot::AfterOpaque,
        GraphSlot::AfterTransparent,
        GraphSlot::BeforePost,
        GraphSlot::AfterPost,
        GraphSlot::Overlay,
    ]
    .into_iter()
    .find(|slot| slot_name(*slot) == name)
}

/// A registered pass and its counters.
pub(crate) struct RegisteredPass {
    pass: Box<dyn FramePass>,
    runs: u64,
    failures: u64,
    last_cpu_ms: f32,
    last_frame: Option<u64>,
    last_error: Option<String>,
}

/// The registered passes of one renderer, in registration order.
pub(crate) struct ExtensionPasses {
    passes: Vec<RegisteredPass>,
    /// The frame graph of [`FRAME_STEPS`] with these passes (ADR-2018), rebuilt when a pass is
    /// registered or removed.
    graph: super::frame_graph::FrameGraph,
    /// The optional built-in nodes the graph holds (ADR-4905).
    options: super::frame_graph::FrameGraphOptions,
    /// The frame's built-in pass dispatch over [`Self::graph`] (ADR-4905).
    pub(crate) dispatch: super::frame_graph::FrameDispatch,
    /// An armed request for read-only copies at the built-in pass boundaries (ADR-7860).
    /// `None` (the default) is the whole cost of the feature when nothing asked for it.
    pub(crate) pass_copies: Option<Box<crate::frame_capture::PassCopies>>,
}

/// One registered pass in [`FrameGraphReport::extensions`].
#[derive(Debug, Clone, PartialEq)]
pub struct FramePassInfo {
    /// [`FramePass::name`].
    pub name: String,
    /// [`FramePass::slot`].
    pub slot: GraphSlot,
    /// The slot records registered passes today ([`slot_color_space`] is `Some`).
    pub wired: bool,
    /// [`FramePass::reads`].
    pub reads: Vec<&'static str>,
    /// [`FramePass::writes`].
    pub writes: Vec<&'static str>,
    /// Frames the pass recorded in.
    pub runs: u64,
    /// Frames the pass returned an error in (skipped).
    pub failures: u64,
    /// CPU time of its last `record`, in milliseconds.
    pub last_cpu_ms: f32,
    /// The renderer frame index it last ran in.
    pub last_frame: Option<u64>,
    /// Its last error message.
    pub last_error: Option<String>,
}

/// [`FrameRenderer::graph_report`]: the frame's steps, the registered passes and the last GPU
/// timings.
#[derive(Debug, Clone)]
pub struct FrameGraphReport {
    /// [`FRAME_STEPS`].
    pub steps: &'static [FrameStep],
    /// The registered passes in registration order.
    pub extensions: Vec<FramePassInfo>,
    /// The last frame's GPU timings (zeros while GPU timing is off).
    pub gpu: GpuTimings,
    /// GPU timing is on ([`FrameRenderer::set_gpu_timing`]).
    pub gpu_timing: bool,
    /// The forward shader permutations this process composed ([`crate::forward_permutation_report`]).
    pub forward_permutations: crate::ForwardPermutationReport,
    /// The frame's compiled render graph: versioned nodes, execution order, warm keys and the
    /// transient pool (ADR-2018).
    pub graph: super::frame_graph::FrameGraphInfo,
    /// How the last frames recorded their built-in passes through the graph (ADR-4905).
    pub dispatch: super::frame_graph::FrameDispatchInfo,
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

impl Default for ExtensionPasses {
    fn default() -> Self {
        let options = super::frame_graph::FrameGraphOptions::default();
        let graph = super::frame_graph::FrameGraph::build(&[], options);
        let mut dispatch = super::frame_graph::FrameDispatch::default();
        dispatch.bind(&graph);
        Self {
            passes: Vec::new(),
            graph,
            options,
            dispatch,
            pass_copies: None,
        }
    }
}

impl ExtensionPasses {
    /// Rebuilds the frame graph from [`FRAME_STEPS`] and the registered passes.
    fn rebuild_graph(&mut self) {
        let decls: Vec<super::frame_graph::PassDecl<'_>> = self
            .passes
            .iter()
            .map(|p| super::frame_graph::PassDecl {
                name: p.pass.name(),
                slot: p.pass.slot(),
                reads: p.pass.reads(),
                writes: p.pass.writes(),
                pipelines: p.pass.pipelines(),
            })
            .collect();
        self.graph = super::frame_graph::FrameGraph::build(&decls, self.options);
        self.dispatch.bind(&self.graph);
    }

    /// Switches the optional built-in nodes (ADR-4905); rebuilds the graph when they change.
    pub(crate) fn set_graph_options(&mut self, options: super::frame_graph::FrameGraphOptions) {
        if self.options != options {
            self.options = options;
            self.rebuild_graph();
        }
    }

    /// Moves `from`'s registered passes, with their counters, here (a renderer rebuilt for a
    /// shader edit keeps the host's passes) and rebuilds the graph.
    pub(crate) fn carry_passes(&mut self, from: &mut Self) {
        self.passes = std::mem::take(&mut from.passes);
        // A pending pass-boundary capture follows the passes (ADR-7860).
        self.pass_copies = from.pass_copies.take();
        from.rebuild_graph();
        self.rebuild_graph();
    }

    /// The optional built-in nodes the graph holds now.
    pub(crate) fn graph_options(&self) -> super::frame_graph::FrameGraphOptions {
        self.options
    }

    /// The frame graph.
    pub(crate) fn graph(&self) -> &super::frame_graph::FrameGraph {
        &self.graph
    }

    /// `true` when no pass is registered: the frame skips every slot without touching anything.
    pub(crate) fn is_empty(&self) -> bool {
        self.passes.is_empty()
    }

    fn register(&mut self, pass: Box<dyn FramePass>) -> Result<(), PassRegisterError> {
        let name = pass.name().to_string();
        if !valid_name(&name) {
            return Err(PassRegisterError::BadName(name));
        }
        let builtin_clash = FRAME_STEPS
            .iter()
            .any(|step| matches!(step, FrameStep::Builtin { name: b, .. } if *b == name));
        if builtin_clash || self.passes.iter().any(|p| p.pass.name() == name) {
            return Err(PassRegisterError::DuplicateName(name));
        }
        self.passes.push(RegisteredPass {
            pass,
            runs: 0,
            failures: 0,
            last_cpu_ms: 0.0,
            last_frame: None,
            last_error: None,
        });
        self.rebuild_graph();
        Ok(())
    }

    fn remove(&mut self, name: &str) -> Option<Box<dyn FramePass>> {
        let index = self.passes.iter().position(|p| p.pass.name() == name)?;
        let pass = self.passes.remove(index).pass;
        self.rebuild_graph();
        Some(pass)
    }

    fn infos(&self) -> Vec<FramePassInfo> {
        self.passes
            .iter()
            .map(|p| {
                let slot = p.pass.slot();
                FramePassInfo {
                    name: p.pass.name().to_string(),
                    slot,
                    wired: slot_color_space(slot).is_some(),
                    reads: p.pass.reads().to_vec(),
                    writes: p.pass.writes().to_vec(),
                    runs: p.runs,
                    failures: p.failures,
                    last_cpu_ms: p.last_cpu_ms,
                    last_frame: p.last_frame,
                    last_error: p.last_error.clone(),
                }
            })
            .collect()
    }

    /// [`FramePass::prewarm`] of every registered pass; returns the pipelines they asked for.
    /// A failing pass is logged and skipped.
    pub(crate) fn prewarm(&mut self, device: &TrackedDevice) -> usize {
        let mut asked = 0;
        for entry in &mut self.passes {
            match entry.pass.prewarm(device) {
                Ok(count) => asked += count,
                Err(err) => tracing::warn!(
                    target: "regia_render",
                    "frame pass `{}` prewarm failed (it builds on first use): {err}",
                    entry.pass.name()
                ),
            }
        }
        asked
    }

    /// Records every registered pass of `slot` in the frame graph's order (registration order
    /// within the slot). The caller checks [`Self::is_empty`] first, so a frame without
    /// extensions never builds a context.
    pub(crate) fn run_slot(&mut self, slot: GraphSlot, target: &mut SlotTarget<'_>) {
        let Some(color_space) = slot_color_space(slot) else {
            return;
        };
        for &index in self.graph.slot_order(slot) {
            let Some(entry) = self.passes.get_mut(index) else {
                continue;
            };
            let _span = tracing::info_span!("extension_pass", name = entry.pass.name()).entered();
            // One breadcrumb scope per registered graph node (spec 54 § 3, ADR-9070).
            let _crumb = self.graph.pass_tag(index).map(crate::breadcrumbs::frame_pass_tag);
            let mut ctx = FramePassContext {
                device: target.device,
                encoder: &mut *target.encoder,
                slot,
                color: target.color,
                color_format: target.color_format,
                color_size: target.color_size,
                color_space,
                viewport: target.viewport,
                depth: target.depth,
                depth_multisampled: target.depth_multisampled,
                clip_from_world: target.clip_from_world,
                clip_from_view: target.clip_from_view,
                eye: target.eye,
                pre_exposure: target.pre_exposure,
                frame_index: target.frame_index,
                texture_view: target.texture_view,
                post: target.post.map(|post| post.settings),
                exposure: target.post.map(|post| post.exposure),
                queue: target.queue,
            };
            let started = std::time::Instant::now();
            let result = entry.pass.record(&mut ctx);
            entry.last_cpu_ms = started.elapsed().as_secs_f32() * 1000.0;
            entry.last_frame = Some(target.frame_index);
            match result {
                Ok(()) => entry.runs += 1,
                Err(err) => {
                    tracing::warn!(
                        target: "regia_render",
                        "frame pass `{}` skipped: {err}",
                        entry.pass.name()
                    );
                    entry.failures += 1;
                    entry.last_error = Some(err);
                }
            }
        }
    }
}

/// The frame's state at a slot, built by `render_into` only when a pass is registered.
pub(crate) struct SlotTarget<'a> {
    pub(crate) device: &'a TrackedDevice,
    pub(crate) queue: &'a crate::device::TrackedQueue,
    pub(crate) encoder: &'a mut wgpu::CommandEncoder,
    pub(crate) color: &'a wgpu::TextureView,
    pub(crate) color_format: wgpu::TextureFormat,
    pub(crate) color_size: (u32, u32),
    pub(crate) viewport: [f32; 4],
    pub(crate) depth: &'a wgpu::TextureView,
    pub(crate) depth_multisampled: bool,
    pub(crate) clip_from_world: Mat4,
    pub(crate) clip_from_view: Mat4,
    pub(crate) eye: [f32; 3],
    pub(crate) pre_exposure: f32,
    pub(crate) frame_index: u64,
    pub(crate) texture_view: Option<u64>,
    pub(crate) post: Option<SlotPost<'a>>,
}

/// What the post stack hands the `before_post` slot about its view.
#[derive(Clone, Copy)]
pub(crate) struct SlotPost<'a> {
    /// The view's resolved post settings.
    pub(crate) settings: &'a crate::PostSettings,
    /// The exposure the post stack applies ([`FramePassContext::exposure`]).
    pub(crate) exposure: f32,
}

/// The frame's view of one slot, handed to `FrameRenderer::run_extension_slot` by
/// `render_into`.
pub(crate) struct SlotView<'a> {
    pub(crate) color: &'a wgpu::TextureView,
    pub(crate) color_format: wgpu::TextureFormat,
    pub(crate) color_size: (u32, u32),
    pub(crate) viewport: [f32; 4],
    pub(crate) msaa_depth: &'a Option<wgpu::TextureView>,
    pub(crate) depth_view: &'a wgpu::TextureView,
    pub(crate) clip_from_world: Mat4,
    pub(crate) clip_from_view: Mat4,
    pub(crate) eye: [f32; 3],
    pub(crate) pre_exposure: f32,
}

/// The frame's depth and camera that every [`SlotView`] of one frame carries (built once by
/// `render_into` before its record sites, ADR-4945).
#[derive(Clone, Copy)]
pub(crate) struct SlotCamera<'a> {
    pub(crate) msaa_depth: &'a Option<wgpu::TextureView>,
    pub(crate) depth_view: &'a wgpu::TextureView,
    pub(crate) clip_from_world: Mat4,
    pub(crate) clip_from_view: Mat4,
    pub(crate) eye: [f32; 3],
    pub(crate) pre_exposure: f32,
}

impl<'a> SlotCamera<'a> {
    /// The frame's depth views and its unjittered camera from `modes`.
    pub(in crate::frame) fn new(
        msaa_depth: &'a Option<wgpu::TextureView>,
        depth_view: &'a wgpu::TextureView,
        modes: &super::steps::modes::FrameModes,
        eye: [f32; 3],
    ) -> Self {
        Self {
            msaa_depth,
            depth_view,
            clip_from_world: modes.unjittered_vp,
            clip_from_view: modes.unjittered_proj,
            eye,
            pre_exposure: modes.pre_exposure,
        }
    }
}

impl<'a> SlotView<'a> {
    /// The view of an HDR image (`after_opaque`, `after_transparent`): the whole texture, at its
    /// format and size.
    pub(crate) fn whole(color: &'a wgpu::TextureView, camera: SlotCamera<'a>) -> Self {
        let size = color.texture().size();
        Self {
            color,
            color_format: color.texture().format(),
            color_size: (size.width, size.height),
            viewport: [0.0, 0.0, size.width as f32, size.height as f32],
            ..Self::with(color, camera)
        }
    }

    /// The view of the display target (`after_post`, `overlay`) with the frame's viewport.
    pub(crate) fn display(
        target: &super::FrameTarget<'a>,
        viewport: [f32; 4],
        camera: SlotCamera<'a>,
    ) -> Self {
        Self {
            color_format: target.format,
            color_size: (target.width, target.height),
            viewport,
            ..Self::with(target.view, camera)
        }
    }

    fn with(color: &'a wgpu::TextureView, camera: SlotCamera<'a>) -> Self {
        Self {
            color,
            color_format: wgpu::TextureFormat::Rgba8Unorm,
            color_size: (0, 0),
            viewport: [0.0; 4],
            msaa_depth: camera.msaa_depth,
            depth_view: camera.depth_view,
            clip_from_world: camera.clip_from_world,
            clip_from_view: camera.clip_from_view,
            eye: camera.eye,
            pre_exposure: camera.pre_exposure,
        }
    }
}

impl FrameRenderer {
    /// Records the registered passes of `slot`. Returns at once (no work, no allocation) when
    /// none is registered or a reflection probe is being captured.
    pub(crate) fn run_extension_slot(
        &mut self,
        slot: GraphSlot,
        device: &TrackedDevice,
        queue: &crate::device::TrackedQueue,
        encoder: &mut wgpu::CommandEncoder,
        view: &SlotView<'_>,
    ) {
        let _ = self.run_extension_slot_with_post(slot, device, queue, encoder, view, None);
    }

    /// [`Self::run_extension_slot`] with the view's resolved post settings and exposure in
    /// [`FramePassContext::post`] and [`FramePassContext::exposure`] (the `before_post` slot,
    /// ADR-1863). Returns the per-frame copy
    /// of the colour target the passes wrote when they were handed one (`before_post` with a
    /// writing pass): the caller reads that copy from then on.
    pub(crate) fn run_extension_slot_with_post(
        &mut self,
        slot: GraphSlot,
        device: &TrackedDevice,
        queue: &crate::device::TrackedQueue,
        encoder: &mut wgpu::CommandEncoder,
        view: &SlotView<'_>,
        post: Option<SlotPost<'_>>,
    ) -> Option<wgpu::TextureView> {
        let frame = SlotFrame {
            capturing_probe: self.capturing_probe,
            frame_index: self.frame_index,
            texture_view: self.view_pass,
        };
        self.extension_passes.run_slot_view_with_post(
            slot,
            frame,
            (device, queue, encoder),
            view,
            post,
            Some(&mut self.transients),
        )
    }
}

/// The renderer state a slot run needs besides the passes (copied, so the runner can borrow only
/// the pass list while the frame holds other renderer fields).
#[derive(Clone, Copy)]
pub(crate) struct SlotFrame {
    pub(crate) capturing_probe: bool,
    pub(crate) frame_index: u64,
    pub(crate) texture_view: Option<u64>,
}

impl ExtensionPasses {
    /// [`FrameRenderer::run_extension_slot`] over the pass list alone.
    pub(crate) fn run_slot_view(
        &mut self,
        slot: GraphSlot,
        frame: SlotFrame,
        device: &TrackedDevice,
        queue: &crate::device::TrackedQueue,
        encoder: &mut wgpu::CommandEncoder,
        view: &SlotView<'_>,
    ) {
        let _ = self.run_slot_view_with_post(slot, frame, (device, queue, encoder), view, None, None);
    }

    /// [`Self::run_slot_view`] with the view's post state for [`FramePassContext::post`] and
    /// [`FramePassContext::exposure`], and the view's transient pool, which the `before_post` copy
    /// is taken from (ADR-4945; without a pool the passes record against the image itself).
    pub(crate) fn run_slot_view_with_post(
        &mut self,
        slot: GraphSlot,
        frame: SlotFrame,
        (device, queue, encoder): (
            &TrackedDevice,
            &crate::device::TrackedQueue,
            &mut wgpu::CommandEncoder,
        ),
        view: &SlotView<'_>,
        post: Option<SlotPost<'_>>,
        pool: Option<&mut crate::TransientPool>,
    ) -> Option<wgpu::TextureView> {
        if self.is_empty() || frame.capturing_probe {
            return None;
        }
        let (depth, depth_multisampled) = match view.msaa_depth.as_ref() {
            Some(msaa) => (msaa, true),
            None => (view.depth_view, false),
        };
        let frame_index = frame.frame_index;
        let texture_view = frame.texture_view;
        // `before_post` passes that write get a copy of the HDR image (see `copy_before_post`);
        // the caller then reads the copy. Nothing is copied when no pass writes this frame.
        let settings = post.map(|post| post.settings);
        let copy = (slot == GraphSlot::BeforePost
            && self
                .passes
                .iter()
                .any(|p| p.pass.slot() == slot && p.pass.writes_color(settings)))
        .then_some(pool)
        .flatten()
        .and_then(|pool| copy_before_post(pool, device, encoder, view.color));
        self.run_slot(
            slot,
            &mut SlotTarget {
                device,
                queue,
                encoder,
                color: copy.as_ref().unwrap_or(view.color),
                color_format: view.color_format,
                color_size: view.color_size,
                viewport: view.viewport,
                depth,
                depth_multisampled,
                clip_from_world: view.clip_from_world,
                clip_from_view: view.clip_from_view,
                eye: view.eye,
                pre_exposure: view.pre_exposure,
                frame_index,
                texture_view,
                post,
            },
        );
        copy
    }
}

/// Copies `color` into the per-frame `before_post` image and returns that image; `None` when
/// `color` cannot be copied (the passes then record against it as before). The image the post
/// stack reads may be the temporal resolve's history or the held-screen cache, which the next
/// frame reads again, so a pass writing it in place would accumulate (ADR-1863). The copy is a
/// frame transient of the view's pool (ADR-4945): one per size and format a frame needs, the same
/// texture every frame, dropped after the pool's idle frames.
fn copy_before_post(
    pool: &mut crate::TransientPool,
    device: &TrackedDevice,
    encoder: &mut wgpu::CommandEncoder,
    color: &wgpu::TextureView,
) -> Option<wgpu::TextureView> {
    let source = color.texture();
    if !source.usage().contains(wgpu::TextureUsages::COPY_SRC)
        || source.sample_count() != 1
        || source.mip_level_count() != 1
        || source.depth_or_array_layers() != 1
    {
        return None;
    }
    let size = source.size();
    let desc = crate::TextureDesc::new(size.width, size.height, source.format()).with_usage(
        wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_SRC
            | wgpu::TextureUsages::COPY_DST,
    );
    let copy = pool.acquire(device, &desc, "before_post copy");
    encoder.copy_texture_to_texture(source.as_image_copy(), copy.texture.as_image_copy(), size);
    Some(copy.view)
}

impl FrameRenderer {
    /// Adds `pass` to the frame graph at [`FramePass::slot`], after the passes already
    /// registered there. It records from the next [`Self::render_into`] on.
    ///
    /// # Errors
    /// [`PassRegisterError::BadName`] when the name is not lowercase ascii letters, digits and
    /// `_`; [`PassRegisterError::DuplicateName`] when a built-in pass ([`FRAME_STEPS`]) or a
    /// registered pass already has it.
    pub fn register_pass(&mut self, pass: Box<dyn FramePass>) -> Result<(), PassRegisterError> {
        self.extension_passes.register(pass)
    }

    /// Removes the registered pass named `name` and returns it (`None` when there is none).
    pub fn remove_pass(&mut self, name: &str) -> Option<Box<dyn FramePass>> {
        self.extension_passes.remove(name)
    }

    /// Names of the registered passes in registration order.
    #[must_use]
    pub fn registered_passes(&self) -> Vec<String> {
        self.extension_passes
            .passes
            .iter()
            .map(|p| p.pass.name().to_string())
            .collect()
    }

    /// The frame's steps, the registered passes with their counters, and the last GPU timings
    /// (`render.graph`).
    #[must_use]
    pub fn graph_report(&self) -> FrameGraphReport {
        FrameGraphReport {
            steps: FRAME_STEPS,
            extensions: self.extension_passes.infos(),
            gpu: self.gpu_timings(),
            gpu_timing: self.gpu_timing_enabled(),
            forward_permutations: crate::forward_permutation_report(),
            // The keyed pool (the two HDR scratch images) and both pools' listing (ADR-7877).
            graph: super::frame_graph::FrameGraphInfo {
                buffers: self.graph_buffer_report(),
                keyed: self.keyed_transients.stats(),
                pooled: [self.transients.entry_info(), self.keyed_transients.entry_info()].concat(),
                ..self.extension_passes.graph().info(self.transients.stats())
            },
            dispatch: self.extension_passes.dispatch.info(),
        }
    }

    /// The frame reaches the record site of the built-in node `node`, whose per-frame switch is
    /// `active` (ADR-4905). Returns whether to record it: the frame graph holds the node, the
    /// switch is on, and it was not recorded already this frame. A node the graph does not hold
    /// records nothing and creates nothing.
    pub(in crate::frame) fn dispatch_node(
        &mut self,
        node: super::frame_graph::BuiltinNode,
        active: bool,
    ) -> bool {
        self.extension_passes.dispatch.enter(node, active)
    }

    /// [`Self::dispatch_node`] for the always-on built-in nodes one record site records, in their
    /// order. Returns whether every one of them is to record.
    pub(in crate::frame) fn dispatch_nodes(&mut self, nodes: &[super::frame_graph::BuiltinNode]) -> bool {
        self.extension_passes.dispatch.enter_all(nodes)
    }

    /// Asks for read-only copies at the built-in pass boundaries of the next main-view frame
    /// (ADR-7860; [`crate::frame_capture`]). Only that frame copies; with no request the frame
    /// does no work for the feature. Collect the copies with
    /// [`crate::frame_capture::CaptureSession::arm_with_passes`]'s `finish`, or drop them with
    /// [`Self::cancel_pass_capture`].
    ///
    /// # Errors
    /// A requested name is not a built-in pass, or a request is already armed.
    pub fn request_pass_capture(
        &mut self,
        request: &crate::frame_capture::PassCaptureRequest,
    ) -> Result<(), crate::frame_capture::PassCaptureError> {
        if self.extension_passes.pass_copies.is_some() {
            return Err(crate::frame_capture::PassCaptureError::Pending);
        }
        let filter = request.resolve()?;
        self.extension_passes.pass_copies = Some(Box::new(crate::frame_capture::PassCopies::new(
            filter,
            request.budget_bytes,
        )));
        Ok(())
    }

    /// Drops an armed pass-boundary request and any copies it recorded. Returns whether one was
    /// armed.
    pub fn cancel_pass_capture(&mut self) -> bool {
        self.extension_passes.pass_copies.take().is_some()
    }

    /// A pass-boundary request is armed ([`Self::request_pass_capture`]).
    #[must_use]
    pub fn pass_capture_requested(&self) -> bool {
        self.extension_passes.pass_copies.is_some()
    }

    /// Read-only copies of what the record site that just ran wrote, only while a capture asked
    /// for them (ADR-7860; `render_into`'s site loop). With no request this is one `None` check:
    /// no copy, no allocation, no barrier. `colors` is the scene color in the order it is
    /// replaced (upscaled, shaded, the HDR target); none of them is a display-referred frame,
    /// whose color is the `target`.
    #[inline]
    pub(in crate::frame) fn copy_site(
        &mut self,
        d: &crate::TrackedDevice,
        rec: &mut super::steps::recording::PassRecording,
        colors: [&Option<wgpu::TextureView>; 3],
        depth: &wgpu::TextureView,
        target: &super::FrameTarget<'_>,
        modes: &super::steps::modes::FrameModes,
    ) {
        let Some(copies) = self.extension_passes.pass_copies.as_deref_mut() else {
            return;
        };
        if !self.extension_passes.dispatch.main_view() {
            return;
        }
        let color = colors.into_iter().find_map(Option::as_ref);
        let dispatch = &self.extension_passes.dispatch;
        let mark = copies.mark();
        copies.record_site(
            d,
            &mut rec.encoder,
            dispatch.recorded_since(mark),
            dispatch.recorded_len(),
            &crate::frame_capture::Boundary {
                frame: self.frame_index,
                color: color.unwrap_or(target.view),
                color_display: color.is_none(),
                depth,
                output: target.view,
                scale: modes.post_exposure / modes.pre_exposure.max(f32::MIN_POSITIVE),
            },
        );
    }

    /// Takes the armed request with its recorded copies (`frame_capture`'s `finish`).
    pub(crate) fn take_pass_copies(&mut self) -> Option<Box<crate::frame_capture::PassCopies>> {
        self.extension_passes.pass_copies.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Named(&'static str, GraphSlot);

    impl FramePass for Named {
        fn name(&self) -> &str {
            self.0
        }
        fn slot(&self) -> GraphSlot {
            self.1
        }
        fn record(&mut self, _ctx: &mut FramePassContext<'_>) -> Result<(), String> {
            Ok(())
        }
    }

    /// A shader edit rebuilds the renderer: the host's registered passes, two-phase occlusion
    /// and the Hi-Z switch must survive it (only shadow occlusion was carried before).
    #[test]
    fn a_shader_hot_swap_keeps_registered_passes_and_occlusion_switches() {
        let mut renderer = FrameRenderer::default();
        renderer
            .register_pass(Box::new(Named("outline", GraphSlot::AfterPost)))
            .expect("register");
        renderer.set_two_phase_occlusion(true);
        renderer.set_hiz_occlusion(false);
        renderer.shader_epoch = crate::shader_reload::shader_epoch().wrapping_add(1);
        assert!(renderer.sync_shader_epoch(), "the epoch moved");
        assert_eq!(renderer.registered_passes(), vec!["outline".to_string()]);
        assert!(renderer.two_phase_occlusion(), "two-phase occlusion turned off");
        assert!(
            renderer.extension_passes.graph_options().two_phase_occlusion,
            "the graph lost its prepass_occlusion node"
        );
        assert!(!renderer.hiz_occlusion(), "Hi-Z occlusion turned back on");
    }

    /// Agent-set switches and the installed lightmap survive a shader hot swap too.
    #[test]
    fn a_shader_hot_swap_keeps_agent_switches_and_the_lightmap() {
        let mut renderer = FrameRenderer::default();
        let lite = Some(crate::forward_lite::ForwardLite::Low);
        renderer.set_forward_lite(lite);
        renderer.set_texture_set_census(!renderer.texture_set_census());
        let census = renderer.texture_set_census();
        renderer.set_camera_cut_detection(None);
        let map = std::sync::Arc::new(
            regia_asset::lightmap::Lightmap::from_irradiance(
                regia_asset::lightmap::LightmapLayout {
                    width: 1,
                    height: 1,
                    density: 1.0,
                    padding: 0,
                    entries: Vec::new(),
                    unwraps: Vec::new(),
                },
                &[[1.0; 3]],
                vec![regia_asset::lightmap::Coverage::BAKED],
                regia_asset::lightmap::LightmapBakeInfo::default(),
            )
            .expect("a 1x1 lightmap"),
        );
        renderer.set_lightmap(
            Some(std::sync::Arc::clone(&map)),
            crate::lightmap::LightmapSettings::default(),
        );
        renderer.shader_epoch = crate::shader_reload::shader_epoch().wrapping_add(1);
        assert!(renderer.sync_shader_epoch());
        assert_eq!(renderer.forward_lite_override(), lite);
        assert_eq!(renderer.texture_set_census(), census);
        assert_eq!(renderer.camera_cut_detection(), None);
        assert!(
            renderer
                .lightmap()
                .is_some_and(|m| std::sync::Arc::ptr_eq(m, &map)),
            "the lightmap was dropped"
        );
    }

    #[test]
    fn registration_refuses_bad_and_duplicate_names() {
        let mut passes = ExtensionPasses::default();
        assert!(passes.is_empty());
        passes
            .register(Box::new(Named("outline", GraphSlot::AfterPost)))
            .expect("first registration");
        assert!(!passes.is_empty());
        let duplicate = passes
            .register(Box::new(Named("outline", GraphSlot::Overlay)))
            .expect_err("same name twice");
        assert_eq!(duplicate.code(), "pass_duplicate_name");
        let builtin = passes
            .register(Box::new(Named("post_stack", GraphSlot::Overlay)))
            .expect_err("a built-in name");
        assert_eq!(builtin, PassRegisterError::DuplicateName("post_stack".into()));
        for bad in ["", "Outline", "out line", "outline-2"] {
            let err = passes
                .register(Box::new(Named(bad, GraphSlot::Overlay)))
                .expect_err("bad name");
            assert_eq!(err.code(), "pass_bad_name", "{bad:?}");
        }
        assert!(passes.remove("missing").is_none());
        assert!(passes.remove("outline").is_some());
        assert!(passes.is_empty());
    }

    /// Review fixrev-graph-dispatch-g finding 3 (ADR-5626): a dev shader hot swap rebuilds the
    /// renderer's GPU state, and the host's registered passes and both two-phase switches (with
    /// their optional graph nodes) survive it.
    #[test]
    fn a_shader_hot_swap_keeps_registered_passes_and_the_two_phase_nodes() {
        let mut renderer = crate::FrameRenderer::new();
        renderer
            .register_pass(Box::new(Named("outline", GraphSlot::Overlay)))
            .expect("register");
        renderer.set_two_phase_occlusion(true);
        renderer.set_shadow_occlusion(true);
        let before = renderer.graph_report().graph.order;
        for node in ["outline", "prepass_occlusion", "shadow_occlusion"] {
            assert!(before.iter().any(|n| n == node), "{node}: {before:?}");
        }
        crate::shader_reload::clear_shader_overrides();
        assert!(renderer.sync_shader_epoch(), "the swap rebuilt the renderer");
        assert_eq!(renderer.registered_passes(), ["outline"]);
        assert!(renderer.two_phase_occlusion());
        assert!(renderer.shadow_occlusion());
        assert_eq!(renderer.graph_report().graph.order, before);
        // The carried pass is the renderer's own again: it can be removed.
        assert!(renderer.remove_pass("outline").is_some());
        assert!(renderer.registered_passes().is_empty());
    }

    #[test]
    fn infos_keep_registration_order_and_report_wiring() {
        let mut passes = ExtensionPasses::default();
        passes
            .register(Box::new(Named("late", GraphSlot::Overlay)))
            .expect("late");
        passes
            .register(Box::new(Named("early", GraphSlot::AfterOpaque)))
            .expect("early");
        let infos = passes.infos();
        assert_eq!(infos.len(), 2);
        assert_eq!((infos[0].name.as_str(), infos[0].wired), ("late", true));
        assert_eq!((infos[1].name.as_str(), infos[1].wired), ("early", true));
        assert_eq!(infos[0].runs, 0);
    }

    #[test]
    fn every_slot_appears_once_in_the_frame_steps_and_round_trips_its_name() {
        for slot in [
            GraphSlot::AfterOpaque,
            GraphSlot::AfterTransparent,
            GraphSlot::BeforePost,
            GraphSlot::AfterPost,
            GraphSlot::Overlay,
        ] {
            let count = FRAME_STEPS
                .iter()
                .filter(|step| **step == FrameStep::Slot(slot))
                .count();
            assert_eq!(count, 1, "{slot:?}");
            assert_eq!(slot_from_name(slot_name(slot)), Some(slot));
        }
        assert_eq!(slot_from_name("before_opaque"), None);
    }

    /// The written size ceiling of `frame.rs` (ADR-1552): passes move out into `frame/<pass>.rs`,
    /// new passes go there or register at a slot, so the file must not grow back.
    #[test]
    fn frame_rs_stays_under_its_written_ceiling() {
        const CEILING_LINES: usize = 10_000;
        let lines = include_str!("../frame.rs").lines().count();
        assert!(
            lines <= CEILING_LINES,
            "frame.rs has {lines} lines, over its {CEILING_LINES}-line ceiling: put the new pass in its own frame/ module"
        );
    }

    /// The written size ceiling of `render_into` itself (ADR-1562): its setup steps moved out
    /// into named `frame/` methods (draw list, shadow setup, targets, uniforms, frame end), so a
    /// new step goes into a module of its own and the function must not grow back. ADR-2300
    /// moved the frame's opening, modes, view group, pipelines, depth passes, screen reuse,
    /// receiver stamps and submit into `frame/steps/` (2398 lines to 1470) and lowered the
    /// ceiling to that size plus 100. ADR-4856 moved the frame's opening, modes and recording
    /// state into the frame recording context (`steps::recording`, 1543 lines to 1486) and
    /// lowered the ceiling to 1530: that size plus the render_into lines of the frame lanes
    /// still waiting to merge.
    #[test]
    fn render_into_stays_under_its_written_ceiling() {
        const CEILING_LINES: usize = 1_530;
        let source = include_str!("../frame.rs");
        let start = source
            .find("    pub fn render_into(")
            .expect("render_into in frame.rs");
        let lines = source[start..]
            .lines()
            .position(|line| line == "    }")
            .expect("end of render_into")
            + 1;
        assert!(
            lines <= CEILING_LINES,
            "render_into has {lines} lines, over its {CEILING_LINES}-line ceiling: move the new step into a frame/ module"
        );
    }

    /// The body of `render_into` in `frame.rs`, from its signature to its closing brace.
    fn render_into_body() -> &'static str {
        let source = include_str!("../frame.rs");
        let start = source
            .find("    pub fn render_into(")
            .expect("render_into in frame.rs");
        let len = source[start..].find("\n    }\n").expect("end of render_into");
        &source[start..start + len]
    }

    /// The frame recording context (ADR-4856): `render_into` keeps the frame's opening, modes
    /// and shadow CPU timings in one `FrameRecording`, and the encoder, its segments, the timer,
    /// the stamp flags and the bind, submit and draw counters in one `PassRecording`, so a pass
    /// callback can be handed the frame's state instead of a list of locals. The step results
    /// are not destructured back into locals, and no recording local is declared beside them.
    #[test]
    fn render_into_records_through_its_frame_context() {
        let body = render_into_body();
        for opened in [
            "steps::recording::FrameRecording",
            "steps::recording::PassRecording",
        ] {
            assert!(body.contains(opened), "render_into does not open {opened}");
        }
        for destructured in ["FrameOpen {", "FrameModes {"] {
            assert!(
                !body.contains(destructured),
                "render_into destructures {destructured}: keep it in the frame context"
            );
        }
        for local in [
            "let mut encoder",
            "let mut segments",
            "let timer",
            "let in_pass_stamps",
            "let mut shared_depth_stamped",
            "let mut shared_color_stamped",
            "let mut glass_stamped",
            "let trims",
            "let mut binds",
            "let mut submit_stats",
            "let mut draw_counts",
            "let mut shadow_cpu",
            "let mut readback_ms",
        ] {
            assert!(
                !body.contains(local),
                "render_into declares `{local}`: it belongs to the frame context"
            );
        }
    }

    /// The written step order of `render_into`: its setup steps, passes, slots and submit, as
    /// they appear in the function. Moving the frame's state into its context (ADR-4856) and
    /// later moving built-ins into graph callbacks must not reorder them; a step added or
    /// removed updates this list and `FRAME_STEPS` in the same change.
    #[test]
    fn render_into_keeps_its_written_step_order() {
        const ORDER: &[&str] = &[
            "self.sync_shader_epoch(",
            "self.open_frame(",
            "self.begin_encode(",
            "self.frame_modes(",
            "self.frame_lights(",
            "self.ensure_gpu_state(",
            "self.write_frame_uniforms(",
            "self.pack_skin_palette(",
            "self.select_sun_variant(",
            "self.build_draw_list(",
            "self.plan_shadow_casters(",
            "self.prepare_shared_frame(",
            "self.build_shadow_jobs(",
            "self.build_instance_lists(",
            "self.many_light.begin(",
            "self.frame_view_group(",
            "self.frame_pipelines(",
            "targets::frame_targets(",
            "shared_engine::set_shared_engine_inputs(",
            // The frame's encoder (`steps::recording::PassRecording::begin`).
            "PassRecording::begin(",
            "self.belt.flush(",
            "self.skin_compute.encode(",
            "morph_gpu::encode(",
            "encode_before_passes(",
            "self.outdoor.encode_cloud_shadow_map(",
            "shadow_passes::record_vis_shadow_cull(",
            "shadow_passes::record_shadow_passes(",
            "shadow_passes::record_translucent_shadows(",
            "self.weather",
            "self.finish_workers.cut(",
            "cluster_probes::record_cluster_and_probes(",
            "steps::forward_pipes::FramePipeClones::take(",
            "self.record_depth_passes(",
            "self.screen_reuse(",
            "screen_occlusion::record_screen_occlusion(",
            "self.many_light.record(",
            "self.finish_workers.cut(",
            "forward_pass::prepare_gpu_color(",
            "self.lightmap_forward.prepare_instanced(",
            "forward_pass::count_forward_draws(",
            "visbuffer_resolve::record_visbuffer_resolve(",
            "subsurface_gpu::record_subsurface(",
            "self.prepare_refraction(",
            "oit_gpu::prepare_oit(",
            "forward_pass::record_forward(",
            "self.oit.finish(",
            "encode_after_color(",
            "crate::GraphSlot::AfterOpaque",
            "self.record_screen_space(",
            "self.record_outdoor(",
            "self.record_temporal(",
            "self.simulate_translucent_layers(",
            "self.receiver_stamps(",
            "self.record_translucent_hdr(",
            "crate::GraphSlot::AfterTransparent",
            "self.record_post_stack(",
            "self.record_translucent_layers(",
            "crate::GraphSlot::AfterPost",
            "self.record_overlays(",
            "crate::GraphSlot::Overlay",
            "self.submit_frame(",
            "self.finish_frame(",
        ];
        let body = render_into_body();
        let mut at = 0;
        for step in ORDER {
            let Some(found) = body[at..].find(step) else {
                panic!("render_into: `{step}` is missing or out of its written order");
            };
            at += found + step.len();
        }
        // The slots appear once each, in `FRAME_STEPS` order.
        let slots: Vec<&str> = ORDER
            .iter()
            .filter_map(|step| step.strip_prefix("crate::GraphSlot::"))
            .collect();
        let listed: Vec<&str> = FRAME_STEPS
            .iter()
            .filter_map(|step| match step {
                FrameStep::Slot(GraphSlot::AfterOpaque) => Some("AfterOpaque"),
                FrameStep::Slot(GraphSlot::AfterTransparent) => Some("AfterTransparent"),
                FrameStep::Slot(GraphSlot::AfterPost) => Some("AfterPost"),
                FrameStep::Slot(GraphSlot::Overlay) => Some("Overlay"),
                // `before_post` records inside the post stack (`post_stack.rs`).
                FrameStep::Slot(GraphSlot::BeforePost) | FrameStep::Builtin { .. } => None,
            })
            .collect();
        assert_eq!(slots, listed);
    }

    /// The graph calls the passes (ADR-4945): `render_into` records every built-in pass and every
    /// slot inside one loop over the record sites the frame graph hands it
    /// (`FrameDispatch::sites`), one match arm per site, so the graph's node order decides the
    /// recording order, not the order of the statements. No record call is left before or after
    /// the loop.
    #[test]
    fn render_into_records_its_passes_in_the_order_the_graph_gives() {
        let body = render_into_body();
        let head = body
            .find("self.extension_passes.dispatch.sites()")
            .expect("render_into takes its record sites from the frame graph");
        let close = body
            .find("self.extension_passes.dispatch.finish()")
            .expect("render_into closes the dispatch");
        let looped = &body[head..close];
        assert!(
            looped.contains("for &site in") && looped.contains("match site {"),
            "render_into must record its passes in a loop over the graph's sites"
        );
        for call in [
            "shadow_passes::record_vis_shadow_cull(",
            "shadow_passes::record_shadow_passes(",
            "shadow_passes::record_translucent_shadows(",
            "cluster_probes::record_cluster_and_probes(",
            "self.record_depth_passes(",
            "screen_occlusion::record_screen_occlusion(",
            "self.many_light.record(",
            "visbuffer_resolve::record_visbuffer_resolve(",
            "subsurface_gpu::record_subsurface(",
            "forward_pass::record_forward(",
            "self.oit.finish(",
            "crate::GraphSlot::AfterOpaque",
            "self.record_screen_space(",
            "self.record_outdoor(",
            "self.record_temporal(",
            "self.record_translucent_hdr(",
            "crate::GraphSlot::AfterTransparent",
            "self.record_post_stack(",
            "self.record_translucent_layers(",
            "crate::GraphSlot::AfterPost",
            "self.record_overlays(",
            "crate::GraphSlot::Overlay",
        ] {
            assert_eq!(body.matches(call).count(), 1, "render_into records `{call}` once");
            assert!(
                looped.contains(call),
                "render_into records `{call}` outside the graph's site loop"
            );
        }
        let mut at = 0;
        for arm in [
            "FrameSite::Shadows =>",
            "FrameSite::ClusterProbes =>",
            "FrameSite::DepthPasses =>",
            "FrameSite::ScreenOcclusion =>",
            "FrameSite::ManyLightSampling =>",
            "FrameSite::VisbufferResolve =>",
            "FrameSite::Subsurface =>",
            "FrameSite::Forward =>",
            "FrameSite::Slot(GraphSlot::AfterOpaque) =>",
            "FrameSite::ScreenSpace =>",
            "FrameSite::Outdoor =>",
            "FrameSite::Temporal =>",
            "FrameSite::TranslucentHdr =>",
            "FrameSite::Slot(GraphSlot::AfterTransparent) =>",
            "FrameSite::Slot(GraphSlot::BeforePost) =>",
            "FrameSite::PostStack =>",
            "FrameSite::TranslucentLayers =>",
            "FrameSite::Slot(GraphSlot::AfterPost) =>",
            "FrameSite::Overlays =>",
            "FrameSite::Slot(GraphSlot::Overlay) =>",
        ] {
            assert_eq!(looped.matches(arm).count(), 1, "one `{arm}` arm");
            let found = looped[at..]
                .find(arm)
                .unwrap_or_else(|| panic!("`{arm}` is out of the graph's site order"));
            at += found + arm.len();
        }
    }

    /// The built-in steps compile as a [`crate::RenderGraph`] in their recording order with
    /// every pass live. Each write makes a new version of its resource (`color#3`) and each read
    /// takes the latest version before it, so the graph checks the declarations, not the order:
    /// a pass whose writes nothing downstream reads (a missing `reads` entry) is culled, and the
    /// compile then differs from the recording order.
    #[test]
    fn frame_steps_compile_as_a_render_graph_in_recording_order_with_every_pass_live() {
        use crate::{GraphInput, GraphOutput, GraphResource, RenderGraph, RenderGraphNode, TextureDesc};
        let mut graph = RenderGraph::new();
        let mut versions: std::collections::HashMap<&str, u32> = std::collections::HashMap::new();
        let mut names = Vec::new();
        for step in FRAME_STEPS {
            let FrameStep::Builtin {
                name, reads, writes, ..
            } = step
            else {
                continue;
            };
            let inputs = reads
                .iter()
                .map(|read| GraphInput {
                    name: format!("{read}#{}", versions.get(read).copied().unwrap_or(0)),
                })
                .collect();
            let outputs = writes
                .iter()
                .map(|write| {
                    let version = versions.entry(*write).or_insert(0);
                    *version += 1;
                    GraphOutput {
                        name: format!("{write}#{version}"),
                        // The graph orders by name only; the desc is not allocated.
                        desc: GraphResource::Texture(TextureDesc::new(
                            1,
                            1,
                            wgpu::TextureFormat::Rgba16Float,
                        )),
                    }
                })
                .collect();
            graph.add_node(RenderGraphNode {
                id: (*name).to_string(),
                inputs,
                outputs,
                slot: None,
                pipelines: vec![],
                record: Box::new(|_| Ok(())),
            });
            names.push(*name);
        }
        let output = format!("output#{}", versions["output"]);
        let order = graph
            .try_compile(&[output.as_str()])
            .expect("frame steps compile");
        let compiled: Vec<&str> = order.iter().map(|&i| names[i]).collect();
        assert_eq!(
            compiled, names,
            "culled or reordered: a pass declares a read or write it does not have"
        );
    }

    /// Every pass module of `render_into` (the `mod` lines under "Passes of `render_into`" in
    /// `frame.rs`) records at least one listed built-in step, and every step names one of them:
    /// a pass split out of `render_into` gets its graph row, and the graph names no stale file.
    #[test]
    fn pass_modules_and_frame_steps_name_each_other() {
        let source = include_str!("../frame.rs");
        let start = source
            .find("// Passes of `render_into`, one module each")
            .expect("pass module list in frame.rs");
        let modules: std::collections::BTreeSet<&str> = source[start..]
            .lines()
            .skip(1)
            .map_while(|line| line.strip_prefix("mod ").and_then(|rest| rest.strip_suffix(';')))
            .collect();
        assert!(modules.len() >= 12, "{modules:?}");
        let listed: std::collections::BTreeSet<&str> = FRAME_STEPS
            .iter()
            .filter_map(|step| match step {
                FrameStep::Builtin { module, .. } => Some(*module),
                FrameStep::Slot(_) => None,
            })
            .collect();
        assert_eq!(modules, listed);
    }

    #[test]
    fn built_in_names_are_unique_and_valid_and_timings_exist() {
        let timed: Vec<&str> = GpuTimings::default().passes().iter().map(|(n, _)| *n).collect();
        let mut seen = std::collections::HashSet::new();
        for step in FRAME_STEPS {
            if let FrameStep::Builtin { name, timing, .. } = step {
                assert!(valid_name(name), "{name}");
                assert!(seen.insert(*name), "{name} twice");
                if let Some(label) = timing {
                    assert!(timed.contains(label), "{name}: no GpuTimings entry {label}");
                }
            }
        }
    }
}
