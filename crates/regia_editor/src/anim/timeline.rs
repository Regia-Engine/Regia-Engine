// SPDX-License-Identifier: MPL-2.0
//! A standalone clip timeline: tracks of keys, a playhead, a range, a loop toggle and a lane
//! of markers (foot contacts, notifies), as an egui widget that returns edit events.
//!
//! The widget owns plain data ([`TimelineTrack`], [`TimelineMarker`]) and knows nothing about
//! [`AnimEditor`](super::AnimEditor) documents, clips or undo: a host fills the tracks, calls
//! [`Timeline::show`] every frame and applies the returned [`TimelineEvent`]s to whatever it
//! edits (the editor's own Clip panel keeps its ruler; this is the reusable one for tools
//! such as Tincal's motion stage). The widget applies each edit to its own data as it emits it,
//! so a drag stays consistent across frames; hosts mirror the events and need not copy the
//! data back.
//!
//! Interaction (all of it also reachable without egui through [`Timeline::pointer`], which is
//! what the tests and `editor.timeline` drive):
//!
//! - click or drag on the ruler scrubs the playhead; shift-drag draws the range; the range's
//!   ends are draggable; a secondary click on the ruler clears the range;
//! - the toolbar's **Loop** button toggles looping;
//! - drag a key or marker to move it; a secondary click, or Delete, removes the selected one;
//! - a double click on a track adds a key there, on the marker lane adds a notify marker;
//! - Ctrl + wheel zooms around the pointer, the plain wheel pans.
//!
//! Key and marker lists are not kept sorted (an index stays valid through a drag); hosts sort
//! on their side.

use regia_animation::clip::{AnimationClip, TrackCurve};
use regia_animation::skeleton::Skeleton;

use egui::{Align2, Color32, FontId, Pos2, Rect, Stroke, vec2};

/// Height of the toolbar row (Loop button, time readout), points.
pub const TOOLBAR_HEIGHT: f32 = 22.0;
/// Height of the ruler row, points.
pub const RULER_HEIGHT: f32 = 20.0;
/// Height of the marker lane, points.
pub const MARKER_HEIGHT: f32 = 20.0;
/// Height of one track row, points.
pub const TRACK_HEIGHT: f32 = 20.0;
/// Width of the track-name column on the left, points.
pub const LABEL_WIDTH: f32 = 110.0;
/// Width of the Loop button, points.
pub const LOOP_BUTTON_WIDTH: f32 = 56.0;
/// How close, in points, a pointer must be to grab a key, marker or range end.
pub const GRAB_RADIUS: f32 = 6.0;

/// What a marker marks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MarkerKind {
    /// A foot contact (plant) or lift event.
    FootContact,
    /// An animation notify.
    #[default]
    Notify,
    /// Anything else the host draws.
    Custom,
}

impl MarkerKind {
    /// Lower-case name used by the agent protocol.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            MarkerKind::FootContact => "foot_contact",
            MarkerKind::Notify => "notify",
            MarkerKind::Custom => "custom",
        }
    }

    /// Inverse of [`MarkerKind::name`].
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "foot_contact" => MarkerKind::FootContact,
            "notify" => MarkerKind::Notify,
            "custom" => MarkerKind::Custom,
            _ => return None,
        })
    }

    fn color(self) -> Color32 {
        match self {
            MarkerKind::FootContact => Color32::from_rgb(90, 190, 110),
            MarkerKind::Notify => Color32::from_rgb(80, 130, 220),
            MarkerKind::Custom => Color32::from_rgb(200, 160, 70),
        }
    }
}

/// A row of keys (for example one joint's translation track).
#[derive(Clone, Debug, PartialEq)]
pub struct TimelineTrack {
    /// Row label.
    pub name: String,
    /// Key times, seconds. Not required to be sorted.
    pub keys: Vec<f32>,
    /// Key color.
    pub color: Color32,
}

impl TimelineTrack {
    /// A track with the default key color.
    #[must_use]
    pub fn new(name: impl Into<String>, keys: Vec<f32>) -> Self {
        Self {
            name: name.into(),
            keys,
            color: Color32::from_rgb(230, 200, 90),
        }
    }
}

/// A marker on the marker lane.
#[derive(Clone, Debug, PartialEq)]
pub struct TimelineMarker {
    /// Time, seconds.
    pub time: f32,
    /// Label drawn beside it.
    pub label: String,
    /// What it marks.
    pub kind: MarkerKind,
}

impl TimelineMarker {
    /// A marker.
    #[must_use]
    pub fn new(time: f32, label: impl Into<String>, kind: MarkerKind) -> Self {
        Self {
            time,
            label: label.into(),
            kind,
        }
    }
}

/// The selected item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Selection {
    /// A key: track and index in that track's key list.
    Key {
        /// Track index.
        track: usize,
        /// Key index.
        index: usize,
    },
    /// A marker by index.
    Marker(usize),
}

/// An edit the user made. Times are seconds, already clamped to `0..=duration` and snapped.
#[derive(Clone, Debug, PartialEq)]
pub enum TimelineEvent {
    /// The playhead moved (also while dragging).
    Scrub(f32),
    /// The selection changed.
    Selected(Option<Selection>),
    /// A key moved. `finished` is true on the release that ends the drag (one undo step).
    KeyMoved {
        /// Track index.
        track: usize,
        /// Key index.
        index: usize,
        /// New time.
        time: f32,
        /// The drag ended.
        finished: bool,
    },
    /// A key was added (double click on a track).
    KeyAdded {
        /// Track index.
        track: usize,
        /// Time.
        time: f32,
    },
    /// A key was removed.
    KeyRemoved {
        /// Track index.
        track: usize,
        /// Index it had.
        index: usize,
    },
    /// A marker moved.
    MarkerMoved {
        /// Marker index.
        index: usize,
        /// New time.
        time: f32,
        /// The drag ended.
        finished: bool,
    },
    /// A marker was added (double click on the marker lane).
    MarkerAdded {
        /// Time.
        time: f32,
        /// Kind.
        kind: MarkerKind,
    },
    /// A marker was removed.
    MarkerRemoved {
        /// Index it had.
        index: usize,
    },
    /// The range changed (`None` = cleared).
    RangeChanged {
        /// The new range, `start <= end`.
        range: Option<(f32, f32)>,
        /// The drag ended.
        finished: bool,
    },
    /// The Loop button was toggled.
    LoopChanged(bool),
}

/// What is under a point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    /// The Loop button.
    Loop,
    /// The range's start handle.
    RangeStart,
    /// The range's end handle.
    RangeEnd,
    /// The ruler.
    Ruler,
    /// A marker.
    Marker(usize),
    /// Empty marker lane.
    MarkerLane,
    /// A key.
    Key {
        /// Track index.
        track: usize,
        /// Key index.
        index: usize,
    },
    /// Empty space on a track row.
    Track(usize),
    /// The label column or below the last track.
    Empty,
}

