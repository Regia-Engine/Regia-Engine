// SPDX-License-Identifier: MPL-2.0
//! Scene readiness (ADR-7883): the host's "prepare this map" call and the one "scene resident
//! and first frame presented" edge.
//!
//! A host spawns its map behind its loading page, then calls [`SceneReadiness::prepare`]. From
//! that call until the scene is ready the window keeps presenting the host's UI (the clear colour
//! plus the UI overlay) and does not draw the scene, while [`crate::GameFrameWarm`] warms the
//! runtime renderer on the extracted frame on a warm worker: every mesh and texture the frame
//! references is uploaded and the pipelines of every draw key are created there. When a warm
//! round returns and the frame holds a mesh or material that round did not see (a spawn that
//! landed after it started), another round warms the frame again (up to
//! [`SCENE_PREPARE_MAX_ROUNDS`]); the size-threshold new-scene warm does not lock a prepare out.
//!
//! The state walks `idle -> preparing -> resident -> presented` ([`SceneReadyState`]). The
//! [`SceneReady`] message is sent once per prepare, at the `presented` edge: after the swap chain
//! present of the first frame that drew the scene returned (on the frame generation path, after
//! the present thread presented that frame's rendered image). Never on CPU time. A warm the
//! watchdog cancels or abandons still ends in `presented`, late, with `timed_out` set; so does a
//! prepare that sees no renderable frame within [`crate::GameFrameWarm::request_grace`]: a host
//! never waits forever. Hidden windows present nothing: they reach `presented` on the first tick
//! the scene is resident and renderable, with `presented_to_window: false`. Headless apps run the
//! same warm rounds and reach `presented` once a frame of the resident scene was submitted (a
//! texture view drawn on the frame thread, or the warm's own frames when the frame has none),
//! also with `presented_to_window: false` (ADR-8122).
//!
//! The host gates its audio, player control and loading page on `SceneReady` (or polls
//! [`SceneReadiness::is_ready`]; agents call `render.scene_ready`).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use regia_ecs::prelude::*;

/// Most warm rounds one prepare runs while its frame keeps growing; after that the scene counts
/// as resident (what still arrives draws on the frame thread) and the prepare's
/// `complete` is false.
pub const SCENE_PREPARE_MAX_ROUNDS: u32 = 8;

/// Where a scene prepare is (`render.scene_ready`'s `state`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SceneReadyState {
    /// No prepare was made.
    #[default]
    Idle,
    /// The host called [`SceneReadiness::prepare`]: the scene is being warmed (the window shows
    /// the host's UI, not the scene).
    Preparing,
    /// Everything the frame references is resident and its pipelines exist; the first scene
    /// frame is being drawn and presented.
    Resident,
    /// The first frame that drew the scene was presented ([`SceneReady`] was sent).
    Presented,
}

impl SceneReadyState {
    /// The protocol name: `idle`, `preparing`, `resident` or `presented`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Preparing => "preparing",
            Self::Resident => "resident",
            Self::Presented => "presented",
        }
    }
}

/// The numbers of the newest prepare ([`SceneReadiness::status`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SceneReadyStatus {
    /// Where the prepare is.
    pub state: SceneReadyState,
    /// The host's map id (`None` before the first prepare).
    pub map_id: Option<String>,
    /// Which prepare this is (1 for the first one).
    pub prepare: u64,
    /// Megabytes of mesh data the prepare's warm rounds uploaded.
    pub uploaded_mb: f32,
    /// Distinct meshes among them.
    pub meshes: usize,
    /// Pipelines the warm rounds created.
    pub pipelines: u64,
    /// Warm rounds run (more than one when the frame grew while a round ran).
    pub warms: u32,
    /// Worker time of the warm rounds, in milliseconds.
    pub ms: f32,
    /// A warm was cancelled or abandoned by the watchdog, or no renderable frame came within
    /// the grace: the scene is drawn anyway, and what was not warmed builds on the frame thread.
    pub timed_out: bool,
    /// Why the prepare ended early, if it did.
    pub error: Option<String>,
    /// Whether the first scene frame found every mesh resident and created no pipeline (`None`
    /// until that frame is drawn, or when nothing was drawn: headless).
    pub complete: Option<bool>,
    /// The window path's render tick of the first presented scene frame.
    pub first_present_frame: Option<u64>,
    /// Milliseconds from the prepare call to that present.
    pub first_present_ms: Option<f32>,
    /// `false` when nothing was presented (headless, or a hidden window): `presented` then
    /// means resident and renderable.
    pub presented_to_window: bool,
}

/// Sent once per [`SceneReadiness::prepare`] when the first frame that drew the scene was
/// presented (or the prepare gave up: [`Self::timed_out`]). Gate audio, player control and the
/// loading page on it.
#[derive(Message, Clone, Debug, PartialEq)]
pub struct SceneReady {
    /// The map id given to [`SceneReadiness::prepare`].
    pub map_id: String,
    /// Which prepare (see [`SceneReadyStatus::prepare`]).
    pub prepare: u64,
    /// The prepare ended late ([`SceneReadyStatus::timed_out`]).
    pub timed_out: bool,
    /// Megabytes of mesh data the warm rounds uploaded.
    pub uploaded_mb: f32,
    /// Distinct meshes among them.
    pub meshes: usize,
    /// Pipelines the warm rounds created.
    pub pipelines: u64,
    /// Warm rounds run.
    pub warms: u32,
    /// Milliseconds from the prepare call to the first present.
    pub first_present_ms: f32,
    /// The render tick of that present.
    pub first_present_frame: u64,
    /// A swap chain present happened (`false`: headless or hidden window).
    pub presented_to_window: bool,
}

