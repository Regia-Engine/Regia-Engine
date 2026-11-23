// SPDX-License-Identifier: MPL-2.0
//! One-shot read-only copies at the public frame extension slots (ADR-7824) and, on request,
//! at every built-in pass boundary (ADR-7860).
//!
//! An armed session installs observers, never changes a render target, and copies each
//! main-view checkpoint once. `finish` or `cancel` removes every observer. No observer,
//! allocation, copy or polling remains when off. Images belong to checkpoints, not to an
//! invented intermediate attachment; unsupported formats and multisampling stay explicit.
//!
//! Pass-boundary copies are opt-in per capture
//! ([`PassCaptureRequest`](crate::frame_capture::PassCaptureRequest),
//! [`CaptureSession::arm_with_passes`](crate::frame_capture::CaptureSession::arm_with_passes) or
//! [`FrameRenderer::request_pass_capture`](crate::FrameRenderer::request_pass_capture)). With a request
//! armed, `FrameRenderer::render_into` copies the frame's shared targets (`color`, `depth`, `output`) a
//! record site declared as writes into capture-owned buffers right after that site recorded, on the
//! frame's own encoder: the pass order and every pass result are unchanged, and the live targets are
//! never handed out. A resource that is not one of the shared targets (shadow atlas, clusters, AO, ...)
//! is listed as skipped with `capture_internal`; copies over the request's byte budget are skipped with
//! `capture_budget`. With no request the frame does nothing for the feature: one `None` check per record
//! site, no copy, no allocation, no barrier
//! ([`pass_hook_counters`](crate::frame_capture::pass_hook_counters) proves it).
//!
//! The same request records the captured frame's draws one by one (ADR-8224,
//! [`DrawRecord`]): the draw loops of
//! [`DRAW_RECORD_PASSES`] push a record per draw while the
//! frame's thread-local draw log is on, and it is on only for the main-view frame the request
//! copies. Every other frame opens the log off: a `bool` per draw loop, no record, no allocation
//! ([`draw_log_counters`] proves it).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::{
    FRAME_STEPS, FrameDrawCounts, FrameGraphReport, FramePass, FramePassContext, FrameRenderer, FrameStep,
    GraphSlot, PassRegisterError, RenderDevice, RenderError, SlotColorSpace, TrackedDevice,
};

const SLOTS: [(&str, GraphSlot); 5] = [
    ("frame_capture_after_opaque", GraphSlot::AfterOpaque),
    ("frame_capture_after_transparent", GraphSlot::AfterTransparent),
    ("frame_capture_before_post", GraphSlot::BeforePost),
    ("frame_capture_after_post", GraphSlot::AfterPost),
    ("frame_capture_overlay", GraphSlot::Overlay),
];
/// Maximum GPU staging storage in a capture. Larger targets report `capture_budget`.
pub const MAX_CAPTURE_BYTES: u64 = 256 * 1024 * 1024;
/// Default staging budget of one capture's pass-boundary copies ([`PassCaptureRequest`]).
pub const DEFAULT_PASS_CAPTURE_BYTES: u64 = 256 * 1024 * 1024;

/// The frame's shared targets a pass-boundary copy can take; every other declared write is a
/// pass-internal resource (`capture_internal`).
const SHARED_TARGETS: [&str; 3] = ["color", "depth", "output"];

static HOOK_BOUNDARIES: AtomicU64 = AtomicU64::new(0);
static HOOK_COPIES: AtomicU64 = AtomicU64::new(0);
static HOOK_BUFFERS: AtomicU64 = AtomicU64::new(0);

/// Process-wide counts of the pass-boundary hook's work (ADR-7860). A record site with no
/// armed request adds nothing to any of them: the way a test proves the off path does no work.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PassHookCounters {
    /// Record-site boundaries the hook ran at (only while a request was armed).
    pub boundaries: u64,
    /// Texture-to-buffer copies the hook recorded.
    pub copies: u64,
    /// Capture-owned staging buffers the hook allocated.
    pub staging_buffers: u64,
}

impl PassHookCounters {
    /// `self - earlier`, per counter.
    #[must_use]
    pub fn since(self, earlier: Self) -> Self {
        Self {
            boundaries: self.boundaries.saturating_sub(earlier.boundaries),
            copies: self.copies.saturating_sub(earlier.copies),
            staging_buffers: self.staging_buffers.saturating_sub(earlier.staging_buffers),
        }
    }
}

