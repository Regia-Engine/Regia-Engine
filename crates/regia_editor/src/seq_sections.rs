// SPDX-License-Identifier: MPL-2.0
//! The section strip of the Sequence Curves panel: the dope sheet of a sequence player's clip,
//! audio and sub-sequence tracks (`.seq.ron` v2, ADR-2135, book `agents/seq-curve-panel.md`).
//!
//! Picking a clip, audio or sub-sequence track in the panel's **Track** list shows its sections
//! as bars on a time strip, overlapping ones on separate rows (`section_spans`). Drag a bar to move
//! it, drag its left or right edge to trim it (a trim keeps the content where it was on the
//! clock), drag empty space to scrub, right-click a bar to duplicate or delete it, and edit the
//! selected section's numbers in the row under the strip. Drags snap to the sequence's frames
//! unless **Snap** is off. Every edit goes through `regia_framework::sequencer::sections` and the
//! panel's own edit path (a copy of the sequence, validated, swapped into the player, one undo
//! entry per gesture). The agent methods `editor.seq_sections_state` and `editor.seq_section_edit`
//! run the same code.
//!
//! `.seq.ron` v3 (ADR-2405) adds a sound section's time scale and 3D placement and a sub-sequence
//! section's repeat to the number row, and a **Rename binding** row: it renames one binding name
//! everywhere the sequence refers to it (tracks, spatial sounds, sub-sequence binding tables) and
//! moves the player's binding table entry, as one undo step (`editor.seq_section_edit` op
//! `rename_binding`).
//!
//! `.seq.ron` v4 (ADR-2862) adds video sections: each bar shows a still of the picture the section
//! starts on ([`SeqVideoThumbs`], decoded off the frame), and the number row edits the section's
//! volume, loop switch and target (`editor.seq_section_edit` ops `gain`, `video_loop`,
//! `video_target`). `editor.seq_sections_state` reports the stills.

use std::collections::{BTreeMap, VecDeque};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::sync::{Mutex, PoisonError};

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use regia_framework::sequencer::edit::EditError;
use regia_framework::sequencer::rename::binding_names;
use regia_framework::sequencer::sections::{
    Section, SectionEdit, SectionSpan, apply_section_edit, section_spans, snap_to_frame,
};
use regia_framework::sequencer::video::{VideoThumbnail, video_thumbnail};
use regia_framework::sequencer::{
    AudioSection, AudioSpatial, Channel, ClipSection, Sequence, SequencePlayer, SoundSource, Spawnable,
    SubRepeat, SubSection, TimeScaleMode, VideoFit, VideoScreenSlot, VideoSection, VideoTarget,
};
use regia_media::plugin::MediaFiles;
use serde_json::{Value, json};

use crate::anim::view2d::View2d;
use crate::command::{EditorCommand, UndoStack};
use crate::seq_curve_panel::{
    SeqCurvePanel, edit_track, number, player_of, players, project_root, rename_player_binding, rpc, select,
};
use crate::viewport_agent::Handler;

/// Height of one strip row, points.
pub const ROW_HEIGHT: f32 = 24.0;
/// Height of the time ruler above the rows, points.
pub const RULER_HEIGHT: f32 = 16.0;
/// How far from a bar's edge (points) a press grabs the edge instead of the bar.
pub const EDGE_GRAB: f32 = 5.0;

/// The strip's view and snapping; one resource shared by the egui strip and its tests. The
/// selected section is the panel's `SeqCurvePanel::selected`.
#[derive(Resource, Debug)]
pub struct SeqSectionStrip {
    /// Pan and zoom (only the time axis is used).
    pub view: View2d,
    /// Drags snap to the sequence's frames.
    pub snap: bool,
    /// The player and track the view was last fitted to; another one refits.
    pub fitted: Option<(Entity, usize)>,
    drag: Option<StripDrag>,
}

impl Default for SeqSectionStrip {
    fn default() -> Self {
        Self {
            view: View2d::default(),
            snap: true,
            fitted: None,
            drag: None,
        }
    }
}

/// The widest still a bar keeps, pixels.
pub const THUMB_WIDTH: u32 = 128;
/// Most stills the strip keeps before it starts over.
const MAX_THUMBS: usize = 256;

/// One still of a video section.
pub enum SeqThumb {
    /// Waiting for the decoder.
    Pending,
    /// Decoded (at most [`THUMB_WIDTH`] wide).
    Ready {
        /// The still.
        still: VideoThumbnail,
        /// Its egui texture, once a strip has drawn it.
        texture: Option<egui::TextureHandle>,
    },
    /// It did not decode: the media error code.
    Failed(String),
}

/// A still's source path and position in milliseconds.
type ThumbKey = (String, u64);
/// The running decode's answer: the still, or the media error code.
type ThumbDecode = Mutex<Receiver<Result<VideoThumbnail, String>>>;

/// The stills of video sections, by source path and start position in milliseconds. One decode runs
/// at a time, on its own thread, so a strip never waits for a keyframe: a bar has no still until
/// [`Self::pump`] has one.
#[derive(Resource, Default)]
pub struct SeqVideoThumbs {
    stills: BTreeMap<ThumbKey, SeqThumb>,
    queued: VecDeque<ThumbKey>,
    running: Option<(ThumbKey, ThumbDecode)>,
}

/// `still` at most [`THUMB_WIDTH`] wide (nearest pixel; a thumbnail needs no better).
fn shrink_still(still: VideoThumbnail) -> VideoThumbnail {
    if still.width <= THUMB_WIDTH || still.width == 0 {
        return still;
    }
    let (w, h) = (still.width as usize, still.height as usize);
    let out_w = THUMB_WIDTH as usize;
    let out_h = (h * out_w / w).max(1);
    let mut rgba = Vec::with_capacity(out_w * out_h * 4);
    for y in 0..out_h {
        let row = y * h / out_h;
        for x in 0..out_w {
            let at = (row * w + x * w / out_w) * 4;
            match still.rgba.get(at..at + 4) {
                Some(px) => rgba.extend_from_slice(px),
                None => rgba.extend_from_slice(&[0, 0, 0, 255]),
            }
        }
    }
    VideoThumbnail {
        width: THUMB_WIDTH,
        height: out_h as u32,
        rgba,
        frame: still.frame,
    }
}

impl SeqVideoThumbs {
    /// The still of `video` at `position` source seconds, asking for it the first time.
    pub fn request(&mut self, video: &str, position: f32) -> &SeqThumb {
        let key = (
            video.to_string(),
            (f64::from(position.max(0.0)) * 1000.0).round() as u64,
        );
        // The bars of one strip fit by far: this only drops what past edits left.
        if self.stills.len() >= MAX_THUMBS && !self.stills.contains_key(&key) {
            self.stills.clear();
            self.queued.clear();
        }
        self.stills.entry(key.clone()).or_insert_with(|| {
            self.queued.push_back(key);
            SeqThumb::Pending
        })
    }

    /// Takes a finished decode and starts the next. Without `files` nothing new starts.
    pub fn pump(&mut self, files: Option<&MediaFiles>) {
        if let Some((key, done)) = self.running.take() {
            let polled = done.lock().unwrap_or_else(PoisonError::into_inner).try_recv();
            let still = match polled {
                Ok(Ok(still)) => SeqThumb::Ready { still, texture: None },
                Ok(Err(code)) => SeqThumb::Failed(code),
                Err(TryRecvError::Disconnected) => SeqThumb::Failed("decode_failed".to_string()),
                Err(TryRecvError::Empty) => {
                    self.running = Some((key, done));
                    return;
                }
            };
            // A still the cap dropped while it decoded stays dropped (review rc1-rev-editor-k
            // finding 13): only a key still waiting takes the result.
            if let Some(slot) = self.stills.get_mut(&key)
                && matches!(slot, SeqThumb::Pending)
            {
                *slot = still;
            }
        }
        let Some(files) = files else {
            return;
        };
        // Skip keys the cap dropped, or that the decode above already answered.
        let next = std::iter::from_fn(|| self.queued.pop_front())
            .find(|key| matches!(self.stills.get(key), Some(SeqThumb::Pending)));
        let Some(key) = next else {
            return;
        };
        let (send, done) = channel();
        let files = files.clone();
        let (video, position_ms) = key.clone();
        let spawned = std::thread::Builder::new()
            .name("regia-seq-thumbnail".into())
            .spawn(move || {
                let still = video_thumbnail(&files, &video, position_ms as f64 / 1000.0)
                    .map(shrink_still)
                    .map_err(|e| e.code().to_string());
                // The strip may be gone: nobody waits for the still then.
                let _ = send.send(still);
            });
        match spawned {
            Ok(_) => self.running = Some((key, Mutex::new(done))),
            Err(e) => {
                tracing::warn!(target: "regia_editor", "no thread for a video thumbnail: {e}");
                self.stills
                    .insert(key, SeqThumb::Failed("decode_failed".to_string()));
            }
        }
    }

    /// `true` while a decode runs or waits.
    #[must_use]
    pub fn busy(&self) -> bool {
        self.running.is_some() || !self.queued.is_empty()
    }
}

/// What thumbnails decode from: the project's media files, or the project root itself.
fn thumb_files(world: &World) -> Option<MediaFiles> {
    world
        .get_resource::<MediaFiles>()
        .cloned()
        .or_else(|| project_root(world).map(MediaFiles::with_root))
}

/// Asks for the still of every section of a video track and moves the decoder on.
fn pump_thumbs(world: &mut World, channel: &Channel) {
    let files = thumb_files(world);
    let mut thumbs = world.get_resource_or_insert_with(SeqVideoThumbs::default);
    if let Channel::Video(list) = channel {
        for section in list {
            thumbs.request(&section.video, section.offset);
        }
    }
    thumbs.pump(files.as_ref());
}

/// The egui texture of each section's still, by section index (uploaded on first use).
fn thumb_textures(ctx: &egui::Context, world: &mut World, channel: &Channel) -> Vec<Option<egui::TextureId>> {
    let Channel::Video(list) = channel else {
        return Vec::new();
    };
    pump_thumbs(world, channel);
    let mut thumbs = world.get_resource_or_insert_with(SeqVideoThumbs::default);
    let thumbs = &mut *thumbs;
    let textures = list
        .iter()
        .map(|section| {
            let key = (
                section.video.clone(),
                (f64::from(section.offset.max(0.0)) * 1000.0).round() as u64,
            );
            let SeqThumb::Ready { still, texture } = thumbs.stills.get_mut(&key)? else {
                return None;
            };
            let handle = texture.get_or_insert_with(|| {
                let size = [still.width as usize, still.height as usize];
                ctx.load_texture(
                    format!("seq_thumb:{}:{}", key.0, key.1),
                    egui::ColorImage::from_rgba_unmultiplied(size, &still.rgba),
                    egui::TextureOptions::LINEAR,
                )
            });
            Some(handle.id())
        })
        .collect();
    if thumbs.busy() {
        ctx.request_repaint();
    }
    textures
}

/// The running drag: what was grabbed, the time under the press, the section's span then and the
/// track as it was. Every frame of the drag edits that snapshot, not the previous frame's result,
/// so a drag back undoes what the drag did (ramps fitted to a shorter section come back, trims add
/// no rounding up).
#[derive(Clone, Debug, PartialEq)]
struct StripDrag {
    pick: Option<SectionPick>,
    press_time: f32,
    start: f32,
    end: f32,
    base: Option<Channel>,
}

/// What a pointer position hits on the strip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SectionPick {
    /// The body of a section's bar.
    Body(usize),
    /// A section's start edge.
    Start(usize),
    /// A section's end edge.
    End(usize),
}

/// The screen rect of a section's bar.
#[must_use]
pub fn section_rect(view: &View2d, span: &SectionSpan) -> egui::Rect {
    let x0 = view.to_screen(egui::vec2(span.start, 0.0)).x;
    let x1 = view.to_screen(egui::vec2(span.end, 0.0)).x;
    let y0 = view.rect.top() + RULER_HEIGHT + span.row as f32 * ROW_HEIGHT;
    egui::Rect::from_min_max(
        egui::pos2(x0, y0 + 2.0),
        egui::pos2(x1.max(x0 + 1.0), y0 + ROW_HEIGHT - 2.0),
    )
}

/// What `pos` hits: an edge within [`EDGE_GRAB`] (a third of a short bar) of it, else the body.
#[must_use]
pub fn pick_section(spans: &[SectionSpan], view: &View2d, pos: egui::Pos2) -> Option<SectionPick> {
    for span in spans {
        let r = section_rect(view, span);
        let grab = EDGE_GRAB.min(r.width() / 3.0);
        if !r.y_range().contains(pos.y) || pos.x < r.left() - grab || pos.x > r.right() + grab {
            continue;
        }
        if (pos.x - r.left()).abs() <= grab {
            return Some(SectionPick::Start(span.index));
        }
        if (pos.x - r.right()).abs() <= grab {
            return Some(SectionPick::End(span.index));
        }
        if r.contains(pos) {
            return Some(SectionPick::Body(span.index));
        }
    }
    None
}

/// The edit a drag of `pick` to the time `pointer` means, the drag having pressed at
/// `press_time` on a section that spanned `start..end` then: the grabbed body or edge moves by the
/// pointer's travel, so a press beside an edge does not make it jump (times snapped to `fps` when
/// `snap`, never below zero).
#[must_use]
pub fn drag_edit(
    pick: SectionPick,
    press_time: f32,
    start: f32,
    end: f32,
    pointer: f32,
    fps: f32,
    snap: bool,
) -> SectionEdit {
    let snapped = |t: f32| {
        let t = t.max(0.0);
        if snap { snap_to_frame(t, fps) } else { t }
    };
    match pick {
        SectionPick::Body(index) => SectionEdit::Move {
            index,
            start: snapped(start + (pointer - press_time)),
        },
        SectionPick::Start(index) => SectionEdit::TrimStart {
            index,
            start: snapped(start + (pointer - press_time)),
        },
        SectionPick::End(index) => SectionEdit::TrimEnd {
            index,
            end: snapped(end + (pointer - press_time)),
        },
    }
}

/// Applies `edit` to the panel's section track of the panel's player (validated, swapped in, an
/// undo entry). Codes: the panel's `seq_curve_no_player`, `seq_curve_bad_track` and
/// `seq_curve_invalid`, and the `seq_section_*` codes of `apply_section_edit`.
pub fn edit_section(
    world: &mut World,
    panel: &mut SeqCurvePanel,
    edit: SectionEdit,
) -> Result<(), EditError> {
    edit_track(world, panel, |channel| apply_section_edit(channel, edit))
}

/// What the strip did this frame.
enum StripAction {
    Edit(SectionEdit),
    /// A drag frame: `edit` applies to `base`, the track as the drag found it.
    Drag {
        edit: SectionEdit,
        base: Channel,
    },
    Scrub(f32),
    /// Rename a binding everywhere the sequence refers to it (one undo step).
    RenameBinding {
        from: String,
        to: String,
    },
    /// Make a binding hand-bound (`None`) or spawned from a prefab (ADR-2948; one undo step).
    BindingKind {
        binding: String,
        spawn: Option<Spawnable>,
    },
}

fn time_bounds(spans: &[SectionSpan]) -> [f32; 4] {
    let start = spans.iter().map(|s| s.start).fold(f32::MAX, f32::min);
    let end = spans.iter().map(|s| s.end).fold(f32::MIN, f32::max);
    if spans.is_empty() {
        return [0.0, 0.0, 1.0, 1.0];
    }
    [start.min(0.0), 0.0, end.max(start + 1.0), 1.0]
}

