// SPDX-License-Identifier: MPL-2.0
//! The Theme panel: edit a runtime UI `.theme.ron` with lint and save
//! (`docs/specs/ui-default-design.md` § 7, lane D10, ADR-371).
//!
//! [`ThemePanel`] is the model (a world resource, so the panel and the agent methods in
//! `crate::theme_panel_agent` share it). It holds a working copy of one theme (the draft) inside
//! a private [`ThemeSet`] with the built-in themes, so every edit is validated against the whole
//! `extends` chain before it is accepted: a rejected edit leaves the draft exactly as it was.
//! [`theme_panel`] draws it with egui: token rows grouped by § 2 section with colour pickers and
//! sliders, a swatch preview of the main roles, the lint findings and a save row.
//!
//! The panel never touches the running game's theme; the file it saves is what a game loads.
//! The editor chrome stays on the ADR-229 tokens (`crate::theme`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regia_ecs::prelude::*;
use regia_ui::UiColor;
use regia_ui::theme_file::{
    BUILTIN_THEME_SOURCES, Density, PropMap, PropValue, ResolvedTheme, ThemeFile, ThemeSet, TokenValue,
    TypeDef, parse_theme_ron,
};
use regia_ui::theme_resolve::builtin_theme_set;
use regia_ui::theme_tokens::{Easing, Value};

use crate::shared_panel_history::{SharedDoc, SharedHistory, StepKind};

/// A failed panel operation: a stable lowercase `code` (documented in `book/agents/ui-theme-panel.md`)
/// and a message that names the key path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelError {
    /// Stable machine-readable code.
    pub code: &'static str,
    /// What went wrong.
    pub message: String,
}

impl PanelError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for PanelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

/// One token row of the panel.
#[derive(Debug, Clone, PartialEq)]
pub struct TokenRow {
    /// Token key (`color.accent`).
    pub key: String,
    /// The § 2 section the key belongs to (`Color`, `Spacing`, ...).
    pub group: &'static str,
    /// The resolved kind: `color`, `number`, `duration`, `easing` or `text`.
    pub kind: &'static str,
    /// The resolved value as text (colours as `#rrggbb`, or `#rrggbbaa` when not opaque, durations in ms).
    pub value: String,
    /// The draft's own expression for this key; `None` when the value is inherited.
    pub raw: Option<String>,
    /// The resolved colour as sRGB bytes, for colour tokens.
    pub rgba: Option<[u8; 4]>,
    /// The resolved number, for number tokens.
    pub number: Option<f32>,
}

/// Severity of a lint finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// The theme is unusable or fails an accessibility requirement.
    Error,
    /// Worth a look; the theme still works.
    Warning,
}

impl Severity {
    /// `error` or `warning`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
        }
    }
}

/// One lint finding.
#[derive(Debug, Clone, PartialEq)]
pub struct LintFinding {
    /// `contrast`, `touch_target`, `unknown_key` or `resolve_error`.
    pub code: &'static str,
    /// How serious it is.
    pub severity: Severity,
    /// The token key (or the foreground key of a contrast pair).
    pub key: String,
    /// A readable explanation including the numbers.
    pub message: String,
}

/// A contrast pair checked by the lint: foreground, background, required ratio (WCAG 2.x).
///
/// The eleven pairs are `color.text` on `color.surface` (4.5), `color.text-muted` on
/// `color.surface` (4.5), `color.on-accent` on `color.accent` (4.5), `color.on-danger` on
/// `color.danger` (4.5), `color.accent` on `color.surface` (3.0), `color.danger` on
/// `color.surface` (4.5), `color.warning` on `color.surface` (4.5), `color.success` on
/// `color.surface` (4.5), `color.border-strong` on `color.surface` (3.0), `color.focus-ring` on
/// `color.bg` (3.0) and `color.text` on `color.surface-raised` (4.5). `docs/specs/ui-default-design.md`
/// § 2.1 is a different table; this const is what `lint` checks.
pub const CONTRAST_PAIRS: [(&str, &str, f32); 11] = [
    ("color.text", "color.surface", 4.5),
    ("color.text-muted", "color.surface", 4.5),
    ("color.on-accent", "color.accent", 4.5),
    ("color.on-danger", "color.danger", 4.5),
    ("color.accent", "color.surface", 3.0),
    ("color.danger", "color.surface", 4.5),
    ("color.warning", "color.surface", 4.5),
    ("color.success", "color.surface", 4.5),
    ("color.border-strong", "color.surface", 3.0),
    ("color.focus-ring", "color.bg", 3.0),
    ("color.text", "color.surface-raised", 4.5),
];

/// The roles the swatch preview shows: label, foreground, background.
pub const PREVIEW_ROLES: [(&str, &str, &str); 7] = [
    ("Body text", "color.text", "color.surface"),
    ("Muted text", "color.text-muted", "color.surface"),
    ("Raised", "color.text", "color.surface-raised"),
    ("Primary", "color.on-accent", "color.accent"),
    ("Danger", "color.on-danger", "color.danger"),
    ("Overlay", "color.text", "color.surface-overlay"),
    ("Field", "color.text-muted", "color.surface-sunken"),
];

/// Whether `name` is a built-in theme (`regia_ui`'s `BUILTIN_THEME_SOURCES`): a draft cannot be
/// saved under one of them.
fn is_builtin_name(name: &str) -> bool {
    BUILTIN_THEME_SOURCES.iter().any(|(builtin, _)| *builtin == name)
}

/// The panel model: the draft theme, its validation set and the UI state.
#[derive(Resource, Clone)]
pub struct ThemePanel {
    set: ThemeSet,
    draft: Option<ThemeFile>,
    path: Option<PathBuf>,
    saved: Option<String>,
    density: Density,
    revision: u64,
    undo: Vec<(ThemeSet, ThemeFile)>,
    redo: Vec<(ThemeSet, ThemeFile)>,
    /// The open draft's identity on the shared history (P-322): a new one per open.
    serial: u64,
    /// The draft's edits on their way to the editor's shared history.
    shared_history: SharedHistory<ThemeFile>,
    selection: StyleSelection,
    type_name: String,
    variation_name: String,
    variation_base: String,
    property_name: String,
    property_value: String,
    import_from: String,
    import_kind: String,
    import_type: String,
    import_state: String,
    import_values: bool,
    pin: Option<StyleSelection>,
    last_ui_frame: Option<u64>,
    /// Token key filter (a substring; empty shows every key).
    pub filter: String,
    /// The last operation's error, shown in the panel.
    pub error: Option<String>,
    /// Text buffer: name of a new child theme.
    pub new_name: String,
    /// Text buffer: save path.
    pub save_path: String,
}

impl Default for ThemePanel {
    fn default() -> Self {
        Self {
            set: builtin_theme_set(),
            draft: None,
            path: None,
            saved: None,
            density: Density::Comfortable,
            revision: 0,
            undo: Vec::new(),
            redo: Vec::new(),
            serial: next_serial(),
            shared_history: SharedHistory::default(),
            selection: StyleSelection::default(),
            type_name: String::new(),
            variation_name: String::new(),
            variation_base: "Button".into(),
            property_name: "background".into(),
            property_value: String::new(),
            import_from: "regia-dark".into(),
            import_kind: "all".into(),
            import_type: String::new(),
            import_state: "normal".into(),
            import_values: true,
            pin: None,
            last_ui_frame: None,
            filter: String::new(),
            error: None,
            new_name: String::new(),
            save_path: String::new(),
        }
    }
}

/// The § 2 section a token key belongs to.
pub fn group_of(key: &str) -> &'static str {
    match key.split('.').next().unwrap_or_default() {
        "color" => "Color",
        "space" => "Spacing",
        "radius" | "border" | "shadow" | "elevation" => "Shape",
        "font" | "type" | "text" => "Type",
        "motion" | "ease" => "Motion",
        "size" => "Size",
        _ => "Other",
    }
}

/// Section names in display order.
pub const GROUPS: [&str; 7] = ["Color", "Spacing", "Shape", "Type", "Motion", "Size", "Other"];

fn srgb_encode(linear: f32) -> u8 {
    let c = linear.clamp(0.0, 1.0);
    let s = if c <= 0.003_130_8 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    };
    (s * 255.0).round().clamp(0.0, 255.0) as u8
}

/// A colour as sRGB bytes `[r, g, b, a]`.
pub fn to_srgb8(c: UiColor) -> [u8; 4] {
    [
        srgb_encode(c.0[0]),
        srgb_encode(c.0[1]),
        srgb_encode(c.0[2]),
        (c.0[3].clamp(0.0, 1.0) * 255.0).round() as u8,
    ]
}

/// `#rrggbb`, or `#rrggbbaa` when not opaque.
pub fn to_hex(rgba: [u8; 4]) -> String {
    if rgba[3] == 255 {
        format!("#{:02x}{:02x}{:02x}", rgba[0], rgba[1], rgba[2])
    } else {
        format!("#{:02x}{:02x}{:02x}{:02x}", rgba[0], rgba[1], rgba[2], rgba[3])
    }
}

/// WCAG 2.x relative luminance of a linear-RGB colour.
fn luminance(c: UiColor) -> f32 {
    0.2126 * c.0[0] + 0.7152 * c.0[1] + 0.0722 * c.0[2]
}

/// WCAG 2.x contrast ratio between two opaque colours (1 to 21).
pub fn contrast_ratio(a: UiColor, b: UiColor) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

fn describe(value: &Value) -> String {
    match value {
        Value::Color(c) => to_hex(to_srgb8(*c)),
        Value::Number(n) => format!("{n}"),
        Value::Duration(s) => format!("{}ms", (s * 1000.0).round()),
        Value::Easing(Easing::Linear) => "linear".to_owned(),
        Value::Easing(Easing::CubicBezier(p)) => {
            format!("cubic-bezier({}, {}, {}, {})", p[0], p[1], p[2], p[3])
        }
        Value::Text(t) => t.clone(),
    }
}

fn raw_text(value: &TokenValue) -> String {
    match value {
        TokenValue::Number(n) => format!("{n}"),
        TokenValue::Text(t) => t.clone(),
    }
}

impl ThemePanel {
    /// A panel with no draft open.
    pub fn new() -> Self {
        Self::default()
    }

    /// Names of every theme the panel can open: the built-ins and the ones loaded from files.
    pub fn theme_names(&self) -> Vec<String> {
        self.set.names().map(str::to_owned).collect()
    }

    /// The draft, if one is open.
    pub fn draft(&self) -> Option<&ThemeFile> {
        self.draft.as_ref()
    }

    /// The file the draft was loaded from or last saved to.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Increments on every accepted change (agents `wait_until` on it).
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// The density the rows and the lint resolve for.
    pub fn density(&self) -> Density {
        self.density
    }

    /// Switches the density the rows and the lint resolve for.
    pub fn set_density(&mut self, density: Density) {
        self.density = density;
    }

