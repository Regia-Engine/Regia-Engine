// SPDX-License-Identifier: MPL-2.0
//! The Motion Matching debug panel (`docs/specs/52-motion-matching.md` § 2.11, § 6; ADR-2409): for
//! the selected character (an entity with `MotionMatching`), what plays, the counters and the
//! candidate table of the latest search, each row with its cost split per feature group as a
//! stacked bar, plus a view of the database the character uses (asset path, frames, clips with
//! their frame ranges and tags, the k-d tree, build warnings). Agent surface:
//! `editor.mm_panel_get`, `editor.mm_panel_select`, `editor.mm_panel_freeze`
//! (`book/agents/motion-matching.md`).
//!
//! Everything the panel draws comes from [`MotionMatchPanel::snapshot`] and [`database_view`],
//! plain data built from the world, so it is tested without a window. A bar segment's class says
//! how the group's cost compares with the best candidate's: `below` (cheaper or equal), `above`
//! (dearer) or `ignored` (the group's weight is 0); the view colors them with the theme's `ok`,
//! `alert` and muted tokens. `Freeze` keeps the current report while the game plays on.
//!
//! The database tab (ADR-2858) sorts the clip list and the tag list ([`ClipSort`]), shows the
//! cost parts of every candidate as numbers next to the bars, and, when the schema has contact
//! labels, a contact and phase strip of the playing clip ([`contact_strip`]): one cell per
//! frame and foot, filled while the foot is planted, with the phase as a tick and the current
//! frame marked.

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_animation::motion_matching::{
    CandidateRow, FeatureGroup, MotionDatabase, MotionDatabases, MotionMatching, Playback, SearchMethod,
};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::data_panels::PanelError;
use crate::viewport_agent::Handler;

const HINT: &str = "see book/agents/motion-matching.md";
/// Default number of candidate rows.
pub const DEFAULT_CANDIDATES: usize = 8;
/// Most candidate rows (the matcher's top-k limit).
pub const MAX_CANDIDATES: usize = 32;

fn err(code: &str, message: impl Into<String>) -> PanelError {
    PanelError::new(&format!("mm_panel_{code}"), message)
}

/// How a group's cost compares with the best candidate's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SegmentClass {
    /// Cheaper than (or equal to) the best candidate's cost for this group.
    Below,
    /// Dearer than the best candidate's.
    Above,
    /// The group's weight is 0: it is in the vector but costs nothing.
    Ignored,
}

impl SegmentClass {
    /// `below`, `above` or `ignored`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Below => "below",
            Self::Above => "above",
            Self::Ignored => "ignored",
        }
    }
}

/// One segment of a candidate's stacked cost bar.
#[derive(Clone, Debug, PartialEq)]
pub struct BarSegment {
    /// Group name (`trajectory_position`, ...).
    pub group: &'static str,
    /// The group's share of the distance.
    pub cost: f32,
    /// Compared with the best candidate.
    pub class: SegmentClass,
}

/// One row of the candidate table.
#[derive(Clone, Debug, PartialEq)]
pub struct CandidateLine {
    /// Database frame.
    pub frame: usize,
    /// Clip index.
    pub clip: usize,
    /// Clip name.
    pub clip_name: String,
    /// Clip time, seconds.
    pub time: f32,
    /// From a mirrored range.
    pub mirrored: bool,
    /// What the jump rule compares.
    pub total: f32,
    /// The transition cost paid.
    pub transition: f32,
    /// The landing bias.
    pub bias: f32,
    /// The loop-clip or stay bias.
    pub extra_bias: f32,
    /// `best`, `continuing`, `candidate`, `pass_through`, `locked_out`, `near_playing`, `filtered`.
    pub status: &'static str,
    /// The stacked bar, one segment per group of the schema.
    pub groups: Vec<BarSegment>,
}

/// What the panel shows for one character.
#[derive(Clone, Debug, PartialEq)]
pub struct MmSnapshot {
    /// The character.
    pub entity: Entity,
    /// The database's asset path, when it is registered.
    pub asset: Option<String>,
    /// `(clip name, time, frame)` of what plays.
    pub playing: Option<(String, f32, usize)>,
    /// Searches so far.
    pub searches: u64,
    /// Jumps so far.
    pub jumps: u64,
    /// The method the latest search ran.
    pub method_used: Option<&'static str>,
    /// The candidates, cheapest first.
    pub rows: Vec<CandidateLine>,
    /// The playing frame's row.
    pub continuation: Option<CandidateLine>,
}