/// The pass-boundary hook's counters so far in this process.
#[must_use]
pub fn pass_hook_counters() -> PassHookCounters {
    PassHookCounters {
        boundaries: HOOK_BOUNDARIES.load(Ordering::Relaxed),
        copies: HOOK_COPIES.load(Ordering::Relaxed),
        staging_buffers: HOOK_BUFFERS.load(Ordering::Relaxed),
    }
}

/// A request for read-only copies at the built-in pass boundaries of the next main-view frame
/// (ADR-7860).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PassCaptureRequest {
    /// Built-in pass names ([`FRAME_STEPS`]) whose record site is copied after. Empty copies
    /// after every record site that recorded a pass.
    pub passes: Vec<String>,
    /// Staging bytes this capture may use; a copy that does not fit is skipped
    /// (`capture_budget`) and the capture keeps going with what fits.
    pub budget_bytes: u64,
}

impl Default for PassCaptureRequest {
    fn default() -> Self {
        Self {
            passes: Vec::new(),
            budget_bytes: DEFAULT_PASS_CAPTURE_BYTES,
        }
    }
}

impl PassCaptureRequest {
    /// Every pass, with the default budget.
    #[must_use]
    pub fn all() -> Self {
        Self::default()
    }

    /// Checks every requested name is a built-in pass and returns them as static names.
    ///
    /// # Errors
    /// [`PassCaptureError::UnknownPass`] for the first name that is not one.
    pub fn resolve(&self) -> Result<Vec<&'static str>, PassCaptureError> {
        self.passes
            .iter()
            .map(|name| builtin_name(name).ok_or_else(|| PassCaptureError::UnknownPass(name.clone())))
            .collect()
    }
}

/// Why a pass-boundary capture request was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PassCaptureError {
    /// The name is not a built-in pass of [`FRAME_STEPS`].
    UnknownPass(String),
    /// A request is already armed on this renderer.
    Pending,
}

impl PassCaptureError {
    /// The protocol error code: `capture_unknown_pass` or `capture_pending`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownPass(_) => "capture_unknown_pass",
            Self::Pending => "capture_pending",
        }
    }
}

impl std::fmt::Display for PassCaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownPass(name) => write!(f, "`{name}` is not a built-in frame pass"),
            Self::Pending => f.write_str("a pass capture is already requested on this renderer"),
        }
    }
}

impl std::error::Error for PassCaptureError {}

/// A declared pass output the capture holds no image of, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkippedCopy {
    /// The last pass of its record site that wrote the resource.
    pub pass: &'static str,
    /// The declared resource.
    pub resource: &'static str,
    /// `capture_budget`, `capture_internal`, `capture_format`, `capture_multisampled` or
    /// `capture_not_copyable`.
    pub reason: &'static str,
}

fn builtin_name(name: &str) -> Option<&'static str> {
    FRAME_STEPS.iter().find_map(|step| match step {
        FrameStep::Builtin { name: n, .. } if *n == name => Some(*n),
        _ => None,
    })
}

fn builtin_writes(name: &str) -> &'static [&'static str] {
    FRAME_STEPS
        .iter()
        .find_map(|step| match step {
            FrameStep::Builtin { name: n, writes, .. } if *n == name => Some(*writes),
            _ => None,
        })
        .unwrap_or(&[])
}

/// The frame's shared targets at one record-site boundary (`render_into`'s site loop).
pub(crate) struct Boundary<'a> {
    /// Renderer frame index.
    pub(crate) frame: u64,
    /// The scene color now: the linear HDR image, or the display target on a display-referred
    /// frame (`color_display`).
    pub(crate) color: &'a wgpu::TextureView,
    /// `color` is display-referred.
    pub(crate) color_display: bool,
    /// The single-sample scene depth.
    pub(crate) depth: &'a wgpu::TextureView,
    /// The display target.
    pub(crate) output: &'a wgpu::TextureView,
    /// Preview scale of a linear HDR color (post exposure over pre-exposure).
    pub(crate) scale: f32,
}

/// An armed pass-boundary request on one renderer (ADR-7860): the copies of its frame.
pub(crate) struct PassCopies {
    filter: Vec<&'static str>,
    budget: u64,
    frame: Option<u64>,
    /// The dispatch's recorded-node count at the end of the last site of the copied frame.
    mark: usize,
    bytes: u64,
    copies: u64,
    targets: Vec<PendingTarget>,
    skipped: Vec<SkippedCopy>,
    /// The draw records of the copied frame (ADR-8224), once it ended.
    draws: Option<Vec<crate::DrawRecord>>,
}

