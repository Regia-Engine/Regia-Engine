// SPDX-License-Identifier: MPL-2.0
//! Keymap persistence and editing (ADR-2321, completing ADR-2045's session-only rebinding): the user
//! shortcuts of [`CommandKeymap`] saved per user, conflict detection, import and export.
//!
//! The file is `<user config>/regia/editor_keymap.ron` (and any exported copy):
//!
//! ```ron
//! (
//!     format_version: 1,
//!     bindings: { "view.toggle_grid": "Ctrl+G", "view.frame_selection": "Alt+F" },
//! )
//! ```
//!
//! Chords use [`KeyChord::label`]'s spelling (`Ctrl` is Cmd on macOS). Reading keeps bindings of
//! command ids this build does not know (they are written back, so a newer editor's bindings
//! survive), drops chords that do not parse or are reserved, and when two commands claim one chord
//! keeps the first in key order and reports the other ([`KeymapProblem`]).
//!
//! [`conflicts`] lists the user chords that shadow a built-in shortcut of another command (the user
//! shortcut wins while it is bound) or a fixed editor key. Import merges into or replaces the
//! current keymap ([`ImportMode`]); a dry run reports without changing anything.
//!
//! The agent surface is `editor.keymap_get`, `editor.keymap_bind`, `editor.keymap_import`,
//! `editor.keymap_export` and `editor.keymap_reset` (`book/agents/editor-preferences.md`).
//!
//! Scoped shortcuts (ADR-7767): a user binding has a [`ShortcutScope`]. The dispatcher
//! ([`dispatch_order`]) tries the bindings of the focused panel's scope first, then the global ones;
//! while a text field has the keyboard only text-scope bindings run. The same chord may be bound in
//! two different non-global scopes; [`conflicts`] reports a clash only inside one scope or against a
//! global binding. The safety keys ([`SAFETY_KEYS`]: Escape stops play, Ctrl+S saves, Ctrl+Z and
//! Ctrl+Y / Ctrl+Shift+Z undo and redo) are global: binding one in a scope is refused
//! (`shortcut_safety_key`). The file keeps the version 1 shape: a scoped binding's key is
//! `"<scope>:<command id>"` (`"viewport:view.toggle_grid"`); an older editor keeps such a key as an
//! unknown command and writes it back unchanged.
//!
//! Presets (ADR-7767): named keymaps in `<user config>/regia/keymaps/<name>.editor_keymap.ron`
//! (the same version 1 shape), plus the built-in `Default` (no user shortcuts) and `Laptop`
//! ([`LAPTOP_PRESET`]). Applying one replaces the current keymap at once, without a restart.
//!
//! [`lookup`] lists every command a chord runs, in any scope, built-in or user ("Press a key…" in
//! the Keymap tab, `editor.keymap_lookup`); [`key_states`] colors the drawn keyboard map.
//!
//! Extension shortcuts: extensions register commands, not chords; when the extension merge (job
//! 3262) adds extension commands, their default chord and scope go through [`CommandKeymap::bind_in`]
//! so the same scope and safety rules apply. This is the registration point.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regia_ecs::prelude::*;
use serde::{Deserialize, Serialize};

use crate::command_palette::{BindError, COMMANDS, CommandKeymap, HOLD_COMMANDS, KeyChord, find};
use crate::editor_prefs::EditorConfigPaths;

/// Current version of `editor_keymap.ron`.
pub const EDITOR_KEYMAP_FORMAT_VERSION: u32 = 1;

/// Editor keys that are not palette commands but are always handled: Ctrl+Y redoes beside
/// Ctrl+Shift+Z and Ctrl+Shift+P opens the palette. User shortcuts run before the panels draw, so a
/// user chord equal to one of these takes it away; [`conflicts`] lists that. The viewport's fly keys
/// and the outliner's keys are commands with built-in shortcuts since ADR-2696, so binding one moves
/// it and [`conflicts`] lists them as any other built-in shortcut.
pub const FIXED_EDITOR_KEYS: &[(&str, &str)] = &[
    ("Ctrl+Y", "edit.redo (second key)"),
    ("Ctrl+Shift+P", "the command palette"),
];

/// Version 1 of the file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct KeymapFileV1 {
    #[serde(default = "crate::editor_prefs::version_missing")]
    format_version: u32,
    #[serde(default)]
    bindings: BTreeMap<String, String>,
}

/// Why a keymap file was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeymapError {
    /// Not a keymap file.
    Parse(String),
    /// Written by a newer editor.
    Newer {
        /// The file's version.
        found: u32,
    },
    /// Reading or writing failed.
    Io(String),
}

impl KeymapError {
    /// The agent error code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Parse(_) => "keymap_parse",
            Self::Newer { .. } => "keymap_newer",
            Self::Io(_) => "keymap_io",
        }
    }
}

impl std::fmt::Display for KeymapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse(m) | Self::Io(m) => f.write_str(m),
            Self::Newer { found } => write!(
                f,
                "written by a newer editor (format_version {found}, this editor reads up to {EDITOR_KEYMAP_FORMAT_VERSION}); left untouched"
            ),
        }
    }
}

impl std::error::Error for KeymapError {}

/// One entry a read dropped or set aside.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeymapProblem {
    /// No command has the id (the binding is kept for write-back, not applied).
    UnknownCommand {
        /// The id.
        id: String,
    },
    /// The chord does not parse.
    BadChord {
        /// The command.
        id: String,
        /// The text.
        keys: String,
    },
    /// The chord is Ctrl+Shift+P.
    Reserved {
        /// The command.
        id: String,
    },
    /// A safety key bound in a non-global scope (refused: safety keys are global).
    SafetyKey {
        /// The command.
        id: String,
        /// The chord.
        keys: String,
    },
    /// Two commands claim one chord; the first (in id order) keeps it.
    Duplicate {
        /// The chord.
        keys: String,
        /// The command that kept it.
        kept: String,
        /// The command that lost it.
        dropped: String,
    },
}

impl KeymapProblem {
    /// One line for people and agents.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::UnknownCommand { id } => format!("{id}: no such command in this editor; kept in the file"),
            Self::BadChord { id, keys } => format!("{id}: {keys:?} is not a key chord; dropped"),
            Self::Reserved { id } => format!("{id}: Ctrl+Shift+P opens the palette; dropped"),
            Self::SafetyKey { id, keys } => {
                format!("{id}: {keys} is a safety key and stays global; dropped")
            }
            Self::Duplicate { keys, kept, dropped } => {
                format!("{keys}: bound to both {kept} and {dropped}; {kept} keeps it")
            }
        }
    }

    /// The problem's wire kind.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::UnknownCommand { .. } => "unknown_command",
            Self::BadChord { .. } => "bad_chord",
            Self::Reserved { .. } => "reserved",
            Self::SafetyKey { .. } => "safety_key",
            Self::Duplicate { .. } => "duplicate",
        }
    }
}

/// A read keymap file.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct KeymapRead {
    /// The bindings that apply.
    pub keymap: CommandKeymap,
    /// Bindings of unknown command ids, kept for write-back.
    pub unknown: BTreeMap<String, String>,
    /// What was dropped or set aside.
    pub problems: Vec<KeymapProblem>,
}

/// The `from` versions of the registered `editor_keymap.ron` migration steps (none: version 1 is
/// the first).
#[must_use]
pub fn keymap_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The serde shape of `editor_keymap.ron` (ADR-7902: the `editor-keymap` format).
#[must_use]
pub fn keymap_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[regia_core::schema::serde_field_names::<KeymapFileV1>()])
}

