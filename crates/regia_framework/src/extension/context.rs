// SPDX-License-Identifier: MPL-2.0
//! The extension trait and the registration context (`specs/57` § 3.2).
//!
//! An extension crate implements [`RegiaExtension`] and exports it with `regia::extension!`.
//! After resolution the host calls [`RegiaExtension::build`] once per extension in the resolved
//! order with an [`ExtensionContext`]. Every registration call checks the capability the call
//! needs against the project's grants, records `(extension id, kind, name)` in the
//! [`ExtensionRegistry`] so `plugin.info` and reloads can enumerate it,
//! and refuses a second registration of the same `(kind, name)` with `registration_conflict`
//! (the first registrant stays).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use regia_app::prelude::*;
use regia_ecs::ecs::system::ScheduleSystem;
use regia_ecs::owner::OwnerSet;
use regia_ecs::reflect::AppTypeRegistry;
use regia_render::{FramePass, slot_color_space, slot_name};

use super::manifest::{ExtensionManifest, IMPLICIT_CAPABILITIES};
use super::registry::{ExtensionRegistry, RegistrationKind, ScheduleOp};

/// Implemented once per extension crate; `regia::extension!` exports it.
pub trait RegiaExtension: Send + Sync + 'static {
    /// The extension id; must equal the manifest's `id`.
    fn id(&self) -> &str;
    /// Called once after resolution; register everything through `cx`.
    ///
    /// # Errors
    ///
    /// Any [`ExtensionError`]; the extension is then `failed` and its registrations are removed.
    fn build(&self, cx: &mut ExtensionContext<'_>) -> Result<(), ExtensionError>;
    /// Called after every extension built (the `Plugin::finish` analogue).
    fn finish(&self, _cx: &mut ExtensionContext<'_>) {}
    /// Before a dynamic reload: a snapshot of private state, restored by [`Self::on_load`].
    fn on_unload(&self, _world: &mut World) -> Option<DynamicStateBlob> {
        None
    }
    /// After a dynamic reload, with the snapshot the previous copy returned.
    fn on_load(&self, _world: &mut World, _prior: Option<DynamicStateBlob>) {}
}

/// State carried across a dynamic reload: serialized bytes the extension reads back itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DynamicStateBlob(pub Vec<u8>);

/// Why a registration or build step was refused. `code()` is the protocol code (spec § 5.3).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExtensionError {
    /// The manifest does not request, or the project does not grant, the capability.
    #[error("capability_not_granted: `{capability}` is needed for {what}")]
    CapabilityNotGranted {
        /// The capability.
        capability: &'static str,
        /// What was being registered.
        what: String,
    },
    /// Another extension registered the same `(kind, name)` first.
    #[error("registration_conflict: {kind} `{name}` is already registered by `{owner}`")]
    RegistrationConflict {
        /// Registration kind.
        kind: &'static str,
        /// The contested name.
        name: String,
        /// The extension that keeps it.
        owner: String,
    },
    /// The seam does not exist yet, or the call is not possible in this host.
    #[error("not_supported: {0}")]
    NotSupported(String),
    /// The frame graph cannot take this pass: a bad name or a slot with no target.
    #[error("render_pass_invalid: {0}")]
    RenderPassInvalid(String),
    /// A content path is already mounted by another extension.
    #[error("content_collision: `{path}` is already provided by `{owner}`")]
    ContentCollision {
        /// The path relative to the mount.
        path: String,
        /// The extension that keeps it.
        owner: String,
    },
    /// A path leaves the extension directory.
    #[error("path_escape: `{0}` leaves the extension directory")]
    PathEscape(String),
    /// The settings table or another input is malformed.
    #[error("manifest_invalid: {0}")]
    Invalid(String),
    /// The extension's own build step failed.
    #[error("load_failed: {0}")]
    BuildFailed(String),
    /// A change to code needs a process restart.
    #[error("restart_required: {0}")]
    RestartRequired(String),
}