    /// True when the draft differs from what was last saved (or loaded).
    pub fn dirty(&self) -> bool {
        match (&self.draft, &self.saved) {
            (Some(d), Some(saved)) => match d.to_ron_string() {
                Ok(text) => &text != saved,
                Err(_) => true,
            },
            (Some(_), None) => true,
            _ => false,
        }
    }

    fn fail(&mut self, e: PanelError) -> PanelError {
        self.error = Some(e.to_string());
        e
    }

    /// A copy of the draft, or `no_draft` (recorded as the panel error).
    fn draft_clone(&mut self) -> Result<ThemeFile, PanelError> {
        match self.draft.clone() {
            Some(file) => Ok(file),
            None => Err(self.fail(PanelError::new("no_draft", "open a theme first"))),
        }
    }

    fn require_draft(&self) -> Result<&ThemeFile, PanelError> {
        self.draft
            .as_ref()
            .ok_or_else(|| PanelError::new("no_draft", "open a theme first"))
    }

    /// Opens a copy of a theme from the set as the draft. Editing a built-in is allowed; saving
    /// it needs a new name ([`ThemePanel::rename`]).
    pub fn open(&mut self, name: &str) -> Result<(), PanelError> {
        let Some(file) = self.set.get(name).cloned() else {
            return Err(self.fail(PanelError::new(
                "unknown_theme",
                format!("no theme named `{name}`"),
            )));
        };
        self.undo.clear();
        self.redo.clear();
        self.serial = next_serial();
        self.selection = StyleSelection::default();
        self.pin = None;
        self.saved = file.to_ron_string().ok();
        self.draft = Some(file);
        self.path = None;
        self.error = None;
        self.revision += 1;
        Ok(())
    }

    /// Opens a `.theme.ron` file (older versions are migrated on load) as the draft.
    pub fn open_path(&mut self, path: &Path) -> Result<(), PanelError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| PanelError::new("io_error", format!("{}: {e}", path.display())))
            .map_err(|e| self.fail(e))?;
        let file = parse_theme_ron(&text, &path.display().to_string())
            .map_err(|e| PanelError::new("parse_error", e.to_string()))
            .map_err(|e| self.fail(e))?;
        self.set
            .insert(file.clone())
            .map_err(|e| PanelError::new("parse_error", e.to_string()))
            .map_err(|e| self.fail(e))?;
        self.undo.clear();
        self.redo.clear();
        self.serial = next_serial();
        self.selection = StyleSelection::default();
        self.pin = None;
        self.saved = file.to_ron_string().ok();
        self.draft = Some(file);
        self.path = Some(path.to_path_buf());
        self.error = None;
        self.revision += 1;
        Ok(())
    }

    /// Starts a new empty theme that extends `parent` (the § 6.7 re-skin workflow).
    pub fn new_child(&mut self, name: &str, parent: &str) -> Result<(), PanelError> {
        let name = name.trim();
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(self.fail(PanelError::new(
                "invalid_name",
                "a theme name uses letters, digits, `-` and `_`",
            )));
        }
        if self.set.get(name).is_some() {
            return Err(self.fail(PanelError::new(
                "name_taken",
                format!("a theme named `{name}` exists"),
            )));
        }
        if self.set.get(parent).is_none() {
            return Err(self.fail(PanelError::new(
                "unknown_theme",
                format!("no parent theme named `{parent}`"),
            )));
        }
        let file = ThemeFile {
            name: name.to_owned(),
            extends: Some(parent.to_owned()),
            ..ThemeFile::default()
        };
        self.set
            .insert(file.clone())
            .map_err(|e| PanelError::new("invalid_token", e.to_string()))
            .map_err(|e| self.fail(e))?;
        self.undo.clear();
        self.redo.clear();
        self.serial = next_serial();
        self.selection = StyleSelection::default();
        self.pin = None;
        self.draft = Some(file);
        self.saved = None;
        self.path = None;
        self.error = None;
        self.revision += 1;
        Ok(())
    }

    /// Renames the draft (needed before saving a copy of a built-in).
    pub fn rename(&mut self, name: &str) -> Result<(), PanelError> {
        let mut file = self.draft_clone()?;
        if name != file.name && self.set.get(name).is_some() {
            return Err(self.fail(PanelError::new(
                "name_taken",
                format!("a theme named `{name}` exists"),
            )));
        }
        name.clone_into(&mut file.name);
        self.commit(file)
    }

    /// Validates `file` against the whole set and, on success, makes it the draft.
    fn commit(&mut self, file: ThemeFile) -> Result<(), PanelError> {
        let before = self.draft.clone().map(|draft| (self.set.clone(), draft));
        if let Err(e) = self.set.insert(file.clone()) {
            return Err(self.fail(PanelError::new("invalid_token", e.to_string())));
        }
        if let Some((_, draft)) = &before {
            let path = self.history_path();
            self.shared_history
                .record(self.serial, &path, draft, &file, StepKind::Discrete, "theme");
        }
        if let Some(before) = before.filter(|_| !self.shared_history.is_shared()) {
            self.undo.push(before);
            if self.undo.len() > 100 {
                self.undo.remove(0);
            }
        }
        self.redo.clear();
        self.draft = Some(file);
        self.reconcile_selection();
        self.error = None;
        self.revision += 1;
        Ok(())
    }

    fn reconcile_selection(&mut self) {
        let Ok(theme) = self.resolved() else {
            return;
        };
        if definition(&theme, &self.selection.type_name).is_none() {
            self.selection = StyleSelection::default();
        }
        if self
            .pin
            .as_ref()
            .is_some_and(|p| definition(&theme, &p.type_name).is_none())
        {
            self.pin = None;
        }
    }

    /// Sets one token from text: a number (`12`) or an expression (`#e8793a`, `80ms`,
    /// `@color.accent`, `lighten(@color.accent, 0.06)`). The whole set is re-validated; a bad value
    /// changes nothing.
    pub fn set_token(&mut self, key: &str, text: &str) -> Result<(), PanelError> {
        let mut file = self.draft_clone()?;
        let key = key.trim();
        if key.is_empty() || key.contains(char::is_whitespace) {
            return Err(self.fail(PanelError::new("unknown_token", "a token key has no spaces")));
        }
        let text = text.trim();
        let value = match text.parse::<f32>() {
            Ok(n) if n.is_finite() => TokenValue::Number(n),
            _ => TokenValue::Text(text.to_owned()),
        };
        value
            .parse()
            .map_err(|m| PanelError::new("invalid_token", format!("tokens.{key}: {m}")))
            .map_err(|e| self.fail(e))?;
        file.tokens.insert(key.to_owned(), value);
        self.commit(file)
    }

    /// Sets a colour token from sRGB bytes.
    pub fn set_color(&mut self, key: &str, rgba: [u8; 4]) -> Result<(), PanelError> {
        self.set_token(key, &to_hex(rgba))
    }

    /// Removes the draft's own value for `key`, so the parent's (or the built-in default) applies.
    pub fn reset_token(&mut self, key: &str) -> Result<(), PanelError> {
        let mut file = self.draft_clone()?;
        if file.tokens.remove(key).is_none() {
            return Err(self.fail(PanelError::new(
                "unknown_token",
                format!("the draft does not set `{key}`"),
            )));
        }
        self.commit(file)
    }

    /// Puts back a draft taken earlier with [`ThemePanel::draft`] and its
    /// [`ThemePanel::revision`], undoing a batch of edits that failed part way.
    pub fn restore(&mut self, draft: ThemeFile, revision: u64) {
        if self.set.insert(draft.clone()).is_ok() {
            self.draft = Some(draft);
            self.revision = revision;
        }
    }

    /// The draft resolved for the panel's density.
    pub fn resolved(&self) -> Result<ResolvedTheme, PanelError> {
        let draft = self.require_draft()?;
        self.set
            .resolve(&draft.name, Some(self.density))
            .map_err(|e| PanelError::new("invalid_token", e.to_string()))
    }

    /// Token rows of the draft (own and inherited), sorted by section then key, keys containing
    /// `filter` only. An empty `filter` keeps all.
    pub fn rows(&self, filter: &str) -> Result<Vec<TokenRow>, PanelError> {
        let theme = self.resolved()?;
        let draft = self.require_draft()?;
        let needle = filter.trim();
        let mut rows: Vec<TokenRow> = theme
            .values
            .iter()
            .filter(|(key, _)| needle.is_empty() || key.contains(needle))
            .map(|(key, value)| TokenRow {
                key: key.clone(),
                group: group_of(key),
                kind: value.kind(),
                value: describe(value),
                raw: draft.tokens.get(key).map(raw_text),
                rgba: value.as_color().map(to_srgb8),
                number: value.as_number(),
            })
            .collect();
        rows.sort_by(|a, b| {
            let order = |g: &str| GROUPS.iter().position(|x| *x == g).unwrap_or(GROUPS.len());
            order(a.group)
                .cmp(&order(b.group))
                .then_with(|| a.key.cmp(&b.key))
        });
        Ok(rows)
    }

    /// Lints the draft: contrast pairs, touch size, keys the built-in theme does not define and
    /// resolve errors. An empty list means a clean theme.
    pub fn lint(&self) -> Result<Vec<LintFinding>, PanelError> {
        let draft = self.require_draft()?;
        let mut out = Vec::new();
        let theme = match self.set.resolve(&draft.name, Some(self.density)) {
            Ok(t) => t,
            Err(e) => {
                out.push(LintFinding {
                    code: "resolve_error",
                    severity: Severity::Error,
                    key: draft.name.clone(),
                    message: e.to_string(),
                });
                return Ok(out);
            }
        };
        for (fg, bg, required) in CONTRAST_PAIRS {
            if let (Some(f), Some(b)) = (theme.color(fg), theme.color(bg)) {
                let ratio = contrast_ratio(f, b);
                if ratio < required {
                    out.push(LintFinding {
                        code: "contrast",
                        severity: Severity::Error,
                        key: fg.to_owned(),
                        message: format!("`{fg}` on `{bg}` is {ratio:.2}:1, needs {required}:1"),
                    });
                }
            }
        }
        if let (Some(touch), Some(control)) =
            (theme.number("size.touch-min"), theme.number("size.control.md"))
            && control < touch
        {
            out.push(LintFinding {
                code: "touch_target",
                severity: Severity::Warning,
                key: "size.control.md".to_owned(),
                message: format!("`size.control.md` is {control} UI px, below `size.touch-min` {touch}"),
            });
        }
        for key in self.set.unknown_token_keys(&draft.name) {
            out.push(LintFinding {
                code: "unknown_key",
                severity: Severity::Warning,
                key: key.clone(),
                message: format!("`{key}` is not a token the built-in theme defines"),
            });
        }
        Ok(out)
    }

    /// The preview swatches: label, foreground and background as sRGB bytes. Roles the theme does
    /// not resolve are skipped.
    pub fn preview(&self) -> Vec<(&'static str, [u8; 4], [u8; 4])> {
        let Ok(theme) = self.resolved() else {
            return Vec::new();
        };
        PREVIEW_ROLES
            .iter()
            .filter_map(|(label, fg, bg)| {
                Some((*label, to_srgb8(theme.color(fg)?), to_srgb8(theme.color(bg)?)))
            })
            .collect()
    }

    /// The draft as `.theme.ron` text.
    pub fn to_ron(&self) -> Result<String, PanelError> {
        self.require_draft()?
            .to_ron_string()
            .map_err(|e| PanelError::new("io_error", e.to_string()))
    }

    /// Saves the draft to `path` through a temp file and a rename. A built-in theme name is
    /// refused (rename first), and the text is parsed back before it is written, so a saved file
    /// always loads.
    pub fn save_to(&mut self, path: &Path) -> Result<(), PanelError> {
        let name = self.draft_clone()?.name;
        if is_builtin_name(&name) {
            return Err(self.fail(PanelError::new(
                "builtin_name",
                format!("`{name}` is a built-in theme; rename the draft before saving"),
            )));
        }
        let text = self.to_ron().map_err(|e| self.fail(e))?;
        parse_theme_ron(&text, &path.display().to_string())
            .map_err(|e| PanelError::new("parse_error", e.to_string()))
            .map_err(|e| self.fail(e))?;
        let io = |e: std::io::Error| PanelError::new("io_error", format!("{}: {e}", path.display()));
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)
                .map_err(io)
                .map_err(|e| self.fail(e))?;
        }
        let tmp = path.with_extension("ron.tmp");
        std::fs::write(&tmp, &text)
            .map_err(io)
            .map_err(|e| self.fail(e))?;
        std::fs::rename(&tmp, path)
            .map_err(io)
            .map_err(|e| self.fail(e))?;
        self.saved = Some(text);
        self.path = Some(path.to_path_buf());
        self.error = None;
        self.revision += 1;
        Ok(())
    }
}

