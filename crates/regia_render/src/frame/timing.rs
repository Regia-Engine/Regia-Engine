// SPDX-License-Identifier: MPL-2.0
//! GPU timestamp queries and the per-pass CPU and GPU timing reports (moved out of
//! `frame.rs` unchanged, ADR-1552).

use super::frame_graph::BuiltinNode;
use super::*;

/// One mappable copy of the resolved timestamps.
pub(super) struct TimerSlot {
    pub(super) staging: wgpu::Buffer,
    pub(super) state: std::sync::Arc<std::sync::atomic::AtomicU8>,
    pub(super) submitted_frame: u64,
    /// Which node recorded each stamp of the frame copied into this slot (ADR-5386).
    pub(super) nodes: GpuStampNodes,
}

/// Per query: the tag of the node whose record site was open when the frame wrote it (ADR-5386;
/// `0` never written, `STAMP_OUTSIDE`, or `STAMP_NODE_TAG` + node). Cleared when a frame starts
/// recording, read into the timer slot at submit. Allocated once with the timer.
pub(super) struct StampLog {
    tags: [std::sync::atomic::AtomicU8; TIMESTAMP_QUERIES as usize],
}

impl StampLog {
    fn new() -> Self {
        Self {
            tags: std::array::from_fn(|_| std::sync::atomic::AtomicU8::new(0)),
        }
    }

