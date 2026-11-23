// SPDX-License-Identifier: MPL-2.0
//! The span timeline (part B, w11-motiondb-editor-b; spec 52 § 6): per clip entry, a horizontal
//! strip of the entry's database frames with one row per span (`unindexed`, `pass_through`,
//! `landing_bias`, `tag`, spec 52 § 2.5) and a bottom row to draw new spans on.
//!
//! Mouse: drag on the bottom row to add a span of the chosen kind; drag a span's body to move it
//! or one of its edges to resize it (edges snap to frames); right-click a span for Delete; click
//! or drag the ruler to scrub (the pose preview follows). Keyboard (the strip takes focus when
//! clicked): Left/Right move the selected edge (or the whole span when its body is selected) one
//! frame, Shift ten, or move the cursor when no span is selected; Up/Down select the previous or
//! next span; E cycles start, end and body; N adds a one-frame span of the chosen kind at the
//! cursor; Delete removes the selected span; Escape clears the selection.
//!
//! Every action is one [`MmDbOp`] (`add_span`, `set_span`, `remove_span`) applied as one undo step
//! through [`super::edit`], so it is what `editor.mm_db_edit` does. The pure parts (frame
//! geometry, hit testing, the op a drag or a key makes) are free functions the tests drive.

use std::collections::BTreeMap;

use regia_animation::motion_matching::{MotionDatabaseSource, SpanKindSource, SpanSource};
use regia_ecs::prelude::*;

use super::MmDbEditor;
use super::ops::{MmDbOp, frame_time, kinds_merge, time_frame};

/// Height of the ruler and of each span row, points.
pub const ROW_HEIGHT: f32 = 16.0;
/// How close to a span edge (points) a press grabs the edge rather than the body.
pub const EDGE_GRAB: f32 = 4.0;

/// Which part of a span is selected or dragged.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Part {
    /// The start edge.
    Start,
    /// The end edge.
    End,
    /// The whole span.
    #[default]
    Body,
}

impl Part {
    /// The next part (E key): start, end, body.
    #[must_use]
    pub fn next(self) -> Self {
        match self {
            Self::Start => Self::End,
            Self::End => Self::Body,
            Self::Body => Self::Start,
        }
    }
}

/// The kind a drag on the bottom row (or N) creates.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NewKind {
    /// `unindexed`.
    #[default]
    Unindexed,
    /// `pass_through`.
    PassThrough,
    /// `landing_bias` with [`TimelineState::new_bias`].
    LandingBias,
    /// `tag` with [`TimelineState::new_tag`].
    Tag,
}

impl NewKind {
    /// Every kind, in the selector's order.
    pub const ALL: [NewKind; 4] = [
        NewKind::Unindexed,
        NewKind::PassThrough,
        NewKind::LandingBias,
        NewKind::Tag,
    ];

    /// The span kind's name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Unindexed => "unindexed",
            Self::PassThrough => "pass_through",
            Self::LandingBias => "landing_bias",
            Self::Tag => "tag",
        }
    }
}

/// A span kind's short name.
#[must_use]
pub fn kind_name(kind: &SpanKindSource) -> String {
    match kind {
        SpanKindSource::PassThrough => "pass_through".into(),
        SpanKindSource::Unindexed => "unindexed".into(),
        SpanKindSource::LandingBias(b) => format!("landing_bias {b:+}"),
        SpanKindSource::Tag(names) => format!("tag {}", names.join(", ")),
    }
}

/// The selected span.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    /// Entry index.
    pub entry: usize,
    /// Span index in the entry.
    pub span: usize,
    /// Which part arrows move.
    pub part: Part,
}

/// The scrub cursor: the frame the pose preview shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cursor {
    /// Entry index.
    pub entry: usize,
    /// Frame within the entry.
    pub frame: u32,
    /// The entry's mirrored copy.
    pub mirrored: bool,
}