/// The groups the schema lays out (the baseline five always, the others when present).
fn schema_groups(database: &MotionDatabase) -> Vec<FeatureGroup> {
    let schema = database.schema();
    FeatureGroup::ALL
        .iter()
        .copied()
        .filter(|&g| FeatureGroup::BASELINE.contains(&g) || !schema.group_range(g).is_empty())
        .collect()
}

/// The stacked bar of `row`, each group classed against `best` (the best candidate's row; with
/// none every weighted group is `below`).
#[must_use]
pub fn bar_segments(
    database: &MotionDatabase,
    row: &CandidateRow,
    best: Option<&CandidateRow>,
) -> Vec<BarSegment> {
    let weights = database.schema().weights;
    schema_groups(database)
        .into_iter()
        .map(|group| {
            let cost = row.parts.groups[group.index()];
            let class = if weights.get(group) == 0.0 {
                SegmentClass::Ignored
            } else if best.is_some_and(|b| cost > b.parts.groups[group.index()]) {
                SegmentClass::Above
            } else {
                SegmentClass::Below
            };
            BarSegment {
                group: group.name(),
                cost,
                class,
            }
        })
        .collect()
}

fn line(database: &MotionDatabase, row: &CandidateRow, best: Option<&CandidateRow>) -> CandidateLine {
    CandidateLine {
        frame: row.frame,
        clip: row.clip,
        clip_name: database
            .clips()
            .get(row.clip)
            .map_or_else(String::new, |c| c.name.clone()),
        time: row.time,
        mirrored: row.mirrored,
        total: row.total,
        transition: row.transition,
        bias: row.parts.bias,
        extra_bias: row.extra_bias,
        status: row.status.name(),
        groups: bar_segments(database, row, best),
    }
}

fn method_name(method: SearchMethod) -> &'static str {
    method.name()
}

/// The asset path a character's database is registered under.
fn asset_of(world: &World, matching: &MotionMatching) -> Option<String> {
    let database = matching.base_database.as_ref().unwrap_or(&matching.database);
    world
        .get_resource::<MotionDatabases>()
        .and_then(|d| d.path_of_database(database))
        .map(str::to_owned)
}

/// Every character with motion matching: `(entity, asset path, frames)`, by entity bits.
#[must_use]
pub fn characters(world: &World) -> Vec<(Entity, Option<String>, usize)> {
    let Some(mut query) = world.try_query::<(Entity, &MotionMatching)>() else {
        return Vec::new();
    };
    let mut out: Vec<(Entity, Option<String>, usize)> = query
        .iter(world)
        .map(|(e, m)| (e, asset_of(world, m), m.database.len()))
        .collect();
    out.sort_by_key(|(e, _, _)| e.to_bits());
    out
}

/// How the database tab orders its clip and tag lists.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ClipSort {
    /// Database order (tags in bit order).
    #[default]
    Index,
    /// By name (clips and tags).
    Name,
    /// By frame count, most first (clips and tags).
    Frames,
    /// By duration, longest first (tags in bit order).
    Duration,
    /// By the clip's first tag name (tags by name).
    Tags,
}

impl ClipSort {
    /// Every order, for the view's selector.
    pub const ALL: [ClipSort; 5] = [
        ClipSort::Index,
        ClipSort::Name,
        ClipSort::Frames,
        ClipSort::Duration,
        ClipSort::Tags,
    ];

    /// `index`, `name`, `frames`, `duration` or `tags`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            ClipSort::Index => "index",
            ClipSort::Name => "name",
            ClipSort::Frames => "frames",
            ClipSort::Duration => "duration",
            ClipSort::Tags => "tags",
        }
    }

    /// The order named `name`.
    #[must_use]
    pub fn from_name(name: &str) -> Option<ClipSort> {
        ClipSort::ALL.into_iter().find(|s| s.name() == name)
    }
}

/// The contact and phase strip of the playing clip (schemas with contact labels only).
#[derive(Clone, Debug, PartialEq)]
pub struct ContactStrip {
    /// The playing clip's name.
    pub clip_name: String,
    /// Feet per frame.
    pub feet: usize,
    /// Per frame of the clip: the contact label per foot.
    pub contacts: Vec<Vec<bool>>,
    /// Per frame of the clip: the phase per foot (`None` = undefined; empty without the phase
    /// group).
    pub phases: Vec<Vec<Option<f32>>>,
    /// The playing frame's index within the clip.
    pub current: usize,
}

