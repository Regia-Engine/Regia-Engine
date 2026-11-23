// SPDX-License-Identifier: MPL-2.0
//! The extension registry (`specs/57` § 3): the resolved set, every registration by kind, build
//! states, content mounts, feature packs, events, and the plugin that applies it all at startup.
//!
//! [`ExtensionRegistry`] is a resource. [`ExtensionPlugin`] resolves the project's extensions
//! (or takes manifests it is given), checks the statically linked extension list against the
//! resolved set (`manifest_not_linked`, `linked_not_enabled`), mounts manifest content roots, and
//! calls each active extension's [`RegiaExtension::build`] in the resolved order, then `finish`.
//! A failing build removes that extension's registrations and marks it `failed`; the others
//! continue. `plugin.list`, `plugin.info` and `regia plugin` read the JSON views here
//! ([`ExtensionRegistry::list_json`], [`ExtensionRegistry::info_json`]).
//!
//! Per-frame cost: one exclusive system that returns at once when no event is queued and no pack
//! is active or withdrawing (span `extension_frame`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use semver::Version;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use regia_app::prelude::*;
use regia_asset::AssetServer;
use regia_ecs::ecs::change_detection::Tick;
use regia_ecs::ecs::schedule::ScheduleCleanupPolicy;
use regia_ecs::owner::{OwnerSet, TypeOwner, TypeOwnership};
use regia_ecs::reflect::{AppTypeRegistry, ReflectComponent, std_traits::ReflectDefault};
use regia_render::{FramePass, RuntimeRenderer};
use regia_scene::components::Tags;

use super::context::{
    AgentMethodDesc, AssetLoaderDesc, ContentMount, ExtensionContext, ExtensionError, ExtensionSet,
    FeaturePackDesc, RegiaExtension, Target,
};
use super::discover::{self, Discovery};
use super::manifest::{ExtensionManifest, IMPLICIT_CAPABILITIES};
use super::project::ExtensionsConfig;
use super::resolver::{
    Diagnostic, DiscoveredManifest, ExtensionState, ResolveInput, ResolvedSet, Severity, resolve,
};

/// Ids an extension may not take when the host passes no list: the protocol's own namespaces
/// that exist in every build. Hosts with the agent pass the full schema list
/// ([`ExtensionPlugin::reserved_ids`]).
pub const DEFAULT_RESERVED_IDS: [&str; 16] = [
    "agent",
    "assets",
    "editor",
    "errors",
    "ext",
    "game",
    "level",
    "plugin",
    "project",
    "regia",
    "render",
    "resources",
    "scene",
    "settings",
    "tools",
    "types",
];

/// Owners of extension code start here, clear of the small numbers hot reload gives game
/// generations.
const OWNER_BASE: u32 = 0x8000_0000;

/// What a registration is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RegistrationKind {
    /// A plugin (`ext/<id>/<name>`).
    Plugin,
    /// A run-time asset loader (by file extension).
    AssetLoader,
    /// An agent method.
    AgentMethod,
    /// The settings resource.
    Settings,
    /// A content mount.
    ContentMount,
    /// A feature pack.
    FeaturePack,
    /// An asset-pipeline importer (editor/tool audience).
    Importer,
    /// An editor panel.
    EditorPanel,
    /// An inspector customization.
    Inspector,
    /// A render pass, by pass name.
    RenderPass,
    /// The extension's [`ExtensionSet`] in a schedule, by schedule name.
    SystemSet,
    /// An editor menu or toolbar action (`<ext>.<action>`, ADR-9850).
    EditorAction,
    /// An editor tool for one asset type (`<ext>.<tool>`, ADR-9850).
    AssetTool,
    /// An editor viewport tool with its own handles (`<ext>.<tool>`, ADR-7791).
    ViewportTool,
    /// A small panel drawn over the editor viewport (`<ext>.<overlay>`, ADR-7791).
    Overlay,
    /// A typed editor setting (`<ext>.<setting>`, ADR-7791).
    EditorSetting,
    /// A before-cook, after-package or before-launch hook (`<ext>.<hook>`, ADR-7791).
    BuildHook,
    /// A named editor workspace (`<ext>.<workspace>`, ADR-8197).
    EditorWorkspace,
}

impl RegistrationKind {
    /// The lowercase kind name `plugin.info` uses.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Plugin => "plugin",
            Self::AssetLoader => "asset_loader",
            Self::AgentMethod => "agent_method",
            Self::Settings => "settings",
            Self::ContentMount => "content_mount",
            Self::FeaturePack => "feature_pack",
            Self::Importer => "importer",
            Self::EditorPanel => "editor_panel",
            Self::Inspector => "inspector",
            Self::RenderPass => "render_pass",
            Self::SystemSet => "system_set",
            Self::EditorAction => "editor_action",
            Self::AssetTool => "asset_tool",
            Self::ViewportTool => "viewport_tool",
            Self::Overlay => "overlay",
            Self::EditorSetting => "editor_setting",
            Self::BuildHook => "build_hook",
            Self::EditorWorkspace => "editor_workspace",
        }
    }
}

/// One recorded registration.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Registration {
    /// The registering extension.
    pub id: String,
    /// Kind.
    pub kind: RegistrationKind,
    /// Name within the kind.
    pub name: String,
}

/// An extension's build outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildState {
    /// Resolved but not built in this host (no matching code entry, or no host build yet).
    NotBuilt,
    /// `build` succeeded.
    Built,
    /// `build` failed or the code was not linked: code and detail.
    Failed(&'static str, String),
}

/// Messages agents and systems can wait on (spec § 3.5).
#[derive(Message, Reflect, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExtensionEvent {
    /// Resolution finished.
    Resolved {
        /// Active extensions.
        count: u32,
        /// Error diagnostics.
        errors: u32,
    },
    /// An extension built.
    Built {
        /// Extension id.
        id: String,
        /// Its phase.
        phase: String,
    },
    /// An extension failed to build or load.
    Failed {
        /// Extension id.
        id: String,
        /// Lowercase code.
        code: String,
    },
    /// The project enabled an extension (takes effect at the next start for code).
    Enabled {
        /// Extension id.
        id: String,
    },
    /// The project disabled an extension.
    Disabled {
        /// Extension id.
        id: String,
    },
    /// A dynamic extension was reloaded.
    Reloaded {
        /// Extension id.
        id: String,
        /// New generation.
        generation: u64,
    },
    /// A feature pack switched on.
    PackActivated {
        /// Pack id.
        pack: String,
    },
    /// A feature pack withdrew its components.
    PackDeactivated {
        /// Pack id.
        pack: String,
    },
}

/// A dynamic extension's library and stamp, for `plugin.info`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DynamicRecord {
    /// The unique copy that was loaded.
    pub library: String,
    /// Stamp fields (name, value).
    pub stamp: Vec<(&'static str, String)>,
}

/// Content mounts (spec § 3.3.7): read-only roots under `ext://<id>/`.
#[derive(Debug, Clone, Default)]
pub struct ContentMounts {
    mounts: Vec<(String, PathBuf, BTreeSet<String>)>,
}

/// Where a content lookup landed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentHit {
    /// The file on disk.
    pub path: PathBuf,
    /// `project` or `extension:<id>`.
    pub origin: String,
}

fn list_files(root: &Path) -> Result<BTreeSet<String>, String> {
    let mut out = BTreeSet::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            match entry.file_type() {
                Ok(t) if t.is_dir() => stack.push(path),
                Ok(t) if t.is_file() => {
                    if let Ok(rel) = path.strip_prefix(root) {
                        out.insert(rel.to_string_lossy().replace('\\', "/"));
                    }
                }
                _ => {}
            }
        }
    }
    Ok(out)
}

/// `dir/<rel>`, a content root of the extension in `dir`, when it really lies inside `dir`. Mounts
/// and cooks go through this: a manifest is data, and a `content` that is a symlink to somewhere
/// else must not read that place, even for an extension with no code and no grant.
///
/// # Errors
///
/// A message when the root leads out of `dir`.
pub fn content_root(dir: &Path, rel: &str) -> Result<PathBuf, String> {
    let root = dir.join(rel);
    regia_core::project_path::check_under_root(dir, &root).map_err(|e| {
        format!(
            "content root `{rel}` leaves the extension directory ({})",
            e.code()
        )
    })?;
    Ok(root)
}

impl ContentMounts {
    /// Mounts `root` for `id`; paths an earlier mount of another extension provides stay with it.
    /// Mounting the same root again refreshes it. An extension may hold several roots.
    ///
    /// # Errors
    ///
    /// The directory cannot be read.
    pub fn mount(&mut self, id: &str, root: &Path) -> Result<ContentMount, String> {
        // Canonical, so the read-time check in `resolve` compares against what was mounted.
        let root = root
            .canonicalize()
            .map_err(|e| format!("{}: {e}", root.display()))?;
        let root = root.as_path();
        self.mounts
            .retain(|(owner, mounted, _)| !(owner == id && mounted == root));
        let mut files = list_files(root)?;
        let mut collisions = Vec::new();
        for (other, _, theirs) in &self.mounts {
            if other == id {
                continue;
            }
            for f in theirs {
                if files.remove(f) {
                    collisions.push(f.clone());
                }
            }
        }
        collisions.sort();
        let mount = ContentMount {
            id: id.to_owned(),
            root: root.to_path_buf(),
            files: files.len(),
            collisions,
        };
        self.mounts.push((id.to_owned(), root.to_path_buf(), files));
        Ok(mount)
    }

    /// Removes every mount of `id` (live unmount of a content extension or a reload).
    pub fn unmount(&mut self, id: &str) -> usize {
        let before = self.mounts.len();
        self.mounts.retain(|(m, _, _)| m != id);
        before - self.mounts.len()
    }

    /// Looks up `path`: `ext://<id>/<rest>` or a plain asset path. The project's own
    /// `project_assets/<rest>` shadows extension content; extensions never shadow each other.
    #[must_use]
    pub fn resolve(&self, path: &str, project_assets: Option<&Path>) -> Option<ContentHit> {
        let (only, rest) = match path.strip_prefix("ext://") {
            Some(tail) => {
                let (id, rest) = tail.split_once('/')?;
                (Some(id), rest)
            }
            None => (None, path),
        };
        if regia_core::project_path::check_relative(rest).is_err() {
            return None;
        }
        if let Some(assets) = project_assets {
            let p = assets.join(rest);
            if p.is_file() {
                return Some(ContentHit {
                    path: p,
                    origin: "project".into(),
                });
            }
        }
        self.mounts
            .iter()
            .filter(|(id, _, _)| only.is_none_or(|o| o == id))
            // A file swapped for a link after the mount listed it stays out too.
            .find(|(_, root, files)| {
                files.contains(rest)
                    && regia_core::project_path::check_under_root(root, &root.join(rest)).is_ok()
            })
            .map(|(id, root, _)| ContentHit {
                path: root.join(rest),
                origin: format!("extension:{id}"),
            })
    }

    /// `(id, root, file count)` of every mount.
    #[must_use]
    pub fn list(&self) -> Vec<(String, PathBuf, usize)> {
        self.mounts
            .iter()
            .map(|(id, root, files)| (id.clone(), root.clone(), files.len()))
            .collect()
    }
}