/// The host's scene prepare and its state (ADR-7883; see the module docs).
///
/// ```no_run
/// use regia_ecs::prelude::*;
/// use regia_render::{SceneReadiness, SceneReady};
///
/// // Behind the loading page, once the map is spawned:
/// fn on_map_spawned(mut ready: ResMut<SceneReadiness>) {
///     ready.prepare("switchbacks");
/// }
/// // Later: drop the loading page, unmute, hand control to the player.
/// fn on_ready(mut events: MessageReader<SceneReady>) {
///     for _ready in events.read() { /* ... */ }
/// }
/// # let _ = (on_map_spawned, on_ready);
/// ```
#[derive(Resource, Default, Debug)]
pub struct SceneReadiness {
    status: SceneReadyStatus,
    /// A prepare not yet taken by the render path: `(prepare, map id)`.
    request: Option<(u64, String)>,
    /// The last prepare whose [`SceneReady`] was sent.
    sent: u64,
}

impl SceneReadiness {
    /// Prepares the scene the host has spawned under `map_id`: the window stops drawing the
    /// scene (it keeps presenting the UI) until the whole frame is resident and warmed, then
    /// draws and presents it and sends [`SceneReady`]. Call it after the map is spawned. A new
    /// prepare before the last one was presented replaces it (only the newest sends
    /// [`SceneReady`]). Returns the prepare's number.
    pub fn prepare(&mut self, map_id: impl Into<String>) -> u64 {
        let map_id = map_id.into();
        let seq = self.status.prepare + 1;
        self.status = SceneReadyStatus {
            state: SceneReadyState::Preparing,
            map_id: Some(map_id.clone()),
            prepare: seq,
            ..SceneReadyStatus::default()
        };
        self.request = Some((seq, map_id));
        seq
    }

    /// The newest prepare's state and numbers.
    #[must_use]
    pub fn status(&self) -> &SceneReadyStatus {
        &self.status
    }

    /// The newest prepare's state.
    #[must_use]
    pub fn state(&self) -> SceneReadyState {
        self.status.state
    }

    /// The newest prepare reached [`SceneReadyState::Presented`].
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.status.state == SceneReadyState::Presented
    }

    /// The render path takes the newest prepare.
    pub(crate) fn take_request(&mut self) -> Option<(u64, String)> {
        self.request.take()
    }
}

/// Frame generation presents on its own thread (ADR-2067): rendered images queued and retired
/// there, and queue flushes (a suspend drops what it holds).
#[cfg(any(feature = "frame-gen", test))]
static PRESENTS_QUEUED: AtomicU64 = AtomicU64::new(0);
static PRESENTS_DONE: AtomicU64 = AtomicU64::new(0);
static PRESENT_FLUSHES: AtomicU64 = AtomicU64::new(0);

/// Frame generation queued an image on its present thread; returns its ticket.
#[cfg(any(feature = "frame-gen", test))]
pub(crate) fn note_present_queued() -> u64 {
    PRESENTS_QUEUED.fetch_add(1, Ordering::AcqRel) + 1
}

/// The present thread's `queue.present` returned for one image.
#[cfg(any(feature = "frame-gen", test))]
pub(crate) fn note_present_done() {
    PRESENTS_DONE.fetch_add(1, Ordering::AcqRel);
}

/// The present thread dropped what it held without presenting it (suspend, termination).
#[cfg(any(feature = "frame-gen", test))]
pub(crate) fn note_present_flush() {
    PRESENT_FLUSHES.fetch_add(1, Ordering::AcqRel);
}

/// `(images presented, flushes)` on the present thread so far.
pub(crate) fn present_progress() -> (u64, u64) {
    (
        PRESENTS_DONE.load(Ordering::Acquire),
        PRESENT_FLUSHES.load(Ordering::Acquire),
    )
}

/// How the frame that drew the scene reached the screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PresentNote {
    /// `queue.present` returned on this thread.
    Presented,
    /// Queued on the frame generation present thread with this ticket and flush count.
    #[cfg(any(feature = "frame-gen", test))]
    Queued { ticket: u64, flushes: u64 },
    /// The scene drew, but its image never reached the present thread (frame generation: the
    /// real image's acquire failed after the generated image was queued). It does not present
    /// the prepare: the next drawn scene frame is still its first (ADR-8122).
    #[cfg(any(feature = "frame-gen", test))]
    Dropped,
}

/// What the first scene frame after a warm still did on the frame thread.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SceneFrameCheck {
    /// Distinct meshes of the frame that were not resident before its draw.
    pub pending_meshes: usize,
    /// Pipelines its draw created.
    pub pipelines: u64,
    /// How it was presented.
    pub present: PresentNote,
}