    /// Forgets the previous frame's stamps.
    pub(super) fn clear(&self) {
        for tag in &self.tags {
            tag.store(0, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn note(&self, first: u32, count: u32, tag: u8) {
        for query in first..first.saturating_add(count) {
            if let Some(slot) = self.tags.get(query as usize) {
                slot.store(tag, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }

    /// The frame's tags, with the nodes it recorded (`FrameDispatch::frame_recorded_mask`).
    pub(super) fn snapshot(&self, recorded: u32) -> GpuStampNodes {
        GpuStampNodes {
            tags: std::array::from_fn(|q| self.tags[q].load(std::sync::atomic::Ordering::Relaxed)),
            recorded,
        }
    }
}

/// The frame's handle on the GPU timer's query set (ADR-5386): every timestamp the frame writes
/// goes through it, so the stamp log knows which graph node's record site wrote it. It does not
/// deref to the query set: a raw `write_timestamp` on the frame's set does not compile.
#[derive(Clone)]
pub(in crate::frame) struct FrameStamps {
    set: wgpu::QuerySet,
    log: std::sync::Arc<StampLog>,
    node: std::sync::Arc<std::sync::atomic::AtomicU8>,
}

impl FrameStamps {
    /// The handle of `timer` for one frame. Clears the stamp log.
    pub(in crate::frame) fn begin(timer: &GpuTimer) -> Self {
        timer.log.clear();
        Self {
            set: timer.set.clone(),
            log: std::sync::Arc::clone(&timer.log),
            node: std::sync::Arc::clone(&timer.node),
        }
    }

    fn note(&self, first: u32, count: u32) {
        self.log
            .note(first, count, self.node.load(std::sync::atomic::Ordering::Relaxed));
    }

    /// Writes stamp `query` on the encoder.
    pub(in crate::frame) fn write(&self, encoder: &mut wgpu::CommandEncoder, query: u32) {
        encoder.write_timestamp(&self.set, query);
        self.note(query, 1);
    }

    /// Writes stamp `query` inside a render pass (`TIMESTAMP_QUERY_INSIDE_PASSES`).
    pub(in crate::frame) fn write_in_pass(&self, pass: &mut wgpu::RenderPass<'_>, query: u32) {
        pass.write_timestamp(&self.set, query);
        self.note(query, 1);
    }

    /// The raw query set for a module that writes stamps `first..first + count` itself (the probe
    /// update's stages, stochastic SSR's split): they are logged as written by the open node now.
    pub(in crate::frame) fn lend(&self, first: u32, count: u32) -> &wgpu::QuerySet {
        self.note(first, count);
        &self.set
    }
}

/// Persistent per-frame GPU-timing state (`specs/13` § 11).
///
/// The query set and resolve buffer are reused every frame (submits on one
/// queue stay ordered). The staging ring is what the CPU maps, two frames late.
pub(super) struct GpuTimer {
    pub(super) set: wgpu::QuerySet,
    pub(super) resolve: wgpu::Buffer,
    pub(super) slots: [TimerSlot; crate::readback::READBACK_SLOTS],
    /// Which node wrote each stamp of the frame being recorded (ADR-5386).
    pub(super) log: std::sync::Arc<StampLog>,
    /// The frame dispatch's open-node tag (`FrameDispatch::stamp_node`) the stamps are tagged with.
    pub(super) node: std::sync::Arc<std::sync::atomic::AtomicU8>,
}

impl GpuTimer {
    /// Writes the frame's last stamp ([`END_QUERY`]) after the frame closed its dispatch.
    pub(super) fn write_end(&self, encoder: &mut wgpu::CommandEncoder) {
        encoder.write_timestamp(&self.set, END_QUERY);
        self.log.note(END_QUERY, 1, super::frame_graph::STAMP_OUTSIDE);
    }
}

/// What [`FrameRenderer::last_extent`] reports: the last view's output and internal size and
/// how the internal image reached the output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameExtent {
    /// Output (target) size, px.
    pub output: (u32, u32),
    /// Internal render size, px.
    pub internal: (u32, u32),
    /// `"native"`, `"TAAU"`, `"TSR"`, `"FSR 3"`, `"SGSR"` or `"bilinear"`.
    pub upscaler: &'static str,
}

/// Named GPU pass timings of the last frame (milliseconds), when the device
/// supports `TIMESTAMP_QUERY` and timing was enabled.
#[derive(Clone, Copy, Debug, Default)]
pub struct GpuTimings {
    /// Renderer frame index described by this sample; `None` before a sample exists.
    pub frame_id: Option<u64>,
    /// Process-wide publication identity; 0 before a sample exists. Readers consume it once.
    pub sample_id: u64,
    /// Shadow passes (cascades + atlas).
    pub shadows_ms: f32,
    /// Froxel cluster build.
    pub clusters_ms: f32,
    /// GPU irradiance probe update: trace, relocate, blend (0 when the probes are off or
    /// settled), plus the half-resolution screen lookup (DDGI's, or the baked volume's on a
    /// baked map: then this is the whole GI cost).
    pub gi_ms: f32,
    /// The probe update's stages: trace, relocate, irradiance blend, visibility blend, then
    /// the screen lookup (0 when the probes are off or settled).
    pub gi_stages_ms: [f32; 5],
    /// Depth prepass.
    pub prepass_ms: f32,
    /// Forward color pass, including the sky background before transparent meshes.
    pub forward_ms: f32,
    /// Hi-Z build plus GPU frustum/occlusion cull (0 when the CPU path draws).
    pub cull_ms: f32,
    /// Screen-space ambient occlusion (0 when off).
    pub ssao_ms: f32,
    /// Screen-space contact shadows (0 when off).
    pub contact_shadows_ms: f32,
    /// Screen-space GI (0 when the pass is skipped).
    pub ssgi_ms: f32,
    /// Screen-space reflections and probe specular (0 when the pass is skipped).
    pub ssr_ms: f32,
    /// Stochastic SSR's trace inside [`Self::ssr_ms`]: the depth pyramid, the tile
    /// classification and the ray march (0 when the pass is skipped).
    pub sssr_trace_ms: f32,
    /// The part of [`Self::sssr_trace_ms`] before the ray march: the depth pyramid and the tile
    /// classification.
    pub sssr_classify_ms: f32,
    /// Stochastic SSR's denoiser inside [`Self::ssr_ms`]: reproject, prefilter and temporal
    /// resolve (0 when the pass is skipped). The rest of `ssr_ms` is the composite.
    pub sssr_denoise_ms: f32,
    /// Temporal upscale (0 when the frame is native resolution).
    pub taau_ms: f32,
    /// Aerial perspective and atmosphere weather veil (0 when the LUT sky is off).
    /// The sky background is included in `forward_ms` (ADR-7963).
    pub atmosphere_ms: f32,
    /// Volumetric cloud march and composite (0 when clouds are off).
    pub clouds_ms: f32,
    /// Froxel volumetric fog (0 when off).
    pub fog_ms: f32,
    /// Whole scene section (shadows through the outdoor composite, before upscale).
    pub scene_ms: f32,
    /// After the upscale to the end of the frame encoder: translucent layers (VFX), the
    /// post stack (bloom, exposure, tonemap, grade) and the display-pass effects.
    pub post_ms: f32,
    /// The whole frame encoder, first stamp to last (`scene + taau + post`).
    pub frame_ms: f32,
    /// The shared draw (terrain) inside the depth prepass (0 without in-pass timestamps).
    pub shared_prepass_ms: f32,
    /// The shared draw (terrain) inside the forward pass: the terrain's own colour cost,
    /// apart from the meshes drawn in the same pass (0 without in-pass timestamps).
    pub shared_forward_ms: f32,
    /// The shadow section per kind: sun cascades 0..3, then the local-light atlas.
    pub shadow_kind_ms: [f32; 5],
    /// The forward pass's translucent section: every glass pane (the own canopy included) and
    /// the other blended draws, after the opaque, terrain and sky draws and the refraction copy
    /// (0 without in-pass timestamps or blended draws; v0.1.17).
    pub forward_glass_ms: f32,
    /// The refraction copy of the opaque frame that glass reads (before
    /// [`Self::forward_glass_ms`], inside [`Self::forward_ms`]): one per frame, and none on a
    /// frame without a visible refracting draw (0 then; ADR-7986).
    pub refraction_copy_ms: f32,
    /// The glass front-layer pass (the nearest transmissive layer's depth and transmittance,
    /// after the refraction copy and before [`Self::forward_glass_ms`], inside
    /// [`Self::forward_ms`]; 0 on a frame without it; ADR-7986).
    pub glass_front_layer_ms: f32,
    /// Which frame-graph node's record site wrote each stamp of this sample, and the nodes the
    /// frame recorded (ADR-5386): read through [`Self::node_passes`] and [`Self::stamps`].
    pub stamp_nodes: GpuStampNodes,
}

/// Which frame-graph node wrote each GPU timestamp of one [`GpuTimings`] sample, and which nodes
/// that frame recorded (ADR-5386). Empty (nothing written) without timing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GpuStampNodes {
    tags: [u8; TIMESTAMP_QUERIES as usize],
    recorded: u32,
}

impl Default for GpuStampNodes {
    fn default() -> Self {
        Self {
            tags: [0; TIMESTAMP_QUERIES as usize],
            recorded: 0,
        }
    }
}

impl GpuStampNodes {
    /// No stamp was written (timing off, or no sample yet).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tags.iter().all(|&tag| tag == 0)
    }

    fn written(&self, query: u32) -> bool {
        self.tags.get(query as usize).is_some_and(|&tag| tag != 0)
    }

    fn recorded(&self, node: BuiltinNode) -> bool {
        self.recorded & (1 << node as u32) != 0
    }

    /// The node whose record site wrote `query`; `None` when it was written outside every node
    /// or never.
    fn written_in(&self, query: u32) -> Option<BuiltinNode> {
        let tag = *self.tags.get(query as usize)?;
        tag.checked_sub(super::frame_graph::STAMP_NODE_TAG)
            .and_then(|node| BuiltinNode::from_index(usize::from(node)))
    }
}

/// One GPU-timed pass of the frame, named after the frame-graph node that records it
/// ([`GpuTimings::node_passes`], ADR-5386).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GpuNodePass {
    /// The built-in node ([`FrameStep::Builtin`] name) the interval is named after: the first of
    /// its nodes the frame recorded.
    pub node: &'static str,
    /// The timed interval: its [`GpuTimings::passes`] label, or `gi lookup` (the probe or baked
    /// irradiance screen lookup, which records in the `ssao` site).
    pub pass: &'static str,
    /// GPU time, ms.
    pub ms: f32,
}

/// One timestamp the frame wrote ([`GpuTimings::stamps`], ADR-5386).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GpuStamp {
    /// Query index in the frame's timestamp set.
    pub query: u32,
    /// The node whose interval the stamp bounds, or `frame` for the frame's first and last stamp.
    pub owner: &'static str,
    /// The node whose record site was open when the stamp was written (`None`: outside every node).
    pub written_in: Option<&'static str>,
    /// The stamp was written where it belongs: inside its owner's record site, or, for the
    /// frame's first and last stamp, outside every node.
    pub in_owner: bool,
}

/// The node whose interval each stamp bounds, by ranges of queries (ADR-5386). The frame's first
/// stamp (0) and [`END_QUERY`] are not listed: they bound the frame.
const STAMP_OWNERS: [(u32, u32, BuiltinNode); 22] = [
    (1, 1, BuiltinNode::Shadows),
    (SHADOW_KIND_QUERY_BASE, 5, BuiltinNode::Shadows),
    (2, 1, BuiltinNode::ClusterLights),
    (DDGI_QUERY_BASE, 2, BuiltinNode::GiProbes),
    (DDGI_STAGE_QUERY_BASE, 8, BuiltinNode::GiProbes),
    (3, 1, BuiltinNode::DepthPrepass),
    (SHARED_QUERY_BASE, 2, BuiltinNode::DepthPrepass),
    (4, 1, BuiltinNode::GpuCull),
    (CONTACT_QUERY, 1, BuiltinNode::Ssao),
    (DDGI_QUERY_BASE + 2, 2, BuiltinNode::Ssao),
    (5, 1, BuiltinNode::ContactShadows),
    (6, 1, BuiltinNode::Forward),
    (SHARED_QUERY_BASE + 2, 2, BuiltinNode::Forward),
    (FORWARD_GLASS_QUERY_BASE, 2, BuiltinNode::Forward),
    (GLASS_SPLIT_QUERY_BASE, 4, BuiltinNode::Forward),
    (7, 1, BuiltinNode::Ssgi),
    (8, 1, BuiltinNode::Ssr),
    (SSSR_QUERY_BASE, 3, BuiltinNode::Ssr),
    (OUTDOOR_QUERY_BASE, 2, BuiltinNode::Atmosphere),
    (OUTDOOR_QUERY_BASE + 2, 2, BuiltinNode::Clouds),
    (OUTDOOR_QUERY_BASE + 4, 3, BuiltinNode::Fog),
    (9, 1, BuiltinNode::Taau),
];

fn stamp_owner(query: u32) -> Option<BuiltinNode> {
    STAMP_OWNERS
        .iter()
        .find(|(first, count, _)| (*first..first + count).contains(&query))
        .map(|row| row.2)
}

/// The GPU-timed passes in frame order (ADR-5386): label, the nodes it may be named after (the
/// first one the frame recorded names it), and the stamp that closes it (a pass whose closing
/// stamp was not written is not reported).
const NODE_PASSES: [(&str, &[BuiltinNode], u32); 16] = [
    ("shadows", &[BuiltinNode::Shadows], 1),
    ("clusters", &[BuiltinNode::ClusterLights], 2),
    ("gi probes", &[BuiltinNode::GiProbes], DDGI_QUERY_BASE + 1),
    ("prepass", &[BuiltinNode::DepthPrepass], 3),
    ("cull", &[BuiltinNode::GpuCull], 4),
    ("ssao", &[BuiltinNode::Ssao], CONTACT_QUERY),
    ("gi lookup", &[BuiltinNode::Ssao], DDGI_QUERY_BASE + 3),
    (
        "contact shadows",
        &[BuiltinNode::ContactShadows, BuiltinNode::SunShadowTrace],
        5,
    ),
    ("forward", &[BuiltinNode::Forward], 6),
    ("ssgi", &[BuiltinNode::Ssgi], 7),
    ("ssr", &[BuiltinNode::Ssr], 8),
    ("atmosphere", &[BuiltinNode::Atmosphere], OUTDOOR_QUERY_BASE + 1),
    ("clouds", &[BuiltinNode::Clouds], OUTDOOR_QUERY_BASE + 3),
    ("fog", &[BuiltinNode::Fog], OUTDOOR_QUERY_BASE + 5),
    ("taau", &[BuiltinNode::Taau, BuiltinNode::Velocity], 9),
    (
        "post",
        &[
            BuiltinNode::PostStack,
            BuiltinNode::TranslucentHdr,
            BuiltinNode::TranslucentLayers,
            BuiltinNode::ScalarOverlays,
            BuiltinNode::Gizmos,
        ],
        END_QUERY,
    ),
];

impl GpuTimings {
    /// The major passes by name, in frame order (the totals `scene_ms`/`frame_ms` and the
    /// sub-stage breakdowns are left out): the performance overlay's per-pass table and its
    /// agent method read this list.
    #[must_use]
    pub fn passes(self) -> [(&'static str, f32); 15] {
        [
            ("shadows", self.shadows_ms),
            ("clusters", self.clusters_ms),
            ("gi probes", self.gi_ms),
            ("prepass", self.prepass_ms),
            ("cull", self.cull_ms),
            ("ssao", self.ssao_ms),
            ("contact shadows", self.contact_shadows_ms),
            ("forward", self.forward_ms),
            ("ssgi", self.ssgi_ms),
            ("ssr", self.ssr_ms),
            ("atmosphere", self.atmosphere_ms),
            ("clouds", self.clouds_ms),
            ("fog", self.fog_ms),
            ("taau", self.taau_ms),
            ("post", self.post_ms),
        ]
    }

    /// The GPU-timed passes named after the frame-graph nodes that record them, in frame (graph)
    /// order (ADR-5386). A pass is listed when the frame wrote its closing stamp and recorded one
    /// of its nodes; empty without timing. `gi probes` here is the probe update alone: the
    /// screen lookup that [`Self::gi_ms`] adds records in the `ssao` site and is listed there as
    /// `gi lookup`.
    #[must_use]
    pub fn node_passes(&self) -> Vec<GpuNodePass> {
        let passes = self.passes();
        let lookup_ms = self.gi_stages_ms[4];
        NODE_PASSES
            .iter()
            .filter(|(_, _, end)| self.stamp_nodes.written(*end))
            .filter_map(|&(pass, nodes, _)| {
                let node = nodes.iter().find(|node| self.stamp_nodes.recorded(**node))?;
                let ms = match pass {
                    "gi probes" => (self.gi_ms - lookup_ms).max(0.0),
                    "gi lookup" => lookup_ms,
                    label => passes
                        .iter()
                        .find(|(name, _)| *name == label)
                        .map_or(0.0, |p| p.1),
                };
                Some(GpuNodePass {
                    node: node.name(),
                    pass,
                    ms,
                })
            })
            .collect()
    }

    /// Every timestamp the sample's frame wrote, by query, with the node whose interval it bounds
    /// and the node whose record site wrote it (ADR-5386). A stamp with `in_owner: false` was
    /// written outside its node: its pass time mixes in another node's work.
    #[must_use]
    pub fn stamps(&self) -> Vec<GpuStamp> {
        (0..TIMESTAMP_QUERIES)
            .filter(|&query| self.stamp_nodes.written(query))
            .map(|query| {
                let owner = stamp_owner(query);
                let written_in = self.stamp_nodes.written_in(query);
                let in_owner = match owner {
                    Some(owner) => written_in.is_some_and(|node| node.site() == owner.site()),
                    None => written_in.is_none(),
                };
                GpuStamp {
                    query,
                    owner: owner.map_or("frame", BuiltinNode::name),
                    written_in: written_in.map(BuiltinNode::name),
                    in_owner,
                }
            })
            .collect()
    }

    /// `name 0.00 ms` for every timed pass, then the scene total.
    #[must_use]
    pub fn report(self) -> String {
        format!(
            "shadows {:.2} ms, clusters {:.2} ms, gi probes {:.2} ms, prepass {:.2} ms, cull {:.2} ms, ssao {:.2} ms, \
             contact shadows {:.2} ms, forward {:.2} ms, ssgi {:.2} ms, ssr {:.2} ms, atmosphere {:.2} ms, clouds {:.2} ms, \
             fog {:.2} ms, taau {:.2} ms, scene total {:.2} ms, post {:.2} ms, frame {:.2} ms",
            self.shadows_ms,
            self.clusters_ms,
            self.gi_ms,
            self.prepass_ms,
            self.cull_ms,
            self.ssao_ms,
            self.contact_shadows_ms,
            self.forward_ms,
            self.ssgi_ms,
            self.ssr_ms,
            self.atmosphere_ms,
            self.clouds_ms,
            self.fog_ms,
            self.taau_ms,
            self.scene_ms,
            self.post_ms,
            self.frame_ms
        )
    }
}

/// Timestamp queries per frame: ten pass boundaries (ADR-027), three outdoor
/// begin/end pairs (atmosphere, clouds, fog), one stamp after the outdoor
/// composite that starts the upscale interval, and one at the end of the encoder.
pub(super) const TIMESTAMP_QUERIES: u32 = 49;

/// Stamps around the refraction copy and the glass front-layer pass, between the forward pass's
/// opaque part and its translucent section: `[before copy, after copy, before front layer, after
/// front layer]` (ADR-7986). Written on the encoder, only on frames that refract.
pub(super) const GLASS_SPLIT_QUERY_BASE: u32 = 45;

/// In-pass stamps around the forward pass's translucent section (every glass pane, the own
/// canopy included, and the other blended draws; `forward_glass`): `[before, after]`. Written
/// only when the device has `TIMESTAMP_QUERY_INSIDE_PASSES`, else both after the pass (0 ms).
pub(super) const FORWARD_GLASS_QUERY_BASE: u32 = 43;

/// Probe update stages: trace, relocate, irradiance blend, visibility blend (2 each).
pub(super) const DDGI_STAGE_QUERY_BASE: u32 = 32;

/// Stamps around the GPU irradiance probe update (inside the cluster interval; after the contact-shadow stamp, 27).
pub(super) const DDGI_QUERY_BASE: u32 = 28;

/// Stochastic SSR's split of the reflection slot: after the classification, after the trace,
/// after the denoiser (40-42, after the probe stamps 28-39). Written next to slot 7 when the
/// pass does not run.
pub(super) const SSSR_QUERY_BASE: u32 = 40;

/// The stamp recorded last in the frame encoder.
pub(super) const END_QUERY: u32 = 17;

/// In-pass stamps around the shared draw (terrain) in the depth prepass and the
/// forward pass: `[before depth, after depth, before colour, after colour]`. Written only
/// when the device has `TIMESTAMP_QUERY_INSIDE_PASSES`.
pub(super) const SHARED_QUERY_BASE: u32 = 18;

/// One stamp at the start of each shadow kind (cascades 0..3, then the local atlas); the
/// shadow section's end stamp (slot 1) closes the last.
pub(super) const SHADOW_KIND_QUERY_BASE: u32 = 22;

/// First outdoor query slot.
pub(super) const OUTDOOR_QUERY_BASE: u32 = 10;

/// Stamp between the SSAO and the contact-shadow dispatches (the interval up to slot 5 is
/// the contact-shadow pass).
pub(super) const CONTACT_QUERY: u32 = 27;

/// Begin slot for an outdoor pass name; the end slot is the next one.
pub(super) fn outdoor_query_slot(name: &str) -> u32 {
    OUTDOOR_QUERY_BASE
        + match name {
            "atmosphere" => 0,
            "clouds" => 2,
            _ => 4,
        }
}

/// CPU phase times for the last [`FrameRenderer::render_into`].
///
/// `total_ms` is the wall time of that call. It does not include a caller's
/// `wait_idle` (benchmarks add that so the wall number includes the GPU) and
/// it does not include present (headless targets do not present; the window
/// path times present separately).
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameCpuTimings {
    /// Renderer frame index described by this sample; `None` before a sample exists.
    pub frame_id: Option<u64>,
    /// Process-wide publication identity; 0 before a sample exists. Readers consume it once.
    pub sample_id: u64,
    /// Recording command buffers. `total_ms - submit_ms - readback_ms`.
    pub encode_ms: f32,
    /// Time to finish the last command-buffer segment, wait for the encode workers'
    /// segments, and `queue.submit` them.
    pub submit_ms: f32,
    /// Non-blocking map pump (`poll(Poll)` and copying a ready slot).
    pub readback_ms: f32,
    /// Present is not part of `render_into`. Always 0.
    pub present_ms: f32,
    /// Wall time of `render_into`.
    pub total_ms: f32,
    /// `queue.submit` calls inside `render_into`. A steady frame is 1.
    pub submits: u32,
}

/// CPU time (ms) of the last [`FrameRenderer::render_into`]'s sun- and local-shadow work on
/// the frame thread, timed with [`std::time::Instant`] (no tracing subscriber needed), for
/// frame summaries and benches. Recording only: the command buffers are finished on the
/// encode workers ([`FrameCpuTimings::submit_ms`]).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ShadowCpuTimings {
    /// The whole shadow plan phase: caster bounds, cascade masks and footprints, the page and
    /// atlas caches, the shared draw's prepare and the per-page caster lists.
    pub plan_ms: f32,
    /// Of [`Self::plan_ms`]: the shared draw's (terrain's) prepare, which also stages its
    /// camera passes (selection, per-cascade node lists, hole cut, uploads).
    pub shared_prepare_ms: f32,
    /// Of [`Self::plan_ms`]: the per-page caster lists.
    pub caster_lists_ms: f32,
    /// The instanced caster and depth-prepass lists, grouped and staged.
    pub instance_lists_ms: f32,
    /// The visibility-buffer casters' per-job cluster cull.
    pub vis_cull_ms: f32,
    /// Recording each shadow pass: sun cascades 0..3, then the local-light atlas (with the
    /// near sun tile).
    pub encode_ms: [f32; 5],
    /// The translucent (tinted) shadow maps.
    pub translucent_ms: f32,
}

impl ShadowCpuTimings {
    /// Everything above except the instance lists (shared with the depth prepass).
    #[must_use]
    pub fn total_ms(&self) -> f32 {
        self.plan_ms + self.vis_cull_ms + self.encode_ms.iter().sum::<f32>() + self.translucent_ms
    }
}

/// Adds the time since `1` to `0` (ms) when dropped: times a scope with early exits.
pub(super) struct AddElapsedMs<'a>(pub(super) &'a mut f32, pub(super) std::time::Instant);

