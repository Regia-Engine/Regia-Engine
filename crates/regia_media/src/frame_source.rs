// SPDX-License-Identifier: MPL-2.0
//! The renderer half of video playback (specs/50 § 2.7, ADR-2044): [`MediaFrameSource`] implements
//! `regia_render::VideoFrameSource` over the ECS players, [`VideoFramePlugin`] (opt-in) installs it,
//! decodes through the bounded ring (`crate::ring`), publishes each newly presented picture as a
//! sRGB RGBA8 texture (the CPU fast path, `regia_render::video::yuv_to_rgba8`) and, when
//! [`VideoFrameSettings::gpu_convert`] is on, also hands the planes to the GPU convert pass
//! (`regia_render::VideoConvertJobs`). [`VideoMaterial`] binds a player's texture to a mesh's
//! material `video` slot.
//!
//! **Fallback frame.** The texture holds the player's clear color (the `.video.ron` `clear_color`,
//! black by default) while there is no picture (closed, opening, failed) and once decoding has
//! stalled: [`VideoFrameSettings::stall_ticks`] ticks in a row in `Buffering`. Before that the last
//! picture holds, so a short hiccup does not flash. Everything is counted in ticks, never wall clock.
//!
//! **Controls.** Widgets and agents queue `VideoControl`s through the source; they are applied in
//! `Update` before the clock ticks (`VideoSystems::ApplyControls`), in queue order.
//!
//! **Memory.** Per player: the ring's byte budget and one RGBA8 buffer. The fullscreen queue
//! and `rgba()` share that buffer. It is rewritten in place when this slot is the only owner,
//! which is the steady state between publishes. A `VideoMaterial` is what copies the buffer
//! into the material texture; a fullscreen player with no material does not. With the GPU
//! path, the queued picture clones the decoder's plane handles (no second sample copy) plus
//! two GPU textures.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use regia_app::prelude::{App, Plugin, PostUpdate, Update};
use regia_ecs::prelude::*;
use regia_render::video::{VIDEO_CONTROL_QUEUE, convert_rows};
use regia_render::{
    DynamicTexture, MeshRenderer, OwnedYuv, VideoControl, VideoControlError, VideoConvertJobs,
    VideoFrameInfo, VideoFrameSource, VideoFrames, VideoPlayback, VideoSlot, VideoSystems, YuvMatrix,
    YuvPlanes, video_material,
};
use serde::{Deserialize, Serialize};

use crate::codec::DecodedPicture;
use crate::player::{ClockMode, LoopMode, PlayState, SeekMode};
use crate::plugin::{MediaSettings, VideoPlayer, VideoPlayerState};
use crate::ring::{RingBudget, RingStats};

/// Settings of the renderer half.
#[derive(Resource, Debug, Clone, PartialEq)]
pub struct VideoFrameSettings {
    /// Ticks in a row in `Buffering` before the fallback frame replaces the last picture.
    pub stall_ticks: u32,
    /// Also convert on the GPU (`regia_render::VideoConvertPass`; needs a `GpuVideoJobs` resource
    /// and the pass enabled on the renderer). Off by default.
    pub gpu_convert: bool,
    /// The decode ring installed by the plugin when `MediaSettings::ring_budget` is unset.
    pub ring: RingBudget,
}

impl Default for VideoFrameSettings {
    fn default() -> Self {
        Self {
            stall_ticks: 30,
            gpu_convert: false,
            ring: RingBudget::default(),
        }
    }
}

/// The GPU convert queue shared with the renderer (insert it and enable the pass with
/// `FrameRenderer::enable_video_convert` to use the GPU path).
#[derive(Resource, Debug, Clone, Default)]
pub struct GpuVideoJobs(pub VideoConvertJobs);

/// The fullscreen video queue shared with the renderer's `VideoFullscreenPass` (insert it and enable
/// the pass with `FrameRenderer::enable_video_fullscreen`). Players with a `VideoFullscreen`
/// component are drawn over the view after the post stack.
#[derive(Resource, Debug, Clone, Default)]
pub struct FullscreenVideos(pub regia_render::VideoFullscreenQueue);

/// Binds a player's picture to this entity's `MeshRenderer` material (`video` parameter).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoMaterial {
    /// The player entity.
    pub player: Entity,
    /// Which slot shows the video.
    pub slot: VideoSlot,
}

/// One player's published state.
#[derive(Debug, Default)]
struct Slot {
    info: Option<VideoFrameInfo>,
    texture: Option<DynamicTexture>,
    rgba: Option<Arc<[u8]>>,
    controls: VecDeque<VideoControl>,
    presented_seen: Option<(u64, i64)>,
    fallback_shown: bool,
    stall: u32,
    ring: Option<RingStats>,
    controls_applied: u64,
    last_control_error: Option<String>,
    publishes: u64,
    gpu_submits: u64,
    /// `publishes` value last copied into the material texture. `0` means never.
    texture_written: u64,
    /// Reused RGBA working buffer. Capacity sticks for a given picture size.
    scratch: Vec<u8>,
}

