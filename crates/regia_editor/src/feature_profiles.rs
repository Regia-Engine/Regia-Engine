// SPDX-License-Identifier: MPL-2.0
//! Editor feature profiles (`docs/specs/19-editor.md` § 2, the v0.6 parity addendum § 1.4,
//! ADR-7753): a profile hides whole feature areas, single panels or entity kinds from the View
//! menu, the command palette, the add-tab popup and Create, so a team that only builds, say, game
//! UI sees only what it uses.
//!
//! A profile is a file the user can save and share (`<name>.editor_profile.ron`, version 1):
//!
//! ```ron
//! (
//!     format_version: 1,
//!     name: "UI only",
//!     hidden_areas: [animation, networking, terrain, procgen, audio, scripting],
//!     hidden_panels: [],
//!     hidden_kinds: [],
//! )
//! ```
//!
//! Areas are RON identifiers (the JSON of `editor.feature_profile_set` takes them as strings).
//! The active profile is per project: a copy lives in `<project>/.regia/editor_profile.ron` (no
//! file means Full). Saved profiles live in `<user config>/profiles/`. Built-in profiles: Full (the
//! default, hides nothing) and UI only (an example). Hiding never deletes anything: a hidden panel's
//! tab closes and comes back when a profile shows it again; agents keep every method.
//!
//! Agent surface: `editor.feature_profile_get` and `editor.feature_profile_set`
//! (`book/agents/editor-layouts.md`).

use std::path::{Path, PathBuf};

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::shell::Panel;

/// Current version of `.editor_profile.ron`.
pub const PROFILE_FORMAT_VERSION: u32 = 1;
/// The file suffix of a profile.
pub const PROFILE_FILE_SUFFIX: &str = ".editor_profile.ron";
/// The project's active profile, relative to the project root.
pub const PROJECT_PROFILE_FILE: &str = ".regia/editor_profile.ron";
/// The folder of saved profiles, inside the user config directory.
pub const PROFILE_DIR: &str = "profiles";
/// The most entries one hidden list may have.
pub const MAX_HIDDEN: usize = 512;

/// A feature area a profile can hide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum FeatureArea {
    /// 3D content: materials, lighting bakes, kits, look development, fracture, cloth, vehicles,
    /// visual effects, and the 3D entity kinds of Create.
    #[serde(rename = "3d")]
    ThreeD,
    /// The 2D tile mode.
    #[serde(rename = "2d")]
    TwoD,
    /// Animation graphs, clips, skeletons, montages, motion matching, ragdolls, sequence curves.
    #[serde(rename = "animation")]
    Animation,
    /// The in-game UI designer, its animation and inspector panels, and text tables.
    #[serde(rename = "ui")]
    Ui,
    /// Network state.
    #[serde(rename = "networking")]
    Networking,
    /// Terrain.
    #[serde(rename = "terrain")]
    Terrain,
    /// Procedural graphs.
    #[serde(rename = "procgen")]
    Procgen,
    /// The audio mixer and cue editor.
    #[serde(rename = "audio")]
    Audio,
    /// Gameplay logic: state trees, behavior trees, smart objects, abilities, conversations.
    #[serde(rename = "scripting")]
    Scripting,
}

/// Every area, in menu order.
pub const FEATURE_AREAS: &[FeatureArea] = &[
    FeatureArea::ThreeD,
    FeatureArea::TwoD,
    FeatureArea::Animation,
    FeatureArea::Ui,
    FeatureArea::Networking,
    FeatureArea::Terrain,
    FeatureArea::Procgen,
    FeatureArea::Audio,
    FeatureArea::Scripting,
];

impl FeatureArea {
    /// The id the file and the agent use.
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::ThreeD => "3d",
            Self::TwoD => "2d",
            Self::Animation => "animation",
            Self::Ui => "ui",
            Self::Networking => "networking",
            Self::Terrain => "terrain",
            Self::Procgen => "procgen",
            Self::Audio => "audio",
            Self::Scripting => "scripting",
        }
    }

    /// The area with `id`.
    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        FEATURE_AREAS.iter().copied().find(|a| a.id() == id)
    }
}