/// The level features a level switches on (spec § 3.3.8), a reflected component on any entity of
/// the level: it loads and unloads with the level, so two overlapping levels hold a feature until
/// both are gone. Saved in `.level.ron` like any component, which needs no level format change.
#[derive(Component, Reflect, Default, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[reflect(Component, Default)]
pub struct LevelFeatures {
    /// Feature names, as `FeaturePackDesc::requires_level_feature` spells them.
    pub features: Vec<String>,
}

/// Life-cycle state of a feature pack (spec § 3.3.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackState {
    /// Registered, level feature off.
    Declared,
    /// Components on tagged entities; systems run.
    Active,
    /// One frame: components removed.
    Withdrawing,
}

impl PackState {
    /// Lowercase name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Declared => "declared",
            Self::Active => "active",
            Self::Withdrawing => "withdrawing",
        }
    }
}

#[derive(Debug, Clone)]
struct PackEntry {
    owner: String,
    desc: FeaturePackDesc,
    state: PackState,
    /// Entities touched, with the type paths the pack added (pre-existing ones are not listed).
    applied: BTreeMap<Entity, Vec<String>>,
}

/// Feature packs and level-feature reference counts.
#[derive(Debug, Clone, Default)]
pub struct FeaturePacks {
    packs: BTreeMap<String, PackEntry>,
    /// Level feature -> owners (levels) that enable it.
    features: BTreeMap<String, BTreeSet<String>>,
    /// `(feature, owner)` holds taken from [`LevelFeatures`] components, released when the
    /// component goes away.
    from_levels: BTreeSet<(String, String)>,
    /// Times an active pack walked every [`Tags`] entity to apply or withdraw components.
    tag_scans: u64,
    /// World change tick of the last tag scan. `0` means a scan has not run.
    tags_accounted: u32,
}

impl FeaturePacks {
    pub(crate) fn declare(&mut self, owner: &str, desc: FeaturePackDesc) {
        self.packs.insert(
            desc.id.clone(),
            PackEntry {
                owner: owner.to_owned(),
                desc,
                state: PackState::Declared,
                applied: BTreeMap::new(),
            },
        );
    }

    /// The state of pack `id`.
    #[must_use]
    pub fn state(&self, id: &str) -> Option<PackState> {
        self.packs.get(id).map(|p| p.state)
    }

    /// Entities pack `id` currently touches.
    #[must_use]
    pub fn applied_count(&self, id: &str) -> usize {
        self.packs.get(id).map_or(0, |p| p.applied.len())
    }

    /// Times feature packs walked every [`Tags`] entity. An idle frame, with no tag insert,
    /// edit or removal, does not move this.
    #[must_use]
    pub fn tag_scans(&self) -> u64 {
        self.tag_scans
    }

    fn wanted(&self, desc: &FeaturePackDesc) -> bool {
        self.features
            .get(&desc.requires_level_feature)
            .is_some_and(|o| !o.is_empty())
    }

    fn busy(&self) -> bool {
        self.packs
            .values()
            .any(|p| p.state != PackState::Declared || self.wanted(&p.desc))
    }

    /// True when no [`Tags`] column changed and no `Tags` component was removed since the last
    /// scan. A missing summary tick is not quiet: the walk is the safe fallback.
    fn tags_quiet(&self, world: &World) -> bool {
        let this_run = world.read_change_tick();
        if world.removed::<Tags>().next().is_some() && self.tags_accounted != this_run.get() {
            return false;
        }
        let Some(id) = world.component_id::<Tags>() else {
            return true;
        };
        for table in world.storages().tables.iter() {
            if !table.has_column(id) {
                continue;
            }
            match table.get_summary_tick(id) {
                Some(summary)
                    if summary
                        .get()
                        .is_newer_than(Tick::new(self.tags_accounted), this_run) =>
                {
                    return false;
                }
                Some(_) => {}
                None => return false,
            }
        }
        true
    }

    /// Takes and releases holds so they match the `(feature, owner)` pairs levels ask for now.
    fn sync_levels(&mut self, wanted: BTreeSet<(String, String)>) {
        for (feature, owner) in self.from_levels.difference(&wanted) {
            if let Some(owners) = self.features.get_mut(feature) {
                owners.remove(owner);
                if owners.is_empty() {
                    self.features.remove(feature);
                }
            }
        }
        for (feature, owner) in wanted.difference(&self.from_levels) {
            self.features
                .entry(feature.clone())
                .or_default()
                .insert(owner.clone());
        }
        self.from_levels = wanted;
    }
}

/// Snapshot of one extension's registrations, taken before a reload and restored on failure.
pub struct RegistrationSnapshot {
    id: String,
    registrations: Vec<Registration>,
    agent_methods: Vec<(String, (String, AgentMethodDesc))>,
    loaders: Vec<(String, Arc<dyn AssetLoaderDesc>)>,
    settings: Option<String>,
    settings_access: Option<SettingsAccess>,
    mounts: Vec<(String, PathBuf, BTreeSet<String>)>,
    packs: Vec<(String, PackEntry)>,
    schedules: Vec<InternedScheduleLabel>,
    pending_passes: Vec<(String, Box<dyn FramePass>)>,
}

impl RegistrationSnapshot {
    /// Counts by kind name.
    #[must_use]
    pub fn counts(&self) -> BTreeMap<&'static str, usize> {
        count_kinds(self.registrations.iter())
    }
}

fn count_kinds<'a>(it: impl Iterator<Item = &'a Registration>) -> BTreeMap<&'static str, usize> {
    let mut out = BTreeMap::new();
    for r in it {
        *out.entry(r.kind.name()).or_insert(0) += 1;
    }
    out
}

/// A change to a schedule that waits for the frame boundary: the schedule running right now is out
/// of `Schedules`, so a change made from inside a frame could land on nothing. Holds a closure
/// because bevy's `ScheduleConfigs` is not `Send`.
pub(crate) enum ScheduleOp {
    /// Removes `owner`'s systems from `label`.
    Remove {
        label: InternedScheduleLabel,
        owner: TypeOwner,
    },
    /// Applies `add` to `label`.
    Add {
        label: InternedScheduleLabel,
        add: Box<dyn FnOnce(&mut Schedule) + Send + Sync>,
    },
}

impl ScheduleOp {
    fn label(&self) -> InternedScheduleLabel {
        match self {
            Self::Remove { label, .. } | Self::Add { label, .. } => *label,
        }
    }
}

/// What a game links, for a process with no project on disk: a packaged game has neither
/// `Regia.toml` nor manifests, so `regia plugin sync` compiles them in (`register_extensions!`).
#[derive(Clone, Default)]
pub struct LinkedExtensions {
    /// The statically linked extensions.
    pub extensions: Vec<Arc<dyn RegiaExtension>>,
    /// Manifest text of every extension of the set, linked or content-only.
    pub manifests: Vec<&'static str>,
    /// The project's `[extensions]` table as `Regia.toml` text.
    pub config: &'static str,
}

impl LinkedExtensions {
    /// The registry inputs these stand for. A manifest that does not parse is a diagnostic.
    #[must_use]
    pub fn inputs(&self) -> RegistryInputs {
        let mut discovery = Discovery::default();
        for text in &self.manifests {
            match ExtensionManifest::parse(text) {
                Ok(manifest) => discovery.manifests.push(DiscoveredManifest {
                    manifest,
                    path: String::new(),
                    hash: discover::hash_hex(text.as_bytes()),
                }),
                Err(error) => discovery.diagnostics.push(Diagnostic {
                    id: "project".into(),
                    code: error.code(),
                    severity: Severity::Error,
                    message: error.to_string(),
                }),
            }
        }
        let config = match ExtensionsConfig::from_project_toml(self.config) {
            Ok(config) => config,
            Err(error) => {
                discovery.diagnostics.push(Diagnostic {
                    id: "project".into(),
                    code: "manifest_invalid",
                    severity: Severity::Error,
                    message: format!("linked [extensions] table: {error}"),
                });
                ExtensionsConfig::default()
            }
        };
        RegistryInputs::for_host(None, config, discovery)
    }
}

/// Inputs of [`ExtensionRegistry::resolve_from`].
#[derive(Debug, Clone)]
pub struct RegistryInputs {
    /// The project directory (`None` in tests and tools without a project).
    pub project_dir: Option<PathBuf>,
    /// The `[extensions]` table.
    pub config: ExtensionsConfig,
    /// What discovery found.
    pub discovery: Discovery,
    /// The running engine version.
    pub engine_version: Version,
    /// `linux`, `windows`, `android` or `macos`.
    pub platform: String,
    /// `dev` or `shipping`.
    pub profile: String,
    /// `game`, `editor`, `server` or `tool`.
    pub audience: String,
    /// Whether the platform can load code at run time.
    pub dynamic_code: bool,
    /// Ids that may not be used.
    pub reserved_ids: Vec<String>,
}

impl RegistryInputs {
    /// Inputs for this process: platform from the compile target, `dev` unless built without
    /// debug assertions, audience `game`, the engine version of this build.
    #[must_use]
    pub fn for_host(project_dir: Option<PathBuf>, config: ExtensionsConfig, discovery: Discovery) -> Self {
        Self {
            project_dir,
            config,
            discovery,
            engine_version: engine_version(),
            platform: host_platform().to_owned(),
            profile: if cfg!(debug_assertions) { "dev" } else { "shipping" }.to_owned(),
            audience: "game".to_owned(),
            dynamic_code: !cfg!(target_os = "android"),
            reserved_ids: DEFAULT_RESERVED_IDS.iter().map(|s| (*s).to_owned()).collect(),
        }
    }
}

/// The engine version extensions are checked against (the framework crate's version).
#[must_use]
pub fn engine_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).unwrap_or_else(|_| Version::new(0, 0, 0))
}

/// `linux`, `windows`, `android` or `macos` for the compile target.
#[must_use]
pub fn host_platform() -> &'static str {
    if cfg!(target_os = "android") {
        "android"
    } else if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    }
}

/// The callback [`SettingsAccess::edit`] runs on an extension's settings resource.
pub type SettingsEditFn<'f> = &'f mut dyn FnMut(&mut dyn Reflect);

/// Reflected access to one extension's settings resource (spec 57 § 3.3.5), recorded by
/// `ExtensionContext::declare_settings` so `settings.get` / `settings.set` can reach it as
/// `<id>.<field>` without knowing its Rust type, and so its value can be saved to and restored
/// from the user settings file (`GameSettings::extensions`, ADR-7237).
#[derive(Debug, Clone, Copy)]
pub struct SettingsAccess {
    /// The resource, if it is in the world.
    pub get: fn(&World) -> Option<&dyn Reflect>,
    /// Runs the callback on the resource (re-inserted, so it reads as added and changed); `false`
    /// when it is absent. Validate and answer dry runs through `get`; call this only for a real
    /// write (ADR-5936).
    pub edit: for<'f> fn(&mut World, SettingsEditFn<'f>) -> bool,
    /// The resource as RON text (what `GameSettings::extensions` stores); `None` when it is absent
    /// or does not serialize.
    pub save: fn(&World) -> Option<String>,
    /// Replaces the resource with the value parsed from RON text; the parse error otherwise (the
    /// resource is then left as it was).
    pub restore: fn(&mut World, &str) -> Result<(), String>,
    /// The tick the resource last changed at (`None` when it is absent), for change detection
    /// outside a system.
    pub changed_tick: fn(&World) -> Option<u32>,
}

