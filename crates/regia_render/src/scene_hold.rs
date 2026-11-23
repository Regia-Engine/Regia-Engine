// SPDX-License-Identifier: MPL-2.0
//! Scene hold (ADR-7890): a paused game keeps its last scene image and redraws only the UI.
//!
//! A host turns [`SceneHold`] on (a pause menu, an inventory over a frozen world). The next frame
//! renders the scene as usual and keeps a copy of the window's composited scene image: the
//! display-stage input after post and upscale, before the UI pass. Every following frame skips
//! the whole scene ([`crate::FrameRenderer::render_into`] is not called: no uploads, shadows,
//! GI, terrain, VFX or post) and only writes the kept image back, draws the UI over it and
//! presents. Turning the hold off renders full frames again.
//!
//! The kept image is per window ([`RetainedScene`]). It is captured again, for one scene frame,
//! whenever what it was captured for changes: the window's size, the draw format (an HDR toggle or
//! a calibration that needs the display stage), a new surface (surface loss recreates the window
//! surface and with it an empty `RetainedScene`), or a host [`SceneHold::refresh`] (a graphics
//! setting changed behind the pause menu). Frame generation does not run while held.
//!
//! Temporal history is kept, not reset: a held frame does not run the renderer, so its frame
//! index, its previous-frame matrices and every history target (TAA/TSR, GI accumulation) still
//! describe the last scene frame. The first frame after the hold reprojects from that frame
//! exactly as it would after one long frame, so nothing pops beyond that one frame.
//!
//! What a frame did is in [`SceneHoldFrame`]: `held`, the scene views it rendered (0 while
//! held) and, on devices with timestamp queries, the GPU time of the composite and the UI. The
//! window path publishes the newest one for telemetry ([`last_window_frame`]); held frames are
//! counted apart there and feed no hitch median and no frame-time governor.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use regia_app::prelude::*;
use serde::Serialize;

use crate::frame_work::counted::*;
use crate::readback::{ST_FAILED, ST_IDLE, ST_INFLIGHT, ST_READY, store_map_result};
use crate::{FrameRenderer, RenderDevice, RenderError};

/// The host's switch for the scene hold (ADR-7890). Off by default.
///
/// The render plugin inserts it; `regia_framework`'s `GameFlowSettings::hold_scene_when_paused`
/// turns it on in `GameFlow::Paused`, and the agent protocol drives it through
/// `render.set_scene_hold`.
///
/// [`Self::set_follow_pause`] (ADR-10105) makes the hold follow the gameplay pause instead: the
/// render plugin's [`follow_pause`] system copies `Time::paused` in every frame, and the hold is on
/// while the game is paused. That is how a paused game's world pixels stay identical however long
/// the pause lasts: the renderer's own temporal state (TAA/TSR jitter and history, noise phases,
/// GI accumulation, exposure adaptation) would otherwise keep moving under a still world. A game
/// that wants a live camera while paused (photo mode) leaves it off.
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SceneHold {
    on: bool,
    refresh: u64,
    follow_pause: bool,
    paused: bool,
}

impl SceneHold {
    /// Turns the hold on or off. On: the next frame renders the scene once and keeps it; later
    /// frames composite the UI over the kept image. Off: full frames from the next frame (unless
    /// [`Self::set_follow_pause`] holds it for a gameplay pause).
    pub fn set(&mut self, on: bool) {
        self.on = on;
    }

    /// Whether the hold is on: set by [`Self::set`], or following a gameplay pause.
    #[must_use]
    pub fn is_on(&self) -> bool {
        self.on || (self.follow_pause && self.paused)
    }

    /// Makes the hold follow the gameplay pause (ADR-10105): on while `Time::paused` is set
    /// ([`Self::note_paused`]), whatever [`Self::set`] says. Off by default.
    pub fn set_follow_pause(&mut self, follow: bool) {
        self.follow_pause = follow;
    }