/// The strip of the clip `playback` plays, `None` when the schema has no contact labels.
#[must_use]
pub fn contact_strip(database: &MotionDatabase, playback: Playback) -> Option<ContactStrip> {
    database.schema().contacts?;
    let range = database.clips().get(playback.clip)?;
    let frames = range.start..range.start + range.frames;
    Some(ContactStrip {
        clip_name: range.name.clone(),
        feet: database.schema().feet.len(),
        contacts: frames.clone().filter_map(|f| database.contacts(f)).collect(),
        phases: frames.filter_map(|f| database.phases(f)).collect(),
        current: playback.frame.saturating_sub(range.start),
    })
}

fn strip_json(strip: &ContactStrip) -> Value {
    json!({
        "clip_name": strip.clip_name,
        "feet": strip.feet,
        "current": strip.current,
        "contacts": strip.contacts,
        "phases": strip.phases,
    })
}

/// The database a character uses, as the panel's database tab shows it (database order).
#[must_use]
pub fn database_view(world: &World, entity: Entity) -> Option<Value> {
    database_view_sorted(world, entity, ClipSort::Index, false)
}

/// [`database_view`] with the clip and tag lists in `sort` order (`descending` reverses it), and
/// the frame count of every tag.
#[must_use]
pub fn database_view_sorted(
    world: &World,
    entity: Entity,
    sort: ClipSort,
    descending: bool,
) -> Option<Value> {
    let matching = world.get::<MotionMatching>(entity)?;
    let database = matching.base_database.as_ref().unwrap_or(&matching.database);
    let table = database.tag_table();
    let mut order: Vec<usize> = (0..database.clips().len()).collect();
    let first_tag = |i: usize| {
        let c = &database.clips()[i];
        database
            .frame_meta(c.start)
            .map(|m| {
                table
                    .names_of(m.tags)
                    .first()
                    .map(|t| (*t).to_string())
                    .unwrap_or_default()
            })
            .unwrap_or_default()
    };
    // Every key sorts ascending; `descending` reverses once (ADR-2858).
    match sort {
        ClipSort::Index => {}
        ClipSort::Name => order.sort_by_key(|&i| database.clips()[i].name.clone()),
        ClipSort::Frames => order.sort_by_key(|&i| database.clips()[i].frames),
        ClipSort::Duration => order.sort_by(|&a, &b| {
            database.clips()[a]
                .duration
                .total_cmp(&database.clips()[b].duration)
        }),
        ClipSort::Tags => order.sort_by_key(|&i| first_tag(i)),
    }
    if descending {
        order.reverse();
    }
    let mut tags: Vec<(String, u32)> = table
        .names()
        .iter()
        .enumerate()
        .map(|(bit, name)| (name.clone(), database.tag_count(bit)))
        .collect();
    match sort {
        ClipSort::Name | ClipSort::Tags => tags.sort_by_key(|t| t.0.clone()),
        ClipSort::Frames => tags.sort_by_key(|t| t.1),
        ClipSort::Index | ClipSort::Duration => {}
    }
    if descending {
        tags.reverse();
    }
    let clips: Vec<Value> = order
        .into_iter()
        .map(|i| {
            let c = &database.clips()[i];
            let tags = database
                .frame_meta(c.start)
                .map(|m| table.names_of(m.tags))
                .unwrap_or_default();
            json!({
                "index": i,
                "name": c.name,
                "duration": c.duration,
                "looping": c.looping,
                "mirrored": c.mirrored,
                "distance_matched": c.distance_matched,
                "frames": [c.start, c.start + c.frames],
                "tags": tags,
            })
        })
        .collect();
    let kd = database.kd_stats();
    Some(json!({
        "asset": asset_of(world, matching),
        "frames": database.len(),
        "dimensions": database.dimensions(),
        "groups": schema_groups(database).iter().map(|g| json!({
            "name": g.name(),
            "weight": database.schema().weights.get(*g),
        })).collect::<Vec<_>>(),
        "clips": clips,
        "sort": sort.name(),
        "descending": descending,
        "tag_frames": tags.iter().map(|(name, frames)| json!({ "name": name, "frames": frames })).collect::<Vec<_>>(),
        "contacts": database.schema().contacts.is_some(),
        "kd": { "nodes": kd.nodes, "leaves": kd.leaves, "depth": kd.depth },
        "content_hash": format!("{:016x}", database.content_hash()),
        "warnings": database.warnings().iter().map(|w| json!({
            "clip": w.clip,
            "kind": w.kind.name(),
        })).collect::<Vec<_>>(),
    }))
}