impl SettingsAccess {
    /// The access for resource type `T`.
    #[must_use]
    pub fn of<T: Resource + Reflect + Serialize + serde::de::DeserializeOwned>() -> Self {
        fn get<T: Resource + Reflect>(world: &World) -> Option<&dyn Reflect> {
            world.get_resource::<T>().map(|r| r as &dyn Reflect)
        }
        // Taken out and put back rather than `get_resource_mut`, which needs a `Mutable`
        // resource bound that `declare_settings` does not ask for; the insert marks it changed.
        fn edit<T: Resource + Reflect>(world: &mut World, f: SettingsEditFn<'_>) -> bool {
            match world.remove_resource::<T>() {
                Some(mut r) => {
                    f(&mut r);
                    world.insert_resource(r);
                    true
                }
                None => false,
            }
        }
        fn save<T: Resource + Serialize>(world: &World) -> Option<String> {
            ron::to_string(world.get_resource::<T>()?).ok()
        }
        fn restore<T: Resource + serde::de::DeserializeOwned>(
            world: &mut World,
            text: &str,
        ) -> Result<(), String> {
            let value: T = ron::from_str(text).map_err(|e| e.to_string())?;
            world.insert_resource(value);
            Ok(())
        }
        fn changed_tick<T: Resource>(world: &World) -> Option<u32> {
            world.get_resource_change_ticks::<T>().map(|t| t.changed.get())
        }
        Self {
            get: get::<T>,
            edit: edit::<T>,
            save: save::<T>,
            restore: restore::<T>,
            changed_tick: changed_tick::<T>,
        }
    }
}

/// `Update`, before the settings apply: keeps `GameSettings::extensions` in step with the declared
/// extension settings resources (spec 57 § 3.3.5, ADR-7237). The first time it sees a resource it
/// applies a saved value that differs (when the extension was built before the settings plugin
/// loaded the file) and takes the result as the baseline; after that, a resource whose change tick
/// moved is saved as RON text, and the settings plugin persists the changed `GameSettings` the
/// same frame. Nothing runs without `GameSettings` or without declared settings.
pub fn persist_extension_settings(world: &mut World, mut seen: Local<BTreeMap<String, u32>>) {
    let Some(registry) = world.get_resource::<ExtensionRegistry>() else {
        return;
    };
    if registry.settings_access.is_empty() || !world.contains_resource::<crate::settings::GameSettings>() {
        return;
    }
    let mut due: Vec<(String, SettingsAccess, bool)> = Vec::new();
    for (id, access) in &registry.settings_access {
        let Some(tick) = (access.changed_tick)(world) else {
            continue;
        };
        match seen.get(id) {
            Some(last) if *last == tick => {}
            Some(_) => due.push((id.clone(), *access, false)),
            None => due.push((id.clone(), *access, true)),
        }
    }
    if due.is_empty() {
        return;
    }
    let _span = tracing::debug_span!("extension_settings_persist").entered();
    for (id, access, first) in due {
        let saved = world
            .resource::<crate::settings::GameSettings>()
            .extensions
            .get(&id)
            .cloned();
        let current = (access.save)(world);
        if first {
            if let Some(saved) = saved.filter(|saved| current.as_ref() != Some(saved))
                && let Err(why) = (access.restore)(world, &saved)
            {
                tracing::warn!(
                    target: "regia_framework",
                    "the saved settings of extension `{id}` do not fit its settings type ({why}); keeping Regia.toml's"
                );
            }
        } else if let Some(text) = current
            && saved.as_ref() != Some(&text)
        {
            world
                .resource_mut::<crate::settings::GameSettings>()
                .extensions
                .insert(id.clone(), text);
        }
        if let Some(tick) = (access.changed_tick)(world) {
            seen.insert(id, tick);
        }
    }
}

/// The extension registry resource.
#[derive(Resource)]
pub struct ExtensionRegistry {
    inputs: RegistryInputs,
    resolved: ResolvedSet,
    registrations: Vec<Registration>,
    pub(crate) agent_methods: BTreeMap<String, (String, AgentMethodDesc)>,
    pub(crate) loaders: BTreeMap<String, Arc<dyn AssetLoaderDesc>>,
    /// Signature of the loaders last installed into the asset server (0 = none).
    content_hook_sig: u64,
    pub(crate) settings: BTreeMap<String, String>,
    pub(crate) settings_access: BTreeMap<String, SettingsAccess>,
    pub(crate) mounts: ContentMounts,
    pub(crate) packs: FeaturePacks,
    built: BTreeMap<String, BuildState>,
    extra: Vec<Diagnostic>,
    generation: BTreeMap<String, u64>,
    dynamic: BTreeMap<String, DynamicRecord>,
    linked: BTreeSet<String>,
    events: Vec<ExtensionEvent>,
    restart_required: BTreeSet<String>,
    /// Extension id -> the [`TypeOwner::Game`] number its code registers under.
    owners: BTreeMap<String, u32>,
    /// Schedules each extension has systems in.
    schedules: BTreeMap<String, Vec<InternedScheduleLabel>>,
    /// `(first, then)` system order between extensions: dependency edges and `[extensions.order]`.
    order_edges: Vec<(String, String)>,
    schedule_ops: Vec<ScheduleOp>,
    pub(crate) pending_passes: Vec<(String, Box<dyn FramePass>)>,
    load_ms: BTreeMap<String, f32>,
    /// A cooked record was compared (or there is none to compare).
    cooked_checked: bool,
}

impl ExtensionRegistry {
    /// Resolves `inputs` (spec § 2.5) into a registry with nothing built yet.
    #[must_use]
    pub fn resolve_from(inputs: RegistryInputs) -> Self {
        let _span =
            tracing::info_span!("extension_resolve", found = inputs.discovery.manifests.len()).entered();
        let (enabled, disabled) = inputs.config.effective_lists(&inputs.platform);
        let grants = inputs.config.grant_map();
        let resolved = resolve(&ResolveInput {
            discovered: &inputs.discovery.manifests,
            enabled: &enabled,
            disabled: &disabled,
            grants: &grants,
            engine_version: &inputs.engine_version,
            platform: &inputs.platform,
            profile: &inputs.profile,
            dynamic_code: inputs.dynamic_code,
            reserved_ids: &inputs.reserved_ids,
        });
        let errors = resolved
            .diagnostics
            .iter()
            .chain(&inputs.discovery.diagnostics)
            .filter(|d| d.severity == Severity::Error)
            .count();
        let count = resolved.order.len();
        let mut registry = Self {
            inputs,
            resolved,
            registrations: Vec::new(),
            agent_methods: BTreeMap::new(),
            loaders: BTreeMap::new(),
            content_hook_sig: 0,
            settings: BTreeMap::new(),
            settings_access: BTreeMap::new(),
            mounts: ContentMounts::default(),
            packs: FeaturePacks::default(),
            built: BTreeMap::new(),
            extra: Vec::new(),
            generation: BTreeMap::new(),
            dynamic: BTreeMap::new(),
            linked: BTreeSet::new(),
            events: Vec::new(),
            restart_required: BTreeSet::new(),
            owners: BTreeMap::new(),
            schedules: BTreeMap::new(),
            order_edges: Vec::new(),
            schedule_ops: Vec::new(),
            pending_passes: Vec::new(),
            load_ms: BTreeMap::new(),
            cooked_checked: false,
        };
        registry.events.push(ExtensionEvent::Resolved {
            count: u32::try_from(count).unwrap_or(u32::MAX),
            errors: u32::try_from(errors).unwrap_or(u32::MAX),
        });
        registry
    }

    /// Reads `<project>/Regia.toml`, discovers the project's extensions and resolves them.
    ///
    /// # Errors
    ///
    /// `Regia.toml` exists but its `[extensions]` table is malformed.
    pub fn load_project(project: &Path, engine_dir: Option<&Path>) -> Result<Self, ExtensionError> {
        let config = match std::fs::read_to_string(project.join("Regia.toml")) {
            Ok(text) => {
                ExtensionsConfig::from_project_toml(&text).map_err(|e| ExtensionError::Invalid(e.0))?
            }
            Err(_) => ExtensionsConfig::default(),
        };
        let discovery =
            discover::discover_project(project, &config, engine_dir, Some(&discovery_cache_file(project)));
        Ok(Self::resolve_from(RegistryInputs::for_host(
            Some(project.to_path_buf()),
            config,
            discovery,
        )))
    }

    /// The inputs the set was resolved from.
    #[must_use]
    pub fn inputs(&self) -> &RegistryInputs {
        &self.inputs
    }

    /// The `[extensions]` table.
    #[must_use]
    pub fn config(&self) -> &ExtensionsConfig {
        &self.inputs.config
    }

    /// The resolved set.
    #[must_use]
    pub fn resolved(&self) -> &ResolvedSet {
        &self.resolved
    }

    /// The discovered manifest of `id` (the first of a duplicate).
    #[must_use]
    pub fn discovered(&self, id: &str) -> Option<&DiscoveredManifest> {
        self.inputs
            .discovery
            .manifests
            .iter()
            .find(|d| d.manifest.id == id)
    }

    /// The state `plugin.list` reports: the resolver's, turned `failed` by a failed build.
    #[must_use]
    pub fn state(&self, id: &str) -> Option<ExtensionState> {
        let resolved = self.resolved.states.get(id).copied()?;
        match self.built.get(id) {
            Some(BuildState::Failed(..)) => Some(ExtensionState::Failed),
            _ => Some(resolved),
        }
    }

    /// The build state of `id`.
    #[must_use]
    pub fn build_state(&self, id: &str) -> BuildState {
        self.built.get(id).cloned().unwrap_or(BuildState::NotBuilt)
    }

    /// Every diagnostic: discovery, resolution, then registration and link findings.
    #[must_use]
    pub fn diagnostics(&self) -> Vec<&Diagnostic> {
        self.inputs
            .discovery
            .diagnostics
            .iter()
            .chain(&self.resolved.diagnostics)
            .chain(&self.extra)
            .collect()
    }