/// The feature area of a built-in panel (`None`: always shown, such as the viewport, outliner,
/// inspector, content browser, log, settings and diagnostics).
#[must_use]
pub fn panel_area(panel: Panel) -> Option<FeatureArea> {
    use FeatureArea as A;
    Some(match panel {
        Panel::Material
        | Panel::MaterialGraph
        | Panel::Kit
        | Panel::Bake
        | Panel::BakeRunner
        | Panel::Lightmaps
        | Panel::LookDev
        | Panel::Fracture
        | Panel::Cloth
        | Panel::VehicleTuning
        | Panel::VfxEditor
        | Panel::DccFeed
        | Panel::Shots
        | Panel::Cameras => A::ThreeD,
        Panel::Mode2d => A::TwoD,
        Panel::AnimGraph
        | Panel::AnimClip
        | Panel::AnimSkeleton
        | Panel::Montage
        | Panel::MotionMatching
        | Panel::Ragdoll
        | Panel::SeqCurves => A::Animation,
        Panel::UiDesigner | Panel::UiAnim | Panel::UiInspector | Panel::Localization => A::Ui,
        Panel::Net => A::Networking,
        Panel::Terrain => A::Terrain,
        Panel::ProcGraph => A::Procgen,
        Panel::AudioMixer | Panel::CueEditor => A::Audio,
        Panel::StateTrees
        | Panel::StateTreeEditor
        | Panel::BehaviorTree
        | Panel::SmartObjects
        | Panel::Abilities
        | Panel::Conversation => A::Scripting,
        _ => return None,
    })
}

/// The feature area of a Create entity kind (`editor.create_entity { kind }`): every kind the
/// Create menu has places a 3D entity.
#[must_use]
pub fn kind_area(kind: &str) -> Option<FeatureArea> {
    crate::shapes::create_kinds()
        .contains(&kind)
        .then_some(FeatureArea::ThreeD)
}

/// A feature profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeatureProfile {
    /// [`PROFILE_FORMAT_VERSION`] (a missing field reads as the current version).
    #[serde(default = "crate::editor_prefs::version_missing")]
    pub format_version: u32,
    /// The name shown in menus.
    pub name: String,
    /// Whole areas hidden.
    #[serde(default)]
    pub hidden_areas: Vec<FeatureArea>,
    /// Single panels hidden, by panel id.
    #[serde(default)]
    pub hidden_panels: Vec<String>,
    /// Create entity kinds hidden, by kind id.
    #[serde(default)]
    pub hidden_kinds: Vec<String>,
}

/// The built-in profile names (`editor.feature_profile_set { name }`).
pub const BUILT_IN_PROFILES: &[&str] = &["Full", "UI only"];

impl FeatureProfile {
    /// Full: hides nothing (the default).
    #[must_use]
    pub fn full() -> Self {
        Self {
            format_version: PROFILE_FORMAT_VERSION,
            name: "Full".to_owned(),
            hidden_areas: Vec::new(),
            hidden_panels: Vec::new(),
            hidden_kinds: Vec::new(),
        }
    }

    /// UI only: everything but the UI area and the always-shown panels hidden.
    #[must_use]
    pub fn ui_only() -> Self {
        Self {
            name: "UI only".to_owned(),
            hidden_areas: FEATURE_AREAS
                .iter()
                .copied()
                .filter(|a| *a != FeatureArea::Ui)
                .collect(),
            ..Self::full()
        }
    }

    /// The built-in profile `name` (without case).
    #[must_use]
    pub fn built_in(name: &str) -> Option<Self> {
        if name.eq_ignore_ascii_case("full") {
            Some(Self::full())
        } else if name.eq_ignore_ascii_case("ui only") || name.eq_ignore_ascii_case("ui_only") {
            Some(Self::ui_only())
        } else {
            None
        }
    }