/// Per-player diagnostics (`media.state`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrameSourceStatus {
    /// Entity bits.
    pub player: u64,
    /// Playback state name.
    pub state: String,
    /// Texture size.
    pub width: u32,
    /// Texture height.
    pub height: u32,
    /// Texture contents serial.
    pub serial: u64,
    /// Shown picture time in seconds, `None` while the fallback shows.
    pub frame_time: Option<f64>,
    /// The fallback frame shows.
    pub fallback: bool,
    /// Ticks in a row in `Buffering`.
    pub stall_ticks: u32,
    /// Position in seconds.
    pub position: f64,
    /// Duration in seconds.
    pub duration: Option<f64>,
    /// Looping.
    pub looping: bool,
    /// The clock follows the audio output.
    pub audio_clock: bool,
    /// Decode ring counters, when the session decodes into a ring.
    pub ring: Option<RingStats>,
    /// Controls applied so far.
    pub controls_applied: u64,
    /// Controls waiting.
    pub controls_pending: u32,
    /// The last control that failed, as a protocol code.
    pub last_control_error: Option<String>,
    /// Texture uploads so far (pictures and fallback frames).
    pub publishes: u64,
    /// Pictures handed to the GPU convert pass.
    pub gpu_submits: u64,
}

/// The `VideoFrameSource` over the ECS players (clones share it).
#[derive(Resource, Clone, Default)]
pub struct MediaFrameSource {
    inner: Arc<Mutex<BTreeMap<u64, Slot>>>,
}

impl std::fmt::Debug for MediaFrameSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaFrameSource")
            .field("players", &self.players())
            .finish()
    }
}

impl MediaFrameSource {
    fn lock(&self) -> MutexGuard<'_, BTreeMap<u64, Slot>> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Diagnostics of every published player, sorted by entity bits.
    pub fn statuses(&self) -> Vec<FrameSourceStatus> {
        self.lock()
            .iter()
            .map(|(id, s)| {
                let info = s.info.clone().unwrap_or(VideoFrameInfo {
                    width: 1,
                    height: 1,
                    aspect: 1.0,
                    serial: 0,
                    pts_ns: None,
                    fallback: true,
                    playback: VideoPlayback::Closed,
                    position: 0.0,
                    duration: None,
                    looping: false,
                    audio_clock: false,
                });
                FrameSourceStatus {
                    player: *id,
                    state: info.playback.name().to_string(),
                    width: info.width,
                    height: info.height,
                    serial: info.serial,
                    frame_time: info.pts_ns.map(|t| t as f64 / 1e9),
                    fallback: info.fallback,
                    stall_ticks: s.stall,
                    position: info.position,
                    duration: info.duration,
                    looping: info.looping,
                    audio_clock: info.audio_clock,
                    ring: s.ring,
                    controls_applied: s.controls_applied,
                    controls_pending: s.controls.len() as u32,
                    last_control_error: s.last_control_error.clone(),
                    publishes: s.publishes,
                    gpu_submits: s.gpu_submits,
                }
            })
            .collect()
    }

    /// Copies the published picture into the material texture when a `VideoMaterial` is bound
    /// and the picture is newer than the last copy. No copy when nothing is bound: the caller
    /// is `bind_video_materials`.
    fn upload_material(&self, player: u64) {
        let (texture, rgba, publishes) = {
            let map = self.lock();
            let Some(slot) = map.get(&player) else {
                return;
            };
            if slot.texture_written == slot.publishes || slot.publishes == 0 {
                return;
            }
            let (Some(texture), Some(rgba)) = (slot.texture.clone(), slot.rgba.clone()) else {
                return;
            };
            (texture, rgba, slot.publishes)
        };
        if let Err(e) = texture.write(&rgba) {
            tracing::warn!(player, "video texture write refused: {e:?}");
            return;
        }
        let mut map = self.lock();
        if let Some(slot) = map.get_mut(&player)
            && slot.publishes == publishes
        {
            slot.texture_written = publishes;
        }
    }
}

impl VideoFrameSource for MediaFrameSource {
    fn frame(&self, player: u64) -> Option<VideoFrameInfo> {
        self.lock().get(&player).and_then(|s| s.info.clone())
    }

    fn texture(&self, player: u64) -> Option<DynamicTexture> {
        self.lock().get(&player).and_then(|s| s.texture.clone())
    }