/// What a drag in progress does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DragTarget {
    /// Scrubbing the ruler.
    Scrub,
    /// Drawing a new span from frame boundary `anchor`.
    Create {
        /// Boundary where the press was.
        anchor: u32,
    },
    /// Moving or resizing span `span` (frames `[start, end)` when the drag began), grabbed at
    /// boundary `grab`.
    Span {
        /// Span index.
        span: usize,
        /// The part dragged.
        part: Part,
        /// Boundary where the press was.
        grab: u32,
        /// Start frame when the drag began.
        start: u32,
        /// End frame when the drag began.
        end: u32,
    },
}

/// A drag in progress.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Drag {
    /// Entry index.
    pub entry: usize,
    /// What it does.
    pub target: DragTarget,
    /// The boundary under the pointer now.
    pub current: u32,
}

/// Where one entry's strip was drawn (for hit tests, and for tests to aim synthetic input).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Geometry {
    /// Left edge (frame boundary 0), points.
    pub left: f32,
    /// Top of the ruler, points.
    pub top: f32,
    /// Width from boundary 0 to boundary `frames`, points.
    pub width: f32,
    /// The entry's frames.
    pub frames: u32,
    /// Span rows (the bottom "new span" row is after them).
    pub spans: usize,
}

/// What is under a point of the strip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    /// The ruler, at frame boundary `boundary` (the frame cell is `frame`).
    Ruler {
        /// Nearest boundary.
        boundary: u32,
        /// The frame cell.
        frame: u32,
    },
    /// A span row: span `span`, `part` grabbed, at boundary `boundary`.
    Span {
        /// Span index.
        span: usize,
        /// Edge or body.
        part: Part,
        /// Nearest boundary.
        boundary: u32,
    },
    /// A span row outside its span.
    Row {
        /// Nearest boundary.
        boundary: u32,
    },
    /// The bottom row, at boundary `boundary`.
    NewRow {
        /// Nearest boundary.
        boundary: u32,
    },
    /// Outside the strip.
    None,
}

impl Geometry {
    /// Points per frame.
    #[must_use]
    pub fn step(&self) -> f32 {
        self.width / self.frames.max(1) as f32
    }

    /// The x of frame boundary `boundary`.
    #[must_use]
    pub fn x_of(&self, boundary: u32) -> f32 {
        self.left + boundary as f32 * self.step()
    }

    /// The nearest frame boundary to `x`, in `0..=frames`.
    #[must_use]
    pub fn boundary_at(&self, x: f32) -> u32 {
        let b = ((x - self.left) / self.step()).round();
        (b.max(0.0) as u32).min(self.frames)
    }

    /// The frame cell under `x`, in `0..frames`.
    #[must_use]
    pub fn frame_at(&self, x: f32) -> u32 {
        let f = ((x - self.left) / self.step()).floor();
        (f.max(0.0) as u32).min(self.frames.saturating_sub(1))
    }

    /// The vertical centre of row `row` (0 the ruler, 1..=spans the span rows, spans + 1 the new
    /// span row).
    #[must_use]
    pub fn row_y(&self, row: usize) -> f32 {
        self.top + (row as f32 + 0.5) * ROW_HEIGHT
    }

    /// Total height.
    #[must_use]
    pub fn height(&self) -> f32 {
        (self.spans + 2) as f32 * ROW_HEIGHT
    }

    /// What is at `(x, y)`; `span_frames` are the spans' `[start, end)` frames.
    #[must_use]
    pub fn hit(&self, x: f32, y: f32, span_frames: &[(u32, u32)]) -> Hit {
        if x < self.left - EDGE_GRAB || x > self.left + self.width + EDGE_GRAB || y < self.top {
            return Hit::None;
        }
        let row = ((y - self.top) / ROW_HEIGHT).floor() as usize;
        let boundary = self.boundary_at(x);
        if row == 0 {
            return Hit::Ruler {
                boundary,
                frame: self.frame_at(x),
            };
        }
        if row == self.spans + 1 {
            return Hit::NewRow { boundary };
        }
        let Some(&(a, b)) = span_frames.get(row - 1) else {
            return Hit::None;
        };
        let (xa, xb) = (self.x_of(a), self.x_of(b));
        let part = if (x - xa).abs() <= EDGE_GRAB && (x - xa).abs() <= (x - xb).abs() {
            Part::Start
        } else if (x - xb).abs() <= EDGE_GRAB {
            Part::End
        } else if x > xa && x < xb {
            Part::Body
        } else {
            return Hit::Row { boundary };
        };
        Hit::Span {
            span: row - 1,
            part,
            boundary,
        }
    }
}