/// Draws the strip of `channel` (the panel's track of `entity`) and applies what it does.
pub(crate) fn section_strip(
    ui: &mut egui::Ui,
    panel: &mut SeqCurvePanel,
    world: &mut World,
    entity: Entity,
    (track, channel): (&str, &Channel),
    playhead: f32,
    fps: f32,
) {
    let _span = tracing::info_span!(target: "regia_editor", "seq_section_strip").entered();
    let Some(spans) = section_spans(channel) else {
        return;
    };
    let mut strip = world.remove_resource::<SeqSectionStrip>().unwrap_or_default();
    if panel.selected.is_some_and(|i| i >= spans.len()) {
        panel.selected = None;
    }
    ui.horizontal(|ui| {
        ui.checkbox(&mut strip.snap, "Snap to frames");
        ui.label(format!("{} {} sections", spans.len(), channel.kind()));
    });
    let rows = spans.iter().map(|s| s.row + 1).max().unwrap_or(1);
    // A drag whose release the strip never saw (it was not drawn then) ends here.
    if strip.drag.is_some() && !ui.input(|i| i.pointer.primary_down()) {
        strip.drag = None;
        if panel.gesture.take().is_some() {
            world
                .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
                .seal();
        }
    }
    let was_dragging = panel.gesture.is_some();
    let thumbs = thumb_textures(ui.ctx(), world, channel);
    let frame = StripFrame {
        entity,
        channel,
        spans: &spans,
        rows,
        playhead,
        fps,
        thumbs: &thumbs,
    };
    let action = strip_widget(ui, &mut strip, panel, &frame);
    if was_dragging && panel.gesture.is_none() {
        world
            .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
            .seal();
    }
    let action = action.or(section_list(ui, panel, track, &spans, fps));
    // The number row draws every frame (it would flicker away during a drag otherwise).
    let typed = properties(ui, panel, world, channel);
    let subs = sub_binding_row(ui, panel, channel);
    let renamed = binding_row(ui, world, entity);
    let kind = spawn_row(ui, world, entity);
    let action = action.or(typed).or(subs).or(renamed).or(kind);
    if let Some(action) = action {
        panel.error = None;
        match action {
            StripAction::Edit(edit) => {
                if let Err(e) = edit_section(world, panel, edit) {
                    panel.error = Some(e.message);
                }
            }
            StripAction::Drag { edit, base } => {
                let applied = edit_track(world, panel, |channel| {
                    *channel = base;
                    apply_section_edit(channel, edit)
                });
                if let Err(e) = applied {
                    panel.error = Some(e.message);
                }
            }
            StripAction::Scrub(time) => {
                if let Some(mut player) = world.get_mut::<SequencePlayer>(entity) {
                    player.seek(f64::from(time));
                }
            }
            StripAction::RenameBinding { from, to } => {
                if let Err(e) = rename_player_binding(world, panel, &from, &to) {
                    panel.error = Some(e.message);
                }
            }
            StripAction::BindingKind { binding, spawn } => {
                if let Err(e) = set_binding_kind(world, entity, &binding, spawn) {
                    panel.error = Some(e.message);
                }
            }
        }
    }
    if let Some(error) = &panel.error {
        ui.colored_label(egui::Color32::LIGHT_RED, error);
    }
    world.insert_resource(strip);
}

/// The row (and accessible) name of section `i` of `track`: its start and what it plays.
#[must_use]
pub fn section_row_name(track: &str, span: &SectionSpan) -> String {
    format!(
        "{track} section {} at {:.3} s, {}",
        span.index, span.start, span.label
    )
}

/// The focusable list of the track's sections (ADR-7383): Enter selects, Left / Right move the
/// section by one frame and Delete removes it, each press one undo entry through [`edit_section`]
/// (the `editor.seq_section_edit` path).
fn section_list(
    ui: &mut egui::Ui,
    panel: &mut SeqCurvePanel,
    track: &str,
    spans: &[SectionSpan],
    fps: f32,
) -> Option<StripAction> {
    use crate::canvas_keys::{RowKey, list_rows};
    if panel.selected.is_some() {
        panel.keys.retarget(panel.selected);
    }
    ui.strong(format!("Sections of {track}"));
    let mut by_index: Vec<&SectionSpan> = spans.iter().collect();
    by_index.sort_by_key(|s| s.index);
    let name = |i: usize| {
        by_index
            .get(i)
            .map(|s| section_row_name(track, s))
            .unwrap_or_default()
    };
    let selected = panel.selected;
    let events = list_rows(
        ui,
        "seq-section-list",
        &mut panel.keys,
        by_index.len(),
        crate::canvas_keys::LIST_HEIGHT / 2.0,
        selected,
        &name,
    );
    let mut action = None;
    for event in events {
        match event {
            RowKey::Activate(i) => panel.selected = Some(i),
            RowKey::Delete(i) => action = Some(StripAction::Edit(SectionEdit::Remove { index: i })),
            RowKey::Arrow {
                row, dx, alt: false, ..
            } if dx != 0 => {
                if let Some(span) = by_index.get(row) {
                    panel.selected = Some(row);
                    let start = (span.start + dx as f32 * crate::seq_curve_panel::frame_step(fps)).max(0.0);
                    action = Some(StripAction::Edit(SectionEdit::Move { index: row, start }));
                }
            }
            _ => {}
        }
    }
    action
}

/// What one frame of the strip draws: the player, its track's sections and the clock.
struct StripFrame<'a> {
    entity: Entity,
    channel: &'a Channel,
    spans: &'a [SectionSpan],
    rows: usize,
    playhead: f32,
    fps: f32,
    /// Each section's still, by index (empty unless a video track).
    thumbs: &'a [Option<egui::TextureId>],
}

fn strip_widget(
    ui: &mut egui::Ui,
    strip: &mut SeqSectionStrip,
    panel: &mut SeqCurvePanel,
    frame: &StripFrame<'_>,
) -> Option<StripAction> {
    let StripFrame {
        entity,
        channel,
        spans,
        rows,
        playhead,
        fps,
        thumbs,
    } = *frame;
    let width = ui.available_width().max(120.0);
    let height = RULER_HEIGHT + rows as f32 * ROW_HEIGHT + 4.0;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::click_and_drag());
    crate::canvas_keys::name_canvas(
        ui.ctx(),
        response.id,
        &format!("{} section strip, {} sections", channel.kind(), spans.len()),
    );
    let view = &mut strip.view;
    let key = (entity, panel.track);
    let resized = (view.rect.width() - rect.width()).abs() > 0.5;
    view.rect = rect;
    if strip.fitted != Some(key) || resized {
        view.zoom_to_fit(time_bounds(spans), 0.05);
        strip.fitted = Some(key);
    }
    let painter = ui.painter_at(rect);
    let tokens = crate::theme::ui_tokens(painter.ctx());
    painter.rect_filled(rect, 0.0, egui::Color32::from_gray(22));
    view.paint_grid(&painter, |x| format!("{x:.2}s"), false);
    let font = egui::FontId::proportional(11.0);
    for span in spans {
        let r = section_rect(view, span);
        let fill = if panel.selected == Some(span.index) {
            tokens.accents.active
        } else {
            egui::Color32::from_rgb(70, 110, 160)
        };
        painter.rect_filled(r, 3.0, fill);
        // The ramps as darker wedges at the ends.
        let ramp = |t: f32| view.to_screen(egui::vec2(t, 0.0)).x;
        let shade = egui::Color32::from_black_alpha(90);
        if span.rise > 0.0 {
            let x = ramp(span.start + span.rise).min(r.right());
            painter.add(egui::Shape::convex_polygon(
                vec![r.left_top(), r.left_bottom(), egui::pos2(x, r.top())],
                shade,
                egui::Stroke::NONE,
            ));
        }
        if span.fall > 0.0 {
            let x = ramp(span.end - span.fall).max(r.left());
            painter.add(egui::Shape::convex_polygon(
                vec![egui::pos2(x, r.top()), r.right_bottom(), r.right_top()],
                shade,
                egui::Stroke::NONE,
            ));
        }
        // A video section's still, at the start of its bar.
        let mut label_at = r.left_center() + egui::vec2(4.0, 0.0);
        if let Some(Some(texture)) = thumbs.get(span.index) {
            let width = (r.height() * 16.0 / 9.0).min(r.width());
            let at = egui::Rect::from_min_size(r.left_top(), egui::vec2(width, r.height()));
            painter.image(
                *texture,
                at,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
            label_at.x += width;
        }
        painter.text(
            label_at,
            egui::Align2::LEFT_CENTER,
            &span.label,
            font.clone(),
            egui::Color32::WHITE,
        );
    }
    let x = view.to_screen(egui::vec2(playhead, 0.0)).x;
    painter.line_segment(
        [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
        egui::Stroke::new(1.5, tokens.accents.alert),
    );

    if response.hovered() {
        let scroll = ui.input(|i| i.smooth_scroll_delta.y);
        if scroll != 0.0
            && let Some(anchor) = response.hover_pos()
        {
            let k = (1.0 + scroll * 0.002).clamp(0.5, 2.0);
            view.zoom_about(anchor, egui::vec2(k, 1.0));
        }
    }
    if response.dragged_by(egui::PointerButton::Secondary) || response.dragged_by(egui::PointerButton::Middle)
    {
        view.pan(egui::vec2(response.drag_delta().x, 0.0));
        return None;
    }
    if response.drag_started_by(egui::PointerButton::Primary) {
        // Where the button went down: a drag only starts once the pointer has left that spot.
        let origin = ui
            .input(|i| i.pointer.press_origin())
            .or(response.interact_pointer_pos());
        let pick = origin.and_then(|p| pick_section(spans, view, p));
        let press_time = origin.map_or(0.0, |p| view.to_value(p).x);
        let index = match pick {
            Some(SectionPick::Body(i) | SectionPick::Start(i) | SectionPick::End(i)) => Some(i),
            None => None,
        };
        let span = index.and_then(|i| spans.get(i));
        strip.drag = Some(StripDrag {
            pick,
            press_time,
            start: span.map_or(0.0, |s| s.start),
            end: span.map_or(0.0, |s| s.end),
            base: index.map(|_| channel.clone()),
        });
        if index.is_some() {
            panel.selected = index;
            panel.gestures += 1;
            panel.gesture = Some(panel.gestures);
        }
    }
    let dragging = strip.drag.clone();
    if response.drag_stopped() {
        strip.drag = None;
        panel.gesture = None;
    }
    if response.dragged_by(egui::PointerButton::Primary)
        && let Some(p) = response.interact_pointer_pos()
        && let Some(drag) = dragging
    {
        let t = strip.view.to_value(p).x;
        return Some(match (drag.pick, drag.base) {
            (Some(pick), Some(base)) => StripAction::Drag {
                edit: drag_edit(pick, drag.press_time, drag.start, drag.end, t, fps, strip.snap),
                base,
            },
            _ => StripAction::Scrub(t.max(0.0)),
        });
    }
    if (response.clicked() || response.secondary_clicked())
        && let Some(p) = response.interact_pointer_pos()
    {
        panel.selected = match pick_section(spans, &strip.view, p) {
            Some(SectionPick::Body(i) | SectionPick::Start(i) | SectionPick::End(i)) => Some(i),
            None => None,
        };
    }
    let mut action = None;
    let selected = panel.selected.and_then(|i| spans.get(i));
    response.context_menu(|ui| {
        let Some(span) = selected else {
            if ui.button("Zoom to fit").clicked() {
                strip.fitted = None;
                ui.close();
            }
            return;
        };
        ui.label(format!("Section {}: {}", span.index, span.label));
        ui.separator();
        if ui.button("Duplicate after it").clicked() {
            action = Some(StripAction::Edit(SectionEdit::Duplicate {
                index: span.index,
                start: span.end,
            }));
            ui.close();
        }
        if ui.button("Delete section").clicked() {
            action = Some(StripAction::Edit(SectionEdit::Remove { index: span.index }));
            ui.close();
        }
    });
    action
}

/// The selected section's numbers; a change is an edit (a drag of a number is one undo entry).
fn properties(
    ui: &mut egui::Ui,
    panel: &mut SeqCurvePanel,
    world: &mut World,
    channel: &Channel,
) -> Option<StripAction> {
    let index = panel.selected?;
    let track = panel.track;
    let fields = SectionFields::of(channel, index)?;
    let mut out = None;
    let mut started = false;
    let mut stopped = false;
    ui.horizontal_wrapped(|ui| {
        ui.label(format!("Section {index}"));
        let mut number = |ui: &mut egui::Ui, label: &str, value: f32, make: &dyn Fn(f32) -> SectionEdit| {
            ui.label(label);
            let mut v = value;
            let response = ui.add(egui::DragValue::new(&mut v).speed(0.01).max_decimals(3));
            started |= response.drag_started();
            stopped |= response.drag_stopped();
            if response.changed() && v != value && v.is_finite() {
                out = Some(StripAction::Edit(make(v)));
            }
        };
        // Like the edges: start and end trim (the body drag moves the section).
        number(ui, "start", fields.start, &|v| SectionEdit::TrimStart {
            index,
            start: v,
        });
        number(ui, "end", fields.end, &|v| SectionEdit::TrimEnd { index, end: v });
        number(ui, "offset", fields.offset, &|v| SectionEdit::Offset {
            index,
            offset: v,
        });
        if let Some(speed) = fields.speed {
            number(ui, "speed", speed, &|v| SectionEdit::Speed { index, speed: v });
        }
        if let Some((rise, fall)) = fields.ramps {
            number(ui, "in", rise, &|v| SectionEdit::Ramps { index, rise: v, fall });
            number(ui, "out", fall, &|v| SectionEdit::Ramps { index, rise, fall: v });
        }
        if let Some((label, gain)) = fields.gain {
            number(ui, label, gain, &|v| SectionEdit::Gain { index, value: v });
        }
        if let Some(scale) = fields.time_scale {
            number(ui, "time scale", scale, &|v| SectionEdit::TimeScale {
                index,
                scale: v,
            });
        }
        if let Some(Some(spatial)) = &fields.spatial {
            let with = |edit: &dyn Fn(&mut AudioSpatial), spatial: &AudioSpatial| {
                let mut s = spatial.clone();
                edit(&mut s);
                SectionEdit::Spatial {
                    index,
                    spatial: Some(s),
                }
            };
            number(ui, "min m", spatial.min_distance, &|v| {
                with(&|s| s.min_distance = v, spatial)
            });
            number(ui, "max m", spatial.max_distance, &|v| {
                with(&|s| s.max_distance = v, spatial)
            });
            number(ui, "3D blend", spatial.spatial_blend, &|v| {
                with(&|s| s.spatial_blend = v, spatial)
            });
            // v5: Doppler factor (0 off, 1 physical) and occlusion (ADR-3197). The edit preview applies
            // both with the runtime's numbers (ADR-8591).
            number(ui, "Doppler", spatial.doppler, &|v| {
                with(&|s| s.doppler = v, spatial)
            });
        }
        if let Some((repeat, loop_length)) = fields.repeat
            && let SubRepeat::Count(n) = repeat
        {
            number(ui, "passes", n as f32, &|v| SectionEdit::Repeat {
                index,
                repeat: SubRepeat::Count(v.round().clamp(1.0, 1.0e6) as u32),
                loop_length,
            });
        }
        // What it plays: typed into a buffer, applied on Enter or when the field loses focus.
        let label = ui.label(fields.name_label);
        let id = ui.id().with(("seq_section_name", track, index));
        let mut text = ui
            .data(|d| d.get_temp::<String>(id))
            .unwrap_or_else(|| fields.name.clone());
        let response = ui
            .add(egui::TextEdit::singleline(&mut text).desired_width(160.0))
            .labelled_by(label.id);
        if response.changed() {
            ui.data_mut(|d| d.insert_temp(id, text.clone()));
        }
        if response.lost_focus() {
            ui.data_mut(|d| d.remove::<String>(id));
            let name = text.trim();
            if !name.is_empty() && name != fields.name {
                out = Some(StripAction::Edit(SectionEdit::Rename {
                    index,
                    name: name.to_string(),
                }));
            }
        }
        // v3 switches: 3D placement of a sound, the repeat of a sub-sequence.
        if let Some(spatial) = &fields.spatial {
            let mut on = spatial.is_some();
            if ui.checkbox(&mut on, "3D").changed() {
                out = Some(StripAction::Edit(SectionEdit::Spatial {
                    index,
                    spatial: on.then(|| AudioSpatial::new(SoundSource::Position([0.0, 0.0, 0.0]))),
                }));
            }
            // v5: occlusion of a 3D sound (ADR-3197).
            if let Some(placed) = spatial {
                let mut occludes = placed.occlusion;
                if ui
                    .checkbox(&mut occludes, "Occlusion")
                    .on_hover_text(
                        "Colliders between the listener and the sound muffle and quieten it, at runtime and \
                         in Preview in level (against the level's colliders, heard from the editor camera).",
                    )
                    .changed()
                {
                    let mut s = placed.clone();
                    s.occlusion = occludes;
                    out = Some(StripAction::Edit(SectionEdit::Spatial {
                        index,
                        spatial: Some(s),
                    }));
                }
            }
        }
        // v5: keep the pitch when the time scale is not 1 (ADR-3197).
        if let Some(mode) = fields.time_scale_mode {
            let mut keep = mode == TimeScaleMode::PreservePitch;
            if ui
                .checkbox(&mut keep, "Keep pitch")
                .on_hover_text(
                    "The time scale changes the speed, not the pitch (a stretch rendered ahead), at runtime, in \
                     the render-out and in Preview in level. Rates 0.5 to 2; outside them it plays by varispeed.",
                )
                .changed()
            {
                out = Some(StripAction::Edit(SectionEdit::TimeScaleMode {
                    index,
                    mode: if keep {
                        TimeScaleMode::PreservePitch
                    } else {
                        TimeScaleMode::Varispeed
                    },
                }));
            }
        }
        if let Some((target, looping)) = &fields.video {
            let mut on = *looping;
            if ui.checkbox(&mut on, "Loop").changed() {
                out = Some(StripAction::Edit(SectionEdit::VideoLoop { index, looping: on }));
            }
            if let Some(target) = video_target_row(ui, track, index, target) {
                out = Some(StripAction::Edit(SectionEdit::VideoTarget { index, target }));
            }
        }
        if let Some((repeat, loop_length)) = fields.repeat {
            let mut picked = repeat;
            let combo = egui::ComboBox::from_id_salt(("seq_section_repeat", track, index))
                .selected_text(repeat.mode_name())
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut picked, SubRepeat::Once, "once");
                    let count = if let SubRepeat::Count(n) = repeat { n } else { 2 };
                    ui.selectable_value(&mut picked, SubRepeat::Count(count), "count");
                    ui.selectable_value(&mut picked, SubRepeat::UntilStop, "until_stop");
                });
            crate::a11y_audit::name(combo.response, egui::WidgetType::ComboBox, "Repeat");
            if picked != repeat {
                out = Some(StripAction::Edit(SectionEdit::Repeat {
                    index,
                    repeat: picked,
                    loop_length,
                }));
            }
        }
    });
    if started {
        panel.gestures += 1;
        panel.gesture = Some(panel.gestures);
    }
    if stopped {
        panel.gesture = None;
        world
            .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
            .seal();
    }
    out
}