/// A pointer button.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    /// Primary (left).
    Primary,
    /// Secondary (right).
    Secondary,
}

/// Pointer input for [`Timeline::pointer`]; positions are in the same space as the layout rect.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Pointer {
    /// A button went down.
    Press {
        /// Position.
        pos: Pos2,
        /// Which button.
        button: Button,
        /// Shift held (starts a range drag on the ruler).
        shift: bool,
    },
    /// The pointer moved with the primary button held.
    Drag {
        /// Position.
        pos: Pos2,
    },
    /// The primary button was released.
    Release {
        /// Position.
        pos: Pos2,
        /// This release completed a double click.
        double: bool,
    },
    /// The Delete key was pressed: remove the selection.
    Delete,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Drag {
    Scrub,
    Key {
        track: usize,
        index: usize,
    },
    Marker(usize),
    RangeStart,
    RangeEnd,
    /// Range being drawn from this anchor time.
    RangeNew(f32),
}

/// The timeline: its data, its view (zoom and scroll) and the drag in progress.
#[derive(Clone, Debug, PartialEq)]
pub struct Timeline {
    /// Length in seconds; times clamp to `0..=duration`.
    pub duration: f32,
    /// Playhead, seconds.
    pub playhead: f32,
    /// Loop range, `start <= end`.
    pub range: Option<(f32, f32)>,
    /// Looping on.
    pub looping: bool,
    /// Key rows.
    pub tracks: Vec<TimelineTrack>,
    /// Markers.
    pub markers: Vec<TimelineMarker>,
    /// Selected key or marker.
    pub selection: Option<Selection>,
    /// Snap step in seconds (for example `1.0 / 30.0`); `None` = free.
    pub snap: Option<f32>,
    /// Time at the left edge of the track area, seconds.
    pub view_start: f32,
    /// Zoom; 0 = fit the duration into the width on the next layout.
    pub pixels_per_second: f32,
    drag: Option<Drag>,
    last_layout: Option<Layout>,
}

impl Default for Timeline {
    fn default() -> Self {
        Self::new(1.0)
    }
}

/// Where the timeline is drawn: the full rect (toolbar, ruler, marker lane, tracks).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Layout {
    /// The widget's rect.
    pub rect: Rect,
}

impl Layout {
    /// A layout at `left_top` of the width `width`.
    #[must_use]
    pub fn new(left_top: Pos2, width: f32, timeline: &Timeline) -> Self {
        Self {
            rect: Rect::from_min_size(left_top, vec2(width, timeline.height())),
        }
    }

    fn track_area_left(&self) -> f32 {
        self.rect.left() + LABEL_WIDTH
    }

    /// Width of the track area (right of the labels), points.
    #[must_use]
    pub fn track_width(&self) -> f32 {
        (self.rect.width() - LABEL_WIDTH).max(1.0)
    }

    fn ruler_top(&self) -> f32 {
        self.rect.top() + TOOLBAR_HEIGHT
    }

    fn marker_top(&self) -> f32 {
        self.ruler_top() + RULER_HEIGHT
    }

    fn tracks_top(&self) -> f32 {
        self.marker_top() + MARKER_HEIGHT
    }
}

impl Timeline {
    /// An empty timeline of `duration` seconds.
    #[must_use]
    pub fn new(duration: f32) -> Self {
        Self {
            duration: duration.max(0.0),
            playhead: 0.0,
            range: None,
            looping: false,
            tracks: Vec::new(),
            markers: Vec::new(),
            selection: None,
            snap: None,
            view_start: 0.0,
            pixels_per_second: 0.0,
            drag: None,
            last_layout: None,
        }
    }

    /// A timeline showing `clip`: one track per animated joint (the union of its translation,
    /// rotation and scale key times; named from `skeleton` when given) and a marker per
    /// notify (a name starting with `Foot`, case-insensitive, is a foot contact).
    #[must_use]
    pub fn from_clip(clip: &AnimationClip, skeleton: Option<&Skeleton>) -> Self {
        let mut timeline = Self::new(clip.duration);
        for track in &clip.tracks {
            let mut keys: Vec<f32> = Vec::new();
            for curve_keys in [
                curve_times(&track.translations),
                curve_times(&track.rotations),
                curve_times(&track.scales),
            ] {
                keys.extend(curve_keys);
            }
            keys.sort_by(f32::total_cmp);
            keys.dedup();
            let name = skeleton
                .and_then(|s| s.joint_names.get(track.joint as usize))
                .cloned()
                .unwrap_or_else(|| format!("joint {}", track.joint));
            timeline.tracks.push(TimelineTrack::new(name, keys));
        }
        for notify in &clip.notifies {
            let kind = if notify.name.to_ascii_lowercase().starts_with("foot") {
                MarkerKind::FootContact
            } else {
                MarkerKind::Notify
            };
            timeline
                .markers
                .push(TimelineMarker::new(notify.time, notify.name.clone(), kind));
        }
        timeline
    }

    /// Where the widget was drawn by the last [`Timeline::show`].
    #[must_use]
    pub fn layout(&self) -> Option<Layout> {
        self.last_layout
    }

    /// Total height the widget wants, points.
    #[must_use]
    pub fn height(&self) -> f32 {
        TOOLBAR_HEIGHT + RULER_HEIGHT + MARKER_HEIGHT + TRACK_HEIGHT * self.tracks.len().max(1) as f32 + 4.0
    }

    /// Fit the whole duration into `width` points of track area.
    pub fn fit(&mut self, width: f32) {
        self.view_start = 0.0;
        self.pixels_per_second = (width.max(1.0) / self.duration.max(0.01)).max(1.0);
    }

    /// Zoom by `factor` keeping `anchor_time` where it is on screen.
    pub fn zoom_at(&mut self, factor: f32, anchor_time: f32) {
        if self.pixels_per_second <= 0.0 || !factor.is_finite() || factor <= 0.0 {
            return;
        }
        let old = self.pixels_per_second;
        let new = (old * factor).clamp(1.0, 20_000.0);
        // anchor stays at the same screen offset: (anchor - start) * pps constant.
        self.view_start = anchor_time - (anchor_time - self.view_start) * old / new;
        self.pixels_per_second = new;
    }

    /// Scroll the view by `seconds`.
    pub fn pan(&mut self, seconds: f32) {
        self.view_start += seconds;
    }

    fn clamp_time(&self, time: f32) -> f32 {
        let time = if time.is_finite() { time } else { 0.0 };
        let time = match self.snap {
            Some(step) if step > 0.0 => (time / step).round() * step,
            _ => time,
        };
        time.clamp(0.0, self.duration)
    }

    /// Screen x of `time`.
    #[must_use]
    pub fn x_of(&self, layout: &Layout, time: f32) -> f32 {
        layout.track_area_left() + (time - self.view_start) * self.pixels_per_second.max(1e-3)
    }