    /// Whether the hold follows the gameplay pause ([`Self::set_follow_pause`]).
    #[must_use]
    pub fn follows_pause(&self) -> bool {
        self.follow_pause
    }

    /// Records the gameplay pause (`Time::paused`); the [`follow_pause`] system calls it every
    /// frame. It changes nothing unless [`Self::set_follow_pause`] is on.
    pub fn note_paused(&mut self, paused: bool) {
        self.paused = paused;
    }

    /// The gameplay pause as last recorded by [`Self::note_paused`].
    #[must_use]
    pub fn paused(&self) -> bool {
        self.paused
    }

    /// Renders one more scene frame and holds that one (a setting changed behind a pause menu).
    /// Does nothing while the hold is off.
    pub fn refresh(&mut self) {
        self.refresh = self.refresh.wrapping_add(1);
    }

    /// How many refreshes were asked for since start.
    #[must_use]
    pub fn refreshes(&self) -> u64 {
        self.refresh
    }
}

/// `PostUpdate` (the render plugin): copies `Time::paused` into [`SceneHold::note_paused`], so a
/// hold that follows the pause turns on and off with it (ADR-10105). Writes only on a change.
pub fn follow_pause(time: Option<Res<Time>>, hold: Option<ResMut<SceneHold>>) {
    let (Some(time), Some(mut hold)) = (time, hold) else {
        return;
    };
    if hold.paused != time.paused {
        hold.paused = time.paused;
    }
}

/// What a kept scene image was captured for: the window's draw size and format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HoldKey {
    /// Width, px.
    pub width: u32,
    /// Height, px.
    pub height: u32,
    /// The draw format (the swap chain's, or the display stage's intermediate).
    pub format: wgpu::TextureFormat,
}

/// What one frame of a [`RetainedScene`] does.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HoldStep {
    /// The hold is off (or cannot be used on this target): the scene renders as usual.
    #[default]
    Full,
    /// The hold is on and nothing valid is kept: the scene renders and is kept.
    Capture,
    /// The kept image is written back; no scene pass runs.
    Held,
}

impl HoldStep {
    /// `full`, `capture` or `held`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Capture => "capture",
            Self::Held => "held",
        }
    }

    /// Whether the scene is rendered this frame.
    #[must_use]
    pub fn renders_scene(self) -> bool {
        self != Self::Held
    }
}

/// What one window frame did with the scene hold ([`RetainedScene::last_frame`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct SceneHoldFrame {
    /// The frame's step.
    pub step: HoldStep,
    /// The frame showed the kept image: no scene pass ran.
    pub held: bool,
    /// Scene views rendered this frame (`render_into` calls; 0 while held).
    pub scene_views: u32,
    /// GPU time of the newest timed held frame: writing the kept image back, ms (0 untimed).
    pub composite_ms: f32,
    /// GPU time of the newest timed held frame: the UI and the display stage, ms (0 untimed).
    pub ui_ms: f32,
    /// `composite_ms + ui_ms`: a held frame's whole GPU time (0 untimed).
    pub gpu_ms: f32,
    /// The GPU times above come from a resolved timestamp sample.
    pub timed: bool,
}

impl SceneHoldFrame {
    /// The GPU passes of a held frame, in order: `scene hold composite`, then `ui`. Empty for a
    /// frame that rendered the scene (its passes are the renderer's [`crate::GpuTimings`]).
    #[must_use]
    pub fn gpu_passes(&self) -> Vec<(&'static str, f32)> {
        if self.held {
            vec![("scene hold composite", self.composite_ms), ("ui", self.ui_ms)]
        } else {
            Vec::new()
        }
    }
}

/// The GPU pass list of a frame: the renderer's passes for a scene frame, the composite and the UI
/// alone for a held one ([`SceneHoldFrame::gpu_passes`]). The overlay, the hitch record and
/// `render.scene_hold` read this.
#[must_use]
pub fn gpu_pass_report(renderer: &FrameRenderer, frame: &SceneHoldFrame) -> Vec<(&'static str, f32)> {
    if frame.held {
        frame.gpu_passes()
    } else {
        renderer.gpu_timings().passes().to_vec()
    }
}