/// The Motion Matching panel's state (an edit-world resource).
#[derive(Resource, Debug, Clone, PartialEq)]
pub struct MotionMatchPanel {
    /// The selected character.
    pub entity: Option<Entity>,
    /// Candidate rows to show (1..32).
    pub candidates: usize,
    /// A frozen report (shown instead of the live one).
    pub frozen: Option<MmSnapshot>,
    /// The last failed operation.
    pub error: Option<PanelError>,
    /// The database tab's clip and tag order.
    pub sort: ClipSort,
    /// Reverse [`MotionMatchPanel::sort`].
    pub descending: bool,
}

impl Default for MotionMatchPanel {
    fn default() -> Self {
        Self {
            entity: None,
            candidates: DEFAULT_CANDIDATES,
            frozen: None,
            error: None,
            sort: ClipSort::Index,
            descending: false,
        }
    }
}

impl MotionMatchPanel {
    /// The selected character, or the first one when none is selected (or it is gone).
    #[must_use]
    pub fn target(&self, world: &World) -> Option<Entity> {
        self.entity
            .filter(|&e| world.get::<MotionMatching>(e).is_some())
            .or_else(|| characters(world).first().map(|(e, _, _)| *e))
    }

    /// The live report of the target character (the frozen one when frozen).
    ///
    /// # Errors
    ///
    /// `mm_panel_no_character` when nothing has motion matching.
    pub fn snapshot(&self, world: &World) -> Result<MmSnapshot, PanelError> {
        if let Some(frozen) = &self.frozen {
            return Ok(frozen.clone());
        }
        let entity = self
            .target(world)
            .ok_or_else(|| err("no_character", "no entity has a MotionMatching component"))?;
        live_snapshot(world, entity, self.candidates)
    }

    /// Select `entity` (it must have motion matching); a frozen report is dropped.
    ///
    /// # Errors
    ///
    /// `mm_panel_not_found` for an entity without `MotionMatching`.
    pub fn select(&mut self, world: &World, entity: Entity) -> Result<(), PanelError> {
        if world.get::<MotionMatching>(entity).is_none() {
            return Err(err(
                "not_found",
                format!("entity {} has no MotionMatching", entity.to_bits()),
            ));
        }
        self.entity = Some(entity);
        self.frozen = None;
        Ok(())
    }

    /// Freeze the current report, or unfreeze.
    ///
    /// # Errors
    ///
    /// As [`MotionMatchPanel::snapshot`].
    pub fn freeze(&mut self, world: &World, frozen: bool) -> Result<(), PanelError> {
        self.frozen = None;
        if frozen {
            self.frozen = Some(self.snapshot(world)?);
        }
        Ok(())
    }

    /// The agent's view of the panel.
    #[must_use]
    pub fn json(&self, world: &World) -> Value {
        let snapshot = self.snapshot(world);
        let target = self.target(world);
        json!({
            "characters": characters(world).iter().map(|(e, asset, frames)| json!({
                "entity": e.to_bits(),
                "asset": asset,
                "frames": frames,
            })).collect::<Vec<_>>(),
            "selected": target.map(Entity::to_bits),
            "candidates": self.candidates,
            "frozen": self.frozen.is_some(),
            "report": snapshot.as_ref().ok().map(snapshot_json),
            "database": target.and_then(|e| database_view_sorted(world, e, self.sort, self.descending)),
            "strip": target.and_then(|e| strip_of(world, e)).as_ref().map(strip_json),
            "error": self.error.as_ref().map(|e| json!({ "code": e.code, "message": e.message })),
        })
    }
}

/// The contact strip of the clip `entity` plays.
fn strip_of(world: &World, entity: Entity) -> Option<ContactStrip> {
    let matching = world.get::<MotionMatching>(entity)?;
    contact_strip(&matching.database, matching.matcher.playback()?)
}