/// A span's `[start, end)` in frames at `rate` (the nearest frames of its seconds; at least one
/// frame long).
#[must_use]
pub fn span_frames(rate: f32, span: &SpanSource) -> (u32, u32) {
    let a = time_frame(rate, span.range.0);
    let b = time_frame(rate, span.range.1).max(a + 1);
    (a, b)
}

/// `[start, end)` after moving `part` of a span at frames `(start, end)` by `delta` frames on a
/// strip of `frames` frames: an edge never crosses the other (a span keeps one frame), the body
/// keeps its length and stays on the strip (a span already past the end may only move left).
#[must_use]
pub fn moved(start: u32, end: u32, part: Part, delta: i64, frames: u32) -> (u32, u32) {
    let (s, e, f) = (i64::from(start), i64::from(end), i64::from(frames.max(end)));
    let (ns, ne) = match part {
        Part::Start => ((s + delta).clamp(0, e - 1), e),
        Part::End => (s, (e + delta).clamp(s + 1, f)),
        Part::Body => {
            let d = delta.clamp(-s, (f - e).max(0));
            (s + d, e + d)
        }
    };
    (ns as u32, ne as u32)
}

/// The `set_span` op that moves `part` of span `span` of entry `entry` by `delta` frames: moved
/// edges snap to frame times ([`frame_time`]), an edge that did not move keeps its exact seconds.
/// `None` when nothing would change.
#[must_use]
pub fn move_op(
    doc: &MotionDatabaseSource,
    entry: usize,
    span: usize,
    part: Part,
    delta: i64,
    frames: u32,
) -> Option<MmDbOp> {
    let source = doc.entries.get(entry)?.spans.get(span)?;
    let rate = doc.schema.sample_rate;
    let (a, b) = span_frames(rate, source);
    let (na, nb) = moved(a, b, part, delta, frames);
    if (na, nb) == (a, b) {
        return None;
    }
    let start = if na == a && part == Part::End {
        source.range.0
    } else {
        frame_time(rate, na)
    };
    let end = if nb == b && part == Part::Start {
        source.range.1
    } else {
        frame_time(rate, nb)
    };
    Some(MmDbOp::SetSpan {
        index: entry,
        span,
        kind: None,
        range: Some((start, end)),
        frames: None,
    })
}

/// The span kind a new span gets.
#[must_use]
pub fn new_kind_source(kind: NewKind, bias: f32, tag: &str) -> SpanKindSource {
    match kind {
        NewKind::Unindexed => SpanKindSource::Unindexed,
        NewKind::PassThrough => SpanKindSource::PassThrough,
        NewKind::LandingBias => SpanKindSource::LandingBias(bias),
        NewKind::Tag => SpanKindSource::Tag(vec![tag.trim().to_string()]),
    }
}

/// The op a finished drag makes (`None` for a scrub or a drag that changed nothing).
#[must_use]
pub fn drag_op(
    doc: &MotionDatabaseSource,
    drag: &Drag,
    frames: u32,
    new_kind: SpanKindSource,
) -> Option<MmDbOp> {
    match drag.target {
        DragTarget::Scrub => None,
        DragTarget::Create { anchor } => {
            let (a, b) = (anchor.min(drag.current), anchor.max(drag.current));
            let b = if a == b { (a + 1).min(frames.max(1)) } else { b };
            let a = a.min(b.saturating_sub(1));
            Some(MmDbOp::AddSpan {
                index: drag.entry,
                kind: new_kind,
                range: None,
                frames: Some((a, b)),
            })
        }
        DragTarget::Span { span, part, grab, .. } => {
            let delta = i64::from(drag.current) - i64::from(grab);
            move_op(doc, drag.entry, span, part, delta, frames)
        }
    }
}