/// Running counts of a [`RetainedScene`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct SceneHoldCounts {
    /// Frames that showed the kept image.
    pub held_frames: u64,
    /// Scene frames that were kept (the first of a hold, and each re-capture).
    pub captures: u64,
    /// Frames rendered without the hold.
    pub full_frames: u64,
}

/// The kept image's copy onto a target that is not a copy destination (a swap-chain image): a
/// full-screen `textureLoad` of the same format, so the target ends with the kept values bit for
/// bit (the frame-generation blit shader's `fs_copy`).
#[derive(Default)]
struct HoldBlit {
    layout: Option<(wgpu::ShaderModule, wgpu::BindGroupLayout, wgpu::PipelineLayout)>,
    pipelines: Vec<(wgpu::TextureFormat, wgpu::RenderPipeline)>,
    /// The bind group of the kept image (rebuilt when the image is).
    group: Option<wgpu::BindGroup>,
}

impl HoldBlit {
    fn clear_groups(&mut self) {
        self.group = None;
    }

    fn record(
        &mut self,
        device: &RenderDevice,
        encoder: &mut wgpu::CommandEncoder,
        source: &wgpu::TextureView,
        target: &wgpu::TextureView,
        format: wgpu::TextureFormat,
    ) -> Result<(), RenderError> {
        let d = device.tracked();
        if self.layout.is_none() {
            let module = crate::shader_lib::compose_source(
                crate::frame_gen::wgsl::BLIT,
                "scene hold blit",
                std::collections::HashMap::new(),
            )?;
            let module = device.device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("scene hold blit"),
                source: wgpu::ShaderSource::Naga(std::borrow::Cow::Owned(module)),
            });
            let bgl = device
                .device
                .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("scene hold blit"),
                    entries: &[wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: false },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    }],
                });
            let layout = device
                .device
                .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some("scene hold blit"),
                    bind_group_layouts: &[Some(&bgl)],
                    immediate_size: 0,
                });
            self.layout = Some((module, bgl, layout));
        }
        let Some((module, bgl, layout)) = self.layout.as_ref() else {
            return Err(RenderError::Shader("scene hold blit layout missing".into()));
        };
        if !self.pipelines.iter().any(|(f, _)| *f == format) {
            let pipeline = d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("scene hold blit"),
                layout: Some(layout),
                vertex: wgpu::VertexState {
                    module,
                    entry_point: Some("vs_main"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module,
                    entry_point: Some("fs_copy"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: crate::pipeline_cache::cache_for(&device.device).as_deref(),
            });
            self.pipelines.push((format, pipeline));
        }
        let group = self.group.get_or_insert_with(|| {
            d.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("scene hold blit"),
                layout: bgl,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(source),
                }],
            })
        });
        let Some((_, pipeline)) = self.pipelines.iter().find(|(f, _)| *f == format) else {
            return Err(RenderError::Shader("scene hold blit pipeline missing".into()));
        };
        let mut pass = encoder.counted_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("scene hold composite"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        pass.counted_set_pipeline(pipeline);
        pass.counted_set_bind_group(0, &*group, &[]);
        pass.counted_draw(0..3, 0..1);
        Ok(())
    }
}

/// Timestamp queries of a held frame: before the composite, after it, after the UI.
const HOLD_QUERIES: u32 = 3;
/// Readback slots for the held-frame timestamps.
const HOLD_SLOTS: usize = 3;

struct HoldSlot {
    staging: wgpu::Buffer,
    state: Arc<AtomicU8>,
}

/// The held frame's GPU timer: its own three queries, read back without waiting.
struct HoldTimer {
    set: wgpu::QuerySet,
    resolve: wgpu::Buffer,
    slots: [HoldSlot; HOLD_SLOTS],
    /// The slot this frame's stamps go to, until it is mapped after the submit.
    recording: Option<usize>,
    pending: Option<usize>,
}

