// SPDX-License-Identifier: MPL-2.0
//! The Output Log panel: the engine's global log ring with level/target/text filters and
//! search (`docs/specs/19-editor.md` § 2).
//!
//! The ring (`regia_core::log::ring`) is always on and shared with the agent protocol's
//! `logs.read`, so the panel and agents see the same records. The panel polls the ring for new
//! records every frame and keeps a longer history of its own ([`HISTORY_CAP`] lines); the
//! filters are applied incrementally (a new line is tested once; a changed filter re-tests the
//! history once), and only the rows on screen are laid out (`egui::ScrollArea::show_rows`,
//! ADR-7306). A clicked row expands: its full message, wrapped, shows under the list.
//!
//! Identical consecutive lines (same level, target and message) collapse into one row with a count
//! (`×12`), and *Group by source* lists the rows under one collapsible header per target (the
//! module that logged them), headers sorted by name (ADR-7789). Both views are kept incrementally:
//! a new line joins the last row or adds one to the flat list and to its group, so a frame costs the
//! rows on screen plus one step per group. *Copy all* copies the shown rows, *Copy selected* the
//! expanded one. An entity GUID in a line is a link that selects and frames the entity
//! ([`crate::log_links`]).

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::Arc;

use regia_core::log::ring::{self, LogQuery, LogRecord};

use crate::log_links::{LogLink, parse_links};

/// Parsed lines kept before the cache starts over (only drawn rows are parsed).
const LINK_CACHE_CAP: usize = 4096;

/// Lines the panel keeps (the ring holds only the newest `regia_core::log::ring::CAPACITY`).
pub const HISTORY_CAP: usize = 100_000;

/// The filters a shown list was built for: (minimum level, target prefix, text, duplicates
/// collapsed).
type FilterKey = (String, String, String, bool);

/// One row of the list: a run of `count` identical consecutive lines, the first and the last at
/// these absolute indices into the kept lines (the row shows the last one).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Row {
    first: u64,
    last: u64,
    count: u32,
}

/// Whether two lines collapse into one row.
fn same_line(a: &LogRecord, b: &LogRecord) -> bool {
    a.level == b.level && a.target == b.target && a.message == b.message
}

/// What a row of the drawn list is: a group header or a log row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListItem<'a> {
    /// A source's header: its name, rows and whether it is open.
    Header(&'a str, usize, bool),
    /// A log row.
    Line(Row),
}

/// Panel state: filters, held outside the world (pure UI).
#[derive(Debug, Clone)]
pub struct LogPanelState {
    /// Minimum level shown (as text: `ERROR` … `TRACE`).
    pub min_level: String,
    /// Target prefix filter.
    pub target: String,
    /// Free-text search.
    pub contains: String,
    /// What the last click on a link did.
    pub status: String,
    /// List the rows under one collapsible header per source (log target).
    pub group_by_source: bool,
    /// Collapse identical consecutive lines into one row with a count.
    pub collapse_duplicates: bool,
    /// Sources whose group is collapsed.
    closed_groups: BTreeSet<String>,
    /// The shown rows of each source, oldest first (the grouped view).
    groups: BTreeMap<String, VecDeque<Row>>,
    /// Records shown so far; new records auto-scroll.
    follow: bool,
    /// The links of the lines drawn so far, by record sequence number (a line never changes).
    links: HashMap<u64, Arc<[LogLink]>>,
    /// The kept lines, oldest first.
    history: VecDeque<LogRecord>,
    /// How many lines were dropped from the front of `history` (the absolute index of its first).
    dropped: u64,
    /// The sequence number of the next record to poll from the ring (`None` before the first poll).
    next_seq: Option<u64>,
    /// The rows of the lines that pass the filters, in order.
    shown: VecDeque<Row>,
    /// The filters `shown` was built for.
    shown_for: Option<FilterKey>,
    /// The expanded row's record sequence number.
    expanded: Option<u64>,
    /// Lines tested against the filters so far (a cost counter for tests and soaks).
    tested: u64,
    /// The egui frame whose control changes were applied ([`LogPanelState::apply_toggles`]).
    toggled_frame: Option<u64>,
}

/// One change an Output Log control asks for. The panel applies them once per egui frame: a second
/// UI pass of the frame sees the same click and would flip the setting back (review
/// sprint-w12-rev-g5 #6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogToggle {
    /// Collapse duplicates on or off.
    CollapseDuplicates(bool),
    /// Group by source on or off.
    GroupBySource(bool),
    /// Open or collapse the group of this source.
    Group(String),
    /// Expand the line of this record sequence number, or collapse it when it is the expanded one.
    Expand(u64),
}

impl Default for LogPanelState {
    fn default() -> Self {
        Self {
            min_level: "INFO".to_string(),
            target: String::new(),
            contains: String::new(),
            status: String::new(),
            group_by_source: false,
            collapse_duplicates: true,
            closed_groups: BTreeSet::new(),
            groups: BTreeMap::new(),
            follow: true,
            links: HashMap::new(),
            history: VecDeque::new(),
            dropped: 0,
            next_seq: None,
            shown: VecDeque::new(),
            shown_for: None,
            expanded: None,
            tested: 0,
            toggled_frame: None,
        }
    }
}