/// The index of the span that holds `range` with `kind` after an add or a merge.
#[must_use]
pub fn locate(spans: &[SpanSource], kind: &SpanKindSource, range: (f32, f32)) -> Option<usize> {
    spans.iter().position(|s| {
        (s.kind == *kind || kinds_merge(&s.kind, kind)) && s.range.0 <= range.0 && s.range.1 >= range.1
    })
}

/// The timeline's window state (part of the motion-database window's state).
#[derive(Clone, Debug)]
pub struct TimelineState {
    /// The selected span.
    pub selected: Option<Selection>,
    /// The drag in progress.
    pub drag: Option<Drag>,
    /// The kind new spans get.
    pub new_kind: NewKind,
    /// The bias of a new `landing_bias` span.
    pub new_bias: f32,
    /// The tag of a new `tag` span.
    pub new_tag: String,
    /// The scrub cursor.
    pub cursor: Option<Cursor>,
    /// Scroll the clip list to this entry on the next draw (a warning row was picked).
    pub scroll_to: Option<usize>,
    /// The preview camera (yaw, pitch, distance factor).
    pub orbit: [f32; 3],
    /// Where each entry's strip was last drawn.
    pub geometry: BTreeMap<usize, Geometry>,
    /// `(frame, entry)` of the last key op (egui may run a frame's UI twice; a key applies once).
    last_key: Option<(u64, usize)>,
}

impl Default for TimelineState {
    fn default() -> Self {
        Self {
            selected: None,
            drag: None,
            new_kind: NewKind::default(),
            new_bias: -1.0,
            new_tag: String::new(),
            cursor: None,
            scroll_to: None,
            orbit: [0.7, 0.35, 1.0],
            geometry: BTreeMap::new(),
            last_key: None,
        }
    }
}

/// Points the scrub cursor at `frame` of entry `entry` and scrolls the clip list there (a
/// warning or problem row, and `editor.mm_db_scrub`).
pub fn select_frame(world: &mut World, entry: usize, frame: u32, mirrored: bool) {
    let mut editor = world.get_resource_or_insert_with(MmDbEditor::default);
    editor.ui.timeline.cursor = Some(Cursor {
        entry,
        frame,
        mirrored,
    });
    editor.ui.timeline.scroll_to = Some(entry);
}

fn apply(world: &mut World, state: &mut TimelineState, label: &str, op: MmDbOp) -> bool {
    apply_span_op(world, state, label, None, op)
}

/// Applies one span op as one undo step (merged with the previous step under the same
/// `merge_key`, e.g. while a frame field is dragged), keeps the selection on the span that holds
/// the result, and puts a refusal in the window's status line. `true` when the document changed.
pub fn apply_span_op(
    world: &mut World,
    state: &mut TimelineState,
    label: &str,
    merge_key: Option<String>,
    op: MmDbOp,
) -> bool {
    let before = world.get_resource::<MmDbEditor>().and_then(|e| e.doc.clone());
    match super::edit(world, label, merge_key, std::slice::from_ref(&op)) {
        Ok(changed) => {
            let after = world.get_resource::<MmDbEditor>().and_then(|e| e.doc.clone());
            if let Some(selection) = selection_after(before.as_ref(), after.as_ref(), &op, state.selected) {
                state.selected = selection;
            }
            changed
        }
        Err(e) => {
            world.get_resource_or_insert_with(MmDbEditor::default).toast =
                format!("{} ({})", e.message, e.code);
            false
        }
    }
}