impl Drop for AddElapsedMs<'_> {
    fn drop(&mut self) {
        *self.0 += self.1.elapsed().as_secs_f32() * 1000.0;
    }
}

/// What the cached shadow maps redrew on the last frame.
///
/// Paging is off on the lab baseline, so every cascade is a full redraw and
/// `cascades_skipped` stays 0. Desktop High records per-page invalidation.
#[derive(Clone, Debug, Default)]
pub struct ShadowCacheReport {
    /// Pages whose casters moved, or every page of a cascade whose matrix changed.
    pub dirty_pages: Vec<crate::paged_shadow::PageId>,
    /// `true` when that cascade was cleared and redrawn as a whole.
    pub cascade_full_redraw: Vec<bool>,
    /// Local-light tiles whose caster volume changed.
    pub dirty_atlas_tiles: Vec<u32>,
    /// Atlas tiles whose depth was redrawn. Once the cache is warm this matches
    /// [`Self::dirty_atlas_tiles`]; the first frame draws every allocated tile.
    pub atlas_tiles_drawn: u32,
    /// Cascades left untouched because nothing in them moved.
    pub cascades_skipped: u32,
    /// The local atlas was reused from the previous frame.
    pub atlas_skipped: bool,
    /// Per-page caster lists rebuilt on the CPU this frame (0 for a held camera whose lists
    /// are cached; every drawn page on the first frame or after a caster moved).
    pub caster_lists_rebuilt: u32,
    /// Per sun cascade (kept, not redrawn whole): mesh casters that moved, appeared and
    /// disappeared this frame — what dirtied its pages.
    pub caster_events: Vec<[u32; 3]>,
    /// Shadow jobs recorded per kind (cascades 0..3, then the local atlas): a whole
    /// cascade or tile is one job, a redrawn page one each.
    pub jobs_per_kind: [u32; 5],
    /// What the shared draw (terrain) did per cascade: 0 nothing, 1 the whole cascade,
    /// 2 only the region under redrawn mesh pages.
    pub shared_per_cascade: [u8; 4],
    /// Alpha-blended and transmissive draws casting a translucent (tinted) shadow this frame.
    pub translucent_casters: u32,
    /// Translucency map passes recorded (one per translucent cascade, one for the atlas).
    pub translucent_passes: u32,
    /// Translucent caster draws over those passes (depth and transmittance sweeps).
    pub translucent_draws: u32,
    /// Shadow render passes recorded this frame (one per cascade or atlas that drew
    /// anything). 0 on a held frame whose cascades and atlas are all retained.
    pub shadow_passes: u32,
    /// The near sun tile (ADR-164) is live this frame: the profile has one and a mesh lies
    /// within its range, so mesh receivers there sample it.
    pub near_sun_tile: bool,
    /// The near sun tile was redrawn this frame (`false` when its last draw still holds).
    pub near_sun_tile_drawn: bool,
    /// What the dynamic-caster shadow class did this frame (ADR-2028).
    pub dynamic: crate::foliage_wind::DynamicCasterReport,
    /// Casters admitted to the page caches whose entities carry
    /// `regia_scene::components::Static` and whose draws are static-eligible
    /// ([`crate::static_batch_eligible`]: rigid and drawn whole; `specs/14` A1). A diagnostic:
    /// they cache exactly like other casters (ADR-5906).
    pub static_casters: u32,
    /// Caster draws over this frame's local-light atlas jobs (the sum of their caster lists'
    /// lengths). A light's face draws only the casters that reach the light's sphere and the
    /// face's frustum (ADR-7986); tiles the plan names no light for draw every caster.
    pub atlas_caster_draws: u32,
    /// Tiles of dirty lights kept this frame because no moved caster reaches them (a mover
    /// under a point light redraws the faces it is in, not all six; ADR-7986).
    pub atlas_faces_kept: u32,
    /// Caster draws per sun cascade this frame (the sum of its jobs' caster lists): a caster
    /// over several redrawn pages is drawn once per page (ADR-7986).
    pub sun_caster_draws: [u32; 4],
    /// Mesh casters drawn into each sun cascade's caster lists this frame (those whose cascade
    /// mask includes it; 0 past the active cascades or without a sun shadow). A caster in a
    /// sun-sealed cell (ADR-8414) counts in none.
    pub sun_casters_per_cascade: [u32; 4],
}