    /// Time at screen x (not clamped).
    #[must_use]
    pub fn time_at(&self, layout: &Layout, x: f32) -> f32 {
        self.view_start + (x - layout.track_area_left()) / self.pixels_per_second.max(1e-3)
    }

    /// What is at `pos`.
    #[must_use]
    pub fn hit_test(&self, layout: &Layout, pos: Pos2) -> Hit {
        if !layout.rect.contains(pos) {
            return Hit::Empty;
        }
        let y = pos.y;
        if y < layout.ruler_top() {
            let button = Rect::from_min_size(
                layout.rect.min + vec2(2.0, 2.0),
                vec2(LOOP_BUTTON_WIDTH, TOOLBAR_HEIGHT - 4.0),
            );
            return if button.contains(pos) {
                Hit::Loop
            } else {
                Hit::Empty
            };
        }
        if pos.x < layout.track_area_left() {
            return Hit::Empty;
        }
        if y < layout.marker_top() {
            if let Some((start, end)) = self.range {
                let (xs, xe) = (self.x_of(layout, start), self.x_of(layout, end));
                let (ds, de) = ((pos.x - xs).abs(), (pos.x - xe).abs());
                if ds <= GRAB_RADIUS && ds <= de {
                    return Hit::RangeStart;
                }
                if de <= GRAB_RADIUS {
                    return Hit::RangeEnd;
                }
            }
            return Hit::Ruler;
        }
        if y < layout.tracks_top() {
            return self
                .nearest_marker(layout, pos.x)
                .map_or(Hit::MarkerLane, Hit::Marker);
        }
        let row = ((y - layout.tracks_top()) / TRACK_HEIGHT) as usize;
        if row >= self.tracks.len() {
            return Hit::Empty;
        }
        let best = self.tracks[row]
            .keys
            .iter()
            .enumerate()
            .map(|(i, &t)| (i, (self.x_of(layout, t) - pos.x).abs()))
            .filter(|&(_, d)| d <= GRAB_RADIUS)
            .min_by(|a, b| a.1.total_cmp(&b.1));
        match best {
            Some((index, _)) => Hit::Key { track: row, index },
            None => Hit::Track(row),
        }
    }