/// The selection after `op`: the span that now holds the added or moved range (it may have merged
/// into another), none after a delete. `None` leaves the selection alone.
#[must_use]
pub fn selection_after(
    before: Option<&MotionDatabaseSource>,
    after: Option<&MotionDatabaseSource>,
    op: &MmDbOp,
    selected: Option<Selection>,
) -> Option<Option<Selection>> {
    let after = after?;
    let rate = after.schema.sample_rate;
    let (index, kind, range) = match op {
        MmDbOp::RemoveSpan { .. } => return Some(None),
        MmDbOp::AddSpan {
            index,
            kind,
            range,
            frames,
        } => (
            *index,
            kind.clone(),
            range.or_else(|| frames.map(|(a, b)| (frame_time(rate, a), frame_time(rate, b))))?,
        ),
        MmDbOp::SetSpan {
            index,
            span,
            kind,
            range,
            frames,
        } => {
            let old = before?.entries.get(*index)?.spans.get(*span)?;
            (
                *index,
                kind.clone().unwrap_or_else(|| old.kind.clone()),
                range
                    .or_else(|| frames.map(|(a, b)| (frame_time(rate, a), frame_time(rate, b))))
                    .unwrap_or(old.range),
            )
        }
        _ => return None,
    };
    let spans = &after.entries.get(index)?.spans;
    let kind = match kind {
        SpanKindSource::LandingBias(b) => SpanKindSource::LandingBias(b.clamp(
            -regia_animation::motion_matching::MAX_BIAS,
            regia_animation::motion_matching::MAX_BIAS,
        )),
        other => other,
    };
    let span = locate(spans, &kind, range)?;
    Some(Some(Selection {
        entry: index,
        span,
        part: selected.map_or(Part::Body, |s| s.part),
    }))
}

/// The span rows' frames of `entry` and the strip's frame count: the preview's frames when the
/// clip loads, otherwise enough frames for every span (with a note).
fn strip_frames(world: &mut World, doc: &MotionDatabaseSource, entry: usize) -> (u32, Option<String>) {
    match super::preview::entry_preview(world, entry) {
        Ok(p) => (p.frames().max(1) as u32, p.note.clone()),
        Err(e) => {
            let rate = doc.schema.sample_rate;
            let longest = doc.entries[entry]
                .spans
                .iter()
                .map(|s| span_frames(rate, s).1)
                .max()
                .unwrap_or(0);
            (
                longest.max(30),
                Some(format!(
                    "clip not loaded ({}): the strip is as long as the spans",
                    e.code
                )),
            )
        }
    }
}

