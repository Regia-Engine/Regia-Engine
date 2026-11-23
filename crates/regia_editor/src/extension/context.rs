// SPDX-License-Identifier: MPL-2.0
//! What editor- and tool-audience extension code registers (`specs/57` § 3.2, § 3.3).
//!
//! An extension's `build` receives the runtime `ExtensionContext`. Editor code wraps it in
//! [`ToolExtensionContext`], which adds importers, panels, actions, asset tools, viewport tools,
//! overlays, typed settings, build hooks and inspector customizations; it derefs
//! to the runtime context. Outside the editor host the wrapper answers `not_supported`, so a
//! crate that registers panels fails loudly when it is listed for the wrong audience.

use std::collections::BTreeMap;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use regia_asset_pipeline::extension_import::{Importer, ImporterRegistry};
use regia_ecs::prelude::*;
use regia_framework::extension::registry::RegistrationKind;
use regia_framework::extension::{ExtensionContext, ExtensionError};

use regia_ecs::reflect::PartialReflect;

use super::actions::{ActionDesc, AssetToolDesc};
use super::hooks::BuildHookDesc;
use super::settings::SettingDesc;
use super::viewport_tools::{OverlayDesc, ViewportToolDesc};

/// Where a panel docks first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DockRegion {
    /// With the outliner.
    Left,
    /// With the inspector.
    Right,
    /// With the content browser and the log.
    #[default]
    Bottom,
    /// With the viewport.
    Center,
}

/// Draws an extension panel.
pub type PanelDraw = Arc<dyn Fn(&mut egui::Ui, &mut World) + Send + Sync>;

/// An editor panel an extension registers.
#[derive(Clone)]
pub struct PanelDesc {
    /// `<ext>.<panel>`.
    pub id: String,
    /// Tab title, a localization key or plain text.
    pub title_key: String,
    /// Default dock region.
    pub dock_region: DockRegion,
    /// The draw callback.
    pub draw: PanelDraw,
}

/// A named workspace supplied by an editor extension (ADR-8197).
#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceDesc {
    /// Namespaced registration id (`<extension>.<workspace>`).
    pub id: String,
    /// Switcher label.
    pub title: String,
    /// Initial dock layout using the existing layout format.
    pub layout: crate::editor_layout::LayoutDoc,
    /// Default tool mode; absent selects World.
    pub mode: Option<crate::workspaces::EditorMode>,
}

/// Draws a reflected value in place of the inspector's default widget; returns whether it changed
/// the value.
pub type InspectorDraw = Arc<dyn Fn(&mut egui::Ui, &mut dyn PartialReflect) -> bool + Send + Sync>;

/// Replaces the inspector's widget for one reflected type.
#[derive(Clone)]
pub struct InspectorCustomization {
    /// The type path (as `types.list` shows it).
    pub type_path: String,
    /// The draw callback.
    pub draw: InspectorDraw,
}

/// The editor's extension registries. An entry only counts while the extension registry still
/// records the owner's registration, so removing an extension's registrations hides its panels
/// and inspectors without touching this resource.
#[derive(Resource, Default)]
pub struct EditorExtensions {
    pub(crate) panels: BTreeMap<String, (String, PanelDesc)>,
    pub(crate) inspectors: BTreeMap<String, (String, InspectorCustomization)>,
    /// Importers extensions registered, consulted for files the built-in importers do not take.
    pub importers: ImporterRegistry,
    pub(crate) actions: BTreeMap<String, (String, ActionDesc)>,
    pub(crate) asset_tools: BTreeMap<String, (String, AssetToolDesc)>,
    pub(crate) tools: BTreeMap<String, (String, ViewportToolDesc)>,
    pub(crate) overlays: BTreeMap<String, (String, OverlayDesc)>,
    pub(crate) settings: BTreeMap<String, (String, SettingDesc)>,
    pub(crate) hooks: BTreeMap<String, (String, BuildHookDesc)>,
    pub(crate) workspaces: BTreeMap<String, (String, WorkspaceDesc)>,
}