    fn nearest_marker(&self, layout: &Layout, x: f32) -> Option<usize> {
        self.markers
            .iter()
            .enumerate()
            .map(|(i, m)| (i, (self.x_of(layout, m.time) - x).abs()))
            .filter(|&(_, d)| d <= GRAB_RADIUS)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(i, _)| i)
    }

    fn select(&mut self, selection: Option<Selection>, out: &mut Vec<TimelineEvent>) {
        if self.selection != selection {
            self.selection = selection;
            out.push(TimelineEvent::Selected(selection));
        }
    }

    fn set_range(&mut self, a: f32, b: f32, finished: bool, out: &mut Vec<TimelineEvent>) {
        let range = Some((a.min(b), a.max(b)));
        self.range = range;
        out.push(TimelineEvent::RangeChanged { range, finished });
    }

    /// Feed one pointer input; returns the edits it caused (also applied to `self`).
    pub fn pointer(&mut self, layout: &Layout, input: Pointer) -> Vec<TimelineEvent> {
        let mut out = Vec::new();
        match input {
            Pointer::Press { pos, button, shift } => {
                let hit = self.hit_test(layout, pos);
                let time = self.clamp_time(self.time_at(layout, pos.x));
                match (button, hit) {
                    (Button::Primary, Hit::Loop) => {
                        self.looping = !self.looping;
                        out.push(TimelineEvent::LoopChanged(self.looping));
                    }
                    (Button::Primary, Hit::RangeStart) => self.drag = Some(Drag::RangeStart),
                    (Button::Primary, Hit::RangeEnd) => self.drag = Some(Drag::RangeEnd),
                    (Button::Primary, Hit::Ruler) if shift => {
                        self.drag = Some(Drag::RangeNew(time));
                        self.set_range(time, time, false, &mut out);
                    }
                    (Button::Primary, Hit::Ruler) => {
                        self.drag = Some(Drag::Scrub);
                        self.playhead = time;
                        out.push(TimelineEvent::Scrub(time));
                    }
                    (Button::Primary, Hit::Marker(i)) => {
                        self.select(Some(Selection::Marker(i)), &mut out);
                        self.drag = Some(Drag::Marker(i));
                    }
                    (Button::Primary, Hit::Key { track, index }) => {
                        self.select(Some(Selection::Key { track, index }), &mut out);
                        self.drag = Some(Drag::Key { track, index });
                    }
                    (Button::Primary, Hit::Track(_) | Hit::MarkerLane | Hit::Empty) => {
                        self.select(None, &mut out);
                    }
                    (Button::Secondary, Hit::Ruler | Hit::RangeStart | Hit::RangeEnd) => {
                        if self.range.take().is_some() {
                            out.push(TimelineEvent::RangeChanged {
                                range: None,
                                finished: true,
                            });
                        }
                    }
                    (Button::Secondary, Hit::Marker(i)) => self.remove_marker(i, &mut out),
                    (Button::Secondary, Hit::Key { track, index }) => self.remove_key(track, index, &mut out),
                    (Button::Secondary, _) => {}
                }
            }
            Pointer::Drag { pos } => {
                let time = self.clamp_time(self.time_at(layout, pos.x));
                match self.drag {
                    Some(Drag::Scrub) => {
                        self.playhead = time;
                        out.push(TimelineEvent::Scrub(time));
                    }
                    Some(Drag::Key { track, index }) => self.move_key(track, index, time, false, &mut out),
                    Some(Drag::Marker(index)) => self.move_marker(index, time, false, &mut out),
                    Some(Drag::RangeStart) => {
                        let end = self.range.map_or(time, |r| r.1);
                        self.set_range(time, end, false, &mut out);
                    }
                    Some(Drag::RangeEnd) => {
                        let start = self.range.map_or(time, |r| r.0);
                        self.set_range(start, time, false, &mut out);
                    }
                    Some(Drag::RangeNew(anchor)) => self.set_range(anchor, time, false, &mut out),
                    None => {}
                }
            }
            Pointer::Release { pos, double } => {
                let time = self.clamp_time(self.time_at(layout, pos.x));
                match self.drag.take() {
                    Some(Drag::Key { track, index }) => self.move_key(track, index, time, true, &mut out),
                    Some(Drag::Marker(index)) => self.move_marker(index, time, true, &mut out),
                    Some(Drag::RangeStart | Drag::RangeEnd | Drag::RangeNew(_)) => {
                        let (a, b) = self.range.unwrap_or((time, time));
                        self.set_range(a, b, true, &mut out);
                    }
                    Some(Drag::Scrub) => {
                        self.playhead = time;
                        out.push(TimelineEvent::Scrub(time));
                    }
                    None => {}
                }
                if double {
                    match self.hit_test(layout, pos) {
                        Hit::Track(track) => {
                            self.tracks[track].keys.push(time);
                            let index = self.tracks[track].keys.len() - 1;
                            out.push(TimelineEvent::KeyAdded { track, time });
                            self.select(Some(Selection::Key { track, index }), &mut out);
                        }
                        Hit::MarkerLane => {
                            self.markers
                                .push(TimelineMarker::new(time, "notify", MarkerKind::Notify));
                            let index = self.markers.len() - 1;
                            out.push(TimelineEvent::MarkerAdded {
                                time,
                                kind: MarkerKind::Notify,
                            });
                            self.select(Some(Selection::Marker(index)), &mut out);
                        }
                        _ => {}
                    }
                }
            }
            Pointer::Delete => match self.selection {
                Some(Selection::Key { track, index }) => self.remove_key(track, index, &mut out),
                Some(Selection::Marker(i)) => self.remove_marker(i, &mut out),
                None => {}
            },
        }
        out
    }

    fn move_key(
        &mut self,
        track: usize,
        index: usize,
        time: f32,
        finished: bool,
        out: &mut Vec<TimelineEvent>,
    ) {
        if let Some(key) = self.tracks.get_mut(track).and_then(|t| t.keys.get_mut(index)) {
            *key = time;
            out.push(TimelineEvent::KeyMoved {
                track,
                index,
                time,
                finished,
            });
        }
    }

    fn move_marker(&mut self, index: usize, time: f32, finished: bool, out: &mut Vec<TimelineEvent>) {
        if let Some(marker) = self.markers.get_mut(index) {
            marker.time = time;
            out.push(TimelineEvent::MarkerMoved {
                index,
                time,
                finished,
            });
        }
    }

    fn remove_key(&mut self, track: usize, index: usize, out: &mut Vec<TimelineEvent>) {
        let Some(keys) = self.tracks.get_mut(track).map(|t| &mut t.keys) else {
            return;
        };
        if index >= keys.len() {
            return;
        }
        keys.remove(index);
        out.push(TimelineEvent::KeyRemoved { track, index });
        self.drag = None;
        self.select(None, out);
    }

    fn remove_marker(&mut self, index: usize, out: &mut Vec<TimelineEvent>) {
        if index >= self.markers.len() {
            return;
        }
        self.markers.remove(index);
        out.push(TimelineEvent::MarkerRemoved { index });
        self.drag = None;
        self.select(None, out);
    }

    /// Draw the timeline in `ui` and return the edits made this frame.
    pub fn show(&mut self, ui: &mut egui::Ui) -> Vec<TimelineEvent> {
        let (rect, response) = ui.allocate_exact_size(
            vec2(ui.available_width(), self.height()),
            egui::Sense::click_and_drag().union(egui::Sense::focusable_noninteractive()),
        );
        let layout = Layout { rect };
        self.last_layout = Some(layout);
        if self.pixels_per_second <= 0.0 {
            self.fit(layout.track_width());
        }
        let mut events = Vec::new();

        let (pressed, secondary, released, double, shift, delete, scroll, zoom_mod, hover) = ui.input(|i| {
            (
                i.pointer.primary_pressed(),
                i.pointer.secondary_pressed(),
                i.pointer.primary_released(),
                i.pointer.button_double_clicked(egui::PointerButton::Primary),
                i.modifiers.shift,
                i.key_pressed(egui::Key::Delete),
                i.smooth_scroll_delta,
                i.modifiers.command,
                i.pointer.hover_pos(),
            )
        });
        let latest = ui.input(|i| i.pointer.latest_pos());
        let over = hover.filter(|p| rect.contains(*p));
        if let Some(pos) = over {
            if pressed {
                events.extend(self.pointer(
                    &layout,
                    Pointer::Press {
                        pos,
                        button: Button::Primary,
                        shift,
                    },
                ));
            } else if secondary {
                events.extend(self.pointer(
                    &layout,
                    Pointer::Press {
                        pos,
                        button: Button::Secondary,
                        shift,
                    },
                ));
            }
            if scroll != egui::Vec2::ZERO {
                if zoom_mod {
                    let anchor = self.time_at(&layout, pos.x);
                    self.zoom_at((scroll.y * 0.01).exp(), anchor);
                } else {
                    let delta = if scroll.x != 0.0 { scroll.x } else { scroll.y };
                    self.pan(-delta / self.pixels_per_second.max(1e-3));
                }
            }
        }
        if self.drag.is_some()
            && !pressed
            && response.dragged()
            && let Some(pos) = latest
        {
            events.extend(self.pointer(&layout, Pointer::Drag { pos }));
        }
        if released && let Some(pos) = latest {
            events.extend(self.pointer(&layout, Pointer::Release { pos, double }));
        }
        if delete && (response.hovered() || response.has_focus()) {
            events.extend(self.pointer(&layout, Pointer::Delete));
        }

        self.paint(ui, &layout, over);
        events.extend(self.keys_list(ui, response.id, &layout));
        events
    }

    /// The rows of the key list: every key (track order, then key order), then every marker.
    fn key_rows(&self) -> Vec<Selection> {
        self.tracks
            .iter()
            .enumerate()
            .flat_map(|(track, t)| (0..t.keys.len()).map(move |index| Selection::Key { track, index }))
            .chain((0..self.markers.len()).map(Selection::Marker))
            .collect()
    }

    /// One keyboard step in time: the snap step, else a 30th of a second (Shift: ten).
    fn key_step(&self, coarse: bool) -> f32 {
        let step = self.snap.filter(|s| *s > 0.0).unwrap_or(1.0 / 30.0);
        if coarse { step * 10.0 } else { step }
    }

    /// The key list under the timeline (FU-5, the convention of `canvas_keys`): one row per key
    /// and marker. Up / Down move the focus; Enter selects the item (the click); Left / Right move
    /// it one step (the drag's `KeyMoved` / `MarkerMoved`, finished: one undo step; Shift: ten
    /// steps); Delete removes it (the right-click); Ctrl+Shift+A adds a key (a notify marker on a
    /// marker row) one step later (the double-click). The list's state lives in egui memory.
    fn keys_list(&mut self, ui: &mut egui::Ui, id: egui::Id, layout: &Layout) -> Vec<TimelineEvent> {
        use crate::canvas_keys::{RowKey, list_rows};
        let rows = self.key_rows();
        let keys_id = id.with("keys");
        let mut keys: crate::canvas_keys::ListKeys = ui.data(|d| d.get_temp(keys_id)).unwrap_or_default();
        if let Some(row) = keys.focused().and_then(|i| rows.get(i)) {
            let (time, y) = match *row {
                Selection::Key { track, index } => (
                    self.tracks[track].keys.get(index).copied().unwrap_or(0.0),
                    layout.tracks_top() + (track as f32 + 0.5) * TRACK_HEIGHT,
                ),
                Selection::Marker(i) => (
                    self.markers.get(i).map_or(0.0, |m| m.time),
                    layout.marker_top() + MARKER_HEIGHT * 0.5,
                ),
            };
            let at = egui::pos2(self.x_of(layout, time), y);
            crate::canvas_keys::focus_ring(
                &ui.painter_at(layout.rect),
                egui::Rect::from_center_size(at, vec2(8.0, 8.0)),
            );
        }
        crate::canvas_keys::name_canvas(
            ui.ctx(),
            id,
            &format!(
                "Timeline canvas, {} keys, {} markers",
                rows.len() - self.markers.len(),
                self.markers.len()
            ),
        );
        let name = |i: usize| match rows.get(i) {
            Some(Selection::Key { track, index }) => {
                let t = &self.tracks[*track];
                format!(
                    "{} key {} at {:.2} s",
                    t.name,
                    index + 1,
                    t.keys.get(*index).copied().unwrap_or(0.0)
                )
            }
            Some(Selection::Marker(i)) => self.markers.get(*i).map_or_else(String::new, |m| {
                format!("Marker {} ({}) at {:.2} s", m.label, m.kind.name(), m.time)
            }),
            None => String::new(),
        };
        let height =
            (ui.spacing().interact_size.y + ui.spacing().item_spacing.y) * rows.len().clamp(1, 6) as f32;
        let events = list_rows(
            ui,
            ("timeline-keys", id),
            &mut keys,
            rows.len(),
            height,
            None,
            &name,
        );
        ui.data_mut(|d| d.insert_temp(keys_id, keys));
        let mut out = Vec::new();
        for event in events {
            match event {
                RowKey::Activate(i) => {
                    if let Some(row) = rows.get(i) {
                        self.select(Some(*row), &mut out);
                    }
                }
                RowKey::Delete(i) => match rows.get(i) {
                    Some(Selection::Key { track, index }) => self.remove_key(*track, *index, &mut out),
                    Some(Selection::Marker(m)) => self.remove_marker(*m, &mut out),
                    None => {}
                },
                RowKey::Arrow { row, dx, coarse, .. } if dx != 0 => {
                    let step = dx as f32 * self.key_step(coarse);
                    match rows.get(row) {
                        Some(Selection::Key { track, index }) => {
                            let Some(t) = self.tracks[*track].keys.get(*index).copied() else {
                                continue;
                            };
                            let time = self.clamp_time(t + step);
                            self.move_key(*track, *index, time, true, &mut out);
                        }
                        Some(Selection::Marker(m)) => {
                            let Some(t) = self.markers.get(*m).map(|m| m.time) else {
                                continue;
                            };
                            let time = self.clamp_time(t + step);
                            self.move_marker(*m, time, true, &mut out);
                        }
                        None => {}
                    }
                }
                RowKey::Palette(i) => match rows.get(i) {
                    Some(Selection::Key { track, index }) => {
                        let track = *track;
                        let t = self.tracks[track].keys.get(*index).copied().unwrap_or(0.0);
                        let time = self.clamp_time(t + self.key_step(false));
                        self.tracks[track].keys.push(time);
                        let index = self.tracks[track].keys.len() - 1;
                        out.push(TimelineEvent::KeyAdded { track, time });
                        self.select(Some(Selection::Key { track, index }), &mut out);
                    }
                    Some(Selection::Marker(m)) => {
                        let t = self.markers.get(*m).map_or(0.0, |m| m.time);
                        let time = self.clamp_time(t + self.key_step(false));
                        self.markers
                            .push(TimelineMarker::new(time, "notify", MarkerKind::Notify));
                        let index = self.markers.len() - 1;
                        out.push(TimelineEvent::MarkerAdded {
                            time,
                            kind: MarkerKind::Notify,
                        });
                        self.select(Some(Selection::Marker(index)), &mut out);
                    }
                    None => {}
                },
                RowKey::Arrow { .. } | RowKey::Connect(_) => {}
            }
        }
        out
    }

    fn paint(&self, ui: &egui::Ui, layout: &Layout, hover: Option<Pos2>) {
        let rect = layout.rect;
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, Color32::from_gray(28));
        let area = Rect::from_min_max(
            Pos2::new(layout.track_area_left(), layout.ruler_top()),
            rect.right_bottom(),
        );
        let track_painter = painter.with_clip_rect(area);

        // Toolbar: loop button and time readout.
        let button = Rect::from_min_size(
            rect.min + vec2(2.0, 2.0),
            vec2(LOOP_BUTTON_WIDTH, TOOLBAR_HEIGHT - 4.0),
        );
        let on = self.looping;
        painter.rect_filled(
            button,
            3.0,
            if on {
                Color32::from_rgb(60, 110, 70)
            } else {
                Color32::from_gray(50)
            },
        );
        painter.text(
            button.center(),
            Align2::CENTER_CENTER,
            if on { "Loop: on" } else { "Loop: off" },
            FontId::proportional(11.0),
            Color32::from_gray(225),
        );
        painter.text(
            Pos2::new(button.right() + 8.0, rect.top() + TOOLBAR_HEIGHT * 0.5),
            Align2::LEFT_CENTER,
            format!("{:.3} / {:.3} s", self.playhead, self.duration),
            FontId::monospace(11.0),
            Color32::from_gray(190),
        );

        // Clip extent.
        let x0 = self.x_of(layout, 0.0);
        let x1 = self.x_of(layout, self.duration);
        track_painter.rect_filled(
            Rect::from_x_y_ranges(x0..=x1, layout.ruler_top()..=rect.bottom()),
            0.0,
            Color32::from_rgb(36, 42, 52),
        );
        // Range.
        if let Some((start, end)) = self.range {
            let (xs, xe) = (self.x_of(layout, start), self.x_of(layout, end));
            track_painter.rect_filled(
                Rect::from_x_y_ranges(xs..=xe, layout.ruler_top()..=rect.bottom()),
                0.0,
                Color32::from_rgba_unmultiplied(110, 150, 255, 36),
            );
            for x in [xs, xe] {
                track_painter.line_segment(
                    [Pos2::new(x, layout.ruler_top()), Pos2::new(x, rect.bottom())],
                    Stroke::new(1.5, Color32::from_rgb(110, 150, 255)),
                );
            }
        }
        // Ruler ticks: a step of 1, 2, 5 × 10ⁿ s that keeps labels about 70 points apart.
        let step = nice_step(70.0 / self.pixels_per_second.max(1e-3));
        let first = (self.view_start / step).floor() as i32;
        let last = (self.time_at(layout, rect.right()) / step).ceil() as i32;
        for n in first..=last {
            let t = n as f32 * step;
            let x = self.x_of(layout, t);
            track_painter.line_segment(
                [
                    Pos2::new(x, layout.ruler_top() + 12.0),
                    Pos2::new(x, layout.marker_top()),
                ],
                Stroke::new(1.0, Color32::from_gray(140)),
            );
            track_painter.text(
                Pos2::new(x + 2.0, layout.ruler_top()),
                Align2::LEFT_TOP,
                format!("{t:.2}s"),
                FontId::monospace(9.0),
                Color32::from_gray(165),
            );
        }
        // Track rows.
        for (row, track) in self.tracks.iter().enumerate() {
            let top = layout.tracks_top() + row as f32 * TRACK_HEIGHT;
            let strip = Rect::from_min_size(Pos2::new(rect.left(), top), vec2(rect.width(), TRACK_HEIGHT));
            if row % 2 == 1 {
                painter.rect_filled(strip, 0.0, Color32::from_white_alpha(6));
            }
            painter.text(
                Pos2::new(rect.left() + 6.0, top + TRACK_HEIGHT * 0.5),
                Align2::LEFT_CENTER,
                &track.name,
                FontId::proportional(11.0),
                Color32::from_gray(200),
            );
            for (index, &time) in track.keys.iter().enumerate() {
                let c = Pos2::new(self.x_of(layout, time), top + TRACK_HEIGHT * 0.5);
                let selected = self.selection == Some(Selection::Key { track: row, index });
                let hot = hover.is_some_and(|p| (p - c).length() <= GRAB_RADIUS);
                let r = if selected || hot { 5.0 } else { 4.0 };
                let diamond = vec![
                    c + vec2(0.0, -r),
                    c + vec2(r, 0.0),
                    c + vec2(0.0, r),
                    c + vec2(-r, 0.0),
                ];
                track_painter.add(egui::Shape::convex_polygon(
                    diamond,
                    if selected { Color32::WHITE } else { track.color },
                    Stroke::new(1.0, Color32::from_black_alpha(160)),
                ));
            }
        }
        // Markers (triangles on the lane's bottom edge, labels beside).
        let lane_bottom = layout.tracks_top() - 2.0;
        for (index, marker) in self.markers.iter().enumerate() {
            let x = self.x_of(layout, marker.time);
            let selected = self.selection == Some(Selection::Marker(index));
            let size = 12.0;
            track_painter.add(egui::Shape::convex_polygon(
                vec![
                    Pos2::new(x - size * 0.5, lane_bottom - size),
                    Pos2::new(x + size * 0.5, lane_bottom - size),
                    Pos2::new(x, lane_bottom),
                ],
                if selected {
                    Color32::WHITE
                } else {
                    marker.kind.color()
                },
                Stroke::NONE,
            ));
            track_painter.text(
                Pos2::new(x + 8.0, lane_bottom - size * 0.5),
                Align2::LEFT_CENTER,
                &marker.label,
                FontId::proportional(10.0),
                Color32::from_gray(210),
            );
        }
        // Playhead on top.
        let px = self.x_of(layout, self.playhead);
        track_painter.line_segment(
            [Pos2::new(px, layout.ruler_top()), Pos2::new(px, rect.bottom())],
            Stroke::new(2.0, Color32::from_rgb(255, 90, 90)),
        );
    }
}