/// The live report of `entity` with `k` candidates.
///
/// # Errors
///
/// `mm_panel_not_found` for an entity without `MotionMatching`.
pub fn live_snapshot(world: &World, entity: Entity, k: usize) -> Result<MmSnapshot, PanelError> {
    let matching = world.get::<MotionMatching>(entity).ok_or_else(|| {
        err(
            "not_found",
            format!("entity {} has no MotionMatching", entity.to_bits()),
        )
    })?;
    let database = &matching.database;
    let report = matching
        .matcher
        .candidate_report(database, k.clamp(1, MAX_CANDIDATES));
    let playing = matching.matcher.playback().map(|p| {
        let name = database
            .clips()
            .get(p.clip)
            .map_or_else(String::new, |c| c.name.clone());
        (name, p.time, p.frame)
    });
    let (rows, continuation, method_used) = match &report {
        Some(report) => {
            let best = report.rows.iter().find(|r| r.status.name() == "best");
            (
                report.rows.iter().map(|r| line(database, r, best)).collect(),
                report.continuation.as_ref().map(|r| line(database, r, best)),
                report.method_used.map(method_name),
            )
        }
        None => (Vec::new(), None, None),
    };
    Ok(MmSnapshot {
        entity,
        asset: asset_of(world, matching),
        playing,
        searches: matching.matcher.searches(),
        jumps: matching.matcher.jumps(),
        method_used,
        rows,
        continuation,
    })
}

fn line_json(line: &CandidateLine) -> Value {
    json!({
        "frame": line.frame,
        "clip": line.clip,
        "clip_name": line.clip_name,
        "time": line.time,
        "mirrored": line.mirrored,
        "total": line.total,
        "transition": line.transition,
        "bias": line.bias,
        "extra_bias": line.extra_bias,
        "status": line.status,
        "groups": line.groups.iter().map(|s| json!({
            "group": s.group,
            "cost": s.cost,
            "class": s.class.name(),
        })).collect::<Vec<_>>(),
    })
}

fn snapshot_json(s: &MmSnapshot) -> Value {
    json!({
        "entity": s.entity.to_bits(),
        "asset": s.asset,
        "playing": s.playing.as_ref().map(|(clip, time, frame)| json!({ "clip_name": clip, "time": time, "frame": frame })),
        "searches": s.searches,
        "jumps": s.jumps,
        "method_used": s.method_used,
        "rows": s.rows.iter().map(line_json).collect::<Vec<_>>(),
        "continuation": s.continuation.as_ref().map(line_json),
    })
}

// --------------------------------------------------------------------------------------- view

fn segment_color(class: SegmentClass, tokens: &crate::theme::ThemeTokens) -> egui::Color32 {
    match class {
        SegmentClass::Below => tokens.accents.ok,
        SegmentClass::Above => tokens.accents.alert,
        SegmentClass::Ignored => tokens.muted,
    }
}

fn draw_bar(ui: &mut egui::Ui, line: &CandidateLine, scale: f32, tokens: &crate::theme::ThemeTokens) {
    let width = 160.0;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, 12.0), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    let mut x = rect.left();
    for segment in &line.groups {
        let w = if scale > 0.0 && segment.cost.is_finite() {
            (segment.cost / scale * width).clamp(0.0, width)
        } else {
            0.0
        };
        if w > 0.0 {
            let r = egui::Rect::from_min_size(egui::pos2(x, rect.top()), egui::vec2(w, rect.height()));
            painter.rect_filled(r, 0.0, segment_color(segment.class, tokens));
            painter.rect_stroke(
                r,
                0.0,
                egui::Stroke::new(1.0, tokens.line),
                egui::StrokeKind::Inside,
            );
            x += w;
        }
    }
    let tip: String = line
        .groups
        .iter()
        .map(|s| format!("{}: {:.4} ({})", s.group, s.cost, s.class.name()))
        .collect::<Vec<_>>()
        .join("\n");
    response.on_hover_text(tip);
}