impl PassCopies {
    /// Arms `request` (names already resolved).
    pub(crate) fn new(filter: Vec<&'static str>, budget: u64) -> Self {
        Self {
            filter,
            budget,
            frame: None,
            mark: 0,
            bytes: 0,
            copies: 0,
            targets: Vec::new(),
            skipped: Vec::new(),
            draws: None,
        }
    }

    /// Whether main-view frame `frame` is the one this request copies and records draws of: no
    /// frame was copied yet, or it is that frame.
    pub(crate) fn records_draws_of(&self, frame: u64) -> bool {
        self.draws.is_none() && self.frame.is_none_or(|copied| copied == frame)
    }

    /// Keeps the draw records of main-view frame `frame` when it is the copied frame.
    pub(crate) fn keep_draws(&mut self, frame: u64, draws: Vec<crate::DrawRecord>) {
        if self.draws.is_none() && self.frame.is_none_or(|copied| copied == frame) {
            self.frame = Some(frame);
            self.draws = Some(draws);
        }
    }

    /// The frame dispatch's recorded-node count at the end of the previous site: where the
    /// next site's recorded nodes start.
    pub(crate) fn mark(&self) -> usize {
        self.mark
    }

    /// Copies what the record site that just ran wrote, read-only, on the frame's encoder.
    /// `recorded` is the nodes the site recorded (the dispatch's nodes since [`Self::mark`]) and
    /// `end` the dispatch's count after them. The request covers exactly one main-view frame:
    /// the first one whose site reaches here.
    pub(crate) fn record_site(
        &mut self,
        device: &TrackedDevice,
        encoder: &mut wgpu::CommandEncoder,
        recorded: impl Iterator<Item = &'static str>,
        end: usize,
        at: &Boundary<'_>,
    ) {
        HOOK_BOUNDARIES.fetch_add(1, Ordering::Relaxed);
        if self.frame.is_some_and(|frame| frame != at.frame) {
            return;
        }
        self.frame = Some(at.frame);
        let recorded: Vec<&'static str> = recorded.collect();
        self.mark = end;
        let Some(&site_last) = recorded.last() else {
            return;
        };
        if !self.filter.is_empty() && !recorded.iter().any(|name| self.filter.contains(name)) {
            return;
        }
        let _span = tracing::info_span!("frame_capture_pass_boundary", pass = site_last).entered();
        // Each declared write once, labeled with the site's last pass that wrote it.
        let mut resources: Vec<(&'static str, &'static str)> = Vec::new();
        for &name in &recorded {
            for &resource in builtin_writes(name) {
                match resources.iter_mut().find(|(r, _)| *r == resource) {
                    Some(entry) => entry.1 = name,
                    None => resources.push((resource, name)),
                }
            }
        }
        for (resource, last) in resources {
            let (view, encoding) = match resource {
                "color" if at.color_display => (at.color, "display"),
                "color" => (at.color, "linear_clamp"),
                "depth" => (at.depth, "depth_ramp"),
                "output" => (at.output, "display"),
                _ => {
                    self.skipped.push(SkippedCopy {
                        pass: last,
                        resource,
                        reason: "capture_internal",
                    });
                    continue;
                }
            };
            debug_assert!(SHARED_TARGETS.contains(&resource));
            let staged = stage(
                device,
                encoder,
                view.texture(),
                resource,
                self.budget.saturating_sub(self.bytes),
            );
            if let Some(reason) = staged.unavailable {
                self.skipped.push(SkippedCopy {
                    pass: last,
                    resource,
                    reason,
                });
            } else {
                self.bytes += staged.bytes;
                self.copies += 1;
                HOOK_COPIES.fetch_add(1, Ordering::Relaxed);
                HOOK_BUFFERS.fetch_add(1, Ordering::Relaxed);
            }
            self.targets.push(PendingTarget {
                target: CapturedTarget {
                    checkpoint: last,
                    after_pass: Some(last.to_string()),
                    version: None,
                    resource,
                    frame: at.frame,
                    size: staged.size,
                    format: staged.format,
                    encoding,
                    pixels: None,
                    unavailable: staged.unavailable,
                },
                buffer: staged.buffer,
                padded: staged.padded,
                scale: at.scale,
            });
        }
    }
}