fn color32(rgba: [u8; 4]) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(rgba[0], rgba[1], rgba[2], rgba[3])
}

/// Draws the Theme panel and applies its edits. The model is a world resource ([`ThemePanel`]).
pub fn theme_panel(ui: &mut egui::Ui, world: &mut World) {
    let mut panel = world.remove_resource::<ThemePanel>().unwrap_or_default();
    let frame = ui.ctx().cumulative_frame_nr();
    let apply = panel.last_ui_frame != Some(frame);
    panel.last_ui_frame = Some(frame);
    let editor_source = editor_import_source(crate::theme::tokens(world));
    egui::ScrollArea::vertical()
        .id_salt("theme_panel_content")
        .show(ui, |ui| {
            draw(ui, &mut panel);
            if panel.draft().is_some() {
                crate::theme_gallery::draw_gallery(ui, world, &mut panel);
                draw_styles(ui, &mut panel, &editor_source, apply);
            }
        });
    world.insert_resource(panel);
}

fn draw(ui: &mut egui::Ui, panel: &mut ThemePanel) {
    ui.horizontal(|ui| {
        let current = panel.draft().map(|d| d.name.clone()).unwrap_or_default();
        egui::ComboBox::from_id_salt("theme_panel_open")
            .selected_text(if current.is_empty() {
                "Open theme…"
            } else {
                current.as_str()
            })
            .show_ui(ui, |ui| {
                for name in panel.theme_names() {
                    if ui.selectable_label(name == current, &name).clicked() {
                        let _ = panel.open(&name);
                    }
                }
            });
        ui.add(
            egui::TextEdit::singleline(&mut panel.new_name)
                .hint_text("new theme name")
                .desired_width(140.0),
        );
        if ui.button("New child").clicked() && !current.is_empty() {
            let name = panel.new_name.clone();
            let _ = panel.new_child(&name, &current);
        }
        let mut density = panel.density();
        ui.selectable_value(&mut density, Density::Comfortable, "Comfortable");
        ui.selectable_value(&mut density, Density::Compact, "Compact");
        panel.set_density(density);
    });
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut panel.save_path)
                .hint_text("path/to/my.theme.ron")
                .desired_width(280.0),
        );
        if ui.button("Save").clicked() && !panel.save_path.trim().is_empty() {
            let path = PathBuf::from(panel.save_path.trim());
            let _ = panel.save_to(&path);
        }
        if ui.button("Open file").clicked() && !panel.save_path.trim().is_empty() {
            let path = PathBuf::from(panel.save_path.trim());
            let _ = panel.open_path(&path);
        }
        if panel.dirty() {
            ui.weak("unsaved changes");
        }
    });
    if let Some(error) = &panel.error {
        ui.colored_label(ui.visuals().error_fg_color, error);
    }
    if panel.draft().is_none() {
        ui.weak("Open a built-in theme, or start a child of one, to edit its tokens.");
        return;
    }
    ui.separator();
    draw_preview(ui, panel);
    ui.add(
        egui::TextEdit::singleline(&mut panel.filter)
            .hint_text("filter tokens")
            .desired_width(220.0),
    );
    let filter = panel.filter.clone();
    let rows = panel.rows(&filter).unwrap_or_default();
    egui::ScrollArea::vertical()
        .id_salt("theme_panel_rows")
        .max_height(360.0)
        .show(ui, |ui| {
            for group in GROUPS {
                let in_group: Vec<&TokenRow> = rows.iter().filter(|r| r.group == group).collect();
                if in_group.is_empty() {
                    continue;
                }
                egui::CollapsingHeader::new(format!("{group} ({})", in_group.len()))
                    .id_salt(("theme_group", group))
                    .default_open(group == "Color")
                    .show(ui, |ui| {
                        for row in in_group {
                            draw_row(ui, panel, row);
                        }
                    });
            }
        });
    ui.separator();
    draw_lint(ui, panel);
}

fn draw_row(ui: &mut egui::Ui, panel: &mut ThemePanel, row: &TokenRow) {
    ui.horizontal(|ui| {
        let label = ui.label(&row.key);
        if let Some(raw) = &row.raw {
            label.on_hover_text(format!("set here: {raw}"));
        } else {
            label.on_hover_text("inherited");
        }
        if let Some(rgba) = row.rgba {
            let mut c = rgba;
            if ui.color_edit_button_srgba_unmultiplied(&mut c).changed() {
                let _ = panel.set_color(&row.key, c);
            }
            ui.monospace(&row.value);
        } else if let (Some(n), "number") = (row.number, row.kind) {
            let mut v = n;
            if ui.add(egui::DragValue::new(&mut v).speed(0.25)).changed() {
                let _ = panel.set_token(&row.key, &format!("{v}"));
            }
        } else {
            let mut text = row.raw.clone().unwrap_or_else(|| row.value.clone());
            let response = ui.add(egui::TextEdit::singleline(&mut text).desired_width(200.0));
            if response.lost_focus() && text != row.value && Some(&text) != row.raw.as_ref() {
                let _ = panel.set_token(&row.key, &text);
            }
        }
        if row.raw.is_some() && ui.small_button("reset").clicked() {
            let _ = panel.reset_token(&row.key);
        }
    });
}

fn draw_preview(ui: &mut egui::Ui, panel: &ThemePanel) {
    ui.horizontal_wrapped(|ui| {
        for (label, fg, bg) in panel.preview() {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(96.0, 34.0), egui::Sense::hover());
            ui.painter().rect_filled(rect, 4.0, color32(bg));
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                label,
                egui::FontId::proportional(13.0),
                color32(fg),
            );
        }
    });
}

fn draw_lint(ui: &mut egui::Ui, panel: &ThemePanel) {
    match panel.lint() {
        Ok(findings) if findings.is_empty() => {
            ui.weak("Lint: clean");
        }
        Ok(findings) => {
            ui.label(format!("Lint: {} finding(s)", findings.len()));
            for f in findings {
                let color = match f.severity {
                    Severity::Error => ui.visuals().error_fg_color,
                    Severity::Warning => ui.visuals().warn_fg_color,
                };
                ui.colored_label(color, format!("{}: {}", f.code, f.message));
            }
        }
        Err(e) => {
            ui.colored_label(ui.visuals().error_fg_color, e.to_string());
        }
    }
}

/// The next draft identity on the shared history.
fn next_serial() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

impl ThemePanel {
    /// What the shared history names the draft by: its file, or the theme's name while unsaved.
    fn history_path(&self) -> String {
        match (&self.path, &self.draft) {
            (Some(path), _) => path.display().to_string().replace('\\', "/"),
            (None, Some(draft)) => format!("theme {}", draft.name),
            (None, None) => "theme".to_owned(),
        }
    }
}

/// The Theme panel's draft on the editor's shared history (P-322, `crate::shared_panel_history`).
pub(crate) struct ThemeHistory;

impl SharedDoc for ThemeHistory {
    type State = ThemeFile;
    const PANEL: &'static str = "Theme";

    fn histories(world: &mut World) -> Vec<&mut SharedHistory<ThemeFile>> {
        world
            .get_resource_mut::<ThemePanel>()
            .map(|panel| vec![&mut panel.into_inner().shared_history])
            .unwrap_or_default()
    }

    fn restore(world: &mut World, doc: u64, path: &str, state: &ThemeFile) {
        let Some(mut panel) = world.get_resource_mut::<ThemePanel>() else {
            return;
        };
        if panel.serial != doc {
            tracing::warn!(target: "regia_editor", path, "theme history step for a draft that is no longer open");
            return;
        }
        if let Err(error) = panel.set.insert(state.clone()) {
            tracing::warn!(target: "regia_editor", path, %error, "theme history step did not validate");
            return;
        }
        panel.draft = Some(state.clone());
        panel.reconcile_selection();
        panel.error = None;
        panel.revision += 1;
    }