/// Reads keymap text of any supported version.
///
/// # Errors
///
/// [`KeymapError::Parse`] or [`KeymapError::Newer`].
pub fn parse_keymap(text: &str) -> Result<KeymapRead, KeymapError> {
    let file = crate::editor_prefs::read_versioned(text, EDITOR_KEYMAP_FORMAT_VERSION, |f: &KeymapFileV1| {
        f.format_version
    })
    .map_err(|e| match e {
        crate::editor_prefs::VersionedError::Parse(m) => KeymapError::Parse(m),
        crate::editor_prefs::VersionedError::Newer(found) => KeymapError::Newer { found },
    })?;
    let mut read = KeymapRead::default();
    for (key, keys) in file.bindings {
        let Some((scope, id)) = split_scoped_key(&key) else {
            read.problems
                .push(KeymapProblem::UnknownCommand { id: key.clone() });
            read.unknown.insert(key, keys);
            continue;
        };
        let Some(def) = find(id) else {
            read.problems
                .push(KeymapProblem::UnknownCommand { id: key.clone() });
            read.unknown.insert(key, keys);
            continue;
        };
        let Some(chord) = KeyChord::parse(&keys) else {
            read.problems.push(KeymapProblem::BadChord { id: key, keys });
            continue;
        };
        if read.keymap.bindings.contains_key(def.id) {
            // One binding per command: the second scope's entry is kept for write-back, not applied.
            read.problems.push(KeymapProblem::Duplicate {
                keys: chord.label(),
                kept: def.id.to_owned(),
                dropped: key.clone(),
            });
            read.unknown.insert(key, keys);
            continue;
        }
        if let Some((kept, _)) = read
            .keymap
            .bindings
            .iter()
            .find(|(other, c)| **c == chord && read.keymap.scope_of(other) == scope)
        {
            read.problems.push(KeymapProblem::Duplicate {
                keys: chord.label(),
                kept: (*kept).to_owned(),
                dropped: key,
            });
            continue;
        }
        match read.keymap.bind_in(def.id, chord, scope) {
            Ok(_) => {}
            Err(BindError::Reserved) => read.problems.push(KeymapProblem::Reserved { id: key }),
            Err(BindError::UnknownCommand) => read.problems.push(KeymapProblem::UnknownCommand { id: key }),
            Err(BindError::SafetyKey) => read.problems.push(KeymapProblem::SafetyKey {
                id: key,
                keys: chord.label(),
            }),
        }
    }
    Ok(read)
}

/// Splits a file key into its scope and command id: `"viewport:view.toggle_grid"` is the viewport
/// scope, a key without a `:` is global. `None` for an unknown scope name.
#[must_use]
pub fn split_scoped_key(key: &str) -> Option<(ShortcutScope, &str)> {
    match key.split_once(':') {
        None => Some((ShortcutScope::Global, key)),
        Some((scope, id)) => {
            let scope = ShortcutScope::parse(scope)?;
            (scope != ShortcutScope::Global).then_some((scope, id))
        }
    }
}

/// The file key of a binding of `id` in `scope` (the inverse of [`split_scoped_key`]).
#[must_use]
pub fn scoped_key(scope: ShortcutScope, id: &str) -> String {
    if scope == ShortcutScope::Global {
        id.to_owned()
    } else {
        format!("{}:{id}", scope.name())
    }
}

/// The file text of `keymap` (plus kept unknown bindings), current version, ids sorted.
#[must_use]
pub fn keymap_to_text(keymap: &CommandKeymap, unknown: &BTreeMap<String, String>) -> String {
    let mut bindings = unknown.clone();
    for (id, chord) in &keymap.bindings {
        bindings.insert(scoped_key(keymap.scope_of(id), id), chord.label());
    }
    let file = KeymapFileV1 {
        format_version: EDITOR_KEYMAP_FORMAT_VERSION,
        bindings,
    };
    ron::ser::to_string_pretty(&file, regia_core::ron_pretty())
        .unwrap_or_else(|_| format!("(format_version: {EDITOR_KEYMAP_FORMAT_VERSION}, bindings: {{}})"))
}

/// A user chord that collides with something else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyConflict {
    /// The chord.
    pub keys: String,
    /// The command the user bound it to.
    pub command: &'static str,
    /// What else uses it: another command's built-in shortcut, a global user binding, or a fixed
    /// editor key.
    pub shadows: String,
    /// The scope of the user binding.
    pub scope: ShortcutScope,
}

/// Whether bindings in scopes `a` and `b` can collide: in the same scope, or when either is global.
#[must_use]
pub fn scopes_clash(a: ShortcutScope, b: ShortcutScope) -> bool {
    a == b || a == ShortcutScope::Global || b == ShortcutScope::Global
}

/// Every user chord that shadows another command's built-in shortcut, a global user binding (for a
/// scoped one) or a fixed editor key. The user binding wins while it exists (a scoped one while its
/// panel has focus); the list tells the user what they lose. Bindings in two different non-global
/// scopes never conflict.
#[must_use]
pub fn conflicts(keymap: &CommandKeymap) -> Vec<KeyConflict> {
    let mut out = Vec::new();
    for (&id, chord) in &keymap.bindings {
        let scope = keymap.scope_of(id);
        for def in COMMANDS.iter() {
            if def.id == id || def.shortcut.is_empty() {
                continue;
            }
            // A command's built-in shortcut is shadowed only while the user has not moved it.
            if keymap.bindings.contains_key(def.id) {
                continue;
            }
            if KeyChord::parse(def.shortcut) == Some(*chord) && scopes_clash(scope, builtin_scope(def.id)) {
                out.push(KeyConflict {
                    keys: chord.label(),
                    command: id,
                    shadows: format!("{} (built-in shortcut of {})", def.title, def.id),
                    scope,
                });
            }
        }
        // A scoped binding shadows a global user binding of the same chord while its panel has focus.
        if scope != ShortcutScope::Global {
            for (&other, other_chord) in &keymap.bindings {
                if other != id && other_chord == chord && keymap.scope_of(other) == ShortcutScope::Global {
                    out.push(KeyConflict {
                        keys: chord.label(),
                        command: id,
                        shadows: format!("{other} (global) while the {} has focus", scope.label()),
                        scope,
                    });
                }
            }
        }
        for (keys, what) in FIXED_EDITOR_KEYS {
            if KeyChord::parse(keys) == Some(*chord) {
                out.push(KeyConflict {
                    keys: chord.label(),
                    command: id,
                    shadows: (*what).to_owned(),
                    scope,
                });
            }
        }
    }
    out
}

/// How an import combines with the current keymap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportMode {
    /// Imported bindings are added; a chord moves to the imported command.
    Merge,
    /// The imported keymap replaces the current one.
    Replace,
}

impl ImportMode {
    /// Parses `merge` or `replace`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "merge" => Some(Self::Merge),
            "replace" => Some(Self::Replace),
            _ => None,
        }
    }
}

/// What an import did (or would do, for a dry run).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ImportReport {
    /// The keymap after the import.
    pub keymap: CommandKeymap,
    /// Bindings the import set, by command id.
    pub applied: Vec<(&'static str, String)>,
    /// Commands that lost their chord to an imported binding.
    pub moved_from: Vec<&'static str>,
    /// What the file reader dropped or set aside.
    pub problems: Vec<KeymapProblem>,
    /// Conflicts in the resulting keymap.
    pub conflicts: Vec<KeyConflict>,
}

/// Combines `current` with the keymap in `text`.
///
/// # Errors
///
/// The text is not a keymap file this editor reads.
pub fn import_keymap(
    current: &CommandKeymap,
    text: &str,
    mode: ImportMode,
) -> Result<ImportReport, KeymapError> {
    let read = parse_keymap(text)?;
    let mut keymap = match mode {
        ImportMode::Merge => current.clone(),
        ImportMode::Replace => CommandKeymap::default(),
    };
    keymap.recording = None;
    let mut report = ImportReport {
        problems: read.problems,
        ..Default::default()
    };
    for (&id, &chord) in &read.keymap.bindings {
        if let Ok(Some(from)) = keymap.bind_in(id, chord, read.keymap.scope_of(id)) {
            report.moved_from.push(from);
        }
        report.applied.push((id, chord.label()));
    }
    report.conflicts = conflicts(&keymap);
    report.keymap = keymap;
    Ok(report)
}