    /// Whether the profile hides nothing.
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.hidden_areas.is_empty() && self.hidden_panels.is_empty() && self.hidden_kinds.is_empty()
    }

    /// Whether `panel` is hidden (extension panels never are).
    #[must_use]
    pub fn hides_panel(&self, panel: Panel) -> bool {
        if matches!(panel, Panel::ExtensionPanel(..)) {
            return false;
        }
        self.hidden_panels.iter().any(|id| id == panel.id())
            || panel_area(panel).is_some_and(|a| self.hidden_areas.contains(&a))
    }

    /// Whether Create entity kind `kind` is hidden.
    #[must_use]
    pub fn hides_kind(&self, kind: &str) -> bool {
        self.hidden_kinds.iter().any(|k| k == kind)
            || kind_area(kind).is_some_and(|a| self.hidden_areas.contains(&a))
    }

    /// Checks the name and list sizes, drops repeated entries and reports unknown panel or kind
    /// ids (kept: a newer editor's panel id stays in the shared file).
    ///
    /// # Errors
    ///
    /// `bad_profile` for an empty or overlong name or an overlong list.
    pub fn check(&mut self) -> Result<Vec<String>, ProfileError> {
        self.name = self.name.trim().to_owned();
        if self.name.is_empty() || self.name.chars().count() > 64 {
            return Err(ProfileError::new(
                "bad_profile",
                "a profile name has 1 to 64 characters",
            ));
        }
        if self.hidden_panels.len() > MAX_HIDDEN || self.hidden_kinds.len() > MAX_HIDDEN {
            return Err(ProfileError::new(
                "bad_profile",
                format!("a hidden list has at most {MAX_HIDDEN} entries"),
            ));
        }
        self.hidden_areas.sort();
        self.hidden_areas.dedup();
        dedup_keep_order(&mut self.hidden_panels);
        dedup_keep_order(&mut self.hidden_kinds);
        let kinds = crate::shapes::create_kinds();
        let mut unknown: Vec<String> = self
            .hidden_panels
            .iter()
            .filter(|id| Panel::from_id(id).is_none())
            .map(|id| format!("panel {id}"))
            .collect();
        unknown.extend(
            self.hidden_kinds
                .iter()
                .filter(|k| !kinds.contains(&k.as_str()))
                .map(|k| format!("kind {k}")),
        );
        Ok(unknown)
    }

    /// Parses profile text.
    ///
    /// # Errors
    ///
    /// `profile_parse`, `profile_newer` or `bad_profile`.
    pub fn from_text(text: &str) -> Result<Self, ProfileError> {
        let mut profile = crate::editor_prefs::read_versioned(text, PROFILE_FORMAT_VERSION, |p: &Self| {
            p.format_version
        })
        .map_err(|e| match e {
            crate::editor_prefs::VersionedError::Parse(m) => ProfileError::new("profile_parse", m),
            crate::editor_prefs::VersionedError::Newer(found) => ProfileError::new(
                "profile_newer",
                format!(
                    "written by a newer editor (format_version {found}, this editor reads up to {PROFILE_FORMAT_VERSION})"
                ),
            ),
        })?;
        profile.format_version = PROFILE_FORMAT_VERSION;
        profile.check()?;
        Ok(profile)
    }

    /// The file text (current version).
    #[must_use]
    pub fn to_text(&self) -> String {
        let mut profile = self.clone();
        profile.format_version = PROFILE_FORMAT_VERSION;
        ron::ser::to_string_pretty(&profile, regia_core::ron_pretty().struct_names(false)).unwrap_or_default()
    }
}

fn dedup_keep_order(list: &mut Vec<String>) {
    let mut seen = std::collections::HashSet::new();
    list.retain(|item| seen.insert(item.clone()));
}

/// The `from` versions of the registered `.editor_profile.ron` migration steps (none: version 1
/// is the first).
#[must_use]
pub fn profile_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The serde shape of `.editor_profile.ron` (ADR-7902: the `editor-profile` format).
#[must_use]
pub fn profile_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<FeatureProfile>(),
        regia_core::schema::serde_variant_names::<FeatureArea>(),
    ])
}