impl SceneFrameCheck {
    /// Nothing was uploaded or compiled for the scene on the frame thread.
    pub(crate) fn complete(&self) -> bool {
        self.pending_meshes == 0 && self.pipelines == 0
    }

    /// The frame's image never reached a present ([`PresentNote::Dropped`]).
    pub(crate) fn dropped(&self) -> bool {
        #[cfg(any(feature = "frame-gen", test))]
        if self.present == PresentNote::Dropped {
            return true;
        }
        false
    }
}

/// The meshes and materials a warm round saw.
#[derive(Default)]
pub(crate) struct SceneCover {
    meshes: crate::key_hash::KeyMap<u64, ()>,
    materials: crate::key_hash::KeyMap<u64, ()>,
}

impl SceneCover {
    fn mesh_key(mesh: &crate::ExtractedMesh) -> u64 {
        crate::runtime::mesh_key(&mesh.mesh) ^ mesh.lod_key
    }

    /// What `frame` holds.
    pub(crate) fn of(frame: &crate::ExtractedFrame) -> Self {
        let mut cover = Self::default();
        for mesh in &frame.meshes {
            cover.meshes.insert(Self::mesh_key(mesh), ());
            cover
                .materials
                .insert(crate::runtime::material_key(&mesh.material), ());
        }
        cover
    }

    /// Every mesh and material of `frame` was seen.
    pub(crate) fn covers(&self, frame: &crate::ExtractedFrame) -> bool {
        frame.meshes.iter().all(|m| {
            self.meshes.contains_key(&Self::mesh_key(m))
                && self
                    .materials
                    .contains_key(&crate::runtime::material_key(&m.material))
        })
    }
}

/// Where an open prepare is inside [`crate::GameFrameWarm`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PreparePhase {
    /// No round runs: the next tick decides (warm a round, or resident).
    Waiting,
    /// A round is on the warm worker.
    Warming,
    /// Warmed (or given up): the next drawn scene frame is its first.
    Resident,
    /// That frame is queued on the frame generation present thread.
    #[cfg(any(feature = "frame-gen", test))]
    Queued { ticket: u64, flushes: u64 },
    /// Presented: [`SceneReady`] goes out.
    Presented,
}

/// An open host prepare ([`SceneReadiness::prepare`]) inside [`crate::GameFrameWarm`].
pub(crate) struct PrepareRun {
    pub(crate) seq: u64,
    pub(crate) map_id: String,
    opened: Instant,
    /// Since when the prepare waits without a round (the request grace runs from here).
    pub(crate) waiting_since: Instant,
    pub(crate) phase: PreparePhase,
    pub(crate) rounds: u32,
    pub(crate) cover: SceneCover,
    ms: f32,
    pipelines: u64,
    bytes: u64,
    meshes: usize,
    pub(crate) timed_out: bool,
    pub(crate) error: Option<String>,
    complete: Option<bool>,
    first_present_frame: Option<u64>,
    first_present_ms: Option<f32>,
    presented_to_window: bool,
}

impl PrepareRun {
    pub(crate) fn new(seq: u64, map_id: String, now: Instant) -> Self {
        Self {
            seq,
            map_id,
            opened: now,
            waiting_since: now,
            phase: PreparePhase::Waiting,
            rounds: 0,
            cover: SceneCover::default(),
            ms: 0.0,
            pipelines: 0,
            bytes: 0,
            meshes: 0,
            timed_out: false,
            error: None,
            complete: None,
            first_present_frame: None,
            first_present_ms: None,
            presented_to_window: false,
        }
    }

    /// A round came back with these numbers.
    pub(crate) fn absorb(
        &mut self,
        ms: f32,
        pipelines: u64,
        uploads: (u64, usize),
        error: Option<&str>,
        now: Instant,
    ) {
        self.ms += ms;
        self.pipelines += pipelines;
        self.bytes += uploads.0;
        self.meshes += uploads.1;
        if let Some(error) = error {
            self.error = Some(error.to_owned());
        }
        self.phase = PreparePhase::Waiting;
        self.waiting_since = now;
    }

    /// Ends warming early: the scene draws now, compiling what is missing.
    pub(crate) fn give_up(&mut self, why: String) {
        tracing::warn!(target: "regia_render", "scene prepare \"{}\": {why}", self.map_id);
        self.timed_out = true;
        self.error = Some(why);
        if matches!(self.phase, PreparePhase::Waiting | PreparePhase::Warming) {
            self.phase = PreparePhase::Resident;
        }
    }

    /// The scene is resident: its next drawn frame is its first.
    pub(crate) fn resident(&mut self) {
        if matches!(self.phase, PreparePhase::Waiting | PreparePhase::Warming) {
            self.phase = PreparePhase::Resident;
            crate::boot::mark_named("scene_resident");
        }
    }