/// The persistence state of the keymap file.
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct KeymapStore {
    /// Whether the file was read this session.
    pub loaded: bool,
    /// The bindings last written (or read).
    pub saved: BTreeMap<&'static str, KeyChord>,
    /// The scopes last written (or read), with [`Self::saved`].
    pub saved_scopes: BTreeMap<&'static str, ShortcutScope>,
    /// The preset applied last (`None` before any), shown by the Keymap tab.
    pub active_preset: Option<String>,
    /// Bindings of command ids this build does not know, written back unchanged.
    pub unknown: BTreeMap<String, String>,
    /// Set when the file must not be written (a newer editor wrote it); the reason.
    pub read_only: Option<String>,
    /// What the last read reported.
    pub problems: Vec<String>,
    /// The last write error, if any.
    pub last_error: Option<String>,
}

/// Reads the keymap file named by [`EditorConfigPaths`] into [`CommandKeymap`] (once per session).
/// Shortcuts bound in this session before the read win over the file's and are saved.
pub fn load_keymap(world: &mut World) {
    if world.get_resource::<KeymapStore>().is_some_and(|s| s.loaded) {
        return;
    }
    let path = world
        .get_resource::<EditorConfigPaths>()
        .and_then(EditorConfigPaths::keymap_file);
    let mut store = KeymapStore {
        loaded: true,
        ..Default::default()
    };
    let session = world.get_resource::<CommandKeymap>().cloned().unwrap_or_default();
    let mut keymap = session.clone();
    if let Some(path) = path {
        match read_keymap_file(&path) {
            Ok(Some(read)) => {
                keymap = read.keymap;
                keymap.recording = session.recording;
                store.unknown = read.unknown;
                store.problems = read.problems.iter().map(KeymapProblem::message).collect();
            }
            Ok(None) => {}
            Err(err) => {
                tracing::warn!(target: "regia_editor", "{}: {err}", path.display());
                if matches!(err, KeymapError::Newer { .. }) {
                    store.read_only = Some(err.to_string());
                } else {
                    // A file that does not parse or cannot be read is never replaced: the next
                    // shortcut edit would lose every binding in it (review sprint-w12-rev-g1 #8).
                    store.problems.push(format!(
                        "{}: {err}; using no user shortcuts, and the file is left untouched until it is fixed",
                        path.display()
                    ));
                    store.read_only = Some(format!("{}: {err}; left untouched", path.display()));
                }
            }
        }
    }
    // Saved = the file's bindings, so session bindings layered on top below are written.
    store.saved = keymap.bindings.clone();
    store.saved_scopes = keymap.scopes.clone();
    for (&id, &chord) in &session.bindings {
        let _ = keymap.bind_in(id, chord, session.scope_of(id));
    }
    world.insert_resource(keymap);
    world.insert_resource(store);
}

/// Reads a keymap file; `Ok(None)` when it does not exist.
///
/// # Errors
///
/// The file cannot be read, does not parse, or is from a newer editor.
pub fn read_keymap_file(path: &Path) -> Result<Option<KeymapRead>, KeymapError> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse_keymap(&text).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(KeymapError::Io(format!("cannot read {}: {e}", path.display()))),
    }
}

/// Writes the keymap when its bindings changed since the last write (and the file is writable).
/// Returns whether it wrote.
pub fn persist_keymap(world: &mut World) -> bool {
    let Some(path) = world
        .get_resource::<EditorConfigPaths>()
        .and_then(EditorConfigPaths::keymap_file)
    else {
        return false;
    };
    let Some(keymap) = world.get_resource::<CommandKeymap>() else {
        return false;
    };
    let Some(store) = world.get_resource::<KeymapStore>() else {
        return false;
    };
    if (store.saved == keymap.bindings && store.saved_scopes == keymap.scopes) || store.read_only.is_some() {
        return false;
    }
    let text = keymap_to_text(keymap, &store.unknown);
    let bindings = keymap.bindings.clone();
    let scopes = keymap.scopes.clone();
    let result = crate::level_io::write_atomic(&path, text.as_bytes());
    let mut store = store.clone();
    // A failed write is not retried every frame: the next change tries again.
    store.saved = bindings;
    store.saved_scopes = scopes;
    store.last_error = result.as_ref().err().cloned();
    if let Err(err) = &result {
        tracing::warn!(target: "regia_editor", "keymap not saved: {err}");
    }
    world.insert_resource(store);
    result.is_ok()
}

// ---------------------------------------------------------------------------------------------
// Scopes (ADR-7767)

/// Where a user shortcut applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum ShortcutScope {
    /// Everywhere (the default, and every binding written before scopes existed).
    #[default]
    Global,
    /// The 3D viewport has focus.
    Viewport,
    /// The Outliner has focus.
    Outliner,
    /// The Inspector has focus.
    Inspector,
    /// The Content Browser has focus.
    Content,
    /// A graph canvas has focus (material, animation, procedural, behavior, state tree,
    /// conversation, VFX).
    Graph,
    /// A timeline has focus (sequence curves, shots, clips, montages, UI animation).
    Timeline,
    /// A text field has the keyboard (only text-scope bindings run while typing).
    Text,
}

impl ShortcutScope {
    /// Every scope, in the Keymap tab's order.
    pub const ALL: [Self; 8] = [
        Self::Global,
        Self::Viewport,
        Self::Outliner,
        Self::Inspector,
        Self::Content,
        Self::Graph,
        Self::Timeline,
        Self::Text,
    ];

    /// The wire and file name (`global`, `viewport`, `outliner`, `inspector`, `content`, `graph`,
    /// `timeline`, `text`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Viewport => "viewport",
            Self::Outliner => "outliner",
            Self::Inspector => "inspector",
            Self::Content => "content",
            Self::Graph => "graph",
            Self::Timeline => "timeline",
            Self::Text => "text",
        }
    }

    /// The name shown in the Keymap tab.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Global => "Global",
            Self::Viewport => "Viewport",
            Self::Outliner => "Outliner",
            Self::Inspector => "Inspector",
            Self::Content => "Content Browser",
            Self::Graph => "Graph canvas",
            Self::Timeline => "Timeline",
            Self::Text => "Text field",
        }
    }

    /// Parses [`Self::name`].
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.name() == name)
    }

    /// The scope a focused panel gives its shortcuts (global for panels without their own).
    #[must_use]
    pub fn of_panel(panel: crate::shell::Panel) -> Self {
        use crate::shell::Panel as P;
        match panel {
            P::Viewport => Self::Viewport,
            P::Outliner => Self::Outliner,
            P::Inspector => Self::Inspector,
            P::Content => Self::Content,
            P::MaterialGraph
            | P::AnimGraph
            | P::ProcGraph
            | P::BehaviorTree
            | P::StateTreeEditor
            | P::Conversation
            | P::VfxEditor => Self::Graph,
            P::SeqCurves | P::Shots | P::AnimClip | P::Montage | P::UiAnim => Self::Timeline,
            _ => Self::Global,
        }
    }
}

/// The safety keys: always global, never bound in a scope (`shortcut_safety_key`), so no focused
/// panel can take stop-play, save, undo or redo away.
pub const SAFETY_KEYS: &[(&str, &str)] = &[
    ("Escape", "stop play"),
    ("Ctrl+S", "save"),
    ("Ctrl+Z", "undo"),
    ("Ctrl+Shift+Z", "redo"),
    ("Ctrl+Y", "redo (second key)"),
];

/// Whether `chord` is one of [`SAFETY_KEYS`].
#[must_use]
pub fn is_safety_key(chord: KeyChord) -> bool {
    SAFETY_KEYS
        .iter()
        .any(|(keys, _)| KeyChord::parse(keys) == Some(chord))
}