/// Which screen-space passes the last frame recorded into its encoder.
///
/// Set where each pass is encoded, so a test can tell a skipped pass from a cheap
/// one without reading GPU timestamps (single stamps move by more than a skip on a
/// shared or loaded device).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScreenPassReport {
    /// The SSAO / GTAO pass was dispatched (a held view reuses last frame's AO).
    pub ssao: bool,
    /// The SSGI bounce was gathered (a held Desktop High view reuses its gather).
    pub ssgi_gather: bool,
    /// The SSGI bounce was composited onto this frame's colour (half-resolution
    /// upsample, or the full-resolution pass that gathers and composites in one).
    pub ssgi_composite: bool,
    /// The full-resolution SSR march was recorded.
    pub ssr: bool,
    /// Stochastic SSR was recorded.
    pub sssr: bool,
    /// The previous SSGI/SSR composite was served from the static-screen cache.
    pub screen_cache_hit: bool,
}

impl FrameRenderer {
    /// Enables or disables per-pass GPU timing (requires `TIMESTAMP_QUERY`).
    pub fn set_gpu_timing(&mut self, device: &RenderDevice, enabled: bool) {
        if enabled && device.capabilities.timestamp_query && self.gpu_timer.is_none() {
            let set = device.device.create_query_set(&wgpu::QuerySetDescriptor {
                label: Some("frame timestamps"),
                ty: wgpu::QueryType::Timestamp,
                count: TIMESTAMP_QUERIES,
            });
            let resolve = device.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("timestamp resolve"),
                size: u64::from(TIMESTAMP_QUERIES) * 8,
                usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            });
            let slot = |index: usize| TimerSlot {
                staging: device.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some(match index {
                        0 => "timestamp staging 0",
                        1 => "timestamp staging 1",
                        _ => "timestamp staging 2",
                    }),
                    size: u64::from(TIMESTAMP_QUERIES) * 8,
                    usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                    mapped_at_creation: false,
                }),
                state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(crate::readback::ST_IDLE)),
                submitted_frame: 0,
                nodes: GpuStampNodes::default(),
            };
            self.gpu_timer = Some(GpuTimer {
                set,
                resolve,
                slots: [slot(0), slot(1), slot(2)],
                log: std::sync::Arc::new(StampLog::new()),
                node: self.extension_passes.dispatch.stamp_node(),
            });
        }
        if !enabled {
            self.release_timer(device);
        }
    }

    /// Takes over another renderer's GPU timer (a shader rebuild, ADR-1552): its stamps are tagged
    /// by this renderer's frame dispatch from now on (ADR-5386).
    pub(super) fn adopt_timer(&mut self, timer: Option<GpuTimer>) {
        self.gpu_timer = timer;
        if let Some(timer) = self.gpu_timer.as_mut() {
            timer.node = self.extension_passes.dispatch.stamp_node();
        }
    }

    /// Unmaps any ready timestamp slot and drops the query state.
    pub(super) fn release_timer(&mut self, device: &RenderDevice) {
        let Some(timer) = self.gpu_timer.take() else {
            return;
        };
        // Disable is not the frame path. A slot still in flight has to finish
        // before its buffer can be unmapped; count that wait.
        if timer.slots.iter().any(|slot| {
            let state = slot.state.load(std::sync::atomic::Ordering::Acquire);
            state == crate::readback::ST_INFLIGHT || state == crate::readback::ST_READY
        }) {
            let _ = device.tracked().poll(wgpu::PollType::wait_indefinitely());
        }
        for slot in &timer.slots {
            if slot.state.load(std::sync::atomic::Ordering::Acquire) == crate::readback::ST_READY {
                slot.staging.unmap();
            }
        }
    }

    /// Index of an idle timestamp slot, stamped with the current frame.
    pub(super) fn acquire_timer_slot(&mut self) -> Option<usize> {
        let frame = self.frame_index;
        let timer = self.gpu_timer.as_mut()?;
        let index = timer.slots.iter().position(|slot| {
            slot.state.load(std::sync::atomic::Ordering::Acquire) == crate::readback::ST_IDLE
        })?;
        timer.slots[index].submitted_frame = frame;
        Some(index)
    }

    /// Maps the timestamp slot copied by the scene submit.
    ///
    /// Does nothing unless GPU timing is on. The frame path reads the slot in
    /// [`Self::collect_timings`] with [`crate::readback::READBACK_LATENCY`]
    /// (two frames). A pixel read that already waited passes latency 0.
    pub(super) fn map_timer_slot(&mut self) {
        let Some(index) = self.pending_timer_slot.take() else {
            return;
        };
        if self.gpu_timer.is_none() {
            return;
        }
        let Some(slot) = self.gpu_timer.as_ref().and_then(|timer| timer.slots.get(index)) else {
            return;
        };
        let state = std::sync::Arc::clone(&slot.state);
        state.store(crate::readback::ST_INFLIGHT, std::sync::atomic::Ordering::Release);
        crate::frame_stats::note_map_async();
        slot.staging
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                crate::readback::store_map_result(&state, result.is_ok());
            });
    }

    /// Copies timestamp slots that are mapped and at least `latency` frames old.
    ///
    /// `render_into` passes [`crate::readback::READBACK_LATENCY`]. A pixel read
    /// that already called `poll(Wait)` passes `0`: the map callback has run,
    /// and holding the sample for two more frames would report zeros to a
    /// one-frame capture.
    pub(super) fn collect_timings(&mut self, device: &RenderDevice, latency: u64) {
        let _span = tracing::info_span!("collect_gpu_timings").entered();
        let frame = self.frame_index;
        let newest = {
            let Some(timer) = self.gpu_timer.as_mut() else {
                return;
            };
            let mut ready = Vec::new();
            for (index, slot) in timer.slots.iter().enumerate() {
                let state = slot.state.load(std::sync::atomic::Ordering::Acquire);
                if state == crate::readback::ST_FAILED {
                    slot.state
                        .store(crate::readback::ST_IDLE, std::sync::atomic::Ordering::Release);
                    continue;
                }
                if crate::readback::slot_is_consumable(state, slot.submitted_frame, frame, latency) {
                    ready.push((slot.submitted_frame, index));
                }
            }
            ready.sort_by_key(|pair| pair.0);
            let mut newest = None;
            for (_, index) in ready {
                let slot = &timer.slots[index];
                let Ok(mapped) = slot.staging.slice(..).get_mapped_range() else {
                    slot.state
                        .store(crate::readback::ST_IDLE, std::sync::atomic::Ordering::Release);
                    continue;
                };
                let stamps: Vec<u64> = mapped
                    .as_chunks::<8>()
                    .0
                    .iter()
                    .map(|c| u64::from_le_bytes(*c))
                    .collect();
                drop(mapped);
                slot.staging.unmap();
                slot.state
                    .store(crate::readback::ST_IDLE, std::sync::atomic::Ordering::Release);
                if stamps.len() < TIMESTAMP_QUERIES as usize {
                    continue;
                }
                // Spec 54 § 2.3 (ADR-2307): the end stamp resolved, so every pass of that frame
                // finished on the GPU; the breadcrumbs reuse this query set.
                if stamps[END_QUERY as usize] != 0 {
                    crate::breadcrumbs::global().note_resolved_frame(slot.submitted_frame);
                }
                let period = device.queue.get_timestamp_period();
                let ms =
                    |a: usize, b: usize| stamps[b].saturating_sub(stamps[a]) as f32 * period / 1_000_000.0;
                let written_ms = |a: u32, b: u32| {
                    if slot.nodes.written(a) && slot.nodes.written(b) {
                        ms(a as usize, b as usize)
                    } else {
                        0.0
                    }
                };
                newest = Some(GpuTimings {
                    frame_id: Some(slot.submitted_frame),
                    sample_id: next_timing_sample_id(),
                    shadows_ms: ms(0, 1),
                    clusters_ms: (ms(1, 2) - ms(DDGI_QUERY_BASE as usize, DDGI_QUERY_BASE as usize + 1))
                        .max(0.0),
                    gi_stages_ms: {
                        let b = DDGI_STAGE_QUERY_BASE as usize;
                        let stage = |k: usize| ms(b + 2 * k, b + 2 * k + 1);
                        [
                            stage(0),
                            stage(1),
                            stage(2),
                            stage(3),
                            ms(DDGI_QUERY_BASE as usize + 2, DDGI_QUERY_BASE as usize + 3),
                        ]
                    },
                    gi_ms: ms(DDGI_QUERY_BASE as usize, DDGI_QUERY_BASE as usize + 1)
                        + ms(DDGI_QUERY_BASE as usize + 2, DDGI_QUERY_BASE as usize + 3),
                    prepass_ms: ms(2, 3),
                    cull_ms: ms(3, 4),
                    ssao_ms: (ms(4, CONTACT_QUERY as usize)
                        - ms(DDGI_QUERY_BASE as usize + 2, DDGI_QUERY_BASE as usize + 3))
                    .max(0.0),
                    contact_shadows_ms: ms(CONTACT_QUERY as usize, 5),
                    forward_ms: ms(5, 6),
                    ssgi_ms: ms(6, 7),
                    ssr_ms: ms(7, 8),
                    sssr_trace_ms: ms(7, SSSR_QUERY_BASE as usize + 1),
                    sssr_classify_ms: ms(7, SSSR_QUERY_BASE as usize),
                    sssr_denoise_ms: ms(SSSR_QUERY_BASE as usize + 1, SSSR_QUERY_BASE as usize + 2),
                    atmosphere_ms: ms(10, 11),
                    clouds_ms: ms(12, 13),
                    fog_ms: ms(14, 15),
                    taau_ms: ms(16, 9),
                    scene_ms: ms(0, 16),
                    post_ms: ms(9, END_QUERY as usize),
                    frame_ms: ms(0, END_QUERY as usize),
                    shared_prepass_ms: ms(SHARED_QUERY_BASE as usize, SHARED_QUERY_BASE as usize + 1),
                    shared_forward_ms: ms(SHARED_QUERY_BASE as usize + 2, SHARED_QUERY_BASE as usize + 3),
                    shadow_kind_ms: std::array::from_fn(|k| {
                        let start = SHADOW_KIND_QUERY_BASE as usize + k;
                        let end = if k < 4 { start + 1 } else { 1 };
                        ms(start, end)
                    }),
                    forward_glass_ms: ms(
                        FORWARD_GLASS_QUERY_BASE as usize,
                        FORWARD_GLASS_QUERY_BASE as usize + 1,
                    ),
                    // Written only on frames that refract: an unwritten pair reads 0.
                    refraction_copy_ms: written_ms(GLASS_SPLIT_QUERY_BASE, GLASS_SPLIT_QUERY_BASE + 1),
                    glass_front_layer_ms: written_ms(GLASS_SPLIT_QUERY_BASE + 2, GLASS_SPLIT_QUERY_BASE + 3),
                    stamp_nodes: slot.nodes,
                });
            }
            newest
        };
        if let Some(timings) = newest {
            self.last_timings = timings;
        }
    }
}