    /// The scene's first frame reached the screen (`to_window`) or, with nothing to present,
    /// was ready to.
    pub(crate) fn presented(&mut self, frame: u64, to_window: bool, now: Instant) {
        if self.phase == PreparePhase::Presented {
            return;
        }
        self.phase = PreparePhase::Presented;
        self.first_present_frame = Some(frame);
        self.first_present_ms = Some(now.saturating_duration_since(self.opened).as_secs_f32() * 1000.0);
        self.presented_to_window = to_window;
        crate::boot::mark_named("scene_ready");
    }

    /// The first scene frame's own check.
    pub(crate) fn note_check(&mut self, complete: bool) {
        if self.complete.is_none() {
            self.complete = Some(complete);
        }
    }

    /// Whether it still holds the scene back (no round decided yet, or one running).
    pub(crate) fn holds_scene(&self) -> bool {
        matches!(self.phase, PreparePhase::Waiting | PreparePhase::Warming)
    }

    /// Its public status.
    pub(crate) fn status(&self) -> SceneReadyStatus {
        SceneReadyStatus {
            state: match self.phase {
                PreparePhase::Waiting | PreparePhase::Warming => SceneReadyState::Preparing,
                PreparePhase::Resident => SceneReadyState::Resident,
                #[cfg(any(feature = "frame-gen", test))]
                PreparePhase::Queued { .. } => SceneReadyState::Resident,
                PreparePhase::Presented => SceneReadyState::Presented,
            },
            map_id: Some(self.map_id.clone()),
            prepare: self.seq,
            uploaded_mb: (self.bytes as f64 / f64::from(1u32 << 20)) as f32,
            meshes: self.meshes,
            pipelines: self.pipelines,
            warms: self.rounds,
            ms: self.ms,
            timed_out: self.timed_out,
            error: self.error.clone(),
            complete: self.complete,
            first_present_frame: self.first_present_frame,
            first_present_ms: self.first_present_ms,
            presented_to_window: self.presented_to_window,
        }
    }
}

/// `Render`, before the window draw: hands the host's newest [`SceneReadiness::prepare`] to
/// [`crate::GameFrameWarm`]. The render plugin adds it; a host that runs its own render schedule
/// adds it before its draw.
pub fn take_scene_prepare(
    readiness: Option<ResMut<SceneReadiness>>,
    warm: Option<ResMut<crate::GameFrameWarm>>,
) {
    let (Some(mut readiness), Some(mut warm)) = (readiness, warm) else {
        return;
    };
    if let Some((seq, map_id)) = readiness.take_request() {
        warm.open_prepare(seq, map_id, Instant::now());
    }
}

/// `Render` on a headless app (no window draw), after `render_texture_views`: with a render
/// device and a started [`crate::PipelineWarmQueue`], a prepare runs the window path's warm
/// rounds, then becomes `presented` once the frame thread submitted a texture-view frame of the
/// resident scene, or right after its warm when the frame has no texture view (ADR-8122).
/// Nothing is presented to a window: `presented_to_window` is false. Without a device (nothing
/// warms or draws) it is `presented` on the first tick its frame is renderable. The render
/// plugin adds it on headless apps.
pub fn advance_headless_scene_ready(
    warm: Option<ResMut<crate::GameFrameWarm>>,
    frame: Option<Res<crate::ExtractedFrame>>,
    device: Option<Res<crate::RenderDevice>>,
    queue: Option<Res<crate::PipelineWarmQueue>>,
    mut runtime: Option<ResMut<crate::RuntimeRenderer>>,
) {
    let Some(mut warm) = warm else {
        return;
    };
    let _span = tracing::debug_span!("advance_headless_scene_ready").entered();
    let frame = frame.as_deref();
    let parts = match (device.as_deref(), queue.as_deref(), runtime.as_deref_mut()) {
        (Some(device), Some(queue), Some(runtime)) if queue.is_started() && !queue.is_shutting_down() => {
            Some((queue, &mut runtime.0, device))
        }
        _ => None,
    };
    warm.headless_tick(parts, frame, Instant::now());
}

/// `Render`, after the window draw: publishes the prepare's state into [`SceneReadiness`] and
/// sends [`SceneReady`] once at its `presented` edge. The render plugin adds it.
pub fn publish_scene_ready(
    readiness: Option<ResMut<SceneReadiness>>,
    warm: Option<Res<crate::GameFrameWarm>>,
    mut sent: MessageWriter<SceneReady>,
) {
    let (Some(mut readiness), Some(warm)) = (readiness, warm) else {
        return;
    };
    if let Some(ready) = publish(&mut readiness, &warm) {
        sent.write(ready);
    }
}