/// The scope of a command's built-in shortcut: the viewport's keys (camera, gizmo, framing,
/// placement, collider handles) and the outliner's keys act only in their panel.
#[must_use]
pub fn builtin_scope(id: &str) -> ShortcutScope {
    if id.starts_with("camera.")
        || id.starts_with("gizmo.")
        || matches!(
            id,
            "view.frame_selection" | "selection.drop_to_floor" | "collider.toggle_handles"
        )
    {
        ShortcutScope::Viewport
    } else if id.starts_with("outliner.") {
        ShortcutScope::Outliner
    } else {
        ShortcutScope::Global
    }
}

/// Commands whose built-in shortcut the palette's dispatcher runs itself (the other built-in
/// shortcuts are read by their panel). F1 (`help.context`) works in every scope, typing included.
pub const DISPATCHED_BUILTINS: &[&str] = &["help.context"];

/// The panel whose tab had keyboard focus in the last drawn frame; the shell writes it, the
/// dispatcher reads it to pick the scope.
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FocusedPanel(pub Option<crate::shell::Panel>);

/// The scope the dispatcher uses now: text while a text field has the keyboard, else the focused
/// panel's.
#[must_use]
pub fn active_scope(world: &World, text_focused: bool) -> ShortcutScope {
    if text_focused {
        return ShortcutScope::Text;
    }
    world
        .get_resource::<FocusedPanel>()
        .and_then(|f| f.0)
        .map_or(ShortcutScope::Global, ShortcutScope::of_panel)
}

fn modifier_count(chord: KeyChord) -> u8 {
    u8::from(chord.command) + u8::from(chord.shift) + u8::from(chord.alt)
}

/// The bindings the dispatcher tries, in order, while `active` is the scope: a chord with more
/// modifiers first (so Ctrl+Shift+K is not taken by Ctrl+K, nor global Shift+X by a scoped X), and
/// among equal ones the user bindings of `active` (unless it is global), then the global user
/// bindings (not while typing), then the [`DISPATCHED_BUILTINS`] the user did not move. Held
/// commands are never dispatched.
#[must_use]
pub fn dispatch_order(keymap: &CommandKeymap, active: ShortcutScope) -> Vec<(&'static str, KeyChord)> {
    let mut scoped = Vec::new();
    let mut global = Vec::new();
    for (&id, &chord) in &keymap.bindings {
        if HOLD_COMMANDS.contains(&id) {
            continue;
        }
        let scope = keymap.scope_of(id);
        if scope == ShortcutScope::Global {
            if active != ShortcutScope::Text {
                global.push((id, chord));
            }
        } else if scope == active {
            scoped.push((id, chord));
        }
    }
    for id in DISPATCHED_BUILTINS {
        if keymap.bindings.contains_key(id) {
            continue;
        }
        if let Some(def) = find(id)
            && let Some(chord) = KeyChord::parse(def.shortcut)
        {
            global.push((def.id, chord));
        }
    }
    // egui matches a chord without Shift or Alt also when they are held, so the chords with more
    // modifiers go first across both groups; on a tie the focused scope's binding wins (stable sort).
    scoped.extend(global);
    scoped.sort_by_key(|(_, c)| std::cmp::Reverse(modifier_count(*c)));
    scoped
}

/// The command a press of `chord` runs while `active` is the scope ([`dispatch_order`]'s first
/// exact match).
#[must_use]
pub fn resolve(keymap: &CommandKeymap, active: ShortcutScope, chord: KeyChord) -> Option<&'static str> {
    dispatch_order(keymap, active)
        .into_iter()
        .find(|(_, c)| *c == chord)
        .map(|(id, _)| id)
}

/// A key chord the Keymap tab's "Press a key…" search is waiting for. The palette's key handler
/// takes the next press (before any shortcut runs) and stores it in `captured`; Escape cancels.
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChordCapture {
    /// Whether the next press is captured.
    pub active: bool,
    /// The chord captured last.
    pub captured: Option<KeyChord>,
    /// The egui frame the Keymap tab last drew the pending capture in; a capture whose tab is no
    /// longer drawn (another tab, the window closed) is dropped instead of eating a later key.
    pub armed_frame: u64,
}

// ---------------------------------------------------------------------------------------------
// Lookup and the keyboard map

/// One command a chord runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChordUse {
    /// The command id.
    pub id: &'static str,
    /// The command title.
    pub title: &'static str,
    /// Where the chord runs it.
    pub scope: ShortcutScope,
    /// Whether it is a user binding (else the built-in shortcut).
    pub user: bool,
}

/// What a chord does.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChordLookup {
    /// The chord's label.
    pub keys: String,
    /// Every command it runs, in any scope.
    pub uses: Vec<ChordUse>,
    /// Fixed editor keys and safety keys on the chord.
    pub fixed: Vec<&'static str>,
    /// Whether two of its uses collide (same scope, or one global).
    pub conflict: bool,
}

/// Every command `chord` runs in any scope (user bindings, and built-in shortcuts the user did not
/// move), plus the fixed and safety keys on it.
#[must_use]
pub fn lookup(keymap: &CommandKeymap, chord: KeyChord) -> ChordLookup {
    let mut out = ChordLookup {
        keys: chord.label(),
        ..Default::default()
    };
    for def in COMMANDS.iter() {
        let (effective, user) = match keymap.bindings.get(def.id) {
            Some(c) => (Some(*c), true),
            None => (KeyChord::parse(def.shortcut), false),
        };
        if effective == Some(chord) {
            out.uses.push(ChordUse {
                id: def.id,
                title: def.title,
                scope: if user {
                    keymap.scope_of(def.id)
                } else {
                    builtin_scope(def.id)
                },
                user,
            });
        }
    }
    for (keys, what) in FIXED_EDITOR_KEYS.iter().chain(SAFETY_KEYS) {
        if KeyChord::parse(keys) == Some(chord) && !out.fixed.contains(what) {
            out.fixed.push(what);
        }
    }
    let uses = &out.uses;
    // Two built-in shortcuts never conflict: their panel arbitrates (W moves the gizmo, held with
    // the right mouse button it flies). A user binding collides with any use in a clashing scope.
    out.conflict = uses.iter().enumerate().any(|(i, a)| {
        uses[i + 1..]
            .iter()
            .any(|b| (a.user || b.user) && scopes_clash(a.scope, b.scope))
    }) || (!out.fixed.is_empty() && uses.iter().any(|u| u.user));
    out
}

/// A key's color in the keyboard map.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyState {
    /// No command uses the key with the chosen modifiers.
    Unbound,
    /// One or more commands use it, none colliding.
    Bound,
    /// Two uses collide ([`ChordLookup::conflict`]).
    Conflict,
}

/// The keyboard map's rows (a 60 % layout plus the function, navigation and arrow keys).
pub const KEYBOARD_ROWS: &[&[egui::Key]] = {
    use egui::Key as K;
    &[
        &[
            K::Escape,
            K::F1,
            K::F2,
            K::F3,
            K::F4,
            K::F5,
            K::F6,
            K::F7,
            K::F8,
            K::F9,
            K::F10,
            K::F11,
            K::F12,
        ],
        &[
            K::Backtick,
            K::Num1,
            K::Num2,
            K::Num3,
            K::Num4,
            K::Num5,
            K::Num6,
            K::Num7,
            K::Num8,
            K::Num9,
            K::Num0,
            K::Minus,
            K::Equals,
            K::Backspace,
        ],
        &[
            K::Tab,
            K::Q,
            K::W,
            K::E,
            K::R,
            K::T,
            K::Y,
            K::U,
            K::I,
            K::O,
            K::P,
            K::OpenBracket,
            K::CloseBracket,
            K::Backslash,
        ],
        &[
            K::A,
            K::S,
            K::D,
            K::F,
            K::G,
            K::H,
            K::J,
            K::K,
            K::L,
            K::Semicolon,
            K::Quote,
            K::Enter,
        ],
        &[
            K::Z,
            K::X,
            K::C,
            K::V,
            K::B,
            K::N,
            K::M,
            K::Comma,
            K::Period,
            K::Slash,
        ],
        &[
            K::Space,
            K::Insert,
            K::Delete,
            K::Home,
            K::End,
            K::PageUp,
            K::PageDown,
            K::ArrowLeft,
            K::ArrowUp,
            K::ArrowDown,
            K::ArrowRight,
        ],
    ]
};