    /// The diagnostic codes for `id`.
    #[must_use]
    pub fn codes_for(&self, id: &str) -> Vec<&'static str> {
        let mut codes: Vec<&'static str> = self
            .diagnostics()
            .into_iter()
            .filter(|d| d.id == id)
            .map(|d| d.code)
            .collect();
        codes.dedup();
        codes
    }

    /// Registrations of `id`, sorted.
    #[must_use]
    pub fn registrations_of(&self, id: &str) -> Vec<&Registration> {
        let mut out: Vec<&Registration> = self.registrations.iter().filter(|r| r.id == id).collect();
        out.sort();
        out
    }

    /// The extension that holds `(kind, name)`.
    #[must_use]
    pub fn owner_of(&self, kind: RegistrationKind, name: &str) -> Option<&str> {
        self.registrations
            .iter()
            .find(|r| r.kind == kind && r.name == name)
            .map(|r| r.id.as_str())
    }

    /// The owner `id`'s code registers under, stable for this registry's life.
    pub(crate) fn owner_for(&mut self, id: &str) -> TypeOwner {
        let next =
            OWNER_BASE.saturating_add(u32::try_from(self.owners.len()).unwrap_or(u32::MAX - OWNER_BASE));
        TypeOwner::Game(*self.owners.entry(id.to_owned()).or_insert(next))
    }

    /// Notes that `id` has systems in `label`.
    pub(crate) fn note_schedule(&mut self, id: &str, label: InternedScheduleLabel) {
        let labels = self.schedules.entry(id.to_owned()).or_default();
        if !labels.contains(&label) {
            labels.push(label);
        }
        let name = format!("{label:?}");
        let known = self
            .registrations
            .iter()
            .any(|r| r.id == id && r.kind == RegistrationKind::SystemSet && r.name == name);
        if !known {
            self.registrations.push(Registration {
                id: id.to_owned(),
                kind: RegistrationKind::SystemSet,
                name,
            });
        }
    }

    pub(crate) fn queue_schedule_op(&mut self, op: ScheduleOp) {
        self.schedule_ops.push(op);
    }

    /// What puts `id`'s systems under its [`ExtensionSet`] in a schedule, ordered after what it
    /// follows.
    pub(crate) fn set_wiring(&mut self, id: &str) -> impl FnOnce(&mut Schedule) + Send + Sync + 'static {
        let owner = self.owner_for(id);
        let set = ExtensionSet(id.to_owned());
        let edges: Vec<(ExtensionSet, ExtensionSet)> = self
            .order_edges
            .iter()
            .filter(|(first, then)| first == id || then == id)
            .map(|(first, then)| (ExtensionSet(first.clone()), ExtensionSet(then.clone())))
            .collect();
        move |schedule: &mut Schedule| {
            schedule.configure_sets(OwnerSet(owner).in_set(set));
            for (first, then) in edges {
                schedule.configure_sets(then.after(first));
            }
        }
    }

    /// The system order between active extensions: dependency edges, then `[extensions.order]`.
    /// An `[extensions.order]` entry that closes a cycle or names an extension outside the set is
    /// reported and left out, so a typo in the project file cannot stop the schedule from building.
    fn compute_order_edges(&mut self) -> Vec<(String, String)> {
        let active: BTreeSet<String> = self.resolved.order.iter().map(|e| e.id.clone()).collect();
        let mut edges: Vec<(String, String)> = Vec::new();
        for id in &active {
            if let Some(found) = self.discovered(id) {
                edges.extend(
                    found
                        .manifest
                        .dependencies
                        .iter()
                        .filter(|dep| active.contains(&dep.id))
                        .map(|dep| (dep.id.clone(), id.clone())),
                );
            }
        }
        let order = self.inputs.config.order.clone();
        for (id, entry) in order {
            let wanted = entry
                .after
                .iter()
                .map(|other| (other.clone(), id.clone()))
                .chain(entry.before.iter().map(|other| (id.clone(), other.clone())));
            for (first, then) in wanted {
                if !active.contains(&first) || !active.contains(&then) {
                    self.warn(
                        "project",
                        "not_found",
                        format!("[extensions.order]: `{first}` before `{then}` names an extension that is not active"),
                    );
                } else if reaches(&edges, &then, &first) {
                    self.error(
                        "project",
                        "dependency_cycle",
                        format!("[extensions.order]: `{first}` before `{then}` closes a cycle; ignored"),
                    );
                } else {
                    edges.push((first, then));
                }
            }
        }
        edges.sort();
        edges.dedup();
        edges
    }

    /// Records `(id, kind, name)`; refuses a name another extension holds.
    ///
    /// # Errors
    ///
    /// `registration_conflict`.
    pub fn claim(&mut self, id: &str, kind: RegistrationKind, name: &str) -> Result<(), ExtensionError> {
        if let Some(owner) = self.owner_of(kind, name) {
            if owner == id && kind == RegistrationKind::ContentMount {
                return Ok(());
            }
            return Err(ExtensionError::RegistrationConflict {
                kind: kind.name(),
                name: name.to_owned(),
                owner: owner.to_owned(),
            });
        }
        self.registrations.push(Registration {
            id: id.to_owned(),
            kind,
            name: name.to_owned(),
        });
        Ok(())
    }

    /// Forgets one registration.
    pub fn release(&mut self, id: &str, kind: RegistrationKind, name: &str) {
        self.registrations
            .retain(|r| !(r.id == id && r.kind == kind && r.name == name));
    }

    pub(crate) fn warn(&mut self, id: &str, code: &'static str, message: String) {
        self.extra.push(Diagnostic {
            id: id.to_owned(),
            code,
            severity: Severity::Warning,
            message,
        });
    }

    /// Reports an extension `filtered` in this host: every code entry names another audience and
    /// it has no content or tools (spec § 2.3, ADR-7237). The resolved order and hash keep it, so
    /// `resolved_hash` does not depend on the host.
    fn filter_for_audience(&mut self, id: &str, audience: &str) {
        self.resolved
            .states
            .insert(id.to_owned(), ExtensionState::Filtered);
        self.extra.push(Diagnostic {
            id: id.to_owned(),
            code: "filtered",
            severity: Severity::Info,
            message: format!("no code entry for the `{audience}` audience and nothing else to load"),
        });
    }

    fn error(&mut self, id: &str, code: &'static str, message: String) {
        self.extra.push(Diagnostic {
            id: id.to_owned(),
            code,
            severity: Severity::Error,
            message,
        });
    }

    /// Requested capabilities that are granted (explicitly or implicitly).
    #[must_use]
    pub fn granted_for(&self, manifest: &ExtensionManifest) -> BTreeSet<String> {
        let grants = self.inputs.config.grant_map();
        let explicit: BTreeSet<&str> = grants
            .get(&manifest.id)
            .map(|g| g.iter().map(String::as_str).collect())
            .unwrap_or_default();
        manifest
            .capabilities
            .iter()
            .filter(|c| IMPLICIT_CAPABILITIES.contains(&c.as_str()) || explicit.contains(c.as_str()))
            .cloned()
            .collect()
    }

    /// The agent method `name` an active extension registered, with its owner.
    #[must_use]
    pub fn agent_method(&self, name: &str) -> Option<(&str, &AgentMethodDesc)> {
        self.agent_methods
            .get(name)
            .filter(|(owner, _)| self.state(owner) == Some(ExtensionState::Active))
            .map(|(owner, m)| (owner.as_str(), m))
    }

    /// Every registered agent method `(owner, desc)`, by name.
    pub fn agent_methods(&self) -> impl Iterator<Item = (&str, &AgentMethodDesc)> {
        self.agent_methods.values().map(|(o, m)| (o.as_str(), m))
    }

    /// For a method name in a discovered extension's namespace whose provider is not active:
    /// the provider id and its state (`method_unavailable`, spec § 5.2).
    #[must_use]
    pub fn unavailable_provider(&self, method: &str) -> Option<(&str, ExtensionState)> {
        let ns = method.split('.').next()?;
        let d = self.discovered(ns)?;
        let state = self.state(ns)?;
        (state != ExtensionState::Active
            || (!d.manifest.code.is_empty() && self.build_state(ns) != BuildState::Built))
            .then_some((d.manifest.id.as_str(), state))
    }

    /// The loader an active extension registered for `file_extension`.
    #[must_use]
    pub fn loader_for(&self, file_extension: &str) -> Option<&Arc<dyn AssetLoaderDesc>> {
        let ext = file_extension.trim_start_matches('.').to_ascii_lowercase();
        let owner = self.owner_of(RegistrationKind::AssetLoader, &ext)?;
        if self.state(owner) != Some(ExtensionState::Active) {
            return None;
        }
        self.loaders.get(&ext)
    }

    /// The content mounts.
    #[must_use]
    pub fn mounts(&self) -> &ContentMounts {
        &self.mounts
    }

    /// The feature packs.
    #[must_use]
    pub fn packs(&self) -> &FeaturePacks {
        &self.packs
    }

    /// Resolves `path` through the mounts and decodes it with the loader an active extension
    /// registered for its file extension. The project's own `project_assets/` shadows mounts.
    ///
    /// # Errors
    ///
    /// A message: no such content, no loader, unreadable file, or the loader's own error.
    pub fn load_content(
        &self,
        path: &str,
        project_assets: Option<&Path>,
    ) -> Result<(ContentHit, Box<dyn Reflect>), String> {
        let hit = self
            .mounts
            .resolve(path, project_assets)
            .ok_or_else(|| format!("no content at `{path}`"))?;
        let file_extension = hit.path.extension().and_then(|e| e.to_str()).unwrap_or_default();
        let loader = self
            .loader_for(file_extension)
            .ok_or_else(|| format!("no extension loads `.{file_extension}` files"))?;
        let bytes = std::fs::read(&hit.path).map_err(|e| format!("{}: {e}", hit.path.display()))?;
        let value = loader.load(&bytes, path)?;
        Ok((hit, value))
    }

    /// Decodes cooked content at `path` (`ext://<id>/<rest>`) with the loader an active extension
    /// registered for its file extension. A packaged game calls this: its content is in the paks,
    /// and [`Self::load_content`] has no mount to read.
    ///
    /// # Errors
    ///
    /// A message: not cooked, no loader, unreadable, or the loader's own error.
    pub fn load_cooked_content(
        &self,
        server: &mut AssetServer,
        path: &str,
    ) -> Result<Box<dyn Reflect>, String> {
        let file_extension = path.rsplit('.').next().unwrap_or_default();
        let loader = self
            .loader_for(file_extension)
            .ok_or_else(|| format!("no extension loads `.{file_extension}` files"))?;
        let id = server
            .id_for_path(path)
            .ok_or_else(|| format!("no cooked content at `{path}`"))?;
        let (bytes, _) = server.read_cooked(id).map_err(|e| format!("{path}: {e}"))?;
        let (_, payload) = regia_asset::decode_cooked_file(&bytes).map_err(|e| format!("{path}: {e}"))?;
        loader.load(payload, path)
    }

    /// The loaders of active extensions, by lowercase file extension.
    fn active_loaders(&self) -> impl Iterator<Item = (&String, &Arc<dyn AssetLoaderDesc>)> {
        self.loaders.iter().filter(|(ext, _)| {
            self.owner_of(RegistrationKind::AssetLoader, ext)
                .is_some_and(|owner| self.state(owner) == Some(ExtensionState::Active))
        })
    }

    /// A number that changes when the set of active loaders (or one of them) does; 0 for none.
    /// Allocation-free, because the frame system compares it every frame.
    fn loader_signature(&self) -> u64 {
        use std::hash::{Hash as _, Hasher as _};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        let mut any = false;
        for (ext, loader) in self.active_loaders() {
            any = true;
            ext.hash(&mut hasher);
            (Arc::as_ptr(loader).cast::<()>() as usize).hash(&mut hasher);
        }
        if any { hasher.finish() | 1 } else { 0 }
    }

    /// The settings resource type path `id` declared.
    #[must_use]
    pub fn settings_type(&self, id: &str) -> Option<&str> {
        self.settings.get(id).map(String::as_str)
    }

    /// Reflected access to the settings resource `id` declared (spec 57 § 3.3.5).
    #[must_use]
    pub fn settings_access(&self, id: &str) -> Option<SettingsAccess> {
        self.settings_access.get(id).copied()
    }

    /// Turns level feature `feature` on for `owner` (a level path). Packs switch at the next frame
    /// boundary; activation is counted per owner, so two levels keep it on until both release it.
    pub fn enable_level_feature(&mut self, feature: &str, owner: &str) {
        self.packs
            .features
            .entry(feature.to_owned())
            .or_default()
            .insert(owner.to_owned());
    }

    /// Releases `owner`'s hold on `feature`.
    pub fn disable_level_feature(&mut self, feature: &str, owner: &str) {
        if let Some(owners) = self.packs.features.get_mut(feature) {
            owners.remove(owner);
            if owners.is_empty() {
                self.packs.features.remove(feature);
            }
        }
    }

    /// Releases every feature `owner` holds (level unload).
    pub fn release_level(&mut self, owner: &str) {
        self.packs.features.retain(|_, owners| {
            owners.remove(owner);
            !owners.is_empty()
        });
    }

    /// The generation of `id` (0 until a dynamic reload).
    #[must_use]
    pub fn generation(&self, id: &str) -> u64 {
        self.generation.get(id).copied().unwrap_or(0)
    }

    /// Records a dynamic load of `id`.
    pub fn record_dynamic(&mut self, id: &str, record: DynamicRecord) {
        self.dynamic.insert(id.to_owned(), record);
    }

    /// The dynamic record of `id`.
    #[must_use]
    pub fn dynamic(&self, id: &str) -> Option<&DynamicRecord> {
        self.dynamic.get(id)
    }

    /// Ids whose code changed since start (restart needed).
    #[must_use]
    pub fn restart_required(&self) -> &BTreeSet<String> {
        &self.restart_required
    }

    /// Notes that the project's lists changed for `id`: queues `Enabled`/`Disabled` and, for a
    /// code extension, marks a restart as needed. Returns whether a restart is needed.
    pub fn note_toggled(&mut self, id: &str, enabled: bool) -> bool {
        let code = self.discovered(id).is_some_and(|d| !d.manifest.code.is_empty());
        self.events.push(if enabled {
            ExtensionEvent::Enabled { id: id.to_owned() }
        } else {
            ExtensionEvent::Disabled { id: id.to_owned() }
        });
        if code {
            self.restart_required.insert(id.to_owned());
        }
        code
    }

    /// Removes everything `id` registered; its systems leave at the frame boundary.
    fn withdraw(&mut self, id: &str) {
        let owner = self.owner_for(id);
        for label in self.take_registrations(id).schedules {
            self.queue_schedule_op(ScheduleOp::Remove { label, owner });
        }
    }

    /// Marks `id` failed with a protocol code (a host-side load failure).
    pub fn mark_failed(&mut self, id: &str, code: &'static str, detail: String) {
        self.error(id, code, detail.clone());
        self.built.insert(id.to_owned(), BuildState::Failed(code, detail));
        self.events.push(ExtensionEvent::Failed {
            id: id.to_owned(),
            code: code.to_owned(),
        });
    }

    /// Compares a cooked `extensions.ron` with the resolved set: a difference is the error
    /// `extension_set_mismatch` (spec § 4.4), an unreadable file is `manifest_invalid`.
    pub fn check_cooked(&mut self, text: &str) {
        self.cooked_checked = true;
        match super::cooked::CookedExtensions::parse(text) {
            Ok(cooked) => {
                if let Some(detail) = cooked.mismatch(&self.resolved, &self.inputs.discovery.manifests) {
                    self.error("project", "extension_set_mismatch", detail);
                }
            }
            Err(error) => self.error("project", "manifest_invalid", error.to_string()),
        }
    }

    /// Mounts content root `rel` of the extension in `dir` while the app runs (`plugin.enable`).
    ///
    /// # Errors
    ///
    /// The root leaves `dir`, or cannot be read.
    pub fn mount_content_live(&mut self, id: &str, dir: &Path, rel: &str) -> Result<ContentMount, String> {
        let root = content_root(dir, rel)?;
        let mount = self.mounts.mount(id, &root)?;
        for path in &mount.collisions {
            self.warn(
                id,
                "content_collision",
                format!("`{path}` is already mounted by another extension"),
            );
        }
        let _ = self.claim(id, RegistrationKind::ContentMount, &format!("ext://{id}/"));
        Ok(mount)
    }

    /// Mounts the `[[content]]` roots of `manifest`, which lives in `dir`. Content mounts for every
    /// audience: the manifest is the declaration.
    pub fn mount_manifest_content(&mut self, manifest: &ExtensionManifest, dir: &Path) {
        // A packaged game has no directory to mount: the cook put its content in the paks.
        if dir.as_os_str().is_empty() {
            return;
        }
        for root in &manifest.content {
            if let Err(error) = content_root(dir, root) {
                self.error(&manifest.id, "path_escape", error);
            } else if let Err(error) = self.mount_content_live(&manifest.id, dir, root) {
                self.warn(
                    &manifest.id,
                    "manifest_invalid",
                    format!("content root `{root}`: {error}"),
                );
            }
        }
    }

    /// For a project opened after startup: mounts its content and marks its active code
    /// extensions as waiting for a restart, because code only builds at startup.
    pub fn activate_without_code(&mut self) {
        let order: Vec<String> = self.resolved.order.iter().map(|e| e.id.clone()).collect();
        for id in order {
            let Some(found) = self.discovered(&id).cloned() else {
                continue;
            };
            self.mount_manifest_content(&found.manifest, Path::new(&found.path));
            if !found.manifest.code.is_empty() {
                self.restart_required.insert(id);
            }
        }
    }

    /// Unmounts a content extension while the app runs (`plugin.disable`). Returns the mounts
    /// removed.
    pub fn unmount_content_live(&mut self, id: &str) -> usize {
        self.release(id, RegistrationKind::ContentMount, &format!("ext://{id}/"));
        self.mounts.unmount(id)
    }

    /// Queues an event for the next frame.
    pub fn push_event(&mut self, event: ExtensionEvent) {
        self.events.push(event);
    }

    /// Removes every registration of `id` and returns them (before a reload or after a failed
    /// build). Systems stay in their schedules (they cannot be removed at run time).
    pub fn take_registrations(&mut self, id: &str) -> RegistrationSnapshot {
        let (mine, rest): (Vec<Registration>, Vec<Registration>) = std::mem::take(&mut self.registrations)
            .into_iter()
            .partition(|r| r.id == id);
        self.registrations = rest;
        let methods: Vec<String> = self
            .agent_methods
            .iter()
            .filter(|(_, (o, _))| o == id)
            .map(|(n, _)| n.clone())
            .collect();
        let agent_methods = methods
            .into_iter()
            .filter_map(|n| self.agent_methods.remove(&n).map(|v| (n, v)))
            .collect();
        let loader_names: Vec<String> = mine
            .iter()
            .filter(|r| r.kind == RegistrationKind::AssetLoader)
            .map(|r| r.name.clone())
            .collect();
        let loaders = loader_names
            .into_iter()
            .filter_map(|n| self.loaders.remove(&n).map(|l| (n, l)))
            .collect();
        let (mounts, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut self.mounts.mounts)
            .into_iter()
            .partition(|(m, _, _)| m == id);
        self.mounts.mounts = keep;
        let pack_ids: Vec<String> = self
            .packs
            .packs
            .iter()
            .filter(|(_, p)| p.owner == id)
            .map(|(k, _)| k.clone())
            .collect();
        let packs = pack_ids
            .into_iter()
            .filter_map(|k| self.packs.packs.remove(&k).map(|p| (k, p)))
            .collect();
        let (pending_passes, keep_passes): (Vec<_>, Vec<_>) = std::mem::take(&mut self.pending_passes)
            .into_iter()
            .partition(|(owner, _)| owner == id);
        self.pending_passes = keep_passes;
        RegistrationSnapshot {
            id: id.to_owned(),
            registrations: mine,
            agent_methods,
            loaders,
            settings: self.settings.remove(id),
            settings_access: self.settings_access.remove(id),
            mounts,
            packs,
            schedules: self.schedules.remove(id).unwrap_or_default(),
            pending_passes,
        }
    }

    /// Puts a snapshot back, dropping anything registered for the id since it was taken.
    pub fn restore_registrations(&mut self, snapshot: RegistrationSnapshot) {
        let _ = self.take_registrations(&snapshot.id);
        self.registrations.extend(snapshot.registrations);
        self.agent_methods.extend(snapshot.agent_methods);
        self.loaders.extend(snapshot.loaders);
        if let Some(s) = snapshot.settings {
            self.settings.insert(snapshot.id.clone(), s);
        }
        if let Some(access) = snapshot.settings_access {
            self.settings_access.insert(snapshot.id.clone(), access);
        }
        self.mounts.mounts.extend(snapshot.mounts);
        self.packs.packs.extend(snapshot.packs);
        self.pending_passes.extend(snapshot.pending_passes);
        if !snapshot.schedules.is_empty() {
            self.schedules.insert(snapshot.id, snapshot.schedules);
        }
    }

    /// Counts of `id`'s registrations by kind.
    #[must_use]
    pub fn counts_of(&self, id: &str) -> BTreeMap<&'static str, usize> {
        count_kinds(self.registrations.iter().filter(|r| r.id == id))
    }

    /// `plugin.list` (spec § 5.1): every discovered extension, optionally only one state.
    #[must_use]
    pub fn list_json(&self, state: Option<&str>) -> Value {
        let extensions: Vec<Value> = self
            .inputs
            .discovery
            .manifests
            .iter()
            .filter(|d| self.discovered(&d.manifest.id).is_some_and(|f| f.path == d.path))
            .filter_map(|d| {
                let m = &d.manifest;
                let st = self.state(&m.id).unwrap_or(ExtensionState::Available);
                if state.is_some_and(|s| s != st.name()) {
                    return None;
                }
                let kind = if self.dynamic.contains_key(&m.id) {
                    "dynamic"
                } else {
                    m.kind()
                };
                Some(json!({
                    "id": m.id, "version": m.version.to_string(), "state": st.name(), "kind": kind,
                    "phase": m.phase().name(), "publisher": m.publisher, "licence": m.licence,
                    "diagnostics": self.codes_for(&m.id), "generation": self.generation(&m.id),
                    "load_ms": self.load_ms.get(&m.id),
                }))
            })
            .collect();
        json!({
            "extensions": extensions,
            "resolved_hash": self.resolved.resolved_hash,
            "order": self.resolved.order.iter().map(|e| e.id.clone()).collect::<Vec<_>>(),
            "restart_required": self.restart_required.iter().collect::<Vec<_>>(),
        })
    }

    /// `plugin.info` (spec § 5.1), or `None` for an unknown id.
    #[must_use]
    pub fn info_json(&self, id: &str) -> Option<Value> {
        let d = self.discovered(id)?;
        let m = &d.manifest;
        let explicit = self.inputs.config.grant_map().remove(id).unwrap_or_default();
        let granted: Vec<Value> = m
            .capabilities
            .iter()
            .filter_map(|c| {
                if IMPLICIT_CAPABILITIES.contains(&c.as_str()) {
                    Some(json!({ "capability": c, "grant": "implicit" }))
                } else if explicit.contains(c) {
                    Some(json!({ "capability": c, "grant": "explicit" }))
                } else {
                    None
                }
            })
            .collect();
        let resolved_version = |dep: &str| self.discovered(dep).map(|f| f.manifest.version.to_string());
        let dependencies: Vec<Value> = m
            .dependencies
            .iter()
            .map(|dep| {
                json!({
                    "id": dep.id, "version": dep.version.to_string(), "optional": dep.optional,
                    "resolved": resolved_version(&dep.id),
                })
            })
            .collect();
        let mut registrations: BTreeMap<&'static str, Vec<String>> = BTreeMap::new();
        for r in self.registrations_of(id) {
            registrations
                .entry(r.kind.name())
                .or_default()
                .push(r.name.clone());
        }
        let packs: Vec<Value> = self
            .packs
            .packs
            .values()
            .filter(|p| p.owner == id)
            .map(|p| {
                json!({
                    "id": p.desc.id, "tag": p.desc.tag, "state": p.state.name(),
                    "level_feature": p.desc.requires_level_feature,
                    "components": p.desc.components.iter().map(|c| c.type_path.clone()).collect::<Vec<_>>(),
                    "entities": p.applied.len(),
                })
            })
            .collect();
        let diagnostics: Vec<Value> = self
            .diagnostics()
            .into_iter()
            .filter(|x| x.id == id)
            .map(|x| json!({ "code": x.code, "severity": format!("{:?}", x.severity).to_lowercase(), "message": x.message }))
            .collect();
        let dynamic = self.dynamic.get(id).map(|r| {
            json!({
                "library": r.library,
                "stamp": r.stamp.iter().map(|(k, v)| ((*k).to_owned(), Value::String(v.clone()))).collect::<serde_json::Map<_, _>>(),
            })
        });
        let build = match self.build_state(id) {
            BuildState::NotBuilt => json!({ "state": "not_built" }),
            BuildState::Built => json!({ "state": "built" }),
            BuildState::Failed(code, detail) => json!({ "state": "failed", "code": code, "detail": detail }),
        };
        let licence_files = std::fs::read_dir(&d.path)
            .map(|rd| {
                let mut v: Vec<String> = rd
                    .filter_map(Result::ok)
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| n.starts_with("LICENSE") || n.starts_with("LICENCE") || n == "NOTICE")
                    .collect();
                v.sort();
                v
            })
            .unwrap_or_default();
        Some(json!({
            "id": m.id, "name": m.name, "version": m.version.to_string(), "publisher": m.publisher,
            "licence": m.licence, "description": m.description, "kind": m.kind(), "phase": m.phase().name(),
            "state": self.state(id).map(ExtensionState::name), "path": d.path, "manifest_hash": d.hash,
            "auto_enable": m.auto_enable, "provides": m.provides, "replaces": m.replaces, "conflicts": m.conflicts,
            "requested": m.capabilities, "granted": granted, "dependencies": dependencies,
            "code": m.code.iter().map(|c| json!({
                "crate": c.crate_name, "audience": c.audience, "phase": c.phase.name(), "library": c.library,
            })).collect::<Vec<_>>(),
            "content": m.content,
            "tools": m.tools.iter().map(|t| json!({
                "name": t.name, "command": t.command, "capabilities": t.capabilities, "timeout_seconds": t.timeout_seconds,
            })).collect::<Vec<_>>(),
            "registrations": registrations, "packs": packs, "diagnostics": diagnostics,
            "build": build, "dynamic": dynamic, "generation": self.generation(id),
            "mounts": self.mounts.list().into_iter().filter(|(owner, _, _)| owner == id).map(|(_, root, files)| json!({
                "root": root.display().to_string(), "files": files, "origin": format!("extension:{id}"),
            })).collect::<Vec<_>>(),
            "settings_type": self.settings.get(id), "licence_files": licence_files,
            "trust": "in-process extension code is trusted native code with the host's full privileges; capabilities are a review aid, not a sandbox",
        }))
    }
}