    fn rgba(&self, player: u64) -> Option<(u64, u32, u32, Arc<[u8]>)> {
        let map = self.lock();
        let s = map.get(&player)?;
        let info = s.info.as_ref()?;
        Some((info.serial, info.width, info.height, s.rgba.clone()?))
    }

    fn control(&self, player: u64, control: VideoControl) -> Result<(), VideoControlError> {
        let mut map = self.lock();
        let slot = map
            .get_mut(&player)
            .ok_or(VideoControlError::PlayerNotFound(player))?;
        if slot.controls.len() >= VIDEO_CONTROL_QUEUE {
            return Err(VideoControlError::QueueFull(player));
        }
        slot.controls.push_back(control);
        Ok(())
    }

    fn players(&self) -> Vec<u64> {
        self.lock().keys().copied().collect()
    }
}

/// Installs the renderer half: the source in `regia_render::VideoFrames`, the decode ring, and the
/// control, publish and material systems. Add it after `MediaPlugin`. Off unless added.
#[derive(Debug, Default)]
pub struct VideoFramePlugin;

impl Plugin for VideoFramePlugin {
    fn build(&self, app: &mut App) {
        let source = MediaFrameSource::default();
        app.init_resource::<VideoFrameSettings>()
            .insert_resource(source.clone())
            .insert_resource(VideoFrames(Some(Arc::new(source))))
            .add_systems(
                Update,
                apply_video_controls
                    .in_set(VideoSystems::ApplyControls)
                    .before(crate::plugin::tick_players),
            )
            .add_systems(
                PostUpdate,
                (publish_video_frames, bind_video_materials)
                    .chain()
                    .in_set(VideoSystems::Publish)
                    .after(crate::plugin::emit_video_events),
            );
        let ring = app
            .world()
            .get_resource::<VideoFrameSettings>()
            .map(|s| s.ring)
            .unwrap_or_default();
        match app.world_mut().get_resource_mut::<MediaSettings>() {
            Some(mut settings) => {
                if settings.ring_budget.is_none() {
                    settings.ring_budget = Some(ring);
                }
            }
            None => {
                app.insert_resource(MediaSettings {
                    ring_budget: Some(ring),
                    ..MediaSettings::default()
                });
            }
        }
    }
}

fn playback(state: PlayState) -> VideoPlayback {
    match state {
        PlayState::Closed => VideoPlayback::Closed,
        PlayState::Opening => VideoPlayback::Opening,
        PlayState::Ready => VideoPlayback::Ready,
        PlayState::Playing => VideoPlayback::Playing,
        PlayState::Paused => VideoPlayback::Paused,
        PlayState::Seeking => VideoPlayback::Seeking,
        PlayState::Buffering => VideoPlayback::Buffering,
        PlayState::Ended => VideoPlayback::Ended,
        PlayState::Failed => VideoPlayback::Failed,
    }
}

/// The planes of a decoded picture as the renderer's view.
pub fn picture_planes(p: &DecodedPicture) -> YuvPlanes<'_> {
    YuvPlanes {
        width: p.width,
        height: p.height,
        bit_depth: p.bit_depth,
        chroma_shift: p.chroma.shifts(),
        matrix: YuvMatrix::from_h273(p.color.matrix),
        full_range: p.color.full_range,
        planes: [p.planes[0].as_ref(), p.planes[1].as_ref(), p.planes[2].as_ref()],
    }
}

/// Applies one control to a player. `skippable` is false for a fullscreen player marked not
/// skippable. Returns the protocol code of a refusal.
pub fn apply_control(
    player: Option<&mut VideoPlayer>,
    state: &mut VideoPlayerState,
    control: VideoControl,
    skippable: bool,
) -> Result<(), String> {
    let core = state.core_mut();
    let result = match control {
        VideoControl::Play => core.play(),
        VideoControl::Pause => core.pause(),
        VideoControl::Toggle => {
            if core.state() == PlayState::Playing {
                core.pause()
            } else {
                core.play()
            }
        }
        VideoControl::Seek(seconds) => core.seek_time(seconds, SeekMode::Accurate),
        VideoControl::SetLoop(on) => {
            let mode = if on { LoopMode::Loop } else { LoopMode::Off };
            core.set_loop(mode);
            if let Some(p) = player {
                // Keep the component in step without re-applying rate and clock (which would
                // re-anchor the lockstep clock).
                p.loop_mode = mode;
                state.applied = Some((p.rate, mode, p.clock));
            }
            Ok(())
        }
        VideoControl::Skip => {
            if !skippable {
                return Err("video_not_skippable".to_string());
            }
            if core.skip() {
                Ok(())
            } else {
                Err(crate::error::MediaError::bad_state("skip", core.state()))
            }
        }
    };
    result.map_err(|e| format!("video_{}", e.code()))
}