/// The state of every key of [`KEYBOARD_ROWS`] with the modifiers `command`, `shift`, `alt` held.
#[must_use]
pub fn key_states(
    keymap: &CommandKeymap,
    command: bool,
    shift: bool,
    alt: bool,
) -> Vec<(egui::Key, KeyState)> {
    KEYBOARD_ROWS
        .iter()
        .flat_map(|row| row.iter())
        .map(|&key| {
            let found = lookup(
                keymap,
                KeyChord {
                    command,
                    shift,
                    alt,
                    key,
                },
            );
            let state = if found.conflict {
                KeyState::Conflict
            } else if found.uses.is_empty() {
                KeyState::Unbound
            } else {
                KeyState::Bound
            };
            (key, state)
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Presets (ADR-7767)

/// The directory of user presets inside the user config directory.
pub const KEYMAP_PRESETS_DIR: &str = "keymaps";
/// The file-name ending of a user preset (`<name>.editor_keymap.ron`).
pub const PRESET_SUFFIX: &str = ".editor_keymap.ron";

/// The built-in `Laptop` preset for keyboards without a navigation block: Drop to floor moves from
/// End to Alt+Down (viewport) and the Outliner's Delete also answers Backspace. The brief's numpad
/// view keys have no command yet (no command binds a numpad key, and egui 0.36 reports a numpad digit
/// as the main-row digit key), so the preset moves no view keys; it grows when view-angle commands exist.
pub const LAPTOP_PRESET: &str = r#"(
    format_version: 1,
    bindings: {
        "outliner:outliner.delete": "Backspace",
        "viewport:selection.drop_to_floor": "Alt+ArrowDown",
    },
)"#;

/// The built-in presets: name and file text (`Default` has no user shortcuts).
pub const BUILTIN_PRESETS: &[(&str, &str)] = &[
    ("Default", "(format_version: 1, bindings: {})"),
    ("Laptop", LAPTOP_PRESET),
];

/// One preset the Keymap tab and `editor.keymap_preset_list` offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeymapPreset {
    /// The name.
    pub name: String,
    /// Whether it ships with the editor (it cannot be overwritten).
    pub builtin: bool,
    /// The user file, for a user preset.
    pub path: Option<PathBuf>,
}

/// Why a preset call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresetError {
    /// The agent error code: `preset_name`, `preset_builtin`, `preset_unknown`, `no_user_config`,
    /// or a [`KeymapError::code`].
    pub code: &'static str,
    /// What to show.
    pub message: String,
}

impl std::fmt::Display for PresetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Whether `name` can name a user preset: 1 to 64 letters, digits, spaces, `-` or `_`, not
/// starting with a space.
#[must_use]
pub fn valid_preset_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with(' ')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_'))
}

/// The user presets directory (`None` when the editor keeps no per-user files).
#[must_use]
pub fn presets_dir(paths: Option<&EditorConfigPaths>) -> Option<PathBuf> {
    paths?.dir.as_ref().map(|d| d.join(KEYMAP_PRESETS_DIR))
}

/// The built-in presets, then the user presets in `dir` by name (a user file named like a built-in
/// is skipped).
#[must_use]
pub fn list_presets(dir: Option<&Path>) -> Vec<KeymapPreset> {
    let mut out: Vec<KeymapPreset> = BUILTIN_PRESETS
        .iter()
        .map(|(name, _)| KeymapPreset {
            name: (*name).to_owned(),
            builtin: true,
            path: None,
        })
        .collect();
    let mut user = Vec::new();
    if let Some(dir) = dir
        && let Ok(entries) = std::fs::read_dir(dir)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(PRESET_SUFFIX))
            else {
                continue;
            };
            if valid_preset_name(name) && !BUILTIN_PRESETS.iter().any(|(b, _)| b.eq_ignore_ascii_case(name)) {
                user.push(KeymapPreset {
                    name: name.to_owned(),
                    builtin: false,
                    path: Some(path.clone()),
                });
            }
        }
    }
    user.sort_by(|a, b| a.name.cmp(&b.name));
    out.extend(user);
    out
}

/// The text of preset `name` (built-in or a file in `dir`).
///
/// # Errors
///
/// `preset_unknown` for no such preset, `keymap_io` when the file cannot be read.
pub fn preset_text(dir: Option<&Path>, name: &str) -> Result<String, PresetError> {
    if let Some((_, text)) = BUILTIN_PRESETS.iter().find(|(b, _)| *b == name) {
        return Ok((*text).to_owned());
    }
    let unknown = || PresetError {
        code: "preset_unknown",
        message: format!("no keymap preset is called {name:?}"),
    };
    if !valid_preset_name(name) {
        return Err(unknown());
    }
    let path = dir.ok_or_else(unknown)?.join(format!("{name}{PRESET_SUFFIX}"));
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(unknown()),
        Err(e) => Err(PresetError {
            code: "keymap_io",
            message: format!("cannot read {}: {e}", path.display()),
        }),
    }
}

/// Replaces the keymap with preset `name` at once (no restart; the shell then saves it as the
/// user's keymap). Answers the import report.
///
/// # Errors
///
/// [`preset_text`]'s, or the preset's [`KeymapError`] code.
pub fn apply_preset(world: &mut World, name: &str) -> Result<ImportReport, PresetError> {
    let dir = presets_dir(world.get_resource::<EditorConfigPaths>());
    let text = preset_text(dir.as_deref(), name)?;
    let current = world.get_resource::<CommandKeymap>().cloned().unwrap_or_default();
    let to_error = |e: KeymapError| PresetError {
        code: e.code(),
        message: format!("preset {name}: {e}"),
    };
    let report = import_keymap(&current, &text, ImportMode::Replace).map_err(to_error)?;
    // Replace: the preset's entries for unknown commands replace the current ones too.
    let unknown = parse_keymap(&text).map_err(to_error)?.unknown;
    world.insert_resource(report.keymap.clone());
    let mut store = world.get_resource_or_insert_with(KeymapStore::default);
    store.active_preset = Some(name.to_owned());
    store.unknown = unknown;
    Ok(report)
}

/// Saves the current keymap as user preset `name`; answers the file.
///
/// # Errors
///
/// `preset_name` for a bad name, `preset_builtin` for a built-in's name, `no_user_config` without a
/// user directory, `keymap_io` when the write fails.
pub fn save_preset(world: &mut World, name: &str) -> Result<PathBuf, PresetError> {
    if !valid_preset_name(name) {
        return Err(PresetError {
            code: "preset_name",
            message: format!("{name:?} is not a preset name (1 to 64 letters, digits, spaces, - or _)"),
        });
    }
    if BUILTIN_PRESETS.iter().any(|(b, _)| b.eq_ignore_ascii_case(name)) {
        return Err(PresetError {
            code: "preset_builtin",
            message: format!("{name} is a built-in preset and cannot be overwritten"),
        });
    }
    let dir = presets_dir(world.get_resource::<EditorConfigPaths>()).ok_or_else(|| PresetError {
        code: "no_user_config",
        message: "this editor keeps no per-user files".to_owned(),
    })?;
    std::fs::create_dir_all(&dir).map_err(|e| PresetError {
        code: "keymap_io",
        message: format!("cannot create {}: {e}", dir.display()),
    })?;
    let path = dir.join(format!("{name}{PRESET_SUFFIX}"));
    let keymap = world.get_resource::<CommandKeymap>().cloned().unwrap_or_default();
    let unknown = world
        .get_resource::<KeymapStore>()
        .map(|s| s.unknown.clone())
        .unwrap_or_default();
    crate::level_io::write_atomic(&path, keymap_to_text(&keymap, &unknown).as_bytes()).map_err(|e| {
        PresetError {
            code: "keymap_io",
            message: e,
        }
    })?;
    world
        .get_resource_or_insert_with(KeymapStore::default)
        .active_preset = Some(name.to_owned());
    Ok(path)
}