impl LogPanelState {
    /// Applies the control changes of egui frame `frame`, once: a second call for the same frame
    /// (egui's second UI pass) changes nothing. Returns whether it applied any.
    pub fn apply_toggles(&mut self, toggles: Vec<LogToggle>, frame: u64) -> bool {
        if toggles.is_empty() || self.toggled_frame == Some(frame) {
            return false;
        }
        self.toggled_frame = Some(frame);
        for toggle in toggles {
            match toggle {
                LogToggle::CollapseDuplicates(on) => self.collapse_duplicates = on,
                LogToggle::GroupBySource(on) => self.group_by_source = on,
                LogToggle::Group(source) => self.toggle_group(&source),
                LogToggle::Expand(seq) => {
                    self.expanded = if self.expanded == Some(seq) {
                        None
                    } else {
                        Some(seq)
                    };
                }
            }
        }
        true
    }
}

/// The compiled filters: what a line must pass.
struct Filter {
    /// Most verbose level shown (`None` = every level).
    max_level: Option<tracing::Level>,
    /// Target prefix (and its current spelling for a legacy engine crate name, ADR-1429).
    target: Option<(String, Option<String>)>,
    /// Text the message must contain.
    contains: Option<String>,
}

impl Filter {
    fn new(state: &LogPanelState) -> Self {
        let query = state.query().unwrap_or_default();
        Self {
            max_level: query.min_level,
            target: query.target.map(|t| {
                let current = regia_core::brand::current_log_target(&t);
                (t, current)
            }),
            contains: query.contains,
        }
    }

    fn passes(&self, record: &LogRecord) -> bool {
        if let Some(max) = self.max_level {
            // `tracing`'s order: more verbose levels compare greater.
            match record.level.parse::<tracing::Level>() {
                Ok(level) if level > max => return false,
                Err(_) => return false,
                Ok(_) => {}
            }
        }
        if let Some((prefix, current)) = &self.target
            && !record.target.starts_with(prefix.as_str())
            && !current.as_deref().is_some_and(|c| record.target.starts_with(c))
        {
            return false;
        }
        self.contains
            .as_deref()
            .is_none_or(|text| record.message.contains(text))
    }
}

impl LogPanelState {
    /// The links of a record's message, parsed once.
    fn links_of(&mut self, seq: u64, message: &str) -> Arc<[LogLink]> {
        if self.links.len() >= LINK_CACHE_CAP {
            self.links.clear();
        }
        self.links
            .entry(seq)
            .or_insert_with(|| parse_links(message).into())
            .clone()
    }

    /// Builds a ring query from the panel's filters.
    ///
    /// # Errors
    /// `Err` (unit) when the level text is not a level name; the UI then falls back to the
    /// unfiltered query.
    #[allow(clippy::result_unit_err)] // a UI filter fallback, not a recoverable failure
    pub fn query(&self) -> Result<LogQuery, ()> {
        let min_level = if self.min_level.is_empty() {
            None
        } else {
            Some(self.min_level.parse().map_err(|_| ())?)
        };
        Ok(LogQuery {
            since: None,
            min_level,
            target: if self.target.is_empty() {
                None
            } else {
                Some(self.target.clone())
            },
            contains: if self.contains.is_empty() {
                None
            } else {
                Some(self.contains.clone())
            },
            limit: Some(500),
        })
    }

    /// Adds `records` (oldest first) to the kept lines, dropping the oldest past [`HISTORY_CAP`],
    /// and tests each new line once against the current filters.
    pub fn ingest(&mut self, records: impl IntoIterator<Item = LogRecord>) {
        let filter = self.shown_for.is_some().then(|| Filter::new(self));
        for record in records {
            if self.next_seq.is_some_and(|next| record.seq < next) {
                continue;
            }
            self.next_seq = Some(record.seq + 1);
            let passes = filter.as_ref().map(|filter| {
                self.tested += 1;
                filter.passes(&record)
            });
            self.history.push_back(record);
            if passes == Some(true) {
                self.add_shown(self.dropped + self.history.len() as u64 - 1);
            }
        }
        while self.history.len() > HISTORY_CAP {
            let Some(old) = self.history.pop_front() else {
                break;
            };
            self.dropped += 1;
            let dropped = self.dropped;
            if let Some(rows) = self.groups.get_mut(&old.target) {
                while rows.front().is_some_and(|row| row.last < dropped) {
                    rows.pop_front();
                }
                if rows.is_empty() {
                    self.groups.remove(&old.target);
                }
            }
        }
        while self.shown.front().is_some_and(|row| row.last < self.dropped) {
            self.shown.pop_front();
        }
    }

    /// The kept line at absolute index `at`.
    fn line(&self, at: u64) -> Option<&LogRecord> {
        self.history
            .get(usize::try_from(at.checked_sub(self.dropped)?).ok()?)
    }

    /// Adds the kept line at absolute index `at` (which passed the filters) to the shown rows: it
    /// joins the last row when it repeats that row's line, else starts a row in the list and in its
    /// source's group.
    fn add_shown(&mut self, at: u64) {
        let Some(record) = self.line(at) else {
            return;
        };
        let repeats = self.collapse_duplicates
            && self
                .shown
                .back()
                .and_then(|row| self.line(row.last))
                .is_some_and(|last| same_line(last, record));
        let target = record.target.clone();
        if repeats {
            if let Some(row) = self.shown.back_mut() {
                row.last = at;
                row.count = row.count.saturating_add(1);
            }
            // The flat list's last row is the newest row of its source, so the group's last row.
            if let Some(row) = self.groups.get_mut(&target).and_then(VecDeque::back_mut) {
                row.last = at;
                row.count = row.count.saturating_add(1);
            }
            return;
        }
        let row = Row {
            first: at,
            last: at,
            count: 1,
        };
        self.shown.push_back(row);
        self.groups.entry(target).or_default().push_back(row);
    }

