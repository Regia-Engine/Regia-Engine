// SPDX-License-Identifier: MPL-2.0
//! The Overrides window of a prefab instance (`docs/specs/19-editor.md`, UI parity addendum § 9.1)
//! and the Edit in place entry (§ 9.2).
//!
//! For the instance a selected entity belongs to (the level-authored instance root, nested
//! instances included) [`override_rows`] lists every difference from the prefab: each changed
//! field with the prefab's value and the instance's side by side, each added or removed component,
//! each removed child and each child the level added under the instance. A field row has Revert
//! and Apply; Apply writes into the target the "Apply to" picker names (the nested prefab, the
//! outer prefab, the variant file or the variant chain's base,
//! [`crate::inspector::multi::prefab_apply_targets`]), preselected to the one the inspector's
//! Apply picks by itself. Revert all and Apply all, and any batch of rows, are one undo step
//! ([`apply_rows`], [`revert_rows`]): each row's own Apply or Revert is folded into one group.
//! Structural rows are listed, not applied: writing an added or removed component or child into a
//! prefab file has no path yet (the window says so on the row).
//!
//! The window opens from the command palette (Prefab: Overrides…), the Outliner's override dot and
//! the Inspector's override rows; every control is a focusable egui widget (Tab, Enter, arrows in
//! the picker). Agent methods: `editor.prefab_overrides`, `editor.prefab_overrides_apply`,
//! `editor.prefab_overrides_revert`, `editor.prefab_overrides_open` and `editor.prefab_edit_in_place`
//! (`book/agents/prefabs.md`).

use std::path::{Path, PathBuf};

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_core::id::Guid;
use regia_ecs::prelude::*;
use regia_ecs::reflect::{AppTypeRegistry, GetPath};
use regia_ecs::serialize::ReflectCodec;
use regia_scene::prefab::{BakedValue, PrefabInstance, PrefabLibrary, PrefabLink};
use serde_json::{Value, json};

use crate::agent_parity::{self as parity};
use crate::command::{CommandGroup, EditorCommand, UndoStack, resolve_entity};
use crate::inspector::EditError;
use crate::inspector::multi::{ApplyChoice, ApplyTarget, apply_prefab_override_to, prefab_apply_targets};
use crate::viewport_agent::Handler;

/// What a row of the Overrides window shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowKind {
    /// A field whose value differs from the prefab's.
    Field,
    /// A component the instance adds to a prefab entity.
    AddedComponent,
    /// A component of a prefab entity the instance removes.
    RemovedComponent,
    /// A child the level added under the instance.
    AddedChild,
    /// A prefab entity the instance removes.
    RemovedChild,
}

impl RowKind {
    /// The protocol name.
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::Field => "field",
            Self::AddedComponent => "added_component",
            Self::RemovedComponent => "removed_component",
            Self::AddedChild => "added_child",
            Self::RemovedChild => "removed_child",
        }
    }
}

/// One difference between an instance and its prefab.
#[derive(Clone, Debug, PartialEq)]
pub struct OverrideRow {
    /// What differs.
    pub kind: RowKind,
    /// The live entity the row is about (the prefab entity, the added child, or for a removed
    /// child the instance root that removes it), as entity bits.
    pub entity: u64,
    /// The entity's name, or its prefab-local id when it has none.
    pub entity_name: String,
    /// The component (empty for child rows).
    pub component: String,
    /// The field path inside the component, without a leading dot (empty: the whole component).
    pub path: String,
    /// The prefab's value (RON text), or a short description for structural rows.
    pub prefab_value: String,
    /// The instance's value (RON text), or a short description for structural rows.
    pub instance_value: String,
    /// Where an Apply of a field row may write, in picker order.
    pub targets: Vec<ApplyChoice>,
    /// The target the inspector's Apply picks by itself (`None`: the session-only Apply, or none).
    pub default_target: Option<ApplyTarget>,
}

impl OverrideRow {
    /// Whether the row has Revert and Apply (field rows do; structural rows are listed only).
    #[must_use]
    pub fn editable(&self) -> bool {
        self.kind == RowKind::Field
    }

    /// The row as the agent sees it; files are relative to `project` when they are inside it.
    #[must_use]
    pub fn to_json(&self, project: Option<&Path>) -> Value {
        let targets: Vec<Value> = self
            .targets
            .iter()
            .map(|choice| {
                json!({
                    "target": choice.target.id(),
                    "label": choice.target.label(),
                    "file": relative(project, &choice.file),
                })
            })
            .collect();
        json!({
            "kind": self.kind.id(),
            "entity": self.entity,
            "name": self.entity_name,
            "component": self.component,
            "field": self.path,
            "prefab_value": self.prefab_value,
            "instance_value": self.instance_value,
            "editable": self.editable(),
            "targets": targets,
            "default_target": self.default_target.map(ApplyTarget::id),
        })
    }
}