fn apply_video_controls(
    source: Res<MediaFrameSource>,
    mut players: Query<(
        Entity,
        &mut VideoPlayer,
        &mut VideoPlayerState,
        Option<&crate::present::VideoFullscreen>,
    )>,
) {
    let mut queued: Vec<(u64, Vec<VideoControl>)> = {
        let mut map = source.lock();
        map.iter_mut()
            .filter(|(_, s)| !s.controls.is_empty())
            .map(|(id, s)| (*id, s.controls.drain(..).collect()))
            .collect()
    };
    if queued.is_empty() {
        return;
    }
    queued.sort_by_key(|(id, _)| *id);
    for (id, controls) in queued {
        let Some((_, mut player, mut state, full)) =
            players.iter_mut().find(|(e, _, _, _)| e.to_bits() == id)
        else {
            continue;
        };
        let skippable = full.is_none_or(|f| f.skippable);
        let mut applied = 0u64;
        let mut error = None;
        for c in controls {
            match apply_control(Some(player.bypass_change_detection()), &mut state, c, skippable) {
                Ok(()) => applied += 1,
                Err(code) => error = Some(code),
            }
        }
        let mut map = source.lock();
        if let Some(slot) = map.get_mut(&id) {
            slot.controls_applied += applied;
            if error.is_some() {
                slot.last_control_error = error;
            }
        }
    }
}

/// Fixed-point fraction bits. Matches `regia_render::video::yuv_to_rgba8`.
const RGBA_FRAC: u32 = 16;

fn plane_sample(plane: &[u8], sample_bytes: usize, pixel: usize) -> i64 {
    let i = pixel * sample_bytes;
    if sample_bytes == 2 {
        match (plane.get(i), plane.get(i + 1)) {
            (Some(lo), Some(hi)) => i64::from(u16::from_le_bytes([*lo, *hi])),
            _ => 0,
        }
    } else {
        plane.get(i).copied().map_or(0, i64::from)
    }
}

/// Writes `planes` as tightly packed RGBA8 into `out`.
///
/// Same fixed-point map as `yuv_to_rgba8` (three coefficients, then alpha 255). `out` must
/// already be `width * height * 4` bytes; the caller keeps that allocation. Returns false when
/// the planes are short or `out` is the wrong length, and leaves `out` unchanged in that case.
fn fill_yuv_rgba8(planes: &YuvPlanes<'_>, out: &mut [u8]) -> bool {
    let width = planes.width as usize;
    let height = planes.height as usize;
    let need = width * height * 4;
    if !planes.is_complete() || out.len() != need {
        return false;
    }
    let _span = tracing::trace_span!("video_convert_cpu").entered();
    let fixed = convert_rows(planes.matrix, planes.full_range, planes.bit_depth)
        .map(|row| row.map(|c| (c * f64::from(1u32 << RGBA_FRAC)).round() as i64));
    let half = 1i64 << (RGBA_FRAC - 1);
    let (sx, sy) = planes.chroma_shift;
    let (cw, _) = planes.chroma_size();
    let sample_bytes = planes.sample_bytes();
    let cw = cw as usize;
    let sx = sx as usize;
    let sy = sy as usize;
    let y_plane = planes.planes[0];
    let u_plane = planes.planes[1];
    let v_plane = planes.planes[2];
    for y in 0..height {
        let chroma_row = (y >> sy) * cw;
        let row_at = y * width * 4;
        for x in 0..width {
            let yy = plane_sample(y_plane, sample_bytes, y * width + x);
            let chroma = chroma_row + (x >> sx);
            let u = plane_sample(u_plane, sample_bytes, chroma);
            let v = plane_sample(v_plane, sample_bytes, chroma);
            let px = row_at + x * 4;
            for (i, coeff) in fixed.iter().enumerate() {
                let value = (coeff[0] * yy + coeff[1] * u + coeff[2] * v + coeff[3] + half) >> RGBA_FRAC;
                out[px + i] = value.clamp(0, 255) as u8;
            }
            out[px + 3] = 255;
        }
    }
    true
}

fn fill_clear(color: [f32; 4], out: &mut [u8]) {
    let px = color.map(|c| (c.clamp(0.0, 1.0) * 255.0).round() as u8);
    let (chunks, _) = out.as_chunks_mut::<4>();
    for chunk in chunks {
        *chunk = px;
    }
}