    /// Polls the ring for records newer than the last poll.
    pub fn poll(&mut self) {
        let (records, next) = ring::recent(&LogQuery {
            since: self.next_seq,
            ..Default::default()
        });
        self.ingest(records);
        // Records dropped by the ring before this poll are gone: skip their numbers.
        self.next_seq = Some(self.next_seq.map_or(next, |seq| seq.max(next)));
    }

    /// Re-tests the kept lines when a filter changed since the shown list was built.
    fn refilter(&mut self) {
        let key = (
            self.min_level.clone(),
            self.target.clone(),
            self.contains.clone(),
            self.collapse_duplicates,
        );
        if self.shown_for.as_ref() == Some(&key) {
            return;
        }
        let filter = Filter::new(self);
        self.shown.clear();
        self.groups.clear();
        for i in 0..self.history.len() {
            if filter.passes(&self.history[i]) {
                self.add_shown(self.dropped + i as u64);
            }
        }
        self.tested += self.history.len() as u64;
        self.shown_for = Some(key);
    }

    /// Lines kept.
    #[must_use]
    pub fn kept(&self) -> usize {
        self.history.len()
    }

    /// Rows of the lines that pass the filters (identical consecutive lines are one row while
    /// [`Self::collapse_duplicates`] is on).
    #[must_use]
    pub fn shown(&self) -> usize {
        self.shown.len()
    }

    /// The sources (log targets) with shown rows, sorted, with their row counts.
    #[must_use]
    pub fn groups(&self) -> Vec<(String, usize)> {
        self.groups
            .iter()
            .map(|(name, rows)| (name.clone(), rows.len()))
            .collect()
    }

    /// Whether the group of `source` is open (groups start open).
    #[must_use]
    pub fn group_open(&self, source: &str) -> bool {
        !self.closed_groups.contains(source)
    }

    /// Opens or collapses the group of `source`.
    pub fn toggle_group(&mut self, source: &str) {
        if !self.closed_groups.remove(source) {
            self.closed_groups.insert(source.to_owned());
        }
    }

    /// The list as drawn: rows in time order, or with [`Self::group_by_source`] one header per
    /// source followed by its rows when it is open.
    fn items(&self) -> Vec<ListItem<'_>> {
        if !self.group_by_source {
            return self.shown.iter().copied().map(ListItem::Line).collect();
        }
        let mut items = Vec::new();
        for (name, rows) in &self.groups {
            let open = self.group_open(name);
            items.push(ListItem::Header(name, rows.len(), open));
            if open {
                items.extend(rows.iter().copied().map(ListItem::Line));
            }
        }
        items
    }

    /// The number of drawn list rows (headers included) without building the list.
    fn item_count(&self) -> usize {
        if !self.group_by_source {
            return self.shown.len();
        }
        self.groups
            .iter()
            .map(|(name, rows)| 1 + if self.group_open(name) { rows.len() } else { 0 })
            .sum()
    }

    /// The `index`-th drawn list row: one step per group before it, never the whole list.
    fn item(&self, index: usize) -> Option<ListItem<'_>> {
        if !self.group_by_source {
            return self.shown.get(index).copied().map(ListItem::Line);
        }
        let mut at = index;
        for (name, rows) in &self.groups {
            let open = self.group_open(name);
            if at == 0 {
                return Some(ListItem::Header(name, rows.len(), open));
            }
            at -= 1;
            if open {
                if at < rows.len() {
                    return rows.get(at).copied().map(ListItem::Line);
                }
                at -= rows.len();
            }
        }
        None
    }

    /// One row as text: `LEVEL target message`, with ` (×N)` for a collapsed run.
    fn row_text(&self, row: Row) -> Option<String> {
        let record = self.line(row.last)?;
        let mut text = format!("{} {} {}", record.level, record.target, record.message);
        if row.count > 1 {
            text.push_str(&format!(" (×{})", row.count));
        }
        Some(text)
    }

    /// What *Copy all* copies: every drawn row in list order (a header as `[source] (N)`), one per
    /// line.
    #[must_use]
    pub fn copy_all_text(&self) -> String {
        let lines: Vec<String> = self
            .items()
            .into_iter()
            .filter_map(|item| match item {
                ListItem::Header(name, rows, _) => Some(format!("[{name}] ({rows})")),
                ListItem::Line(row) => self.row_text(row),
            })
            .collect();
        lines.join("\n")
    }

    /// What *Copy selected* copies: the expanded row's whole line, when a row is expanded.
    #[must_use]
    pub fn selected_text(&self) -> Option<String> {
        let seq = self.expanded?;
        let record = self.history.iter().rev().find(|r| r.seq == seq)?;
        Some(format!("{} {} {}", record.level, record.target, record.message))
    }

    /// The first and last sequence numbers of a row (the first line may have left the history).
    fn row_seqs(&self, row: Row) -> (u64, u64) {
        let last = self.line(row.last).map_or(0, |r| r.seq);
        let first = self.line(row.first).map_or(0, |r| r.seq);
        (first, last)
    }

    /// Lines tested against the filters so far (each new line once, the history once per filter
    /// change).
    #[must_use]
    pub fn tested(&self) -> u64 {
        self.tested
    }

    /// The `row`-th shown row's line (its last).
    #[cfg(test)]
    fn shown_record(&self, row: usize) -> Option<&LogRecord> {
        self.line(self.shown.get(row)?.last)
    }

    /// The `row`-th shown row's line count.
    #[cfg(test)]
    fn shown_count(&self, row: usize) -> Option<u32> {
        self.shown.get(row).map(|row| row.count)
    }
}