    fn open_doc(world: &World) -> Option<(u64, String)> {
        world
            .get_resource::<ThemePanel>()
            .filter(|panel| panel.draft.is_some())
            .map(|panel| (panel.serial, panel.history_path()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Review sprint-w12-rev-g9 #3: the click frame's second pass sees the same click; only the
    /// first pass's toggle sticks, so Copy values can be turned off.
    #[test]
    fn a_repeated_pass_does_not_flip_copy_values_back() {
        // Pass 1 of the click frame: on -> off sticks.
        let after_first = keep_toggle(true, false, true);
        assert!(!after_first);
        // Pass 2 sees the same click (off -> on) and must not apply it.
        assert!(!keep_toggle(after_first, true, false));
    }

    fn open(name: &str) -> ThemePanel {
        let mut p = ThemePanel::new();
        p.open(name).expect("open built-in");
        p
    }

    fn child() -> ThemePanel {
        let mut p = ThemePanel::new();
        p.new_child("my-game", "regia-dark").expect("new child");
        p
    }

    fn color_of(p: &ThemePanel, key: &str) -> UiColor {
        p.resolved().expect("resolves").color(key).expect("color token")
    }

    #[test]
    fn hex_round_trips_for_every_channel_value() {
        for v in 0..=255_u8 {
            let c = UiColor::from_srgb8(v, v, v, 255);
            assert_eq!(to_srgb8(c), [v, v, v, 255], "channel {v}");
        }
        assert_eq!(to_hex([0xe8, 0x79, 0x3a, 255]), "#e8793a");
        assert_eq!(to_hex([0, 0, 0, 0x80]), "#00000080");
    }

    #[test]
    fn contrast_ratio_known_values() {
        let black = UiColor::BLACK;
        let white = UiColor::WHITE;
        assert!((contrast_ratio(black, white) - 21.0).abs() < 1e-3);
        assert!((contrast_ratio(white, white) - 1.0).abs() < 1e-6);
        assert_eq!(contrast_ratio(black, white), contrast_ratio(white, black));
    }

    #[test]
    fn built_in_themes_lint_clean() {
        for (name, _) in BUILTIN_THEME_SOURCES {
            let p = open(name);
            let findings = p.lint().expect("lint");
            let hard: Vec<_> = findings
                .iter()
                .filter(|f| f.severity == Severity::Error)
                .collect();
            assert!(hard.is_empty(), "{name}: {hard:?}");
        }
    }

    #[test]
    fn rows_are_grouped_and_cover_the_sections() {
        let p = open("regia-dark");
        let rows = p.rows("").expect("rows");
        for group in ["Color", "Spacing", "Shape", "Type", "Motion", "Size"] {
            assert!(rows.iter().any(|r| r.group == group), "no {group} rows");
        }
        let mut last = 0;
        for r in &rows {
            let at = GROUPS.iter().position(|g| *g == r.group).expect("known group");
            assert!(at >= last, "rows sorted by section");
            last = at;
        }
        let accent = rows.iter().find(|r| r.key == "color.accent").expect("accent row");
        assert_eq!(accent.kind, "color");
        assert_eq!(accent.rgba, Some([0x3d, 0x9d, 0xf0, 255]));
        assert!(accent.raw.is_some(), "the dark theme sets its own accent");
        let only: Vec<_> = p.rows("motion.").expect("rows");
        assert!(!only.is_empty() && only.iter().all(|r| r.key.contains("motion.")));
    }

    #[test]
    fn child_inherits_then_overrides_one_token() {
        let mut p = child();
        let before = p.rows("").expect("rows");
        assert!(
            before.iter().all(|r| r.raw.is_none()),
            "an empty child owns nothing"
        );
        let parent_hover = color_of(&p, "color.accent-hover");
        p.set_color("color.accent", [0xe8, 0x79, 0x3a, 255])
            .expect("set accent");
        assert_eq!(to_srgb8(color_of(&p, "color.accent")), [0xe8, 0x79, 0x3a, 255]);
        assert_ne!(
            color_of(&p, "color.accent-hover"),
            parent_hover,
            "derived tokens follow the accent"
        );
        let rows = p.rows("color.accent").expect("rows");
        assert_eq!(rows.iter().filter(|r| r.raw.is_some()).count(), 1);
        p.reset_token("color.accent").expect("reset");
        assert_eq!(color_of(&p, "color.accent-hover"), parent_hover);
    }

    #[test]
    fn a_bad_value_changes_nothing() {
        let mut p = child();
        p.set_token("radius.md", "3").expect("number");
        let revision = p.revision();
        let draft = p.draft().cloned();
        let e = p.set_token("color.accent", "lighten(").expect_err("syntax");
        assert_eq!(e.code, "invalid_token");
        let e = p
            .set_token("color.accent", "@color.nope")
            .expect_err("dangling reference");
        assert_eq!(e.code, "invalid_token");
        assert!(
            e.message.contains("color.nope") || e.message.contains("color.accent"),
            "{}",
            e.message
        );
        let e = p
            .set_token("color.accent", "80ms")
            .expect_err("a duration is not a colour");
        assert_eq!(e.code, "invalid_token");
        assert_eq!(p.draft().cloned(), draft);
        assert_eq!(p.revision(), revision, "a rejected edit is not a change");
        assert!(p.error.is_some());
    }

    #[test]
    fn reset_of_an_unset_key_is_an_error() {
        let mut p = child();
        assert_eq!(
            p.reset_token("color.accent").expect_err("unset").code,
            "unknown_token"
        );
    }

    #[test]
    fn operations_need_a_draft() {
        let mut p = ThemePanel::new();
        assert_eq!(
            p.set_token("radius.md", "3").expect_err("no draft").code,
            "no_draft"
        );
        assert_eq!(p.lint().expect_err("no draft").code, "no_draft");
        assert_eq!(p.rows("").expect_err("no draft").code, "no_draft");
        assert!(p.preview().is_empty());
        assert!(!p.dirty());
        assert_eq!(p.open("nope").expect_err("unknown").code, "unknown_theme");
    }

    #[test]
    fn lint_flags_a_two_to_one_pair_and_names_the_numbers() {
        let mut p = child();
        // Mid grey text on the dark surface is well under 4.5:1.
        p.set_color("color.text", [0x2a, 0x32, 0x3d, 255]).expect("set");
        let findings = p.lint().expect("lint");
        let hit = findings
            .iter()
            .find(|f| f.code == "contrast" && f.key == "color.text")
            .expect("contrast finding");
        assert_eq!(hit.severity, Severity::Error);
        assert!(hit.message.contains("needs 4.5:1"), "{}", hit.message);
        assert!(hit.message.contains("color.surface"), "{}", hit.message);
    }

    #[test]
    fn lint_flags_unknown_keys_and_small_touch_targets() {
        let mut p = child();
        p.set_token("brand.glow", "#ffffff").expect("custom key");
        p.set_token("size.control.md", "30").expect("small control");
        let findings = p.lint().expect("lint");
        assert!(
            findings
                .iter()
                .any(|f| f.code == "unknown_key" && f.key == "brand.glow")
        );
        assert!(findings.iter().any(|f| f.code == "touch_target"));
        p.set_density(Density::Compact);
        let compact = p.lint().expect("lint");
        assert!(
            compact.iter().all(|f| f.code != "touch_target"),
            "compact's touch-min is 28, so 30 passes"
        );
    }

    #[test]
    fn preview_lists_the_main_roles_and_follows_edits() {
        let mut p = child();
        let before = p.preview();
        assert_eq!(before.len(), PREVIEW_ROLES.len());
        p.set_color("color.accent", [0xe8, 0x79, 0x3a, 255]).expect("set");
        let primary = p
            .preview()
            .into_iter()
            .find(|(l, ..)| *l == "Primary")
            .expect("Primary");
        assert_eq!(
            primary.2,
            [0xe8, 0x79, 0x3a, 255],
            "the swatch background is the accent"
        );
    }

    #[test]
    fn save_round_trip_is_unchanged() {
        let dir = regia_test_utils::unique_temp_dir("theme-panel-save");
        let path = dir.join("my-game.theme.ron");
        let mut p = child();
        p.set_color("color.accent", [0xe8, 0x79, 0x3a, 255]).expect("set");
        p.set_token("radius.md", "3").expect("set");
        assert!(p.dirty());
        p.save_to(&path).expect("save");
        assert!(!p.dirty());
        let first = std::fs::read_to_string(&path).expect("read");

        let mut q = ThemePanel::new();
        q.open_path(&path).expect("reopen");
        assert!(!q.dirty(), "a freshly loaded file is clean");
        assert_eq!(q.draft(), p.draft(), "the file loads back to the same draft");
        q.save_to(&path).expect("save again");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            first,
            "saving twice is byte-identical"
        );
        assert!(
            !path.with_extension("ron.tmp").exists(),
            "no temp file left behind"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every theme the engine ships is a built-in name here, so none can be overwritten by a draft
    /// (`regia-light-high-contrast`, ADR-1309, was missing; ADR-5664).
    #[test]
    fn every_shipped_theme_is_a_refused_built_in_name() {
        let shipped: Vec<String> = builtin_theme_set().names().map(str::to_owned).collect();
        assert_eq!(shipped.len(), BUILTIN_THEME_SOURCES.len(), "{shipped:?}");
        for name in &shipped {
            assert!(is_builtin_name(name), "{name} is not refused");
            let dir = regia_test_utils::unique_temp_dir("theme-panel-shipped");
            let path = dir.join("copy.theme.ron");
            let mut p = open(name);
            assert_eq!(p.save_to(&path).expect_err(name).code, "builtin_name", "{name}");
            assert!(!path.exists(), "{name}");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn saving_a_built_in_name_is_refused_until_renamed() {
        let dir = regia_test_utils::unique_temp_dir("theme-panel-builtin");
        let path = dir.join("copy.theme.ron");
        let mut p = open("regia-dark");
        assert!(!p.dirty(), "an opened built-in starts clean");
        assert_eq!(p.save_to(&path).expect_err("built-in name").code, "builtin_name");
        assert!(!path.exists());
        p.rename("copy").expect("rename");
        p.save_to(&path).expect("save the renamed copy");
        let mut q = ThemePanel::new();
        q.open_path(&path).expect("loads");
        assert_eq!(q.draft().map(|d| d.name.as_str()), Some("copy"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_built_in_name_is_refused_on_save() {
        let dir = regia_test_utils::unique_temp_dir("theme-panel-every-builtin");
        let path = dir.join("copy.theme.ron");
        for (name, _) in BUILTIN_THEME_SOURCES {
            let mut p = open(name);
            assert_eq!(
                p.save_to(&path).expect_err("built-in name").code,
                "builtin_name",
                "{name}"
            );
            assert!(!path.exists(), "{name} was written");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rename_and_new_child_reject_taken_or_bad_names() {
        let mut p = ThemePanel::new();
        assert_eq!(
            p.new_child("regia-dark", "regia-dark").expect_err("taken").code,
            "name_taken"
        );
        assert_eq!(
            p.new_child("bad name!", "regia-dark").expect_err("bad").code,
            "invalid_name"
        );
        assert_eq!(
            p.new_child("ok", "missing").expect_err("parent").code,
            "unknown_theme"
        );
        p.new_child("ok", "regia-light").expect("fine");
        assert_eq!(p.rename("regia-dark").expect_err("taken").code, "name_taken");
    }

    #[test]
    fn open_path_reports_io_and_parse_errors() {
        let dir = regia_test_utils::unique_temp_dir("theme-panel-errors");
        let mut p = ThemePanel::new();
        assert_eq!(
            p.open_path(&dir.join("missing.ron")).expect_err("missing").code,
            "io_error"
        );
        let bad = dir.join("bad.theme.ron");
        std::fs::write(
            &bad,
            "(format_version: 1, name: \"x\", tokens: {\"color.accent\": \"@nope\"})",
        )
        .expect("write");
        assert_eq!(p.open_path(&bad).expect_err("dangling").code, "parse_error");
        assert!(p.draft().is_none(), "a failed open keeps the previous state");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn group_mapping() {
        assert_eq!(group_of("color.accent"), "Color");
        assert_eq!(group_of("space.3"), "Spacing");
        assert_eq!(group_of("radius.md"), "Shape");
        assert_eq!(group_of("border.width.focus"), "Shape");
        assert_eq!(group_of("font.body"), "Type");
        assert_eq!(group_of("ease.standard"), "Motion");
        assert_eq!(group_of("size.touch-min"), "Size");
        assert_eq!(group_of("brand.glow"), "Other");
    }
}

/// A selected widget type (or variation) and state in the style tree and gallery.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StyleSelection {
    /// Type or variation name, such as `Button.danger`.
    #[serde(rename = "type")]
    pub type_name: String,
    /// `normal`, `hovered`, `pressed`, `focused`, `disabled`, `checked`, `error`, or `sizes.<name>`.
    pub state: String,
}

impl Default for StyleSelection {
    fn default() -> Self {
        Self {
            type_name: "Button".into(),
            state: "normal".into(),
        }
    }
}

/// State layers supported by the current frozen theme format.
pub const STYLE_STATES: [&str; 7] = [
    "normal", "hovered", "pressed", "focused", "disabled", "checked", "error",
];

/// An import selector. Omitted state and kind select all states and all property kinds.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportItem {
    /// Type/variation name; `tokens` selects global tokens and `fonts` font definitions.
    #[serde(rename = "type")]
    pub type_name: String,
    /// One state layer, or every layer when absent.
    #[serde(default)]
    pub state: Option<String>,
    /// `all`, `colors`, `sizes`, `fonts`, `borders`, `paddings`, or `other`.
    #[serde(default = "all_kind")]
    pub kind: String,
}

fn all_kind() -> String {
    "all".into()
}

fn layer_key(state: &str) -> Result<&str, PanelError> {
    match state {
        "normal" | "props" => Ok("props"),
        "hovered" | "hover" => Ok("hover"),
        "checked" | "selected" => Ok("selected"),
        "pressed" | "focused" | "disabled" | "error" => Ok(state),
        s if s.starts_with("sizes.") && s.len() > 6 => Ok(s),
        _ => Err(PanelError::new(
            "invalid_state",
            format!("unknown state `{state}`"),
        )),
    }
}

fn layer<'a>(def: &'a TypeDef, state: &str) -> Result<Option<&'a PropMap>, PanelError> {
    Ok(match layer_key(state)? {
        "props" => Some(&def.props),
        "hover" => Some(&def.hover),
        "pressed" => Some(&def.pressed),
        "focused" => Some(&def.focused),
        "disabled" => Some(&def.disabled),
        "selected" => Some(&def.selected),
        "error" => Some(&def.error),
        size => def.sizes.get(&size[6..]),
    })
}

fn layer_mut<'a>(def: &'a mut TypeDef, state: &str) -> Result<&'a mut PropMap, PanelError> {
    Ok(match layer_key(state)? {
        "props" => &mut def.props,
        "hover" => &mut def.hover,
        "pressed" => &mut def.pressed,
        "focused" => &mut def.focused,
        "disabled" => &mut def.disabled,
        "selected" => &mut def.selected,
        "error" => &mut def.error,
        size => def.sizes.entry(size[6..].into()).or_default(),
    })
}

fn definition<'a>(theme: &'a ResolvedTheme, name: &str) -> Option<&'a TypeDef> {
    theme.variations.get(name).or_else(|| theme.types.get(name))
}

fn property_kind(name: &str, value: &PropValue, theme: &ResolvedTheme) -> &'static str {
    if matches!(name, "border_width" | "focus_ring_width" | "focus_ring_offset") {
        return "borders";
    }
    if name.starts_with("padding") {
        return "paddings";
    }
    if matches!(name, "font" | "font_family" | "text" | "font_size" | "text_px") {
        return "fonts";
    }
    let resolved = materialize(value, theme).ok();
    if matches!(&resolved, Some(PropValue::Text(s)) if regia_ui::theme_tokens::parse_hex_color(s).is_some())
        || matches!(name, "background" | "text_color" | "border_color" | "icon_color")
    {
        "colors"
    } else if matches!(name, "border" | "focus_ring") {
        "borders"
    } else if matches!(resolved, Some(PropValue::Number(_)))
        || matches!(
            name,
            "height"
                | "min_width"
                | "max_width"
                | "min_height"
                | "max_height"
                | "radius"
                | "gap"
                | "translate"
        )
    {
        "sizes"
    } else {
        "other"
    }
}

fn materialize(value: &PropValue, theme: &ResolvedTheme) -> Result<PropValue, PanelError> {
    match value {
        PropValue::List(values) => values
            .iter()
            .map(|v| materialize(v, theme))
            .collect::<Result<Vec<_>, _>>()
            .map(PropValue::List),
        PropValue::Text(text)
            if text.trim().starts_with('@')
                || text.trim().starts_with('#')
                || (text.contains('(') && text.trim().ends_with(')')) =>
        {
            let expr =
                regia_ui::theme_tokens::Expr::parse(text).map_err(|e| PanelError::new("invalid_token", e))?;
            let value = theme
                .table
                .resolve_expr("import", &expr)
                .map_err(|e| PanelError::new("invalid_token", format!("{e:?}")))?;
            Ok(match value {
                Value::Number(n) => PropValue::Number(n),
                Value::Duration(s) => PropValue::Text(format!("{s}s")),
                v => PropValue::Text(describe(&v)),
            })
        }
        _ => Ok(value.clone()),
    }
}

impl ThemePanel {
    /// An isolated runtime theme collection containing the edited draft.
    pub fn preview_themes(&self) -> regia_ui::UiThemes {
        regia_ui::UiThemes {
            set: self.set.clone(),
            ..Default::default()
        }
    }

    /// The style currently selected by the tree or gallery.
    pub fn selection(&self) -> &StyleSelection {
        &self.selection
    }

    /// Selects a type/state without changing the saved document.
    pub fn select_style(&mut self, selection: StyleSelection) -> Result<(), PanelError> {
        layer_key(&selection.state)?;
        if definition(&self.resolved()?, &selection.type_name).is_none() {
            return Err(PanelError::new(
                "unknown_type",
                format!("unknown type `{}`", selection.type_name),
            ));
        }
        self.selection = selection;
        Ok(())
    }

    /// Applies runtime token/type/variation patches as one validated edit.
    /// `rename` changes the draft's name in the same transaction.
    pub fn edit(&mut self, patch: &serde_json::Value, rename: Option<&str>) -> Result<(), PanelError> {
        let _span = tracing::info_span!("theme_panel_edit").entered();
        let current = self.require_draft()?;
        let mut scratch = World::new();
        scratch.insert_resource(self.preview_themes());
        let mut params = serde_json::json!({"theme": current.name});
        for field in ["tokens", "types", "variations"] {
            if let Some(value) = patch.get(field) {
                params[field] = value.clone();
            }
        }
        regia_ui::theme_agent::call(&mut scratch, "ui.theme_set", &params)
            .map_err(|e| PanelError::new("invalid_token", e.message))?;
        let mut file = scratch
            .resource::<regia_ui::UiThemes>()
            .set
            .get(&current.name)
            .cloned()
            .ok_or_else(|| PanelError::new("no_draft", "draft disappeared"))?;
        let resolved = scratch
            .resource::<regia_ui::UiThemes>()
            .set
            .resolve(&current.name, Some(self.density))
            .map_err(|e| PanelError::new("invalid_token", e.to_string()))?;
        for group in ["types", "variations"] {
            if let Some(defs) = patch.get(group).and_then(serde_json::Value::as_object) {
                for (name, fields) in defs {
                    let Some(fields) = fields.as_object() else {
                        continue;
                    };
                    let Some(def) = (if group == "types" {
                        file.types.get(name)
                    } else {
                        file.variations.get(name)
                    }) else {
                        continue;
                    };
                    for (state, edits) in fields {
                        if state == "extends" {
                            continue;
                        }
                        if state == "sizes" {
                            if let Some(sizes) = edits.as_object() {
                                for (size, edits) in sizes {
                                    validate_property_patch(def, &format!("sizes.{size}"), edits, &resolved)?;
                                }
                            }
                        } else {
                            validate_property_patch(def, state, edits, &resolved)?;
                        }
                    }
                }
            }
        }
        if let Some(name) = rename {
            if name != current.name && self.set.get(name).is_some() {
                return Err(PanelError::new(
                    "name_taken",
                    format!("a theme named `{name}` exists"),
                ));
            }
            file.name = name.into();
        }
        self.commit(file)
    }

    /// Undo or redo a whole draft operation, including a selective import.
    pub fn history(&mut self, redo: bool) -> Result<(), PanelError> {
        if self.shared_history.is_shared() {
            self.require_draft()?;
            // The editor's shared history (P-322): the step runs at the next flush, or ran
            // already for an agent call (`shared_panel_history::before_request`).
            return if self.shared_history.navigate(redo) {
                self.error = None;
                Ok(())
            } else {
                Err(PanelError::new("empty_history", "no operation to restore"))
            };
        }
        let current = (self.set.clone(), self.require_draft()?.clone());
        let stack = if redo { &mut self.redo } else { &mut self.undo };
        let Some((set, draft)) = stack.pop() else {
            return Err(PanelError::new("empty_history", "no operation to restore"));
        };
        if redo {
            self.undo.push(current);
        } else {
            self.redo.push(current);
        }
        self.set = set;
        self.draft = Some(draft);
        self.reconcile_selection();
        self.error = None;
        self.revision += 1;
        Ok(())
    }

    /// Root-first variation chain, ending with the selected type/variation.
    pub fn style_chain(&self, name: &str) -> Result<Vec<String>, PanelError> {
        let theme = self.resolved()?;
        let mut chain = Vec::new();
        let mut next = name.to_owned();
        while let Some(def) = definition(&theme, &next) {
            if chain.contains(&next) {
                return Err(PanelError::new("invalid_token", "variation cycle"));
            }
            chain.push(next.clone());
            if !theme.variations.contains_key(&next) {
                break;
            }
            next = def.extends.clone().unwrap_or_else(|| {
                next.rsplit_once('.')
                    .map_or(next.as_str(), |(base, _)| base)
                    .to_owned()
            });
        }
        chain.reverse();
        Ok(chain)
    }

    /// Effective property expressions of a type/variation in one state, including its bases.
    pub fn style_properties(&self, selection: &StyleSelection) -> Result<PropMap, PanelError> {
        let theme = self.resolved()?;
        effective_properties(&theme, &selection.type_name, &selection.state)
    }

    /// Copies the selected state's effective style box into every supported state as one edit.
    pub fn apply_all_states(&mut self, selection: &StyleSelection) -> Result<(), PanelError> {
        if definition(&self.resolved()?, &selection.type_name).is_none() {
            return Err(PanelError::new(
                "unknown_type",
                format!("unknown type `{}`", selection.type_name),
            ));
        }
        let props = self.style_properties(selection)?;
        let mut file = self.draft_clone()?;
        let variation = self.resolved()?.variations.contains_key(&selection.type_name);
        let defs = if variation {
            &mut file.variations
        } else {
            &mut file.types
        };
        let def = defs.entry(selection.type_name.clone()).or_default();
        for state in STYLE_STATES {
            layer_mut(def, state)?.extend(props.clone());
        }
        self.commit(file)
    }

    /// Imports selected keys from a validated source theme, committing exactly one undo step.
    /// Source expressions are resolved so no unrelated source token is imported.
    pub fn import_items(
        &mut self,
        source: &ThemeFile,
        items: &[ImportItem],
        with_values: bool,
    ) -> Result<Vec<String>, PanelError> {
        if items.is_empty() {
            return Err(PanelError::new("invalid_import", "select at least one item"));
        }
        let mut sources = self.set.clone();
        sources
            .insert(source.clone())
            .map_err(|e| PanelError::new("invalid_import", e.to_string()))?;
        let source = sources
            .resolve(&source.name, Some(self.density))
            .map_err(|e| PanelError::new("invalid_import", e.to_string()))?;
        let target = self.resolved()?;
        let builtin = builtin_theme_set()
            .resolve("regia-dark", Some(self.density))
            .map_err(|e| PanelError::new("invalid_import", e.to_string()))?;
        let mut file = self.draft_clone()?;
        let mut copied = std::collections::BTreeSet::new();
        for item in items {
            if !["all", "colors", "sizes", "fonts", "borders", "paddings", "other"]
                .contains(&item.kind.as_str())
            {
                return Err(PanelError::new(
                    "invalid_import",
                    format!("unknown kind `{}`", item.kind),
                ));
            }
            if item.type_name == "tokens" {
                if item.state.is_some() {
                    return Err(PanelError::new("invalid_import", "tokens have no state"));
                }
                for (key, value) in &source.values {
                    let kind = match value {
                        Value::Color(_) => "colors",
                        Value::Number(_) => "sizes",
                        Value::Text(_) => "fonts",
                        _ => "other",
                    };
                    if item.kind != "all" && item.kind != kind {
                        continue;
                    }
                    let value = if with_values {
                        value
                    } else {
                        target
                            .values
                            .get(key)
                            .or_else(|| builtin.values.get(key))
                            .ok_or_else(|| {
                                PanelError::new(
                                    "invalid_import",
                                    format!("no destination/default value for {key}"),
                                )
                            })?
                    };
                    file.tokens.insert(
                        key.clone(),
                        match value {
                            Value::Number(n) => TokenValue::Number(*n),
                            Value::Duration(s) => TokenValue::Text(format!("{s}s")),
                            v => TokenValue::Text(describe(v)),
                        },
                    );
                    copied.insert(format!("tokens.{key}"));
                }
                continue;
            }
            if item.type_name == "fonts" {
                if item.state.is_some() || !["all", "fonts"].contains(&item.kind.as_str()) {
                    return Err(PanelError::new(
                        "invalid_import",
                        "font definitions accept only kind fonts/all and no state",
                    ));
                }
                for (name, font) in &source.fonts {
                    let value = if with_values {
                        font
                    } else {
                        target
                            .fonts
                            .get(name)
                            .or_else(|| builtin.fonts.get(name))
                            .ok_or_else(|| {
                                PanelError::new(
                                    "invalid_import",
                                    format!("no destination/default font {name}"),
                                )
                            })?
                    };
                    file.fonts.insert(name.clone(), value.clone());
                    copied.insert(format!("fonts.{name}"));
                }
                continue;
            }
            let def = definition(&source, &item.type_name).ok_or_else(|| {
                PanelError::new(
                    "unknown_type",
                    format!("unknown source type `{}`", item.type_name),
                )
            })?;
            let states: Vec<String> = match &item.state {
                Some(s) => vec![s.clone()],
                None => STYLE_STATES
                    .iter()
                    .map(|s| (*s).into())
                    .chain(def.sizes.keys().map(|s| format!("sizes.{s}")))
                    .collect(),
            };
            for state in states {
                let Some(props) = layer(def, &state)? else {
                    continue;
                };
                for (key, value) in props {
                    if item.kind != "all" && property_kind(key, value, &source) != item.kind {
                        continue;
                    }
                    let value = if with_values {
                        materialize(value, &source)?
                    } else {
                        let dest = effective_properties(&target, &item.type_name, &state)?;
                        let defaults = effective_properties(&builtin, &item.type_name, &state)?;
                        dest.get(key)
                            .or_else(|| defaults.get(key))
                            .cloned()
                            .ok_or_else(|| {
                                PanelError::new(
                                    "invalid_import",
                                    format!(
                                        "no destination/default value for {}.{state}.{key}",
                                        item.type_name
                                    ),
                                )
                            })?
                    };
                    let variation = source.variations.contains_key(&item.type_name);
                    let defs = if variation {
                        &mut file.variations
                    } else {
                        &mut file.types
                    };
                    layer_mut(defs.entry(item.type_name.clone()).or_default(), &state)?
                        .insert(key.clone(), value);
                    copied.insert(format!(
                        "{}.{}.{}.{key}",
                        if variation { "variations" } else { "types" },
                        item.type_name,
                        layer_key(&state)?
                    ));
                }
            }
        }
        if copied.is_empty() {
            return Err(PanelError::new("invalid_import", "no matching items"));
        }
        self.commit(file)?;
        Ok(copied.into_iter().collect())
    }

    /// Reads a built-in/loaded theme by name or a theme file without changing the draft.
    pub fn import_source(&self, from: &str) -> Result<ThemeFile, PanelError> {
        if let Some(file) = self.set.get(from) {
            return Ok(file.clone());
        }
        let text = std::fs::read_to_string(from).map_err(|e| PanelError::new("io_error", e.to_string()))?;
        parse_theme_ron(&text, from).map_err(|e| PanelError::new("parse_error", e.to_string()))
    }
}

fn effective_properties(theme: &ResolvedTheme, name: &str, state: &str) -> Result<PropMap, PanelError> {
    let mut chain = Vec::new();
    let mut next = name.to_owned();
    while let Some(def) = definition(theme, &next) {
        if chain.contains(&next) {
            return Err(PanelError::new("invalid_token", "variation cycle"));
        }
        chain.push(next.clone());
        if !theme.variations.contains_key(&next) {
            break;
        }
        next = def.extends.clone().unwrap_or_else(|| {
            next.rsplit_once('.')
                .map_or(next.as_str(), |(base, _)| base)
                .to_owned()
        });
    }
    let mut props = BTreeMap::new();
    for name in chain.iter().rev() {
        if let Some(def) = definition(theme, name) {
            props.extend(def.props.clone());
            if let Some(layer) = layer(def, state)? {
                props.extend(layer.clone());
            }
        }
    }
    Ok(props)
}

/// Adapts the editor's existing UI-role bridge back to a runtime theme import source.
/// Only unambiguous color roles and the button state's colors are mapped.
pub fn editor_import_source(tokens: &crate::theme::ThemeTokens) -> ThemeFile {
    let mut file = ThemeFile {
        name: "__editor_import".into(),
        ..Default::default()
    };
    for (key, color) in [
        ("color.surface-overlay", tokens.ground),
        ("color.surface", tokens.panel),
        ("color.surface-sunken", tokens.well),
        ("color.border", tokens.line),
        ("color.text-muted", tokens.muted),
        ("color.text", tokens.text),
        ("color.accent", tokens.signal),
        ("color.on-accent", tokens.on_signal),
        ("color.warning", tokens.warn),
        ("color.danger", tokens.error),
    ] {
        file.tokens.insert(
            key.into(),
            TokenValue::Text(to_hex(color.to_srgba_unmultiplied())),
        );
    }
    let colors = |background: egui::Color32, text: egui::Color32| {
        BTreeMap::from([
            (
                "background".into(),
                PropValue::Text(to_hex(background.to_srgba_unmultiplied())),
            ),
            (
                "text_color".into(),
                PropValue::Text(to_hex(text.to_srgba_unmultiplied())),
            ),
        ])
    };
    file.types.insert(
        "Button".into(),
        TypeDef {
            props: colors(tokens.widgets.idle, tokens.widgets.button_text),
            hover: colors(tokens.widgets.hover, tokens.widgets.hover_text),
            pressed: colors(tokens.widgets.pressed, tokens.strong),
            ..Default::default()
        },
    );
    file
}

fn named_text(ui: &mut egui::Ui, name: &str, text: &mut String) -> egui::Response {
    ui.label(name);
    let response = ui.add(egui::TextEdit::singleline(text).desired_width(160.0));
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, ui.is_enabled(), name));
    response
}

fn show_result(panel: &mut ThemePanel, result: Result<(), PanelError>) {
    if let Err(error) = result {
        panel.error = Some(error.to_string());
    }
}

fn set_property(panel: &mut ThemePanel, name: &str, value: serde_json::Value) -> Result<(), PanelError> {
    if name.trim().is_empty() {
        return Err(PanelError::new("invalid_token", "property name is empty"));
    }
    let group = if panel
        .resolved()?
        .variations
        .contains_key(&panel.selection.type_name)
    {
        "variations"
    } else {
        "types"
    };
    let state = layer_key(&panel.selection.state)?;
    let mut patch = serde_json::json!({});
    if let Some(size) = state.strip_prefix("sizes.") {
        patch[group][&panel.selection.type_name]["sizes"][size][name] = value;
    } else {
        patch[group][&panel.selection.type_name][state][name] = value;
    }
    panel.edit(&patch, None)
}

/// A checkbox whose click counts only when `apply` (the first UI pass of the frame): returns the
/// value to keep.
fn gated_checkbox(ui: &mut egui::Ui, value: bool, label: &str, apply: bool) -> bool {
    let mut shown = value;
    ui.checkbox(&mut shown, label);
    keep_toggle(value, shown, apply)
}

/// The value a toggle keeps: the widget's new value on the first pass of a frame, else the old one.
fn keep_toggle(before: bool, after: bool, apply: bool) -> bool {
    if apply { after } else { before }
}

fn draw_styles(ui: &mut egui::Ui, panel: &mut ThemePanel, editor_source: &ThemeFile, apply: bool) {
    use crate::agent_parity::{self as actions, action_button};
    let _span = tracing::info_span!("theme_panel_styles").entered();
    ui.separator();
    ui.heading("Widget styles");
    ui.horizontal_wrapped(|ui| {
        if action_button(ui, &actions::THEME_UNDO).clicked() && apply {
            let result = panel.history(false);
            show_result(panel, result);
        }
        if action_button(ui, &actions::THEME_REDO).clicked() && apply {
            let result = panel.history(true);
            show_result(panel, result);
        }
    });
    let Ok(theme) = panel.resolved() else {
        return;
    };
    let mut names: Vec<_> = theme
        .types
        .keys()
        .chain(theme.variations.keys())
        .cloned()
        .collect();
    names.sort();
    names.dedup();
    egui::ScrollArea::vertical()
        .id_salt("theme_style_tree")
        .max_height(180.0)
        .show(ui, |ui| {
            for name in &names {
                egui::CollapsingHeader::new(name)
                    .id_salt(("theme_type", name))
                    .show(ui, |ui| {
                        let mut states: Vec<String> = STYLE_STATES.iter().map(|s| (*s).into()).collect();
                        if let Some(def) = definition(&theme, name) {
                            states.extend(def.sizes.keys().map(|s| format!("sizes.{s}")));
                        }
                        for state in states {
                            let selection = StyleSelection {
                                type_name: name.clone(),
                                state: state.clone(),
                            };
                            let response = ui.selectable_label(panel.selection == selection, &state);
                            if response.clicked() && apply {
                                panel.selection = selection;
                            }
                        }
                    });
            }
        });
    ui.horizontal_wrapped(|ui| {
        named_text(ui, "Type name", &mut panel.type_name);
        if action_button(ui, &actions::THEME_TYPE).clicked() && apply {
            let name = panel.type_name.trim().to_owned();
            let result = if name.is_empty()
                || name.split('.').any(str::is_empty)
                || !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
            {
                Err(PanelError::new(
                    "invalid_name",
                    "a type name uses letters, digits, underscores, hyphens and nonempty dot-separated parts",
                ))
            } else if definition(&theme, &name).is_some() {
                Err(PanelError::new(
                    "name_taken",
                    "the theme already defines this type or variation",
                ))
            } else {
                panel.edit(&serde_json::json!({"types":{name.clone():{"props":{}}}}), None)
            };
            if result.is_ok() {
                panel.selection = StyleSelection {
                    type_name: name,
                    state: "normal".into(),
                };
            }
            show_result(panel, result);
        }
    });
    ui.horizontal_wrapped(|ui| {
        named_text(ui, "Variation name", &mut panel.variation_name);
        named_text(ui, "Base type", &mut panel.variation_base);
        let response = action_button(ui, &actions::THEME_VARIATION);
        #[cfg(test)]
        ui.ctx()
            .data_mut(|d| d.insert_temp(egui::Id::new("theme_variation_button"), response.id));
        if response.clicked() && apply {
            let name = panel.variation_name.trim().to_owned();
            let base = panel.variation_base.trim().to_owned();
            let result = if !name.contains('.')
                || name.contains(char::is_whitespace)
                || definition(&theme, &base).is_none()
            {
                Err(PanelError::new(
                    "invalid_name",
                    "use Type.variation and an existing base type",
                ))
            } else if panel.draft().is_some_and(|d| d.variations.contains_key(&name)) {
                Err(PanelError::new(
                    "name_taken",
                    "the draft already defines this variation",
                ))
            } else {
                panel.edit(
                    &serde_json::json!({"variations":{name.clone():{"extends":base}}}),
                    None,
                )
            };
            if result.is_ok() {
                panel.selection = StyleSelection {
                    type_name: name,
                    state: "normal".into(),
                };
            }
            show_result(panel, result);
        }
    });
    ui.label(format!(
        "{} / {}",
        panel.selection.type_name, panel.selection.state
    ));
    if let Ok(chain) = panel.style_chain(&panel.selection.type_name) {
        ui.weak(chain.join(" → "));
    }
    ui.horizontal_wrapped(|ui| {
        if action_button(ui, &actions::THEME_PIN).clicked() && apply {
            panel.pin = Some(panel.selection.clone());
        }
        if let Some(pin) = panel.pin.clone() {
            ui.label(format!("Pinned: {} / {}", pin.type_name, pin.state));
            if action_button(ui, &actions::THEME_ALL_STATES).clicked() && apply {
                let result = panel.apply_all_states(&pin);
                show_result(panel, result);
            }
        }
    });
    let props = panel.style_properties(&panel.selection).unwrap_or_default();
    let own = panel
        .draft()
        .and_then(|d| {
            d.variations
                .get(&panel.selection.type_name)
                .or_else(|| d.types.get(&panel.selection.type_name))
        })
        .and_then(|d| layer(d, &panel.selection.state).ok().flatten())
        .cloned()
        .unwrap_or_default();
    egui::ScrollArea::vertical()
        .id_salt("theme_properties")
        .max_height(240.0)
        .show(ui, |ui| {
            for (key, value) in props {
                ui.push_id(
                    (
                        &panel.selection.type_name.clone(),
                        &panel.selection.state.clone(),
                        &key,
                    ),
                    |ui| {
                        ui.horizontal_wrapped(|ui| {
                            if own.contains_key(&key) {
                                ui.label(
                                    egui::RichText::new(&key).color(ui.visuals().selection.stroke.color),
                                );
                            } else {
                                ui.label(&key);
                            }
                            ui.weak(property_kind(&key, &value, &theme));
                            let mut edit = None;
                            if let PropValue::Number(mut n) = value {
                                let response = ui.add(egui::DragValue::new(&mut n).speed(0.25));
                                response.widget_info(|| {
                                    egui::WidgetInfo::labeled(
                                        egui::WidgetType::DragValue,
                                        ui.is_enabled(),
                                        &key,
                                    )
                                });
                                if response.changed() {
                                    edit = Some(serde_json::json!(n));
                                }
                            } else {
                                let initial = match &value {
                                    PropValue::Text(s) => s.clone(),
                                    v => serde_json::to_string(v).unwrap_or_default(),
                                };
                                let id = ui.id().with("property_buffer");
                                let mut text = ui
                                    .ctx()
                                    .data_mut(|d| d.get_temp::<String>(id))
                                    .unwrap_or_else(|| initial.clone());
                                let response =
                                    ui.add(egui::TextEdit::singleline(&mut text).desired_width(200.0));
                                response.widget_info(|| {
                                    egui::WidgetInfo::labeled(
                                        egui::WidgetType::TextEdit,
                                        ui.is_enabled(),
                                        &key,
                                    )
                                });
                                if (response.lost_focus()
                                    || (response.has_focus()
                                        && ui.input(|i| i.key_pressed(egui::Key::Enter))))
                                    && text != initial
                                {
                                    if matches!(value, PropValue::List(_)) {
                                        match serde_json::from_str(&text) {
                                            Ok(value) => edit = Some(value),
                                            Err(error) => {
                                                panel.error = Some(format!("invalid_token: {error}"))
                                            }
                                        }
                                    } else {
                                        edit = Some(
                                            text.parse::<f32>().ok().filter(|n| n.is_finite()).map_or_else(
                                                || serde_json::json!(text),
                                                |n| serde_json::json!(n),
                                            ),
                                        );
                                    }
                                    ui.ctx().data_mut(|d| d.remove::<String>(id));
                                } else if response.has_focus() {
                                    ui.ctx().data_mut(|d| d.insert_temp(id, text));
                                }
                                if let Ok(PropValue::Text(color)) = materialize(&value, &theme)
                                    && let Some(color) = regia_ui::theme_tokens::parse_hex_color(&color)
                                {
                                    let mut rgba = to_srgb8(color);
                                    let response = ui.color_edit_button_srgba_unmultiplied(&mut rgba);
                                    response.widget_info(|| {
                                        egui::WidgetInfo::labeled(
                                            egui::WidgetType::ColorButton,
                                            ui.is_enabled(),
                                            format!("{key} color"),
                                        )
                                    });
                                    if response.changed() {
                                        edit = Some(serde_json::json!(to_hex(rgba)));
                                    }
                                }
                            }
                            if let Some(value) = edit
                                && apply
                            {
                                let result = set_property(panel, &key, value);
                                show_result(panel, result);
                            }
                            if own.contains_key(&key)
                                && action_button(ui, &actions::THEME_RESET).clicked()
                                && apply
                            {
                                let result = set_property(panel, &key, serde_json::Value::Null);
                                show_result(panel, result);
                            }
                        });
                    },
                );
            }
        });
    ui.horizontal_wrapped(|ui| {
        named_text(ui, "Property", &mut panel.property_name);
        named_text(ui, "Value (number, text or list)", &mut panel.property_value);
        let response = action_button(ui, &actions::THEME_PROPERTY);
        #[cfg(test)]
        ui.ctx()
            .data_mut(|d| d.insert_temp(egui::Id::new("theme_property_button"), response.id));
        if response.clicked() && apply {
            let value = serde_json::from_str(&panel.property_value)
                .unwrap_or_else(|_| serde_json::json!(panel.property_value));
            let key = panel.property_name.clone();
            let result = set_property(panel, &key, value);
            show_result(panel, result);
        }
    });
    ui.separator();
    ui.label("Import items (empty type uses the selection; tokens and fonts select global items)");
    ui.horizontal_wrapped(|ui| {
        named_text(ui, "Import type", &mut panel.import_type);
        named_text(ui, "Import state (all or a state name)", &mut panel.import_state);
    });
    ui.horizontal_wrapped(|ui| {
        named_text(ui, "Source theme or path", &mut panel.import_from);
        egui::ComboBox::from_id_salt("theme_import_kind")
            .selected_text(&panel.import_kind)
            .show_ui(ui, |ui| {
                for kind in ["all", "colors", "sizes", "fonts", "borders", "paddings", "other"] {
                    ui.selectable_value(&mut panel.import_kind, kind.into(), kind);
                }
            });
        // The checkbox writes on every pass; only the first pass of the frame keeps the new value,
        // so a repeated pass does not flip it back (review sprint-w12-rev-g9 #3).
        panel.import_values = gated_checkbox(ui, panel.import_values, "Copy values", apply);
        if action_button(ui, &actions::THEME_IMPORT).clicked() && apply {
            let source = if panel.import_from == "editor" {
                Ok(editor_source.clone())
            } else {
                panel.import_source(&panel.import_from)
            };
            let item = ImportItem {
                type_name: if panel.import_type.trim().is_empty() {
                    panel.selection.type_name.clone()
                } else {
                    panel.import_type.trim().into()
                },
                state: if panel.import_state.trim() == "all"
                    || ["tokens", "fonts"].contains(&panel.import_type.trim())
                {
                    None
                } else {
                    Some(panel.import_state.trim().into())
                },
                kind: panel.import_kind.clone(),
            };
            let result =
                source.and_then(|s| panel.import_items(&s, &[item], panel.import_values).map(|_| ()));
            show_result(panel, result);
        }
    });
}

#[cfg(test)]
mod parity_tests {
    use super::*;
    use serde_json::json;