impl ExtensionError {
    /// The lowercase protocol code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::CapabilityNotGranted { .. } => "capability_not_granted",
            Self::RegistrationConflict { .. } => "registration_conflict",
            Self::NotSupported(_) => "not_supported",
            Self::RenderPassInvalid(_) => "render_pass_invalid",
            Self::ContentCollision { .. } => "content_collision",
            Self::PathEscape(_) => "path_escape",
            Self::Invalid(_) => "manifest_invalid",
            Self::BuildFailed(_) => "load_failed",
            Self::RestartRequired(_) => "restart_required",
        }
    }
}

/// The access class of an extension agent method (`specs/23` § 5 permission names).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MethodAccess {
    /// Read-only.
    Read,
    /// Drives the running game.
    Play,
    /// Changes scene content.
    EditScene,
}

impl MethodAccess {
    /// The permission name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Play => "play",
            Self::EditScene => "edit_scene",
        }
    }
}

/// A structured failure of an extension method: a lowercase code and data.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentFault {
    /// Lowercase ascii, digits and `_`.
    pub code: String,
    /// Extra data for the caller.
    pub data: Value,
}

/// The handler of an extension agent method.
pub type AgentMethodHandler = Arc<dyn Fn(&mut World, Value) -> Result<Value, AgentFault> + Send + Sync>;

/// An agent method an extension registers (plain data, so this layer never names agent types).
#[derive(Clone)]
pub struct AgentMethodDesc {
    /// `<extension id>.<verb>[.<noun>]`, lowercase, dots and underscores.
    pub name: String,
    /// One-line description.
    pub summary: String,
    /// Access class.
    pub access: MethodAccess,
    /// JSON Schema of the params.
    pub params_schema: Value,
    /// JSON Schema of the result.
    pub result_schema: Value,
    /// The handler.
    pub handler: AgentMethodHandler,
}

impl std::fmt::Debug for AgentMethodDesc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentMethodDesc")
            .field("name", &self.name)
            .field("access", &self.access)
            .finish_non_exhaustive()
    }
}

/// A run-time asset loader an extension registers for file extensions.
pub trait AssetLoaderDesc: Send + Sync + 'static {
    /// File extensions without the dot, e.g. `["dlg"]`.
    fn file_extensions(&self) -> &[&str];
    /// The asset type's path.
    fn type_name(&self) -> &str;
    /// Decodes bytes into a reflected value.
    ///
    /// # Errors
    ///
    /// A message for malformed input.
    fn load(&self, bytes: &[u8], path: &str) -> Result<Box<dyn Reflect>, String>;
}

/// The system set of one extension (spec § 3.3.1): everything the extension added to a schedule,
/// through [`ExtensionContext::add_systems`] or inside a plugin. Order against it like any set;
/// `[extensions.order]` and dependency edges do so for you.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct ExtensionSet(pub String);

/// One component a feature pack adds: a reflected component type, inserted with its
/// registered default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentSpec {
    /// The component's type path (as `types.list` shows it).
    pub type_path: String,
}

/// A feature pack (spec § 3.3.8): components added to entities carrying `tag` while a level
/// enables `requires_level_feature`. Its systems run under `pack_active(id)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeaturePackDesc {
    /// Pack id, unique across extensions.
    pub id: String,
    /// Entities carrying this tag (`regia_scene::Tags`) get the components.
    pub tag: String,
    /// Components added on activation and removed on withdrawal.
    pub components: Vec<ComponentSpec>,
    /// The level feature that switches the pack on.
    pub requires_level_feature: String,
}

/// A mounted content root (`ext://<id>/`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentMount {
    /// The extension id.
    pub id: String,
    /// The mounted directory.
    pub root: PathBuf,
    /// Files it provides (relative, `/`-separated).
    pub files: usize,
    /// Paths it lost to an earlier mount (`content_collision`).
    pub collisions: Vec<String>,
}