/// Draws `message` with its `links` as link widgets on one line (`wrap` = false) or wrapped;
/// returns the link clicked and whether the plain text was clicked.
fn message_with_links(
    ui: &mut egui::Ui,
    message: &str,
    links: &[LogLink],
    wrap: bool,
) -> (Option<LogLink>, bool) {
    let mut clicked = None;
    let mut text_clicked = false;
    let mut at = 0;
    let mut text = |ui: &mut egui::Ui, part: &str| {
        let label = egui::Label::new(part).sense(egui::Sense::click());
        let label = if wrap { label.wrap() } else { label.truncate() };
        text_clicked |= ui.add(label).clicked();
    };
    let mut draw = |ui: &mut egui::Ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        for link in links {
            if link.start > at {
                text(ui, &message[at..link.start]);
            }
            let hover = match link.target {
                crate::log_links::LinkTarget::Entity => format!("Select and frame entity {}", link.path()),
                _ => format!("Open {}", link.path()),
            };
            let response = ui.link(&message[link.start..link.end]).on_hover_text(hover);
            if response.clicked() {
                clicked = Some(link.clone());
            }
            at = link.end;
        }
        if at < message.len() {
            text(ui, &message[at..]);
        }
    };
    if wrap {
        ui.horizontal_wrapped(|ui| draw(ui));
    } else {
        draw(ui);
    }
    (clicked, text_clicked)
}

/// The first line of `message` (a row shows one line; the expanded row shows all).
fn first_line(message: &str) -> &str {
    message.split('\n').next().unwrap_or(message)
}

/// Draws the Output Log panel. Returns the link clicked this frame, if any: `path:line[:col]`, asset
/// paths and entity GUIDs in a line are links ([`crate::log_links`]), and the caller opens them with
/// [`crate::log_links::open_clicked`].
#[tracing::instrument(target = "regia_editor", skip_all, name = "log_panel")]
pub fn output_log_panel(ui: &mut egui::Ui, state: &mut LogPanelState) -> Option<LogLink> {
    let mut copy = None;
    // Toggle changes are applied once per frame, after the list (LogToggle).
    let mut toggles: Vec<LogToggle> = Vec::new();
    ui.horizontal_wrapped(|ui| {
        ui.strong("Output Log");
        egui::ComboBox::from_id_salt("log-level")
            .selected_text(state.min_level.clone())
            .show_ui(ui, |ui| {
                for level in ["ERROR", "WARN", "INFO", "DEBUG", "TRACE"] {
                    ui.selectable_value(&mut state.min_level, level.to_string(), level);
                }
            });
        // Hinted and narrow, so the row wraps less in a short dock (bug 6, ADR-8586).
        ui.add(
            egui::TextEdit::singleline(&mut state.target)
                .hint_text("Source")
                .desired_width(110.0),
        )
        .on_hover_text("Show only lines whose source (log target) starts with this");
        ui.add(
            egui::TextEdit::singleline(&mut state.contains)
                .hint_text("Search")
                .desired_width(140.0),
        )
        .on_hover_text("Show only lines that contain this text");
        let mut collapse = state.collapse_duplicates;
        if ui
            .checkbox(&mut collapse, "Collapse duplicates")
            .on_hover_text("Identical consecutive lines show once, with a count")
            .changed()
        {
            toggles.push(LogToggle::CollapseDuplicates(collapse));
        }
        let mut grouped = state.group_by_source;
        if ui
            .checkbox(&mut grouped, "Group by source")
            .on_hover_text("One collapsible group per log target")
            .changed()
        {
            toggles.push(LogToggle::GroupBySource(grouped));
        }
        if ui.button("Copy all").clicked() {
            copy = Some(state.copy_all_text());
        }
        if ui
            .add_enabled(state.expanded.is_some(), egui::Button::new("Copy selected"))
            .clicked()
        {
            copy = state.selected_text();
        }
        if !state.status.is_empty() {
            ui.weak(&state.status);
        }
    });
    if let Some(text) = copy {
        ui.ctx().copy_text(text);
    }
    ui.separator();

    state.poll();
    state.refilter();

    let mut clicked = None;
    let mut toggled: Option<u64> = None;
    let mut toggled_group: Option<String> = None;
    let accents = crate::theme::ui_tokens(ui.ctx()).accents;
    let weak = ui.visuals().weak_text_color();
    let color_of = |level: &str| match level {
        "ERROR" => accents.log_error,
        "WARN" => accents.log_warn,
        "INFO" => accents.log_info,
        _ => weak,
    };
    let row_height = ui.spacing().interact_size.y;
    let expanded = state
        .expanded
        .and_then(|seq| state.history.iter().rev().find(|r| r.seq == seq).cloned());
    let detail_height = if expanded.is_some() {
        (ui.available_height() * 0.3).max(row_height * 3.0)
    } else {
        0.0
    };
    let total = state.item_count();
    egui::ScrollArea::vertical()
        .id_salt("log-rows")
        .auto_shrink([false, false])
        .max_height((ui.available_height() - detail_height).max(row_height))
        .stick_to_bottom(state.follow)
        .show_rows(ui, row_height, total, |ui, range| {
            for index in range {
                let Some(item) = state.item(index) else {
                    continue;
                };
                let row = match item {
                    ListItem::Header(name, rows, open) => {
                        let mark = if open { "▼" } else { "▶" };
                        let response = ui
                            .selectable_label(open, format!("{mark} {name} ({rows})"))
                            .on_hover_text(if open {
                                "Collapse the group"
                            } else {
                                "Open the group"
                            });
                        if response.clicked() {
                            toggled_group = Some(name.to_owned());
                        }
                        continue;
                    }
                    ListItem::Line(row) => row,
                };
                let (first_seq, last_seq) = state.row_seqs(row);
                let Some(record) = state.line(row.last) else {
                    continue;
                };
                let (seq, level, target, message) = (
                    record.seq,
                    record.level.clone(),
                    record.target.clone(),
                    first_line(&record.message).to_string(),
                );
                let multiline = message.len() < record.message.len();
                let (rect, _) = ui
                    .allocate_exact_size(egui::vec2(ui.available_width(), row_height), egui::Sense::hover());
                let mut row_ui = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(rect)
                        .id_salt(("log-row", row.first))
                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                );
                row_ui.set_clip_rect(rect.intersect(ui.clip_rect()));
                if state
                    .expanded
                    .is_some_and(|e| (first_seq..=last_seq).contains(&e))
                {
                    row_ui
                        .painter()
                        .rect_filled(rect, 2.0, row_ui.visuals().selection.bg_fill);
                }
                if state.group_by_source {
                    row_ui.add_space(row_height);
                }
                row_ui.label(egui::RichText::new(&level).color(color_of(&level)).strong());
                if row.count > 1 {
                    row_ui
                        .label(egui::RichText::new(format!("×{}", row.count)).strong())
                        .on_hover_text(format!("{} identical lines in a row", row.count));
                }
                if !state.group_by_source {
                    row_ui.weak(&target);
                }
                let links = state.links_of(seq, &message);
                let (link, text) = message_with_links(&mut row_ui, &message, &links, false);
                if multiline {
                    row_ui.weak("…");
                }
                if link.is_some() {
                    clicked = link;
                } else if text {
                    toggled = Some(seq);
                }
            }
        });
    if total == 0 {
        ui.weak("(no records match the filters)");
    }
    if let Some(name) = toggled_group {
        toggles.push(LogToggle::Group(name));
    }
    if let Some(seq) = toggled {
        toggles.push(LogToggle::Expand(seq));
    }
    if state.apply_toggles(toggles, ui.ctx().cumulative_frame_nr()) {
        // The list is laid out from the new settings on the next frame.
        ui.ctx().request_repaint();
    }
    if let Some(record) = expanded {
        ui.separator();
        egui::ScrollArea::vertical()
            .id_salt("log-expanded")
            .max_height(detail_height)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(&record.level)
                            .color(color_of(&record.level))
                            .strong(),
                    );
                    ui.weak(&record.target);
                    if ui.small_button("Close").clicked() {
                        state.expanded = None;
                    }
                });
                let links: Arc<[LogLink]> = parse_links(&record.message).into();
                if let (Some(link), _) = message_with_links(ui, &record.message, &links, true) {
                    clicked = Some(link);
                }
            });
    }
    clicked
}