/// The target of a video section: its kind, then what that kind asks for. Returns the new target
/// when one changed.
fn video_target_row(
    ui: &mut egui::Ui,
    track: usize,
    index: usize,
    target: &VideoTarget,
) -> Option<VideoTarget> {
    let binding = target.binding().unwrap_or("").to_string();
    let mut out = None;
    let mut kind = target.kind();
    let combo = egui::ComboBox::from_id_salt(("seq_video_target", track, index))
        .selected_text(kind)
        .show_ui(ui, |ui| {
            for name in ["fullscreen", "player", "screen", "ui_image"] {
                ui.selectable_value(&mut kind, name, name);
            }
        });
    crate::a11y_audit::name(combo.response, egui::WidgetType::ComboBox, "Video target");
    if kind != target.kind() {
        // A new binding starts as the one the section had, or as a name to overwrite.
        let name = if binding.is_empty() {
            "screen".to_string()
        } else {
            binding.clone()
        };
        out = Some(match kind {
            "player" => VideoTarget::Player(name),
            "screen" => VideoTarget::Screen {
                binding: name,
                slot: VideoScreenSlot::default(),
            },
            "ui_image" => VideoTarget::UiImage(name),
            _ => VideoTarget::default(),
        });
    }
    match target {
        VideoTarget::Fullscreen { layer, fit } => {
            ui.label("layer");
            let mut picked_layer = *layer;
            ui.add(egui::DragValue::new(&mut picked_layer).speed(0.1));
            let mut picked_fit = *fit;
            let combo = egui::ComboBox::from_id_salt(("seq_video_fit", track, index))
                .selected_text(format!("{fit:?}"))
                .show_ui(ui, |ui| {
                    for option in [VideoFit::Letterbox, VideoFit::Fill, VideoFit::Stretch] {
                        ui.selectable_value(&mut picked_fit, option, format!("{option:?}"));
                    }
                });
            crate::a11y_audit::name(combo.response, egui::WidgetType::ComboBox, "Fit");
            if picked_layer != *layer || picked_fit != *fit {
                out = Some(VideoTarget::Fullscreen {
                    layer: picked_layer,
                    fit: picked_fit,
                });
            }
        }
        VideoTarget::Screen { slot, .. } => {
            let mut picked = *slot;
            let combo = egui::ComboBox::from_id_salt(("seq_video_slot", track, index))
                .selected_text(format!("{slot:?}"))
                .show_ui(ui, |ui| {
                    for option in [VideoScreenSlot::Emissive, VideoScreenSlot::Albedo] {
                        ui.selectable_value(&mut picked, option, format!("{option:?}"));
                    }
                });
            crate::a11y_audit::name(combo.response, egui::WidgetType::ComboBox, "Screen slot");
            if picked != *slot {
                out = Some(VideoTarget::Screen {
                    binding: binding.clone(),
                    slot: picked,
                });
            }
        }
        VideoTarget::Player(_) | VideoTarget::UiImage(_) => {}
    }
    if target.binding().is_some() {
        let label = ui.label("binding");
        let id = ui.id().with(("seq_video_binding", track, index));
        let mut text = ui
            .data(|d| d.get_temp::<String>(id))
            .unwrap_or_else(|| binding.clone());
        let response = ui
            .add(egui::TextEdit::singleline(&mut text).desired_width(100.0))
            .labelled_by(label.id);
        if response.changed() {
            ui.data_mut(|d| d.insert_temp(id, text.clone()));
        }
        if response.lost_focus() {
            ui.data_mut(|d| d.remove::<String>(id));
            let name = text.trim();
            if !name.is_empty() && name != binding {
                let mut renamed = target.clone();
                if let Some(slot) = renamed.binding_mut() {
                    *slot = name.to_string();
                }
                out = Some(renamed);
            }
        }
    }
    out
}

/// The selected sub-sequence section's **Bindings** row: each target rename as
/// `target → binds as` with a remove button, and a target and a name field that set a pair on
/// Enter in either field or when either loses focus, once both are filled (whichever is filled
/// last); a half-filled pair keeps its text. Each action is one undo step
/// (`editor.seq_section_edit` op `sub_binding`); other sections keep their tables.
fn sub_binding_row(ui: &mut egui::Ui, panel: &SeqCurvePanel, channel: &Channel) -> Option<StripAction> {
    let Channel::Sub(list) = channel else {
        return None;
    };
    let index = panel.selected?;
    let section = list.get(index)?;
    let mut out = None;
    ui.horizontal_wrapped(|ui| {
        ui.label("Bindings");
        if section.bindings.is_empty() {
            ui.weak("(the sub-sequence's own names)");
        }
        for (from, to) in &section.bindings {
            ui.label(format!("{from} → {to}"));
            let remove = crate::a11y_audit::name(
                ui.small_button(format!("remove {from}")),
                egui::WidgetType::Button,
                &format!("Remove the binding of {from}"),
            );
            if remove.clicked() {
                out = Some(StripAction::Edit(SectionEdit::SubBinding {
                    index,
                    from: from.clone(),
                    to: None,
                }));
            }
        }
        // A new pair: the sub-sequence's target name and the name it binds as here.
        let id_from = ui.id().with(("seq_sub_binding_from", panel.track, index));
        let id_to = ui.id().with(("seq_sub_binding_to", panel.track, index));
        let mut from = ui.data(|d| d.get_temp::<String>(id_from)).unwrap_or_default();
        let mut to = ui.data(|d| d.get_temp::<String>(id_to)).unwrap_or_default();
        let target = crate::a11y_audit::name(
            ui.add(
                egui::TextEdit::singleline(&mut from)
                    .desired_width(90.0)
                    .hint_text("target"),
            ),
            egui::WidgetType::TextEdit,
            "Sub-sequence target name",
        );
        let binds = crate::a11y_audit::name(
            ui.add(
                egui::TextEdit::singleline(&mut to)
                    .desired_width(90.0)
                    .hint_text("binds as"),
            ),
            egui::WidgetType::TextEdit,
            "Local binding name",
        );
        if target.changed() {
            ui.data_mut(|d| d.insert_temp(id_from, from.clone()));
        }
        if binds.changed() {
            ui.data_mut(|d| d.insert_temp(id_to, to.clone()));
        }
        // Enter in a single-line field also ends its focus. The fields are cleared with the
        // commit, so egui's second pass of a frame finds nothing to commit (one undo step).
        let (from, to) = (from.trim(), to.trim());
        if (target.lost_focus() || binds.lost_focus()) && !from.is_empty() && !to.is_empty() {
            ui.data_mut(|d| {
                d.remove::<String>(id_from);
                d.remove::<String>(id_to);
            });
            out = Some(StripAction::Edit(SectionEdit::SubBinding {
                index,
                from: from.to_string(),
                to: Some(to.to_string()),
            }));
        }
    });
    out
}

/// The **Rename binding** row: a binding of the player's sequence and its new name, applied on
/// Enter or when the field loses focus.
fn binding_row(ui: &mut egui::Ui, world: &World, entity: Entity) -> Option<StripAction> {
    let player = world.get::<SequencePlayer>(entity)?;
    let names = binding_names(player.sequence(), player.library());
    if names.is_empty() {
        return None;
    }
    let mut out = None;
    ui.horizontal(|ui| {
        ui.label("Rename binding");
        let pick = ui.id().with(("seq_binding_pick", entity));
        let mut from = ui
            .data(|d| d.get_temp::<String>(pick))
            .filter(|n| names.contains(n))
            .unwrap_or_else(|| names[0].clone());
        let combo = egui::ComboBox::from_id_salt(("seq_binding_from", entity))
            .selected_text(from.as_str())
            .show_ui(ui, |ui| {
                for name in &names {
                    ui.selectable_value(&mut from, name.clone(), name.as_str());
                }
            });
        crate::a11y_audit::name(combo.response, egui::WidgetType::ComboBox, "Binding to rename");
        ui.data_mut(|d| d.insert_temp(pick, from.clone()));
        ui.label("to");
        let id = ui.id().with(("seq_binding_to", entity));
        let mut text = ui.data(|d| d.get_temp::<String>(id)).unwrap_or_default();
        let response = crate::a11y_audit::name(
            ui.add(egui::TextEdit::singleline(&mut text).desired_width(120.0)),
            egui::WidgetType::TextEdit,
            "New binding name",
        );
        if response.changed() {
            ui.data_mut(|d| d.insert_temp(id, text.clone()));
        }
        if response.lost_focus() {
            ui.data_mut(|d| d.remove::<String>(id));
            let to = text.trim();
            if !to.is_empty() && to != from {
                out = Some(StripAction::RenameBinding {
                    from: from.clone(),
                    to: to.to_string(),
                });
            }
        }
    });
    out
}

/// The binding names the kind row offers: what the sequence binds plus its spawnables, sorted.
fn kind_names(sequence: &Sequence, player: &SequencePlayer) -> Vec<String> {
    let mut names = binding_names(sequence, player.library());
    names.extend(sequence.spawnables.iter().map(|s| s.binding.clone()));
    names.sort_unstable();
    names.dedup();
    names
}

/// Most `.prefab.ron` files the prefab picker lists, and how deep it looks.
const MAX_PICKER_PREFABS: usize = 512;
const MAX_PICKER_DEPTH: usize = 8;

/// The project's `.prefab.ron` files as `/`-separated relative paths, sorted (the prefab picker).
pub fn project_prefabs(root: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            // Build output, caches and hidden folders hold no project prefabs.
            if name.starts_with('.') || name == "target" || name == "build" {
                continue;
            }
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() && depth < MAX_PICKER_DEPTH {
                stack.push((path, depth + 1));
            } else if kind.is_file()
                && name.ends_with(".prefab.ron")
                && let Ok(rel) = path.strip_prefix(root)
            {
                let rel: Vec<String> = rel
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect();
                out.push(rel.join("/"));
                if out.len() >= MAX_PICKER_PREFABS {
                    out.sort_unstable();
                    return out;
                }
            }
        }
    }
    out.sort_unstable();
    out
}

/// The **Binding kind** row (ADR-2948): a binding, whether the player finds it (Bound) or spawns
/// it from a prefab (Spawnable), and for a spawnable its prefab, range and `keep_on_stop`.
fn spawn_row(ui: &mut egui::Ui, world: &World, entity: Entity) -> Option<StripAction> {
    let player = world.get::<SequencePlayer>(entity)?;
    let sequence = player.sequence();
    let names = kind_names(sequence, player);
    if names.is_empty() {
        return None;
    }
    let mut out = None;
    ui.horizontal(|ui| {
        ui.label("Binding kind");
        let pick = ui.id().with(("seq_kind_pick", entity));
        let mut binding = ui
            .data(|d| d.get_temp::<String>(pick))
            .filter(|n| names.contains(n))
            .unwrap_or_else(|| names[0].clone());
        let combo = egui::ComboBox::from_id_salt(("seq_kind_binding", entity))
            .selected_text(binding.as_str())
            .show_ui(ui, |ui| {
                for name in &names {
                    ui.selectable_value(&mut binding, name.clone(), name.as_str());
                }
            });
        crate::a11y_audit::name(combo.response, egui::WidgetType::ComboBox, "Binding");
        ui.data_mut(|d| d.insert_temp(pick, binding.clone()));
        let current = sequence.spawnables.iter().find(|s| s.binding == binding).cloned();
        let mut spawned = current.is_some();
        ui.radio_value(&mut spawned, false, "Bound");
        ui.radio_value(&mut spawned, true, "Spawnable");
        if spawned != current.is_some() {
            out = Some(StripAction::BindingKind {
                binding: binding.clone(),
                spawn: spawned.then(|| Spawnable::new(binding.clone(), "", 0.0)),
            });
            return;
        }
        let Some(spawn) = current else {
            return;
        };
        let mut edited = spawn.clone();
        // The prefab picker: the project's `.prefab.ron` files, read when the list opens.
        let list_id = ui.id().with(("seq_kind_prefabs", entity));
        let combo = egui::ComboBox::from_id_salt(("seq_kind_prefab", entity))
            .selected_text(if edited.prefab.is_empty() {
                "(pick a prefab)"
            } else {
                edited.prefab.as_str()
            })
            .show_ui(ui, |ui| {
                let list = ui
                    .data(|d| d.get_temp::<Vec<String>>(list_id))
                    .unwrap_or_else(|| {
                        let list = world
                            .get_resource::<crate::agent_ext::EditorProject>()
                            .and_then(|p| p.root.clone())
                            .or_else(|| project_root(world))
                            .map(|root| project_prefabs(&root))
                            .unwrap_or_default();
                        ui.data_mut(|d| d.insert_temp(list_id, list.clone()));
                        list
                    });
                if list.is_empty() {
                    ui.label("no .prefab.ron in the project");
                }
                for path in &list {
                    ui.selectable_value(&mut edited.prefab, path.clone(), path.as_str());
                }
            });
        crate::a11y_audit::name(combo.response, egui::WidgetType::ComboBox, "Prefab");
        ui.label("from");
        ui.add(
            egui::DragValue::new(&mut edited.start)
                .speed(0.05)
                .range(0.0..=1.0e6),
        );
        let mut open = edited.end.is_none();
        ui.checkbox(&mut open, "to end");
        if open {
            edited.end = None;
        } else {
            let mut end = edited.end.unwrap_or(edited.start + 1.0);
            ui.add(egui::DragValue::new(&mut end).speed(0.05).range(0.0..=1.0e6));
            edited.end = Some(end);
        }
        ui.checkbox(&mut edited.keep_on_stop, "keep on stop");
        if edited != spawn {
            out = Some(StripAction::BindingKind {
                binding: binding.clone(),
                spawn: Some(edited),
            });
        }
    });
    out
}