/// Unique timing publications also distinguish renderer replacements with equal frame indices.
pub(super) fn next_timing_sample_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::frame_graph::{STAMP_NODE_TAG, STAMP_OUTSIDE};

    fn tag(node: BuiltinNode) -> u8 {
        STAMP_NODE_TAG + node as u8
    }

    /// A sample whose every stamp was written inside its owner's site, with every node recorded.
    fn clean() -> GpuTimings {
        let mut nodes = GpuStampNodes {
            recorded: u32::MAX >> 2,
            ..GpuStampNodes::default()
        };
        for query in 0..TIMESTAMP_QUERIES {
            nodes.tags[query as usize] = stamp_owner(query).map_or(STAMP_OUTSIDE, tag);
        }
        GpuTimings {
            stamp_nodes: nodes,
            ..GpuTimings::default()
        }
    }

    #[test]
    fn every_stamp_but_the_frame_bounds_has_one_owner() {
        for query in 0..TIMESTAMP_QUERIES {
            let owners = STAMP_OWNERS
                .iter()
                .filter(|(first, count, _)| (*first..first + count).contains(&query))
                .count();
            let bound = query == 0 || query == END_QUERY;
            assert_eq!(owners, usize::from(!bound), "stamp {query}");
        }
    }

    #[test]
    fn every_timed_pass_label_has_a_node_pass_and_frame_step_labels_match() {
        let labels: Vec<&str> = NODE_PASSES.iter().map(|row| row.0).collect();
        for (label, _) in GpuTimings::default().passes() {
            assert!(labels.contains(&label), "{label}");
        }
        // A node a pass may be named after is timed under that label in FRAME_STEPS (or is a
        // node the label's interval also covers).
        for (label, nodes, _) in NODE_PASSES {
            let first = nodes[0].name();
            let timed = FRAME_STEPS.iter().any(|step| {
                matches!(step, FrameStep::Builtin { name, timing, .. }
                    if *name == first && (*timing == Some(label) || label == "gi lookup"))
            });
            assert!(timed, "{label}: {first}");
        }
    }

    #[test]
    fn a_clean_sample_names_its_passes_after_nodes_in_graph_order() {
        let gpu = clean();
        let passes = gpu.node_passes();
        assert_eq!(passes.len(), NODE_PASSES.len());
        let ranks: Vec<usize> = passes
            .iter()
            .map(|p| BuiltinNode::from_name_index(p.node).expect("a node"))
            .collect();
        assert!(ranks.windows(2).all(|w| w[0] <= w[1]), "{passes:?}");
        assert!(gpu.stamps().iter().all(|s| s.in_owner), "{:?}", gpu.stamps());
        assert_eq!(gpu.stamps().len(), TIMESTAMP_QUERIES as usize);
    }

    #[test]
    fn a_dropped_closing_stamp_drops_its_pass() {
        let mut gpu = clean();
        gpu.stamp_nodes.tags[7] = 0;
        assert!(!gpu.node_passes().iter().any(|p| p.pass == "ssgi"));
        assert!(gpu.node_passes().iter().any(|p| p.pass == "ssr"));
    }

    #[test]
    fn a_stamp_written_in_another_site_is_outside_its_node() {
        let mut gpu = clean();
        // The SSGI stamp written by the forward site.
        gpu.stamp_nodes.tags[7] = tag(BuiltinNode::Forward);
        let outside: Vec<GpuStamp> = gpu.stamps().into_iter().filter(|s| !s.in_owner).collect();
        assert_eq!(outside.len(), 1, "{outside:?}");
        assert_eq!(
            (outside[0].query, outside[0].owner, outside[0].written_in),
            (7, "ssgi", Some("forward"))
        );
        // Another node of the same site is still inside: the site records them together.
        let mut gpu = clean();
        gpu.stamp_nodes.tags[7] = tag(BuiltinNode::Ssr);
        assert!(gpu.stamps().iter().all(|s| s.in_owner));
        // A pass stamp written outside every node, and a frame bound written inside one.
        let mut gpu = clean();
        gpu.stamp_nodes.tags[1] = STAMP_OUTSIDE;
        gpu.stamp_nodes.tags[0] = tag(BuiltinNode::Shadows);
        let outside: Vec<u32> = gpu
            .stamps()
            .iter()
            .filter(|s| !s.in_owner)
            .map(|s| s.query)
            .collect();
        assert_eq!(outside, [0, 1]);
    }

    #[test]
    fn a_pass_takes_the_first_recorded_of_its_nodes() {
        let mut gpu = clean();
        gpu.stamp_nodes.recorded &= !(1 << BuiltinNode::ContactShadows as u32);
        gpu.stamp_nodes.recorded &= !(1 << BuiltinNode::PostStack as u32);
        let passes = gpu.node_passes();
        let node = |label: &str| passes.iter().find(|p| p.pass == label).map(|p| p.node);
        assert_eq!(node("contact shadows"), Some("sun_shadow_trace"));
        assert_eq!(node("post"), Some("translucent_hdr"));
        // A pass none of whose nodes recorded is not listed.
        gpu.stamp_nodes.recorded &= !(1 << BuiltinNode::Ssgi as u32);
        assert!(!gpu.node_passes().iter().any(|p| p.pass == "ssgi"));
    }

    #[test]
    fn gi_probes_is_the_update_and_the_lookup_is_its_own_pass() {
        let mut gpu = clean();
        gpu.gi_ms = 3.0;
        gpu.gi_stages_ms[4] = 1.0;
        let passes = gpu.node_passes();
        let ms = |label: &str| passes.iter().find(|p| p.pass == label).map(|p| (p.node, p.ms));
        assert_eq!(ms("gi probes"), Some(("gi_probes", 2.0)));
        assert_eq!(ms("gi lookup"), Some(("ssao", 1.0)));
    }

    #[test]
    fn the_stamp_log_tags_with_the_open_node_and_clears() {
        let log = StampLog::new();
        log.note(3, 2, tag(BuiltinNode::DepthPrepass));
        log.note(END_QUERY, 1, STAMP_OUTSIDE);
        let nodes = log.snapshot(0b11);
        assert_eq!(nodes.written_in(3), Some(BuiltinNode::DepthPrepass));
        assert_eq!(nodes.written_in(4), Some(BuiltinNode::DepthPrepass));
        assert!(nodes.written(END_QUERY) && nodes.written_in(END_QUERY).is_none());
        assert!(!nodes.written(5));
        // Out of range is ignored, not a panic.
        log.note(TIMESTAMP_QUERIES, 4, STAMP_OUTSIDE);
        log.clear();
        assert!(log.snapshot(0).is_empty());
    }
}