/// A target copied at a named graph checkpoint, or the reason it could not be copied.
#[derive(Clone, Debug)]
pub struct CapturedTarget {
    /// The observer node in the captured graph (`frame_capture_*`), or for a pass-boundary
    /// copy (ADR-7860) the built-in pass whose output it is.
    pub checkpoint: &'static str,
    /// Last recorded pass before this checkpoint, excluding the capture observers; for a
    /// pass-boundary copy, the pass itself.
    pub after_pass: Option<String>,
    /// The versioned resource the checkpoint declared as an input, or the version a
    /// pass-boundary copy's pass declared as its write.
    pub version: Option<String>,
    /// `color`, `output` or `depth`.
    pub resource: &'static str,
    /// Renderer frame index at the copy site.
    pub frame: u64,
    /// Target dimensions in pixels.
    pub size: [u32; 2],
    /// GPU texture format.
    pub format: wgpu::TextureFormat,
    /// Preview conversion: `display`, `linear_clamp` or `depth_ramp`.
    pub encoding: &'static str,
    /// Tightly packed RGBA8, top row first. None if unavailable.
    pub pixels: Option<Vec<u8>>,
    /// A lowercase code when unavailable.
    pub unavailable: Option<&'static str>,
}

/// One frame's graph, draw counters and checkpoint images. No persisted file format.
#[derive(Debug)]
pub struct CapturedFrame {
    /// Captured graph with the temporary capture observers removed.
    pub graph: FrameGraphReport,
    /// The renderer's aggregate mesh draw counters, with their existing coverage limits.
    pub counts: FrameDrawCounts,
    /// Targets in checkpoint order, color then depth.
    pub targets: Vec<CapturedTarget>,
    /// Texture-to-buffer copies recorded for this capture.
    pub copies: u64,
    /// Staging bytes allocated for this capture.
    pub staging_bytes: u64,
    /// Declared pass outputs the capture holds no image of, with the reason (ADR-7860):
    /// pass-internal resources, copies over the budget, unsupported targets. Empty when no
    /// pass-boundary copies were requested.
    pub skipped: Vec<SkippedCopy>,
    /// Pass-boundary copies were requested for this capture ([`CaptureSession::arm_with_passes`]).
    pub pass_copies: bool,
    /// The captured frame's draws in recording order (ADR-8224): one record per draw of the draw
    /// loops in [`crate::DRAW_RECORD_PASSES`]. Empty without pass copies.
    pub draws: Vec<crate::DrawRecord>,
}

#[derive(Default)]
struct Pending {
    frame: Option<u64>,
    targets: Vec<PendingTarget>,
    visited: [bool; 5],
    bytes: u64,
    copies: u64,
}

struct PendingTarget {
    target: CapturedTarget,
    buffer: Option<wgpu::Buffer>,
    padded: u32,
    scale: f32,
}

/// Armed read-only capture. Always call `finish` or `cancel` on the same renderer,
/// including after a render failure. Dropping it alone cannot unregister its observers.
#[must_use = "finish or cancel the capture on its renderer"]
pub struct CaptureSession {
    pending: Arc<Mutex<Pending>>,
    /// This session armed the renderer's pass-boundary request (ADR-7860).
    passes: bool,
}

impl CaptureSession {
    /// Arms the next main-view frame. A duplicate observer name refuses the request and
    /// rolls back only the observers installed by this call.
    ///
    /// # Errors
    /// A reserved observer name is already registered.
    pub fn arm(renderer: &mut FrameRenderer) -> Result<Self, PassRegisterError> {
        let pending = Arc::new(Mutex::new(Pending::default()));
        for (index, &(name, slot)) in SLOTS.iter().enumerate() {
            let pass = Observer {
                index,
                name,
                slot,
                pending: Arc::clone(&pending),
            };
            if let Err(error) = renderer.register_pass(Box::new(pass)) {
                for &(installed, _) in &SLOTS[..index] {
                    renderer.remove_pass(installed);
                }
                return Err(error);
            }
        }
        Ok(Self {
            pending,
            passes: false,
        })
    }

    /// [`Self::arm`] plus read-only copies at the built-in pass boundaries `request` names
    /// (ADR-7860). Nothing is armed when either part is refused.
    ///
    /// # Errors
    /// A reserved observer name is registered, a requested pass is unknown, or a pass capture
    /// is already requested on `renderer`.
    pub fn arm_with_passes(
        renderer: &mut FrameRenderer,
        request: &PassCaptureRequest,
    ) -> Result<Self, CaptureArmError> {
        renderer
            .request_pass_capture(request)
            .map_err(CaptureArmError::Passes)?;
        match Self::arm(renderer) {
            Ok(mut session) => {
                session.passes = true;
                Ok(session)
            }
            Err(error) => {
                renderer.cancel_pass_capture();
                Err(CaptureArmError::Observers(error))
            }
        }
    }