    fn child() -> ThemePanel {
        let mut panel = ThemePanel::default();
        panel.new_child("parity", "regia-dark").expect("child");
        panel
    }

    #[test]
    fn variation_runtime_resolution_save_reopen_and_atomic_history() {
        let mut panel = ThemePanel::default();
        panel.open("regia-dark").expect("standalone template");
        panel.rename("parity").expect("name");
        // The built-in already has a danger variation. Remove it from this standalone draft
        // so the new definition proves inheritance of every other value from its chosen base.
        panel
            .edit(&json!({"variations":{"Button.danger":null}}), None)
            .expect("remove template variation");
        let before = panel.to_ron().expect("before");
        panel.edit(&json!({"variations":{"Button.danger":{"extends":"Button","props":{"background":"#ca1234"}}}}),None).expect("variation");
        let themes = panel.preview_themes();
        let active = regia_ui::ActiveTheme::named("parity");
        let look = regia_ui::theme_agent::resolve_widget_look(
            &themes,
            &active,
            "Button.danger",
            &Default::default(),
        )
        .expect("resolve");
        let base =
            regia_ui::theme_agent::resolve_widget_look(&themes, &active, "Button", &Default::default())
                .expect("base");
        assert_eq!(look.background, Some(UiColor::from_srgb8(0xca, 0x12, 0x34, 255)));
        assert_eq!(look.radius, base.radius, "unoverridden base radius survives");
        let mut expected = base;
        let overridden = UiColor::from_srgb8(0xca, 0x12, 0x34, 255);
        expected.background = Some(overridden);
        // Reduced-motion resolution also derives the settled animation values from background.
        expected.anim_background = overridden;
        expected.anim_target = overridden;
        expected.anim_to[0] = Some(overridden);
        assert_eq!(look, expected, "every unoverridden base value survives");
        let after = panel.to_ron().expect("after");
        panel.history(false).expect("undo");
        assert_eq!(panel.to_ron().expect("undo text"), before);
        panel.history(true).expect("redo");
        assert_eq!(panel.to_ron().expect("redo text"), after);
        let dir = regia_test_utils::unique_temp_dir("theme-panel-variation");
        let path = dir.join("parity.theme.ron");
        panel.save_to(&path).expect("save");
        let first = std::fs::read(&path).expect("read");
        let mut reopened = ThemePanel::default();
        reopened.open_path(&path).expect("reopen");
        reopened.save_to(&path).expect("resave");
        assert_eq!(first, std::fs::read(&path).expect("read again"));
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    #[test]
    fn selective_import_resolves_source_expressions_without_leaking_keys() {
        let mut panel = child();
        let source = ThemeFile {
            name: "source".into(),
            extends: Some("regia-light".into()),
            tokens: BTreeMap::from([("brand.accent".into(), TokenValue::Text("#ca1234".into()))]),
            types: BTreeMap::from([(
                "Button".into(),
                TypeDef {
                    hover: BTreeMap::from([
                        ("background".into(), PropValue::Text("@brand.accent".into())),
                        ("radius".into(), PropValue::Number(25.0)),
                    ]),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };
        let item = ImportItem {
            type_name: "Button".into(),
            state: Some("hovered".into()),
            kind: "colors".into(),
        };
        let copied = panel
            .import_items(&source, std::slice::from_ref(&item), true)
            .expect("import");
        assert_eq!(
            copied,
            ["types.Button.hover.background", "types.Button.hover.border_color"]
        );
        let draft = panel.draft().expect("draft");
        assert!(draft.tokens.is_empty());
        assert_eq!(draft.types.len(), 1);
        assert_eq!(draft.types["Button"].hover.len(), 2);
        assert_eq!(
            draft.types["Button"].hover["background"],
            PropValue::Text("#ca1234".into())
        );
        assert!(draft.types["Button"].props.is_empty());
        let imported = panel.to_ron().expect("text");
        let rev = panel.revision();
        let invalid = ImportItem {
            type_name: "Missing".into(),
            ..item.clone()
        };
        assert!(
            panel
                .import_items(&source, &[item.clone(), invalid], true)
                .is_err()
        );
        assert_eq!(panel.to_ron().expect("text"), imported);
        assert_eq!(panel.revision(), rev);
        panel.history(false).expect("undo one import");
        let destination = panel
            .style_properties(&StyleSelection {
                type_name: "Button".into(),
                state: "hovered".into(),
            })
            .expect("properties");
        panel.import_items(&source, &[item], false).expect("names only");
        assert_eq!(
            panel.draft().expect("draft").types["Button"].hover["background"],
            destination["background"]
        );
    }

    #[test]
    fn reset_and_apply_all_states_are_single_roundtripping_edits() {
        let mut panel = child();
        panel
            .edit(&json!({"types":{"Button":{"hover":{"radius":19}}}}), None)
            .expect("edit");
        let before = panel.to_ron().expect("before");
        panel
            .apply_all_states(&StyleSelection {
                type_name: "Button".into(),
                state: "hovered".into(),
            })
            .expect("copy");
        for state in STYLE_STATES {
            assert_eq!(
                layer(&panel.draft().expect("draft").types["Button"], state)
                    .expect("state")
                    .expect("map")["radius"],
                PropValue::Number(19.0)
            );
        }
        panel.history(false).expect("undo copy");
        assert_eq!(panel.to_ron().expect("text"), before);
        panel
            .edit(&json!({"types":{"Button":{"hover":{"radius":null}}}}), None)
            .expect("reset");
        assert!(
            !panel.draft().expect("draft").types["Button"]
                .hover
                .contains_key("radius")
        );
        assert_eq!(
            parse_theme_ron(&panel.to_ron().expect("text"), "test").expect("parse"),
            panel.draft().expect("draft").clone()
        );
    }

    #[test]
    fn color_selectors_include_color_only_rings_but_not_compound_borders() {
        let theme = child().resolved().expect("theme");
        assert_eq!(
            property_kind("focus_ring", &PropValue::Text("@color.accent".into()), &theme),
            "colors"
        );
        assert_eq!(
            property_kind(
                "border",
                &PropValue::List(vec![
                    PropValue::Number(2.0),
                    PropValue::Text("@color.accent".into())
                ]),
                &theme
            ),
            "borders"
        );
        assert_eq!(
            property_kind("border_width", &PropValue::Number(2.0), &theme),
            "borders"
        );
    }

    #[test]
    fn editor_import_contains_only_mapped_editor_values() {
        let mut panel = child();
        let source = editor_import_source(&crate::theme::ThemeTokens::default());
        let colors = ImportItem {
            type_name: "tokens".into(),
            state: None,
            kind: "colors".into(),
        };
        let copied = panel
            .import_items(&source, &[colors], true)
            .expect("mapped colors");
        assert_eq!(copied.len(), 10);
        assert_eq!(panel.draft().expect("draft").tokens.len(), 10);
        assert!(
            !panel.draft().expect("draft").tokens.contains_key("color.bg"),
            "unmapped built-in values must not masquerade as editor values"
        );
        let before = panel.to_ron().expect("before");
        let fonts = ImportItem {
            type_name: "fonts".into(),
            state: None,
            kind: "fonts".into(),
        };
        assert_eq!(
            panel
                .import_items(&source, &[fonts], true)
                .expect_err("unmapped fonts")
                .code,
            "invalid_import"
        );
        assert_eq!(panel.to_ron().expect("after"), before);
    }

    #[test]
    fn wrong_property_types_and_cycles_leave_the_draft_and_history_unchanged() {
        let mut panel = child();
        let before = panel.to_ron().expect("before");
        let revision = panel.revision();
        for patch in [
            json!({"types":{"Button":{"props":{"background":12}}}}),
            json!({"types":{"Button":{"props":{"radius":"#ffffff"}}}}),
            json!({"variations":{"Button.loop":{"extends":"Button.loop"}}}),
        ] {
            assert!(panel.edit(&patch, None).is_err());
            assert_eq!(panel.to_ron().expect("text"), before);
            assert_eq!(panel.revision(), revision);
            assert!(panel.undo.is_empty());
        }
    }

    #[test]
    fn keyboard_enter_creates_a_variation_in_the_actual_panel() {
        let mut panel = child();
        panel.variation_name = "Button.danger".into();
        panel.variation_base = "Button".into();
        let ctx = egui::Context::default();
        let source = editor_import_source(&crate::theme::ThemeTokens::default());
        let frame = |panel: &mut ThemePanel, events: Vec<egui::Event>| {
            let _ = crate::canvas_keys::test_input::frame(&ctx, [1800.0, 1400.0], events, |ui| {
                draw_styles(ui, panel, &source, true);
            });
        };
        frame(&mut panel, vec![]);
        let id = ctx
            .data(|d| d.get_temp::<egui::Id>(egui::Id::new("theme_variation_button")))
            .expect("button drawn");
        ctx.memory_mut(|m| m.request_focus(id));
        frame(
            &mut panel,
            vec![egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        assert_eq!(
            panel.draft().expect("draft").variations["Button.danger"]
                .extends
                .as_deref(),
            Some("Button")
        );
        assert_eq!(panel.selection().type_name, "Button.danger");
        panel.property_name = "background".into();
        panel.property_value = "#ca1234".into();
        frame(
            &mut panel,
            vec![egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: false,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        let property = ctx
            .data(|d| d.get_temp::<egui::Id>(egui::Id::new("theme_property_button")))
            .expect("property button");
        ctx.memory_mut(|m| m.request_focus(property));
        frame(
            &mut panel,
            vec![egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        let look = regia_ui::theme_agent::resolve_widget_look(
            &panel.preview_themes(),
            &regia_ui::ActiveTheme::named("parity"),
            "Button.danger",
            &Default::default(),
        )
        .expect("runtime look");
        assert_eq!(look.background, Some(UiColor::from_srgb8(0xca, 0x12, 0x34, 255)));
    }
}

fn validate_property_patch(
    def: &TypeDef,
    state: &str,
    edits: &serde_json::Value,
    theme: &ResolvedTheme,
) -> Result<(), PanelError> {
    let Some(edits) = edits.as_object() else {
        return Ok(());
    };
    let Some(props) = layer(def, state)? else {
        return Ok(());
    };
    for key in edits.keys() {
        let Some(value) = props.get(key) else {
            continue;
        };
        let value = materialize(value, theme)?;
        let color = |v: &PropValue| matches!(v, PropValue::Text(s) if regia_ui::theme_tokens::parse_hex_color(s).is_some());
        let number = |v: &PropValue| matches!(v, PropValue::Number(n) if n.is_finite());
        let valid = match key.as_str() {
            "background" | "text_color" | "border_color" | "icon_color" => color(&value),
            "border_width" | "focus_ring_width" | "focus_ring_offset" | "height" | "min_width"
            | "max_width" | "min_height" | "max_height" | "padding_x" | "gap" | "font_size" | "text_px"
            | "opacity" | "scale" | "elevation" => number(&value),
            "radius" | "padding" | "translate" => {
                number(&value)
                    || matches!(&value, PropValue::List(values) if !values.is_empty() && values.iter().all(number))
            }
            "border" | "focus_ring" => {
                color(&value)
                    || number(&value)
                    || matches!(&value, PropValue::List(values) if !values.is_empty() && values.iter().all(|v| color(v) || number(v)))
            }
            _ => true,
        };
        if !valid {
            return Err(PanelError::new(
                "invalid_token",
                format!("{state}.{key}: value does not match the property's data kind"),
            ));
        }
    }
    Ok(())
}