/// The GPU picture. Clones the decoder's plane handles; it does not copy the samples.
fn owned_yuv(p: &DecodedPicture) -> OwnedYuv {
    let view = picture_planes(p);
    OwnedYuv {
        width: view.width,
        height: view.height,
        bit_depth: view.bit_depth,
        chroma_shift: view.chroma_shift,
        matrix: view.matrix,
        full_range: view.full_range,
        planes: [
            Arc::clone(&p.planes[0]),
            Arc::clone(&p.planes[1]),
            Arc::clone(&p.planes[2]),
        ],
    }
}

/// Fills `need` bytes. Rewrites `taken` in place when this task is its only owner; otherwise
/// fills `scratch` and copies once into a new shared buffer (`Arc::from` of a slice cannot
/// adopt the `Vec`).
fn write_rgba(
    taken: &mut Option<Arc<[u8]>>,
    scratch: &mut Vec<u8>,
    need: usize,
    fill: impl FnOnce(&mut [u8]) -> bool,
) -> Option<Arc<[u8]>> {
    if let Some(mut arc) = taken.take() {
        if arc.len() == need && Arc::strong_count(&arc) == 1 {
            if fill(Arc::make_mut(&mut arc)) {
                return Some(arc);
            }
            *taken = Some(arc);
            return None;
        }
        *taken = Some(arc);
    }
    if scratch.len() != need {
        scratch.resize(need, 0);
    }
    if !fill(scratch) {
        return None;
    }
    Some(Arc::from(scratch.as_slice()))
}

/// Display aspect from the track's `DisplayWidth` / `DisplayHeight` when both are set.
/// The texture stays the coded picture size; letterboxing uses this ratio (specs/50 § 2.7).
fn track_display_aspect(info: Option<&crate::container::MediaInfo>) -> Option<f32> {
    let video = info?.video_track()?.video.as_ref()?;
    if video.display_width == 0 || video.display_height == 0 {
        return None;
    }
    Some(video.display_width as f32 / video.display_height as f32)
}