/// One extension's editor registrations, set aside while a reload runs.
pub struct EditorSnapshot {
    panels: BTreeMap<String, (String, PanelDesc)>,
    inspectors: BTreeMap<String, (String, InspectorCustomization)>,
    importers: ImporterRegistry,
    actions: BTreeMap<String, (String, ActionDesc)>,
    asset_tools: BTreeMap<String, (String, AssetToolDesc)>,
    tools: BTreeMap<String, (String, ViewportToolDesc)>,
    overlays: BTreeMap<String, (String, OverlayDesc)>,
    settings: BTreeMap<String, (String, SettingDesc)>,
    hooks: BTreeMap<String, (String, BuildHookDesc)>,
    workspaces: BTreeMap<String, (String, WorkspaceDesc)>,
}

/// Moves `owner`'s entries of `map` out.
fn take<T>(map: &mut BTreeMap<String, (String, T)>, owner: &str) -> BTreeMap<String, (String, T)> {
    let (taken, rest) = std::mem::take(map)
        .into_iter()
        .partition(|(_, (o, _))| o == owner);
    *map = rest;
    taken
}

impl EditorExtensions {
    /// Takes `owner`'s registrations out, for [`Self::restore`] if the new copy fails to build.
    pub fn take_owner(&mut self, owner: &str) -> EditorSnapshot {
        let (panels, rest) = std::mem::take(&mut self.panels)
            .into_iter()
            .partition(|(_, (o, _))| o == owner);
        self.panels = rest;
        let (inspectors, rest) = std::mem::take(&mut self.inspectors)
            .into_iter()
            .partition(|(_, (o, _))| o == owner);
        self.inspectors = rest;
        let (actions, rest) = std::mem::take(&mut self.actions)
            .into_iter()
            .partition(|(_, (o, _))| o == owner);
        self.actions = rest;
        let (asset_tools, rest) = std::mem::take(&mut self.asset_tools)
            .into_iter()
            .partition(|(_, (o, _))| o == owner);
        self.asset_tools = rest;
        EditorSnapshot {
            panels,
            inspectors,
            importers: self.importers.take_owner(owner),
            actions,
            asset_tools,
            tools: take(&mut self.tools, owner),
            overlays: take(&mut self.overlays, owner),
            settings: take(&mut self.settings, owner),
            hooks: take(&mut self.hooks, owner),
            workspaces: take(&mut self.workspaces, owner),
        }
    }

    /// Puts back what [`Self::take_owner`] took, over anything the failed copy registered.
    pub fn restore(&mut self, owner: &str, snapshot: EditorSnapshot) {
        self.panels.retain(|_, (o, _)| o != owner);
        self.inspectors.retain(|_, (o, _)| o != owner);
        self.actions.retain(|_, (o, _)| o != owner);
        self.asset_tools.retain(|_, (o, _)| o != owner);
        self.tools.retain(|_, (o, _)| o != owner);
        self.overlays.retain(|_, (o, _)| o != owner);
        self.settings.retain(|_, (o, _)| o != owner);
        self.hooks.retain(|_, (o, _)| o != owner);
        self.workspaces.retain(|_, (o, _)| o != owner);
        self.tools.extend(snapshot.tools);
        self.overlays.extend(snapshot.overlays);
        self.settings.extend(snapshot.settings);
        self.hooks.extend(snapshot.hooks);
        self.workspaces.extend(snapshot.workspaces);
        self.panels.extend(snapshot.panels);
        self.inspectors.extend(snapshot.inspectors);
        self.actions.extend(snapshot.actions);
        self.asset_tools.extend(snapshot.asset_tools);
        self.importers.restore(owner, snapshot.importers);
    }

    /// `(owner, panel)` of every registered panel.
    pub fn panels(&self) -> impl Iterator<Item = (&str, &PanelDesc)> {
        self.panels.values().map(|(owner, desc)| (owner.as_str(), desc))
    }

    /// The panel `id`.
    #[must_use]
    pub fn panel(&self, id: &str) -> Option<&PanelDesc> {
        self.panels.get(id).map(|(_, desc)| desc)
    }

    /// The customization of `type_path` and the extension that registered it.
    #[must_use]
    pub fn inspector(&self, type_path: &str) -> Option<(&str, &InspectorCustomization)> {
        self.inspectors
            .get(type_path)
            .map(|(owner, desc)| (owner.as_str(), desc))
    }
}

/// [`ExtensionContext`] plus the editor's registries.
pub struct ToolExtensionContext<'c, 'a> {
    cx: &'c mut ExtensionContext<'a>,
}