/// `path` relative to `project` with `/` separators (as is when outside it).
fn relative(project: Option<&Path>, path: &Path) -> String {
    project
        .and_then(|root| path.strip_prefix(root).ok())
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Every override of one instance.
#[derive(Clone, Debug, PartialEq)]
pub struct OverrideList {
    /// The level-authored instance root.
    pub root: Entity,
    /// The instance's prefab.
    pub prefab: Guid,
    /// The rows: the outer instance's first, then each nested instance's, in entity order.
    pub rows: Vec<OverrideRow>,
}

fn edit_error(code: &'static str, message: impl Into<String>) -> EditError {
    EditError {
        code,
        message: message.into(),
    }
}

/// The level-authored instance root `entity` belongs to (itself, or the first ancestor that is an
/// instance root and not prefab-internal).
fn authored_root(world: &World, entity: Entity) -> Option<Entity> {
    let mut cursor = entity;
    for _ in 0..256 {
        if world.get::<PrefabInstance>(cursor).is_some() && world.get::<PrefabLink>(cursor).is_none() {
            return Some(cursor);
        }
        cursor = world.get::<ChildOf>(cursor)?.parent();
    }
    None
}

/// The nearest instance root above `entity` (not `entity` itself).
fn nearest_root(world: &World, entity: Entity) -> Option<Entity> {
    let mut cursor = world.get::<ChildOf>(entity)?.parent();
    for _ in 0..256 {
        if world.get::<PrefabInstance>(cursor).is_some() {
            return Some(cursor);
        }
        cursor = world.get::<ChildOf>(cursor)?.parent();
    }
    None
}

/// `root` and every prefab-internal entity under it (the walk does not enter added children),
/// in entity order after the root.
fn internal_subtree(world: &World, root: Entity) -> Vec<Entity> {
    let mut found = Vec::new();
    let mut stack = vec![root];
    while let Some(at) = stack.pop() {
        let Some(children) = world.get::<Children>(at) else {
            continue;
        };
        for child in children.iter() {
            if world.get::<PrefabLink>(child).is_some() && found.len() < 65_536 {
                found.push(child);
                stack.push(child);
            }
        }
    }
    found.sort_unstable();
    found.insert(0, root);
    found
}

fn name_of(world: &World, entity: Entity, local: Option<u128>) -> String {
    if let Some(name) = world.get::<regia_scene::prelude::Name>(entity) {
        return name.as_str().to_string();
    }
    if let Some(name) = world.get::<regia_ecs::ecs::name::Name>(entity) {
        return name.as_str().to_string();
    }
    local.map_or_else(
        || entity.to_bits().to_string(),
        |id| Guid::from_u128(id).to_string(),
    )
}

fn strip_dot(path: &str) -> &str {
    path.strip_prefix('.').unwrap_or(path)
}

fn short(component: &str) -> &str {
    component.rsplit("::").next().unwrap_or(component)
}

fn same_component(left: &str, right: &str) -> bool {
    left == right || short(left) == short(right)
}

/// The prefab-side value of `component.path` on the prefab entity `local` of `asset`: the
/// template's value, then each variant step's override of exactly that field.
fn prefab_value(world: &World, asset: Guid, local: u128, component: &str, path: &str) -> Option<String> {
    let library = world.get_resource::<PrefabLibrary>()?;
    let resolved = library.resolve(asset).ok()?;
    let registry = world.get_resource::<AppTypeRegistry>()?.clone();
    let registry = registry.read();
    let mut value = resolved
        .template
        .entities
        .iter()
        .find(|item| item.local_id.as_u128() == local)
        .and_then(|item| {
            item.components
                .iter()
                .find(|(type_path, _)| same_component(type_path, component))
        })
        .and_then(|(_, baked)| match baked {
            BakedValue::Reflected(value) => {
                let field = if path.is_empty() {
                    Some(value.as_partial_reflect())
                } else {
                    value.reflect_path(path).ok()
                };
                field.and_then(|field| ReflectCodec::new(&registry).to_ron_typed(field).ok())
            }
            BakedValue::Raw(raw, _) => path.is_empty().then(|| raw.clone()),
        });
    for step in &resolved.chain {
        for edit in &step.edits.overrides {
            if edit.target.as_u128() == local
                && same_component(&edit.component, component)
                && strip_dot(&edit.path) == path
            {
                value = Some(edit.value.get_ron().to_string());
            }
        }
    }
    value.map(|text| text.trim().to_string())
}

/// The override entries a prefab that nests `root` wrote on its nested record (template data,
/// not the instance's own edits): `(local, component, path, value)`.
fn template_owned(world: &World, root: Entity) -> Vec<(u128, String, String, String)> {
    let (Some(link), Some(parent), Some(library)) = (
        world.get::<PrefabLink>(root),
        nearest_root(world, root),
        world.get_resource::<PrefabLibrary>(),
    ) else {
        return Vec::new();
    };
    let Some(template) = world
        .get::<PrefabInstance>(parent)
        .and_then(|instance| library.resolve(instance.prefab_guid()).ok())
    else {
        return Vec::new();
    };
    template
        .template
        .entities
        .iter()
        .find(|item| item.local_id.as_u128() == link.local_id)
        .and_then(|item| item.nested.as_ref())
        .map(|nested| {
            nested
                .instance
                .overrides
                .iter()
                .map(|entry| {
                    (
                        entry.target.as_u128(),
                        entry.component.clone(),
                        strip_dot(&entry.path).to_string(),
                        entry.value.get_ron().trim().to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The live member of the instance rooted at `root` for the prefab-local id `local` (nearest
/// instance root is `root`).
fn member_of(world: &World, root: Entity, members: &[Entity], local: u128) -> Option<Entity> {
    members.iter().copied().find(|member| {
        *member != root
            && world
                .get::<PrefabLink>(*member)
                .is_some_and(|link| link.local_id == local)
            && nearest_root(world, *member) == Some(root)
    })
}

/// Every override of the instance `entity` belongs to (see the module docs).
///
/// # Errors
/// [`EditError`] `not_found` (no such entity) or `not_prefab_instance`.
pub fn override_rows(world: &World, entity: Entity) -> Result<OverrideList, EditError> {
    if world.get_entity(entity).is_err() {
        return Err(edit_error(
            "not_found",
            format!("entity {} does not exist", entity.to_bits()),
        ));
    }
    let outer = authored_root(world, entity).ok_or_else(|| {
        edit_error(
            "not_prefab_instance",
            "the entity is not a prefab instance or inside one",
        )
    })?;
    let prefab = world
        .get::<PrefabInstance>(outer)
        .map(PrefabInstance::prefab_guid)
        .ok_or_else(|| edit_error("not_prefab_instance", "the instance root lost PrefabInstance"))?;
    let members = internal_subtree(world, outer);
    let mut rows = Vec::new();
    for root in members
        .iter()
        .copied()
        .filter(|at| world.get::<PrefabInstance>(*at).is_some())
    {
        let Some(instance) = world.get::<PrefabInstance>(root) else {
            continue;
        };
        let asset = instance.prefab_guid();
        let owned = if root == outer {
            Vec::new()
        } else {
            template_owned(world, root)
        };
        for entry in &instance.overrides {
            let path = strip_dot(&entry.path).to_string();
            let value = entry.value.trim().to_string();
            if owned.iter().any(|(local, component, owned_path, owned_value)| {
                *local == entry.target
                    && same_component(component, &entry.component)
                    && *owned_path == path
                    && *owned_value == value
            }) {
                continue;
            }
            let Some(member) = member_of(world, root, &members, entry.target) else {
                continue;
            };
            let (targets, default_target) = prefab_apply_targets(world, member, &entry.component, &path);
            rows.push(OverrideRow {
                kind: RowKind::Field,
                entity: member.to_bits(),
                entity_name: name_of(world, member, Some(entry.target)),
                component: entry.component.clone(),
                prefab_value: prefab_value(world, asset, entry.target, &entry.component, &path)
                    .unwrap_or_else(|| "(unknown)".to_string()),
                path,
                instance_value: value,
                targets,
                default_target,
            });
        }
        for added in &instance.added_components {
            let Some(member) = member_of(world, root, &members, added.target) else {
                continue;
            };
            rows.push(structural(
                world,
                RowKind::AddedComponent,
                member,
                Some(added.target),
                &added.component,
                ("(none)", added.value.trim()),
            ));
        }
        for removed in &instance.removed_components {
            let Some(member) = member_of(world, root, &members, removed.target) else {
                continue;
            };
            rows.push(structural(
                world,
                RowKind::RemovedComponent,
                member,
                Some(removed.target),
                &removed.component,
                ("(present)", "(removed)"),
            ));
        }
        for local in &instance.removed_children {
            let name = world
                .get_resource::<PrefabLibrary>()
                .and_then(|library| library.resolve(asset).ok())
                .and_then(|resolved| {
                    resolved
                        .template
                        .entities
                        .iter()
                        .find(|item| item.local_id.as_u128() == *local)
                        .and_then(|item| item.name.clone())
                })
                .unwrap_or_else(|| Guid::from_u128(*local).to_string());
            rows.push(OverrideRow {
                kind: RowKind::RemovedChild,
                entity: root.to_bits(),
                entity_name: name,
                component: String::new(),
                path: String::new(),
                prefab_value: "(present)".to_string(),
                instance_value: "(removed)".to_string(),
                targets: Vec::new(),
                default_target: None,
            });
        }
    }
    // Children the level added under the instance: scene entities hanging off a member.
    for member in &members {
        let Some(children) = world.get::<Children>(*member) else {
            continue;
        };
        let mut added: Vec<Entity> = children
            .iter()
            .filter(|child| world.get::<PrefabLink>(*child).is_none())
            .collect();
        added.sort_unstable();
        for child in added {
            rows.push(OverrideRow {
                kind: RowKind::AddedChild,
                entity: child.to_bits(),
                entity_name: name_of(world, child, None),
                component: String::new(),
                path: String::new(),
                prefab_value: "(none)".to_string(),
                instance_value: "(added)".to_string(),
                targets: Vec::new(),
                default_target: None,
            });
        }
    }
    Ok(OverrideList {
        root: outer,
        prefab,
        rows,
    })
}

fn structural(
    world: &World,
    kind: RowKind,
    member: Entity,
    local: Option<u128>,
    component: &str,
    (prefab, instance): (&str, &str),
) -> OverrideRow {
    OverrideRow {
        kind,
        entity: member.to_bits(),
        entity_name: name_of(world, member, local),
        component: component.to_string(),
        path: String::new(),
        prefab_value: prefab.to_string(),
        instance_value: instance.to_string(),
        targets: Vec::new(),
        default_target: None,
    }
}

/// One field row of a batch: the member, the component and field, and for an Apply the target
/// (`None`: the row's default).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowRef {
    /// The prefab entity whose field is overridden.
    pub entity: Entity,
    /// The component.
    pub component: String,
    /// The field path (empty: the whole component).
    pub path: String,
    /// Where an Apply writes (`None`: the row's default target).
    pub target: Option<ApplyTarget>,
}

/// What a batch did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchReport {
    /// Rows applied or reverted.
    pub done: usize,
    /// Undo entries afterwards.
    pub undo_depth: usize,
    /// The files the Apply wrote, in row order, without repeats.
    pub files: Vec<PathBuf>,
}

fn undo_depth(world: &World) -> usize {
    world.get_resource::<UndoStack>().map_or(0, UndoStack::undo_depth)
}

/// Takes the top `count` entries off the undo stack, in apply order.
fn take_entries(world: &mut World, count: usize) -> Vec<Box<dyn EditorCommand>> {
    let mut taken = Vec::with_capacity(count);
    if let Some(mut stack) = world.get_resource_mut::<UndoStack>() {
        for _ in 0..count {
            match stack.take_top() {
                Some(command) => taken.push(command),
                None => break,
            }
        }
    }
    taken.reverse();
    taken
}

/// Runs `each` on every row; on success the rows' undo entries become one, on failure the rows
/// already done are reverted and the error returned.
fn batch(
    world: &mut World,
    rows: &[RowRef],
    label: &str,
    mut each: impl FnMut(&mut World, &RowRef) -> Result<Option<PathBuf>, EditError>,
) -> Result<BatchReport, EditError> {
    if rows.is_empty() {
        return Err(edit_error("rows_empty", "no rows to apply or revert"));
    }
    let mut seen: Vec<(Entity, String, String)> = Vec::new();
    let mut done = 0usize;
    let mut files: Vec<PathBuf> = Vec::new();
    for row in rows {
        let key = (
            row.entity,
            short(&row.component).to_string(),
            strip_dot(&row.path).to_string(),
        );
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        let before = undo_depth(world);
        match each(world, row) {
            Ok(file) => {
                // An Apply or Revert pushes one entry (a no-op pushes none).
                done += usize::from(undo_depth(world) > before);
                if let Some(file) = file
                    && !files.contains(&file)
                {
                    files.push(file);
                }
            }
            Err(err) => {
                for mut command in take_entries(world, done).into_iter().rev() {
                    command.revert(world);
                }
                return Err(err);
            }
        }
    }
    if done > 1 {
        let mut group = CommandGroup::new(format!("{label} {done} overrides"));
        for command in take_entries(world, done) {
            group.push(command);
        }
        crate::undo_agent::push_discrete(world, Box::new(group));
    }
    Ok(BatchReport {
        done: seen.len(),
        undo_depth: undo_depth(world),
        files,
    })
}

/// Applies each of `rows` into its target (its own `target`, else the row's default) as one undo
/// step: one undo restores every file written and every override.
///
/// # Errors
/// `rows_empty`, or the first row's error ([`apply_prefab_override_to`]: `override_absent`,
/// `apply_target_invalid`, `prefab_apply_failed`, ...); rows already applied are reverted.
pub fn apply_rows(world: &mut World, rows: &[RowRef]) -> Result<BatchReport, EditError> {
    batch(world, rows, "Apply", |world, row| {
        let (choices, default) = prefab_apply_targets(world, row.entity, &row.component, &row.path);
        let wanted = row.target.or(default);
        let file = choices
            .into_iter()
            .find(|choice| Some(choice.target) == wanted)
            .map(|choice| choice.file);
        apply_prefab_override_to(world, row.entity, &row.component, &row.path, row.target)?;
        Ok(file)
    })
}

/// Reverts each of `rows` (the prefab's value comes back on this instance) as one undo step.
///
/// # Errors
/// `rows_empty`, or the first row's error (`override_absent`, ...); rows already reverted are
/// put back.
pub fn revert_rows(world: &mut World, rows: &[RowRef]) -> Result<BatchReport, EditError> {
    batch(world, rows, "Revert", |world, row| {
        crate::inspector::revert_prefab_override(world, row.entity, &row.component, &row.path)?;
        Ok(None)
    })
}

/// The field rows of `list` as batch rows with `target` (`None`: each row's default; a row that
/// cannot take `target` keeps its default).
#[must_use]
pub fn all_rows(world: &World, list: &OverrideList, target: Option<ApplyTarget>) -> Vec<RowRef> {
    list.rows
        .iter()
        .filter(|row| row.editable())
        .filter_map(|row| {
            let entity = resolve_entity(world, row.entity)?;
            let target = target.filter(|target| row.targets.iter().any(|choice| choice.target == *target));
            Some(RowRef {
                entity,
                component: row.component.clone(),
                path: row.path.clone(),
                target,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Window

/// The Overrides window's state (editor-session, not saved).
#[derive(Resource, Debug, Clone, Default)]
pub struct PrefabOverridesWindow {
    /// Whether the window shows.
    pub open: bool,
    /// The entity it was opened for, as entity bits (its instance is listed).
    pub entity: Option<u64>,
    /// The "Apply to" choice (`None`: each row's default target).
    pub target: Option<ApplyTarget>,
    /// The last action's result, shown at the bottom.
    pub status: String,
    /// The egui frame the last action ran in (egui may run a panel twice in one frame).
    last_action_frame: Option<u64>,
}

/// Opens the Overrides window for the instance `entity` belongs to.
pub fn open_for(world: &mut World, entity: Entity) {
    let mut window = world.get_resource_or_insert_with(PrefabOverridesWindow::default);
    window.open = true;
    window.entity = Some(entity.to_bits());
    window.target = None;
    window.status.clear();
}

/// What a click asked the window to do.
enum WindowAction {
    Apply(Vec<RowRef>),
    Revert(Vec<RowRef>),
    EditInPlace(Entity),
    Close,
}

/// The window, drawn by the shell every frame (nothing while it is closed).
pub fn window(ctx: &egui::Context, world: &mut World) {
    if !world
        .get_resource::<PrefabOverridesWindow>()
        .is_some_and(|window| window.open)
    {
        return;
    }
    let mut open = true;
    egui::Window::new("Prefab overrides")
        .id(egui::Id::new("prefab-overrides-window"))
        .open(&mut open)
        .resizable(true)
        .default_width(560.0)
        .show(ctx, |ui| overrides_ui(ui, world));
    if !open && let Some(mut window) = world.get_resource_mut::<PrefabOverridesWindow>() {
        window.open = false;
    }
}

/// The window's contents (the headless tests draw it in a central panel).
pub fn overrides_ui(ui: &mut egui::Ui, world: &mut World) {
    let _span = tracing::trace_span!("prefab_overrides_ui").entered();
    let state = world
        .get_resource::<PrefabOverridesWindow>()
        .cloned()
        .unwrap_or_default();
    let Some(entity) = state.entity.and_then(|bits| resolve_entity(world, bits)) else {
        ui.label("Select a prefab instance, then open its overrides.");
        return;
    };
    let list = match override_rows(world, entity) {
        Ok(list) => list,
        Err(err) => {
            ui.label(format!("No overrides to show: {}", err.message));
            return;
        }
    };
    let project = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|project| project.root.clone());
    let mut target = state.target;
    let mut action = None;

    ui.horizontal(|ui| {
        ui.label(format!("Instance: {}", name_of(world, list.root, None)));
        if parity::action_button(ui, &parity::PREFAB_EDIT_IN_PLACE)
            .on_hover_text("Open the prefab in place: the level stays drawn around it, read-only")
            .clicked()
        {
            action = Some(WindowAction::EditInPlace(list.root));
        }
    });
    // The targets any row can take, in picker order.
    let mut offered: Vec<ApplyTarget> = Vec::new();
    for row in &list.rows {
        for choice in &row.targets {
            if !offered.contains(&choice.target) {
                offered.push(choice.target);
            }
        }
    }
    offered.sort_by_key(|target| ApplyTarget::ALL.iter().position(|t| t == target));
    ui.horizontal(|ui| {
        let label = ui.label("Apply to");
        let shown = target.map_or("Each row's default", ApplyTarget::label);
        let _ = egui::ComboBox::from_id_salt("prefab-overrides-target")
            .selected_text(shown)
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut target, None, "Each row's default");
                for choice in &offered {
                    ui.selectable_value(&mut target, Some(*choice), choice.label());
                }
            })
            .response
            .labelled_by(label.id);
    });
    let editable = list.rows.iter().any(OverrideRow::editable);
    ui.horizontal(|ui| {
        if parity::action_enabled(ui, editable, &parity::PREFAB_OVERRIDES_APPLY_ALL, None).clicked() {
            action = Some(WindowAction::Apply(all_rows(world, &list, target)));
        }
        if parity::action_enabled(ui, editable, &parity::PREFAB_OVERRIDES_REVERT_ALL, None).clicked() {
            action = Some(WindowAction::Revert(all_rows(world, &list, None)));
        }
        if parity::action_button(ui, &parity::PREFAB_OVERRIDES_CLOSE).clicked() {
            action = Some(WindowAction::Close);
        }
    });
    ui.separator();
    if list.rows.is_empty() {
        ui.label("This instance matches its prefab.");
    }
    egui::ScrollArea::vertical()
        .max_height(ui.available_height().max(120.0))
        .show(ui, |ui| {
            egui::Grid::new("prefab-overrides-rows")
                .striped(true)
                .num_columns(5)
                .show(ui, |ui| {
                    ui.strong("Entity");
                    ui.strong("Override");
                    ui.strong("Prefab");
                    ui.strong("Instance");
                    ui.strong("");
                    ui.end_row();
                    for (index, row) in list.rows.iter().enumerate() {
                        ui.label(&row.entity_name);
                        let what = match row.kind {
                            RowKind::Field if row.path.is_empty() => short(&row.component).to_string(),
                            RowKind::Field => format!("{}.{}", short(&row.component), row.path),
                            RowKind::AddedComponent => format!("Added {}", short(&row.component)),
                            RowKind::RemovedComponent => format!("Removed {}", short(&row.component)),
                            RowKind::AddedChild => "Added child".to_string(),
                            RowKind::RemovedChild => "Removed child".to_string(),
                        };
                        ui.label(&what);
                        ui.label(&row.prefab_value);
                        ui.label(&row.instance_value);
                        ui.push_id(index, |ui| {
                            ui.horizontal(|ui| {
                                if !row.editable() {
                                    ui.weak("Apply and Revert are not available for this kind of override yet");
                                    return;
                                }
                                let Some(member) = resolve_entity(world, row.entity) else {
                                    return;
                                };
                                let row_target = target.filter(|t| row.targets.iter().any(|c| c.target == *t));
                                let file = row
                                    .targets
                                    .iter()
                                    .find(|choice| Some(choice.target) == row_target.or(row.default_target))
                                    .map(|choice| relative(project.as_deref(), &choice.file));
                                let apply_name = format!("Apply {what}");
                                let revert_name = format!("Revert {what}");
                                let applied = parity::action_button(ui, &parity::PREFAB_OVERRIDES_APPLY);
                                let applied = crate::a11y_audit::name(applied, egui::WidgetType::Button, &apply_name)
                                    .on_hover_text(file.map_or_else(
                                        || "Apply to the prefab (this session only: it has no file in the project)".to_string(),
                                        |file| format!("Apply to {file}"),
                                    ));
                                if applied.clicked() {
                                    action = Some(WindowAction::Apply(vec![RowRef {
                                        entity: member,
                                        component: row.component.clone(),
                                        path: row.path.clone(),
                                        target: row_target,
                                    }]));
                                }
                                let reverted = parity::action_button(ui, &parity::PREFAB_OVERRIDES_REVERT);
                                if crate::a11y_audit::name(reverted, egui::WidgetType::Button, &revert_name).clicked() {
                                    action = Some(WindowAction::Revert(vec![RowRef {
                                        entity: member,
                                        component: row.component.clone(),
                                        path: row.path.clone(),
                                        target: None,
                                    }]));
                                }
                            });
                        });
                        ui.end_row();
                    }
                });
        });
    if !state.status.is_empty() {
        ui.separator();
        ui.label(&state.status);
    }

    let frame = ui.ctx().cumulative_frame_nr();
    let ran = state.last_action_frame == Some(frame);
    let status = match action {
        // egui can run the window twice in one frame: the second pass must not act again.
        Some(_) if ran => None,
        Some(WindowAction::Apply(rows)) => Some(match apply_rows(world, &rows) {
            Ok(report) => format!("Applied {} override(s)", report.done),
            Err(err) => format!("Apply failed: {}", err.message),
        }),
        Some(WindowAction::Revert(rows)) => Some(match revert_rows(world, &rows) {
            Ok(report) => format!("Reverted {} override(s)", report.done),
            Err(err) => format!("Revert failed: {}", err.message),
        }),
        Some(WindowAction::EditInPlace(root)) => Some(
            match crate::prefab_edit::enter_prefab_edit_in_context(world, root) {
                Ok(path) => {
                    if let Some(mut window) = world.get_resource_mut::<PrefabOverridesWindow>() {
                        window.open = false;
                    }
                    format!("Editing {} in place", relative(project.as_deref(), &path))
                }
                Err(err) => format!("Edit in place failed: {err}"),
            },
        ),
        Some(WindowAction::Close) => {
            if let Some(mut window) = world.get_resource_mut::<PrefabOverridesWindow>() {
                window.open = false;
            }
            None
        }
        None => None,
    };
    let mut window = world.get_resource_or_insert_with(PrefabOverridesWindow::default);
    window.target = target;
    if let Some(status) = status {
        window.status = status;
        window.last_action_frame = Some(frame);
    }
}

// ---------------------------------------------------------------------------------------------
// Agent methods

fn entity_param(world: &World, params: &Value, name: &str) -> Result<Entity, RpcError> {
    let bits = params
        .get(name)
        .and_then(Value::as_u64)
        .ok_or_else(|| RpcError::invalid_params(format!("`{name}` (integer entity id) is required")))?;
    resolve_entity(world, bits).ok_or_else(|| {
        RpcError::not_found(
            format!("entity {bits} does not exist"),
            "use scene.query_entities; an undo may have removed it",
        )
    })
}

fn rpc_error(err: EditError) -> RpcError {
    let mut rpc = match err.code {
        "not_found" | "not_prefab_instance" | "override_absent" => RpcError::not_found(
            err.message,
            "pick an entity inside a prefab instance; editor.prefab_overrides lists its overrides",
        ),
        _ => RpcError::invalid_params(err.message),
    };
    let data = rpc.data.get_or_insert_with(|| json!({}));
    if let Value::Object(data) = data {
        data.insert("reason".into(), Value::String(err.code.to_string()));
    }
    rpc
}

fn target_param(value: Option<&Value>) -> Result<Option<ApplyTarget>, RpcError> {
    match value.and_then(Value::as_str) {
        None | Some("" | "default") => Ok(None),
        Some(text) => ApplyTarget::parse(text).map(Some).ok_or_else(|| {
            let mut rpc = RpcError::invalid_params(format!(
                "unknown target `{text}`: one of nested, outer, variant, base"
            ));
            if let Some(Value::Object(data)) = rpc.data.as_mut() {
                data.insert("reason".into(), Value::String("apply_target_unknown".into()));
            } else {
                rpc.data = Some(json!({ "reason": "apply_target_unknown" }));
            }
            rpc
        }),
    }
}

fn project_root(world: &World) -> Option<PathBuf> {
    world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|project| project.root.clone())
}

/// `editor.prefab_overrides { entity }`.
fn editor_prefab_overrides(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = entity_param(world, params, "entity")?;
    let list = override_rows(world, entity).map_err(rpc_error)?;
    let project = project_root(world);
    let rows: Vec<Value> = list
        .rows
        .iter()
        .map(|row| row.to_json(project.as_deref()))
        .collect();
    Ok(json!({
        "root": list.root.to_bits(),
        "prefab": list.prefab.to_string(),
        "rows": rows,
    }))
}

/// The batch rows of `editor.prefab_overrides_apply` / `_revert`: `rows`, or every field row of
/// the instance with `target`.
fn rows_param(
    world: &World,
    params: &Value,
    entity: Entity,
    target: Option<ApplyTarget>,
) -> Result<Vec<RowRef>, RpcError> {
    let Some(rows) = params.get("rows").and_then(Value::as_array) else {
        let list = override_rows(world, entity).map_err(rpc_error)?;
        return Ok(all_rows(world, &list, target));
    };
    rows.iter()
        .map(|row| {
            let member = entity_param(world, row, "entity")?;
            let component = row
                .get("component")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .ok_or_else(|| RpcError::invalid_params("each row needs `component`"))?;
            Ok(RowRef {
                entity: member,
                component: component.to_string(),
                path: row.get("field").and_then(Value::as_str).unwrap_or("").to_string(),
                target: target_param(row.get("target"))?.or(target),
            })
        })
        .collect()
}

/// `editor.prefab_overrides_apply { entity, rows?, target? }`.
fn editor_prefab_overrides_apply(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = entity_param(world, params, "entity")?;
    let target = target_param(params.get("target"))?;
    let rows = rows_param(world, params, entity, target)?;
    let report = apply_rows(world, &rows).map_err(rpc_error)?;
    let project = project_root(world);
    let files: Vec<String> = report
        .files
        .iter()
        .map(|file| relative(project.as_deref(), file))
        .collect();
    Ok(json!({ "applied": report.done, "undo_depth": report.undo_depth, "files": files }))
}

/// `editor.prefab_overrides_revert { entity, rows? }`.
fn editor_prefab_overrides_revert(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = entity_param(world, params, "entity")?;
    let rows = rows_param(world, params, entity, None)?;
    let report = revert_rows(world, &rows).map_err(rpc_error)?;
    Ok(json!({ "reverted": report.done, "undo_depth": report.undo_depth }))
}

/// `editor.prefab_overrides_open { entity }`: the window, for a person watching.
fn editor_prefab_overrides_open(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = entity_param(world, params, "entity")?;
    let list = override_rows(world, entity).map_err(rpc_error)?;
    open_for(world, entity);
    Ok(json!({ "open": true, "root": list.root.to_bits(), "rows": list.rows.len() }))
}

/// `editor.prefab_edit_in_place { entity }`.
fn editor_prefab_edit_in_place(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = entity_param(world, params, "entity")?;
    match crate::prefab_edit::enter_prefab_edit_in_context(world, entity) {
        Ok(path) => {
            let project = project_root(world);
            Ok(json!({ "editing": relative(project.as_deref(), &path), "in_context": true }))
        }
        Err(message) => {
            let mut rpc = RpcError::invalid_params(format!("cannot edit in place: {message}"));
            let reason = if message.contains("variant") {
                "prefab_variant_in_place"
            } else if message.contains("not a prefab instance") {
                "not_prefab_instance"
            } else {
                "prefab_edit_failed"
            };
            rpc.data = Some(json!({ "reason": reason }));
            Err(rpc)
        }
    }
}

const ENTITY: ParamSchema = ParamSchema {
    name: "entity",
    kind: "integer",
    required: true,
    description: "The instance root or any entity inside the instance.",
};

const ROWS: ParamSchema = ParamSchema {
    name: "rows",
    kind: "array",
    required: false,
    description: "Field rows [{ entity, component, field?, target? }] (entity: the prefab entity whose field is overridden, as editor.prefab_overrides lists it); omitted: every field row of the instance.",
};

/// The methods of this module.
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.prefab_overrides",
                tool: "prefab_overrides",
                description: "The Overrides window's rows for the prefab instance an entity belongs to: every changed field (prefab value and instance value side by side, the valid Apply targets and the default one), every added or removed component, every removed child and every child the level added. Nested instances' own overrides are included; overrides a nesting prefab wrote are template data and are not.",
                params: &[ENTITY],
                result: "{ root, prefab, rows: [{ kind, entity, name, component, field, prefab_value, instance_value, editable, targets: [{ target, label, file }], default_target }] }",
                mutating: false,
                permission: Permission::Read,
            },
            editor_prefab_overrides as Handler,
        ),
        (
            MethodSchema {
                name: "editor.prefab_overrides_apply",
                tool: "prefab_overrides_apply",
                description: "Apply field overrides of a prefab instance (the Overrides window's Apply and Apply all) into their targets: nested (the nested prefab's file), outer (the outer prefab's file), variant (the variant file) or base (the variant chain's base file); omitted: each row's default, what the inspector's Apply picks. Every row is one undo step together; a failing row reverts the rows already applied.",
                params: &[
                    ENTITY,
                    ROWS,
                    ParamSchema {
                        name: "target",
                        kind: "string",
                        required: false,
                        description: "nested, outer, variant or base for every row that can take it (a row's own target wins); omitted: each row's default.",
                    },
                ],
                result: "{ applied, undo_depth, files }",
                mutating: true,
                permission: Permission::EditScene,
            },
            editor_prefab_overrides_apply as Handler,
        ),
        (
            MethodSchema {
                name: "editor.prefab_overrides_revert",
                tool: "prefab_overrides_revert",
                description: "Revert field overrides of a prefab instance (the Overrides window's Revert and Revert all): the prefab's values come back on this instance; other instances and the files are untouched. One undo step.",
                params: &[ENTITY, ROWS],
                result: "{ reverted, undo_depth }",
                mutating: true,
                permission: Permission::EditScene,
            },
            editor_prefab_overrides_revert as Handler,
        ),
        (
            MethodSchema {
                name: "editor.prefab_overrides_open",
                tool: "prefab_overrides_open",
                description: "Open the Overrides window for the prefab instance an entity belongs to (view only: what Prefab: Overrides…, the Outliner's override dot and the Inspector's Overrides… do). Agents read the rows with editor.prefab_overrides.",
                params: &[ENTITY],
                result: "{ open, root, rows }",
                mutating: false,
                permission: Permission::Read,
            },
            editor_prefab_overrides_open as Handler,
        ),
        (
            MethodSchema {
                name: "editor.prefab_edit_in_place",
                tool: "prefab_edit_in_place",
                description: "Edit in place: open the prefab of the instance an entity belongs to in prefab edit mode with the rest of the level drawn around it, read-only (not pickable, out of the Outliner, never saved into the prefab), the instance's world transform as the frame and the instance hidden. editor.save_prefab writes the prefab and rebuilds every instance; editor.close_prefab restores the level. A variant cannot be opened (variant edit mode is out of scope).",
                params: &[ENTITY],
                result: "{ editing, in_context }",
                mutating: true,
                permission: Permission::EditScene,
            },
            editor_prefab_edit_in_place as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests;