/// Why a profile was refused. `code` is the agent error code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileError {
    /// Lowercase agent error code.
    pub code: &'static str,
    /// What went wrong.
    pub message: String,
}

impl ProfileError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ProfileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ProfileError {}

/// The project's active profile file.
#[must_use]
pub fn project_profile_path(root: &Path) -> PathBuf {
    root.join(PROJECT_PROFILE_FILE)
}

/// Reads a profile file; `Ok(None)` when it does not exist.
///
/// # Errors
///
/// `profile_io` or the text's error.
pub fn read_profile_file(path: &Path) -> Result<Option<FeatureProfile>, ProfileError> {
    match std::fs::read_to_string(path) {
        Ok(text) => FeatureProfile::from_text(&text).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ProfileError::new(
            "profile_io",
            format!("cannot read {}: {e}", path.display()),
        )),
    }
}

/// Writes `profile` to `path` atomically.
///
/// # Errors
///
/// `profile_io`.
pub fn write_profile_file(path: &Path, profile: &FeatureProfile) -> Result<(), ProfileError> {
    crate::level_io::write_atomic(path, profile.to_text().as_bytes())
        .map_err(|e| ProfileError::new("profile_io", e))
}

/// The active profile and the project it belongs to (a resource; absent means Full).
#[derive(Resource, Debug, Clone, PartialEq)]
pub struct ActiveFeatureProfile {
    /// The profile.
    pub profile: FeatureProfile,
    /// The project whose `.regia/editor_profile.ron` it came from.
    pub root: Option<PathBuf>,
    /// Bumped on every change (the shell closes and reopens tabs when it moves).
    pub generation: u64,
}

/// Loads the open project's active profile when the project changed (cheap otherwise: one
/// resource compare per frame). A file that does not read leaves Full with a warning.
pub fn sync(world: &mut World) {
    // Compared by reference first: an unchanged project allocates nothing per frame.
    let open = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.as_deref());
    let current = world.get_resource::<ActiveFeatureProfile>();
    if current.is_some_and(|c| c.root.as_deref() == open) {
        return;
    }
    let root = open.map(Path::to_path_buf);
    let generation = current.map_or(1, |c| c.generation + 1);
    let profile = match root
        .as_deref()
        .map(|r| read_profile_file(&project_profile_path(r)))
    {
        Some(Ok(Some(profile))) => profile,
        Some(Err(err)) => {
            tracing::warn!(target: "regia_editor", "feature profile not read: {err}");
            FeatureProfile::full()
        }
        _ => FeatureProfile::full(),
    };
    world.insert_resource(ActiveFeatureProfile {
        profile,
        root,
        generation,
    });
}

/// Whether the active profile hides `panel`.
#[must_use]
pub fn panel_hidden(world: &World, panel: Panel) -> bool {
    world
        .get_resource::<ActiveFeatureProfile>()
        .is_some_and(|a| a.profile.hides_panel(panel))
}

/// Whether the active profile hides Create entity kind `kind`.
#[must_use]
pub fn kind_hidden(world: &World, kind: &str) -> bool {
    world
        .get_resource::<ActiveFeatureProfile>()
        .is_some_and(|a| a.profile.hides_kind(kind))
}

/// The palette command ids the active profile hides: `window.open_<panel>` of hidden panels and
/// `create.<kind>` of hidden kinds.
#[must_use]
pub fn hidden_command_ids(world: &World) -> Vec<&'static str> {
    let Some(active) = world.get_resource::<ActiveFeatureProfile>() else {
        return Vec::new();
    };
    if active.profile.is_full() {
        return Vec::new();
    }
    crate::command_palette::COMMANDS
        .iter()
        .filter(|def| {
            if let Some(id) = def.id.strip_prefix(crate::command_palette::OPEN_PANEL_PREFIX) {
                Panel::from_id(id).is_some_and(|p| active.profile.hides_panel(p))
            } else if let Some(kind) = def.id.strip_prefix("create.") {
                active.profile.hides_kind(kind)
            } else {
                false
            }
        })
        .map(|def| def.id)
        .collect()
}