/// [`publish_scene_ready`] without the message writer: the [`SceneReady`] to send, if this is
/// the prepare's `presented` edge.
pub(crate) fn publish(readiness: &mut SceneReadiness, warm: &crate::GameFrameWarm) -> Option<SceneReady> {
    let status = warm.prepare_status()?;
    // A newer prepare the render path has not taken yet keeps its own `preparing` status.
    if status.prepare != readiness.status.prepare || readiness.request.is_some() {
        return None;
    }
    readiness.status = status;
    let s = &readiness.status;
    if s.state != SceneReadyState::Presented || readiness.sent >= s.prepare {
        return None;
    }
    readiness.sent = s.prepare;
    tracing::info!(
        target: "regia_render",
        "scene ready: \"{}\" presented {:.0} ms after its prepare ({} warms, {} pipelines, {:.1} MB){}",
        s.map_id.as_deref().unwrap_or(""),
        s.first_present_ms.unwrap_or(0.0),
        s.warms,
        s.pipelines,
        s.uploaded_mb,
        if s.timed_out { " (timed out)" } else { "" }
    );
    Some(SceneReady {
        map_id: s.map_id.clone().unwrap_or_default(),
        prepare: s.prepare,
        timed_out: s.timed_out,
        uploaded_mb: s.uploaded_mb,
        meshes: s.meshes,
        pipelines: s.pipelines,
        warms: s.warms,
        first_present_ms: s.first_present_ms.unwrap_or(0.0),
        first_present_frame: s.first_present_frame.unwrap_or(0),
        presented_to_window: s.presented_to_window,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use crate::pipeline_warm::canonical_frame;
    use crate::{GameFrameWarm, PipelineWarmQueue, RenderDevice};

    /// The window size the ticks warm at and draw at.
    const WINDOW: (u32, u32) = (64, 48);

    /// One distinct mesh of `mib` MiB of vertex data that draws a single quad (the upload is
    /// the point, not the raster work).
    fn heavy_mesh(template: &crate::ExtractedMesh, salt: usize, mib: usize) -> crate::ExtractedMesh {
        let quad = template.mesh.vertices.clone();
        let count = (mib << 20) / std::mem::size_of::<crate::lab_frame::LabVertex>();
        let mut vertices = Vec::with_capacity(count.max(4));
        for i in 0..count.max(4) {
            let mut v = quad[i % quad.len()];
            v.position[1] = salt as f32 * 0.001;
            vertices.push(v);
        }
        let mut mesh = template.clone();
        mesh.mesh = Arc::new(crate::CpuMesh {
            vertices,
            indices: vec![0, 2, 1, 0, 3, 2],
        });
        mesh
    }

    /// `frame` plus `count` heavy meshes of 1 MiB each (salted so every one is distinct).
    fn with_heavy(mut frame: crate::ExtractedFrame, count: usize, salt: usize) -> crate::ExtractedFrame {
        let template = canonical_frame().meshes[0].clone();
        for i in 0..count {
            frame.meshes.push(heavy_mesh(&template, salt + i, 1));
        }
        frame
    }

    /// Draws `frame`'s first window view; returns the pipelines the draw created on this thread.
    fn draw(runtime: &mut crate::FrameRenderer, device: &RenderDevice, frame: &crate::ExtractedFrame) -> u64 {
        let before = crate::frame_stats::counters();
        let (view_index, _) = frame.window_views()[0];
        let scene = frame.frame_scene(view_index, WINDOW.0, WINDOW.1);
        runtime.render_offscreen(device, &scene).expect("scene frame");
        crate::frame_stats::counters().since(before).pipelines
    }

    /// What `render_windows` does each tick before its draw (poll, watchdog, new-scene check,
    /// start). Returns whether the scene may draw this tick.
    fn warm_steps(
        gw: &mut GameFrameWarm,
        queue: &PipelineWarmQueue,
        runtime: &mut crate::FrameRenderer,
        frame: &crate::ExtractedFrame,
        device: &RenderDevice,
    ) -> bool {
        gw.begin_tick();
        gw.poll(runtime);
        gw.watchdog(queue, runtime);
        gw.note_scene(Some(queue), runtime, Some(frame), true);
        gw.start(
            queue,
            runtime,
            frame,
            (wgpu::TextureFormat::Rgba8Unorm, true),
            device.device_id(),
            Some(WINDOW),
        );
        !gw.holds_scene() && frame.is_renderable()
    }

    /// The host's call after its map is spawned, and the render path taking it.
    fn host_prepare(gw: &mut GameFrameWarm, readiness: &mut super::SceneReadiness, map_id: &str) {
        readiness.prepare(map_id);
        let (seq, map_id) = readiness.take_request().expect("the prepare is queued");
        gw.open_prepare(seq, map_id, Instant::now());
    }

    /// The first scene frame: what it still had to upload and compile on the frame thread.
    struct FirstFrame {
        pending: (u64, usize),
        pipelines: u64,
    }

    /// Ticks `frame_at(tick)` until the scene draws; returns its first frame.
    fn run_until_drawn(
        gw: &mut GameFrameWarm,
        queue: &PipelineWarmQueue,
        runtime: &mut crate::FrameRenderer,
        device: &RenderDevice,
        mut frame_at: impl FnMut(u32, &mut GameFrameWarm) -> crate::ExtractedFrame,
    ) -> FirstFrame {
        let deadline = Instant::now() + Duration::from_secs(600);
        let mut tick = 0u32;
        loop {
            assert!(Instant::now() < deadline, "the scene never drew");
            let frame = frame_at(tick, gw);
            tick += 1;
            if warm_steps(gw, queue, runtime, &frame, device) {
                let pending = runtime.pending_mesh_uploads(&frame, (u64::MAX, usize::MAX));
                let pipelines = draw(runtime, device, &frame);
                gw.note_frame_with(
                    0.0,
                    super::SceneFrameCheck {
                        pending_meshes: pending.1,
                        pipelines,
                        present: super::PresentNote::Presented,
                    },
                );
                return FirstFrame { pending, pipelines };
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// DO27 first-frame-stall, order (a): the map spawns in two batches behind the loading page
    /// (40 MiB, which starts the size-threshold warm, then 300 MiB more while that warm runs) and
    /// the host prepares the scene after the second. The first scene frame must find everything resident and compile nothing.
    #[test]
    fn a_map_spawned_in_two_batches_is_resident_on_its_first_frame() {
        let device = crate::device::test_device();
        let queue = PipelineWarmQueue::new(1);
        queue.start(&device);
        let mut runtime = crate::RuntimeRenderer::default().0;
        let mut gw = GameFrameWarm::default();
        gw.timeout = Duration::from_secs(600);
        gw.on_new_scene = true;
        let mut readiness = super::SceneReadiness::default();
        let mut menu = canonical_frame();
        menu.meshes.clear();
        let first = with_heavy(menu.clone(), 40, 0);
        let both = with_heavy(first.clone(), 300, 1000);
        let first_frame = run_until_drawn(&mut gw, &queue, &mut runtime, &device, |tick, gw| match tick {
            0..=2 => menu.clone(),
            // The first batch starts the size-threshold warm on this tick.
            3 => first.clone(),
            4 => {
                // The second batch lands while that warm runs (the next tick, so a warm that is
                // fast on this machine cannot draw the first batch alone); the host prepares.
                host_prepare(gw, &mut readiness, "two_batches");
                both.clone()
            }
            _ => both.clone(),
        });
        let ready = super::publish(&mut readiness, &gw).expect("SceneReady at the first present");
        assert_eq!(ready.map_id, "two_batches");
        assert!(!ready.timed_out);
        assert!(ready.warms >= 1, "{ready:?}");
        // The auto warm took the first batch; the prepare's round uploaded the second (300
        // distinct meshes of just under 1 MiB each).
        assert_eq!(
            ready.meshes, 300,
            "the prepare uploaded the second batch: {ready:?}"
        );
        assert!(
            ready.uploaded_mb > 299.0,
            "the prepare uploaded the second batch: {ready:?}"
        );
        assert_eq!(readiness.status().complete, Some(true));
        let report = gw.last_report().expect("a warm report");
        assert_eq!(report.map_id.as_deref(), Some("two_batches"));
        assert_eq!(report.complete, Some(true), "{report:?}");
        assert_eq!(
            first_frame.pending,
            (0, 0),
            "the map's first frame uploaded meshes on the frame thread"
        );
        assert_eq!(
            first_frame.pipelines, 0,
            "the map's first frame compiled pipelines"
        );
        queue.shutdown();
    }

    /// DO27 first-frame-stall, order (b) (tester A2): a warm requested while the frame had
    /// nothing to upload (a lobby backdrop already resident) must not use up the scene-less
    /// stretch: the map that arrives while it runs is warmed before it draws.
    #[test]
    fn a_warm_with_nothing_to_upload_does_not_use_up_the_stretch() {
        let device = crate::device::test_device();
        let queue = PipelineWarmQueue::new(1);
        queue.start(&device);
        let mut runtime = crate::RuntimeRenderer::default().0;
        let backdrop = canonical_frame();
        // Resident already (drawn outside the window path: no scene was shown on the window).
        draw(&mut runtime, &device, &backdrop);
        assert_eq!(
            runtime.pending_mesh_uploads(&backdrop, (u64::MAX, usize::MAX)),
            (0, 0)
        );
        let mut gw = GameFrameWarm::default();
        gw.timeout = Duration::from_secs(600);
        gw.on_new_scene = true;
        gw.request();
        let map = with_heavy(backdrop.clone(), 40, 0);
        let first_frame = run_until_drawn(&mut gw, &queue, &mut runtime, &device, |tick, _| {
            if tick == 0 { backdrop.clone() } else { map.clone() }
        });
        assert_eq!(
            first_frame.pending,
            (0, 0),
            "the map that arrived during the empty warm was uploaded on the frame thread"
        );
        assert_eq!(
            first_frame.pipelines, 0,
            "the map's first frame compiled pipelines"
        );
        queue.shutdown();
    }

    /// (c) `SceneReady` goes out once per prepare, only at the present of a frame that drew the
    /// scene, never before the scene is resident; a frame queued on the frame generation present
    /// thread counts once that thread presented it, and a flush (suspend) re-arms the prepare.
    #[test]
    fn scene_ready_fires_once_after_the_present_of_a_drawn_scene_frame() {
        use super::{PresentNote, SceneFrameCheck, SceneReadyState, publish};
        let check = |present| SceneFrameCheck {
            pending_meshes: 0,
            pipelines: 0,
            present,
        };
        // No started warm queue: nothing can warm, so a renderable frame is resident at once.
        let queue = PipelineWarmQueue::new(1);
        let runtime = crate::FrameRenderer::new();
        let mut gw = GameFrameWarm::default();
        let mut readiness = super::SceneReadiness::default();
        assert_eq!(readiness.state(), SceneReadyState::Idle);
        host_prepare(&mut gw, &mut readiness, "lobby_to_map");
        assert_eq!(readiness.state(), SceneReadyState::Preparing);
        let now = Instant::now();
        let empty = crate::ExtractedFrame::default();
        let scene = canonical_frame();
        let mut sent = 0;
        // No scene yet: held, nothing sent; a draw note before the scene is resident is ignored.
        gw.begin_tick_with(0, 0, now);
        gw.note_scene(Some(&queue), &runtime, Some(&empty), true);
        assert!(gw.holds_scene());
        gw.note_frame_with(0.0, check(PresentNote::Presented));
        sent += usize::from(publish(&mut readiness, &gw).is_some());
        assert_eq!((readiness.state(), sent), (SceneReadyState::Preparing, 0));
        // The scene arrives: resident, still not sent (nothing presented yet).
        gw.begin_tick_with(0, 0, now);
        gw.note_scene(Some(&queue), &runtime, Some(&scene), true);
        assert!(!gw.holds_scene(), "the resident scene draws");
        sent += usize::from(publish(&mut readiness, &gw).is_some());
        assert_eq!((readiness.state(), sent), (SceneReadyState::Resident, 0));
        // Its first frame goes to the frame generation present thread as ticket 5.
        gw.note_frame_with(
            0.0,
            check(PresentNote::Queued {
                ticket: 5,
                flushes: 0,
            }),
        );
        gw.begin_tick_with(4, 0, now);
        sent += usize::from(publish(&mut readiness, &gw).is_some());
        assert_eq!((readiness.state(), sent), (SceneReadyState::Resident, 0));
        // Presented: sent once, and only once.
        gw.begin_tick_with(5, 0, now);
        let ready = publish(&mut readiness, &gw).expect("the presented edge");
        assert_eq!(ready.map_id, "lobby_to_map");
        assert!(ready.presented_to_window && !ready.timed_out);
        for _ in 0..3 {
            gw.begin_tick_with(9, 0, now);
            gw.note_frame_with(0.0, check(PresentNote::Presented));
            assert!(
                publish(&mut readiness, &gw).is_none(),
                "SceneReady went out twice"
            );
        }
        assert!(readiness.is_ready());

        // A second prepare whose queued image is dropped by a suspend: back to resident, then
        // the next presented frame sends it.
        host_prepare(&mut gw, &mut readiness, "next_map");
        gw.note_scene(Some(&queue), &runtime, Some(&scene), true);
        gw.note_frame_with(
            0.0,
            check(PresentNote::Queued {
                ticket: 12,
                flushes: 0,
            }),
        );
        gw.begin_tick_with(11, 1, now);
        assert!(publish(&mut readiness, &gw).is_none());
        assert_eq!(
            readiness.state(),
            SceneReadyState::Resident,
            "re-armed after the flush"
        );
        gw.note_frame_with(0.0, check(PresentNote::Presented));
        let ready = publish(&mut readiness, &gw).expect("the second prepare's edge");
        assert_eq!((ready.map_id.as_str(), ready.prepare), ("next_map", 2));
        assert!(publish(&mut readiness, &gw).is_none());
    }

    /// (c) A prepare that never sees a renderable frame does not leave the host waiting: it ends
    /// in `presented` with `timed_out` once the request grace has passed; headless apps reach
    /// `presented` on the first renderable tick, with nothing presented to a window.
    #[test]
    fn a_prepare_without_a_scene_times_out_and_headless_prepares_are_ready_when_renderable() {
        use super::{SceneReadyState, publish};
        let queue = PipelineWarmQueue::new(1);
        let runtime = crate::FrameRenderer::new();
        let mut gw = GameFrameWarm::default();
        let mut readiness = super::SceneReadiness::default();
        host_prepare(&mut gw, &mut readiness, "never_spawned");
        let empty = crate::ExtractedFrame::default();
        gw.note_scene(Some(&queue), &runtime, Some(&empty), true);
        assert!(publish(&mut readiness, &gw).is_none());
        if let Some(p) = gw.prepare.as_mut() {
            p.waiting_since = Instant::now()
                .checked_sub(gw.request_grace + Duration::from_secs(1))
                .expect("a monotonic clock older than the grace");
        }
        gw.note_scene(Some(&queue), &runtime, Some(&empty), true);
        let ready = publish(&mut readiness, &gw).expect("the prepare gave up");
        assert!(ready.timed_out && !ready.presented_to_window, "{ready:?}");
        assert_eq!(readiness.state(), SceneReadyState::Presented);

        let mut headless = GameFrameWarm::default();
        let mut readiness = super::SceneReadiness::default();
        host_prepare(&mut headless, &mut readiness, "headless");
        headless.note_headless_tick(false, Instant::now());
        assert!(publish(&mut readiness, &headless).is_none());
        headless.note_headless_tick(true, Instant::now());
        let ready = publish(&mut readiness, &headless).expect("renderable: ready");
        assert!(!ready.timed_out && !ready.presented_to_window);
        headless.note_headless_tick(true, Instant::now());
        assert!(publish(&mut readiness, &headless).is_none(), "once");
    }

    /// Review rev-d row 2 (ADR-8122): a headless host (render-to-texture cameras, no window)
    /// reaches `presented` only after the prepare's warm round uploaded the scene and a
    /// texture-view frame of the resident scene was submitted; never on its first renderable
    /// tick. Driven through the render path's own systems.
    #[test]
    fn a_headless_prepare_is_presented_after_a_warm_round_and_a_submitted_frame() {
        use regia_ecs::prelude::*;
        let device = crate::device::test_device();
        let queue = PipelineWarmQueue::new(1);
        queue.start(&device);
        let mut world = World::new();
        world.insert_resource(device.clone());
        world.insert_resource(queue.clone());
        world.insert_resource(crate::RuntimeRenderer::default());
        let mut gw = GameFrameWarm::default();
        gw.timeout = Duration::from_secs(600);
        gw.request_grace = Duration::from_secs(600);
        world.insert_resource(gw);
        world.init_resource::<super::SceneReadiness>();
        world.init_resource::<Messages<super::SceneReady>>();
        // The map behind a render-to-texture camera (a headless capture host).
        let mut frame = canonical_frame();
        frame.views[0].target = crate::runtime::ExtractedTarget::Texture { asset_id: 0x8122 };
        world.insert_resource(frame);
        let mut render = Schedule::default();
        render.add_systems(
            (
                super::take_scene_prepare,
                crate::runtime::render_texture_views,
                super::advance_headless_scene_ready,
                super::publish_scene_ready,
            )
                .chain(),
        );
        world
            .resource_mut::<super::SceneReadiness>()
            .prepare("headless_map");
        render.run(&mut world);
        assert_ne!(
            world.resource::<super::SceneReadiness>().state(),
            super::SceneReadyState::Presented,
            "a headless prepare was presented on its first renderable tick, before any warm round \
             or submitted frame"
        );
        let deadline = Instant::now() + Duration::from_secs(600);
        let mut resident_ticks = 0u32;
        while !world.resource::<super::SceneReadiness>().is_ready() {
            assert!(
                Instant::now() < deadline,
                "the headless prepare never became presented"
            );
            if world.resource::<super::SceneReadiness>().state() == super::SceneReadyState::Resident {
                resident_ticks += 1;
            }
            std::thread::sleep(Duration::from_millis(5));
            render.run(&mut world);
        }
        let status = world.resource::<super::SceneReadiness>().status().clone();
        assert!(status.warms >= 1, "presented without a warm round: {status:?}");
        assert_eq!(
            status.meshes, 1,
            "the prepare's warm uploaded the scene (not the frame thread): {status:?}"
        );
        assert!(!status.timed_out && !status.presented_to_window, "{status:?}");
        assert!(
            resident_ticks >= 1,
            "presented on the tick it became resident, before a texture-view frame was submitted"
        );
        assert_eq!(world.resource::<Messages<super::SceneReady>>().len(), 1);
        queue.shutdown();
    }

    /// (c) A prepare whose warm the watchdog cancels (a stuck job holds the only worker) still
    /// ends in `presented`, late, with `timed_out`: the renderer comes back and the scene draws.
    #[test]
    fn a_prepare_whose_warm_is_cancelled_still_sends_scene_ready_timed_out() {
        use super::{PresentNote, SceneFrameCheck, publish};
        let device = crate::device::test_device();
        let queue = PipelineWarmQueue::new(1);
        let (release, stuck) = std::sync::mpsc::channel::<()>();
        queue.push(crate::WarmTier::Boot, "stuck boot job", move |_| {
            let _ = stuck.recv();
            Ok(())
        });
        queue.start(&device);
        let deadline = Instant::now() + Duration::from_secs(60);
        while !queue.describe().contains("running [boot] stuck boot job") {
            assert!(Instant::now() < deadline, "the stuck job never started");
            std::thread::sleep(Duration::from_millis(2));
        }
        let scene = canonical_frame();
        let mut runtime = crate::RuntimeRenderer::default().0;
        let mut gw = GameFrameWarm::default();
        let mut readiness = super::SceneReadiness::default();
        host_prepare(&mut gw, &mut readiness, "stuck");
        assert!(!warm_steps(&mut gw, &queue, &mut runtime, &scene, &device));
        assert!(
            gw.is_warming(),
            "the prepare's round is queued behind the stuck job"
        );
        let timeout = gw.timeout;
        gw.watchdog_at(
            &queue,
            &mut runtime,
            Instant::now() + timeout + Duration::from_secs(1),
        );
        assert!(!gw.holds_scene(), "the watchdog gave up: the scene draws");
        assert!(publish(&mut readiness, &gw).is_none(), "not before the present");
        draw(&mut runtime, &device, &scene);
        gw.note_frame_with(
            0.0,
            SceneFrameCheck {
                pending_meshes: 1,
                pipelines: 3,
                present: PresentNote::Presented,
            },
        );
        let ready = publish(&mut readiness, &gw).expect("SceneReady, late");
        assert!(ready.timed_out, "{ready:?}");
        assert_eq!(readiness.status().complete, Some(false));
        release.send(()).expect("release the stuck job");
        queue.shutdown();
    }
}