/// Runs extension code, turning a panic into its message. Extension code is called from everywhere
/// here, and one failing extension must not take the others or the host with it (spec § 2.8).
/// Under `panic = "abort"` nothing unwinds, as for any game code in a shipping build.
fn caught<T>(f: impl FnOnce() -> T) -> Result<T, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).map_err(|payload| {
        payload
            .downcast_ref::<&str>()
            .map(|text| (*text).to_owned())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "a panic with no message".to_owned())
    })
}

/// Runs `ext.build` for one manifest under the extension's owner, recording the outcome. Every
/// system it adds, inside a plugin or not, lands in the owner's set. On failure the extension's
/// registrations are removed and its systems leave at the frame boundary.
fn build_one(
    app: &mut App,
    registry: &mut ExtensionRegistry,
    manifest: &ExtensionManifest,
    dir: PathBuf,
    ext: &dyn RegiaExtension,
) -> Result<(), ExtensionError> {
    let id = manifest.id.clone();
    let _span = tracing::info_span!("extension_build", id = %id).entered();
    let owner = registry.owner_for(&id);
    let previous = app.set_current_owner(owner);
    let started = std::time::Instant::now();
    let result = caught(|| {
        let mut cx = ExtensionContext::new(Target::App(&mut *app), &mut *registry, manifest, dir);
        ext.build(&mut cx)
    })
    .unwrap_or_else(|panic| Err(ExtensionError::BuildFailed(format!("build panicked: {panic}"))));
    app.set_current_owner(previous);
    // Informational (spec § 10); nothing depends on it.
    registry
        .load_ms
        .insert(id.clone(), started.elapsed().as_secs_f32() * 1000.0);
    let labels: Vec<InternedScheduleLabel> = app
        .world()
        .get_resource::<TypeOwnership>()
        .map(|ownership| ownership.systems_of(owner).map(|s| s.schedule).collect())
        .unwrap_or_default();
    for label in labels {
        registry.note_schedule(&id, label);
    }
    match &result {
        Ok(()) => {
            registry.built.insert(id.clone(), BuildState::Built);
            registry.events.push(ExtensionEvent::Built {
                id,
                phase: manifest.phase().name().to_owned(),
            });
        }
        Err(e) => {
            registry.withdraw(&id);
            registry
                .built
                .insert(id.clone(), BuildState::Failed(e.code(), e.to_string()));
            registry.error(&id, e.code(), e.to_string());
            registry.events.push(ExtensionEvent::Failed {
                id,
                code: e.code().to_owned(),
            });
        }
    }
    result
}