    /// Removes all this session's observers and discards pending copies.
    pub fn cancel(self, renderer: &mut FrameRenderer) {
        self.remove(renderer);
    }

    fn remove(&self, renderer: &mut FrameRenderer) {
        for (name, _) in SLOTS {
            renderer.remove_pass(name);
        }
        if self.passes {
            renderer.cancel_pass_capture();
        }
    }

    /// Removes the observers and maps the recorded copies. The frame must have been submitted.
    /// Call the renderer's `read_offscreen` first if current-frame GPU timings are needed;
    /// otherwise `graph.gpu` retains the renderer's latest available timing sample.
    ///
    /// # Errors
    /// No main view was captured, a later frame replaced its metadata, or GPU polling /
    /// buffer readback failed. Observers are removed even on error.
    #[tracing::instrument(skip_all)]
    pub fn finish(
        self,
        renderer: &mut FrameRenderer,
        device: &RenderDevice,
    ) -> Result<CapturedFrame, RenderError> {
        let mut graph = renderer.graph_report();
        let counts = renderer.draw_counts();
        let passes = if self.passes {
            renderer.take_pass_copies()
        } else {
            None
        };
        self.remove(renderer);
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if pending.frame.is_none() || pending.targets.is_empty() {
            return Err(RenderError::Readback("No main-view frame was captured".into()));
        }
        if pending.frame != graph.dispatch.last_frame {
            return Err(RenderError::Readback(
                "Another frame was rendered before the capture was collected".into(),
            ));
        }
        let mut targets = Vec::with_capacity(pending.targets.len());
        for mut copy in pending.targets.drain(..) {
            let target = &mut copy.target;
            target.after_pass = graph
                .graph
                .nodes
                .iter()
                .take_while(|n| n.name != target.checkpoint)
                .map(|node| &node.name)
                .filter(|name| !SLOTS.iter().any(|(capture, _)| *capture == name.as_str()))
                .filter(|name| {
                    graph.dispatch.recorded.contains(&name.as_str())
                        || graph
                            .extensions
                            .iter()
                            .any(|p| p.name == name.as_str() && p.last_frame == Some(target.frame))
                })
                .last()
                .cloned();
            target.version = graph
                .graph
                .nodes
                .iter()
                .find(|n| n.name == target.checkpoint)
                .and_then(|n| {
                    n.reads
                        .iter()
                        .find(|r| r.split('#').next() == Some(target.resource))
                })
                .cloned();
            if let Some(buffer) = copy.buffer.take() {
                copy.target.pixels = Some(read(&buffer, device, &copy)?);
            }
            targets.push(copy.target);
        }
        let (mut copies, mut staging_bytes, mut skipped) = (pending.copies, pending.bytes, Vec::new());
        let mut draws = Vec::new();
        if let Some(mut passes) = passes {
            if passes.frame.is_some() && passes.frame != graph.dispatch.last_frame {
                return Err(RenderError::Readback(
                    "The pass copies belong to another frame than the capture".into(),
                ));
            }
            for mut copy in passes.targets.drain(..) {
                let target = &mut copy.target;
                target.version = graph
                    .graph
                    .nodes
                    .iter()
                    .find(|n| n.name == target.checkpoint)
                    .and_then(|n| {
                        n.writes
                            .iter()
                            .find(|w| w.split('#').next() == Some(target.resource))
                    })
                    .cloned();
                if let Some(buffer) = copy.buffer.take() {
                    copy.target.pixels = Some(read(&buffer, device, &copy)?);
                }
                targets.push(copy.target);
            }
            copies += passes.copies;
            staging_bytes += passes.bytes;
            skipped = std::mem::take(&mut passes.skipped);
            draws = passes.draws.take().unwrap_or_default();
        }
        // Graph order: a pass's copies sit with the checkpoints after the same pass.
        let rank = |target: &CapturedTarget| {
            target
                .after_pass
                .as_ref()
                .and_then(|pass| graph.graph.order.iter().position(|name| name == pass))
                .unwrap_or(usize::MAX)
        };
        targets.sort_by_key(rank);
        graph
            .graph
            .order
            .retain(|name| !SLOTS.iter().any(|(capture, _)| *capture == name.as_str()));
        graph
            .graph
            .nodes
            .retain(|node| !SLOTS.iter().any(|(capture, _)| *capture == node.name));
        graph
            .extensions
            .retain(|pass| !SLOTS.iter().any(|(capture, _)| *capture == pass.name));
        Ok(CapturedFrame {
            graph,
            counts,
            targets,
            copies,
            staging_bytes,
            skipped,
            pass_copies: self.passes,
            draws,
        })
    }
}