/// The saved profiles folder of the world's user config.
#[must_use]
pub fn profile_dir_of(world: &World) -> Option<PathBuf> {
    world
        .get_resource::<crate::editor_prefs::EditorConfigPaths>()
        .and_then(|paths| paths.dir.as_deref().map(|d| d.join(PROFILE_DIR)))
}

/// The saved profiles in `dir`: `(name, path)` by file name.
#[must_use]
pub fn saved_profiles(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<(String, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let name = e
                .file_name()
                .to_str()?
                .strip_suffix(PROFILE_FILE_SUFFIX)?
                .to_owned();
            Some((name, e.path()))
        })
        .collect();
    out.sort();
    out
}

/// Makes `profile` the open project's active profile: writes (Full: removes)
/// `.regia/editor_profile.ron` and updates the resource.
///
/// # Errors
///
/// `no_project` or `profile_io`.
pub fn activate(world: &mut World, profile: FeatureProfile) -> Result<(), ProfileError> {
    sync(world);
    let root = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone())
        .ok_or_else(|| {
            ProfileError::new(
                "no_project",
                "open a project first: the active profile is per project",
            )
        })?;
    let path = project_profile_path(&root);
    if profile.is_full() {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(ProfileError::new(
                    "profile_io",
                    format!("cannot remove {}: {e}", path.display()),
                ));
            }
        }
    } else {
        write_profile_file(&path, &profile)?;
    }
    let generation = world
        .get_resource::<ActiveFeatureProfile>()
        .map_or(1, |a| a.generation + 1);
    world.insert_resource(ActiveFeatureProfile {
        profile,
        root: Some(root),
        generation,
    });
    Ok(())
}

/// The View ▸ Feature profile menu: the built-in and saved profiles (pick one to activate it).
/// Returns a status line.
pub fn profile_menu(ui: &mut egui::Ui, world: &mut World) -> Option<String> {
    let active = world
        .get_resource::<ActiveFeatureProfile>()
        .map_or_else(|| "Full".to_owned(), |a| a.profile.name.clone());
    let mut picked = None;
    for name in BUILT_IN_PROFILES {
        // `editor.feature_profile_set { name }` (`editor.menu_list` lists each).
        if crate::agent_parity::action_enabled(
            ui,
            active != *name,
            &crate::agent_parity::VIEW_FEATURE_PROFILE,
            Some((*name).to_owned()),
        )
        .clicked()
        {
            picked = FeatureProfile::built_in(name).map(Ok);
        }
    }
    if let Some(dir) = profile_dir_of(world) {
        let saved = saved_profiles(&dir);
        if !saved.is_empty() {
            ui.separator();
        }
        for (name, path) in saved {
            // `editor.feature_profile_set { name }`.
            if ui.selectable_label(active == name, name.as_str()).clicked() {
                picked = Some(read_profile_file(&path).and_then(|p| {
                    p.ok_or_else(|| ProfileError::new("unknown_profile", format!("{name} is gone")))
                }));
            }
        }
    }
    let picked = picked?;
    ui.close();
    Some(
        match picked.and_then(|p| {
            let name = p.name.clone();
            activate(world, p).map(|()| name)
        }) {
            Ok(name) => format!("Feature profile: {name}"),
            Err(err) => format!("Feature profile not changed: {err}"),
        },
    )
}

// ---------------------------------------------------------------------------------------------
// Agent methods

type Handler = fn(&mut World, &Value) -> Result<Value, RpcError>;

fn rpc(err: &ProfileError) -> RpcError {
    RpcError::new(INVALID_PARAMS, err.code, err.message.clone(), "")
}