impl<'c, 'a> ToolExtensionContext<'c, 'a> {
    /// Wraps `cx`.
    ///
    /// # Errors
    ///
    /// `not_supported` when this host is not the editor.
    pub fn new(cx: &'c mut ExtensionContext<'a>) -> Result<Self, ExtensionError> {
        if !cx.world_mut().contains_resource::<EditorExtensions>() {
            return Err(ExtensionError::NotSupported(
                "editor registrations need the editor host; list this code entry for the `editor` audience"
                    .into(),
            ));
        }
        Ok(Self { cx })
    }

    /// Registers an importer for its file extensions (needs `asset_importer`).
    ///
    /// # Errors
    ///
    /// `capability_not_granted`; `registration_conflict` when a file extension is taken.
    pub fn register_importer(&mut self, importer: Box<dyn Importer>) -> Result<(), ExtensionError> {
        self.cx.require("asset_importer", "an importer")?;
        let names: Vec<String> = importer
            .file_extensions()
            .iter()
            .map(|e| e.trim_start_matches('.').to_ascii_lowercase())
            .collect();
        let id = self.cx.id().to_owned();
        for name in &names {
            self.cx.claim(RegistrationKind::Importer, name)?;
        }
        let importer: Arc<dyn Importer> = Arc::from(importer);
        let mut registries = self.cx.world_mut().resource_mut::<EditorExtensions>();
        for name in names {
            registries.importers.insert(&name, &id, Arc::clone(&importer));
        }
        Ok(())
    }

    /// Registers a panel (needs `editor_panel`). Its id must start with `<extension id>.`.
    ///
    /// # Errors
    ///
    /// `capability_not_granted`; `registration_conflict` for a taken id or a foreign prefix.
    pub fn register_editor_panel(&mut self, panel: PanelDesc) -> Result<(), ExtensionError> {
        self.cx.require("editor_panel", format!("panel `{}`", panel.id))?;
        let id = self.cx.id().to_owned();
        if !panel
            .id
            .strip_prefix(&format!("{id}."))
            .is_some_and(|rest| !rest.is_empty())
        {
            return Err(ExtensionError::RegistrationConflict {
                kind: RegistrationKind::EditorPanel.name(),
                name: panel.id,
                owner: format!("(panel ids must be `{id}.<panel>`)"),
            });
        }
        self.cx.claim(RegistrationKind::EditorPanel, &panel.id)?;
        self.cx
            .world_mut()
            .resource_mut::<EditorExtensions>()
            .panels
            .insert(panel.id.clone(), (id, panel));
        Ok(())
    }

    /// Checks that `name` is `<extension id>.<rest>` with a non-empty rest.
    fn own_name(&self, kind: RegistrationKind, name: &str) -> Result<(), ExtensionError> {
        let id = self.cx.id();
        if name
            .strip_prefix(&format!("{id}."))
            .is_some_and(|rest| !rest.is_empty())
        {
            return Ok(());
        }
        Err(ExtensionError::RegistrationConflict {
            kind: kind.name(),
            name: name.to_owned(),
            owner: format!("(ids must be `{id}.<name>`)"),
        })
    }

    /// Registers a menu action, and a toolbar button when `toolbar` is set (needs `editor_panel`;
    /// ADR-9850). It is also a palette command with an optional default shortcut, and may join
    /// context menus under selection predicates (ADR-7791). Its id must start with
    /// `<extension id>.`.
    ///
    /// # Errors
    ///
    /// `capability_not_granted`; `registration_conflict` for a taken id or a foreign prefix;
    /// `manifest_invalid` for a shortcut that does not parse.
    pub fn register_editor_action(&mut self, action: ActionDesc) -> Result<(), ExtensionError> {
        self.cx
            .require("editor_panel", format!("action `{}`", action.id))?;
        self.own_name(RegistrationKind::EditorAction, &action.id)?;
        if !action.shortcut.trim().is_empty()
            && crate::command_palette::KeyChord::parse(&action.shortcut).is_none()
        {
            return Err(ExtensionError::Invalid(format!(
                "action `{}`: shortcut `{}` is not a chord (e.g. Ctrl+Alt+G)",
                action.id, action.shortcut
            )));
        }
        self.cx.claim(RegistrationKind::EditorAction, &action.id)?;
        let id = self.cx.id().to_owned();
        self.cx
            .world_mut()
            .resource_mut::<EditorExtensions>()
            .actions
            .insert(action.id.clone(), (id, action));
        Ok(())
    }