/// Why [`CaptureSession::arm_with_passes`] armed nothing.
#[derive(Debug)]
pub enum CaptureArmError {
    /// A reserved checkpoint observer name is already registered.
    Observers(PassRegisterError),
    /// The pass-boundary request was refused.
    Passes(PassCaptureError),
}

impl CaptureArmError {
    /// The protocol error code: `capture_observer_name`, or the [`PassCaptureError::code`].
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Observers(_) => "capture_observer_name",
            Self::Passes(error) => error.code(),
        }
    }
}

impl std::fmt::Display for CaptureArmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Observers(error) => write!(f, "{error}"),
            Self::Passes(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for CaptureArmError {}

struct Observer {
    index: usize,
    name: &'static str,
    slot: GraphSlot,
    pending: Arc<Mutex<Pending>>,
}

impl FramePass for Observer {
    fn name(&self) -> &str {
        self.name
    }
    fn slot(&self) -> GraphSlot {
        self.slot
    }
    fn reads(&self) -> &[&'static str] {
        match self.slot {
            GraphSlot::AfterPost | GraphSlot::Overlay => &["output", "depth"],
            _ => &["color", "depth"],
        }
    }
    fn record(&mut self, ctx: &mut FramePassContext<'_>) -> Result<(), String> {
        if ctx.texture_view.is_some() {
            return Ok(());
        }
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if pending.visited[self.index] || pending.frame.is_some_and(|frame| frame != ctx.frame_index) {
            return Ok(());
        }
        pending.frame = Some(ctx.frame_index);
        pending.visited[self.index] = true;
        let _span = tracing::info_span!("frame_capture_checkpoint").entered();
        let display = ctx.color_space == SlotColorSpace::Display;
        let color = ctx.color.texture().clone();
        let depth = ctx.depth.texture().clone();
        record_target(
            ctx,
            &color,
            self.name,
            if display { "output" } else { "color" },
            &mut pending,
        );
        record_target(ctx, &depth, self.name, "depth", &mut pending);
        Ok(())
    }
}

fn texel_bytes(format: wgpu::TextureFormat) -> Option<u32> {
    use wgpu::TextureFormat as F;
    match format {
        F::Rgba8Unorm
        | F::Rgba8UnormSrgb
        | F::Bgra8Unorm
        | F::Bgra8UnormSrgb
        | F::Depth32Float
        | F::Rg11b10Ufloat => Some(4),
        F::Rgba16Float => Some(8),
        _ => None,
    }
}

/// One texture's staging copy, or why there is none.
struct Staged {
    buffer: Option<wgpu::Buffer>,
    padded: u32,
    bytes: u64,
    size: [u32; 2],
    format: wgpu::TextureFormat,
    unavailable: Option<&'static str>,
}

/// Copies mip 0, layer 0 of `texture` into a new capture-owned `MAP_READ` buffer on `encoder`
/// when it is copyable and fits in `room` bytes. The texture is only read.
fn stage(
    device: &TrackedDevice,
    encoder: &mut wgpu::CommandEncoder,
    texture: &wgpu::Texture,
    resource: &'static str,
    room: u64,
) -> Staged {
    let format = texture.format();
    let width = texture.width();
    let height = texture.height();
    let bpp = texel_bytes(format);
    let padded = (width * bpp.unwrap_or(4)).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
        * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let bytes = u64::from(padded) * u64::from(height);
    let unavailable = if texture.sample_count() != 1 {
        Some("capture_multisampled")
    } else if bpp.is_none() {
        Some("capture_format")
    } else if !texture.usage().contains(wgpu::TextureUsages::COPY_SRC) {
        Some("capture_not_copyable")
    } else if bytes > room {
        Some("capture_budget")
    } else {
        None
    };
    let buffer = unavailable.is_none().then(|| {
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("frame debugger target copy"),
            size: bytes,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: if resource == "depth" {
                    wgpu::TextureAspect::DepthOnly
                } else {
                    wgpu::TextureAspect::All
                },
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        buffer
    });
    Staged {
        buffer,
        padded,
        bytes,
        size: [width, height],
        format,
        unavailable,
    }
}

fn record_target(
    ctx: &mut FramePassContext<'_>,
    texture: &wgpu::Texture,
    checkpoint: &'static str,
    resource: &'static str,
    pending: &mut Pending,
) {
    let encoding = if resource == "depth" {
        "depth_ramp"
    } else if ctx.color_space == SlotColorSpace::Display {
        "display"
    } else {
        "linear_clamp"
    };
    let staged = stage(
        ctx.device,
        ctx.encoder,
        texture,
        resource,
        MAX_CAPTURE_BYTES.saturating_sub(pending.bytes),
    );
    if staged.buffer.is_some() {
        pending.bytes += staged.bytes;
        pending.copies += 1;
    }
    pending.targets.push(PendingTarget {
        target: CapturedTarget {
            checkpoint,
            after_pass: None,
            version: None,
            resource,
            frame: ctx.frame_index,
            size: staged.size,
            format: staged.format,
            encoding,
            pixels: None,
            unavailable: staged.unavailable,
        },
        buffer: staged.buffer,
        padded: staged.padded,
        scale: ctx.exposure.unwrap_or(1.0) / ctx.pre_exposure.max(f32::MIN_POSITIVE),
    });
}

fn read(buffer: &wgpu::Buffer, device: &RenderDevice, copy: &PendingTarget) -> Result<Vec<u8>, RenderError> {
    let (tx, rx) = std::sync::mpsc::channel();
    crate::frame_stats::note_map_async();
    buffer.slice(..).map_async(wgpu::MapMode::Read, move |result| {
        let _ = tx.send(result);
    });
    device
        .tracked()
        .poll(wgpu::PollType::wait_indefinitely())
        .map_err(RenderError::Poll)?;
    rx.recv()
        .map_err(|_| RenderError::Readback("frame capture callback dropped".into()))?
        .map_err(|e| RenderError::Readback(e.to_string()))?;
    let mapped = buffer
        .slice(..)
        .get_mapped_range()
        .map_err(|e| RenderError::Readback(e.to_string()))?;
    let bpp = texel_bytes(copy.target.format).unwrap_or(4) as usize;
    let [width, height] = copy.target.size;
    let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
    for y in 0..height as usize {
        let row = &mapped[y * copy.padded as usize..][..width as usize * bpp];
        for x in 0..width as usize {
            rgba.extend_from_slice(&preview(&row[x * bpp..][..bpp], copy));
        }
    }
    drop(mapped);
    buffer.unmap();
    Ok(rgba)
}

fn code(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn preview(bytes: &[u8], copy: &PendingTarget) -> [u8; 4] {
    use wgpu::TextureFormat as F;
    let mut rgba = [0, 0, 0, 255];
    match copy.target.format {
        F::Rgba8Unorm | F::Rgba8UnormSrgb => rgba.copy_from_slice(bytes),
        F::Bgra8Unorm | F::Bgra8UnormSrgb => rgba = [bytes[2], bytes[1], bytes[0], bytes[3]],
        F::Depth32Float => {
            let value = f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            rgba[..3].fill(code(value));
        }
        F::Rgba16Float | F::Rg11b10Ufloat => {
            let rgb = if copy.target.format == F::Rgba16Float {
                std::array::from_fn(|i| {
                    crate::deep_readout::half_to_f32(u16::from_le_bytes([bytes[i * 2], bytes[i * 2 + 1]]))
                })
            } else {
                let bits = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
                // R11/G11/B10 share binary16's five-bit exponent, without a sign.
                [
                    crate::deep_readout::half_to_f32(((bits & 0x7ff) << 4) as u16),
                    crate::deep_readout::half_to_f32((((bits >> 11) & 0x7ff) << 4) as u16),
                    crate::deep_readout::half_to_f32(((bits >> 22) << 5) as u16),
                ]
            };
            for (out, value) in rgba[..3].iter_mut().zip(rgb) {
                *out = code(if copy.target.encoding == "linear_clamp" {
                    (value * copy.scale).max(0.0).powf(1.0 / 2.2)
                } else {
                    value
                });
            }
        }
        _ => {}
    }
    rgba
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_capture_duplicate_registration_rolls_back_only_its_own_observers() {
        let mut renderer = FrameRenderer::new();
        let pending = Arc::new(Mutex::new(Pending::default()));
        renderer
            .register_pass(Box::new(Observer {
                index: 2,
                name: SLOTS[2].0,
                slot: SLOTS[2].1,
                pending,
            }))
            .expect("existing pass");
        assert!(CaptureSession::arm(&mut renderer).is_err());
        let passes = renderer.registered_passes();
        assert_eq!(passes.len(), 1);
        assert_eq!(passes[0], SLOTS[2].0);
        renderer.remove_pass(SLOTS[2].0);
        let session = CaptureSession::arm(&mut renderer).expect("arm");
        assert!(CaptureSession::arm(&mut renderer).is_err());
        assert_eq!(renderer.registered_passes().len(), SLOTS.len());
        session.cancel(&mut renderer);
        assert!(renderer.registered_passes().is_empty());
    }

    #[test]
    fn pass_capture_request_resolves_builtin_names_and_codes_its_errors() {
        let request = PassCaptureRequest {
            passes: vec!["forward".into(), "post_stack".into()],
            ..PassCaptureRequest::all()
        };
        assert_eq!(request.resolve(), Ok(vec!["forward", "post_stack"]));
        assert_eq!(request.budget_bytes, DEFAULT_PASS_CAPTURE_BYTES);
        // Checkpoint observers and slots are not built-in passes.
        for name in ["gbuffer", "frame_capture_overlay", "after_post", ""] {
            let request = PassCaptureRequest {
                passes: vec![name.into()],
                ..PassCaptureRequest::all()
            };
            let error = request.resolve().expect_err(name);
            assert_eq!(error, PassCaptureError::UnknownPass(name.into()));
            assert_eq!(error.code(), "capture_unknown_pass");
        }
        assert_eq!(PassCaptureError::Pending.code(), "capture_pending");
        assert_eq!(builtin_writes("forward"), &["color"]);
        assert!(builtin_writes("not_a_pass").is_empty());
    }

    #[test]
    fn pass_capture_request_arms_once_and_cancels_without_a_frame() {
        let mut renderer = FrameRenderer::new();
        assert!(!renderer.pass_capture_requested());
        assert!(!renderer.cancel_pass_capture());
        renderer
            .request_pass_capture(&PassCaptureRequest::all())
            .expect("request");
        assert_eq!(
            renderer.request_pass_capture(&PassCaptureRequest::all()),
            Err(PassCaptureError::Pending)
        );
        assert!(renderer.cancel_pass_capture());
        assert!(!renderer.pass_capture_requested());
        // A refused name arms nothing.
        let unknown = PassCaptureRequest {
            passes: vec!["gbuffer".into()],
            ..PassCaptureRequest::all()
        };
        assert!(renderer.request_pass_capture(&unknown).is_err());
        assert!(!renderer.pass_capture_requested());
    }

    fn pixel(format: wgpu::TextureFormat, bytes: &[u8]) -> [u8; 4] {
        preview(
            bytes,
            &PendingTarget {
                target: CapturedTarget {
                    checkpoint: "test",
                    after_pass: None,
                    version: None,
                    resource: "test",
                    frame: 1,
                    size: [1, 1],
                    format,
                    encoding: "display",
                    pixels: None,
                    unavailable: None,
                },
                buffer: None,
                padded: 256,
                scale: 1.0,
            },
        )
    }

    #[test]
    fn frame_capture_preview_decodes_channel_order_and_stored_depth() {
        use wgpu::TextureFormat as F;
        assert_eq!(pixel(F::Bgra8Unorm, &[1, 2, 3, 4]), [3, 2, 1, 4]);
        assert_eq!(pixel(F::Rgba8UnormSrgb, &[1, 2, 3, 4]), [1, 2, 3, 4]);
        assert_eq!(pixel(F::Depth32Float, &0.0_f32.to_le_bytes()), [0, 0, 0, 255]);
        assert_eq!(
            pixel(F::Depth32Float, &0.5_f32.to_le_bytes()),
            [128, 128, 128, 255]
        );
        assert_eq!(pixel(F::Depth32Float, &1.0_f32.to_le_bytes()), [255; 4]);
        // IEEE binary16: R=1, G=1/2, B=0; packed R11G11B10 uses the same exponent bias.
        assert_eq!(
            pixel(F::Rgba16Float, &[0, 0x3c, 0, 0x38, 0, 0, 0, 0x3c]),
            [255, 128, 0, 255]
        );
        let packed = (15_u32 << 6) | ((14_u32 << 6) << 11);
        assert_eq!(pixel(F::Rg11b10Ufloat, &packed.to_le_bytes()), [255, 128, 0, 255]);
    }
}