impl HoldTimer {
    fn new(device: &RenderDevice) -> Self {
        let d = device.tracked();
        let size = u64::from(HOLD_QUERIES) * 8;
        let slot = |label: &'static str| HoldSlot {
            staging: d.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            }),
            state: Arc::new(AtomicU8::new(ST_IDLE)),
        };
        Self {
            set: device.device.create_query_set(&wgpu::QuerySetDescriptor {
                label: Some("scene hold timestamps"),
                ty: wgpu::QueryType::Timestamp,
                count: HOLD_QUERIES,
            }),
            resolve: d.create_buffer(&wgpu::BufferDescriptor {
                label: Some("scene hold timestamp resolve"),
                size,
                usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            }),
            slots: [
                slot("scene hold timestamps 0"),
                slot("scene hold timestamps 1"),
                slot("scene hold timestamps 2"),
            ],
            recording: None,
            pending: None,
        }
    }

    /// Reads every mapped slot; the newest sample wins. `(composite_ms, ui_ms)`.
    fn collect(&mut self, period: f32) -> Option<(f32, f32)> {
        let mut newest = None;
        for slot in &self.slots {
            match slot.state.load(Ordering::Acquire) {
                ST_FAILED => slot.state.store(ST_IDLE, Ordering::Release),
                ST_READY => {
                    if let Ok(mapped) = slot.staging.slice(..).get_mapped_range() {
                        let stamps: Vec<u64> = mapped
                            .as_chunks::<8>()
                            .0
                            .iter()
                            .map(|c| u64::from_le_bytes(*c))
                            .collect();
                        drop(mapped);
                        if let [a, b, c] = stamps[..] {
                            let ms = |x: u64, y: u64| y.saturating_sub(x) as f32 * period / 1_000_000.0;
                            newest = Some((ms(a, b), ms(b, c)));
                        }
                    }
                    slot.staging.unmap();
                    slot.state.store(ST_IDLE, Ordering::Release);
                }
                _ => {}
            }
        }
        newest
    }
}

/// One window frame's scene decision when both scene gates apply: the host's open scene prepare
/// (ADR-7883) and the paused-scene hold (ADR-7890). A prepare that holds the scene back drops it:
/// the frame is [`HoldStep::Full`], no scene pass runs, and a kept image is freed (the window shows
/// a clear plus the UI, as during any prepare). Otherwise the hold decides, and a
/// [`HoldStep::Held`] frame skips every scene pass. `drawable` is whether the frame has a scene to
/// draw at all (renderable views, no splash over them); `target` is the draw image's key, `None`
/// when it cannot be copied. Returns the step and whether the scene is rendered this frame.
pub(crate) fn plan_window_scene(
    retained: &mut RetainedScene,
    hold: &SceneHold,
    drawable: bool,
    prepare_holds: bool,
    target: Option<HoldKey>,
) -> (HoldStep, bool) {
    let drawable = drawable && !prepare_holds;
    let step = retained.plan(hold, target.filter(|_| drawable));
    (step, drawable && step.renders_scene())
}

/// One window's kept scene image and the per-frame decision (ADR-7890).
///
/// Per frame: [`Self::plan`], then render the scene only when the step
/// [`HoldStep::renders_scene`], then [`Self::record`] into the frame's tail encoder before the UI,
/// then [`Self::finish_ui`] after the UI and display stage, and [`Self::after_submit`] after the
/// submit.
#[derive(Default)]
pub struct RetainedScene {
    image: Option<(wgpu::Texture, wgpu::TextureView, HoldKey)>,
    valid: bool,
    refresh_seen: u64,
    blit: HoldBlit,
    timer: Option<HoldTimer>,
    step: HoldStep,
    last: SceneHoldFrame,
    counts: SceneHoldCounts,
}