    /// Registers a tool for the asset types its suffixes name (needs `editor_panel`; ADR-9850). Its
    /// id must start with `<extension id>.` and it must name at least one suffix.
    ///
    /// # Errors
    ///
    /// `capability_not_granted`; `registration_conflict` for a taken id or a foreign prefix;
    /// `manifest_invalid` for a tool with no suffix.
    pub fn register_asset_tool(&mut self, tool: AssetToolDesc) -> Result<(), ExtensionError> {
        self.cx
            .require("editor_panel", format!("asset tool `{}`", tool.id))?;
        self.own_name(RegistrationKind::AssetTool, &tool.id)?;
        if tool.suffixes.iter().all(|suffix| suffix.trim().is_empty()) {
            return Err(ExtensionError::Invalid(format!(
                "asset tool `{}` names no file suffix",
                tool.id
            )));
        }
        self.cx.claim(RegistrationKind::AssetTool, &tool.id)?;
        let id = self.cx.id().to_owned();
        self.cx
            .world_mut()
            .resource_mut::<EditorExtensions>()
            .asset_tools
            .insert(tool.id.clone(), (id, tool));
        Ok(())
    }

    /// Inserts `desc` under `name` into one of the editor maps after the naming and claim checks.
    fn register_owned<T>(
        &mut self,
        kind: RegistrationKind,
        name: &str,
        map: impl FnOnce(&mut EditorExtensions) -> &mut BTreeMap<String, (String, T)>,
        desc: T,
    ) -> Result<(), ExtensionError> {
        self.cx
            .require("editor_panel", format!("{} `{name}`", kind.name()))?;
        self.own_name(kind, name)?;
        self.cx.claim(kind, name)?;
        let id = self.cx.id().to_owned();
        let mut registries = self.cx.world_mut().resource_mut::<EditorExtensions>();
        map(&mut registries).insert(name.to_owned(), (id, desc));
        Ok(())
    }

    /// Registers a named workspace (needs `editor_panel` and an id prefixed by this extension).
    /// The initial layout is validated before the registration is claimed.
    ///
    /// # Errors
    ///
    /// `manifest_invalid` for an empty title or invalid layout; `capability_not_granted` or
    /// `registration_conflict` for a missing grant or an unavailable id.
    pub fn register_workspace(&mut self, workspace: WorkspaceDesc) -> Result<(), ExtensionError> {
        if workspace.title.trim().is_empty() {
            return Err(ExtensionError::Invalid(
                "workspace title must not be empty".into(),
            ));
        }
        if workspace.layout.format_version > crate::editor_layout::EDITOR_LAYOUT_FORMAT_VERSION {
            return Err(ExtensionError::Invalid(
                "workspace layout uses a newer format version".into(),
            ));
        }
        workspace
            .layout
            .to_dock()
            .map_err(|e| ExtensionError::Invalid(e.to_string()))?;
        let name = workspace.id.clone();
        self.register_owned(
            RegistrationKind::EditorWorkspace,
            &name,
            |e| &mut e.workspaces,
            workspace,
        )
    }

    /// Registers a viewport tool: a Tools menu entry whose handles the viewport draws and drags
    /// while it is active, each drag one undo step (needs `editor_panel`; ADR-7791). Its id must
    /// start with `<extension id>.`.
    ///
    /// # Errors
    ///
    /// `capability_not_granted`; `registration_conflict` for a taken id or a foreign prefix.
    pub fn register_viewport_tool(&mut self, tool: ViewportToolDesc) -> Result<(), ExtensionError> {
        let name = tool.id.clone();
        self.register_owned(RegistrationKind::ViewportTool, &name, |e| &mut e.tools, tool)
    }

    /// Registers an overlay: a small panel drawn over the viewport in a corner (needs
    /// `editor_panel`; ADR-7791). Its id must start with `<extension id>.`.
    ///
    /// # Errors
    ///
    /// `capability_not_granted`; `registration_conflict` for a taken id or a foreign prefix.
    pub fn register_overlay(&mut self, overlay: OverlayDesc) -> Result<(), ExtensionError> {
        let name = overlay.id.clone();
        self.register_owned(RegistrationKind::Overlay, &name, |e| &mut e.overlays, overlay)
    }