fn publish_video_frames(
    source: Res<MediaFrameSource>,
    settings: Res<VideoFrameSettings>,
    gpu: Option<Res<GpuVideoJobs>>,
    fullscreen: Option<Res<FullscreenVideos>>,
    above_ui: Option<Res<crate::overlay::OverlayVideos>>,
    mut players: Query<
        (
            Entity,
            &mut VideoPlayerState,
            Option<&crate::present::VideoFullscreen>,
        ),
        With<VideoPlayer>,
    >,
) {
    let _span = tracing::trace_span!("video_publish").entered();
    // Drop the previous frame's queue clones before converting, so a same-size picture can
    // rewrite its published buffer. The queues are filled again before this system returns.
    if let Some(queue) = fullscreen.as_deref() {
        queue.0.set(Vec::new());
    }
    if let Some(over) = above_ui.as_deref() {
        over.0.set(Vec::new());
    }
    let mut seen = Vec::new();
    let mut screens = Vec::new();
    for (entity, mut state, full) in &mut players {
        let id = entity.to_bits();
        seen.push(id);
        let clear = state.clear_color();
        let core = state.core_mut();
        let play = core.state();
        let frames = core.stats().frames_presented;
        let presented = core.presented().map(|p| (frames, p.pts_ns));
        let duration = core.duration();
        let position = core.position();
        let looping = core.loop_mode() != LoopMode::Off;
        let audio_clock = core.clock() == ClockMode::Audio;
        let track_aspect = track_display_aspect(core.info());
        let ring = core.ring_stats();
        let (mut width, mut height, mut aspect, fallback, changed, mut taken, mut scratch) = {
            let mut map = source.lock();
            let slot = map.entry(id).or_default();
            slot.stall = if play == PlayState::Buffering {
                slot.stall.saturating_add(1)
            } else {
                0
            };
            slot.ring = ring;
            let fallback = presented.is_none()
                || matches!(play, PlayState::Closed | PlayState::Failed)
                || slot.stall >= settings.stall_ticks.max(1);
            let changed = if fallback {
                !slot.fallback_shown
            } else {
                slot.fallback_shown || slot.presented_seen != presented
            };
            let (width, height, aspect) = slot
                .info
                .as_ref()
                .map_or((1, 1, 1.0), |i| (i.width, i.height, i.aspect));
            let (taken, scratch) = if changed {
                (slot.rgba.take(), std::mem::take(&mut slot.scratch))
            } else {
                (None, Vec::new())
            };
            (width, height, aspect, fallback, changed, taken, scratch)
        };
        // The clear frame is published before the first picture. Layout still uses the track's
        // display aspect (specs/50 § 2.7, audfix2-media-g). The texture stays the clear color
        // until a picture arrives.
        if let Some(ratio) = track_aspect {
            aspect = ratio;
        }
        let mut published = None;
        let mut gpu_submitted = false;
        if changed {
            if fallback {
                let need = width as usize * height as usize * 4;
                published = write_rgba(&mut taken, &mut scratch, need, |buf| {
                    fill_clear(clear, buf);
                    true
                });
            } else if let Some(p) = core.presented() {
                let converted = {
                    let planes = picture_planes(p);
                    if planes.is_complete() {
                        let w = p.width.max(1);
                        let h = p.height.max(1);
                        let need = w as usize * h as usize * 4;
                        write_rgba(&mut taken, &mut scratch, need, |buf| fill_yuv_rgba8(&planes, buf))
                            .map(|rgba| (rgba, w, h))
                    } else {
                        None
                    }
                };
                if let Some((rgba, w, h)) = converted {
                    width = w;
                    height = h;
                    aspect = track_aspect.unwrap_or(width as f32 / height as f32);
                    published = Some(rgba);
                    if settings.gpu_convert
                        && let Some(jobs) = gpu.as_deref()
                    {
                        jobs.0.submit(id, owned_yuv(p));
                        gpu_submitted = true;
                    }
                }
            }
        }
        let screen = {
            let mut map = source.lock();
            let slot = map.entry(id).or_default();
            if changed {
                slot.scratch = scratch;
                if published.is_some() {
                    if slot.texture.as_ref().is_none_or(|t| t.size() != (width, height)) {
                        slot.texture = Some(DynamicTexture::new(width, height, true));
                        slot.texture_written = 0;
                    }
                    slot.rgba = published;
                    slot.publishes += 1;
                    if gpu_submitted {
                        slot.gpu_submits += 1;
                    }
                    slot.fallback_shown = fallback;
                    slot.presented_seen = if fallback { None } else { presented };
                } else {
                    slot.rgba = taken;
                }
            }
            let serial = slot.publishes;
            let shown_pts = if slot.fallback_shown {
                None
            } else {
                slot.presented_seen.map(|(_, t)| t)
            };
            slot.info = Some(VideoFrameInfo {
                width,
                height,
                aspect,
                serial,
                pts_ns: shown_pts,
                fallback: slot.fallback_shown,
                playback: playback(play),
                position,
                duration,
                looping,
                audio_clock,
            });
            // A closed player (skipped, finished and closed) leaves the screen.
            if let (Some(full), Some(rgba)) = (full, slot.rgba.clone())
                && play != PlayState::Closed
            {
                Some(regia_render::FullscreenVideo {
                    player: id,
                    layer: full.layer,
                    fit: match full.fit {
                        crate::present::Fit::Letterbox => regia_render::FullscreenFit::Letterbox,
                        crate::present::Fit::Fill => regia_render::FullscreenFit::Fill,
                        crate::present::Fit::Stretch => regia_render::FullscreenFit::Stretch,
                    },
                    aspect,
                    serial,
                    width,
                    height,
                    rgba,
                })
            } else {
                None
            }
        };
        if let Some(screen) = screen {
            screens.push(screen);
        }
    }
    // With the above-UI layer installed, `layer >= 0` players draw over the UI (ADR-2410); the
    // frame-graph pass keeps the ones under it.
    match above_ui.as_deref() {
        Some(over) => {
            let (top, under): (Vec<_>, Vec<_>) = screens.into_iter().partition(|s| s.layer >= 0);
            over.0.set(top);
            if let Some(queue) = fullscreen.as_deref() {
                queue.0.set(under);
            }
        }
        None => {
            if let Some(queue) = fullscreen.as_deref() {
                queue.0.set(screens);
            }
        }
    }
    let gone: Vec<u64> = {
        let map = source.lock();
        map.keys().filter(|k| !seen.contains(k)).copied().collect()
    };
    if !gone.is_empty() {
        let mut map = source.lock();
        for id in gone {
            map.remove(&id);
            if let Some(jobs) = gpu.as_deref() {
                jobs.0.remove(id);
            }
        }
    }
}

/// Bound texture per material entity (identity, generation).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoMaterialBound {
    /// Texture identity.
    pub identity: usize,
    /// Texture generation.
    pub generation: u64,
}

/// Material binding errors by entity bits (`material_missing_video_slot`), for `media.state`.
#[derive(Resource, Debug, Default, Clone)]
pub struct VideoMaterialErrors(pub BTreeMap<u64, String>);