/// The per-group costs of every candidate as numbers (the bar's segments).
fn draw_cost_parts(ui: &mut egui::Ui, snapshot: &MmSnapshot, tokens: &crate::theme::ThemeTokens) {
    let Some(first) = snapshot.rows.first().or(snapshot.continuation.as_ref()) else {
        ui.colored_label(tokens.muted, "no search yet");
        return;
    };
    egui::Grid::new("mm_panel_cost_parts")
        .striped(true)
        .show(ui, |ui| {
            ui.strong("frame");
            for segment in &first.groups {
                ui.strong(segment.group);
            }
            ui.end_row();
            for row in snapshot.rows.iter().chain(snapshot.continuation.iter()) {
                ui.label(row.frame.to_string());
                for segment in &row.groups {
                    ui.colored_label(
                        segment_color(segment.class, tokens),
                        format!("{:.4}", segment.cost),
                    );
                }
                ui.end_row();
            }
        });
}

/// The contact and phase strip: a row per foot, a cell per frame (filled while planted), the
/// phase as a tick height, the current frame outlined.
fn draw_strip(ui: &mut egui::Ui, strip: &ContactStrip, tokens: &crate::theme::ThemeTokens) {
    ui.label(format!("{} · frame {}", strip.clip_name, strip.current));
    let frames = strip.contacts.len().max(1);
    let cell = (320.0 / frames as f32).clamp(2.0, 8.0);
    for foot in 0..strip.feet {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(cell * frames as f32, 14.0), egui::Sense::hover());
        let painter = ui.painter_at(rect);
        for (i, contacts) in strip.contacts.iter().enumerate() {
            let x = rect.left() + i as f32 * cell;
            let r = egui::Rect::from_min_size(egui::pos2(x, rect.top()), egui::vec2(cell, rect.height()));
            if contacts.get(foot).copied().unwrap_or(false) {
                painter.rect_filled(r, 0.0, tokens.accents.ok);
            }
            if let Some(Some(phase)) = strip.phases.get(i).and_then(|p| p.get(foot)) {
                let y = rect.bottom() - phase * rect.height();
                painter.line_segment(
                    [egui::pos2(x, y), egui::pos2(x + cell, y)],
                    egui::Stroke::new(1.0, tokens.line),
                );
            }
            if i == strip.current {
                painter.rect_stroke(
                    r,
                    0.0,
                    egui::Stroke::new(1.5, tokens.accents.alert),
                    egui::StrokeKind::Inside,
                );
            }
        }
    }
}