impl RetainedScene {
    /// Decides this frame. `key` is what the frame draws into, `None` when this target cannot be
    /// held this frame (no scene to draw, a draw image that cannot be copied, a splash over it):
    /// then the frame is [`HoldStep::Full`] and nothing kept survives.
    pub fn plan(&mut self, hold: &SceneHold, key: Option<HoldKey>) -> HoldStep {
        let step = match key {
            Some(key) if hold.is_on() => {
                if hold.refresh != self.refresh_seen {
                    self.valid = false;
                }
                let same = self.image.as_ref().is_some_and(|(_, _, k)| *k == key);
                if self.valid && same {
                    HoldStep::Held
                } else {
                    self.valid = false;
                    HoldStep::Capture
                }
            }
            _ => {
                // Off (or unusable): the kept image is stale from now on; free it.
                self.valid = false;
                self.image = None;
                self.blit.clear_groups();
                HoldStep::Full
            }
        };
        self.refresh_seen = hold.refresh;
        self.step = step;
        self.last = SceneHoldFrame {
            step,
            held: step == HoldStep::Held,
            composite_ms: self.last.composite_ms,
            ui_ms: self.last.ui_ms,
            gpu_ms: self.last.gpu_ms,
            timed: self.last.timed,
            scene_views: 0,
        };
        match step {
            HoldStep::Full => self.counts.full_frames += 1,
            HoldStep::Capture => self.counts.captures += 1,
            HoldStep::Held => self.counts.held_frames += 1,
        }
        step
    }

    /// The step [`Self::plan`] chose for this frame.
    #[must_use]
    pub fn step(&self) -> HoldStep {
        self.step
    }

    /// Notes the scene views this frame rendered (`render_into` calls).
    pub fn note_scene_views(&mut self, views: u32) {
        self.last.scene_views = self.last.scene_views.saturating_add(views);
    }

    /// The scene was not drawn after all (a failed draw, a missing scene): nothing is kept.
    pub fn cancel_capture(&mut self) {
        if self.step == HoldStep::Capture {
            self.step = HoldStep::Full;
            self.last.step = HoldStep::Full;
            self.valid = false;
        }
    }

    /// Records this frame's hold work into the tail encoder, before the UI: a capture copies the
    /// scene image `texture` (it must allow `COPY_SRC`) into the kept image; a held frame writes
    /// the kept image over `view` (copied when `texture` allows `COPY_DST`, else drawn).
    ///
    /// # Errors
    /// The composite's shader failed to build; the caller clears the frame instead.
    pub fn record(
        &mut self,
        device: &RenderDevice,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        texture: Option<&wgpu::Texture>,
        format: wgpu::TextureFormat,
    ) -> Result<(), RenderError> {
        match self.step {
            HoldStep::Full => Ok(()),
            HoldStep::Capture => {
                self.capture(device, encoder, texture, format);
                Ok(())
            }
            HoldStep::Held => {
                let _span = tracing::info_span!("scene_hold_composite").entered();
                self.begin_timer(device, encoder);
                let Some((kept, kept_view, _)) = self.image.as_ref() else {
                    return Err(RenderError::Shader("scene hold: no kept image".into()));
                };
                match texture {
                    Some(target)
                        if target.usage().contains(wgpu::TextureUsages::COPY_DST)
                            && target.format() == kept.format()
                            && target.size() == kept.size() =>
                    {
                        encoder.copy_texture_to_texture(
                            kept.as_image_copy(),
                            target.as_image_copy(),
                            kept.size(),
                        );
                    }
                    _ => {
                        let kept_view = kept_view.clone();
                        self.blit.record(device, encoder, &kept_view, view, format)?;
                    }
                }
                if let Some(timer) = self.timer.as_ref()
                    && timer.recording.is_some()
                {
                    encoder.write_timestamp(&timer.set, 1);
                }
                Ok(())
            }
        }
    }