/// Makes `binding` hand-bound (`None`: its spawnable is removed) or a spawnable (`Some`: added or
/// replaced), checked by the sequence's own validation. A new spawnable with no prefab yet is
/// kept out of the file until one is picked: it is refused with `seq_spawnable_invalid`, as is
/// any other invalid entry.
pub fn apply_binding_kind(
    sequence: &mut Sequence,
    binding: &str,
    spawn: Option<Spawnable>,
) -> Result<(), EditError> {
    let invalid = |message: String| EditError {
        code: "seq_spawnable_invalid",
        message,
    };
    let mut next = sequence.clone();
    let at = next.spawnables.iter().position(|s| s.binding == binding);
    match (spawn, at) {
        (None, Some(i)) => {
            next.spawnables.remove(i);
        }
        (None, None) => return Err(invalid(format!("`{binding}` is not a spawnable"))),
        (Some(mut spawn), at) => {
            spawn.binding = binding.to_string();
            if spawn.prefab.is_empty() {
                // Pick the first prefab later: a placeholder that validates cannot exist, so the
                // row shows the picker for an entry that is not saved yet.
                return Err(invalid(format!("pick a prefab for `{binding}`")));
            }
            match at {
                Some(i) => next.spawnables[i] = spawn,
                None => next.spawnables.push(spawn),
            }
        }
    }
    next.validate().map_err(|e| invalid(e.to_string()))?;
    *sequence = next;
    Ok(())
}

/// One undo entry of a binding kind change: the whole sequence before and after.
struct BindingKindEdit {
    player: Entity,
    before: Sequence,
    after: Sequence,
}

impl EditorCommand for BindingKindEdit {
    fn apply(&mut self, world: &mut World) {
        if let Some(mut player) = world.get_mut::<SequencePlayer>(self.player) {
            player.replace_sequence(self.after.clone());
        }
    }

    fn revert(&mut self, world: &mut World) {
        if let Some(mut player) = world.get_mut::<SequencePlayer>(self.player) {
            player.replace_sequence(self.before.clone());
        }
    }

    fn merge(&mut self, _next: &dyn EditorCommand) -> bool {
        false
    }

    fn description(&self) -> String {
        "Change sequence binding kind".to_string()
    }
}

/// Applies a binding kind change to `entity`'s sequence as one undo entry.
fn set_binding_kind(
    world: &mut World,
    entity: Entity,
    binding: &str,
    spawn: Option<Spawnable>,
) -> Result<(), EditError> {
    let Some(player) = world.get::<SequencePlayer>(entity) else {
        return Err(EditError {
            code: "seq_curve_no_player",
            message: "no sequence player exists".into(),
        });
    };
    let before = player.sequence().clone();
    let mut after = before.clone();
    apply_binding_kind(&mut after, binding, spawn)?;
    // A locked track keeps its binding (`.seq.ron` v6, ADR-8105).
    regia_framework::sequencer::edit::tracks::check_locked(&before, &after)?;
    if let Some(mut player) = world.get_mut::<SequencePlayer>(entity) {
        player.replace_sequence(after.clone());
    }
    let mut undo = world.get_resource_or_insert_with::<UndoStack>(UndoStack::new);
    undo.seal();
    undo.push(Box::new(BindingKindEdit {
        player: entity,
        before,
        after,
    }));
    undo.seal();
    Ok(())
}

/// `editor.seq_section_edit { op: "binding_kind", binding, kind: "bound" | "spawnable", prefab?,
/// start?, end?, keep_on_stop? }` (ADR-2948).
fn binding_kind_op(world: &mut World, panel: &mut SeqCurvePanel, params: &Value) -> Result<Value, RpcError> {
    let text = |field: &str| params.get(field).and_then(Value::as_str);
    let binding = text("binding")
        .filter(|b| !b.is_empty())
        .ok_or_else(|| RpcError::invalid_params("`binding` (a string) is required for `binding_kind`"))?
        .to_string();
    let spawn = match text("kind") {
        Some("bound") => None,
        Some("spawnable") => {
            let prefab = text("prefab")
                .ok_or_else(|| RpcError::invalid_params("`prefab` is required for a spawnable"))?
                .to_string();
            let end = match params.get("end") {
                None | Some(Value::Null) => None,
                Some(_) => number(params, "end")?,
            };
            Some(Spawnable {
                binding: binding.clone(),
                prefab,
                start: number(params, "start")?.unwrap_or(0.0),
                end,
                keep_on_stop: params
                    .get("keep_on_stop")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            })
        }
        _ => {
            return Err(RpcError::invalid_params(
                "`kind` must be \"bound\" or \"spawnable\"",
            ));
        }
    };
    let entity = player_of(world, panel).ok_or_else(|| {
        rpc(EditError {
            code: "seq_curve_no_player",
            message: "no sequence player exists".into(),
        })
    })?;
    set_binding_kind(world, entity, &binding, spawn).map_err(section_rpc)?;
    let spawnables = world
        .get::<SequencePlayer>(entity)
        .map(|p| p.sequence().spawnables.clone())
        .unwrap_or_default();
    let list: Vec<Value> = spawnables
        .iter()
        .map(|s| json!({ "binding": s.binding, "prefab": s.prefab, "start": s.start, "end": s.end, "keep_on_stop": s.keep_on_stop }))
        .collect();
    Ok(json!({ "applied": true, "binding": binding, "spawnables": list }))
}

/// The editable numbers of one section.
struct SectionFields {
    start: f32,
    end: f32,
    offset: f32,
    speed: Option<f32>,
    ramps: Option<(f32, f32)>,
    gain: Option<(&'static str, f32)>,
    /// The clip, sound or sequence name, and what to call it.
    name: String,
    name_label: &'static str,
    /// An audio section's time scale (v3).
    time_scale: Option<f32>,
    /// An audio section's 3D placement (v3; the outer `None` for other kinds).
    spatial: Option<Option<AudioSpatial>>,
    /// An audio section's time scale mode (v5; `None` for other kinds).
    time_scale_mode: Option<TimeScaleMode>,
    /// A sub-sequence section's repeat and pass length (v3).
    repeat: Option<(SubRepeat, Option<f32>)>,
    /// A video section's target and loop switch (v4).
    video: Option<(VideoTarget, bool)>,
}

impl SectionFields {
    fn of(channel: &Channel, index: usize) -> Option<Self> {
        Some(match channel {
            Channel::Clip(list) => {
                let s = list.get(index)?;
                Self {
                    start: s.start,
                    end: s.end,
                    offset: s.offset,
                    speed: Some(s.speed),
                    ramps: Some((s.blend_in, s.blend_out)),
                    gain: Some(("weight", s.weight)),
                    name: s.clip.clone(),
                    name_label: "clip",
                    time_scale: None,
                    spatial: None,
                    time_scale_mode: None,
                    repeat: None,
                    video: None,
                }
            }
            Channel::Audio(list) => {
                let s = list.get(index)?;
                Self {
                    start: s.start,
                    end: s.end,
                    offset: s.offset,
                    speed: None,
                    ramps: Some((s.fade_in, s.fade_out)),
                    gain: Some(("dB", s.volume_db)),
                    name: s.sound.clone(),
                    name_label: "sound",
                    time_scale: Some(s.time_scale),
                    spatial: Some(s.spatial.clone()),
                    time_scale_mode: Some(s.time_scale_mode),
                    repeat: None,
                    video: None,
                }
            }
            Channel::Sub(list) => {
                let s = list.get(index)?;
                Self {
                    start: s.start,
                    end: s.end,
                    offset: s.offset,
                    speed: Some(s.speed),
                    ramps: None,
                    gain: None,
                    name: s.sequence.clone(),
                    name_label: "sequence",
                    time_scale: None,
                    spatial: None,
                    time_scale_mode: None,
                    repeat: Some((s.repeat, s.loop_length)),
                    video: None,
                }
            }
            Channel::Video(list) => {
                let s = list.get(index)?;
                Self {
                    start: s.start,
                    end: s.end,
                    offset: s.offset,
                    speed: None,
                    ramps: None,
                    gain: Some(("dB", s.volume_db)),
                    name: s.video.clone(),
                    name_label: "video",
                    time_scale: None,
                    spatial: None,
                    time_scale_mode: None,
                    repeat: None,
                    video: Some((s.target.clone(), s.looping)),
                }
            }
            Channel::Float(_) | Channel::Vec3(_) | Channel::Bool(_) | Channel::Event(_) => return None,
        })
    }
}

// ---- agent methods ----

const PLAYER: ParamSchema = ParamSchema {
    name: "player",
    kind: "number",
    required: false,
    description: "Sequence player entity id to select (default: the panel's player, else the first).",
};
const TRACK: ParamSchema = ParamSchema {
    name: "track",
    kind: "string",
    required: false,
    description: "Track name to select (a Clip, Audio, Sub or Video track).",
};

/// The agent methods of the section strip.
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.seq_sections_state",
                tool: "seq_sections_state",
                description: "Read the section strip of the Sequence Curves panel: the selected clip, audio, \
                    sub-sequence or video track's sections in index order with their strip row (overlapping sections \
                    take separate rows) and every field, the selected section, and a video track's first-frame stills. \
                    Optionally selects a player and track.",
                params: &[PLAYER, TRACK],
                result: "{ players: [id], player, time, fps, track, track_name, kind, selected, rows, \
                    sections: [{ index, row, start, end, label, rise, fall, section }], \
                    thumbnails: [{ index, state: pending|ready|failed, frame, width, height, code }], bindings: [name], error }",
                mutating: false,
                permission: Permission::Read,
            },
            seq_sections_state as Handler,
        ),
        (
            MethodSchema {
                name: "editor.seq_section_edit",
                tool: "seq_section_edit",
                description: "Edit one section of the selected clip, audio, sub-sequence or video track, as the section \
                    strip would: add, duplicate, remove, move, trim_start, trim_end (trims keep the content on the \
                    clock), speed, offset, ramps (blend or fade in and out), gain (clip weight, audio dB) or rename (the \
                    clip, sound or sub-sequence it plays); v3: time_scale (a sound's varispeed rate), spatial (a sound's \
                    3D placement, or null for flat), repeat (a sub-sequence's passes and pass length); v4: gain and rename \
                    (a video's volume in dB and source path), video_target and video_loop; v5: time_scale_mode (a \
                    sound's varispeed or preserve_pitch), and spatial's doppler and occlusion fields; sub_binding (one \
                    sub-sequence section's target rename: `from` the sub-sequence's target name, `to` the name it binds \
                    as here, null to remove — other sections keep their tables); and rename_binding (a binding name \
                    everywhere the sequence refers to it, plus the player's binding table entry; needs no track). The \
                    whole sequence is validated first; a refused edit changes nothing. Each call is an undo entry.",
                params: &[
                    PLAYER,
                    TRACK,
                    ParamSchema {
                        name: "op",
                        kind: "string",
                        required: true,
                        description: "add, duplicate, remove, move, trim_start, trim_end, speed, offset, ramps, gain, rename, \
                            time_scale, time_scale_mode, spatial, repeat, video_target, video_loop, sub_binding, \
                            rename_binding or binding_kind.",
                    },
                    ParamSchema {
                        name: "index",
                        kind: "number",
                        required: false,
                        description: "Every op but add: the section index.",
                    },
                    ParamSchema {
                        name: "section",
                        kind: "object",
                        required: false,
                        description: "add: the section as in the file (clip, sound, sequence or video with start and \
                            end; other fields default).",
                    },
                    ParamSchema {
                        name: "start",
                        kind: "number",
                        required: false,
                        description: "duplicate, move, trim_start: seconds on the sequence clock.",
                    },
                    ParamSchema {
                        name: "end",
                        kind: "number",
                        required: false,
                        description: "trim_end: seconds on the sequence clock.",
                    },
                    ParamSchema {
                        name: "speed",
                        kind: "number",
                        required: false,
                        description: "speed: content seconds per sequence second, in (0, 1000].",
                    },
                    ParamSchema {
                        name: "offset",
                        kind: "number",
                        required: false,
                        description: "offset: content seconds at the section's start.",
                    },
                    ParamSchema {
                        name: "rise",
                        kind: "number",
                        required: false,
                        description: "ramps: seconds of blend or fade in.",
                    },
                    ParamSchema {
                        name: "fall",
                        kind: "number",
                        required: false,
                        description: "ramps: seconds of blend or fade out.",
                    },
                    ParamSchema {
                        name: "value",
                        kind: "number",
                        required: false,
                        description: "gain: a clip's weight in (0, 1], or the volume in dB (at most 24) of a sound or of \
                            a video's own sound.",
                    },
                    ParamSchema {
                        name: "name",
                        kind: "string",
                        required: false,
                        description: "rename: the clip, sound, sub-sequence name or video path the section plays.",
                    },
                    ParamSchema {
                        name: "scale",
                        kind: "number",
                        required: false,
                        description: "time_scale: sound seconds per sequence second, in (0, 16] (pitch follows unless \
                            the section keeps its pitch; then in 0.5..=2).",
                    },
                    ParamSchema {
                        name: "mode",
                        kind: "string",
                        required: false,
                        description: "time_scale_mode: varispeed (pitch follows the time scale) or preserve_pitch (a WSOLA \
                            stretch rendered ahead; time scale 0.5..=2).",
                    },
                    ParamSchema {
                        name: "spatial",
                        kind: "object",
                        required: false,
                        description: "spatial: { source: { \"Binding\": name } or { \"Position\": [x, y, z] }, min_distance, \
                            max_distance, rolloff: \"Inverse\" | \"Linear\" | { \"Log\": dB }, spatial_blend, doppler (0 off, 1 physical, \
                            up to 4; 1 when left out), occlusion (bool, false when left out) }, or null for flat.",
                    },
                    ParamSchema {
                        name: "repeat",
                        kind: "string",
                        required: false,
                        description: "repeat: once, count or until_stop.",
                    },
                    ParamSchema {
                        name: "count",
                        kind: "number",
                        required: false,
                        description: "repeat count: the number of passes, 1..=1000000.",
                    },
                    ParamSchema {
                        name: "loop_length",
                        kind: "number",
                        required: false,
                        description: "repeat: sub-sequence seconds per pass (omit or null: its length after the offset).",
                    },
                    ParamSchema {
                        name: "target",
                        kind: "object",
                        required: false,
                        description: "video_target: { \"Fullscreen\": { layer, fit: \"Letterbox\" | \"Fill\" | \"Stretch\" } }, \
                            { \"Player\": binding }, { \"Screen\": { binding, slot: \"Albedo\" | \"Emissive\" } } or \
                            { \"UiImage\": binding }.",
                    },
                    ParamSchema {
                        name: "looping",
                        kind: "boolean",
                        required: false,
                        description: "video_loop: wrap over the source's length instead of holding its last frame.",
                    },
                    ParamSchema {
                        name: "from",
                        kind: "string",
                        required: false,
                        description: "rename_binding: the binding name to rename. sub_binding: the sub-sequence's \
                            target name (the bindings map's key).",
                    },
                    ParamSchema {
                        name: "to",
                        kind: "string",
                        required: false,
                        description: "rename_binding: the new name (not already a binding of the sequence or the \
                            player). sub_binding: the name the target binds as here (null or absent removes the \
                            entry).",
                    },
                    ParamSchema {
                        name: "binding",
                        kind: "string",
                        required: false,
                        description: "binding_kind: the binding to make hand-bound or spawned (ADR-2948).",
                    },
                    ParamSchema {
                        name: "kind",
                        kind: "string",
                        required: false,
                        description: "binding_kind: \"bound\" (the player finds it) or \"spawnable\" (spawned from a prefab).",
                    },
                    ParamSchema {
                        name: "prefab",
                        kind: "string",
                        required: false,
                        description: "binding_kind spawnable: a project .prefab.ron path or guid:<32 hex>.",
                    },
                    ParamSchema {
                        name: "keep_on_stop",
                        kind: "boolean",
                        required: false,
                        description: "binding_kind spawnable: leave the actor in the world when the player stops.",
                    },
                ],
                result: "{ applied, selected, sections: [{ index, row, start, end, label, rise, fall, section }] } \
                    (rename_binding: { applied, from, to, tracks, sounds, videos, sub_sections, unresolved, bindings })",
                mutating: true,
                permission: Permission::EditScene,
            },
            seq_section_edit as Handler,
        ),
    ]
}