/// Where registration calls go while building.
pub(crate) enum Target<'a> {
    /// Startup: plugins can be added.
    App(&'a mut App),
    /// A live reload: plugins (systems) need a restart.
    World(&'a mut World),
}

/// What one extension's `build` may register (spec § 3.2).
pub struct ExtensionContext<'a> {
    pub(crate) target: Target<'a>,
    pub(crate) registry: &'a mut ExtensionRegistry,
    pub(crate) manifest: &'a ExtensionManifest,
    pub(crate) dir: PathBuf,
    pub(crate) granted: BTreeSet<String>,
}

impl<'a> ExtensionContext<'a> {
    pub(crate) fn new(
        target: Target<'a>,
        registry: &'a mut ExtensionRegistry,
        manifest: &'a ExtensionManifest,
        dir: PathBuf,
    ) -> Self {
        let granted = registry.granted_for(manifest);
        Self {
            target,
            registry,
            manifest,
            dir,
            granted,
        }
    }

    /// The extension id.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.manifest.id
    }

    /// The extension's manifest.
    #[must_use]
    pub fn manifest(&self) -> &ExtensionManifest {
        self.manifest
    }

    /// The extension directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The world being configured.
    pub fn world_mut(&mut self) -> &mut World {
        match &mut self.target {
            Target::App(app) => app.world_mut(),
            Target::World(world) => world,
        }
    }

    /// Checks that the manifest requests and the project grants `capability` (hosts that add
    /// registrations of their own, like the editor, call this first).
    ///
    /// # Errors
    ///
    /// `capability_not_granted`.
    pub fn require(&self, capability: &'static str, what: impl Into<String>) -> Result<(), ExtensionError> {
        let requested = self.manifest.capabilities.iter().any(|c| c == capability);
        let granted = IMPLICIT_CAPABILITIES.contains(&capability) || self.granted.contains(capability);
        if requested && granted {
            Ok(())
        } else {
            Err(ExtensionError::CapabilityNotGranted {
                capability,
                what: what.into(),
            })
        }
    }

    /// Records `(this extension, kind, name)` for a registration a host keeps itself: importers,
    /// panels and inspectors. The kinds this context registers go through their own calls, which
    /// hold the naming rules, so claiming one here would squat on another extension's name.
    ///
    /// # Errors
    ///
    /// `capability_not_granted`; `registration_conflict`; `not_supported` for any other kind.
    pub fn claim(&mut self, kind: RegistrationKind, name: &str) -> Result<(), ExtensionError> {
        let capability = match kind {
            RegistrationKind::Importer => "asset_importer",
            RegistrationKind::EditorPanel
            | RegistrationKind::Inspector
            | RegistrationKind::EditorAction
            | RegistrationKind::AssetTool
            | RegistrationKind::ViewportTool
            | RegistrationKind::Overlay
            | RegistrationKind::EditorSetting
            | RegistrationKind::BuildHook
            | RegistrationKind::EditorWorkspace => "editor_panel",
            other => {
                return Err(ExtensionError::NotSupported(format!(
                    "`{}` registrations go through their own call",
                    other.name()
                )));
            }
        };
        self.require(capability, format!("{} `{name}`", kind.name()))?;
        self.registry.claim(&self.manifest.id, kind, name)
    }

    /// `[extensions.settings.<id>]` of `Regia.toml` decoded as `T` (an empty table when absent).
    ///
    /// # Errors
    ///
    /// `manifest_invalid` when the table does not fit `T`.
    pub fn settings<T: DeserializeOwned>(&self) -> Result<T, ExtensionError> {
        let table = self
            .registry
            .config()
            .settings
            .get(&self.manifest.id)
            .cloned()
            .unwrap_or_default();
        toml::Value::Table(table)
            .try_into()
            .map_err(|e: toml::de::Error| {
                ExtensionError::Invalid(format!(
                    "[extensions.settings.{}]: {}",
                    self.manifest.id,
                    e.message()
                ))
            })
    }

    /// Adds systems to `schedule`, in this extension's [`ExtensionSet`] (needs `systems`). During
    /// a reload they join at the next frame boundary, when the old copy's systems leave, so an
    /// extension that wants to reload live adds its systems here and not through a plugin.
    ///
    /// # Errors
    ///
    /// `capability_not_granted`.
    pub fn add_systems<M: 'static>(
        &mut self,
        schedule: impl ScheduleLabel,
        systems: impl IntoScheduleConfigs<ScheduleSystem, M> + Send + Sync + 'static,
    ) -> Result<(), ExtensionError> {
        self.require("systems", "systems")?;
        let id = self.manifest.id.clone();
        let label = schedule.intern();
        self.registry.note_schedule(&id, label);
        match &mut self.target {
            Target::App(app) => {
                app.add_systems(label, systems);
            }
            Target::World(_) => {
                let owner = self.registry.owner_for(&id);
                let sets = self.registry.set_wiring(&id);
                self.registry.queue_schedule_op(ScheduleOp::Add {
                    label,
                    add: Box::new(move |schedule| {
                        sets(schedule);
                        schedule.add_systems(systems.in_set(OwnerSet(owner)));
                    }),
                });
            }
        }
        Ok(())
    }

    /// Adds a plugin, named `ext/<id>/<name>` (needs `systems`). Its systems join this extension's
    /// [`ExtensionSet`] and leave with it.
    ///
    /// # Errors
    ///
    /// `capability_not_granted`; `registration_conflict` for a second plugin of the same name;
    /// `restart_required` during a live reload (a plugin builds against the app, which a running
    /// frame does not have; use [`Self::add_systems`]).
    pub fn add_plugin(&mut self, plugin: impl Plugin) -> Result<(), ExtensionError> {
        self.require("systems", "a plugin")?;
        let name = format!("ext/{}/{}", self.manifest.id, plugin.name());
        self.registry
            .claim(&self.manifest.id, RegistrationKind::Plugin, &name)?;
        match &mut self.target {
            Target::App(app) => {
                app.add_plugins(ExtensionPluginWrapper { name, inner: plugin });
                Ok(())
            }
            Target::World(_) => {
                self.registry
                    .release(&self.manifest.id, RegistrationKind::Plugin, &name);
                Err(ExtensionError::RestartRequired(format!(
                    "plugin `{name}` adds systems; restart to load it"
                )))
            }
        }
    }

    /// Registers a run-time asset loader for its file extensions (needs `asset_loader`).
    ///
    /// # Errors
    ///
    /// `capability_not_granted`; `registration_conflict` when a file extension is taken.
    pub fn register_asset_loader(&mut self, loader: Box<dyn AssetLoaderDesc>) -> Result<(), ExtensionError> {
        self.require("asset_loader", "an asset loader")?;
        let exts: Vec<String> = loader
            .file_extensions()
            .iter()
            .map(|e| e.trim_start_matches('.').to_ascii_lowercase())
            .collect();
        for ext in &exts {
            if let Some(owner) = self.registry.owner_of(RegistrationKind::AssetLoader, ext) {
                return Err(ExtensionError::RegistrationConflict {
                    kind: RegistrationKind::AssetLoader.name(),
                    name: ext.clone(),
                    owner: owner.to_owned(),
                });
            }
        }
        let loader: Arc<dyn AssetLoaderDesc> = Arc::from(loader);
        for ext in exts {
            self.registry
                .claim(&self.manifest.id, RegistrationKind::AssetLoader, &ext)?;
            self.registry.loaders.insert(ext, Arc::clone(&loader));
        }
        Ok(())
    }

    /// Registers an agent method under `<id>.` (needs `agent_method`).
    ///
    /// # Errors
    ///
    /// `capability_not_granted`; `registration_conflict` for a name outside the extension's
    /// namespace, a malformed name, or a name already registered.
    pub fn register_agent_method(&mut self, method: AgentMethodDesc) -> Result<(), ExtensionError> {
        self.require("agent_method", format!("agent method `{}`", method.name))?;
        let prefix = format!("{}.", self.manifest.id);
        let rest = method.name.strip_prefix(&prefix).unwrap_or("");
        let well_formed = !rest.is_empty()
            && rest.split('.').all(|s| {
                !s.is_empty()
                    && s.bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            });
        if !well_formed {
            return Err(ExtensionError::RegistrationConflict {
                kind: RegistrationKind::AgentMethod.name(),
                name: method.name.clone(),
                owner: format!("(names must be `{prefix}<verb>[.<noun>]`, lowercase)"),
            });
        }
        self.registry
            .claim(&self.manifest.id, RegistrationKind::AgentMethod, &method.name)?;
        self.registry
            .agent_methods
            .insert(method.name.clone(), (self.manifest.id.clone(), method));
        Ok(())
    }

    /// Adds a pass to the frame graph at its [`FramePass::slot`] (needs `render_pass`). It records
    /// from the first frame after the renderer exists; a pass the graph refuses then is dropped
    /// with one `render_pass_invalid` warning and the frame goes on.
    ///
    /// # Errors
    ///
    /// `capability_not_granted`; `render_pass_invalid` for a malformed name or a slot with no
    /// target; `registration_conflict` for a name another extension holds.
    pub fn add_render_pass(&mut self, pass: Box<dyn FramePass>) -> Result<(), ExtensionError> {
        let name = pass.name().to_owned();
        self.require("render_pass", format!("render pass `{name}`"))?;
        if name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        {
            return Err(ExtensionError::RenderPassInvalid(format!(
                "pass name `{name}`: use lowercase ascii letters, digits and `_`"
            )));
        }
        if slot_color_space(pass.slot()).is_none() {
            return Err(ExtensionError::RenderPassInvalid(format!(
                "pass `{name}`: slot `{}` has no color target",
                slot_name(pass.slot())
            )));
        }
        self.registry
            .claim(&self.manifest.id, RegistrationKind::RenderPass, &name)?;
        self.registry
            .pending_passes
            .push((self.manifest.id.clone(), pass));
        Ok(())
    }

    /// Declares the extension's settings resource, initialized from
    /// `[extensions.settings.<id>]` and listed by `plugin.info` (`settings_type`). Keys `T` does
    /// not have are `unknown_field` warnings. The type is registered in the app's type registry,
    /// and `settings.get` / `settings.set` reach the resource as `<id>` or `<id>.<field>` (spec 57
    /// § 3.3.5; a `GameSettings` field of the same name wins). A value saved in the user settings
    /// file (`GameSettings::extensions`) is applied over the table, and later changes are saved
    /// there (`persist_extension_settings`, ADR-7237).
    ///
    /// # Errors
    ///
    /// `manifest_invalid` when the table does not fit `T`; `registration_conflict` on a second call.
    pub fn declare_settings<T>(&mut self) -> Result<(), ExtensionError>
    where
        T: Resource + Reflect + Default + Serialize + DeserializeOwned + GetTypeRegistration,
    {
        self.require("settings", "a settings resource")?;
        let id = self.manifest.id.clone();
        let table = self
            .registry
            .config()
            .settings
            .get(&id)
            .cloned()
            .unwrap_or_default();
        let value: T = self.settings()?;
        let known: BTreeSet<String> = toml::Value::try_from(T::default())
            .ok()
            .and_then(|v| v.as_table().map(|t| t.keys().cloned().collect()))
            .unwrap_or_default();
        for key in table.keys().filter(|k| !known.contains(*k)) {
            self.registry.warn(
                &id,
                "unknown_field",
                format!("[extensions.settings.{id}] has no field `{key}`"),
            );
        }
        self.registry.claim(&id, RegistrationKind::Settings, &id)?;
        let type_path = T::get_type_registration().type_info().type_path().to_owned();
        self.registry
            .settings_access
            .insert(id.clone(), super::registry::SettingsAccess::of::<T>());
        self.registry.settings.insert(id.clone(), type_path);
        let world = self.world_mut();
        if let Some(types) = world.get_resource::<AppTypeRegistry>() {
            types.write().register::<T>();
        }
        world.insert_resource(value);
        // The user's saved value wins over the project table (spec 57 § 3.3.5, ADR-7237). The
        // settings plugin loads the file at its build; when it is built after the extensions,
        // `persist_extension_settings` applies the saved value on its first run instead.
        let saved = world
            .get_resource::<crate::settings::GameSettings>()
            .and_then(|s| s.extensions.get(&id).cloned());
        if let Some(saved) = saved {
            let parsed: Result<T, _> = ron::from_str(&saved);
            match parsed {
                Ok(saved) => world.insert_resource(saved),
                Err(why) => tracing::warn!(
                    target: "regia_framework",
                    "the saved settings of extension `{id}` do not fit its settings type ({why}); keeping Regia.toml's"
                ),
            }
        }
        Ok(())
    }

    /// Mounts `root` (relative to the extension directory) at `ext://<id>/` (needs
    /// `content_mount`). Paths another extension already mounted stay with it and are listed in
    /// [`ContentMount::collisions`] (`content_collision`).
    ///
    /// # Errors
    ///
    /// `capability_not_granted`; `path_escape` for a root outside the extension directory;
    /// `manifest_invalid` when the root cannot be read.
    pub fn mount_content(&mut self, root: &Path) -> Result<ContentMount, ExtensionError> {
        self.require("content_mount", "a content mount")?;
        let text = root.to_string_lossy().replace('\\', "/");
        if regia_core::project_path::check_relative(&text).is_err() && text != "." {
            return Err(ExtensionError::PathEscape(text));
        }
        if self.dir.as_os_str().is_empty() {
            // A packaged game has no extension directory: its content is in the paks.
            return Ok(ContentMount {
                id: self.manifest.id.clone(),
                root: root.to_path_buf(),
                files: 0,
                collisions: Vec::new(),
            });
        }
        let dir = super::registry::content_root(&self.dir, &text)
            .map_err(|_| ExtensionError::PathEscape(text.clone()))?;
        let mount = self
            .registry
            .mounts
            .mount(&self.manifest.id, &dir)
            .map_err(ExtensionError::Invalid)?;
        for path in &mount.collisions {
            self.registry.warn(
                &self.manifest.id,
                "content_collision",
                format!("`{path}` is already mounted by another extension"),
            );
        }
        self.registry
            .claim(
                &self.manifest.id,
                RegistrationKind::ContentMount,
                &format!("ext://{}/", self.manifest.id),
            )
            .ok();
        Ok(mount)
    }

    /// Declares a feature pack (needs `feature_pack`).
    ///
    /// # Errors
    ///
    /// `capability_not_granted`; `registration_conflict` for a pack id already declared.
    pub fn declare_feature_pack(&mut self, pack: FeaturePackDesc) -> Result<(), ExtensionError> {
        self.require("feature_pack", format!("feature pack `{}`", pack.id))?;
        self.registry
            .claim(&self.manifest.id, RegistrationKind::FeaturePack, &pack.id)?;
        self.registry.packs.declare(&self.manifest.id, pack);
        Ok(())
    }
}

/// An extension's plugin under its `ext/<id>/<name>` name.
struct ExtensionPluginWrapper<P> {
    name: String,
    inner: P,
}

impl<P: Plugin> Plugin for ExtensionPluginWrapper<P> {
    fn build(&self, app: &mut App) {
        self.inner.build(app);
    }
    fn finish(&self, app: &mut App) {
        self.inner.finish(app);
    }
    fn cleanup(&self, app: &mut App) {
        self.inner.cleanup(app);
    }
    fn name(&self) -> &str {
        &self.name
    }
}