/// Draws entry `entry`'s span timeline and handles its input (`state` is the window state the
/// caller took out of the world).
pub fn timeline_ui(ui: &mut egui::Ui, world: &mut World, state: &mut TimelineState, entry: usize) {
    let Some(doc) = world.get_resource::<MmDbEditor>().and_then(|e| e.doc.clone()) else {
        return;
    };
    let Some(source) = doc.entries.get(entry) else {
        return;
    };
    let rate = doc.schema.sample_rate;
    let (frames, note) = strip_frames(world, &doc, entry);
    let spans: Vec<(u32, u32)> = source.spans.iter().map(|s| span_frames(rate, s)).collect();
    let width = ui.available_width().max(120.0);
    let size = egui::vec2(width, (spans.len() + 2) as f32 * ROW_HEIGHT);
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let id = egui::Id::new(("mm_db_timeline", entry));
    let response = ui
        .interact(rect, id, egui::Sense::click_and_drag())
        .on_hover_text(
            "Drag on the bottom row to add a span; drag a span to move it or its edges to resize it; right-click \
             a span to delete it; click the ruler to scrub. Keys: Left/Right move the selected edge one frame \
             (Shift: ten), Up/Down pick a span, E picks start, end or body, N adds a span at the cursor, Delete \
             removes it.",
        );
    let geometry = Geometry {
        left: rect.left(),
        top: rect.top(),
        width: rect.width(),
        frames,
        spans: spans.len(),
    };
    state.geometry.insert(entry, geometry);
    if state.scroll_to == Some(entry) {
        ui.scroll_to_rect(rect, Some(egui::Align::Center));
        state.scroll_to = None;
    }
    let new_kind = new_kind_source(state.new_kind, state.new_bias, &state.new_tag);

    // Mouse.
    if response.drag_started() {
        response.request_focus();
        if let Some(origin) = ui.input(|i| i.pointer.press_origin()) {
            let target = match geometry.hit(origin.x, origin.y, &spans) {
                Hit::Ruler { frame, .. } => {
                    state.cursor = Some(Cursor {
                        entry,
                        frame,
                        mirrored: false,
                    });
                    Some(DragTarget::Scrub)
                }
                Hit::NewRow { boundary } => Some(DragTarget::Create { anchor: boundary }),
                Hit::Span { span, part, boundary } => {
                    state.selected = Some(Selection { entry, span, part });
                    Some(DragTarget::Span {
                        span,
                        part,
                        grab: boundary,
                        start: spans[span].0,
                        end: spans[span].1,
                    })
                }
                Hit::Row { .. } | Hit::None => None,
            };
            state.drag = target.map(|target| Drag {
                entry,
                target,
                current: geometry.boundary_at(origin.x),
            });
        }
    }
    if let Some(drag) = state.drag.as_mut().filter(|d| d.entry == entry)
        && let Some(pos) = ui.input(|i| i.pointer.interact_pos())
        && (response.dragged() || response.drag_stopped())
    {
        drag.current = geometry.boundary_at(pos.x);
        if drag.target == DragTarget::Scrub {
            state.cursor = Some(Cursor {
                entry,
                frame: geometry.frame_at(pos.x),
                mirrored: state.cursor.is_some_and(|c| c.entry == entry && c.mirrored),
            });
        }
    }
    if response.drag_stopped()
        && let Some(drag) = state.drag.take().filter(|d| d.entry == entry)
        && let Some(op) = drag_op(&doc, &drag, frames, new_kind.clone())
    {
        let label = if matches!(drag.target, DragTarget::Create { .. }) {
            "add span"
        } else {
            "move span"
        };
        apply(world, state, label, op);
    }
    if (response.clicked() || response.secondary_clicked())
        && let Some(pos) = response.interact_pointer_pos()
    {
        response.request_focus();
        match geometry.hit(pos.x, pos.y, &spans) {
            Hit::Ruler { frame, .. } => {
                state.cursor = Some(Cursor {
                    entry,
                    frame,
                    mirrored: false,
                });
            }
            Hit::Span { span, part, .. } => state.selected = Some(Selection { entry, span, part }),
            Hit::Row { .. } | Hit::NewRow { .. } => state.selected = None,
            Hit::None => {}
        }
    }
    let selected = state
        .selected
        .filter(|s| s.entry == entry && s.span < spans.len());
    response.context_menu(|ui| match selected {
        Some(sel) => {
            ui.label(kind_name(&source.spans[sel.span].kind));
            if ui.button("Delete span").clicked() {
                apply(
                    world,
                    state,
                    "delete span",
                    MmDbOp::RemoveSpan {
                        index: entry,
                        span: sel.span,
                    },
                );
                ui.close();
            }
        }
        None => {
            ui.weak("Right-click a span to delete it.");
        }
    });

    // Keyboard.
    if response.has_focus() {
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(
                id,
                egui::EventFilter {
                    horizontal_arrows: true,
                    vertical_arrows: true,
                    escape: true,
                    ..Default::default()
                },
            );
        });
        let frame_nr = ui.ctx().cumulative_frame_nr();
        if state.last_key != Some((frame_nr, entry)) {
            let key_op = keyboard(ui, state, &doc, entry, frames, &spans, new_kind);
            if let Some((label, op)) = key_op {
                state.last_key = Some((frame_nr, entry));
                apply(world, state, label, op);
            }
        }
    }

    paint(ui, &geometry, source, &spans, state, entry, response.has_focus());
    if let Some(note) = note {
        ui.weak(note);
    }
}