/// What a live reload changed (`plugin.reload`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReloadReport {
    /// New generation.
    pub generation: u64,
    /// Registrations removed, by kind.
    pub removed: BTreeMap<&'static str, usize>,
    /// Registrations added, by kind.
    pub added: BTreeMap<&'static str, usize>,
}

/// Takes the passes named in `names` out of the frame graph.
fn park_passes(world: &mut World, names: &[String]) -> Vec<Box<dyn FramePass>> {
    let Some(mut renderer) = world.get_resource_mut::<RuntimeRenderer>() else {
        return Vec::new();
    };
    names.iter().filter_map(|n| renderer.0.remove_pass(n)).collect()
}

/// Hands the queued passes to the frame graph, once there is one. A pass it refuses is dropped
/// with one warning; the frame never fails for it (spec § 3.3.6).
fn flush_passes(world: &mut World, registry: &mut ExtensionRegistry) {
    if registry.pending_passes.is_empty() {
        return;
    }
    let Some(mut renderer) = world.get_resource_mut::<RuntimeRenderer>() else {
        return;
    };
    for (id, pass) in std::mem::take(&mut registry.pending_passes) {
        let name = pass.name().to_owned();
        if let Err(error) = renderer.0.register_pass(pass) {
            tracing::warn!(target: "regia_framework", id, pass = name, %error, "extension render pass refused");
            registry.release(&id, RegistrationKind::RenderPass, &name);
            registry.warn(&id, "render_pass_invalid", error.to_string());
        }
    }
}

/// Moves the state of the packs `old` held onto the copy that replaced them, so what the old copy
/// added stays tracked and comes off at withdrawal. A pack the new copy dropped or changed
/// withdraws now.
fn carry_packs(world: &mut World, registry: &mut ExtensionRegistry, old: Vec<(String, PackEntry)>) {
    let types = world.get_resource::<AppTypeRegistry>().cloned();
    for (id, entry) in old {
        match registry.packs.packs.get_mut(&id) {
            Some(new) if new.desc == entry.desc => {
                new.state = entry.state;
                new.applied = entry.applied;
            }
            _ => {
                if let Some(types) = &types {
                    for (e, added) in entry.applied {
                        for type_path in added {
                            remove_component(world, types, e, &type_path);
                        }
                    }
                }
                if entry.state == PackState::Active {
                    registry.events.push(ExtensionEvent::PackDeactivated { pack: id });
                }
            }
        }
    }
}

/// Swaps `old` for `new` in a running world (spec § 3.4): `on_unload` of the old copy, remove its
/// registrations, `build` the new copy, `on_load` with the saved state. The old copy's systems
/// leave and the new copy's join at the next frame boundary, so behavior changes between frames.
/// A failing build restores the old registrations and answers `reload_failed`; the old systems
/// never left.
///
/// # Errors
///
/// `not_found` for an id without an active manifest; `reload_failed` (wrapping the build error)
/// when the new copy's build fails.
pub fn reload_in_world(
    world: &mut World,
    old: &dyn RegiaExtension,
    new: &dyn RegiaExtension,
) -> Result<ReloadReport, (&'static str, String)> {
    let id = new.id().to_owned();
    let Some(registry) = world.get_resource::<ExtensionRegistry>() else {
        return Err(("not_found", "no extension registry".into()));
    };
    let Some(found) = registry.discovered(&id).cloned() else {
        return Err(("not_found", format!("no extension `{id}`")));
    };
    // Before the registry leaves the world: a panic here changes nothing.
    let saved = caught(|| old.on_unload(world)).map_err(|panic| {
        (
            "reload_failed",
            format!("{id}: on_unload panicked: {panic}; nothing changed"),
        )
    })?;
    let Some(mut registry) = world.remove_resource::<ExtensionRegistry>() else {
        return Err(("not_found", "no extension registry".into()));
    };
    let mut snapshot = registry.take_registrations(&id);
    let removed = snapshot.counts();
    let pass_names: Vec<String> = snapshot
        .registrations
        .iter()
        .filter(|r| r.kind == RegistrationKind::RenderPass)
        .map(|r| r.name.clone())
        .collect();
    let parked = park_passes(world, &pass_names);
    let owner = registry.owner_for(&id);
    let old_schedules = snapshot.schedules.clone();
    let mark = registry.schedule_ops.len();
    let dir = PathBuf::from(&found.path);
    let result = caught(|| {
        let mut cx = ExtensionContext::new(Target::World(&mut *world), &mut registry, &found.manifest, dir);
        new.build(&mut cx)
    })
    .unwrap_or_else(|panic| Err(ExtensionError::BuildFailed(format!("build panicked: {panic}"))));
    let out = match result {
        Ok(()) => {
            let generation = registry.generation(&id) + 1;
            registry.generation.insert(id.clone(), generation);
            registry.built.insert(id.clone(), BuildState::Built);
            registry.events.push(ExtensionEvent::Reloaded {
                id: id.clone(),
                generation,
            });
            let added = registry.counts_of(&id);
            // The old copy leaves before the new one joins; both wait for the frame boundary.
            let removals = old_schedules
                .into_iter()
                .map(|label| ScheduleOp::Remove { label, owner });
            registry.schedule_ops.splice(mark..mark, removals);
            let observers: Vec<Entity> = world
                .get_resource::<TypeOwnership>()
                .map(|o| o.observers_of(owner).map(|x| x.entity).collect())
                .unwrap_or_default();
            for entity in observers {
                if let Ok(entity) = world.get_entity_mut(entity) {
                    entity.despawn();
                }
            }
            if let Some(mut ownership) = world.get_resource_mut::<TypeOwnership>() {
                ownership.forget(owner);
            }
            carry_packs(world, &mut registry, std::mem::take(&mut snapshot.packs));
            flush_passes(world, &mut registry);
            world.insert_resource(registry);
            if let Err(panic) = caught(|| new.on_load(world, saved)) {
                // The swap is done and cannot be undone: the new copy runs without its old state.
                tracing::error!(target: "regia_framework", id, panic, "on_load panicked");
                if let Some(mut registry) = world.get_resource_mut::<ExtensionRegistry>() {
                    registry.warn(&id, "load_failed", format!("on_load panicked: {panic}"));
                }
            }
            // Code changed between two frames: replays and determinism checks of this session
            // are not reproducible from here on (ADR-9850 (b), spec § 7).
            crate::session_determinism::mark_nondeterministic(
                world,
                crate::session_determinism::NONDETERMINISTIC_RELOAD,
                &id,
                format!("generation {generation}"),
            );
            return Ok(ReloadReport {
                generation,
                removed,
                added,
            });
        }
        Err(e) => {
            registry.schedule_ops.truncate(mark);
            registry.restore_registrations(snapshot);
            if let Some(mut renderer) = world.get_resource_mut::<RuntimeRenderer>() {
                for pass in parked {
                    if let Err(error) = renderer.0.register_pass(pass) {
                        tracing::warn!(target: "regia_framework", id, %error, "a parked render pass did not go back");
                    }
                }
            }
            registry.events.push(ExtensionEvent::Failed {
                id: id.clone(),
                code: "reload_failed".into(),
            });
            Err((
                "reload_failed",
                format!("{id}: {e}; the previous registrations were restored"),
            ))
        }
    };
    world.insert_resource(registry);
    // The old copy stays loaded and keeps its state.
    if let Err(panic) = caught(|| old.on_load(world, saved)) {
        tracing::error!(target: "regia_framework", id, panic, "on_load of the old copy panicked");
    }
    out
}