fn profile_reply(world: &mut World) -> Value {
    sync(world);
    let active = world
        .get_resource::<ActiveFeatureProfile>()
        .map_or_else(FeatureProfile::full, |a| a.profile.clone());
    let hidden_panels: Vec<&str> = crate::editor_layout::BUILT_IN_PANELS
        .iter()
        .filter(|p| active.hides_panel(**p))
        .map(|p| p.id())
        .collect();
    let hidden_kinds: Vec<&str> = crate::shapes::create_kinds()
        .into_iter()
        .filter(|k| active.hides_kind(k))
        .collect();
    let saved: Vec<String> = profile_dir_of(world)
        .map(|d| saved_profiles(&d).into_iter().map(|(n, _)| n).collect())
        .unwrap_or_default();
    json!({
        "active": serde_json::to_value(&active).unwrap_or(Value::Null),
        "hidden_panels": hidden_panels,
        "hidden_kinds": hidden_kinds,
        "built_in": BUILT_IN_PROFILES,
        "saved": saved,
        "areas": FEATURE_AREAS.iter().map(|a| a.id()).collect::<Vec<_>>(),
        "format_version": PROFILE_FORMAT_VERSION,
    })
}

/// `editor.feature_profile_get`.
fn feature_profile_get(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    Ok(profile_reply(world))
}

/// `editor.feature_profile_set { name | profile, save_as? }`.
fn feature_profile_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mut unknown = Vec::new();
    let mut profile = if let Some(value) = params.get("profile").filter(|v| !v.is_null()) {
        let mut profile: FeatureProfile = serde_json::from_value(value.clone()).map_err(|e| {
            RpcError::new(
                INVALID_PARAMS,
                "profile_parse",
                e.to_string(),
                "the `active` of editor.feature_profile_get",
            )
        })?;
        profile.format_version = PROFILE_FORMAT_VERSION;
        unknown = profile.check().map_err(|e| rpc(&e))?;
        profile
    } else if let Some(name) = params.get("name").and_then(Value::as_str) {
        match FeatureProfile::built_in(name) {
            Some(profile) => profile,
            None => {
                let path = profile_dir_of(world)
                    .map(|d| d.join(format!("{name}{PROFILE_FILE_SUFFIX}")))
                    .filter(|p| p.is_file())
                    .ok_or_else(|| {
                        RpcError::new(
                            INVALID_PARAMS,
                            "unknown_profile",
                            format!("no profile is called {name:?}"),
                            "built in: Full, UI only; saved ones from editor.feature_profile_get",
                        )
                    })?;
                read_profile_file(&path).map_err(|e| rpc(&e))?.ok_or_else(|| {
                    RpcError::new(INVALID_PARAMS, "unknown_profile", "the profile file is gone", "")
                })?
            }
        }
    } else {
        return Err(RpcError::new(
            INVALID_PARAMS,
            "bad_profile",
            "pass `name` or `profile`",
            "",
        ));
    };
    let mut saved_to = Value::Null;
    if let Some(save_as) = params.get("save_as").and_then(Value::as_str) {
        let name = crate::workspaces::check_layout_name(save_as)
            .map_err(|e| RpcError::new(INVALID_PARAMS, "bad_profile", e.message, ""))?;
        let dir = profile_dir_of(world).ok_or_else(|| {
            RpcError::new(
                INVALID_PARAMS,
                "no_user_config",
                "this editor keeps no per-user files",
                "",
            )
        })?;
        std::fs::create_dir_all(&dir)
            .map_err(|e| RpcError::new(INVALID_PARAMS, "profile_io", e.to_string(), ""))?;
        profile.name.clone_from(&name);
        let path = dir.join(format!("{name}{PROFILE_FILE_SUFFIX}"));
        write_profile_file(&path, &profile).map_err(|e| rpc(&e))?;
        saved_to = json!(path.display().to_string());
    }
    activate(world, profile).map_err(|e| rpc(&e))?;
    let mut out = profile_reply(world);
    out["unknown"] = json!(unknown);
    out["saved_to"] = saved_to;
    Ok(out)
}