/// The op a key press makes, and the selection and cursor moves that make none.
fn keyboard(
    ui: &egui::Ui,
    state: &mut TimelineState,
    doc: &MotionDatabaseSource,
    entry: usize,
    frames: u32,
    spans: &[(u32, u32)],
    new_kind: SpanKindSource,
) -> Option<(&'static str, MmDbOp)> {
    let (left, right, up, down, e, n, delete, escape, shift) = ui.input(|i| {
        (
            i.key_pressed(egui::Key::ArrowLeft),
            i.key_pressed(egui::Key::ArrowRight),
            i.key_pressed(egui::Key::ArrowUp),
            i.key_pressed(egui::Key::ArrowDown),
            i.key_pressed(egui::Key::E),
            i.key_pressed(egui::Key::N),
            i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace),
            i.key_pressed(egui::Key::Escape),
            i.modifiers.shift,
        )
    });
    let step = if shift { 10 } else { 1 };
    let delta = if right {
        step
    } else if left {
        -step
    } else {
        0
    };
    let selected = state
        .selected
        .filter(|s| s.entry == entry && s.span < spans.len());
    if escape {
        state.selected = None;
        state.drag = None;
        return None;
    }
    if up || down {
        let next = match selected {
            Some(s) if up => s.span.checked_sub(1),
            Some(s) => Some(s.span + 1).filter(|i| *i < spans.len()),
            None => (!spans.is_empty()).then_some(0),
        };
        if let Some(span) = next {
            state.selected = Some(Selection {
                entry,
                span,
                part: selected.map_or(Part::Body, |s| s.part),
            });
        }
        return None;
    }
    if e && let Some(mut s) = selected {
        s.part = s.part.next();
        state.selected = Some(s);
        return None;
    }
    if delete && let Some(s) = selected {
        return Some((
            "delete span",
            MmDbOp::RemoveSpan {
                index: entry,
                span: s.span,
            },
        ));
    }
    if n {
        let at = state
            .cursor
            .filter(|c| c.entry == entry)
            .map_or(0, |c| c.frame)
            .min(frames.saturating_sub(1));
        return Some((
            "add span",
            MmDbOp::AddSpan {
                index: entry,
                kind: new_kind,
                range: None,
                frames: Some((at, at + 1)),
            },
        ));
    }
    if delta != 0 {
        match selected {
            Some(s) => {
                return move_op(doc, entry, s.span, s.part, delta, frames).map(|op| ("move span", op));
            }
            None => {
                let cursor = state.cursor.filter(|c| c.entry == entry).unwrap_or(Cursor {
                    entry,
                    frame: 0,
                    mirrored: false,
                });
                let frame = (i64::from(cursor.frame) + delta).clamp(0, i64::from(frames.saturating_sub(1)));
                state.cursor = Some(Cursor {
                    frame: frame as u32,
                    ..cursor
                });
            }
        }
    }
    None
}