// ---------------------------------------------------------------------------------------------
// Agent methods: editor.keymap_lookup, editor.keymap_preset_list, _apply, _save

type Handler =
    fn(&mut World, &serde_json::Value) -> Result<serde_json::Value, regia_agent::protocol::RpcError>;

fn rpc_bad(code: &str, message: impl Into<String>, hint: &str) -> regia_agent::protocol::RpcError {
    regia_agent::protocol::RpcError::new(regia_agent::protocol::INVALID_PARAMS, code, message, hint)
}

/// The JSON of a [`ChordLookup`].
#[must_use]
pub fn lookup_json(found: &ChordLookup) -> serde_json::Value {
    serde_json::json!({
        "keys": found.keys,
        "commands": found.uses.iter().map(|u| serde_json::json!({
            "id": u.id,
            "title": u.title,
            "scope": u.scope.name(),
            "user": u.user,
        })).collect::<Vec<_>>(),
        "fixed": found.fixed,
        "conflict": found.conflict,
    })
}

/// `editor.keymap_lookup { chord }`.
fn keymap_lookup(
    world: &mut World,
    params: &serde_json::Value,
) -> Result<serde_json::Value, regia_agent::protocol::RpcError> {
    let chord = params
        .get("chord")
        .and_then(serde_json::Value::as_str)
        .and_then(KeyChord::parse)
        .ok_or_else(|| {
            rpc_bad(
                "bad_binding",
                "`chord` must be a key chord like \"Ctrl+Shift+K\"",
                "modifiers Ctrl (Cmd), Alt, Shift, then one key such as K, F5 or Space",
            )
        })?;
    let keymap = world.get_resource::<CommandKeymap>().cloned().unwrap_or_default();
    Ok(lookup_json(&lookup(&keymap, chord)))
}

fn presets_json(world: &World) -> serde_json::Value {
    let dir = presets_dir(world.get_resource::<EditorConfigPaths>());
    let active = world
        .get_resource::<KeymapStore>()
        .and_then(|s| s.active_preset.clone());
    serde_json::json!({
        "presets": list_presets(dir.as_deref()).iter().map(|p| serde_json::json!({
            "name": p.name,
            "builtin": p.builtin,
            "path": p.path.as_ref().map(|p| p.display().to_string()),
        })).collect::<Vec<_>>(),
        "dir": dir.as_ref().map(|d| d.display().to_string()),
        "active": active,
    })
}

fn preset_name(params: &serde_json::Value) -> Result<String, regia_agent::protocol::RpcError> {
    params
        .get("name")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| {
            rpc_bad(
                "preset_name",
                "`name` is required",
                "see editor.keymap_preset_list",
            )
        })
}

/// `editor.keymap_preset_list`.
fn keymap_preset_list(
    world: &mut World,
    _params: &serde_json::Value,
) -> Result<serde_json::Value, regia_agent::protocol::RpcError> {
    Ok(presets_json(world))
}

/// `editor.keymap_preset_apply { name }`.
fn keymap_preset_apply(
    world: &mut World,
    params: &serde_json::Value,
) -> Result<serde_json::Value, regia_agent::protocol::RpcError> {
    let name = preset_name(params)?;
    let report = apply_preset(world, &name)
        .map_err(|e| rpc_bad(e.code, e.message, "see editor.keymap_preset_list"))?;
    let mut out = presets_json(world);
    out["applied"] = serde_json::json!(
        report
            .applied
            .iter()
            .map(|(id, keys)| serde_json::json!({ "id": id, "keys": keys }))
            .collect::<Vec<_>>()
    );
    out["problems"] = serde_json::json!(
        report
            .problems
            .iter()
            .map(|p| serde_json::json!({ "kind": p.kind(), "message": p.message() }))
            .collect::<Vec<_>>()
    );
    Ok(out)
}

/// `editor.keymap_preset_save { name }`.
fn keymap_preset_save(
    world: &mut World,
    params: &serde_json::Value,
) -> Result<serde_json::Value, regia_agent::protocol::RpcError> {
    let name = preset_name(params)?;
    let path = save_preset(world, &name)
        .map_err(|e| rpc_bad(e.code, e.message, "names: letters, digits, spaces, - and _"))?;
    let mut out = presets_json(world);
    out["saved_to"] = serde_json::json!(path.display().to_string());
    Ok(out)
}