/// Installs the registry, builds the linked extensions in resolved order, and runs pack
/// activation and event delivery at the frame boundary.
#[derive(Clone)]
pub struct ExtensionPlugin {
    /// Where the inputs come from.
    pub source: ExtensionSource,
    /// The statically linked extensions (`register_extensions!`).
    pub linked: Vec<Arc<dyn RegiaExtension>>,
    /// Replaces [`DEFAULT_RESERVED_IDS`] (hosts with the agent pass every protocol namespace).
    pub reserved_ids: Option<Vec<String>>,
    /// Overrides the audience (`game` by default; the editor passes `editor`).
    pub audience: Option<String>,
}

/// Where [`ExtensionPlugin`] gets manifests.
#[derive(Clone)]
pub enum ExtensionSource {
    /// Read `Regia.toml` and discover under the project (and engine) directories.
    Project {
        /// Project directory.
        project: PathBuf,
        /// Engine directory (`<engine>/extensions/`).
        engine: Option<PathBuf>,
    },
    /// Fixed inputs (tests, tools, a cooked set).
    Inputs(Box<RegistryInputs>),
}

impl ExtensionPlugin {
    /// A plugin over prepared inputs.
    #[must_use]
    pub fn from_inputs(inputs: RegistryInputs, linked: Vec<Arc<dyn RegiaExtension>>) -> Self {
        Self {
            source: ExtensionSource::Inputs(Box::new(inputs)),
            linked,
            reserved_ids: None,
            audience: None,
        }
    }

    /// The plugin a game installs: the project on disk when `project` holds a `Regia.toml` (a
    /// development run), else the set compiled into the binary (a packaged game, every Android
    /// build).
    #[must_use]
    pub fn for_game(project: Option<PathBuf>, linked: LinkedExtensions) -> Self {
        match project.filter(|p| p.join("Regia.toml").is_file()) {
            Some(project) => Self::for_project(project, linked.extensions),
            None => Self::from_inputs(linked.inputs(), linked.extensions.clone()),
        }
    }

    /// Also discovers `<engine>/extensions/` after the project's roots, under the override rule
    /// of ADR-9850 (a) (`discover::discover_project`). Only a plugin made by
    /// [`Self::for_project`] (or [`Self::for_game`] with a project) reads it.
    #[must_use]
    pub fn with_engine(mut self, engine_dir: Option<PathBuf>) -> Self {
        if let ExtensionSource::Project { engine, .. } = &mut self.source {
            *engine = engine_dir;
        }
        self
    }

    /// A plugin that discovers the extensions of `project`.
    #[must_use]
    pub fn for_project(project: PathBuf, linked: Vec<Arc<dyn RegiaExtension>>) -> Self {
        Self {
            source: ExtensionSource::Project {
                project,
                engine: None,
            },
            linked,
            reserved_ids: None,
            audience: None,
        }
    }

    /// The registry these inputs resolve to, with nothing built: a host reads it to learn which
    /// libraries to load before [`Plugin::build`] runs.
    #[must_use]
    pub fn resolve_now(&self) -> ExtensionRegistry {
        self.registry()
    }

    fn registry(&self) -> ExtensionRegistry {
        let mut inputs = match &self.source {
            ExtensionSource::Inputs(inputs) => (**inputs).clone(),
            ExtensionSource::Project { project, engine } => {
                let text = std::fs::read_to_string(project.join("Regia.toml")).unwrap_or_default();
                let (config, bad) = match ExtensionsConfig::from_project_toml(&text) {
                    Ok(c) => (c, None),
                    Err(e) => (ExtensionsConfig::default(), Some(e)),
                };
                let mut discovery = discover::discover_project(
                    project,
                    &config,
                    engine.as_deref(),
                    Some(&discovery_cache_file(project)),
                );
                if let Some(e) = bad {
                    discovery.diagnostics.push(Diagnostic {
                        id: "project".into(),
                        code: "manifest_invalid",
                        severity: Severity::Error,
                        message: format!("Regia.toml: {e}"),
                    });
                }
                RegistryInputs::for_host(Some(project.clone()), config, discovery)
            }
        };
        let cooked = inputs.project_dir.as_ref().map(|project| {
            project
                .join("build")
                .join("paks")
                .join(&inputs.platform)
                .join(super::cooked::COOKED_FILE)
        });
        if let Some(r) = &self.reserved_ids {
            inputs.reserved_ids.clone_from(r);
        }
        if let Some(a) = &self.audience {
            inputs.audience.clone_from(a);
        }
        let mut registry = ExtensionRegistry::resolve_from(inputs);
        if let Some(text) = cooked.and_then(|path| std::fs::read_to_string(path).ok()) {
            registry.check_cooked(&text);
        }
        registry
    }
}

/// `<project>/.regia/extension-discovery.json`, the warm discovery cache (spec § 10).
fn discovery_cache_file(project: &Path) -> PathBuf {
    project.join(".regia").join(super::cache::CACHE_FILE)
}

impl Plugin for ExtensionPlugin {
    fn build(&self, app: &mut App) {
        let mut registry = self.registry();
        let audience = registry.inputs.audience.clone();
        let linked: BTreeMap<String, Arc<dyn RegiaExtension>> = self
            .linked
            .iter()
            .map(|e| (e.id().to_owned(), Arc::clone(e)))
            .collect();
        registry.linked = linked.keys().cloned().collect();
        for id in linked.keys() {
            if registry.state(id) != Some(ExtensionState::Active) {
                registry.warn(
                    id,
                    "linked_not_enabled",
                    format!("`{id}` is linked into this build but not in the resolved set"),
                );
            }
        }
        let order: Vec<String> = registry.resolved.order.iter().map(|e| e.id.clone()).collect();
        let mut built: Vec<String> = Vec::new();
        for id in order {
            let Some(found) = registry.discovered(&id).cloned() else {
                continue;
            };
            let m = &found.manifest;
            let dir = PathBuf::from(&found.path);
            registry.mount_manifest_content(m, &dir);
            let entries: Vec<_> = m.code.iter().filter(|c| c.audience.contains(&audience)).collect();
            if entries.is_empty() {
                // Code only for other hosts, and nothing else to load here (spec § 2.3).
                if !m.code.is_empty() && m.content.is_empty() && m.tools.is_empty() {
                    registry.filter_for_audience(&id, &audience);
                }
                continue;
            }
            let Some(ext) = linked.get(&id) else {
                if entries.iter().any(|c| c.crate_name.is_some()) {
                    let msg =
                        format!("`{id}` is enabled but its crate is not linked; run `regia plugin sync`");
                    registry.error(&id, "manifest_not_linked", msg.clone());
                    registry
                        .built
                        .insert(id.clone(), BuildState::Failed("manifest_not_linked", msg));
                    registry.events.push(ExtensionEvent::Failed {
                        id,
                        code: "manifest_not_linked".into(),
                    });
                }
                continue;
            };
            if build_one(app, &mut registry, m, dir, ext.as_ref()).is_ok() {
                built.push(id);
            }
        }
        for id in built {
            if let (Some(found), Some(ext)) = (registry.discovered(&id).cloned(), linked.get(&id)) {
                let previous = app.set_current_owner(registry.owner_for(&id));
                let finished = caught(|| {
                    let mut cx = ExtensionContext::new(
                        Target::App(&mut *app),
                        &mut registry,
                        &found.manifest,
                        PathBuf::from(&found.path),
                    );
                    ext.finish(&mut cx);
                });
                app.set_current_owner(previous);
                if let Err(panic) = finished {
                    registry.withdraw(&id);
                    registry.mark_failed(&id, "load_failed", format!("finish panicked: {panic}"));
                }
            }
        }
        registry.order_edges = registry.compute_order_edges();
        let wired: Vec<(String, Vec<InternedScheduleLabel>)> = registry
            .schedules
            .iter()
            .map(|(id, labels)| (id.clone(), labels.clone()))
            .collect();
        for (id, labels) in wired {
            for label in labels {
                let wire = registry.set_wiring(&id);
                if let Some(mut schedules) = app.world_mut().get_resource_mut::<Schedules>() {
                    wire(schedules.entry(label));
                }
            }
        }
        if let Some(types) = app.world().get_resource::<AppTypeRegistry>() {
            let mut types = types.write();
            types.register::<ExtensionEvent>();
            types.register::<LevelFeatures>();
        }
        app.add_message::<ExtensionEvent>();
        app.insert_resource(registry);
        app.add_systems(First, extension_schedule_boundary);
        app.add_systems(Last, extension_frame);
        app.add_systems(
            Update,
            persist_extension_settings.before(crate::settings::SettingsSystems::Apply),
        );
    }

    fn name(&self) -> &str {
        "regia_framework::extension::ExtensionPlugin"
    }
}

/// A run condition: true while feature pack `id` is active.
pub fn pack_active(id: &'static str) -> impl Fn(Option<Res<ExtensionRegistry>>) -> bool + Clone {
    move |registry: Option<Res<ExtensionRegistry>>| {
        registry.is_some_and(|r| r.packs.state(id) == Some(PackState::Active))
    }
}

/// Whether `from` reaches `to` over `edges`.
fn reaches(edges: &[(String, String)], from: &str, to: &str) -> bool {
    let mut seen = BTreeSet::new();
    let mut stack = vec![from];
    while let Some(at) = stack.pop() {
        if at == to {
            return true;
        }
        if seen.insert(at) {
            stack.extend(edges.iter().filter(|(a, _)| a == at).map(|(_, b)| b.as_str()));
        }
    }
    false
}