    fn capture(
        &mut self,
        device: &RenderDevice,
        encoder: &mut wgpu::CommandEncoder,
        texture: Option<&wgpu::Texture>,
        format: wgpu::TextureFormat,
    ) {
        let Some(source) = texture.filter(|t| t.usage().contains(wgpu::TextureUsages::COPY_SRC)) else {
            // Nothing copyable: this frame is a full frame and the next one captures again.
            self.cancel_capture();
            return;
        };
        let key = HoldKey {
            width: source.width(),
            height: source.height(),
            format,
        };
        if self.image.as_ref().is_none_or(|(_, _, k)| *k != key) {
            let texture = device.tracked().create_texture(&wgpu::TextureDescriptor {
                label: Some("scene hold image"),
                size: source.size(),
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: source.format(),
                usage: wgpu::TextureUsages::COPY_DST
                    | wgpu::TextureUsages::COPY_SRC
                    | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            self.blit.clear_groups();
            self.image = Some((texture, view, key));
        }
        if let Some((kept, _, _)) = self.image.as_ref() {
            encoder.copy_texture_to_texture(source.as_image_copy(), kept.as_image_copy(), source.size());
            self.valid = true;
        }
    }

    fn begin_timer(&mut self, device: &RenderDevice, encoder: &mut wgpu::CommandEncoder) {
        if !device.capabilities.timestamp_query {
            return;
        }
        let timer = self.timer.get_or_insert_with(|| HoldTimer::new(device));
        timer.recording = timer
            .slots
            .iter()
            .position(|slot| slot.state.load(Ordering::Acquire) == ST_IDLE);
        if timer.recording.is_some() {
            encoder.write_timestamp(&timer.set, 0);
        }
    }

    /// Closes a held frame's timing after the UI and the display stage were recorded into the same
    /// encoder. Does nothing on other frames or without timestamp queries.
    pub fn finish_ui(&mut self, encoder: &mut wgpu::CommandEncoder) {
        if self.step != HoldStep::Held {
            return;
        }
        let Some(timer) = self.timer.as_mut() else {
            return;
        };
        let Some(index) = timer.recording.take() else {
            return;
        };
        encoder.write_timestamp(&timer.set, 2);
        encoder.resolve_query_set(&timer.set, 0..HOLD_QUERIES, &timer.resolve, 0);
        encoder.copy_buffer_to_buffer(
            &timer.resolve,
            0,
            &timer.slots[index].staging,
            0,
            u64::from(HOLD_QUERIES) * 8,
        );
        timer.pending = Some(index);
    }

    /// After the frame's submit: starts the read of this frame's stamps and takes any sample that
    /// is ready (never waits).
    pub fn after_submit(&mut self, device: &RenderDevice) {
        let Some(timer) = self.timer.as_mut() else {
            return;
        };
        if let Some(index) = timer.pending.take() {
            let state = Arc::clone(&timer.slots[index].state);
            state.store(ST_INFLIGHT, Ordering::Release);
            crate::frame_stats::note_map_async();
            timer.slots[index]
                .staging
                .slice(..)
                .map_async(wgpu::MapMode::Read, move |result| {
                    store_map_result(&state, result.is_ok())
                });
        }
        // A non-waiting poll runs the map callbacks of earlier frames.
        let _ = device.device.poll(wgpu::PollType::Poll);
        if let Some((composite_ms, ui_ms)) = timer.collect(device.queue.get_timestamp_period()) {
            self.last.composite_ms = composite_ms;
            self.last.ui_ms = ui_ms;
            self.last.gpu_ms = composite_ms + ui_ms;
            self.last.timed = true;
        }
    }

    /// What this frame did (after [`Self::plan`]; GPU times are the newest resolved held sample).
    #[must_use]
    pub fn last_frame(&self) -> SceneHoldFrame {
        self.last
    }

    /// Running counts.
    #[must_use]
    pub fn counts(&self) -> SceneHoldCounts {
        self.counts
    }

    /// Whether a valid scene image is kept.
    #[must_use]
    pub fn is_holding(&self) -> bool {
        self.valid && self.image.is_some()
    }

    /// The kept image's key, if any.
    #[must_use]
    pub fn kept_key(&self) -> Option<HoldKey> {
        self.image.as_ref().map(|(_, _, key)| *key)
    }
}

/// The newest window frame's hold report, for telemetry (set by the window path every frame).
static LAST_WINDOW: std::sync::Mutex<(u64, SceneHoldFrame)> = std::sync::Mutex::new((
    0,
    SceneHoldFrame {
        step: HoldStep::Full,
        held: false,
        scene_views: 0,
        composite_ms: 0.0,
        ui_ms: 0.0,
        gpu_ms: 0.0,
        timed: false,
    },
));

/// Publishes the window path's frame report ([`last_window_frame`]).
pub(crate) fn set_last_window_frame(frame: SceneHoldFrame) {
    let mut last = LAST_WINDOW
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    last.0 = last.0.wrapping_add(1);
    last.1 = frame;
}

/// The newest window frame's hold report and its sequence number (it grows by one per window
/// frame; `0` before the first). A frame with `held: true` rendered no scene: telemetry counts it
/// apart and keeps it out of the hitch median and the frame-time governor.
#[must_use]
pub fn last_window_frame() -> (u64, SceneHoldFrame) {
    *LAST_WINDOW
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// One offscreen frame the way the window path draws it, with the hold: every extracted window
/// view into `target` (unless held), the hold's tail work, then the UI. For tests and tools.
#[cfg(test)]
pub(crate) fn render_views_held(
    device: &RenderDevice,
    renderer: &mut FrameRenderer,
    extracted: &crate::ExtractedFrame,
    target: (&wgpu::Texture, &wgpu::TextureView),
    overlay: Option<&crate::WindowOverlay>,
    retained: &mut RetainedScene,
    (hold, prepare_holds): (&SceneHold, bool),
) -> Result<SceneHoldFrame, RenderError> {
    let (texture, view) = target;
    let (width, height, format) = (texture.width(), texture.height(), texture.format());
    let key = texture
        .usage()
        .contains(wgpu::TextureUsages::COPY_SRC)
        .then_some(HoldKey {
            width,
            height,
            format,
        });
    let (step, draw_scene) = plan_window_scene(retained, hold, extracted.is_renderable(), prepare_holds, key);
    if !draw_scene && step != HoldStep::Held {
        // The window path clears a frame that draws no scene and holds nothing (`surface.rs`).
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("scene hold test clear"),
            });
        crate::surface::encode_clear(&mut encoder, view, [0.0, 0.0, 0.0, 1.0]);
        device.queue.submit([encoder.finish()]);
    }
    if draw_scene {
        crate::surface::sync_window_frame(renderer, extracted);
        let views = extracted.window_views();
        renderer.set_light_extras(extracted.light_extras.clone());
        for (index, (view_index, window_view)) in views.iter().enumerate() {
            let rect = crate::surface::pixel_viewport(window_view.viewport, width, height);
            let scene = extracted.frame_scene(*view_index, rect[2], rect[3]);
            let frame_target = crate::FrameTarget::rect(view, format, width, height, rect, index > 0)
                .with_color(Some(texture));
            renderer.render_into(device, &scene, frame_target)?;
            retained.note_scene_views(1);
        }
    }
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("scene hold test tail"),
        });
    retained.record(device, &mut encoder, view, Some(texture), format)?;
    if let Some(overlay) = overlay {
        crate::overlay::record_overlay_on(
            overlay,
            device,
            &mut encoder,
            Some(texture),
            view,
            format,
            width,
            height,
        );
    }
    retained.finish_ui(&mut encoder);
    device.queue.submit([encoder.finish()]);
    retained.after_submit(device);
    Ok(retained.last_frame())
}

#[cfg(test)]
mod tests;