/// The panel's UI.
pub fn mm_panel(ui: &mut egui::Ui, world: &mut World) {
    let tokens = crate::theme::ui_tokens(ui.ctx());
    let mut panel = world.remove_resource::<MotionMatchPanel>().unwrap_or_default();
    let list = characters(world);
    ui.horizontal(|ui| {
        ui.label("Character");
        let current = panel.target(world);
        let label = current.map_or_else(|| "none".to_string(), |e| format!("{}", e.to_bits()));
        egui::ComboBox::from_id_salt("mm_panel_character")
            .selected_text(label)
            .show_ui(ui, |ui| {
                for (entity, asset, frames) in &list {
                    let text = format!(
                        "{} · {} · {frames} frames",
                        entity.to_bits(),
                        asset.as_deref().unwrap_or("(built in code)")
                    );
                    if ui.selectable_label(current == Some(*entity), text).clicked() {
                        let _ = panel.select(world, *entity);
                    }
                }
            });
        let mut k = panel.candidates;
        ui.add(
            egui::DragValue::new(&mut k)
                .range(1..=MAX_CANDIDATES)
                .prefix("top "),
        );
        panel.candidates = k;
        let frozen = panel.frozen.is_some();
        if ui
            .selectable_label(frozen, if frozen { "Frozen" } else { "Freeze" })
            .clicked()
        {
            let _ = panel.freeze(world, !frozen);
        }
    });
    match panel.snapshot(world) {
        Ok(snapshot) => {
            ui.label(format!(
                "playing {} · searches {} · jumps {} · method {}",
                snapshot.playing.as_ref().map_or_else(
                    || "nothing".to_string(),
                    |(c, t, f)| format!("{c} @ {t:.2}s (frame {f})")
                ),
                snapshot.searches,
                snapshot.jumps,
                snapshot.method_used.unwrap_or("-"),
            ));
            let scale = snapshot
                .rows
                .iter()
                .chain(snapshot.continuation.iter())
                .map(|r| r.groups.iter().map(|g| g.cost).sum::<f32>())
                .fold(0.0f32, f32::max);
            egui::Grid::new("mm_panel_candidates")
                .striped(true)
                .show(ui, |ui| {
                    for head in ["status", "clip", "time", "frame", "total", "groups"] {
                        ui.strong(head);
                    }
                    ui.end_row();
                    for row in snapshot.rows.iter().chain(snapshot.continuation.iter()) {
                        ui.label(row.status);
                        ui.label(&row.clip_name);
                        ui.label(format!("{:.2}", row.time));
                        ui.label(row.frame.to_string());
                        ui.label(format!("{:.4}", row.total));
                        draw_bar(ui, row, scale, &tokens);
                        ui.end_row();
                    }
                });
            egui::CollapsingHeader::new("Cost parts")
                .default_open(false)
                .show(ui, |ui| draw_cost_parts(ui, &snapshot, &tokens));
        }
        Err(e) => {
            ui.colored_label(tokens.muted, e.message.clone());
        }
    }
    if let Some(entity) = panel.target(world)
        && let Some(strip) = strip_of(world, entity)
    {
        egui::CollapsingHeader::new("Contacts and phase")
            .default_open(true)
            .show(ui, |ui| draw_strip(ui, &strip, &tokens));
    }
    if let Some(entity) = panel.target(world)
        && let Some(view) = database_view_sorted(world, entity, panel.sort, panel.descending)
    {
        egui::CollapsingHeader::new("Database")
            .default_open(false)
            .show(ui, |ui| {
                ui.label(format!(
                    "{} · {} frames · {} dims",
                    view["asset"].as_str().unwrap_or("(built in code)"),
                    view["frames"],
                    view["dimensions"]
                ));
                // Opens this database in the motion-database editor (ADR-7740; `editor.mm_db_open`).
                if let Some(asset) = view["asset"].as_str()
                    && ui.button("Edit database").clicked()
                {
                    let file = crate::mm_db_editor::panel::edit_target(world, asset);
                    let opened = crate::mm_db_editor::open(world, &file, false);
                    let mut editor =
                        world.get_resource_or_insert_with(crate::mm_db_editor::MmDbEditor::default);
                    editor.ui.window_open = true;
                    if let Err(e) = opened {
                        editor.toast = format!("{} ({})", e.message, e.code);
                    }
                }
                ui.horizontal(|ui| {
                    ui.label("Sort");
                    for sort in ClipSort::ALL {
                        if ui.selectable_label(panel.sort == sort, sort.name()).clicked() {
                            if panel.sort == sort {
                                panel.descending = !panel.descending;
                            } else {
                                panel.sort = sort;
                                panel.descending = false;
                            }
                        }
                    }
                });
                let tags: Vec<String> = view["tag_frames"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|t| format!("{} ({})", t["name"].as_str().unwrap_or(""), t["frames"]))
                    .collect();
                if !tags.is_empty() {
                    ui.label(format!("tags: {}", tags.join(", ")));
                }
                for clip in view["clips"].as_array().into_iter().flatten() {
                    ui.label(format!(
                        "{} · {} frames {} · {}",
                        clip["name"].as_str().unwrap_or(""),
                        clip["frames"],
                        if clip["looping"] == true {
                            "looping"
                        } else {
                            "one-shot"
                        },
                        clip["tags"]
                    ));
                }
            });
    }
    world.insert_resource(panel);
}

// --------------------------------------------------------------------------------------- agent

fn with_panel<T>(
    world: &mut World,
    f: impl FnOnce(&mut MotionMatchPanel, &World) -> Result<T, PanelError>,
) -> Result<T, RpcError> {
    let mut panel = world.remove_resource::<MotionMatchPanel>().unwrap_or_default();
    let result = f(&mut panel, world);
    match &result {
        Ok(_) => panel.error = None,
        Err(e) => panel.error = Some(e.clone()),
    }
    world.insert_resource(panel);
    result.map_err(|e| e.rpc(HINT))
}

fn state(world: &World) -> Value {
    match world.get_resource::<MotionMatchPanel>() {
        Some(panel) => panel.json(world),
        None => MotionMatchPanel::default().json(world),
    }
}

fn panel_get(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    Ok(state(world))
}