/// Applies queued schedule changes, leaving those for `running`: that schedule is out of
/// `Schedules` while it runs, so its changes wait for the next boundary.
fn apply_schedule_ops(world: &mut World, running: InternedScheduleLabel) {
    let ops = match world.get_resource_mut::<ExtensionRegistry>() {
        Some(mut registry) if !registry.schedule_ops.is_empty() => std::mem::take(&mut registry.schedule_ops),
        _ => return,
    };
    let Some(mut schedules) = world.remove_resource::<Schedules>() else {
        if let Some(mut registry) = world.get_resource_mut::<ExtensionRegistry>() {
            registry.schedule_ops = ops;
        }
        return;
    };
    let mut kept = Vec::new();
    for op in ops {
        if op.label() == running {
            kept.push(op);
            continue;
        }
        match op {
            ScheduleOp::Remove { label, owner } => {
                // A schedule the set never reached has nothing to remove.
                if let Err(error) = schedules.remove_systems_in_set(
                    label,
                    OwnerSet(owner),
                    world,
                    ScheduleCleanupPolicy::RemoveSystemsOnly,
                ) {
                    tracing::debug!(target: "regia_framework", "extension systems in {label:?}: {error}");
                }
            }
            ScheduleOp::Add { label, add } => add(schedules.entry(label)),
        }
    }
    // Removing systems can initialize an empty `Schedules` while this one is out; the edited one
    // goes back.
    world.remove_resource::<Schedules>();
    world.insert_resource(schedules);
    if let Some(mut registry) = world.get_resource_mut::<ExtensionRegistry>() {
        kept.append(&mut registry.schedule_ops);
        registry.schedule_ops = kept;
    }
}

/// Applies schedule changes queued for `Last` (the frame-end boundary cannot touch its own
/// schedule).
pub fn extension_schedule_boundary(world: &mut World) {
    apply_schedule_ops(world, First.intern());
}

/// The cooked record a packaged game carries in its paks, compared once.
fn check_embedded_record(world: &mut World) {
    let Some(mut server) = world.get_resource_mut::<AssetServer>() else {
        return;
    };
    let text = server
        .id_for_path(super::cooked::COOKED_ASSET_PATH)
        .and_then(|id| server.read_cooked(id).ok())
        .and_then(|(bytes, _)| {
            regia_asset::decode_cooked_file(&bytes)
                .ok()
                .map(|(_, payload)| String::from_utf8_lossy(payload).into_owned())
        });
    if let Some(mut registry) = world.get_resource_mut::<ExtensionRegistry>() {
        registry.cooked_checked = true;
        if let Some(text) = text {
            registry.check_cooked(&text);
            if let Some(mismatch) = registry.extra.iter().find(|d| d.code == "extension_set_mismatch") {
                tracing::error!(target: "regia_framework", "extension_set_mismatch: {}", mismatch.message);
            }
        }
    }
}

/// The asset server's view of the active extensions' loaders (`regia_asset::ContentLoaderHook`).
struct RegistryContentHook {
    loaders: Vec<(String, Arc<dyn AssetLoaderDesc>)>,
}

impl regia_asset::ContentLoaderHook for RegistryContentHook {
    fn handles(&self, file_extension: &str) -> bool {
        self.loaders
            .iter()
            .any(|(ext, _)| ext.eq_ignore_ascii_case(file_extension))
    }

    fn load(
        &self,
        file_extension: &str,
        bytes: &[u8],
        path: &str,
    ) -> Result<Box<dyn std::any::Any + Send>, String> {
        let (_, loader) = self
            .loaders
            .iter()
            .find(|(ext, _)| ext.eq_ignore_ascii_case(file_extension))
            .ok_or_else(|| format!("no extension loads `.{file_extension}` files"))?;
        let value: Box<dyn Reflect> = loader.load(bytes, path)?;
        Ok(Box::new(value))
    }
}

/// Keeps the asset server's content hook equal to the active loaders: installs it when the server
/// appears or the loader set changes, and does nothing (and allocates nothing) otherwise.
fn sync_content_hook(world: &mut World) {
    let Some(registry) = world.get_resource::<ExtensionRegistry>() else {
        return;
    };
    let (signature, last) = (registry.loader_signature(), registry.content_hook_sig);
    let Some(installed) = world
        .get_resource::<AssetServer>()
        .map(AssetServer::has_content_hook)
    else {
        return;
    };
    if signature == last && (installed || signature == 0) {
        return;
    }
    let loaders = registry
        .active_loaders()
        .map(|(ext, loader)| (ext.clone(), Arc::clone(loader)))
        .collect();
    let hook: Arc<dyn regia_asset::ContentLoaderHook> = Arc::new(RegistryContentHook { loaders });
    if let Some(mut server) = world.get_resource_mut::<AssetServer>() {
        server.set_content_hook(Some(hook));
    }
    if let Some(mut registry) = world.get_resource_mut::<ExtensionRegistry>() {
        registry.content_hook_sig = signature;
    }
}

/// The frame-boundary system: applies queued schedule changes, delivers queued events and applies
/// feature-pack transitions.
pub fn extension_frame(world: &mut World) {
    apply_schedule_ops(world, Last.intern());
    sync_content_hook(world);
    let record_due = world
        .get_resource::<ExtensionRegistry>()
        .is_some_and(|r| !r.cooked_checked);
    if record_due {
        check_embedded_record(world);
    }
    // Levels load and unload between frames; no packs, nothing to look at.
    let has_packs = world
        .get_resource::<ExtensionRegistry>()
        .is_some_and(|r| !r.packs.packs.is_empty());
    if has_packs {
        let wanted = level_feature_holds(world);
        if let Some(mut registry) = world.get_resource_mut::<ExtensionRegistry>()
            && wanted != registry.packs.from_levels
        {
            registry.packs.sync_levels(wanted);
        }
    }
    let can_render = world.contains_resource::<RuntimeRenderer>();
    let busy = world.get_resource::<ExtensionRegistry>().is_some_and(|r| {
        !r.events.is_empty() || r.packs.busy() || (can_render && !r.pending_passes.is_empty())
    });
    if !busy {
        return;
    }
    let _span = tracing::info_span!("extension_frame").entered();
    let Some(mut registry) = world.remove_resource::<ExtensionRegistry>() else {
        return;
    };
    flush_passes(world, &mut registry);
    apply_packs(world, &mut registry);
    let events = std::mem::take(&mut registry.events);
    world.insert_resource(registry);
    if let Some(mut messages) = world.get_resource_mut::<Messages<ExtensionEvent>>() {
        for e in events {
            messages.write(e);
        }
    }
}

/// The `(feature, owner)` holds of every [`LevelFeatures`] component; the owner is its level.
fn level_feature_holds(world: &mut World) -> BTreeSet<(String, String)> {
    let mut query = world.query::<(&LevelFeatures, Option<&regia_scene::level::LevelMember>)>();
    let mut out = BTreeSet::new();
    for (features, member) in query.iter(world) {
        let owner = member.map_or_else(|| "world".to_owned(), |m| format!("level:{:032x}", m.0));
        out.extend(features.features.iter().map(|f| (f.clone(), owner.clone())));
    }
    out
}

fn apply_packs(world: &mut World, registry: &mut ExtensionRegistry) {
    let ids: Vec<String> = registry.packs.packs.keys().cloned().collect();
    let Some(types) = world.get_resource::<AppTypeRegistry>().cloned() else {
        return;
    };
    let quiet = registry.packs.tags_quiet(world);
    let mut scanned = false;
    for id in ids {
        let Some(entry) = registry.packs.packs.get(&id) else {
            continue;
        };
        let wanted = registry.packs.wanted(&entry.desc);
        let state = entry.state;
        match (state, wanted) {
            (PackState::Active, true) if quiet => {}
            (PackState::Declared, true) | (PackState::Active, true) | (PackState::Withdrawing, true) => {
                scanned = true;
                let _span = tracing::info_span!("extension_pack_activate", pack = %id).entered();
                registry.packs.tag_scans = registry.packs.tag_scans.saturating_add(1);
                let tagged: BTreeSet<Entity> = {
                    let mut q = world.query::<(Entity, &Tags)>();
                    let tag = entry.desc.tag.clone();
                    q.iter(world)
                        .filter(|(_, t)| t.contains(&tag))
                        .map(|(e, _)| e)
                        .collect()
                };
                let Some(entry) = registry.packs.packs.get_mut(&id) else {
                    continue;
                };
                // An entity that lost the tag (or is gone) gets back what the pack added.
                let left: Vec<Entity> = entry
                    .applied
                    .keys()
                    .filter(|e| !tagged.contains(e))
                    .copied()
                    .collect();
                for e in left {
                    for tp in entry.applied.remove(&e).unwrap_or_default() {
                        remove_component(world, &types, e, &tp);
                    }
                }
                let mut warnings = Vec::new();
                for e in tagged {
                    if entry.applied.contains_key(&e) {
                        continue;
                    }
                    let mut added = Vec::new();
                    for spec in &entry.desc.components {
                        match insert_default(world, &types, e, &spec.type_path) {
                            Ok(true) => added.push(spec.type_path.clone()),
                            Ok(false) => {}
                            Err(msg) => warnings.push(msg),
                        }
                    }
                    entry.applied.insert(e, added);
                }
                if state != PackState::Active {
                    entry.state = PackState::Active;
                    registry
                        .events
                        .push(ExtensionEvent::PackActivated { pack: id.clone() });
                }
                let owner = entry.owner.clone();
                for w in warnings {
                    if !registry.extra.iter().any(|d| d.message == w) {
                        registry.warn(&owner, "manifest_invalid", w);
                    }
                }
            }
            (PackState::Active, false) => {
                let Some(entry) = registry.packs.packs.get_mut(&id) else {
                    continue;
                };
                for (e, added) in std::mem::take(&mut entry.applied) {
                    for tp in added {
                        remove_component(world, &types, e, &tp);
                    }
                }
                entry.state = PackState::Withdrawing;
                registry
                    .events
                    .push(ExtensionEvent::PackDeactivated { pack: id.clone() });
            }
            (PackState::Withdrawing, false) => {
                if let Some(entry) = registry.packs.packs.get_mut(&id) {
                    entry.state = PackState::Declared;
                }
            }
            (PackState::Declared, false) => {}
        }
    }
    if scanned {
        registry.packs.tags_accounted = world.read_change_tick().get();
    }
}

/// Inserts the registered default of `type_path` on `e` unless it already has one. Returns
/// whether it inserted.
fn insert_default(
    world: &mut World,
    types: &AppTypeRegistry,
    e: Entity,
    type_path: &str,
) -> Result<bool, String> {
    let types = types.read();
    let reg = types
        .get_with_type_path(type_path)
        .ok_or_else(|| format!("feature pack component `{type_path}` is not a registered type"))?;
    let component = reg
        .data::<ReflectComponent>()
        .ok_or_else(|| format!("`{type_path}` is not a reflected component"))?;
    let default = reg
        .data::<ReflectDefault>()
        .ok_or_else(|| format!("`{type_path}` has no registered default"))?;
    let Ok(entity) = world.get_entity(e) else {
        return Ok(false);
    };
    if entity.contains_type_id(reg.type_id()) {
        return Ok(false);
    }
    let value = default.default();
    let mut entity = world.entity_mut(e);
    component.insert(&mut entity, value.as_partial_reflect(), &types);
    Ok(true)
}

fn remove_component(world: &mut World, types: &AppTypeRegistry, e: Entity, type_path: &str) {
    let types = types.read();
    let Some(component) = types
        .get_with_type_path(type_path)
        .and_then(|r| r.data::<ReflectComponent>())
    else {
        return;
    };
    if let Ok(mut entity) = world.get_entity_mut(e) {
        component.remove(&mut entity);
    }
}