/// The Output Log with the console line at its bottom (ADR-7754): the docked tab and the log
/// drawer. `salt` tells the two lines apart when both show. Returns the link clicked, as
/// [`output_log_panel`] does.
pub fn output_log_with_console(
    ui: &mut egui::Ui,
    state: &mut LogPanelState,
    world: &mut regia_ecs::prelude::World,
    salt: &str,
) -> Option<LogLink> {
    ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
        crate::console::console_line(ui, world, salt);
        ui.separator();
        // The log gets the rect above the console line and is clipped to it: in a short dock its
        // toolbar wraps and is cut off instead of drawing over the console row (bug 6, ADR-8586).
        let rect = ui.available_rect_before_wrap();
        let mut log = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(rect)
                .layout(egui::Layout::top_down(egui::Align::LEFT)),
        );
        log.set_clip_rect(rect.intersect(ui.clip_rect()));
        let clicked = output_log_panel(&mut log, state);
        ui.allocate_rect(rect, egui::Sense::hover());
        clicked
    })
    .inner
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// The console line sits under the log rows and takes typed text from the keyboard.
    #[test]
    fn the_console_line_draws_under_the_log() {
        let mut world = regia_ecs::prelude::World::new();
        world
            .get_resource_or_insert_with(crate::console::ConsoleState::default)
            .focus();
        let mut state = LogPanelState::default();
        let ctx = egui::Context::default();
        let mut run = |events: Vec<egui::Event>| {
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(900.0, 600.0),
                )),
                events,
                ..Default::default()
            };
            let mut output = ctx.run_ui(raw, |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    let _ = output_log_with_console(ui, &mut state, &mut world, "tab");
                });
            });
            output.textures_delta.clear();
            output
        };
        run(vec![]);
        run(vec![]);
        let output = run(vec![egui::Event::Text("edit.undo".into())]);
        let mut console_y = None;
        let mut title_y = None;
        for shape in &output.shapes {
            if let egui::epaint::Shape::Text(text) = &shape.shape {
                match text.galley.text() {
                    "Console" => console_y = Some(text.pos.y),
                    "Output Log" => title_y = Some(text.pos.y),
                    _ => {}
                }
            }
        }
        assert!(
            console_y.unwrap() > title_y.unwrap() + 400.0,
            "{console_y:?} {title_y:?}"
        );
        assert_eq!(
            world.resource::<crate::console::ConsoleState>().input,
            "edit.undo"
        );
    }

    /// The visible rect of each text of one frame of the log with its console line in a
    /// `size` panel: `(text, galley rect clipped to its clip rect)`.
    fn log_texts(size: egui::Vec2) -> Vec<(String, egui::Rect)> {
        let mut world = regia_ecs::prelude::World::new();
        let mut state = LogPanelState::default();
        let ctx = egui::Context::default();
        let mut texts = Vec::new();
        for _ in 0..3 {
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                ..Default::default()
            };
            let mut output = ctx.run_ui(raw, |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    let _ = output_log_with_console(ui, &mut state, &mut world, "tab");
                });
            });
            output.textures_delta.clear();
            texts.clear();
            for clipped in &output.shapes {
                if let egui::epaint::Shape::Text(text) = &clipped.shape {
                    let rect = text.galley.rect.translate(text.pos.to_vec2());
                    texts.push((text.galley.text().to_owned(), rect.intersect(clipped.clip_rect)));
                }
            }
        }
        texts
    }

    /// Bug 6 (ADR-8586): at the smoke run's dock size (the Mac's 421x131-point drawer, about 90
    /// points for the log) and a few around it, no text of the log's toolbar shows over the
    /// console row, and the filter fields carry hint text.
    #[test]
    fn the_log_toolbar_never_draws_over_the_console_row() {
        for size in [
            egui::vec2(421.0, 90.0),
            egui::vec2(421.0, 131.0),
            egui::vec2(300.0, 70.0),
            egui::vec2(900.0, 120.0),
        ] {
            let texts = log_texts(size);
            let console = texts
                .iter()
                .find(|(t, _)| t == "Console")
                .map(|(_, r)| *r)
                .unwrap_or_else(|| panic!("{size:?}: the console label draws: {texts:?}"));
            let row = egui::Rect::from_x_y_ranges(0.0..=size.x, console.y_range());
            for (text, rect) in &texts {
                if [
                    "Output Log",
                    "Collapse duplicates",
                    "Group by source",
                    "Copy all",
                    "Copy selected",
                    "Source",
                    "Search",
                ]
                .contains(&text.as_str())
                    && rect.is_positive()
                {
                    assert!(
                        !rect.intersects(row),
                        "{size:?}: `{text}` at {rect:?} draws over the console row {row:?}"
                    );
                }
            }
        }
        let tall = log_texts(egui::vec2(900.0, 400.0));
        for hint in ["Source", "Search"] {
            assert!(
                tall.iter().any(|(t, _)| t == hint),
                "the `{hint}` filter hint shows: {tall:?}"
            );
        }
    }

    #[test]
    fn query_builds_filters() {
        let state = LogPanelState {
            min_level: "WARN".into(),
            target: "regia_render".into(),
            contains: "GPU".into(),
            ..Default::default()
        };
        let query = state.query().expect("valid");
        assert_eq!(query.min_level, Some(tracing::Level::WARN));
        assert_eq!(query.target.as_deref(), Some("regia_render"));
        assert_eq!(query.contains.as_deref(), Some("GPU"));

        let empty = LogPanelState {
            min_level: String::new(),
            target: String::new(),
            contains: String::new(),
            ..Default::default()
        };
        let query = empty.query().expect("valid");
        assert!(query.min_level.is_none());
        assert!(query.target.is_none());

        let bad = LogPanelState {
            min_level: "LOUD".into(),
            ..Default::default()
        };
        assert!(bad.query().is_err());
    }

    fn line(seq: u64, level: &str, target: &str, message: &str) -> LogRecord {
        LogRecord {
            seq,
            unix_ms: 0,
            level: level.to_string(),
            target: target.to_string(),
            message: message.to_string(),
        }
    }

    /// Headless frames of the panel, returning the text the last one drew.
    fn draw(state: &mut LogPanelState, frames: usize) -> Vec<String> {
        let ctx = egui::Context::default();
        let mut texts = Vec::new();
        for n in 0..frames {
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(900.0, 600.0),
                )),
                time: Some(n as f64 / 60.0),
                ..Default::default()
            };
            let mut output = ctx.run_ui(raw, |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    let _ = output_log_panel(ui, state);
                });
            });
            output.textures_delta.clear();
            texts.clear();
            for clipped in &output.shapes {
                if let egui::Shape::Text(text) = &clipped.shape {
                    texts.push(text.galley.text().to_string());
                }
            }
        }
        texts
    }

    #[test]
    fn the_history_is_bounded_and_filters_test_each_line_once() {
        let mut state = LogPanelState {
            min_level: "WARN".into(),
            // A panel that never polls the ring: lines come from `ingest` only.
            next_seq: Some(0),
            ..Default::default()
        };
        state.refilter();
        let total = HISTORY_CAP as u64 + 1_000;
        state.ingest((0..total).map(|i| {
            let level = if i % 10 == 0 { "WARN" } else { "INFO" };
            line(i, level, "scale::unit", &format!("line {i}"))
        }));
        assert_eq!(state.kept(), HISTORY_CAP, "the oldest lines are dropped");
        assert_eq!(state.tested(), total, "each new line tested once");
        assert_eq!(state.shown(), HISTORY_CAP / 10, "one line in ten is a warning");
        assert_eq!(
            state.shown_record(0).map(|r| r.seq),
            Some(1_000),
            "the first kept warning"
        );

        // A filter change re-tests the kept lines once, not on every frame.
        state.min_level = "INFO".into();
        state.refilter();
        state.refilter();
        assert_eq!(state.tested(), total + HISTORY_CAP as u64);
        assert_eq!(state.shown(), HISTORY_CAP);
        state.contains = "line 1000".into();
        state.refilter();
        // "line 1000", "line 10000".."line 10009", "line 100000".."line 100099".
        assert_eq!(state.shown(), 1 + 10 + 100);
        // Duplicate or older sequence numbers are ignored.
        let before = state.kept();
        state.ingest([line(5, "ERROR", "scale::unit", "old")]);
        assert_eq!(state.kept(), before);
    }

    #[test]
    fn launch_lines_filter_by_their_target_and_launch_id() {
        // A launch tags each line `[launch <id>]` under `crate::launch::LOG_TARGET` (ADR-8980).
        let mut state = LogPanelState {
            min_level: "INFO".into(),
            target: crate::launch::LOG_TARGET.into(),
            contains: "[launch 2]".into(),
            next_seq: Some(0),
            ..Default::default()
        };
        state.refilter();
        state.ingest([
            line(
                0,
                "INFO",
                crate::launch::LOG_TARGET,
                "[launch 1] fake game: hello",
            ),
            line(
                1,
                "INFO",
                crate::launch::LOG_TARGET,
                "[launch 2] I regia: fake logcat line",
            ),
            line(
                2,
                "INFO",
                "regia_editor::pie_process",
                "[launch 2] not a launch target",
            ),
            line(
                3,
                "DEBUG",
                crate::launch::LOG_TARGET,
                "[launch 2] below the level",
            ),
        ]);
        assert_eq!(state.shown(), 1);
        assert_eq!(state.shown_record(0).map(|r| r.seq), Some(1));
    }

    #[test]
    fn only_a_screenful_of_rows_is_laid_out_and_a_click_expands_one() {
        let mut state = LogPanelState {
            min_level: String::new(),
            next_seq: Some(0),
            ..Default::default()
        };
        state.ingest(
            (0..20_000u64).map(|i| line(i, "INFO", "scale::draw", &format!("row {i:05}\nsecond line {i}"))),
        );
        // Past every ring record: the panel's polls add nothing from other tests' logging.
        state.next_seq = Some(u64::MAX - 1);
        let texts = draw(&mut state, 3);
        let rows = texts.iter().filter(|t| t.starts_with("row ")).count();
        assert!(rows > 5 && rows < 80, "a screenful of rows: {rows}");
        assert!(
            texts.iter().any(|t| t == "row 19999"),
            "stuck to the bottom: the newest line is on screen"
        );
        assert!(
            !texts.iter().any(|t| t.contains("second line")),
            "rows show the first line only"
        );
        state.expanded = Some(19_999);
        let texts = draw(&mut state, 2);
        assert!(
            texts.iter().any(|t| t.contains("second line 19999")),
            "the expanded row shows its whole message: {texts:?}"
        );
    }

    fn lines(spec: &[(&str, &str)]) -> Vec<LogRecord> {
        spec.iter()
            .enumerate()
            .map(|(i, (target, message))| line(i as u64, "INFO", target, message))
            .collect()
    }

    /// P-154: identical consecutive lines are one row with a count; a different line between
    /// them starts a new row; turning the collapse off shows every line again.
    #[test]
    fn identical_consecutive_lines_collapse_with_a_count() {
        let mut state = LogPanelState {
            next_seq: Some(0),
            ..Default::default()
        };
        state.refilter();
        state.ingest(lines(&[
            ("net", "retry"),
            ("net", "retry"),
            ("net", "retry"),
            ("net", "connected"),
            ("net", "retry"),
            ("gpu", "retry"),
        ]));
        assert_eq!(state.shown(), 4, "retry ×3, connected, retry, gpu retry");
        assert_eq!(state.shown_count(0), Some(3));
        assert_eq!(
            state.shown_record(0).map(|r| r.seq),
            Some(2),
            "a row shows its newest line"
        );
        assert_eq!(state.shown_count(2), Some(1));
        // A later repeat joins the last row, as it arrives.
        state.ingest([line(6, "INFO", "gpu", "retry")]);
        assert_eq!((state.shown(), state.shown_count(3)), (4, Some(2)));
        assert!(
            state.copy_all_text().contains("INFO net retry (×3)"),
            "{}",
            state.copy_all_text()
        );

        state.collapse_duplicates = false;
        state.refilter();
        assert_eq!(state.shown(), 7);
        assert_eq!(state.shown_count(0), Some(1));

        state.collapse_duplicates = true;
        state.next_seq = Some(u64::MAX - 1);
        let texts = draw(&mut state, 2);
        assert!(texts.iter().any(|t| t == "×3"), "the count is drawn: {texts:?}");
    }

    /// Review sprint-w12-rev-g5 #6: egui's second UI pass of a frame sees the same click; the
    /// Output Log toggles apply it once, so grouping, a group header and an expanded line stick.
    #[test]
    fn log_toggles_apply_once_per_frame() {
        let mut state = LogPanelState::default();
        assert!(state.apply_toggles(vec![LogToggle::GroupBySource(true), LogToggle::Expand(4)], 3));
        // The second pass reads the new values and asks to flip them back.
        assert!(!state.apply_toggles(vec![LogToggle::GroupBySource(false), LogToggle::Expand(4)], 3));
        assert!(state.group_by_source && state.expanded == Some(4));
        assert!(state.apply_toggles(
            vec![
                LogToggle::Group("regia_net".into()),
                LogToggle::CollapseDuplicates(false)
            ],
            4
        ));
        assert!(!state.apply_toggles(vec![LogToggle::Group("regia_net".into())], 4));
        assert!(!state.group_open("regia_net"), "the header stays collapsed");
        assert!(!state.collapse_duplicates);
        assert!(state.apply_toggles(vec![LogToggle::Expand(4)], 5));
        assert_eq!(state.expanded, None, "a later click collapses the line");
    }

    /// P-154: grouped by source, one collapsible header per target with its rows; a closed group
    /// shows its header only; the drawn list and Copy all follow.
    #[test]
    fn group_by_source_lists_collapsible_groups() {
        let mut state = LogPanelState {
            next_seq: Some(0),
            group_by_source: true,
            ..Default::default()
        };
        state.refilter();
        state.ingest(lines(&[
            ("regia_render::gi", "probe a"),
            ("regia_net", "joined"),
            ("regia_render::gi", "probe b"),
            ("regia_net", "left"),
            ("regia_net", "left"),
        ]));
        assert_eq!(
            state.groups(),
            [("regia_net".to_owned(), 2), ("regia_render::gi".to_owned(), 2)]
        );
        assert_eq!(state.item_count(), 2 + 4);
        assert_eq!(state.item(0), Some(ListItem::Header("regia_net", 2, true)));
        assert_eq!(state.item(3), Some(ListItem::Header("regia_render::gi", 2, true)));
        state.toggle_group("regia_net");
        assert!(!state.group_open("regia_net"));
        assert_eq!(state.item_count(), 2 + 2);
        assert_eq!(state.item(1), Some(ListItem::Header("regia_render::gi", 2, true)));
        assert_eq!(state.item(4), None);
        let copied = state.copy_all_text();
        assert_eq!(
            copied,
            "[regia_net] (2)\n[regia_render::gi] (2)\nINFO regia_render::gi probe a\nINFO regia_render::gi probe b"
        );
        state.next_seq = Some(u64::MAX - 1);
        let texts = draw(&mut state, 2);
        assert!(texts.iter().any(|t| t == "▶ regia_net (2)"), "{texts:?}");
        assert!(texts.iter().any(|t| t == "▼ regia_render::gi (2)"), "{texts:?}");
        assert!(texts.iter().any(|t| t == "probe b"), "{texts:?}");
        assert!(
            !texts.iter().any(|t| t == "joined"),
            "a closed group hides its rows: {texts:?}"
        );
    }

    /// Groups drop their rows with the kept lines, so the grouped view never shows a line the
    /// history no longer holds.
    #[test]
    fn groups_forget_lines_that_leave_the_history() {
        let mut state = LogPanelState {
            min_level: String::new(),
            next_seq: Some(0),
            ..Default::default()
        };
        state.refilter();
        let total = HISTORY_CAP as u64 + 10;
        state.ingest((0..total).map(|i| {
            let target = if i < 10 { "early" } else { "late" };
            line(i, "INFO", target, &format!("n {i}"))
        }));
        assert_eq!(state.groups(), [("late".to_owned(), HISTORY_CAP)]);
        let rows: usize = state.groups().iter().map(|(_, n)| n).sum();
        assert_eq!(rows, state.shown());
    }

    /// Copy selected copies the expanded line; without one it has nothing.
    #[test]
    fn copy_selected_is_the_expanded_line() {
        let mut state = LogPanelState {
            next_seq: Some(0),
            ..Default::default()
        };
        state.refilter();
        state.ingest(lines(&[("a", "one\ntwo"), ("b", "three")]));
        assert_eq!(state.selected_text(), None);
        state.expanded = Some(0);
        assert_eq!(state.selected_text().as_deref(), Some("INFO a one\ntwo"));
    }

    /// Accessibility bar: the new controls are reached and worked from the keyboard alone (Tab to
    /// the checkbox, Space; Tab to a group header, Enter).
    #[test]
    fn grouping_and_group_headers_work_from_the_keyboard() {
        use crate::canvas_keys::test_input::{frame, key, tab_to};
        let mut state = LogPanelState {
            next_seq: Some(0),
            ..Default::default()
        };
        state.refilter();
        state.ingest(lines(&[("alpha", "x"), ("beta", "y")]));
        state.next_seq = Some(u64::MAX - 1);
        let ctx = egui::Context::default();
        let size = [1200.0, 600.0];
        let draw = |ui: &mut egui::Ui, state: &mut LogPanelState| {
            egui::CentralPanel::default().show(ui, |ui| {
                let _ = output_log_panel(ui, state);
            });
        };
        frame(&ctx, size, vec![], |ui| draw(ui, &mut state));
        tab_to(&ctx, size, "Group by source", false, 40, |ui| {
            draw(ui, &mut state)
        });
        frame(
            &ctx,
            size,
            vec![key(egui::Key::Space, egui::Modifiers::NONE)],
            |ui| draw(ui, &mut state),
        );
        assert!(state.group_by_source, "Space toggles the focused checkbox");
        tab_to(&ctx, size, "▼ alpha (1)", false, 40, |ui| draw(ui, &mut state));
        frame(
            &ctx,
            size,
            vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
            |ui| draw(ui, &mut state),
        );
        assert!(!state.group_open("alpha"), "Enter collapses the focused group");
        tab_to(&ctx, size, "Copy all", false, 40, |ui| draw(ui, &mut state));
    }
}