/// `rpc` with the section codes' classes and hints.
fn section_rpc(e: EditError) -> RpcError {
    let hint = match e.code {
        "seq_section_not_sections" => {
            "editor.seq_sections_state lists the tracks; Float and Vec3 tracks are edited with editor.seq_curve_edit"
        }
        "seq_section_bad_index" => "editor.seq_sections_state lists the sections",
        "seq_section_wrong_kind" => "the op or section does not apply to this track kind",
        "seq_section_last" => "disable the track instead of removing its last section",
        "seq_section_limit" => "a track holds at most 1024 sections",
        "seq_section_out_of_range" => "the message says which number is out of range; nothing was changed",
        "seq_binding_empty" => "`from` and `to` must both be non-empty",
        "seq_binding_same" => "`to` must differ from `from`",
        "seq_binding_in_use" => "pick a name no track, sound or sub-sequence of this sequence binds yet",
        "seq_binding_unknown" => "editor.seq_sections_state lists the bindings",
        "seq_binding_table_in_use" => "sequence.player { \"action\": \"unbind\" } frees the name first",
        _ => return rpc(e),
    };
    let class = if matches!(e.code, "seq_section_bad_index" | "seq_binding_unknown") {
        regia_agent::protocol::NOT_FOUND
    } else {
        regia_agent::protocol::INVALID_PARAMS
    };
    RpcError::new(class, e.code, e.message, hint)
}

fn section_json(channel: &Channel, span: &SectionSpan) -> Value {
    let section = match channel {
        Channel::Clip(list) => list.get(span.index).map(serde_json::to_value),
        Channel::Audio(list) => list.get(span.index).map(serde_json::to_value),
        Channel::Sub(list) => list.get(span.index).map(serde_json::to_value),
        Channel::Video(list) => list.get(span.index).map(serde_json::to_value),
        Channel::Float(_) | Channel::Vec3(_) | Channel::Bool(_) | Channel::Event(_) => None,
    };
    json!({
        "index": span.index,
        "row": span.row,
        "start": span.start,
        "end": span.end,
        "label": span.label,
        "rise": span.rise,
        "fall": span.fall,
        "section": section.and_then(Result::ok),
    })
}

fn sections_json(channel: &Channel) -> Value {
    section_spans(channel).map_or(json!([]), |spans| {
        json!(spans.iter().map(|s| section_json(channel, s)).collect::<Vec<_>>())
    })
}

/// The state of each section's still (a video track; empty for others).
fn thumbs_json(world: &mut World, channel: Option<&Channel>) -> Value {
    let Some(channel @ Channel::Video(list)) = channel else {
        return json!([]);
    };
    pump_thumbs(world, channel);
    let mut thumbs = world.get_resource_or_insert_with(SeqVideoThumbs::default);
    let list: Vec<Value> = list
        .iter()
        .enumerate()
        .map(
            |(index, section)| match thumbs.request(&section.video, section.offset) {
                SeqThumb::Pending => json!({ "index": index, "state": "pending" }),
                SeqThumb::Ready { still, .. } => json!({
                    "index": index,
                    "state": "ready",
                    "frame": still.frame,
                    "width": still.width,
                    "height": still.height,
                }),
                SeqThumb::Failed(code) => json!({ "index": index, "state": "failed", "code": code }),
            },
        )
        .collect();
    json!(list)
}

fn state_json(world: &mut World, panel: &SeqCurvePanel) -> Value {
    let channel = player_of(world, panel)
        .and_then(|e| world.get::<SequencePlayer>(e))
        .and_then(|p| p.sequence().tracks.get(panel.track).map(|t| t.keys.clone()));
    let thumbnails = thumbs_json(world, channel.as_ref());
    let all: Vec<u64> = players(world).into_iter().map(Entity::to_bits).collect();
    let Some(player) = player_of(world, panel).and_then(|e| world.get::<SequencePlayer>(e).map(|p| (e, p)))
    else {
        return json!({ "players": all, "player": null, "error": panel.error });
    };
    let (entity, player) = player;
    let sequence = player.sequence();
    let track = sequence.tracks.get(panel.track);
    let channel = track.map(|t| &t.keys);
    let rows = channel
        .and_then(section_spans)
        .map_or(0, |spans| spans.iter().map(|s| s.row + 1).max().unwrap_or(0));
    json!({
        "players": all,
        "player": entity.to_bits(),
        "time": player.time(),
        "fps": sequence.fps,
        "track": panel.track,
        "track_name": track.map(|t| t.name.clone()),
        "kind": channel.map(Channel::kind),
        "selected": panel.selected,
        "rows": rows,
        "sections": channel.map_or(json!([]), sections_json),
        "thumbnails": thumbnails,
        "bindings": binding_names(sequence, player.library()),
        "error": panel.error,
    })
}

fn seq_sections_state(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mut panel = world.remove_resource::<SeqCurvePanel>().unwrap_or_default();
    let out = match select(world, &mut panel, params) {
        Ok(()) => Ok(state_json(world, &panel)),
        Err(e) => Err(rpc(e)),
    };
    world.insert_resource(panel);
    out
}

fn index_param(params: &Value) -> Result<usize, RpcError> {
    params
        .get("index")
        .and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
        .ok_or_else(|| RpcError::invalid_params("`index` must be a whole number (the section index)"))
}

/// The edit an `editor.seq_section_edit` call asks for. `kind` is the selected track's channel
/// kind (an added `section` is read as that kind).
pub fn section_edit_of_params(params: &Value, kind: Option<&str>) -> Result<SectionEdit, RpcError> {
    let op = params.get("op").and_then(Value::as_str).ok_or_else(|| {
        RpcError::invalid_params(
            "`op` is required: add, duplicate, remove, move, trim_start, trim_end, speed, offset, ramps, gain, rename, \
             time_scale, time_scale_mode, spatial, repeat, video_target, video_loop, sub_binding or rename_binding",
        )
    })?;
    let need = |field: &str| -> Result<f32, RpcError> {
        number(params, field)?
            .ok_or_else(|| RpcError::invalid_params(format!("`{field}` is required for `{op}`")))
    };
    Ok(match op {
        "add" => {
            let section = params
                .get("section")
                .filter(|v| v.is_object())
                .cloned()
                .ok_or_else(|| RpcError::invalid_params("`section` (an object) is required for `add`"))?;
            let bad = |e: serde_json::Error| RpcError::invalid_params(format!("`section`: {e}"));
            SectionEdit::Add(match kind {
                Some("Clip") => Section::Clip(serde_json::from_value::<ClipSection>(section).map_err(bad)?),
                Some("Audio") => {
                    Section::Audio(serde_json::from_value::<AudioSection>(section).map_err(bad)?)
                }
                Some("Sub") => Section::Sub(serde_json::from_value::<SubSection>(section).map_err(bad)?),
                Some("Video") => {
                    Section::Video(serde_json::from_value::<VideoSection>(section).map_err(bad)?)
                }
                _ => {
                    return Err(section_rpc(EditError {
                        code: "seq_section_not_sections",
                        message: "the selected track has no sections".into(),
                    }));
                }
            })
        }
        "duplicate" => SectionEdit::Duplicate {
            index: index_param(params)?,
            start: need("start")?,
        },
        "remove" => SectionEdit::Remove {
            index: index_param(params)?,
        },
        "move" => SectionEdit::Move {
            index: index_param(params)?,
            start: need("start")?,
        },
        "trim_start" => SectionEdit::TrimStart {
            index: index_param(params)?,
            start: need("start")?,
        },
        "trim_end" => SectionEdit::TrimEnd {
            index: index_param(params)?,
            end: need("end")?,
        },
        "speed" => SectionEdit::Speed {
            index: index_param(params)?,
            speed: need("speed")?,
        },
        "offset" => SectionEdit::Offset {
            index: index_param(params)?,
            offset: need("offset")?,
        },
        "ramps" => SectionEdit::Ramps {
            index: index_param(params)?,
            rise: need("rise")?,
            fall: need("fall")?,
        },
        "gain" => SectionEdit::Gain {
            index: index_param(params)?,
            value: need("value")?,
        },
        "rename" => SectionEdit::Rename {
            index: index_param(params)?,
            name: params
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| RpcError::invalid_params("`name` (a string) is required for `rename`"))?
                .to_string(),
        },
        "time_scale" => SectionEdit::TimeScale {
            index: index_param(params)?,
            scale: need("scale")?,
        },
        "time_scale_mode" => {
            let mode = params.get("mode").and_then(Value::as_str).ok_or_else(|| {
                RpcError::invalid_params(
                    "`mode` (varispeed or preserve_pitch) is required for `time_scale_mode`",
                )
            })?;
            SectionEdit::TimeScaleMode {
                index: index_param(params)?,
                mode: TimeScaleMode::parse(mode).ok_or_else(|| {
                    RpcError::invalid_params(format!("unknown `mode` `{mode}` (varispeed, preserve_pitch)"))
                })?,
            }
        }
        "spatial" => {
            let value = params.get("spatial").ok_or_else(|| {
                RpcError::invalid_params("`spatial` (an object, or null for flat) is required")
            })?;
            let spatial = if value.is_null() {
                None
            } else {
                Some(
                    serde_json::from_value::<AudioSpatial>(value.clone())
                        .map_err(|e| RpcError::invalid_params(format!("`spatial`: {e}")))?,
                )
            };
            SectionEdit::Spatial {
                index: index_param(params)?,
                spatial,
            }
        }
        "repeat" => {
            let mode = params.get("repeat").and_then(Value::as_str).ok_or_else(|| {
                RpcError::invalid_params("`repeat` (once, count or until_stop) is required for `repeat`")
            })?;
            let repeat = match mode {
                "once" => SubRepeat::Once,
                "until_stop" => SubRepeat::UntilStop,
                "count" => {
                    let count = params
                        .get("count")
                        .and_then(Value::as_u64)
                        .and_then(|n| u32::try_from(n).ok())
                        .ok_or_else(|| {
                            RpcError::invalid_params("`count` (a whole number) is required for `count`")
                        })?;
                    SubRepeat::Count(count)
                }
                other => {
                    return Err(RpcError::invalid_params(format!(
                        "unknown `repeat` `{other}` (once, count, until_stop)"
                    )));
                }
            };
            SectionEdit::Repeat {
                index: index_param(params)?,
                repeat,
                loop_length: number(params, "loop_length")?,
            }
        }
        "video_target" => {
            let target = params
                .get("target")
                .cloned()
                .ok_or_else(|| RpcError::invalid_params("`target` is required for `video_target`"))
                .and_then(|value| {
                    serde_json::from_value::<VideoTarget>(value)
                        .map_err(|e| RpcError::invalid_params(format!("`target`: {e}")))
                })?;
            SectionEdit::VideoTarget {
                index: index_param(params)?,
                target,
            }
        }
        "video_loop" => SectionEdit::VideoLoop {
            index: index_param(params)?,
            looping: params
                .get("looping")
                .and_then(Value::as_bool)
                .ok_or_else(|| RpcError::invalid_params("`looping` (a bool) is required for `video_loop`"))?,
        },
        "sub_binding" => {
            // A sub-sequence section's target rename: `from` is the sub-sequence's target name,
            // `to` the name it binds as here (null or absent removes the entry).
            let name = |field: &str| -> Result<String, RpcError> {
                params
                    .get(field)
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .ok_or_else(|| {
                        RpcError::invalid_params(format!(
                            "`{field}` (a string) is required for `sub_binding`"
                        ))
                    })
            };
            let to = match params.get("to") {
                None | Some(Value::Null) => None,
                Some(Value::String(to)) => Some(to.clone()),
                Some(_) => {
                    return Err(RpcError::invalid_params(
                        "`to` must be a string (or null to remove the binding)",
                    ));
                }
            };
            SectionEdit::SubBinding {
                index: index_param(params)?,
                from: name("from")?,
                to,
            }
        }
        other => {
            return Err(RpcError::invalid_params(format!(
                "unknown `op` `{other}` (add, duplicate, remove, move, trim_start, trim_end, speed, offset, ramps, gain, \
                 rename, time_scale, time_scale_mode, spatial, repeat, video_target, video_loop, sub_binding, rename_binding)"
            )));
        }
    })
}

fn selected_kind(world: &mut World, panel: &SeqCurvePanel) -> Option<&'static str> {
    player_of(world, panel)
        .and_then(|e| world.get::<SequencePlayer>(e))
        .and_then(|p| p.sequence().tracks.get(panel.track))
        .map(|t| t.keys.kind())
}

/// `editor.seq_section_edit { op: "rename_binding", from, to }`: the sequence-wide rename.
fn rename_binding_op(
    world: &mut World,
    panel: &mut SeqCurvePanel,
    params: &Value,
) -> Result<Value, RpcError> {
    let name = |field: &str| -> Result<String, RpcError> {
        params
            .get(field)
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| {
                RpcError::invalid_params(format!("`{field}` (a string) is required for `rename_binding`"))
            })
    };
    let (from, to) = (name("from")?, name("to")?);
    let done = rename_player_binding(world, panel, &from, &to).map_err(section_rpc)?;
    let bindings = player_of(world, panel)
        .and_then(|e| world.get::<SequencePlayer>(e))
        .map_or_else(Vec::new, |p| binding_names(p.sequence(), p.library()));
    Ok(json!({
        "applied": true,
        "from": from,
        "to": to,
        "tracks": done.tracks,
        "sounds": done.sounds,
        "videos": done.videos,
        "sub_sections": done.sub_sections,
        "unresolved": done.unresolved,
        "bindings": bindings,
    }))
}