fn bind_video_materials(
    source: Res<MediaFrameSource>,
    mut commands: Commands,
    mut bindings: Query<(
        Entity,
        &VideoMaterial,
        Option<&mut MeshRenderer>,
        Option<&VideoMaterialBound>,
    )>,
) {
    let mut errors = BTreeMap::new();
    for (entity, binding, mesh, bound) in &mut bindings {
        source.upload_material(binding.player.to_bits());
        let Some(texture) = source.texture(binding.player.to_bits()) else {
            continue;
        };
        let Some(mut mesh) = mesh else {
            errors.insert(entity.to_bits(), "video_material_missing_video_slot".to_string());
            continue;
        };
        let now = VideoMaterialBound {
            identity: texture.identity(),
            generation: texture.generation(),
        };
        if bound == Some(&now) && regia_render::video::binds_video(&mesh.material, &texture, binding.slot) {
            continue;
        }
        mesh.material = Arc::new(video_material(&mesh.material, &texture, binding.slot));
        commands.entity(entity).insert(now);
    }
    commands.insert_resource(VideoMaterialErrors(errors));
}

/// Runs the renderer half's publish and material-bind systems once on `world` (what
/// [`VideoFramePlugin`] runs in `PostUpdate`), for a host that drives players in a world with no
/// schedule: the editor's sequence preview keeps its sessions in a world of its own (ADR-8591).
/// Inserts a [`MediaFrameSource`] and [`VideoFrameSettings`] when `world` has none.
///
/// # Errors
/// The system run's error, as text (a world that cannot run the systems).
pub fn publish_frames_once(world: &mut World) -> Result<(), String> {
    use regia_ecs::ecs::system::RunSystemOnce;

    if !world.contains_resource::<MediaFrameSource>() {
        world.insert_resource(MediaFrameSource::default());
    }
    if !world.contains_resource::<VideoFrameSettings>() {
        world.insert_resource(VideoFrameSettings::default());
    }
    world
        .run_system_once(publish_video_frames)
        .map_err(|e| e.to_string())?;
    world
        .run_system_once(bind_video_materials)
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod convert_tests {
    use super::{fill_yuv_rgba8, plane_sample};
    use regia_render::YuvMatrix;
    use regia_render::video::{YuvPlanes, yuv_to_rgba8};

    fn assert_matches(planes: &YuvPlanes<'_>) {
        let mut push = Vec::new();
        assert!(yuv_to_rgba8(planes, &mut push), "push path");
        let mut row = vec![0u8; push.len()];
        let ptr = row.as_ptr();
        assert!(fill_yuv_rgba8(planes, &mut row), "row path");
        assert_eq!(row, push, "row fill matches yuv_to_rgba8");
        assert!(fill_yuv_rgba8(planes, &mut row), "second fill");
        assert_eq!(row.as_ptr(), ptr, "the second fill keeps the buffer");
        assert_eq!(row, push);
    }

    #[test]
    fn row_fill_matches_yuv_to_rgba8_for_8bit_and_10bit() {
        let width = 8u32;
        let height = 4u32;
        let y: Vec<u8> = (0..width * height).map(|i| (i * 3) as u8).collect();
        let (cw, ch) = (4u32, 2u32);
        let u: Vec<u8> = (0..cw * ch).map(|i| 64 + i as u8).collect();
        let v: Vec<u8> = (0..cw * ch).map(|i| 200u8.wrapping_sub(i as u8)).collect();
        assert_matches(&YuvPlanes {
            width,
            height,
            bit_depth: 8,
            chroma_shift: (1, 1),
            matrix: YuvMatrix::Bt709,
            full_range: false,
            planes: [&y, &u, &v],
        });

        let mut y10 = Vec::new();
        let mut u10 = Vec::new();
        let mut v10 = Vec::new();
        for i in 0..width * height {
            let i = u16::try_from(i).unwrap_or(0);
            y10.extend_from_slice(&(16 + i.saturating_mul(7)).to_le_bytes());
        }
        for i in 0..cw * ch {
            let i = u16::try_from(i).unwrap_or(0);
            u10.extend_from_slice(&(128 + i.saturating_mul(3)).to_le_bytes());
            v10.extend_from_slice(&(900u16.saturating_sub(i.saturating_mul(5))).to_le_bytes());
        }
        assert_eq!(plane_sample(&y10, 2, 0), 16);
        assert_matches(&YuvPlanes {
            width,
            height,
            bit_depth: 10,
            chroma_shift: (1, 1),
            matrix: YuvMatrix::Bt2020Ncl,
            full_range: true,
            planes: [&y10, &u10, &v10],
        });

        let short = YuvPlanes {
            width,
            height,
            bit_depth: 8,
            chroma_shift: (1, 1),
            matrix: YuvMatrix::Bt601,
            full_range: false,
            planes: [&y[..4], &u, &v],
        };
        let mut untouched = vec![7u8; 4];
        assert!(!fill_yuv_rgba8(&short, &mut untouched));
        assert_eq!(untouched, vec![7, 7, 7, 7], "a short picture does not write");

        // Informational only: no elapsed assert. Debug numbers go in the lane report.
        let big_w = 320u32;
        let big_h = 180u32;
        let big_y: Vec<u8> = (0..big_w * big_h).map(|i| (i % 251) as u8).collect();
        let (bcw, bch) = (160u32, 90u32);
        let big_u = vec![128u8; (bcw * bch) as usize];
        let big_v = vec![128u8; (bcw * bch) as usize];
        let big = YuvPlanes {
            width: big_w,
            height: big_h,
            bit_depth: 8,
            chroma_shift: (1, 1),
            matrix: YuvMatrix::Bt709,
            full_range: false,
            planes: [&big_y, &big_u, &big_v],
        };
        let mut push = Vec::new();
        let mut row = vec![0u8; big_w as usize * big_h as usize * 4];
        let rounds = 8u32;
        let push_start = std::time::Instant::now();
        for _ in 0..rounds {
            assert!(yuv_to_rgba8(&big, &mut push));
        }
        let push_ns = push_start.elapsed().as_nanos();
        let row_start = std::time::Instant::now();
        for _ in 0..rounds {
            assert!(fill_yuv_rgba8(&big, &mut row));
        }
        let row_ns = row_start.elapsed().as_nanos();
        assert_eq!(row, push, "320x180 row fill matches the push loop");
        eprintln!(
            "cpu convert {big_w}x{big_h} x{rounds}: yuv_to_rgba8 {push_ns} ns, fill_yuv_rgba8 {row_ns} ns"
        );
    }
}

#[cfg(test)]
mod audfix2_tests {
    use super::*;
    use crate::codec::{ChromaFormat, CodecVerdict, ColorInfo};
    use crate::container::{ContainerKind, MediaInfo, TrackInfo, TrackKind, VideoParams};
    use crate::session::{FeedPoll, FeedStats, FrameFeed};
    use regia_app::prelude::MinimalPlugins;

    struct PendingFeed;

    impl FrameFeed for PendingFeed {
        fn poll(&mut self, _block: bool) -> FeedPoll {
            FeedPoll::Pending
        }

        fn seek(&mut self, _to_ns: u64) {}

        fn stats(&self) -> FeedStats {
            FeedStats::default()
        }
    }

    fn anamorphic_info() -> MediaInfo {
        MediaInfo {
            container: ContainerKind::Webm,
            duration_ns: Some(1_000_000_000),
            timestamp_scale: 1_000_000,
            tracks: vec![TrackInfo {
                number: 1,
                index: 0,
                kind: TrackKind::Video,
                codec_id: "V_AV1".into(),
                codec_private: Vec::new(),
                language: "eng".into(),
                default: true,
                default_duration_ns: Some(33_333_333),
                frame_rate: Some(30.0),
                video: Some(VideoParams {
                    width: 320,
                    height: 240,
                    display_width: 320,
                    display_height: 180,
                    bit_depth: 8,
                    chroma: ChromaFormat::default(),
                    color: ColorInfo::default(),
                    fourcc: None,
                }),
                audio: None,
                verdict: CodecVerdict::Supported,
            }],
            cue_count: 0,
            markers: Vec::new(),
        }
    }

    /// Specs/50 § 2.7: layout uses the clip's display aspect, which the track knows at open.
    /// A game-clock player with no picture yet still publishes that ratio on the clear frame.
    #[test]
    fn opening_fallback_uses_the_track_display_aspect() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, crate::plugin::MediaPlugin, VideoFramePlugin));
        let opening = app.world_mut().spawn(VideoPlayer::default()).id();
        {
            let mut state = app
                .world_mut()
                .get_mut::<VideoPlayerState>(opening)
                .expect("state");
            state.core_mut().open(
                Box::new(PendingFeed),
                anamorphic_info(),
                ClockMode::Game,
                LoopMode::Off,
                false,
                0.0,
            );
        }
        let closed = app.world_mut().spawn(VideoPlayer::default()).id();
        app.update();
        let info = app
            .world()
            .resource::<MediaFrameSource>()
            .frame(opening.to_bits())
            .expect("opening frame");
        assert!(info.fallback, "no picture yet, so the frame is the clear color");
        assert_eq!((info.width, info.height), (1, 1), "the clear texture stays 1x1");
        let want = 320.0_f32 / 180.0;
        assert!(
            (info.aspect - want).abs() < 1e-4,
            "opening fallback aspect {} is not the track display ratio {want}",
            info.aspect
        );
        let closed_info = app
            .world()
            .resource::<MediaFrameSource>()
            .frame(closed.to_bits())
            .expect("closed frame");
        assert!(
            (closed_info.aspect - 1.0).abs() < 1e-6,
            "a closed player with no track stays square"
        );
    }
}