fn panel_select(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = match params.get("entity") {
        Some(v) => Some(
            v.as_u64()
                .and_then(Entity::try_from_bits)
                .ok_or_else(|| err("bad_entity", "entity must be an entity id (integer)").rpc(HINT))?,
        ),
        None => None,
    };
    let candidates = match params.get("candidates") {
        Some(v) => Some(
            v.as_u64()
                .filter(|n| (1..=MAX_CANDIDATES as u64).contains(n))
                .ok_or_else(|| err("bad_candidates", "candidates must be 1..32").rpc(HINT))?
                as usize,
        ),
        None => None,
    };
    let sort = match params.get("sort") {
        Some(v) => Some(v.as_str().and_then(ClipSort::from_name).ok_or_else(|| {
            err("bad_sort", "sort must be index, name, frames, duration or tags").rpc(HINT)
        })?),
        None => None,
    };
    let descending = match params.get("descending") {
        Some(v) => Some(
            v.as_bool()
                .ok_or_else(|| err("bad_sort", "descending must be true or false").rpc(HINT))?,
        ),
        None => None,
    };
    with_panel(world, |panel, world| {
        if let Some(entity) = entity {
            panel.select(world, entity)?;
        }
        if let Some(sort) = sort {
            panel.sort = sort;
        }
        if let Some(descending) = descending {
            panel.descending = descending;
        }
        if let Some(k) = candidates {
            panel.candidates = k;
            panel.frozen = None;
        }
        Ok(())
    })?;
    Ok(state(world))
}

fn panel_freeze(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let frozen = params
        .get("frozen")
        .and_then(Value::as_bool)
        .ok_or_else(|| err("bad_frozen", "frozen must be true or false").rpc(HINT))?;
    with_panel(world, |panel, world| panel.freeze(world, frozen))?;
    Ok(state(world))
}

const STATE: &str = "{ characters: [{ entity, asset, frames }], selected, candidates, frozen, report: { entity, asset, playing: { clip_name, time, frame }, searches, jumps, method_used, rows: [{ frame, clip, clip_name, time, mirrored, total, transition, bias, extra_bias, status, groups: [{ group, cost, class }] }], continuation }, database: { asset, frames, dimensions, groups, clips: [{ index, name, duration, looping, mirrored, distance_matched, frames, tags }], sort, descending, tag_frames: [{ name, frames }], contacts, kd, content_hash, warnings }, strip: null | { clip_name, feet, current, contacts: [[bool]], phases: [[number | null]] }, error }";

const SELECT_PARAMS: &[ParamSchema] = &[
    ParamSchema {
        name: "entity",
        kind: "integer",
        required: false,
        description: "The character (an entity with MotionMatching).",
    },
    ParamSchema {
        name: "candidates",
        kind: "integer",
        required: false,
        description: "Candidate rows, 1..32 (default 8).",
    },
    ParamSchema {
        name: "sort",
        kind: "string",
        required: false,
        description: "Database tab order of the clip and tag lists: index, name, frames, duration or tags (mm_panel_bad_sort).",
    },
    ParamSchema {
        name: "descending",
        kind: "boolean",
        required: false,
        description: "Reverse the sort order.",
    },
];

const FREEZE_PARAMS: &[ParamSchema] = &[ParamSchema {
    name: "frozen",
    kind: "boolean",
    required: true,
    description: "true keeps the current report while the game plays on; false shows the live one again.",
}];

/// The Motion Matching panel's agent surface.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.mm_panel_get",
                tool: "mm_panel_get",
                description: "The Motion Matching panel: characters with motion matching, the selected one's playback, \
                    counters and candidate table (each row's cost split per feature group, classed below, above or \
                    ignored against the best candidate) and its database (asset path, clips and tags in the chosen order, \
                    frames per tag, tree, warnings), plus the contact and phase strip of the playing clip when the schema \
                    has contact labels.",
                params: &[],
                result: STATE,
                mutating: false,
                permission: Permission::Read,
            },
            panel_get as Handler,
        ),
        (
            MethodSchema {
                name: "editor.mm_panel_select",
                tool: "mm_panel_select",
                description: "Pick the character the Motion Matching panel shows, how many candidate rows, and the database tab's clip and tag order.",
                params: SELECT_PARAMS,
                result: STATE,
                mutating: true,
                permission: Permission::EditScene,
            },
            panel_select as Handler,
        ),
        (
            MethodSchema {
                name: "editor.mm_panel_freeze",
                tool: "mm_panel_freeze",
                description: "Freeze the Motion Matching panel's candidate report (or unfreeze it).",
                params: FREEZE_PARAMS,
                result: STATE,
                mutating: true,
                permission: Permission::EditScene,
            },
            panel_freeze as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests;