    /// Registers a typed setting shown in Preferences (user scope) or Project Settings (project
    /// scope) under the extension's name (needs `editor_panel`; ADR-7791). Its key must be
    /// `<extension id>.<name>` with `name` of ASCII letters, digits and `_`, and its default must
    /// fit its own range and choices.
    ///
    /// # Errors
    ///
    /// `capability_not_granted`; `registration_conflict` for a taken key or a foreign prefix;
    /// `manifest_invalid` for a bad name or a default that does not fit.
    pub fn register_setting(&mut self, setting: SettingDesc) -> Result<(), ExtensionError> {
        let name_ok = !setting.name().is_empty()
            && setting
                .name()
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_');
        if !name_ok {
            return Err(ExtensionError::Invalid(format!(
                "setting `{}`: the name after `<extension>.` is ASCII letters, digits and `_`",
                setting.key
            )));
        }
        if let Err(why) = setting.check(&setting.default) {
            return Err(ExtensionError::Invalid(format!(
                "setting `{}`: the default does not fit: {why}",
                setting.key
            )));
        }
        let name = setting.key.clone();
        self.register_owned(
            RegistrationKind::EditorSetting,
            &name,
            |e| &mut e.settings,
            setting,
        )
    }

    /// Registers a before-cook, after-package or before-launch hook (needs `editor_panel`;
    /// ADR-7791). Its id must start with `<extension id>.`.
    ///
    /// # Errors
    ///
    /// `capability_not_granted`; `registration_conflict` for a taken id or a foreign prefix.
    pub fn register_build_hook(&mut self, hook: BuildHookDesc) -> Result<(), ExtensionError> {
        let name = hook.id.clone();
        self.register_owned(RegistrationKind::BuildHook, &name, |e| &mut e.hooks, hook)
    }

    /// Replaces the inspector's widget for one reflected type (needs `editor_panel`).
    ///
    /// # Errors
    ///
    /// `capability_not_granted`; `registration_conflict` for a type already customized.
    pub fn register_inspector(
        &mut self,
        customization: InspectorCustomization,
    ) -> Result<(), ExtensionError> {
        self.cx.require(
            "editor_panel",
            format!("inspector for `{}`", customization.type_path),
        )?;
        let id = self.cx.id().to_owned();
        self.cx
            .claim(RegistrationKind::Inspector, &customization.type_path)?;
        self.cx
            .world_mut()
            .resource_mut::<EditorExtensions>()
            .inspectors
            .insert(customization.type_path.clone(), (id, customization));
        Ok(())
    }
}

impl ToolExtensionContext<'_, '_> {
    /// Registers a component action: an Inspector button (and `editor.run_component_action`)
    /// that runs on one entity's component as one undo step (needs `editor_panel`; ADR-7785,
    /// [`crate::inspector::actions`]). Registering the same action again replaces it.
    ///
    /// # Errors
    ///
    /// `capability_not_granted`; `registration_conflict` when another owner registered it.
    pub fn register_component_action(
        &mut self,
        action: crate::inspector::actions::ComponentAction,
    ) -> Result<(), ExtensionError> {
        let name = format!("{}.{}", action.component, action.id);
        self.cx
            .require("editor_panel", format!("component action `{name}`"))?;
        let id = self.cx.id().to_owned();
        let world = self.cx.world_mut();
        world.init_resource::<crate::inspector::actions::ComponentActions>();
        world
            .resource_mut::<crate::inspector::actions::ComponentActions>()
            .register(&id, action)
            .map_err(|owner| ExtensionError::RegistrationConflict {
                kind: "component_action",
                name,
                owner,
            })
    }
}

impl<'a> Deref for ToolExtensionContext<'_, 'a> {
    type Target = ExtensionContext<'a>;

    fn deref(&self) -> &Self::Target {
        self.cx
    }
}

impl DerefMut for ToolExtensionContext<'_, '_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.cx
    }
}

/// Live workspace descriptors, excluding registrations removed during disable or reload.
pub fn live_workspaces(world: &World) -> impl Iterator<Item = (&str, &WorkspaceDesc)> {
    let registry = world.get_resource::<regia_framework::extension::ExtensionRegistry>();
    world
        .get_resource::<EditorExtensions>()
        .into_iter()
        .flat_map(|e| e.workspaces.values())
        .filter(move |(owner, desc)| {
            registry.is_some_and(|r| {
                r.owner_of(RegistrationKind::EditorWorkspace, &desc.id) == Some(owner.as_str())
            })
        })
        .map(|(owner, desc)| (owner.as_str(), desc))
}