fn curve_times<T>(curve: &TrackCurve<T>) -> Vec<f32> {
    match curve {
        TrackCurve::Static(_) => Vec::new(),
        TrackCurve::Animated { times, .. } => times.clone(),
    }
}

/// The smallest 1, 2 or 5 × 10ⁿ at least `min`.
fn nice_step(min: f32) -> f32 {
    let min = min.max(1e-4);
    let base = 10f32.powf(min.log10().floor());
    [1.0, 2.0, 5.0, 10.0]
        .iter()
        .map(|m| m * base)
        .find(|s| *s >= min)
        .unwrap_or(base * 10.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (Timeline, Layout) {
        let mut timeline = Timeline::new(2.0);
        timeline
            .tracks
            .push(TimelineTrack::new("hips", vec![0.0, 1.0, 2.0]));
        timeline.tracks.push(TimelineTrack::new("foot_l", vec![0.5]));
        timeline
            .markers
            .push(TimelineMarker::new(0.5, "plant_l", MarkerKind::FootContact));
        timeline
            .markers
            .push(TimelineMarker::new(1.5, "step", MarkerKind::Notify));
        let layout = Layout::new(Pos2::ZERO, LABEL_WIDTH + 400.0, &timeline);
        timeline.fit(layout.track_width()); // 200 px per second
        (timeline, layout)
    }

    fn at(timeline: &Timeline, layout: &Layout, time: f32, y: f32) -> Pos2 {
        Pos2::new(timeline.x_of(layout, time), y)
    }

    fn ruler_y() -> f32 {
        TOOLBAR_HEIGHT + RULER_HEIGHT * 0.5
    }

    fn marker_y() -> f32 {
        TOOLBAR_HEIGHT + RULER_HEIGHT + MARKER_HEIGHT * 0.5
    }

    fn track_y(row: usize) -> f32 {
        TOOLBAR_HEIGHT + RULER_HEIGHT + MARKER_HEIGHT + (row as f32 + 0.5) * TRACK_HEIGHT
    }

    fn press(pos: Pos2) -> Pointer {
        Pointer::Press {
            pos,
            button: Button::Primary,
            shift: false,
        }
    }

    #[test]
    fn scrubbing_the_ruler_moves_the_playhead_and_clamps() {
        let (mut t, l) = fixture();
        let events = t.pointer(&l, press(at(&t, &l, 0.75, ruler_y())));
        assert_eq!(events, vec![TimelineEvent::Scrub(0.75)]);
        let events = t.pointer(
            &l,
            Pointer::Drag {
                pos: at(&t, &l, 5.0, ruler_y()),
            },
        );
        assert_eq!(events, vec![TimelineEvent::Scrub(2.0)]);
        let events = t.pointer(
            &l,
            Pointer::Drag {
                pos: at(&t, &l, -1.0, 400.0),
            },
        );
        assert_eq!(events, vec![TimelineEvent::Scrub(0.0)]);
        assert_eq!(t.playhead, 0.0);
        t.pointer(
            &l,
            Pointer::Release {
                pos: Pos2::ZERO,
                double: false,
            },
        );
        assert!(
            t.pointer(
                &l,
                Pointer::Drag {
                    pos: at(&t, &l, 1.0, 0.0)
                }
            )
            .is_empty(),
            "drag over"
        );
    }

    #[test]
    fn snapping_rounds_to_the_frame_step() {
        let (mut t, l) = fixture();
        t.snap = Some(0.1);
        let events = t.pointer(&l, press(at(&t, &l, 0.734, ruler_y())));
        match events.as_slice() {
            [TimelineEvent::Scrub(time)] => assert!((time - 0.7).abs() < 1e-5, "{time}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn dragging_a_key_reports_each_step_and_a_finish() {
        let (mut t, l) = fixture();
        let events = t.pointer(&l, press(at(&t, &l, 1.0, track_y(0))));
        assert_eq!(
            events,
            vec![TimelineEvent::Selected(Some(Selection::Key {
                track: 0,
                index: 1
            }))]
        );
        let events = t.pointer(
            &l,
            Pointer::Drag {
                pos: at(&t, &l, 1.4, track_y(0)),
            },
        );
        assert_eq!(
            events,
            vec![TimelineEvent::KeyMoved {
                track: 0,
                index: 1,
                time: 1.4,
                finished: false
            }]
        );
        let events = t.pointer(
            &l,
            Pointer::Release {
                pos: at(&t, &l, 1.6, track_y(0)),
                double: false,
            },
        );
        assert_eq!(
            events,
            vec![TimelineEvent::KeyMoved {
                track: 0,
                index: 1,
                time: 1.6,
                finished: true
            }]
        );
        assert_eq!(t.tracks[0].keys, vec![0.0, 1.6, 2.0]);
    }

    #[test]
    fn markers_move_add_and_remove() {
        let (mut t, l) = fixture();
        t.pointer(&l, press(at(&t, &l, 1.5, marker_y())));
        let events = t.pointer(
            &l,
            Pointer::Release {
                pos: at(&t, &l, 1.25, marker_y()),
                double: false,
            },
        );
        assert_eq!(
            events,
            vec![TimelineEvent::MarkerMoved {
                index: 1,
                time: 1.25,
                finished: true
            }]
        );

        // Double click on the empty lane adds a notify.
        let pos = at(&t, &l, 1.0, marker_y());
        t.pointer(&l, press(pos));
        let events = t.pointer(&l, Pointer::Release { pos, double: true });
        assert_eq!(
            events,
            vec![
                TimelineEvent::MarkerAdded {
                    time: 1.0,
                    kind: MarkerKind::Notify
                },
                TimelineEvent::Selected(Some(Selection::Marker(2))),
            ]
        );
        assert_eq!(t.markers.len(), 3);

        // Delete removes the selection.
        let events = t.pointer(&l, Pointer::Delete);
        assert_eq!(
            events,
            vec![
                TimelineEvent::MarkerRemoved { index: 2 },
                TimelineEvent::Selected(None)
            ]
        );
        // Secondary click removes the marker under it.
        let events = t.pointer(
            &l,
            Pointer::Press {
                pos: at(&t, &l, 0.5, marker_y()),
                button: Button::Secondary,
                shift: false,
            },
        );
        assert_eq!(events[0], TimelineEvent::MarkerRemoved { index: 0 });
        assert_eq!(t.markers.len(), 1);
    }

    #[test]
    fn double_click_adds_a_key_and_delete_removes_it() {
        let (mut t, l) = fixture();
        let pos = at(&t, &l, 1.2, track_y(1));
        t.pointer(&l, press(pos));
        let events = t.pointer(&l, Pointer::Release { pos, double: true });
        assert_eq!(events[0], TimelineEvent::KeyAdded { track: 1, time: 1.2 });
        assert_eq!(t.tracks[1].keys, vec![0.5, 1.2]);
        assert_eq!(t.selection, Some(Selection::Key { track: 1, index: 1 }));
        let events = t.pointer(&l, Pointer::Delete);
        assert_eq!(events[0], TimelineEvent::KeyRemoved { track: 1, index: 1 });
        assert_eq!(t.tracks[1].keys, vec![0.5]);
        // Nothing selected now: Delete is a no-op.
        assert!(t.pointer(&l, Pointer::Delete).is_empty());
    }

    #[test]
    fn range_is_drawn_dragged_and_cleared() {
        let (mut t, l) = fixture();
        let shift_press = Pointer::Press {
            pos: at(&t, &l, 1.5, ruler_y()),
            button: Button::Primary,
            shift: true,
        };
        t.pointer(&l, shift_press);
        let events = t.pointer(
            &l,
            Pointer::Drag {
                pos: at(&t, &l, 0.5, ruler_y()),
            },
        );
        assert_eq!(
            events,
            vec![TimelineEvent::RangeChanged {
                range: Some((0.5, 1.5)),
                finished: false
            }]
        );
        let events = t.pointer(
            &l,
            Pointer::Release {
                pos: at(&t, &l, 0.5, ruler_y()),
                double: false,
            },
        );
        assert_eq!(
            events,
            vec![TimelineEvent::RangeChanged {
                range: Some((0.5, 1.5)),
                finished: true
            }]
        );
        // Grab the start handle (not a scrub) and push it past the end: the ends swap.
        assert_eq!(t.hit_test(&l, at(&t, &l, 0.5, ruler_y())), Hit::RangeStart);
        t.pointer(&l, press(at(&t, &l, 0.5, ruler_y())));
        let events = t.pointer(
            &l,
            Pointer::Drag {
                pos: at(&t, &l, 1.9, ruler_y()),
            },
        );
        assert_eq!(
            events,
            vec![TimelineEvent::RangeChanged {
                range: Some((1.5, 1.9)),
                finished: false
            }]
        );
        t.pointer(
            &l,
            Pointer::Release {
                pos: Pos2::ZERO,
                double: false,
            },
        );
        // Secondary click clears.
        let events = t.pointer(
            &l,
            Pointer::Press {
                pos: at(&t, &l, 0.2, ruler_y()),
                button: Button::Secondary,
                shift: false,
            },
        );
        assert_eq!(
            events,
            vec![TimelineEvent::RangeChanged {
                range: None,
                finished: true
            }]
        );
        assert_eq!(t.range, None);
    }

    #[test]
    fn loop_button_toggles() {
        let (mut t, l) = fixture();
        let pos = Pos2::new(LOOP_BUTTON_WIDTH * 0.5, TOOLBAR_HEIGHT * 0.5);
        assert_eq!(t.pointer(&l, press(pos)), vec![TimelineEvent::LoopChanged(true)]);
        assert_eq!(t.pointer(&l, press(pos)), vec![TimelineEvent::LoopChanged(false)]);
    }

    #[test]
    fn zoom_keeps_the_anchor_and_pan_scrolls() {
        let (mut t, l) = fixture();
        let before = t.x_of(&l, 1.0);
        t.zoom_at(2.0, 1.0);
        assert!((t.x_of(&l, 1.0) - before).abs() < 1e-3);
        assert!((t.pixels_per_second - 400.0).abs() < 1e-3);
        t.pan(0.5);
        assert!(t.x_of(&l, 1.0) < before);
    }

    #[test]
    fn empty_clicks_clear_the_selection_and_out_of_range_hits_are_empty() {
        let (mut t, l) = fixture();
        t.pointer(&l, press(at(&t, &l, 1.0, track_y(0))));
        assert!(t.selection.is_some());
        let events = t.pointer(&l, press(at(&t, &l, 1.3, track_y(0))));
        assert_eq!(events, vec![TimelineEvent::Selected(None)]);
        assert_eq!(t.hit_test(&l, Pos2::new(-5.0, -5.0)), Hit::Empty);
        assert_eq!(
            t.hit_test(&l, Pos2::new(5.0, track_y(0))),
            Hit::Empty,
            "label column"
        );
    }

    #[test]
    fn from_clip_lists_animated_joints_and_notifies() {
        use regia_animation::clip::{JointTrack, Notify};
        use regia_core::math::{Quat, Vec3};
        let mut clip = AnimationClip::empty("walk", 2.0);
        clip.tracks.push(JointTrack {
            joint: 3,
            translations: TrackCurve::Animated {
                times: vec![0.0, 1.0],
                values: vec![Vec3::ZERO, Vec3::X],
                exact_rotation: false,
            },
            rotations: TrackCurve::Animated {
                times: vec![0.0, 0.5, 1.0],
                values: vec![Quat::IDENTITY; 3],
                exact_rotation: false,
            },
            scales: TrackCurve::Static(Vec3::ONE),
        });
        for (time, name) in [(0.25, "Footstep_L"), (1.0, "Whoosh")] {
            clip.notifies.push(Notify {
                time,
                name: name.into(),
                payload: None,
            });
        }
        let t = Timeline::from_clip(&clip, None);
        assert_eq!(t.duration, 2.0);
        assert_eq!(t.tracks[0].name, "joint 3");
        assert_eq!(t.tracks[0].keys, vec![0.0, 0.5, 1.0]);
        assert_eq!(t.markers[0].kind, MarkerKind::FootContact);
        assert_eq!(t.markers[1].kind, MarkerKind::Notify);
    }

    #[test]
    fn nice_steps_are_one_two_five() {
        assert!((nice_step(0.3) - 0.5).abs() < 1e-6);
        assert!((nice_step(0.07) - 0.1).abs() < 1e-6);
        assert!((nice_step(1.5) - 2.0).abs() < 1e-6);
        assert!((nice_step(7.0) - 10.0).abs() < 1e-6);
    }

    /// The egui front end: a frame with synthetic pointer events scrubs the playhead.
    #[test]
    fn show_turns_egui_pointer_input_into_events() {
        let ctx = egui::Context::default();
        let mut timeline = Timeline::new(2.0);
        timeline.tracks.push(TimelineTrack::new("hips", vec![0.0, 1.0]));
        let screen = Rect::from_min_size(Pos2::ZERO, vec2(600.0, 200.0));
        let mut all = Vec::new();
        let run = |timeline: &mut Timeline, events: Vec<egui::Event>, time: f64| {
            let raw = egui::RawInput {
                screen_rect: Some(screen),
                events,
                time: Some(time),
                ..Default::default()
            };
            let mut got = Vec::new();
            let mut output = ctx.run_ui(raw, |ui| {
                egui::CentralPanel::default().show(ui, |ui| got = timeline.show(ui));
            });
            output.textures_delta.clear();
            got
        };
        run(&mut timeline, vec![], 0.0);
        run(&mut timeline, vec![], 0.1);
        let layout = timeline.layout().expect("drawn");
        let pos = Pos2::new(
            timeline.x_of(&layout, 1.0),
            layout.rect.top() + TOOLBAR_HEIGHT + RULER_HEIGHT * 0.5,
        );
        let button = |pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        run(&mut timeline, vec![egui::Event::PointerMoved(pos)], 0.2);
        all.extend(run(&mut timeline, vec![button(true)], 0.3));
        all.extend(run(&mut timeline, vec![button(false)], 0.4));
        let scrubbed = all
            .iter()
            .any(|e| matches!(e, TimelineEvent::Scrub(t) if (t - 1.0).abs() < 0.05));
        assert!(scrubbed, "{all:?} playhead {}", timeline.playhead);
    }

    /// FU-5: the key list selects a key (Enter), moves it one snap step (Right: one finished
    /// `KeyMoved`, one undo step for the panel) and removes a marker (Delete) with keys only; the
    /// timeline's keys, markers and selection equal the ones the mouse's click, drag and
    /// right-click leave.
    #[test]
    fn the_key_list_edits_by_keyboard_what_the_mouse_edits() {
        use crate::canvas_keys::test_input::{frame, key, labels_of, tab_to};
        use egui::{Key, Modifiers};
        const SIZE: [f32; 2] = [700.0, 600.0];
        let (mut mouse, layout) = fixture();
        mouse.snap = Some(0.1);
        mouse.pointer(&layout, press(at(&mouse, &layout, 1.0, track_y(0))));
        mouse.pointer(
            &layout,
            Pointer::Drag {
                pos: at(&mouse, &layout, 1.1, track_y(0)),
            },
        );
        let moved = mouse.pointer(
            &layout,
            Pointer::Release {
                pos: at(&mouse, &layout, 1.1, track_y(0)),
                double: false,
            },
        );
        assert!(
            moved
                .iter()
                .any(|e| matches!(e, TimelineEvent::KeyMoved { finished: true, .. }))
        );
        mouse.pointer(
            &layout,
            Pointer::Press {
                pos: at(&mouse, &layout, 1.5, marker_y()),
                button: Button::Secondary,
                shift: false,
            },
        );

        let (mut timeline, _) = fixture();
        timeline.snap = Some(0.1);
        let ctx = egui::Context::default();
        let mut all = Vec::new();
        let first = frame(&ctx, SIZE, vec![], |ui| all.extend(timeline.show(ui)));
        assert!(
            labels_of(&first, egui::accesskit::Role::Canvas)
                .contains(&"Timeline canvas, 4 keys, 2 markers".to_string()),
            "{:?}",
            labels_of(&first, egui::accesskit::Role::Canvas)
        );
        tab_to(&ctx, SIZE, "hips key 2 at 1.00 s", false, 20, |ui| {
            all.extend(timeline.show(ui));
        });
        frame(&ctx, SIZE, vec![key(Key::Enter, Modifiers::NONE)], |ui| {
            all.extend(timeline.show(ui))
        });
        assert_eq!(
            timeline.selection,
            Some(Selection::Key { track: 0, index: 1 }),
            "Enter selects"
        );
        all.clear();
        frame(&ctx, SIZE, vec![key(Key::ArrowRight, Modifiers::NONE)], |ui| {
            all.extend(timeline.show(ui));
        });
        let finished: Vec<_> = all
            .iter()
            .filter(|e| matches!(e, TimelineEvent::KeyMoved { finished: true, .. }))
            .collect();
        assert_eq!(finished.len(), 1, "one finished move per press: {all:?}");
        for _ in 0..4 {
            frame(&ctx, SIZE, vec![key(Key::ArrowDown, Modifiers::NONE)], |ui| {
                all.extend(timeline.show(ui));
            });
        }
        frame(&ctx, SIZE, vec![key(Key::Delete, Modifiers::NONE)], |ui| {
            all.extend(timeline.show(ui))
        });
        assert_eq!(timeline.tracks, mouse.tracks, "same keys");
        assert_eq!(timeline.markers, mouse.markers, "same markers");
        assert_eq!(timeline.selection, mouse.selection, "same selection");
    }
}