fn seq_section_edit(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mut panel = world.remove_resource::<SeqCurvePanel>().unwrap_or_default();
    if params.get("op").and_then(Value::as_str) == Some("binding_kind") {
        let live = panel.gesture.take();
        let out = select(world, &mut panel, &json!({ "player": params.get("player") }))
            .map_err(rpc)
            .and_then(|()| binding_kind_op(world, &mut panel, params));
        panel.gesture = live;
        world.insert_resource(panel);
        return out;
    }
    if params.get("op").and_then(Value::as_str) == Some("rename_binding") {
        let live = panel.gesture.take();
        let out = select(world, &mut panel, &json!({ "player": params.get("player") }))
            .map_err(rpc)
            .and_then(|()| rename_binding_op(world, &mut panel, params));
        panel.gesture = live;
        world.insert_resource(panel);
        return out;
    }
    // An agent call is its own undo entry, even while a pointer drag is open.
    let live = panel.gesture.take();
    let result = select(world, &mut panel, params).map_err(rpc).and_then(|()| {
        let kind = selected_kind(world, &panel);
        let edit = section_edit_of_params(params, kind)?;
        edit_section(world, &mut panel, edit).map_err(section_rpc)
    });
    let out = result.map(|()| {
        let sections = player_of(world, &panel)
            .and_then(|e| world.get::<SequencePlayer>(e))
            .and_then(|p| p.sequence().tracks.get(panel.track))
            .map_or(json!([]), |t| sections_json(&t.keys));
        json!({ "applied": true, "selected": panel.selected, "sections": sections })
    });
    panel.gesture = live;
    world.insert_resource(panel);
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use regia_framework::sequencer::{Ease, ScalarKey, Sequence, Track};

    use super::*;
    use crate::seq_curve_panel::seq_curve_panel;

    /// Review rc1-rev-editor-k finding 13: when the cap cleared the stills while a decode ran, the
    /// decode's result put its key back into the cleared map. A dropped still stays dropped, and a
    /// pump without media files leaves the queue as it is.
    #[test]
    fn a_decode_finishing_after_the_cap_cleared_does_not_come_back() {
        let mut thumbs = SeqVideoThumbs::default();
        for i in 0..MAX_THUMBS {
            thumbs.request("clip.webm", i as f32);
        }
        let running = thumbs.queued.pop_front().expect("queued");
        let (send, done) = channel();
        thumbs.running = Some((running.clone(), Mutex::new(done)));
        // One more still: the cap clears the map and the queue.
        thumbs.request("other.webm", 0.0);
        assert_eq!(thumbs.stills.len(), 1);
        send.send(Err("decode_failed".to_string())).expect("send");
        thumbs.pump(None);
        assert!(thumbs.running.is_none(), "the finished decode is taken");
        assert!(
            !thumbs.stills.contains_key(&running),
            "the dropped still came back"
        );
        assert_eq!(thumbs.stills.len(), 1);
        assert_eq!(
            thumbs.queued.len(),
            1,
            "a pump without files starts nothing and drops nothing"
        );
    }

    fn clip(name: &str, start: f32, end: f32) -> ClipSection {
        ClipSection {
            clip: name.into(),
            start,
            end,
            offset: 0.0,
            speed: 1.0,
            looping: false,
            blend_in: 0.0,
            blend_out: 0.0,
            weight: 1.0,
        }
    }

    fn world_with_player() -> (World, Entity) {
        let mut world = World::new();
        let sequence = Sequence {
            fps: 10.0,
            tracks: vec![
                Track {
                    name: "fade".into(),
                    target: "ui".into(),
                    property: "opacity".into(),
                    keys: Channel::Float(vec![ScalarKey::new(0.0, 0.0, Ease::Linear)]),
                    enabled: true,
                    locked: false,
                },
                Track {
                    name: "body".into(),
                    target: "hero".into(),
                    property: String::new(),
                    keys: Channel::Clip(vec![
                        clip("walk", 0.0, 2.0),
                        ClipSection {
                            blend_in: 0.5,
                            ..clip("wave", 1.5, 4.0)
                        },
                    ]),
                    enabled: true,
                    locked: false,
                },
                Track {
                    name: "music".into(),
                    target: String::new(),
                    property: String::new(),
                    keys: Channel::Audio(vec![AudioSection {
                        sound: "theme".into(),
                        start: 0.0,
                        end: 4.0,
                        offset: 0.0,
                        volume_db: -6.0,
                        fade_in: 0.0,
                        fade_out: 1.0,
                        bus: None,
                        time_scale: 1.0,
                        spatial: None,
                        time_scale_mode: TimeScaleMode::Varispeed,
                    }]),
                    enabled: true,
                    locked: false,
                },
                Track {
                    name: "fx".into(),
                    target: String::new(),
                    property: String::new(),
                    keys: Channel::Sub(vec![SubSection {
                        sequence: "fx.seq.ron".into(),
                        start: 1.0,
                        end: 2.0,
                        offset: 0.0,
                        speed: 1.0,
                        bindings: BTreeMap::new(),
                        repeat: Default::default(),
                        loop_length: None,
                    }]),
                    enabled: true,
                    locked: false,
                },
            ],
            ..Sequence::default()
        };
        sequence.validate().expect("valid fixture");
        let player = world.spawn(SequencePlayer::new(sequence)).id();
        (world, player)
    }

    fn code(e: RpcError) -> String {
        e.data
            .and_then(|d| d["code"].as_str().map(str::to_owned))
            .unwrap_or_default()
    }

    fn clips(world: &World, player: Entity) -> Vec<ClipSection> {
        let p = world.get::<SequencePlayer>(player).expect("player");
        let Channel::Clip(list) = &p.sequence().tracks[1].keys else {
            panic!("clip track")
        };
        list.clone()
    }

    #[test]
    fn every_wire_name_reaches_its_handler() {
        let (mut world, _) = world_with_player();
        let table = methods();
        let mut call = |name: &str, params: Value| {
            let (_, handler) = table.iter().find(|(s, _)| s.name == name).expect(name);
            handler(&mut world, &params)
        };
        call("editor.seq_sections_state", json!({ "track": "body" })).expect("state");
        assert!(call("editor.seq_section_edit", json!({ "op": "explode" })).is_err());
    }

    /// A numeric `track` is not an index. The call is `seq_curve_bad_track` and the previous
    /// track's sections stay.
    #[test]
    fn a_numeric_track_does_not_edit_the_previous_section_track() {
        let (mut world, player) = world_with_player();
        seq_sections_state(&mut world, &json!({ "track": "fade" })).expect("select fade");
        let before = clips(&world, player);
        let err = seq_section_edit(&mut world, &json!({ "track": 1, "op": "remove", "index": 0 }))
            .expect_err("number");
        assert_eq!(code(err), "seq_curve_bad_track");
        assert_eq!(clips(&world, player), before);
        assert_eq!(world.resource::<crate::seq_curve_panel::SeqCurvePanel>().track, 0);
    }

    #[test]
    fn state_lists_sections_with_rows_and_fields() {
        let (mut world, player) = world_with_player();
        let state = seq_sections_state(&mut world, &json!({ "track": "body" })).expect("state");
        assert_eq!(state["player"], json!(player.to_bits()));
        assert_eq!(state["kind"], json!("Clip"));
        assert_eq!(state["rows"], json!(2), "the overlapping wave takes a second row");
        assert_eq!(state["sections"][1]["row"], json!(1));
        assert_eq!(state["sections"][1]["label"], json!("wave"));
        assert_eq!(state["sections"][1]["rise"], json!(0.5));
        assert_eq!(state["sections"][1]["section"]["clip"], json!("wave"));
        let audio = seq_sections_state(&mut world, &json!({ "track": "music" })).expect("audio");
        assert_eq!(audio["sections"][0]["section"]["volume_db"], json!(-6.0));
        let keyed = seq_sections_state(&mut world, &json!({ "track": "fade" })).expect("keyed");
        assert_eq!(keyed["sections"], json!([]));
        assert_eq!(keyed["rows"], json!(0));
    }

    #[test]
    fn edits_reach_the_player_undo_and_refused_ones_change_nothing() {
        let (mut world, player) = world_with_player();
        let edit = |world: &mut World, params: Value| seq_section_edit(world, &params);
        let out = edit(
            &mut world,
            json!({ "track": "body", "op": "trim_start", "index": 1, "start": 2.0 }),
        )
        .expect("trim");
        assert_eq!(out["selected"], json!(1));
        let wave = &clips(&world, player)[1];
        assert_eq!((wave.start, wave.offset, wave.blend_in), (2.0, 0.5, 0.5));
        edit(
            &mut world,
            json!({ "track": "body", "op": "add", "section": { "clip": "idle", "start": 4.0, "end": 5.0 } }),
        )
        .expect("add");
        assert_eq!(clips(&world, player).len(), 3);
        assert_eq!(clips(&world, player)[2].speed, 1.0, "defaults as in the file");
        edit(
            &mut world,
            json!({ "track": "music", "op": "gain", "index": 0, "value": -3.0 }),
        )
        .expect("gain");
        edit(
            &mut world,
            json!({ "track": "fx", "op": "speed", "index": 0, "speed": 2.0 }),
        )
        .expect("speed");
        let before = world.get::<SequencePlayer>(player).expect("p").sequence().clone();
        for (params, expected) in [
            (
                json!({ "track": "body", "op": "remove", "index": 9 }),
                "seq_section_bad_index",
            ),
            (
                json!({ "track": "music", "op": "speed", "index": 0, "speed": 2.0 }),
                "seq_section_wrong_kind",
            ),
            (
                json!({ "track": "music", "op": "remove", "index": 0 }),
                "seq_section_last",
            ),
            (
                json!({ "track": "body", "op": "trim_end", "index": 0, "end": -1.0 }),
                "seq_section_out_of_range",
            ),
            (
                json!({ "track": "fade", "op": "remove", "index": 0 }),
                "seq_section_not_sections",
            ),
            (
                json!({ "track": "fade", "op": "add", "section": {} }),
                "seq_section_not_sections",
            ),
            (
                json!({ "track": "nope", "op": "remove", "index": 0 }),
                "seq_curve_bad_track",
            ),
        ] {
            let e = edit(&mut world, params.clone()).expect_err("refused");
            assert_eq!(code(e), expected, "{params}");
        }
        assert!(
            edit(&mut world, json!({ "track": "body", "op": "move", "index": 0 })).is_err(),
            "no start"
        );
        assert!(
            edit(
                &mut world,
                json!({ "track": "body", "op": "add", "section": { "start": 1.0 } })
            )
            .is_err(),
            "no clip name"
        );
        assert_eq!(
            world.get::<SequencePlayer>(player).expect("p").sequence(),
            &before,
            "refused edits change nothing"
        );
        let undo = world.resource::<UndoStack>().undo_depth();
        assert_eq!(undo, 4, "one entry per call");
        for _ in 0..undo {
            assert!(crate::command::undo_world(&mut world));
        }
        assert_eq!(
            clips(&world, player)[1],
            ClipSection {
                blend_in: 0.5,
                ..clip("wave", 1.5, 4.0)
            }
        );
    }

    /// `editor.seq_section_edit { op: "sub_binding" }` (w8-editor-smalls-z): a set writes the
    /// pair into that one sub-sequence section's `bindings` map and no other section's, a remove
    /// takes it out, and one undo step restores the sequence's bytes.
    #[test]
    fn sub_binding_edits_one_section_and_undoes_in_one_step() {
        let (mut world, player) = world_with_player();
        // Two sub sections on the fx track: index 0 and 1.
        {
            let mut p = world.get_mut::<SequencePlayer>(player).expect("player");
            let mut sequence = p.sequence().clone();
            let Some(track) = sequence.tracks.iter_mut().find(|t| t.name == "fx") else {
                panic!("fx track")
            };
            let Channel::Sub(list) = &mut track.keys else {
                panic!("sub track")
            };
            list.push(SubSection::new("fx2.seq.ron", 3.0, 4.0));
            p.replace_sequence(sequence);
        }
        let seq_text = |world: &World| {
            let p = world.get::<SequencePlayer>(player).expect("player");
            p.sequence().to_ron().expect("seq ron")
        };
        let original = seq_text(&world);

        let edit = |world: &mut World, params: Value| seq_section_edit(world, &params);
        let out = edit(
            &mut world,
            json!({ "track": "fx", "op": "sub_binding", "index": 0, "from": "lamp", "to": "hero" }),
        )
        .expect("set");
        assert_eq!(out["applied"], json!(true));
        assert_eq!(out["selected"], json!(0));
        let subs = |world: &World| -> Vec<SubSection> {
            let p = world.get::<SequencePlayer>(player).expect("player");
            let track = p.sequence().tracks.iter().find(|t| t.name == "fx").expect("fx");
            let Channel::Sub(list) = &track.keys else {
                panic!("sub")
            };
            list.clone()
        };
        assert_eq!(
            subs(&world)[0].bindings.get("lamp").map(String::as_str),
            Some("hero"),
            "the section's map holds the pair"
        );
        assert!(
            subs(&world)[1].bindings.is_empty(),
            "the other section is untouched"
        );

        // The saved bytes carry the pair; the section JSON reports it.
        let with_pair = seq_text(&world);
        assert!(with_pair.contains("lamp"), "{with_pair}");
        let state = edit(
            &mut world,
            json!({ "track": "fx", "op": "sub_binding", "index": 0, "from": "bell", "to": "hero" }),
        )
        .expect("second set");
        assert_eq!(state["sections"][0]["section"]["bindings"]["lamp"], json!("hero"));

        // Remove, and the refusal codes.
        edit(
            &mut world,
            json!({ "track": "fx", "op": "sub_binding", "index": 1, "from": "lamp", "to": null }),
        )
        .expect_err("section 1 has no such binding");
        for (params, expected) in [
            (
                json!({ "track": "fx", "op": "sub_binding", "index": 0, "from": "lamp", "to": "" }),
                "seq_binding_empty",
            ),
            (
                json!({ "track": "fx", "op": "sub_binding", "index": 0, "from": "lamp", "to": "lamp" }),
                "seq_binding_same",
            ),
            (
                json!({ "track": "fx", "op": "sub_binding", "index": 9, "from": "lamp", "to": "hero" }),
                "seq_section_bad_index",
            ),
            (
                json!({ "track": "body", "op": "sub_binding", "index": 0, "from": "lamp", "to": "hero" }),
                "seq_section_wrong_kind",
            ),
            (
                json!({ "track": "fx", "op": "sub_binding", "index": 0, "from": "nobody", "to": null }),
                "seq_binding_unknown",
            ),
        ] {
            let e = edit(&mut world, params.clone()).expect_err("refused");
            assert_eq!(code(e), expected, "{params}");
        }

        // One undo step per call: undoing the sets restores the original bytes.
        assert!(crate::command::undo_world(&mut world));
        assert!(
            !subs(&world)[0].bindings.contains_key("bell"),
            "the second set (bell) is undone"
        );
        assert_eq!(
            subs(&world)[0].bindings.get("lamp").map(String::as_str),
            Some("hero"),
            "the first set (lamp) is still there"
        );
        assert!(crate::command::undo_world(&mut world));
        assert_eq!(seq_text(&world), original, "undo restores the bytes");
    }

    #[test]
    fn drag_edits_snap_to_frames_and_keep_the_grab_offset() {
        let body = drag_edit(SectionPick::Body(1), 2.0, 1.5, 3.0, 2.53, 10.0, true);
        assert_eq!(body, SectionEdit::Move { index: 1, start: 2.0 });
        let free = drag_edit(SectionPick::Body(1), 2.0, 1.5, 3.0, 2.53, 10.0, false);
        let SectionEdit::Move { start, .. } = free else {
            panic!("move")
        };
        assert!((start - 2.03).abs() < 1.0e-5, "{start}");
        assert_eq!(
            drag_edit(SectionPick::Start(0), 0.0, 0.0, 2.0, -3.0, 10.0, true),
            SectionEdit::TrimStart { index: 0, start: 0.0 },
            "never before zero"
        );
        // Pressed a little left of the end edge (2.98 for an end at 3.0): the edge moves by the
        // pointer's travel and does not jump to the pointer.
        assert_eq!(
            drag_edit(SectionPick::End(0), 2.98, 0.0, 3.0, 3.48, 10.0, true),
            SectionEdit::TrimEnd { index: 0, end: 3.5 }
        );
        assert_eq!(
            drag_edit(SectionPick::Start(0), 1.03, 1.0, 3.0, 1.53, 10.0, true),
            SectionEdit::TrimStart { index: 0, start: 1.5 }
        );
    }

    #[test]
    fn picking_finds_edges_bodies_and_rows() {
        let channel = Channel::Clip(vec![clip("walk", 0.0, 2.0), clip("wave", 1.0, 3.0)]);
        let spans = section_spans(&channel).expect("spans");
        let mut view = View2d {
            rect: egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(300.0, 100.0)),
            ..View2d::default()
        };
        view.zoom_to_fit([0.0, 0.0, 3.0, 1.0], 0.0);
        let row_y = |row: f32| RULER_HEIGHT + row * ROW_HEIGHT + ROW_HEIGHT * 0.5;
        let x = |t: f32| view.to_screen(egui::vec2(t, 0.0)).x;
        assert_eq!(
            pick_section(&spans, &view, egui::pos2(x(1.0), row_y(0.0))),
            Some(SectionPick::Body(0))
        );
        assert_eq!(
            pick_section(&spans, &view, egui::pos2(x(2.0) - 1.0, row_y(0.0))),
            Some(SectionPick::End(0))
        );
        assert_eq!(
            pick_section(&spans, &view, egui::pos2(x(1.0) + 2.0, row_y(1.0))),
            Some(SectionPick::Start(1))
        );
        assert_eq!(
            pick_section(&spans, &view, egui::pos2(x(2.5), row_y(0.0))),
            None,
            "row 0 is empty there"
        );
        assert_eq!(
            pick_section(&spans, &view, egui::pos2(x(2.5), row_y(1.0))),
            Some(SectionPick::Body(1))
        );
    }

    /// Runs the real panel in an egui context with synthetic pointer events.
    struct Pointer {
        ctx: egui::Context,
        time: f64,
    }

    impl Pointer {
        fn new() -> Self {
            Self {
                ctx: egui::Context::default(),
                time: 0.0,
            }
        }

        fn frame(&mut self, world: &mut World, events: Vec<egui::Event>) {
            self.time += 0.1;
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(800.0, 600.0),
                )),
                events,
                time: Some(self.time),
                ..Default::default()
            };
            let mut output = self.ctx.run_ui(raw, |ui| {
                egui::CentralPanel::default().show(ui, |ui| seq_curve_panel(ui, world));
            });
            output.textures_delta.clear();
        }

        fn drag(&mut self, world: &mut World, from: egui::Pos2, path: &[egui::Pos2]) {
            let button = |pos, pressed| egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::default(),
            };
            self.frame(world, vec![egui::Event::PointerMoved(from)]);
            self.frame(world, vec![button(from, true)]);
            for &to in path {
                self.frame(world, vec![egui::Event::PointerMoved(to)]);
            }
            let end = path.last().copied().unwrap_or(from);
            self.frame(world, vec![button(end, false)]);
            self.frame(world, vec![]);
        }
    }

    /// The screen point of time `t` on row `row` of the strip.
    fn at(world: &World, t: f32, row: f32) -> egui::Pos2 {
        let view = &world.resource::<SeqSectionStrip>().view;
        egui::pos2(
            view.to_screen(egui::vec2(t, 0.0)).x,
            view.rect.top() + RULER_HEIGHT + row * ROW_HEIGHT + ROW_HEIGHT * 0.5,
        )
    }

    fn on_track(world: &mut World, track: usize) {
        world.insert_resource(SeqCurvePanel {
            track,
            ..SeqCurvePanel::default()
        });
    }

    #[test]
    fn a_pointer_drag_moves_a_bar_snapped_as_one_undo_entry() {
        let (mut world, player) = world_with_player();
        on_track(&mut world, 1);
        let mut pointer = Pointer::new();
        pointer.frame(&mut world, vec![]);
        let from = at(&world, 1.0, 0.0);
        let path = [at(&world, 1.3, 0.0), at(&world, 1.52, 0.0)];
        pointer.drag(&mut world, from, &path);
        let walk = &clips(&world, player)[0];
        assert_eq!(
            (walk.start, walk.end),
            (0.5, 2.5),
            "moved by the drag, snapped to 10 fps"
        );
        assert_eq!(world.resource::<SeqCurvePanel>().selected, Some(0));
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 1, "one entry");
        assert!(crate::command::undo_world(&mut world));
        assert_eq!(clips(&world, player)[0], clip("walk", 0.0, 2.0));
    }

    #[test]
    fn a_pointer_on_an_edge_trims_and_on_empty_space_scrubs() {
        let (mut world, player) = world_with_player();
        on_track(&mut world, 1);
        let mut pointer = Pointer::new();
        pointer.frame(&mut world, vec![]);
        // The wave's end edge, on row 1.
        let edge = at(&world, 4.0, 1.0) - egui::vec2(1.0, 0.0);
        let path = [at(&world, 3.5, 1.0), at(&world, 3.0, 1.0)];
        pointer.drag(&mut world, edge, &path);
        let wave = &clips(&world, player)[1];
        assert_eq!((wave.start, wave.end), (1.5, 3.0));
        // Empty space on row 0 after the walk.
        let (empty, path) = (at(&world, 3.0, 0.0), [at(&world, 3.2, 0.0), at(&world, 3.5, 0.0)]);
        pointer.drag(&mut world, empty, &path);
        let time = world.get::<SequencePlayer>(player).expect("player").time();
        assert!((time - 3.5).abs() < 0.02, "scrubbed: {time}");
        assert_eq!(
            world.resource::<UndoStack>().undo_depth(),
            1,
            "a scrub is not an edit"
        );
    }

    /// ADR-2862, through the real panel: the strip draws a video track, uploads its section's still, a
    /// drag of the bar moves the section as one undo entry, and the source's frames stay on the
    /// section (a move keeps the offset).
    #[test]
    fn the_strip_draws_a_video_sections_still_and_a_drag_moves_it() {
        use regia_media::mux::PatternClip;

        let mut world = World::new();
        let mut files = MediaFiles::default();
        files.insert("movies/a.webm", PatternClip::new(16, 8, 60, 30).to_webm());
        world.insert_resource(files);
        world.insert_resource(UndoStack::new());
        let section = VideoSection {
            offset: 0.5,
            ..VideoSection::new("movies/a.webm", 1.0, 3.0)
        };
        let sequence = Sequence {
            fps: 10.0,
            tracks: vec![Track {
                name: "wall".into(),
                target: String::new(),
                property: String::new(),
                keys: Channel::Video(vec![section]),
                enabled: true,
                locked: false,
            }],
            ..Sequence::default()
        };
        let player = world.spawn(SequencePlayer::new(sequence)).id();
        seq_sections_state(&mut world, &json!({ "track": "wall" })).expect("selects");
        let mut pointer = Pointer::new();
        let mut frames = 0u64;
        loop {
            pointer.frame(&mut world, vec![]);
            let thumbs = world.resource::<SeqVideoThumbs>();
            let uploaded = thumbs
                .stills
                .values()
                .any(|s| matches!(s, SeqThumb::Ready { texture: Some(_), .. }));
            if uploaded {
                break;
            }
            frames += 1;
            assert!(frames < 5_000_000, "the still never reached the strip");
            std::thread::yield_now();
        }
        let body = at(&world, 2.0, 0.0);
        let path = [at(&world, 2.5, 0.0), at(&world, 3.0, 0.0)];
        pointer.drag(&mut world, body, &path);
        let p = world.get::<SequencePlayer>(player).expect("player");
        let Channel::Video(list) = &p.sequence().tracks[0].keys else {
            panic!("a video track")
        };
        assert_eq!((list[0].start, list[0].end, list[0].offset), (2.0, 4.0, 0.5));
        assert_eq!(
            world.resource::<UndoStack>().undo_depth(),
            1,
            "one gesture, one entry"
        );
        let mut undo = world.remove_resource::<UndoStack>().expect("stack");
        assert!(undo.undo(&mut world));
        world.insert_resource(undo);
        let p = world.get::<SequencePlayer>(player).expect("player");
        let Channel::Video(list) = &p.sequence().tracks[0].keys else {
            panic!("a video track")
        };
        assert_eq!((list[0].start, list[0].end), (1.0, 3.0));
    }

    /// Review finding (ADR-2135): every drag frame edits the track as the drag found it, so a drag
    /// that shortens a section below its ramps and comes back restores them, and a start trim
    /// dragged back to where it began leaves the offset exactly where it was.
    #[test]
    fn a_drag_back_restores_ramps_and_offsets_exactly() {
        let (mut world, player) = world_with_player();
        on_track(&mut world, 1);
        let mut pointer = Pointer::new();
        pointer.frame(&mut world, vec![]);
        // The wave (1.5..4.0, blend in 0.5): its end edge in to 1.8 and back out to 4.0.
        let edge = at(&world, 4.0, 1.0) - egui::vec2(1.0, 0.0);
        let back = edge + egui::vec2(0.0, 0.0);
        let path = [
            at(&world, 3.0, 1.0),
            at(&world, 1.8, 1.0),
            at(&world, 3.0, 1.0),
            back,
        ];
        pointer.drag(&mut world, edge, &path);
        assert_eq!(
            clips(&world, player)[1],
            ClipSection {
                blend_in: 0.5,
                ..clip("wave", 1.5, 4.0)
            },
            "the ramps came back with the length"
        );
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 1);
        // The music's start edge right and back, on many frames.
        on_track(&mut world, 2);
        pointer.frame(&mut world, vec![]);
        let start = at(&world, 0.0, 0.0) + egui::vec2(1.0, 0.0);
        let mut path: Vec<egui::Pos2> = (1..=12)
            .map(|i| start + egui::vec2(7.0 * i as f32, 0.0))
            .collect();
        path.extend((0..12).rev().map(|i| start + egui::vec2(7.0 * i as f32, 0.0)));
        pointer.drag(&mut world, start, &path);
        let p = world.get::<SequencePlayer>(player).expect("player");
        let Channel::Audio(sounds) = &p.sequence().tracks[2].keys else {
            panic!("audio")
        };
        assert_eq!((sounds[0].start, sounds[0].offset), (0.0, 0.0), "exactly back");
    }

    #[test]
    fn rename_changes_what_a_section_plays() {
        let (mut world, player) = world_with_player();
        let out = seq_section_edit(
            &mut world,
            &json!({ "track": "body", "op": "rename", "index": 0, "name": "run" }),
        )
        .expect("rename");
        assert_eq!(out["sections"][0]["label"], json!("run"));
        assert_eq!(clips(&world, player)[0].clip, "run");
        let e = seq_section_edit(
            &mut world,
            &json!({ "track": "body", "op": "rename", "index": 0, "name": "" }),
        )
        .expect_err("empty");
        assert_eq!(code(e), "seq_section_out_of_range");
        assert!(
            seq_section_edit(
                &mut world,
                &json!({ "track": "body", "op": "rename", "index": 0 })
            )
            .is_err(),
            "no name"
        );
        assert_eq!(clips(&world, player)[0].clip, "run");
    }

    /// Review finding (ADR-2135): an agent call during a live pointer drag is its own undo entry.
    /// ADR-2862: the strip lays a video track out, edits it through the agent method with undo, and
    /// reports the first-frame stills.
    #[test]
    fn video_sections_edit_with_undo_and_show_their_first_frame() {
        use regia_media::mux::PatternClip;

        let mut world = World::new();
        let mut files = MediaFiles::default();
        files.insert("movies/a.webm", PatternClip::new(160, 80, 60, 30).to_webm());
        world.insert_resource(files);
        let sequence = Sequence {
            tracks: vec![Track {
                name: "wall".into(),
                target: String::new(),
                property: String::new(),
                keys: Channel::Video(vec![
                    VideoSection {
                        offset: 0.5,
                        ..VideoSection::new("movies/a.webm", 1.0, 3.0)
                    },
                    VideoSection::new("movies/gone.webm", 2.0, 4.0),
                ]),
                enabled: true,
                locked: false,
            }],
            ..Sequence::default()
        };
        let player = world.spawn(SequencePlayer::new(sequence)).id();
        world.insert_resource(UndoStack::new());
        let select = json!({ "track": "wall" });
        let state = seq_sections_state(&mut world, &select).expect("state");
        assert_eq!(state["kind"], "Video");
        assert_eq!(state["rows"], json!(2), "overlapping sections: {state}");
        assert_eq!(state["sections"][0]["section"]["video"], "movies/a.webm");
        assert_eq!(state["sections"][0]["rise"], json!(0.0));
        // The stills arrive off the frame. A counted loop, no timing.
        let mut polls = 0u64;
        let state = loop {
            let state = seq_sections_state(&mut world, &select).expect("state");
            if state["thumbnails"]
                .as_array()
                .expect("list")
                .iter()
                .all(|t| t["state"] != "pending")
            {
                break state;
            }
            polls += 1;
            assert!(polls < 50_000_000, "the decode never finished: {state}");
            std::thread::yield_now();
        };
        let first = &state["thumbnails"][0];
        assert_eq!(first["state"], "ready", "{state}");
        assert_eq!(
            first["frame"],
            json!(15),
            "0.5 s into a 30 fps clip: the section's first picture"
        );
        assert_eq!(
            (first["width"].as_u64(), first["height"].as_u64()),
            (Some(128), Some(64))
        );
        assert_eq!(state["thumbnails"][1]["state"], "failed");
        assert_eq!(state["thumbnails"][1]["code"], "source_not_found");

        let edit = |world: &mut World, params: Value| seq_section_edit(world, &params);
        let video = |world: &World, index: usize| -> VideoSection {
            let p = world.get::<SequencePlayer>(player).expect("player");
            let Channel::Video(list) = &p.sequence().tracks[0].keys else {
                panic!("video track")
            };
            list[index].clone()
        };
        edit(
            &mut world,
            json!({ "track": "wall", "op": "trim_start", "index": 0, "start": 2.0 }),
        )
        .expect("trim");
        assert_eq!(video(&world, 0).offset, 1.5, "the frames stay on the clock");
        edit(
            &mut world,
            json!({ "track": "wall", "op": "gain", "index": 0, "value": -9.0 }),
        )
        .expect("gain");
        assert_eq!(video(&world, 0).volume_db, -9.0);
        edit(
            &mut world,
            json!({ "track": "wall", "op": "video_loop", "index": 0, "looping": true }),
        )
        .expect("loop");
        assert!(video(&world, 0).looping);
        edit(
            &mut world,
            json!({ "track": "wall", "op": "video_target", "index": 0,
                    "target": { "Screen": { "binding": "tv", "slot": "Albedo" } } }),
        )
        .expect("target");
        assert_eq!(
            video(&world, 0).target,
            VideoTarget::Screen {
                binding: "tv".into(),
                slot: VideoScreenSlot::Albedo
            }
        );
        edit(
            &mut world,
            json!({ "track": "wall", "op": "rename", "index": 1, "name": "movies/a.webm" }),
        )
        .expect("rename");
        assert_eq!(video(&world, 1).video, "movies/a.webm");
        edit(
            &mut world,
            json!({ "track": "wall", "op": "add", "section": { "video": "movies/a.webm", "start": 5.0, "end": 6.0 } }),
        )
        .expect("add");
        assert_eq!(video(&world, 2).target, VideoTarget::default());
        let code_of = |e: RpcError| code(e);
        assert_eq!(
            code_of(
                edit(
                    &mut world,
                    json!({ "track": "wall", "op": "speed", "index": 0, "speed": 2.0 })
                )
                .expect_err("no speed")
            ),
            "seq_section_wrong_kind"
        );
        assert_eq!(
            code_of(
                edit(
                    &mut world,
                    json!({ "track": "wall", "op": "video_target", "index": 0, "target": { "Player": "" } })
                )
                .expect_err("empty binding")
            ),
            "seq_section_out_of_range"
        );
        assert!(
            edit(
                &mut world,
                json!({ "track": "wall", "op": "video_target", "index": 0, "target": { "Nope": 1 } })
            )
            .is_err()
        );
        assert!(
            edit(
                &mut world,
                json!({ "track": "wall", "op": "video_loop", "index": 0 })
            )
            .is_err()
        );
        assert!(video(&world, 0).looping, "refused edits change nothing");
        // Every edit undoes.
        for _ in 0..6 {
            let mut undo = world.remove_resource::<UndoStack>().expect("stack");
            assert!(undo.undo(&mut world), "an entry per edit");
            world.insert_resource(undo);
        }
        assert_eq!(video(&world, 0).offset, 0.5);
        assert_eq!(video(&world, 0).target, VideoTarget::default());
        assert_eq!(video(&world, 1).video, "movies/gone.webm");
    }

    #[test]
    fn a_still_shrinks_to_the_widest_a_bar_keeps() {
        let wide = VideoThumbnail {
            width: 256,
            height: 128,
            rgba: vec![9; 256 * 128 * 4],
            frame: 3,
        };
        let small = shrink_still(wide);
        assert_eq!((small.width, small.height, small.frame), (128, 64, 3));
        assert_eq!(small.rgba.len(), 128 * 64 * 4);
        let narrow = VideoThumbnail {
            width: 16,
            height: 8,
            rgba: vec![1; 16 * 8 * 4],
            frame: 0,
        };
        assert_eq!(shrink_still(narrow.clone()), narrow, "small enough already");
    }

    #[test]
    fn agent_edits_during_a_drag_are_their_own_undo_entries() {
        let (mut world, _) = world_with_player();
        world.insert_resource(SeqCurvePanel {
            gesture: Some(99),
            ..SeqCurvePanel::default()
        });
        for start in [0.5, 1.0] {
            seq_section_edit(
                &mut world,
                &json!({ "track": "fx", "op": "move", "index": 0, "start": start }),
            )
            .expect("move");
        }
        assert_eq!(
            world.resource::<UndoStack>().undo_depth(),
            2,
            "two entries, not one merged"
        );
        assert_eq!(
            world.resource::<SeqCurvePanel>().gesture,
            Some(99),
            "the drag's gesture stays"
        );
    }

    #[test]
    fn the_strip_draws_every_section_kind_and_survives_a_shrunk_track() {
        let (mut world, player) = world_with_player();
        let mut pointer = Pointer::new();
        for track in [1, 2, 3, 0] {
            on_track(&mut world, track);
            world.resource_mut::<SeqCurvePanel>().selected = Some(0);
            pointer.frame(&mut world, vec![]);
        }
        // A selection past the end after an outside edit is dropped, not drawn.
        on_track(&mut world, 1);
        world.resource_mut::<SeqCurvePanel>().selected = Some(1);
        pointer.frame(&mut world, vec![]);
        {
            let mut p = world.get_mut::<SequencePlayer>(player).expect("player");
            let mut sequence = p.sequence().clone();
            sequence.tracks[1].keys = Channel::Clip(vec![clip("walk", 0.0, 2.0)]);
            p.replace_sequence(sequence);
        }
        pointer.frame(&mut world, vec![]);
        assert_eq!(world.resource::<SeqCurvePanel>().selected, None);
    }

    /// A rename (ADR-2405) keeps every reference: tracks, a spatial sound and the player's binding
    /// table move to the new name in one undo entry, and undo brings the old name back.
    #[test]
    fn a_binding_rename_keeps_every_reference_and_undo_restores_the_old_name() {
        use regia_framework::sequencer::{Binding, SequenceBindings};
        let (mut world, player) = world_with_player();
        let hero = world.spawn_empty().id();
        world
            .entity_mut(player)
            .insert(SequenceBindings::default().with("hero", Binding::Entity(hero)));
        let edit = |world: &mut World, params: Value| seq_section_edit(world, &params);
        // Place the music on hero, and make the fx loop (v3 ops through the agent method).
        edit(
            &mut world,
            json!({ "track": "music", "op": "spatial", "index": 0,
                "spatial": { "source": { "Binding": "hero" }, "max_distance": 30.0 } }),
        )
        .expect("spatial");
        edit(
            &mut world,
            json!({ "track": "music", "op": "time_scale", "index": 0, "scale": 1.5 }),
        )
        .expect("time scale");
        edit(
            &mut world,
            json!({ "track": "fx", "op": "repeat", "index": 0, "repeat": "count", "count": 3, "loop_length": 0.25 }),
        )
        .expect("repeat");
        let undo_before = world.resource::<UndoStack>().undo_depth();
        let original = world
            .get::<SequencePlayer>(player)
            .expect("player")
            .sequence()
            .clone();
        let out = edit(
            &mut world,
            json!({ "op": "rename_binding", "from": "hero", "to": "guard" }),
        )
        .expect("rename");
        assert_eq!(
            (out["tracks"].as_u64(), out["sounds"].as_u64()),
            (Some(1), Some(1))
        );
        assert_eq!(out["bindings"], json!(["guard", "ui"]));
        assert_eq!(
            world.resource::<UndoStack>().undo_depth(),
            undo_before + 1,
            "one undo entry"
        );
        let renamed = world
            .get::<SequencePlayer>(player)
            .expect("player")
            .sequence()
            .clone();
        assert_eq!(renamed.tracks[1].target, "guard");
        let Channel::Audio(sounds) = &renamed.tracks[2].keys else {
            unreachable!()
        };
        assert_eq!(
            sounds[0].spatial.as_ref().map(|s| &s.source),
            Some(&SoundSource::Binding("guard".into()))
        );
        assert_eq!(sounds[0].time_scale, 1.5);
        let Channel::Sub(subs) = &renamed.tracks[3].keys else {
            unreachable!()
        };
        assert_eq!(
            (subs[0].repeat, subs[0].loop_length),
            (SubRepeat::Count(3), Some(0.25))
        );
        let table = world.get::<SequenceBindings>(player).expect("bindings");
        assert_eq!(
            table.get("guard"),
            Some(Binding::Entity(hero)),
            "the entity stays bound"
        );
        assert_eq!(table.get("hero"), None);
        // Nothing else changed.
        let mut back = renamed.clone();
        back.tracks[1].target = "hero".into();
        if let Channel::Audio(sounds) = &mut back.tracks[2].keys
            && let Some(spatial) = sounds[0].spatial.as_mut()
        {
            spatial.source = SoundSource::Binding("hero".into());
        }
        assert_eq!(back, original);
        // Undo: the old name and the old table, in one step.
        assert!(crate::command::undo_world(&mut world));
        assert_eq!(
            world.get::<SequencePlayer>(player).expect("player").sequence(),
            &original
        );
        let table = world.get::<SequenceBindings>(player).expect("bindings");
        assert_eq!(table.get("hero"), Some(Binding::Entity(hero)));
        assert_eq!(table.get("guard"), None);
        // Refusals carry their codes and change nothing.
        for (params, expected) in [
            (
                json!({ "op": "rename_binding", "from": "hero", "to": "ui" }),
                "seq_binding_in_use",
            ),
            (
                json!({ "op": "rename_binding", "from": "nobody", "to": "x" }),
                "seq_binding_unknown",
            ),
            (
                json!({ "op": "rename_binding", "from": "hero", "to": "hero" }),
                "seq_binding_same",
            ),
            (
                json!({ "op": "rename_binding", "from": "hero", "to": "" }),
                "seq_binding_empty",
            ),
        ] {
            assert_eq!(code(edit(&mut world, params).expect_err("refused")), expected);
        }
        world
            .get_mut::<SequenceBindings>(player)
            .expect("bindings")
            .bind("spare", Binding::Entity(hero));
        assert_eq!(
            code(
                edit(
                    &mut world,
                    json!({ "op": "rename_binding", "from": "hero", "to": "spare" })
                )
                .expect_err("taken")
            ),
            "seq_binding_table_in_use"
        );
        assert!(edit(&mut world, json!({ "op": "rename_binding", "from": "hero" })).is_err());
        assert_eq!(
            world.get::<SequencePlayer>(player).expect("player").sequence(),
            &original
        );
        // The v3 ops refuse the wrong kinds.
        assert_eq!(
            code(
                edit(
                    &mut world,
                    json!({ "track": "body", "op": "time_scale", "index": 0, "scale": 2.0 })
                )
                .expect_err("clip")
            ),
            "seq_section_wrong_kind"
        );
        assert_eq!(
            code(
                edit(
                    &mut world,
                    json!({ "track": "music", "op": "repeat", "index": 0, "repeat": "once" })
                )
                .expect_err("audio")
            ),
            "seq_section_wrong_kind"
        );
        assert!(
            edit(
                &mut world,
                json!({ "track": "fx", "op": "repeat", "index": 0, "repeat": "often" })
            )
            .is_err()
        );
        assert!(
            edit(
                &mut world,
                json!({ "track": "music", "op": "spatial", "index": 0, "spatial": 3 })
            )
            .is_err()
        );
        // `null` makes the sound flat again.
        edit(
            &mut world,
            json!({ "track": "music", "op": "spatial", "index": 0, "spatial": null }),
        )
        .expect("flat");
        let state = seq_sections_state(&mut world, &json!({ "track": "music" })).expect("state");
        assert!(
            state["sections"][0]["section"].get("spatial").is_none(),
            "{state}"
        );
        assert_eq!(
            state["bindings"],
            json!(["hero", "ui"]),
            "the sequence's names, not the table's"
        );
    }

    /// ADR-2948: the binding kind op makes a binding spawned and back, one undo entry each, and
    /// refuses an invalid prefab with `seq_spawnable_invalid`.
    #[test]
    fn binding_kind_switches_a_binding_to_a_spawnable_and_back_with_undo() {
        let (mut world, player) = world_with_player();
        let edit = |world: &mut World, params: Value| seq_section_edit(world, &params);
        let out = edit(
            &mut world,
            json!({ "op": "binding_kind", "binding": "hero", "kind": "spawnable",
                "prefab": "actors/hero.prefab.ron", "start": 0.5, "end": 2.0, "keep_on_stop": true }),
        )
        .expect("spawnable");
        assert_eq!(out["spawnables"][0]["binding"], "hero");
        assert_eq!(out["spawnables"][0]["keep_on_stop"], true);
        let spawn = |world: &World| {
            world
                .get::<SequencePlayer>(player)
                .expect("player")
                .sequence()
                .spawnables
                .clone()
        };
        assert_eq!(spawn(&world)[0].end, Some(2.0));
        assert_eq!(
            code(
                edit(
                    &mut world,
                    json!({ "op": "binding_kind", "binding": "hero", "kind": "spawnable", "prefab": "../x.prefab.ron" }),
                )
                .expect_err("escape")
            ),
            "seq_spawnable_invalid"
        );
        assert_eq!(
            spawn(&world)[0].prefab,
            "actors/hero.prefab.ron",
            "a refusal changes nothing"
        );
        edit(
            &mut world,
            json!({ "op": "binding_kind", "binding": "hero", "kind": "bound" }),
        )
        .expect("bound");
        assert!(spawn(&world).is_empty());
        let mut undo = world.remove_resource::<UndoStack>().expect("stack");
        assert!(undo.undo(&mut world));
        world.insert_resource(undo);
        assert_eq!(spawn(&world).len(), 1, "undo brings the spawnable back");
        let mut sequence = Sequence::default();
        assert!(
            apply_binding_kind(&mut sequence, "x", None).is_err(),
            "not a spawnable"
        );
        assert!(apply_binding_kind(&mut sequence, "x", Some(Spawnable::new("x", "", 0.0))).is_err());
    }

    #[test]
    fn the_prefab_picker_lists_project_prefabs_sorted_and_relative() {
        let root = regia_test_utils::unique_temp_dir("seq-prefab-picker");
        std::fs::create_dir_all(root.join("actors/props")).expect("dirs");
        std::fs::create_dir_all(root.join(".regia")).expect("dirs");
        for f in [
            "actors/b.prefab.ron",
            "actors/props/a.prefab.ron",
            ".regia/c.prefab.ron",
            "actors/d.scene.ron",
        ] {
            std::fs::write(root.join(f), "").expect("file");
        }
        assert_eq!(
            project_prefabs(&root),
            ["actors/b.prefab.ron", "actors/props/a.prefab.ron"]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// The v5 sound fields through the agent method (ADR-3197): Doppler and occlusion ride the
    /// spatial op, time_scale_mode keeps the pitch; a bad mode and a pitch-preserving time scale
    /// out of 0.5..=2 are refused and change nothing; undo restores varispeed.
    #[test]
    fn v5_sound_fields_edit_through_the_agent_method() {
        let (mut world, player) = world_with_player();
        let edit = |world: &mut World, params: Value| seq_section_edit(world, &params);
        edit(
            &mut world,
            json!({ "track": "music", "op": "spatial", "index": 0,
                "spatial": { "source": { "Position": [0.0, 0.0, -4.0] }, "doppler": 0.5, "occlusion": true } }),
        )
        .expect("spatial");
        edit(
            &mut world,
            json!({ "track": "music", "op": "time_scale_mode", "index": 0, "mode": "preserve_pitch" }),
        )
        .expect("mode");
        let sound = |world: &World| -> AudioSection {
            let p = world.get::<SequencePlayer>(player).expect("player");
            let Some(Channel::Audio(list)) = p
                .sequence()
                .tracks
                .iter()
                .find(|t| t.name == "music")
                .map(|t| &t.keys)
            else {
                panic!("music track")
            };
            list[0].clone()
        };
        let s = sound(&world);
        assert_eq!(s.time_scale_mode, TimeScaleMode::PreservePitch);
        let spatial = s.spatial.expect("spatial");
        assert_eq!((spatial.doppler, spatial.occlusion), (0.5, true));
        let bad_mode = edit(
            &mut world,
            json!({ "track": "music", "op": "time_scale_mode", "index": 0, "mode": "tape" }),
        );
        assert!(bad_mode.is_err());
        let too_fast = edit(
            &mut world,
            json!({ "track": "music", "op": "time_scale", "index": 0, "scale": 3.0 }),
        )
        .expect_err("out of range");
        assert_eq!(code(too_fast), "seq_section_out_of_range");
        assert_eq!(sound(&world).time_scale, 1.0, "refused: unchanged");
        let state = seq_sections_state(&mut world, &json!({ "track": "music" })).expect("state");
        assert_eq!(
            state["sections"][0]["section"]["time_scale_mode"],
            json!("PreservePitch")
        );
        assert_eq!(
            state["sections"][0]["section"]["spatial"]["occlusion"],
            json!(true)
        );
        let mut undo = world.remove_resource::<UndoStack>().expect("stack");
        assert!(undo.undo(&mut world));
        world.insert_resource(undo);
        assert_eq!(sound(&world).time_scale_mode, TimeScaleMode::Varispeed);
    }

    /// w11 item 1e (sprint-w9-rev-v06-lanes § 2 finding 5): the sub-sequence **Bindings** row sets
    /// its pair on Enter in either field and when either field loses focus, whichever field is filled
    /// last, once (one `sub_binding` edit, so one undo step); a field left half filled keeps its
    /// text. Driven by egui input events on the row alone.
    #[test]
    fn the_sub_binding_row_commits_on_enter_or_focus_loss_in_either_field_once() {
        struct Row {
            ctx: egui::Context,
            panel: SeqCurvePanel,
            channel: Channel,
            actions: Vec<(usize, String, Option<String>)>,
            tree: Option<egui::accesskit::TreeUpdate>,
        }
        impl Row {
            fn frame(&mut self, events: Vec<egui::Event>) {
                let raw = egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(800.0, 200.0),
                    )),
                    events,
                    ..Default::default()
                };
                let (panel, channel) = (&self.panel, &self.channel);
                let mut got = None;
                let mut output = self.ctx.run_ui(raw, |ui| {
                    if let Some(action) = sub_binding_row(ui, panel, channel) {
                        got = Some(action);
                    }
                });
                output.textures_delta.clear();
                if let Some(tree) = output.platform_output.accesskit_update {
                    self.tree = Some(tree);
                }
                if let Some(StripAction::Edit(SectionEdit::SubBinding { index, from, to })) = got {
                    self.actions.push((index, from, to));
                }
            }
            fn center(&self, name: &str) -> egui::Pos2 {
                let tree = self.tree.as_ref().expect("accesskit is on");
                let rect = tree
                    .nodes
                    .iter()
                    .find_map(|(_, n)| (n.label() == Some(name)).then(|| n.bounds()).flatten())
                    .unwrap_or_else(|| panic!("a `{name}` field"));
                egui::pos2(
                    ((rect.x0 + rect.x1) / 2.0) as f32,
                    ((rect.y0 + rect.y1) / 2.0) as f32,
                )
            }
            fn click_at(&mut self, pos: egui::Pos2) {
                self.frame(vec![egui::Event::PointerMoved(pos)]);
                for pressed in [true, false] {
                    self.frame(vec![egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    }]);
                }
                self.frame(Vec::new());
            }
            fn type_into(&mut self, name: &str, text: &str) {
                let pos = self.center(name);
                self.click_at(pos);
                self.frame(vec![egui::Event::Text(text.to_owned())]);
                self.frame(Vec::new());
            }
            fn enter(&mut self) {
                self.frame(vec![egui::Event::Key {
                    key: egui::Key::Enter,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }]);
                self.frame(Vec::new());
            }
        }
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut row = Row {
            ctx,
            panel: SeqCurvePanel {
                selected: Some(0),
                ..SeqCurvePanel::default()
            },
            channel: Channel::Sub(vec![SubSection::new("fx.seq.ron", 0.0, 2.0)]),
            actions: Vec::new(),
            tree: None,
        };
        row.frame(Vec::new());
        row.frame(Vec::new());
        const TARGET: &str = "Sub-sequence target name";
        const BINDS: &str = "Local binding name";
        // "binds as" first, then the target, then Enter in the target field.
        row.type_into(BINDS, "hero");
        row.type_into(TARGET, "lamp");
        assert!(
            row.actions.is_empty(),
            "half filled: nothing yet: {:?}",
            row.actions
        );
        row.enter();
        assert_eq!(
            row.actions,
            vec![(0, "lamp".to_string(), Some("hero".to_string()))],
            "Enter in the target field sets the pair once"
        );
        // The target first, then "binds as", then a click elsewhere (focus loss).
        row.actions.clear();
        row.type_into(TARGET, "bell");
        row.type_into(BINDS, "boss");
        row.click_at(egui::pos2(790.0, 190.0));
        row.frame(Vec::new());
        assert_eq!(
            row.actions,
            vec![(0, "bell".to_string(), Some("boss".to_string()))],
            "focus loss sets the pair once"
        );
    }
}