/// This module's agent methods, installed by `agent_ext::all_methods`.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.feature_profile_get",
                tool: "feature_profile_get",
                description: "The open project's active editor feature profile (which feature areas, panels and Create kinds it hides), the panels and kinds hidden as a result, and the built-in and saved profiles.",
                params: &[],
                result: "{ active: { format_version, name, hidden_areas, hidden_panels, hidden_kinds }, hidden_panels, hidden_kinds, built_in, saved, areas, format_version }",
                mutating: false,
                permission: Permission::Read,
            },
            feature_profile_get as Handler,
        ),
        (
            MethodSchema {
                name: "editor.feature_profile_set",
                tool: "feature_profile_set",
                description: "Make a feature profile the open project's active one (saved in .regia/editor_profile.ron; Full removes it): a built-in or saved one by name, or a profile object; save_as also saves it to the user's profiles to share.",
                params: &[
                    ParamSchema {
                        name: "name",
                        kind: "string",
                        required: false,
                        description: "\"Full\", \"UI only\" or a saved profile's name.",
                    },
                    ParamSchema {
                        name: "profile",
                        kind: "object",
                        required: false,
                        description: "{ name, hidden_areas: [\"3d\", \"2d\", \"animation\", \"ui\", \"networking\", \"terrain\", \"procgen\", \"audio\", \"scripting\"], hidden_panels: [panel ids], hidden_kinds: [Create kinds] }.",
                    },
                    ParamSchema {
                        name: "save_as",
                        kind: "string",
                        required: false,
                        description: "Also save the profile as <user config>/profiles/<save_as>.editor_profile.ron.",
                    },
                ],
                result: "the editor.feature_profile_get result plus unknown (ids kept but not known here) and saved_to",
                mutating: true,
                permission: Permission::EditScene,
            },
            feature_profile_set as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn ui_only_hides_every_area_but_ui_and_keeps_the_core_panels() {
        let profile = FeatureProfile::ui_only();
        for panel in crate::editor_layout::BUILT_IN_PANELS {
            let expected = panel_area(*panel).is_some_and(|a| a != FeatureArea::Ui);
            assert_eq!(profile.hides_panel(*panel), expected, "{}", panel.id());
        }
        for panel in [
            Panel::Viewport,
            Panel::Outliner,
            Panel::Inspector,
            Panel::Content,
            Panel::UiDesigner,
        ] {
            assert!(!profile.hides_panel(panel), "{}", panel.id());
        }
        for kind in crate::shapes::create_kinds() {
            assert!(profile.hides_kind(kind), "{kind}");
            assert!(!FeatureProfile::full().hides_kind(kind));
        }
    }

    #[test]
    fn profile_files_round_trip_and_refuse_newer_ones() {
        let mut profile = FeatureProfile::ui_only();
        profile.hidden_panels = vec!["tests".into(), "tests".into(), "panel_from_the_future".into()];
        let unknown = profile.check().unwrap();
        assert_eq!(profile.hidden_panels, ["tests", "panel_from_the_future"]);
        assert_eq!(unknown, ["panel panel_from_the_future"]);
        let back = FeatureProfile::from_text(&profile.to_text()).unwrap();
        assert_eq!(back, profile);
        assert!(back.hides_panel(Panel::Tests));
        let newer = profile
            .to_text()
            .replace("format_version: 1", "format_version: 9");
        assert_eq!(
            FeatureProfile::from_text(&newer).unwrap_err().code,
            "profile_newer"
        );
        assert_eq!(
            FeatureProfile::from_text("(oops").unwrap_err().code,
            "profile_parse"
        );
        assert_eq!(
            FeatureProfile::from_text("(name: \"x\", hidden_areas: [\"4d\"])")
                .unwrap_err()
                .code,
            "profile_parse"
        );
        for area in FEATURE_AREAS {
            assert_eq!(FeatureArea::from_id(area.id()), Some(*area));
        }
    }
}