/// The keymap lookup and preset methods, installed by `agent_ext::all_methods`.
#[must_use]
pub fn methods() -> Vec<(regia_agent::schema::MethodSchema, Handler)> {
    use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
    const NAME: ParamSchema = ParamSchema {
        name: "name",
        kind: "string",
        required: true,
        description: "The preset name: Default, Laptop, or a user preset (letters, digits, spaces, - and _).",
    };
    vec![
        (
            MethodSchema {
                name: "editor.keymap_lookup",
                tool: "keymap_lookup",
                description: "Every command a key chord runs, in any scope (user bindings and the built-in shortcuts the user did not move), plus the fixed and safety keys on it, and whether two uses collide. The Keymap tab's \"Press a key…\" search and keyboard map show the same.",
                params: &[ParamSchema {
                    name: "chord",
                    kind: "string",
                    required: true,
                    description: "A chord such as \"Ctrl+Shift+K\", \"X\" or \"F1\".",
                }],
                result: "{ keys, commands: [{ id, title, scope, user }], fixed, conflict }",
                mutating: false,
                permission: Permission::Read,
            },
            keymap_lookup as Handler,
        ),
        (
            MethodSchema {
                name: "editor.keymap_preset_list",
                tool: "keymap_preset_list",
                description: "The keymap presets: the built-in Default and Laptop, then the user presets in <user config>/regia/keymaps/<name>.editor_keymap.ron, and the one applied last.",
                params: &[],
                result: "{ presets: [{ name, builtin, path }], dir, active }",
                mutating: false,
                permission: Permission::Read,
            },
            keymap_preset_list as Handler,
        ),
        (
            MethodSchema {
                name: "editor.keymap_preset_apply",
                tool: "keymap_preset_apply",
                description: "Replace the keymap with a preset at once (no restart); the editor then saves it as the user's keymap. preset_unknown for no such preset.",
                params: &[NAME],
                result: "the editor.keymap_preset_list result plus applied: [{ id, keys }], problems",
                mutating: true,
                permission: Permission::EditScene,
            },
            keymap_preset_apply as Handler,
        ),
        (
            MethodSchema {
                name: "editor.keymap_preset_save",
                tool: "keymap_preset_save",
                description: "Save the current keymap as a user preset. preset_name for a bad name, preset_builtin for Default or Laptop, no_user_config when the editor keeps no per-user files.",
                params: &[NAME],
                result: "the editor.keymap_preset_list result plus saved_to",
                mutating: true,
                permission: Permission::EditScene,
            },
            keymap_preset_save as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn duplicate_chords_keep_the_first_id() {
        let text = r#"(format_version: 1, bindings: { "view.toggle_grid": "Ctrl+G", "view.toggle_labels": "ctrl+g" })"#;
        let read = parse_keymap(text).unwrap();
        assert_eq!(read.keymap.bindings.len(), 1);
        assert!(read.keymap.bindings.contains_key("view.toggle_grid"));
        assert_eq!(read.problems.len(), 1);
        assert_eq!(read.problems[0].kind(), "duplicate");
    }

    #[test]
    fn a_user_chord_on_undo_conflicts_until_undo_moves() {
        let mut k = CommandKeymap::default();
        k.bind("view.toggle_grid", KeyChord::parse("Ctrl+Z").unwrap())
            .unwrap();
        let c = conflicts(&k);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].command, "view.toggle_grid");
        assert!(c[0].shadows.contains("edit.undo"), "{c:?}");
        // Give undo another chord: its built-in no longer applies, nothing is shadowed.
        k.bind("edit.undo", KeyChord::parse("Alt+Z").unwrap()).unwrap();
        assert!(conflicts(&k).is_empty());
        k.bind("view.toggle_labels", KeyChord::parse("Ctrl+Y").unwrap())
            .unwrap();
        assert_eq!(conflicts(&k)[0].shadows, "edit.redo (second key)");
    }

    fn chord(text: &str) -> KeyChord {
        KeyChord::parse(text).unwrap()
    }

    #[test]
    fn one_chord_in_two_scopes_keeps_both_and_conflicts_only_within_a_scope() {
        let mut k = CommandKeymap::default();
        assert_eq!(
            k.bind_in("view.toggle_grid", chord("X"), ShortcutScope::Viewport),
            Ok(None)
        );
        // X in the timeline does not take it from the viewport binding.
        assert_eq!(
            k.bind_in("view.toggle_labels", chord("X"), ShortcutScope::Timeline),
            Ok(None)
        );
        assert_eq!(k.bindings.len(), 2);
        assert!(conflicts(&k).is_empty(), "{:?}", conflicts(&k));
        // In the same scope the chord moves, as before scopes.
        assert_eq!(
            k.bind_in("view.toggle_realtime", chord("X"), ShortcutScope::Timeline),
            Ok(Some("view.toggle_labels"))
        );
        // A global X conflicts with both scoped ones.
        k.bind_in("view.toggle_audio_gizmos", chord("X"), ShortcutScope::Global)
            .unwrap();
        let c = conflicts(&k);
        assert_eq!(c.len(), 2, "{c:?}");
        assert!(c.iter().all(|c| c.shadows.contains("view.toggle_audio_gizmos")));
    }

    #[test]
    fn the_focused_scope_wins_then_global_and_typing_runs_only_text_bindings() {
        let mut k = CommandKeymap::default();
        k.bind_in("view.toggle_grid", chord("X"), ShortcutScope::Viewport)
            .unwrap();
        k.bind_in("view.toggle_labels", chord("X"), ShortcutScope::Timeline)
            .unwrap();
        k.bind_in("view.toggle_realtime", chord("Y"), ShortcutScope::Global)
            .unwrap();
        k.bind_in("view.toggle_audio_gizmos", chord("Y"), ShortcutScope::Viewport)
            .unwrap();
        assert_eq!(
            resolve(&k, ShortcutScope::Viewport, chord("X")),
            Some("view.toggle_grid")
        );
        assert_eq!(
            resolve(&k, ShortcutScope::Timeline, chord("X")),
            Some("view.toggle_labels")
        );
        assert_eq!(resolve(&k, ShortcutScope::Outliner, chord("X")), None);
        assert_eq!(
            resolve(&k, ShortcutScope::Viewport, chord("Y")),
            Some("view.toggle_audio_gizmos")
        );
        assert_eq!(
            resolve(&k, ShortcutScope::Outliner, chord("Y")),
            Some("view.toggle_realtime")
        );
        assert_eq!(
            resolve(&k, ShortcutScope::Text, chord("Y")),
            None,
            "global keys wait while typing"
        );
        // F1 works everywhere, typing included.
        assert_eq!(
            resolve(&k, ShortcutScope::Text, chord("F1")),
            Some("help.context")
        );
    }

    #[test]
    fn safety_keys_cannot_be_bound_in_a_scope() {
        let mut k = CommandKeymap::default();
        for keys in ["Ctrl+S", "Ctrl+Z", "Ctrl+Y", "Ctrl+Shift+Z", "Escape"] {
            assert_eq!(
                k.bind_in("view.toggle_grid", chord(keys), ShortcutScope::Viewport),
                Err(BindError::SafetyKey),
                "{keys}"
            );
        }
        assert!(k.bindings.is_empty());
        // Globally they bind (and are reported as conflicts) as before.
        assert!(
            k.bind_in("view.toggle_grid", chord("Ctrl+S"), ShortcutScope::Global)
                .is_ok()
        );
    }

    #[test]
    fn scoped_bindings_round_trip_through_the_v1_file() {
        let mut k = CommandKeymap::default();
        k.bind_in("view.toggle_grid", chord("X"), ShortcutScope::Viewport)
            .unwrap();
        k.bind_in("view.toggle_labels", chord("X"), ShortcutScope::Timeline)
            .unwrap();
        k.bind("view.toggle_realtime", chord("Ctrl+G")).unwrap();
        let text = keymap_to_text(&k, &BTreeMap::new());
        assert!(text.contains("\"viewport:view.toggle_grid\""), "{text}");
        assert!(text.contains("format_version: 1"), "{text}");
        let read = parse_keymap(&text).unwrap();
        assert!(read.problems.is_empty(), "{:?}", read.problems);
        assert_eq!(read.keymap.bindings, k.bindings);
        assert_eq!(read.keymap.scopes, k.scopes);
        // An unknown scope is kept for write-back, like an unknown command.
        let read =
            parse_keymap(r#"(format_version: 1, bindings: { "wing:view.toggle_grid": "X" })"#).unwrap();
        assert!(read.unknown.contains_key("wing:view.toggle_grid"));
        // A safety key in a scope is dropped with a problem.
        let read =
            parse_keymap(r#"(format_version: 1, bindings: { "viewport:view.toggle_grid": "Ctrl+S" })"#)
                .unwrap();
        assert_eq!(read.problems[0].kind(), "safety_key");
    }

    #[test]
    fn lookup_lists_every_use_and_holds_do_not_collide() {
        let k = CommandKeymap::default();
        let w = lookup(&k, chord("W"));
        let ids: Vec<&str> = w.uses.iter().map(|u| u.id).collect();
        assert!(
            ids.contains(&"gizmo.translate") && ids.contains(&"camera.fly_forward"),
            "{ids:?}"
        );
        assert!(
            w.uses
                .iter()
                .all(|u| u.scope == ShortcutScope::Viewport && !u.user)
        );
        assert!(!w.conflict, "the fly key is held, the gizmo key pressed");
        assert_eq!(lookup(&k, chord("Ctrl+S")).fixed, ["save"]);
        let states = key_states(&k, false, false, false);
        assert_eq!(
            states.iter().find(|(key, _)| *key == egui::Key::W).map(|s| s.1),
            Some(KeyState::Bound)
        );
        assert_eq!(
            states.iter().find(|(key, _)| *key == egui::Key::J).map(|s| s.1),
            Some(KeyState::Unbound)
        );
        let mut k = CommandKeymap::default();
        k.bind_in("view.toggle_grid", chord("W"), ShortcutScope::Viewport)
            .unwrap();
        let states = key_states(&k, false, false, false);
        assert_eq!(
            states.iter().find(|(key, _)| *key == egui::Key::W).map(|s| s.1),
            Some(KeyState::Conflict)
        );
    }

    /// Review sprint-w12-rev-g1 #8: a keymap file with a typo is reported and not replaced by the
    /// next shortcut edit.
    #[test]
    fn a_keymap_file_that_does_not_parse_is_never_replaced() {
        let dir = regia_test_utils::unique_temp_dir("keymap-broken");
        std::fs::create_dir_all(&dir).unwrap();
        let mut world = World::new();
        world.insert_resource(EditorConfigPaths::at(&dir));
        let path = world.resource::<EditorConfigPaths>().keymap_file().unwrap();
        let broken = "(format_version: 1, bindings: {\"view.toggle_grid\": \"Ctrl+G\"}";
        std::fs::write(&path, broken).unwrap();
        load_keymap(&mut world);
        assert!(world.resource::<KeymapStore>().read_only.is_some());
        assert!(!world.resource::<KeymapStore>().problems.is_empty());
        let _ = world
            .resource_mut::<CommandKeymap>()
            .bind("view.toggle_grid", chord("Ctrl+Shift+G"));
        assert!(
            !persist_keymap(&mut world),
            "no write over a file that did not parse"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), broken);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn presets_list_apply_and_save_without_a_restart() {
        let dir = regia_test_utils::unique_temp_dir("keymap-presets");
        let mut world = World::new();
        world.insert_resource(EditorConfigPaths::at(&dir));
        let names: Vec<String> = list_presets(presets_dir(world.get_resource()).as_deref())
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, ["Default", "Laptop"]);
        let report = apply_preset(&mut world, "Laptop").unwrap();
        assert!(report.problems.is_empty(), "{:?}", report.problems);
        let k = world.resource::<CommandKeymap>().clone();
        assert_eq!(
            k.bindings.get("selection.drop_to_floor"),
            Some(&chord("Alt+ArrowDown"))
        );
        assert_eq!(k.scope_of("outliner.delete"), ShortcutScope::Outliner);
        let path = save_preset(&mut world, "My laptop").unwrap();
        assert!(
            path.ends_with("keymaps/My laptop.editor_keymap.ron"),
            "{}",
            path.display()
        );
        apply_preset(&mut world, "Default").unwrap();
        assert!(world.resource::<CommandKeymap>().bindings.is_empty());
        apply_preset(&mut world, "My laptop").unwrap();
        assert_eq!(world.resource::<CommandKeymap>().bindings, k.bindings);
        assert_eq!(
            world.resource::<KeymapStore>().active_preset.as_deref(),
            Some("My laptop")
        );
        assert_eq!(
            save_preset(&mut world, "laptop").unwrap_err().code,
            "preset_builtin"
        );
        assert_eq!(save_preset(&mut world, "../x").unwrap_err().code, "preset_name");
        assert_eq!(
            apply_preset(&mut world, "Nope").unwrap_err().code,
            "preset_unknown"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn press(ctx: &egui::Context, world: &mut World, key: egui::Key, frame: f64) {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            time: Some(frame / 60.0),
            events: vec![egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ui| {
            crate::command_palette::show(ui.ctx(), world, false);
        });
        out.textures_delta.clear();
    }

    /// The brief's acceptance: X bound in the viewport and X in the timeline to different commands;
    /// each fires only with its panel focused, through the palette's real key handler.
    #[test]
    fn x_in_viewport_and_x_in_timeline_fire_only_with_their_panel_focused() {
        use crate::shell::Panel;
        use crate::viewport::ViewportSettings;
        let mut world = World::new();
        let mut k = CommandKeymap::default();
        k.bind_in("view.toggle_grid", chord("X"), ShortcutScope::Viewport)
            .unwrap();
        k.bind_in("view.toggle_labels", chord("X"), ShortcutScope::Timeline)
            .unwrap();
        world.insert_resource(k);
        let start = ViewportSettings::default();
        let ctx = egui::Context::default();
        world.insert_resource(FocusedPanel(Some(Panel::Viewport)));
        press(&ctx, &mut world, egui::Key::X, 1.0);
        let s = world.resource::<ViewportSettings>().clone();
        assert_eq!(
            (s.grid, s.labels),
            (!start.grid, start.labels),
            "viewport X toggles the grid only"
        );
        world.insert_resource(FocusedPanel(Some(Panel::SeqCurves)));
        press(&ctx, &mut world, egui::Key::X, 2.0);
        let s = world.resource::<ViewportSettings>().clone();
        assert_eq!(
            (s.grid, s.labels),
            (!start.grid, !start.labels),
            "timeline X toggles labels only"
        );
        world.insert_resource(FocusedPanel(Some(Panel::Outliner)));
        press(&ctx, &mut world, egui::Key::X, 3.0);
        let after = world.resource::<ViewportSettings>();
        assert_eq!(
            (after.grid, after.labels),
            (s.grid, s.labels),
            "no X binding in the outliner"
        );
    }

    #[test]
    fn a_captured_press_runs_no_shortcut() {
        let mut world = World::new();
        let mut k = CommandKeymap::default();
        k.bind("view.toggle_grid", chord("X")).unwrap();
        world.insert_resource(k);
        world.insert_resource(ChordCapture {
            active: true,
            captured: None,
            armed_frame: 0,
        });
        let ctx = egui::Context::default();
        press(&ctx, &mut world, egui::Key::X, 1.0);
        assert_eq!(world.resource::<ChordCapture>().captured, Some(chord("X")));
        assert!(
            world
                .get_resource::<crate::viewport::ViewportSettings>()
                .is_none(),
            "the grid did not toggle"
        );
    }

    #[test]
    fn a_global_chord_with_more_modifiers_beats_a_plainer_scoped_one() {
        let mut k = CommandKeymap::default();
        k.bind_in("view.toggle_grid", chord("X"), ShortcutScope::Viewport)
            .unwrap();
        k.bind_in("view.toggle_labels", chord("Shift+X"), ShortcutScope::Global)
            .unwrap();
        let order: Vec<&str> = dispatch_order(&k, ShortcutScope::Viewport)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(order[0], "view.toggle_labels", "{order:?}");
        // An equal chord: the focused scope first.
        k.bind_in("view.toggle_realtime", chord("X"), ShortcutScope::Global)
            .unwrap();
        assert_eq!(
            resolve(&k, ShortcutScope::Viewport, chord("X")),
            Some("view.toggle_grid")
        );
    }

    #[test]
    fn rebinding_a_viewport_command_keeps_it_in_the_viewport() {
        let mut k = CommandKeymap::default();
        k.bind("gizmo.translate", chord("T")).unwrap();
        assert_eq!(k.scope_of("gizmo.translate"), ShortcutScope::Viewport);
        k.bind("view.toggle_grid", chord("G")).unwrap();
        assert_eq!(k.scope_of("view.toggle_grid"), ShortcutScope::Global);
        // A user press binding on a held fly key's chord collides (pressing A while flying runs it).
        let mut k = CommandKeymap::default();
        k.bind_in("view.toggle_grid", chord("A"), ShortcutScope::Viewport)
            .unwrap();
        assert!(conflicts(&k)[0].shadows.contains("camera.fly_left"));
        assert!(lookup(&k, chord("A")).conflict);
    }

    #[test]
    fn a_command_bound_in_two_scopes_keeps_the_second_entry_for_write_back() {
        let text =
            r#"(format_version: 1, bindings: { "view.toggle_grid": "G", "viewport:view.toggle_grid": "X" })"#;
        let read = parse_keymap(text).unwrap();
        assert_eq!(read.keymap.bindings.len(), 1);
        assert_eq!(
            read.unknown.get("viewport:view.toggle_grid").map(String::as_str),
            Some("X")
        );
        let back = keymap_to_text(&read.keymap, &read.unknown);
        assert!(back.contains("\"viewport:view.toggle_grid\""), "{back}");
    }

    #[test]
    fn a_capture_whose_tab_stops_drawing_expires_and_eats_no_key() {
        let mut world = World::new();
        let mut k = CommandKeymap::default();
        k.bind("view.toggle_grid", chord("X")).unwrap();
        world.insert_resource(k);
        world.insert_resource(ChordCapture {
            active: true,
            captured: None,
            armed_frame: 0,
        });
        let ctx = egui::Context::default();
        // Frames 0 and 1 draw no Keymap tab (nothing re-arms the capture); at frame 2 it has expired.
        for _ in 0..2 {
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
                crate::command_palette::show(ui.ctx(), &mut world, false);
            });
            out.textures_delta.clear();
        }
        assert!(
            world.resource::<ChordCapture>().active,
            "still pending one frame after arming"
        );
        press(&ctx, &mut world, egui::Key::X, 3.0);
        assert!(!world.resource::<ChordCapture>().active);
        assert_eq!(world.resource::<ChordCapture>().captured, None);
        assert!(
            world
                .get_resource::<crate::viewport::ViewportSettings>()
                .is_some(),
            "X ran its shortcut"
        );
    }
}