fn paint(
    ui: &egui::Ui,
    g: &Geometry,
    source: &regia_animation::motion_matching::EntrySource,
    spans: &[(u32, u32)],
    state: &TimelineState,
    entry: usize,
    focused: bool,
) {
    let tokens = crate::theme::ui_tokens(ui.ctx());
    let painter = ui.painter();
    let rect = egui::Rect::from_min_size(egui::pos2(g.left, g.top), egui::vec2(g.width, g.height()));
    painter.rect_filled(rect, 2.0, tokens.well);
    if focused {
        painter.rect_stroke(
            rect,
            2.0,
            egui::Stroke::new(1.0, tokens.accents.select),
            egui::StrokeKind::Inside,
        );
    }
    // Ruler ticks: every frame when there is room, otherwise every 5 or 10.
    let step = g.step();
    let every: u32 = if step >= 6.0 {
        1
    } else if step * 5.0 >= 6.0 {
        5
    } else {
        10
    };
    let font = egui::FontId::monospace(9.0);
    for b in (0..=g.frames).step_by(every as usize) {
        let x = g.x_of(b);
        let major = b.is_multiple_of(every * 5);
        let h = if major { ROW_HEIGHT * 0.6 } else { ROW_HEIGHT * 0.3 };
        painter.line_segment(
            [
                egui::pos2(x, g.top + ROW_HEIGHT - h),
                egui::pos2(x, g.top + ROW_HEIGHT),
            ],
            egui::Stroke::new(1.0, tokens.line),
        );
        if major {
            painter.text(
                egui::pos2(x + 2.0, g.top + 1.0),
                egui::Align2::LEFT_TOP,
                b.to_string(),
                font.clone(),
                tokens.muted,
            );
        }
    }
    let row_rect = |row: usize, a: u32, b: u32| {
        egui::Rect::from_min_max(
            egui::pos2(g.x_of(a), g.top + row as f32 * ROW_HEIGHT + 2.0),
            egui::pos2(
                g.x_of(b).max(g.x_of(a) + 2.0),
                g.top + (row + 1) as f32 * ROW_HEIGHT - 2.0,
            ),
        )
    };
    let drag = state.drag.filter(|d| d.entry == entry);
    for (index, (span, &(a, b))) in source.spans.iter().zip(spans).enumerate() {
        let colour = match &span.kind {
            SpanKindSource::Unindexed => tokens.muted,
            SpanKindSource::PassThrough => tokens.warn,
            SpanKindSource::LandingBias(_) => tokens.signal,
            SpanKindSource::Tag(_) => tokens.accents.select,
        };
        let (a, b) = match drag.map(|d| d.target) {
            Some(DragTarget::Span {
                span: s,
                part,
                grab,
                start,
                end,
            }) if s == index => moved(
                start,
                end,
                part,
                i64::from(drag.map_or(grab, |d| d.current)) - i64::from(grab),
                g.frames,
            ),
            _ => (a, b),
        };
        let r = row_rect(index + 1, a, b);
        painter.rect_filled(r, 2.0, colour.gamma_multiply(0.7));
        painter.with_clip_rect(r).text(
            r.left_center() + egui::vec2(3.0, 0.0),
            egui::Align2::LEFT_CENTER,
            kind_name(&span.kind),
            font.clone(),
            tokens.text,
        );
        if let Some(sel) = state.selected.filter(|s| s.entry == entry && s.span == index) {
            let outline = egui::Stroke::new(1.5, tokens.overlay.selection_outline);
            painter.rect_stroke(r, 2.0, outline, egui::StrokeKind::Outside);
            let edge = match sel.part {
                Part::Start => Some(r.left()),
                Part::End => Some(r.right()),
                Part::Body => None,
            };
            if let Some(x) = edge {
                painter.line_segment(
                    [egui::pos2(x, r.top() - 1.0), egui::pos2(x, r.bottom() + 1.0)],
                    egui::Stroke::new(3.0, tokens.overlay.selection_outline),
                );
            }
        }
    }
    let new_row = spans.len() + 1;
    if let Some(Drag {
        target: DragTarget::Create { anchor },
        current,
        ..
    }) = drag
    {
        let (a, b) = (
            anchor.min(current),
            anchor.max(current).max(anchor.min(current) + 1),
        );
        painter.rect_stroke(
            row_rect(new_row, a, b),
            2.0,
            egui::Stroke::new(1.0, tokens.accents.select),
            egui::StrokeKind::Inside,
        );
    } else {
        painter.text(
            egui::pos2(g.left + 3.0, g.row_y(new_row)),
            egui::Align2::LEFT_CENTER,
            format!("drag here to add a {} span", state.new_kind.name()),
            font,
            tokens.muted,
        );
    }
    if let Some(c) = state.cursor.filter(|c| c.entry == entry) {
        let x = g.x_of(c.frame) + step * 0.5;
        painter.line_segment(
            [egui::pos2(x, g.top), egui::pos2(x, g.top + g.height())],
            egui::Stroke::new(1.5, tokens.accents.alert),
        );
    }
}
