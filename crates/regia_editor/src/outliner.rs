// SPDX-License-Identifier: MPL-2.0
//! The Outliner panel: the entity hierarchy tree with search, rename, drag-reparenting,
//! multi-select and a context menu (`docs/specs/19-editor.md` § 2).
//!
//! The hierarchy model (tree building, cycle-safe reparent validation, subtree collection) is
//! pure and unit-tested; the panel draws it with egui and turns interactions into editor
//! commands on the [`UndoStack`]. Multi-entity edits (drag-and-drop of a selection, group,
//! duplicate, delete) go through [`crate::hierarchy_ops`], one undo entry each.

use std::collections::HashSet;

use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_ecs::ecs::entity::Entity;
use regia_ecs::prelude::*;
use regia_scene::prelude::{Name, Transform};

use crate::command::EditorCommand as _;
use crate::command::{SetProperty, SpawnEntities, UndoStack};
use crate::hierarchy_ops::{
    self, CommandBatch, EntityKey, HierarchyError, ReparentEntities, ReparentMove, SelectScope,
};
use crate::selection::{SelectMode, Selection};

/// One outliner row: the entity, its label, its icon, its search facts and children (sorted by
/// name for stable order).
#[derive(Debug, Clone)]
pub struct HierarchyNode {
    /// The entity.
    pub entity: Entity,
    /// Display label (name or the entity id as hex).
    pub label: String,
    /// The row icon: the entity's kind, else its derived role (spec 101 § 4).
    pub icon: crate::kind_icons::RowIcon,
    /// The entity's components' short type names, lowercase (`type:` search terms, spec 19 § 2).
    pub components: Vec<String>,
    /// The entity's tags, lowercase (`tag:` search terms, spec 19 § 2).
    pub tags: Vec<String>,
    /// Children of this entity.
    pub children: Vec<HierarchyNode>,
}

/// The entity's display label. Prefers `regia_scene`'s `Name`; falls back to the ECS's own
/// `Name` (agent `scene.spawn` inserts that one — the two `Name` components coexist until the
/// scene crate adopts the ECS's), then to the entity id as hex.
#[must_use]
pub fn display_name(world: &World, entity: Entity) -> String {
    if let Some(name) = world.get::<Name>(entity) {
        return name.as_str().to_string();
    }
    if let Some(name) = world.get::<regia_ecs::ecs::name::Name>(entity) {
        return name.as_str().to_string();
    }
    format!("{:016x}", entity.to_bits())
}

/// Whether the entity carries a `Name` (either crate's): [`display_name`] then returns it rather
/// than the hex id.
#[must_use]
pub fn has_name(world: &World, entity: Entity) -> bool {
    world.get::<Name>(entity).is_some() || world.get::<regia_ecs::ecs::name::Name>(entity).is_some()
}

/// The Outliner row text of an entity without a `Name` (bug 12, ADR-8586): `<Kind> (unnamed)` from
/// the kind the row icon shows, else its derived role, else `Entity (unnamed)`. The row's tooltip
/// carries the id; agents keep reading [`display_name`].
#[must_use]
pub fn unnamed_label(icon: &crate::kind_icons::RowIcon) -> String {
    let kind = icon
        .kind
        .as_deref()
        .or_else(|| icon.role.map(|role| role.name()))
        .unwrap_or("entity");
    let words = kind.replace(['_', '-'], " ");
    let mut chars = words.chars();
    let title = match (kind, chars.next()) {
        ("ui", _) => "UI".to_owned(),
        (_, Some(first)) => first.to_uppercase().chain(chars).collect(),
        (_, None) => "Entity".to_owned(),
    };
    format!("{title} (unnamed)")
}

/// Builds the full hierarchy tree of `world` (roots = entities without `ChildOf`), children
/// sorted by label so the outliner is stable frame to frame, with every row's `type:`/`tag:`
/// search terms.
#[must_use]
pub fn hierarchy_tree(world: &World) -> Vec<HierarchyNode> {
    build_tree(world, true)
}

/// [`hierarchy_tree`], with the search terms (`components`, `tags`) only when `terms` is set: the
/// Outliner's cache builds without them and fills them on demand ([`fill_terms`]).
fn build_tree(world: &World, terms: bool) -> Vec<HierarchyNode> {
    // Feed-owned entities carry a text mark with their state (specs/60 § 6), never color alone.
    let label = |entity: Entity| match crate::dcc_feed::outliner_badge(world, entity) {
        Some(badge) => format!("{} [{badge}]", display_name(world, entity)),
        None => display_name(world, entity),
    };
    fn build(world: &World, label: &dyn Fn(Entity) -> String, terms: bool, entity: Entity) -> HierarchyNode {
        let children: Vec<Entity> = world
            .get::<Children>(entity)
            .map(|c| c.iter().collect::<Vec<_>>())
            .unwrap_or_default();
        let mut children: Vec<HierarchyNode> = children
            .into_iter()
            .map(|child| build(world, label, terms, child))
            .collect();
        children.sort_by(|a, b| {
            a.label
                .cmp(&b.label)
                .then_with(|| a.entity.to_bits().cmp(&b.entity.to_bits()))
        });
        let (components, tags) = if terms {
            crate::kind_icons::row_terms(world, entity)
        } else {
            (Vec::new(), Vec::new())
        };
        HierarchyNode {
            entity,
            label: label(entity),
            icon: crate::kind_icons::row_icon(world, entity),
            components,
            tags,
            children,
        }
    }

    let mut children_of: HashSet<Entity> = HashSet::new();
    let mut visible: Vec<Entity> = Vec::new();
    for entity_ref in world.iter_entities() {
        let entity = entity_ref.id();
        // Resources are entities too (bevy_ecs 0.20); they are never scene content (ADR-1034).
        if entity_ref.contains::<regia_ecs::ecs::resource::IsResource>() {
            continue;
        }
        // Every entity the user created (level content, editor spawns, agent scene.spawn) is
        // listed; only editor-internal entities — window markers, bookkeeping — stay hidden,
        // via the `EditorHidden` marker (`crate::visibility::mark_internal_entities`).
        if !crate::visibility::is_hidden(world, entity) {
            visible.push(entity);
        }
        // The (hidden) frame of a prefab opened in place: its children, the prefab's roots, are
        // listed as roots.
        if crate::prefab_edit::is_in_place_frame(world, entity) {
            continue;
        }
        if let Some(children) = world.get::<Children>(entity) {
            for child in children.iter() {
                children_of.insert(child);
            }
        }
    }
    let mut roots: Vec<HierarchyNode> = visible
        .into_iter()
        .filter(|entity| !children_of.contains(entity))
        .map(|entity| build(world, &label, terms, entity))
        .collect();
    roots.sort_by(|a, b| {
        a.label
            .cmp(&b.label)
            .then_with(|| a.entity.to_bits().cmp(&b.entity.to_bits()))
    });
    roots
}

/// Keeps the nodes the search `query` matches and the ancestors that lead to them; an empty (or
/// blank) query keeps everything, and so does a malformed one (the panel shows its error under the
/// search box). A kept node keeps only its kept children.
///
/// The search is the Outliner query language ([`crate::outliner_query`], ADR-7757): today's
/// tokens keep their meaning (bare words are one "name contains" phrase; `kind:<name>`,
/// `kind:none`, `kind:?`, `type:<Component>` and `tag:<name>` filter as before, spec 101 § 4.2,
/// spec 19 § 2), and `has:`, `under:`, `parent:`, property comparisons, `and`, `or`, `not` and
/// parentheses are new. Property terms never match here (no world); see [`filter_tree_query`].
#[must_use]
pub fn filter_tree(nodes: &[HierarchyNode], query: &str) -> Vec<HierarchyNode> {
    match crate::outliner_query::Query::parse(query) {
        Ok(parsed) => filter_tree_query(nodes, &parsed, &crate::outliner_query::NoProperties),
        Err(_) => nodes.to_vec(),
    }
}

/// [`filter_tree`] with a parsed query and a source for property terms and folders (the edited
/// `World`).
#[must_use]
pub fn filter_tree_query(
    nodes: &[HierarchyNode],
    query: &crate::outliner_query::Query,
    props: &dyn crate::outliner_query::PropertySource,
) -> Vec<HierarchyNode> {
    filter_tree_marked(nodes, query, props).0
}

/// [`filter_tree_query`], plus the entities the query matches itself (the kept ancestors that
/// only lead to a match are not in the set).
#[must_use]
pub fn filter_tree_marked(
    nodes: &[HierarchyNode],
    query: &crate::outliner_query::Query,
    props: &dyn crate::outliner_query::PropertySource,
) -> (Vec<HierarchyNode>, HashSet<Entity>) {
    struct Walk<'a> {
        query: &'a crate::outliner_query::Query,
        props: &'a dyn crate::outliner_query::PropertySource,
        ancestors: Vec<String>,
        matched: HashSet<Entity>,
    }
    fn keep(node: &HierarchyNode, walk: &mut Walk<'_>) -> Option<HierarchyNode> {
        let matches = {
            let chain: Vec<&str> = walk.ancestors.iter().map(String::as_str).collect();
            walk.query.matches(
                &crate::outliner_query::RowFacts {
                    entity: node.entity,
                    label: &node.label,
                    icon: &node.icon,
                    components: &node.components,
                    tags: &node.tags,
                    ancestors: &chain,
                },
                walk.props,
            )
        };
        if matches {
            walk.matched.insert(node.entity);
        }
        walk.ancestors.push(node.label.clone());
        let children: Vec<HierarchyNode> = node.children.iter().filter_map(|c| keep(c, walk)).collect();
        walk.ancestors.pop();
        (matches || !children.is_empty()).then(|| HierarchyNode {
            entity: node.entity,
            label: node.label.clone(),
            icon: node.icon.clone(),
            components: node.components.clone(),
            tags: node.tags.clone(),
            children,
        })
    }
    if query.is_empty() {
        return (nodes.to_vec(), HashSet::new());
    }
    let mut walk = Walk {
        query,
        props,
        ancestors: Vec::new(),
        matched: HashSet::new(),
    };
    let kept = nodes
        .iter()
        .filter_map(|node| {
            // A top-level entity's folders are its outermost ancestors for `under:`/`parent:`.
            walk.ancestors = props
                .folder(node.entity)
                .map(|path| path.split('/').map(str::to_string).collect())
                .unwrap_or_default();
            keep(node, &mut walk)
        })
        .collect();
    (kept, walk.matched)
}

/// The tree flattened into category groups for the "Group by category" view (spec 101 § 4.2): kind
/// categories in name order, then `Uncategorized`, then `No kind`. Rows keep their tree order within
/// a group and lose their children (every row is listed on its own).
#[must_use]
pub fn group_by_category(nodes: &[HierarchyNode]) -> Vec<(String, Vec<HierarchyNode>)> {
    fn flatten(node: &HierarchyNode, out: &mut Vec<HierarchyNode>) {
        out.push(HierarchyNode {
            entity: node.entity,
            label: node.label.clone(),
            icon: node.icon.clone(),
            components: node.components.clone(),
            tags: node.tags.clone(),
            children: Vec::new(),
        });
        for child in &node.children {
            flatten(child, out);
        }
    }
    let mut flat = Vec::new();
    for node in nodes {
        flatten(node, &mut flat);
    }
    let rank = |category: &str| -> u8 {
        match category {
            regia_scene::kind::UNCATEGORIZED => 1,
            crate::kind_icons::NO_KIND => 2,
            _ => 0,
        }
    };
    let mut groups: std::collections::BTreeMap<(u8, String), Vec<HierarchyNode>> =
        std::collections::BTreeMap::new();
    for node in flat {
        groups
            .entry((rank(&node.icon.category), node.icon.category.clone()))
            .or_default()
            .push(node);
    }
    groups.into_iter().map(|((_, name), rows)| (name, rows)).collect()
}

/// Whether parenting `child` under `parent` would create a cycle (`parent` inside `child`'s
/// ancestry, or the entities are identical).
#[must_use]
pub fn reparent_creates_cycle(world: &World, child: Entity, parent: Entity) -> bool {
    if child == parent {
        return true;
    }
    // Walk `parent`'s ancestry until the root or a revisit: a malformed `ChildOf` cycle above
    // `parent` that does not hold `child` would otherwise loop forever (bevy_ecs accepts such
    // inserts). A visited set, not a step limit: a limit answered "no cycle" for any legitimate
    // chain deeper than it, and the reparent then closed a real cycle.
    let mut seen = HashSet::new();
    let mut cursor = Some(parent);
    while let Some(current) = cursor {
        if current == child {
            return true;
        }
        if !seen.insert(current) {
            return false;
        }
        cursor = world.get::<ChildOf>(current).map(|c| c.parent());
    }
    false
}

/// The entities of `root`'s subtree (root included, cycle-safe).
#[must_use]
pub fn subtree_of(world: &World, root: Entity) -> Vec<Entity> {
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(current) = stack.pop() {
        if out.contains(&current) {
            continue;
        }
        out.push(current);
        stack.extend(
            world
                .get::<Children>(current)
                .map(|c| c.iter().collect::<Vec<_>>())
                .unwrap_or_default(),
        );
    }
    out
}

/// Creates a child entity under `parent` (name "New entity") as one undo entry. The reparent is
/// keyed by the new entity's guid, so redo after undo finds the re-spawned entity. Returns the
/// child.
pub(crate) fn create_child(world: &mut World, parent: Entity) -> Option<Entity> {
    let guid = Guid::new_random();
    let Ok(mut spawn) = SpawnEntities::new_entity(world, guid, "New entity", Transform::default()) else {
        return None;
    };
    spawn.apply(world);
    let child = hierarchy_ops::guid_entity(world, guid)?;
    let mut place = ReparentEntities {
        moves: vec![ReparentMove {
            entity: EntityKey::capture(world, child),
            old_parent: None,
            new_parent: Some(EntityKey::capture(world, parent)),
            old_local: None,
            new_local: None,
        }],
        ..ReparentEntities::default()
    };
    place.apply(world);
    world
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .push(Box::new(CommandBatch {
            label: "Create child".to_string(),
            commands: vec![Box::new(spawn), Box::new(place)],
        }));
    Some(child)
}

/// Where the Outliner drew its rows on its last draw, top to bottom (rows inside collapsed
/// parents are not drawn and not listed; rows scrolled out of view are listed, not drawn). Shift-click range selection walks this order, and
/// tests and tools aim synthetic pointer input with the rects.
#[derive(Resource, Debug, Clone, Default)]
pub struct OutlinerRows {
    /// The drawn rows in draw order: only the rows on screen (the panel lays out no others,
    /// ADR-7306).
    pub rows: Vec<OutlinerRow>,
    /// Every listed entity row in order, on screen or scrolled out of view (rows inside closed
    /// parents are not listed).
    pub listed: Vec<Entity>,
    /// The "move to root" drop zone under the tree.
    pub root_drop: Option<egui::Rect>,
    /// The search box.
    pub search: Option<egui::Rect>,
    /// The search text the rows were filtered by ("" = none).
    pub filter: String,
    /// The folder rows drawn this frame: (path, the label's screen rect) (ADR-7757).
    pub folders: Vec<(String, egui::Rect)>,
}

impl OutlinerRows {
    /// The drawn row of `entity`, if it was drawn.
    #[must_use]
    pub fn row(&self, entity: Entity) -> Option<&OutlinerRow> {
        self.rows.iter().find(|row| row.entity == entity)
    }

    /// The listed entities in order, including rows scrolled out of view (what Shift-click
    /// ranges and arrow keys walk).
    #[must_use]
    pub fn order(&self) -> Vec<Entity> {
        self.listed.clone()
    }
}

/// One drawn Outliner row.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutlinerRow {
    /// The row's entity.
    pub entity: Entity,
    /// The label's screen rect (the click, drag and drop target).
    pub rect: egui::Rect,
    /// The disclosure toggle's screen rect (disabled without children or while searching).
    pub toggle: egui::Rect,
    /// Nesting depth (0 = root).
    pub depth: usize,
}

/// The rows from `anchor` to `clicked` (inclusive, in draw order): the Shift-click range. Just
/// `clicked` when there is no anchor or it is not drawn.
#[must_use]
pub fn range_between(order: &[Entity], anchor: Option<Entity>, clicked: Entity) -> Vec<Entity> {
    let position = |entity: Entity| order.iter().position(|e| *e == entity);
    match (anchor.and_then(position), position(clicked)) {
        (Some(a), Some(b)) => order[a.min(b)..=a.max(b)].to_vec(),
        _ => vec![clicked],
    }
}

/// How a row click changes the selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClickMode {
    /// Plain click.
    Replace,
    /// Ctrl-click.
    Toggle,
    /// Shift-click: the range from the primary to the row replaces the selection.
    Range,
    /// Ctrl+Shift-click: the range is added.
    AddRange,
}

/// One queued outliner interaction, applied after drawing (single `&mut World` borrow at the end).
enum OutlinerAction {
    /// Select all: every listed entity, or only `only` (the rows the search matches).
    SelectAll {
        /// The matching rows while searching (`None` = every entity).
        only: Option<Vec<Entity>>,
    },
    /// The tree's rubber band: select these rows (added to the selection when `add`).
    Band {
        /// The rows inside the band that the search matches.
        entities: Vec<Entity>,
        /// Ctrl held: add instead of replace.
        add: bool,
    },
    /// Rename or move a folder.
    RenameFolder {
        /// The folder.
        path: String,
        /// Its new path.
        to: String,
    },
    /// Move top-level entities into a folder (`None` = out of folders).
    FolderMove {
        /// The entities.
        entities: Vec<Entity>,
        /// The folder.
        path: Option<String>,
    },
    /// Delete a folder, keeping its entities.
    FolderDelete {
        /// The folder.
        path: String,
    },
    /// New Folder (Ctrl+Shift+N): under `parent`, holding the top-level `entities`; its name is
    /// then edited in place.
    NewFolder {
        /// The top-level entities to move in.
        entities: Vec<Entity>,
        /// The parent folder.
        parent: Option<String>,
    },
    /// Paste the clipboard as children of `entity`.
    PasteAsChild {
        /// The new parent.
        entity: Entity,
    },
    /// A row click.
    Click {
        /// The clicked entity.
        entity: Entity,
        /// Modifier-dependent mode.
        mode: ClickMode,
    },
    /// Drag-and-drop: move `entities` under `parent` (root when `None`), keeping world poses
    /// (Alt held at the drop: keeping the local transforms).
    Reparent {
        /// The moved entities (the drag set).
        entities: Vec<Entity>,
        /// The new parent.
        parent: Option<Entity>,
        /// Keep the world pose (false while Alt is held).
        keep_world: bool,
    },
    /// Rename an entity.
    Rename {
        /// The entity.
        entity: Entity,
        /// The new name.
        name: String,
    },
    /// Delete entities (and their subtrees).
    Delete {
        /// The entities.
        entities: Vec<Entity>,
    },
    /// Ctrl+D style duplicate.
    Duplicate {
        /// The entities.
        entities: Vec<Entity>,
    },
    /// Ctrl+G: group under a new empty entity.
    Group {
        /// The entities.
        entities: Vec<Entity>,
    },
    /// Widen the selection to relatives.
    /// Row menu → Select similar (the targets seed it, ADR-7769).
    SelectSimilar {
        /// The seeds.
        entities: Vec<Entity>,
        /// What to compare.
        by: crate::selection::SimilarBy,
    },
    /// Row menu → Selection root on or off.
    SelectionRoot {
        /// The targets.
        entities: Vec<Entity>,
        /// Mark (true) or unmark.
        on: bool,
    },
    SelectScope {
        /// Whose relatives.
        entities: Vec<Entity>,
        /// Which relatives.
        scope: SelectScope,
    },
    /// Create an empty child.
    CreateChild {
        /// The parent.
        entity: Entity,
    },
    /// Ctrl+Z.
    Undo,
    /// Ctrl+Shift+Z / Ctrl+Y.
    Redo,
    /// The row's eye: show or hide in the viewport (one undo step, ADR-7204).
    SetVisible {
        /// The entity.
        entity: Entity,
        /// Shown or hidden.
        visible: bool,
    },
    /// The row's lock: a locked entity is not picked or box-selected (ADR-7204).
    SetLocked {
        /// The entity.
        entity: Entity,
        /// Locked or not.
        locked: bool,
    },
    /// Context menu Focus: frame the entities in the main viewport's camera (eased).
    Focus {
        /// The entities.
        entities: Vec<Entity>,
    },
    /// Row menu → Edit in grid: the entities as Bulk Edit rows (ADR-7787).
    EditInGrid {
        /// The entities.
        entities: Vec<Entity>,
    },
    /// The override dot: open the instance's Overrides window (ADR-7790).
    OpenOverrides {
        /// The instance root the row shows.
        entity: Entity,
    },
    /// An extension action of the row menu, run on the entities (ADR-7791).
    Extension {
        /// The action id.
        id: String,
        /// The entities.
        entities: Vec<Entity>,
    },
}

/// The selection, or an empty one.
fn selection_of(world: &World) -> Selection {
    world.get_resource::<Selection>().cloned().unwrap_or_default()
}

/// Logs a refused hierarchy edit (the panel has no error surface beyond the log).
fn report(result: Result<(), HierarchyError>) {
    if let Err(err) = result {
        tracing::warn!(target: "regia_editor", "outliner: {err} ({})", err.code());
    }
}

/// Applies one queued action. Returns a folder whose name should be edited in place next (New
/// Folder). `order` is the listed rows; `matched` the rows the search matches (`None` while not
/// searching): Shift ranges then select only matching rows.
fn perform(
    action: OutlinerAction,
    world: &mut World,
    order: &[Entity],
    matched: Option<&HashSet<Entity>>,
) -> Option<RenameTarget> {
    match action {
        OutlinerAction::SelectAll { only } => {
            let mut selection = selection_of(world);
            match only {
                Some(entities) => selection.select_only(&entities),
                None => selection.select_all(world),
            }
            world.insert_resource(selection);
        }
        OutlinerAction::Band { entities, add } => {
            let mut selection = selection_of(world);
            if !add {
                selection.clear();
            }
            for entity in entities {
                selection.select(entity, SelectMode::Add);
            }
            world.insert_resource(selection);
        }
        OutlinerAction::RenameFolder { path, to } => {
            if let Err(err) = crate::outliner_folders::rename_folder(world, &path, &to) {
                tracing::warn!(target: "regia_editor", "outliner: {err} ({})", err.code());
            }
        }
        OutlinerAction::FolderMove { entities, path } => {
            if let Err(err) = crate::outliner_folders::move_to_folder(world, &entities, path.as_deref()) {
                tracing::warn!(target: "regia_editor", "outliner: {err} ({})", err.code());
            }
        }
        OutlinerAction::FolderDelete { path } => {
            if let Err(err) = crate::outliner_folders::delete_folder(world, &path) {
                tracing::warn!(target: "regia_editor", "outliner: {err} ({})", err.code());
            }
        }
        OutlinerAction::NewFolder { entities, parent } => {
            return new_folder(world, &entities, parent.as_deref()).map(RenameTarget::Folder);
        }
        OutlinerAction::PasteAsChild { entity } => {
            let options = crate::viewport_ops::PasteOptions {
                into: Some(entity),
                at_original: false,
            };
            report(crate::viewport_ops::paste_with(world, options).map(drop));
        }
        OutlinerAction::Click { entity, mode } => {
            let mut selection = selection_of(world);
            match mode {
                ClickMode::Replace => selection.select(entity, SelectMode::Replace),
                ClickMode::Toggle => selection.select(entity, SelectMode::Toggle),
                ClickMode::Range | ClickMode::AddRange => {
                    let range = range_between(order, selection.primary(), entity);
                    if mode == ClickMode::Range {
                        selection.clear();
                    }
                    // While searching, a range holds only the rows the search matches.
                    for member in range
                        .into_iter()
                        .filter(|e| matched.is_none_or(|m| m.contains(e)))
                    {
                        selection.select(member, SelectMode::Add);
                    }
                    // The clicked row is the new primary (the next range's anchor).
                    selection.select(entity, SelectMode::Add);
                }
            }
            world.insert_resource(selection);
        }
        OutlinerAction::Reparent {
            entities,
            parent,
            keep_world,
        } => {
            report(hierarchy_ops::reparent_entities(world, &entities, parent, keep_world).map(drop));
        }
        OutlinerAction::Rename { entity, name } => {
            // A marked entity's name stays unique (ADR-7772).
            if let Some(code) = crate::outliner_columns::rename_refusal(world, entity, &name) {
                tracing::warn!(target: "regia_editor", "rename: {code}");
            } else if let Err((code, err)) = rename_entity(world, entity, &name) {
                tracing::warn!(target: "regia_editor", "rename: {err} ({code})");
            }
        }
        OutlinerAction::Delete { entities } => {
            report(hierarchy_ops::delete_entities(world, &entities).map(drop));
        }
        OutlinerAction::Extension { id, entities } => {
            let selection = crate::extension::actions::MenuSelection {
                entities,
                assets: Vec::new(),
            };
            match crate::extension::actions::run_action_on(world, &id, selection) {
                Ok((_, message)) => tracing::info!(target: "regia_editor", "{id}: {message}"),
                Err(error) => tracing::warn!(target: "regia_editor", "{id}: {}", error.message),
            }
        }
        OutlinerAction::SetVisible { entity, visible } => {
            crate::viewport_ops::set_visible(world, entity, visible);
        }
        OutlinerAction::SetLocked { entity, locked } => {
            crate::viewport_ops::set_locked(world, entity, locked);
        }
        OutlinerAction::OpenOverrides { entity } => {
            crate::prefab_overrides_panel::open_for(world, entity);
        }
        OutlinerAction::Focus { entities } => {
            let camera = world
                .get_resource_or_insert_with(crate::viewport::EditorCamera::default)
                .clone();
            let to = crate::viewport_ops::focus_pose(world, &camera, &entities);
            world
                .get_resource_or_insert_with(crate::viewport_ops::CameraEases::default)
                .start(crate::viewport_ops::MAIN_CAMERA, camera, to);
        }
        OutlinerAction::Duplicate { entities } => {
            report(hierarchy_ops::duplicate_entities(world, &entities, Vec3::new(1.0, 0.0, 0.0)).map(drop));
        }
        OutlinerAction::Group { entities } => {
            report(hierarchy_ops::group_entities(world, &entities, "Group").map(drop));
        }
        OutlinerAction::SelectSimilar { entities, by } => {
            let mut seed = Selection::default();
            seed.apply_region(&entities, crate::selection::RegionMode::Replace);
            world.insert_resource(seed);
            let _ = crate::selection::select_similar(world, by);
        }
        OutlinerAction::SelectionRoot { entities, on } => {
            for entity in entities {
                crate::pick_stack::set_selection_root(world, entity, on);
            }
        }
        OutlinerAction::SelectScope { entities, scope } => {
            let picked = hierarchy_ops::scope_entities(world, &entities, scope);
            if !picked.is_empty() {
                let mut selection = Selection::default();
                for entity in picked {
                    selection.select(entity, SelectMode::Add);
                }
                world.insert_resource(selection);
            }
        }
        OutlinerAction::CreateChild { entity } => {
            let _ = create_child(world, entity);
        }
        OutlinerAction::Undo => {
            crate::command::undo_world(world);
            let mut selection = selection_of(world);
            selection.retain_alive(world);
            world.insert_resource(selection);
        }
        OutlinerAction::Redo => {
            crate::command::redo_world(world);
            let mut selection = selection_of(world);
            selection.retain_alive(world);
            world.insert_resource(selection);
        }
        OutlinerAction::EditInGrid { entities } => {
            let status = crate::bulk_edit_panel::edit_entities_in_grid(world, &entities);
            // A refusal (no shared component, an asset grid with unsaved edits) is logged, not lost
            // while the panel is closed (review sprint-w12-rev-g4 #2).
            if !crate::bulk_edit_panel::grid_open(world) {
                tracing::warn!(target: "regia_editor", "outliner: Edit in grid: {status}");
            }
        }
    }
    None
}

/// New Folder: a free "New Folder" name under `parent`, holding the top-level `entities` (one undo
/// step when any moves). Returns the folder's path.
fn new_folder(world: &mut World, entities: &[Entity], parent: Option<&str>) -> Option<String> {
    let path = crate::outliner_folders::free_folder_path(world, parent, crate::outliner_folders::NEW_FOLDER);
    let path = match crate::outliner_folders::create_folder(world, &path) {
        Ok(path) => path,
        Err(err) => {
            tracing::warn!(target: "regia_editor", "outliner: {err} ({})", err.code());
            return None;
        }
    };
    let top: Vec<Entity> = entities
        .iter()
        .copied()
        .filter(|e| world.get::<ChildOf>(*e).is_none())
        .collect();
    if !top.is_empty()
        && let Err(err) = crate::outliner_folders::move_to_folder(world, &top, Some(&path))
    {
        tracing::warn!(target: "regia_editor", "outliner: {err} ({})", err.code());
    }
    Some(path)
}

/// Renames `entity` to `name` as one undo step (the agent's `editor.outliner_rename` and the
/// in-place field).
///
/// # Errors
/// `rename_empty` for a blank name, `rename_unnamed` when the entity has no level `Name`
/// (internal entities), `rename_failed` when the undo command cannot be built.
pub fn rename_entity(world: &mut World, entity: Entity, name: &str) -> Result<(), (&'static str, String)> {
    if name.trim().is_empty() {
        return Err(("rename_empty", "the name is blank".to_string()));
    }
    if world.get::<Name>(entity).is_none() {
        return Err((
            "rename_unnamed",
            format!("entity {} has no level name to edit", entity.to_bits()),
        ));
    }
    if world.get::<Name>(entity).is_some_and(|n| n.as_str() == name) {
        return Ok(());
    }
    match SetProperty::capture(world, entity, "Name", "", Box::new(Name::new(name.to_string()))) {
        Ok(mut command) => {
            command.apply(world);
            let mut stack = world.get_resource_or_insert_with::<UndoStack>(UndoStack::new);
            stack.push(Box::new(command));
            stack.seal();
            Ok(())
        }
        Err(err) => Err(("rename_failed", err.to_string())),
    }
}

/// Per-draw state shared by the rows.
struct DrawState {
    /// The selection at the start of the draw.
    selection: Selection,
    /// The entities the current drag carries (empty when nothing is dragged).
    dragged: Vec<Entity>,
    /// A search filter is active: every kept row is drawn open.
    filtering: bool,
    /// The selected entities without a parent (what a folder can hold).
    top_level: HashSet<Entity>,
}

/// One row of the flattened Outliner (ADR-7306): the tree after the search, the grouping and the
/// open/closed state are applied, in draw order. The panel draws only the rows on screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlatRow {
    /// A section heading of the "Group by category" view.
    Category {
        /// The category name.
        name: String,
        /// Entity rows in the section.
        count: usize,
        /// Whether the section is open.
        open: bool,
    },
    /// An entity row.
    Entity {
        /// The entity.
        entity: Entity,
        /// Display label.
        label: String,
        /// The row icon.
        icon: crate::kind_icons::RowIcon,
        /// Nesting depth (0 = root).
        depth: usize,
        /// Whether the entity has (kept) children.
        has_children: bool,
        /// Whether its children are listed below it.
        open: bool,
    },
    /// An open level's (or layer file's) heading while several documents are open (spec 37 § 12,
    /// P-097): its roots are listed under it.
    Level {
        /// The document id (`level_io::LevelDocument::id`).
        document: u128,
        /// The heading: the file name, ` *` when unsaved, `(active)` for the active level.
        label: String,
        /// Root rows in the group.
        count: usize,
        /// Whether the group is open.
        open: bool,
        /// It is the active level (new entities land in it).
        active: bool,
    },
    /// An Outliner folder (ADR-7757): a tree row that holds top-level entities without being an
    /// entity ([`crate::outliner_folders`]).
    Folder {
        /// The folder's full path ("Lights/Street").
        path: String,
        /// Its last segment, the row label ("Street").
        name: String,
        /// Nesting depth (0 = a top-level folder).
        depth: usize,
        /// Entities in it and its subfolders.
        count: usize,
        /// Whether its contents are listed below it.
        open: bool,
    },
}

impl FlatRow {
    /// The row's entity (`None` for a category heading or a folder).
    #[must_use]
    pub fn entity(&self) -> Option<Entity> {
        match self {
            Self::Entity { entity, .. } => Some(*entity),
            Self::Category { .. } | Self::Folder { .. } | Self::Level { .. } => None,
        }
    }
}

/// [`flatten_tree`] with Outliner folders (ADR-7757): top-level entities whose `folder_of` is a
/// path are listed inside folder rows; at each level the subfolders come first (by name), then
/// the entities. `extra` adds folders without members (shown only while not filtering).
/// `collapsed_folders` holds the closed folder paths; while `filtering` every row is open and a
/// folder without a kept entity is left out.
#[must_use]
pub fn flatten_with_folders(
    tree: &[HierarchyNode],
    folder_of: &dyn Fn(Entity) -> Option<String>,
    extra: &std::collections::BTreeSet<String>,
    collapsed: &HashSet<Entity>,
    collapsed_folders: &HashSet<String>,
    filtering: bool,
) -> Vec<FlatRow> {
    use std::collections::{BTreeMap, BTreeSet};
    let mut by_folder: BTreeMap<String, Vec<&HierarchyNode>> = BTreeMap::new();
    let mut loose: Vec<&HierarchyNode> = Vec::new();
    let mut folders: BTreeSet<String> = BTreeSet::new();
    let add_prefixes = |path: &str, folders: &mut BTreeSet<String>| {
        let mut prefix = String::new();
        for segment in path.split('/') {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(segment);
            folders.insert(prefix.clone());
        }
    };
    for node in tree {
        match folder_of(node.entity) {
            Some(path) => {
                add_prefixes(&path, &mut folders);
                by_folder.entry(path).or_default().push(node);
            }
            None => loose.push(node),
        }
    }
    if !filtering {
        for path in extra {
            add_prefixes(path, &mut folders);
        }
    }
    struct Emit<'a> {
        by_folder: &'a BTreeMap<String, Vec<&'a HierarchyNode>>,
        loose: &'a [&'a HierarchyNode],
        folders: &'a BTreeSet<String>,
        collapsed: &'a HashSet<Entity>,
        collapsed_folders: &'a HashSet<String>,
        filtering: bool,
    }
    fn emit(ctx: &Emit<'_>, parent: Option<&str>, depth: usize, out: &mut Vec<FlatRow>) {
        for path in ctx
            .folders
            .iter()
            .filter(|p| crate::outliner_folders::parent_path(p) == parent)
        {
            let count = ctx
                .by_folder
                .iter()
                .filter(|(member, _)| crate::outliner_folders::is_within(member, path))
                .map(|(_, nodes)| nodes.len())
                .sum();
            let open = ctx.filtering || !ctx.collapsed_folders.contains(path);
            out.push(FlatRow::Folder {
                path: path.clone(),
                name: crate::outliner_folders::folder_name(path).to_string(),
                depth,
                count,
                open,
            });
            if open {
                emit(ctx, Some(path.as_str()), depth + 1, out);
            }
        }
        let nodes: &[&HierarchyNode] = match parent {
            None => ctx.loose,
            Some(path) => ctx.by_folder.get(path).map(Vec::as_slice).unwrap_or_default(),
        };
        for node in nodes {
            flatten_node(node, depth, ctx.collapsed, ctx.filtering, out);
        }
    }
    let ctx = Emit {
        by_folder: &by_folder,
        loose: &loose,
        folders: &folders,
        collapsed,
        collapsed_folders,
        filtering,
    };
    let mut out = Vec::new();
    emit(&ctx, None, 0, &mut out);
    out
}

/// Flattens `tree` into its listed rows, pre-order: a node's children follow it when it is open.
/// A node is open unless it is in `collapsed`; while `filtering`, every kept node is open (the
/// stored state is left alone for when the search clears).
#[must_use]
pub fn flatten_tree(tree: &[HierarchyNode], collapsed: &HashSet<Entity>, filtering: bool) -> Vec<FlatRow> {
    let mut out = Vec::new();
    for node in tree {
        flatten_node(node, 0, collapsed, filtering, &mut out);
    }
    out
}

fn flatten_node(
    node: &HierarchyNode,
    depth: usize,
    collapsed: &HashSet<Entity>,
    filtering: bool,
    out: &mut Vec<FlatRow>,
) {
    let has_children = !node.children.is_empty();
    let open = filtering || !collapsed.contains(&node.entity);
    out.push(FlatRow::Entity {
        entity: node.entity,
        label: node.label.clone(),
        icon: node.icon.clone(),
        depth,
        has_children,
        open,
    });
    if open && has_children {
        for child in &node.children {
            flatten_node(child, depth + 1, collapsed, filtering, out);
        }
    }
}

/// Flattens the "Group by category" sections: one heading per category, then its rows when the
/// category is not in `collapsed_categories`.
#[must_use]
pub fn flatten_groups(
    groups: &[(String, Vec<HierarchyNode>)],
    collapsed_categories: &HashSet<String>,
    collapsed: &HashSet<Entity>,
    filtering: bool,
) -> Vec<FlatRow> {
    let mut out = Vec::new();
    for (name, nodes) in groups {
        let open = !collapsed_categories.contains(name);
        out.push(FlatRow::Category {
            name: name.clone(),
            count: nodes.len(),
            open,
        });
        if open {
            for node in nodes {
                flatten_node(node, 0, collapsed, filtering, &mut out);
            }
        }
    }
    out
}

/// Flattens the hierarchy view while several documents are open (spec 37 § 12, P-097): one
/// [`FlatRow::Level`] heading per document (host first), then, when the group is open, its roots
/// as `flatten` lays them out (folders included); roots in no open document come last under a
/// [`crate::layers_panel::NO_LEVEL_GROUP`] heading.
#[must_use]
pub fn flatten_level_groups(
    tree: &[HierarchyNode],
    levels: &crate::layers_panel::LevelGrouping,
    collapsed_levels: &HashSet<u128>,
    flatten: &dyn Fn(&[HierarchyNode]) -> Vec<FlatRow>,
) -> Vec<FlatRow> {
    let mut out = Vec::new();
    for row in &levels.rows {
        let nodes: Vec<HierarchyNode> = tree
            .iter()
            .filter(|node| levels.roots.get(&node.entity) == Some(&row.document.id))
            .cloned()
            .collect();
        let open = !collapsed_levels.contains(&row.document.id);
        out.push(FlatRow::Level {
            document: row.document.id,
            label: crate::layers_panel::level_group_heading(row),
            count: nodes.len(),
            open,
            active: row.active,
        });
        if open {
            out.extend(flatten(&nodes));
        }
    }
    let loose: Vec<HierarchyNode> = tree
        .iter()
        .filter(|node| !levels.roots.contains_key(&node.entity))
        .cloned()
        .collect();
    if !loose.is_empty() {
        let open = !collapsed_levels.contains(&0);
        out.push(FlatRow::Level {
            document: 0,
            label: crate::layers_panel::NO_LEVEL_GROUP.to_owned(),
            count: loose.len(),
            open,
            active: false,
        });
        if open {
            out.extend(flatten(&loose));
        }
    }
    out
}

/// A cheap signature of the world's structure: every archetype's id and entity count. A spawn,
/// a despawn or an inserted/removed component changes it. Resource archetypes are left out
/// (resources are entities in bevy_ecs 0.20 and never rows; a panel inserting its first
/// resource must not rebuild the tree).
fn structure_signature(world: &World) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let resource = world.component_id::<regia_ecs::ecs::resource::IsResource>();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for archetype in world.archetypes().iter() {
        if resource.is_some_and(|id| archetype.contains(id)) {
            continue;
        }
        archetype.id().hash(&mut hasher);
        archetype.len().hash(&mut hasher);
    }
    hasher.finish()
}

/// Whether any component the rows show (names, parents, kinds, tags) changed after `since`.
fn row_values_changed(world: &mut World, since: regia_ecs::ecs::change_detection::Tick) -> bool {
    use regia_ecs::ecs::change_detection::{DetectChanges as _, Ref};
    fn any<T: Component>(world: &mut World, since: regia_ecs::ecs::change_detection::Tick) -> bool {
        let now = world.change_tick();
        let mut query = world.query::<Ref<T>>();
        query
            .iter(world)
            .any(|value| value.last_changed().is_newer_than(since, now))
    }
    any::<Name>(world, since)
        || any::<regia_ecs::ecs::name::Name>(world, since)
        || any::<ChildOf>(world, since)
        || any::<regia_scene::kind::KindTag>(world, since)
        // The rows bake each entity's tags for `tag:` (an in-place edit keeps the archetype).
        || any::<regia_scene::components::Tags>(world, since)
        // A folder rename edits `EditorFolder` in place; New Folder edits the session's set.
        || any::<regia_scene::components::EditorFolder>(world, since)
        || {
            let now = world.change_tick();
            world
                .get_resource_ref::<crate::outliner_folders::OutlinerFolders>()
                .is_some_and(|folders| folders.last_changed().is_newer_than(since, now))
        }
}

/// Computes every node's `type:`/`tag:` search terms (`kind_icons::row_terms`); returns how many
/// nodes it computed.
fn fill_terms(world: &World, nodes: &mut [HierarchyNode]) -> u64 {
    let mut count = 0;
    for node in nodes {
        (node.components, node.tags) = crate::kind_icons::row_terms(world, node.entity);
        count += 1 + fill_terms(world, &mut node.children);
    }
    count
}

/// Whether `query` reads the rows' `type:`/`has:`/`tag:` terms (the only terms [`fill_terms`]
/// computes); integ-v06s-editor: w11-seq-preview-fixes' lazy terms over the ADR-7757 query.
fn reads_row_terms(query: &crate::outliner_query::Query) -> bool {
    use crate::outliner_query::{Expr, Term};
    fn walk(expr: &Expr) -> bool {
        match expr {
            Expr::And(items) | Expr::Or(items) => items.iter().any(walk),
            Expr::Not(inner) => walk(inner),
            Expr::Term(term) => matches!(term, Term::Has(_) | Term::Tag(_)),
        }
    }
    query.expr().is_some_and(walk)
}

/// The Outliner's row cache (ADR-7306): the hierarchy tree is rebuilt only when the world's
/// structure or a shown value (name, parent, kind) changed, and the flat rows only when the tree,
/// the search, the grouping or an open/closed state changed. The panel then lays out only the
/// rows on screen (`egui::ScrollArea::show_rows`), so a 100 000-entity level costs a screenful of
/// rows per frame.
#[derive(Resource, Debug, Default)]
pub struct OutlinerView {
    /// The full tree (search not applied).
    tree: Vec<HierarchyNode>,
    /// The flat rows the panel lists.
    rows: Vec<FlatRow>,
    /// Structure signature the tree was built at.
    signature: Option<u64>,
    /// The change tick the tree was checked at.
    since: Option<regia_ecs::ecs::change_detection::Tick>,
    /// (search, grouped, open-state revision, values revision) the rows were flattened at.
    view_key: Option<(String, bool, u64, u64)>,
    /// Entities shown closed.
    collapsed: HashSet<Entity>,
    /// Folder paths shown closed (ADR-7757).
    collapsed_folders: HashSet<String>,
    /// The folder paths of the last flattening.
    folders: std::collections::BTreeSet<String>,
    /// The entities the search matches itself (`None` while not searching): what Select all,
    /// Shift ranges and the rubber band act on (ADR-7757).
    matched: Option<HashSet<Entity>>,
    /// Why the search does not parse (the panel shows it under the search box; rows are then
    /// not filtered).
    query_error: Option<crate::outliner_query::QueryError>,
    /// Category sections shown closed.
    collapsed_categories: HashSet<String>,
    /// Bumped by every open/close.
    open_revision: u64,
    /// Tree rebuilds so far.
    tree_builds: u64,
    /// Row flattenings so far.
    flattens: u64,
    /// `type:`/`tag:` term evaluations so far (one per entity whose terms were computed).
    term_evaluations: u64,
    /// The tree carries its search terms (filled for the first `type:`/`tag:` search of this tree
    /// generation; a rebuild clears it).
    terms_filled: bool,
    /// The selection's primary at the end of the last draw (a change made elsewhere, such as a
    /// viewport pick, scrolls to the new primary).
    last_primary: Option<Entity>,
    /// Row to bring into view on the next draw, and whether to center it.
    scroll_to: Option<(usize, bool)>,
    /// The scroll offset and the visible height of the rows at the last draw.
    viewport: (f32, f32),
    /// The tree was sorted by creation order (ADR-7772).
    creation_order: bool,
    /// The per-level grouping while several documents are open (P-097; `None` with one).
    levels: Option<crate::layers_panel::LevelGrouping>,
    /// Level groups shown closed, by document id.
    collapsed_levels: HashSet<u128>,
}

impl OutlinerView {
    /// The flat rows of the last draw.
    #[must_use]
    pub fn rows(&self) -> &[FlatRow] {
        &self.rows
    }

    /// How many times the hierarchy tree was rebuilt (a revision counter for tests and soaks).
    #[must_use]
    pub fn tree_builds(&self) -> u64 {
        self.tree_builds
    }

    /// How many times the flat rows were rebuilt.
    #[must_use]
    pub fn flattens(&self) -> u64 {
        self.flattens
    }

    /// How many entities' `type:`/`tag:` search terms were computed so far (a counter for tests
    /// and soaks).
    #[must_use]
    pub fn term_evaluations(&self) -> u64 {
        self.term_evaluations
    }

    /// Whether `entity`'s children are listed (entities are open by default).
    #[must_use]
    pub fn is_open(&self, entity: Entity) -> bool {
        !self.collapsed.contains(&entity)
    }

    /// Opens or closes `entity`'s row; the rows are re-flattened on the next draw.
    pub fn set_open(&mut self, entity: Entity, open: bool) {
        let changed = if open {
            self.collapsed.remove(&entity)
        } else {
            self.collapsed.insert(entity)
        };
        if changed {
            self.open_revision += 1;
        }
    }

    /// Opens or closes a folder row.
    pub fn set_folder_open(&mut self, path: &str, open: bool) {
        let changed = if open {
            self.collapsed_folders.remove(path)
        } else {
            self.collapsed_folders.insert(path.to_string())
        };
        if changed {
            self.open_revision += 1;
        }
    }

    /// Whether a folder row is open (folders are open by default).
    #[must_use]
    pub fn is_folder_open(&self, path: &str) -> bool {
        !self.collapsed_folders.contains(path)
    }

    /// Expand all (`open`) or Collapse all: every entity row, folder and category section.
    pub fn set_all_open(&mut self, open: bool) {
        if open {
            self.collapsed.clear();
            self.collapsed_folders.clear();
            self.collapsed_categories.clear();
            self.collapsed_levels.clear();
        } else {
            fn walk(nodes: &[HierarchyNode], out: &mut HashSet<Entity>) {
                for node in nodes {
                    if !node.children.is_empty() {
                        out.insert(node.entity);
                        walk(&node.children, out);
                    }
                }
            }
            walk(&self.tree, &mut self.collapsed);
            self.collapsed_folders.extend(self.folders.iter().cloned());
        }
        self.open_revision += 1;
    }

    /// Opens or closes `entity`'s row and every row below it (Shift+click on the arrow).
    /// Returns whether the entity is in the tree.
    pub fn set_open_recursive(&mut self, entity: Entity, open: bool) -> bool {
        fn find(nodes: &[HierarchyNode], entity: Entity) -> Option<&HierarchyNode> {
            nodes.iter().find_map(|node| {
                if node.entity == entity {
                    Some(node)
                } else {
                    find(&node.children, entity)
                }
            })
        }
        fn walk(node: &HierarchyNode, out: &mut Vec<Entity>) {
            if !node.children.is_empty() {
                out.push(node.entity);
            }
            for child in &node.children {
                walk(child, out);
            }
        }
        let mut branch = Vec::new();
        match find(&self.tree, entity) {
            Some(node) => walk(node, &mut branch),
            None => return false,
        }
        for member in branch {
            self.set_open(member, open);
        }
        true
    }

    /// The entities the search matches itself (`None` while not searching).
    #[must_use]
    pub fn matched(&self) -> Option<&HashSet<Entity>> {
        self.matched.as_ref()
    }

    /// Why the current search does not parse.
    #[must_use]
    pub fn query_error(&self) -> Option<&crate::outliner_query::QueryError> {
        self.query_error.as_ref()
    }

    /// Opens or closes a category section of the grouped view.
    pub fn set_category_open(&mut self, category: &str, open: bool) {
        let changed = if open {
            self.collapsed_categories.remove(category)
        } else {
            self.collapsed_categories.insert(category.to_string())
        };
        if changed {
            self.open_revision += 1;
        }
    }

    /// Opens or closes an open level's group (by document id) in the hierarchy view.
    pub fn set_level_open(&mut self, document: u128, open: bool) {
        let changed = if open {
            self.collapsed_levels.remove(&document)
        } else {
            self.collapsed_levels.insert(document)
        };
        if changed {
            self.open_revision += 1;
        }
    }

    /// Whether an open level's group is open (groups are open by default).
    #[must_use]
    pub fn is_level_open(&self, document: u128) -> bool {
        !self.collapsed_levels.contains(&document)
    }

    /// The document `entity` is listed under while several documents are open: its root's.
    fn level_of(&self, world: &World, entity: Entity) -> Option<u128> {
        let levels = self.levels.as_ref()?;
        let mut root = entity;
        let mut seen = HashSet::new();
        while let Some(parent) = world.get::<ChildOf>(root).map(ChildOf::parent) {
            if !seen.insert(parent) {
                break;
            }
            root = parent;
        }
        levels.roots.get(&root).copied()
    }

    /// Opens every ancestor of `entity`, and scrolls its row into view (centred) on the next
    /// draw. Returns whether the entity is listed.
    pub fn reveal(&mut self, world: &World, entity: Entity) -> bool {
        let mut seen = HashSet::new();
        let mut cursor = world.get::<ChildOf>(entity).map(ChildOf::parent);
        while let Some(parent) = cursor {
            if !seen.insert(parent) {
                break;
            }
            self.set_open(parent, true);
            cursor = world.get::<ChildOf>(parent).map(ChildOf::parent);
        }
        if let Some(document) = self.level_of(world, entity) {
            self.set_level_open(document, true);
        }
        if let Some(key) = &self.view_key {
            let (filter, grouped) = (key.0.clone(), key.1);
            self.flatten(world, &filter, grouped);
        }
        match self.index_of(entity) {
            Some(index) => {
                self.scroll_to = Some((index, true));
                true
            }
            None => false,
        }
    }

    /// The row index of `entity`.
    fn index_of(&self, entity: Entity) -> Option<usize> {
        self.rows.iter().position(|row| row.entity() == Some(entity))
    }

    /// Brings the tree and the rows up to date with `world`, the search and the grouping.
    fn update(&mut self, world: &mut World, filter: &str, grouped: bool) {
        let signature = structure_signature(world);
        let creation_order = crate::outliner_columns::sort_by_creation(world);
        let stale = self.signature != Some(signature)
            || self.creation_order != creation_order
            || self.since.is_none_or(|since| row_values_changed(world, since));
        // Changes made after this point carry a newer tick than `since`.
        self.since = Some(world.increment_change_tick());
        if stale {
            let _span = tracing::trace_span!(target: "regia_editor", "outliner_tree_build").entered();
            // The search terms are computed on demand, once per tree generation (`flatten`).
            self.tree = build_tree(world, false);
            self.terms_filled = false;
            if creation_order {
                crate::outliner_columns::sort_tree_by_creation(&mut self.tree);
            }
            self.creation_order = creation_order;
            self.signature = Some(signature);
            self.tree_builds += 1;
            self.collapsed.retain(|entity| world.get_entity(*entity).is_ok());
            self.view_key = None;
        }
        // Several open levels: the roots are listed under one row per document (P-097). The
        // badges and the membership may change without a structure change, so it is read every
        // update and the rows re-flattened when it differs.
        let levels = crate::layers_panel::outliner_level_grouping(world, &self.tree);
        if levels != self.levels {
            self.levels = levels;
            self.collapsed_levels.retain(|document| {
                self.levels
                    .as_ref()
                    .is_some_and(|levels| levels.rows.iter().any(|row| row.document.id == *document))
            });
            self.view_key = None;
        }
        let values = Self::values_revision(world, filter);
        if self.view_key.as_ref().is_none_or(|(f, g, r, v)| {
            f != filter || *g != grouped || *r != self.open_revision || *v != values
        }) {
            self.flatten(world, filter, grouped);
        }
    }

    /// What property terms depend on: the undo stack's state id while the search reads
    /// properties (an inspector edit re-runs the search), else 0.
    fn values_revision(world: &World, filter: &str) -> u64 {
        let reads = crate::outliner_query::Query::parse(filter).is_ok_and(|q| q.reads_properties());
        if reads {
            world.get_resource::<UndoStack>().map_or(0, UndoStack::state_id)
        } else {
            0
        }
    }

    fn flatten(&mut self, world: &World, filter: &str, grouped: bool) {
        let parsed = crate::outliner_query::Query::parse(filter);
        self.query_error = parsed.as_ref().err().cloned();
        let query = parsed.unwrap_or_default();
        let filtering = !query.is_empty();
        if filtering && !self.terms_filled && reads_row_terms(&query) {
            let _span = tracing::trace_span!(target: "regia_editor", "outliner_row_terms").entered();
            self.term_evaluations += fill_terms(world, &mut self.tree);
            self.terms_filled = true;
        }
        let filtered;
        let tree = if filtering {
            let (kept, matched) = filter_tree_marked(&self.tree, &query, world);
            filtered = kept;
            self.matched = Some(matched);
            &filtered
        } else {
            self.matched = None;
            &self.tree
        };
        let empty = world
            .get_resource::<crate::outliner_folders::OutlinerFolders>()
            .map(|f| f.empty.clone())
            .unwrap_or_default();
        self.rows = if grouped {
            flatten_groups(
                &group_by_category(tree),
                &self.collapsed_categories,
                &self.collapsed,
                filtering,
            )
        } else if let Some(levels) = &self.levels {
            flatten_level_groups(tree, levels, &self.collapsed_levels, &|nodes| {
                flatten_with_folders(
                    nodes,
                    &|entity| crate::outliner_folders::folder_of(world, entity),
                    &empty,
                    &self.collapsed,
                    &self.collapsed_folders,
                    filtering,
                )
            })
        } else {
            flatten_with_folders(
                tree,
                &|entity| crate::outliner_folders::folder_of(world, entity),
                &empty,
                &self.collapsed,
                &self.collapsed_folders,
                filtering,
            )
        };
        self.folders = self
            .rows
            .iter()
            .filter_map(|row| match row {
                FlatRow::Folder { path, .. } => Some(path.clone()),
                _ => None,
            })
            .collect();
        let values = Self::values_revision(world, filter);
        self.view_key = Some((filter.to_string(), grouped, self.open_revision, values));
        self.flattens += 1;
    }

    /// The scroll offset that brings row `index` into view (`None` when it already is).
    fn offset_for(&self, index: usize, center: bool, row_step: f32) -> Option<f32> {
        let (offset, height) = self.viewport;
        let top = index as f32 * row_step;
        let bottom = top + row_step;
        if top >= offset && bottom <= offset + height {
            return None;
        }
        Some(if center {
            (top - (height - row_step) * 0.5).max(0.0)
        } else if top < offset {
            top
        } else {
            (bottom - height).max(0.0)
        })
    }
}

/// An open/close click on a row's disclosure toggle, and the other view-only requests of a draw
/// (open state, the in-place rename) applied after it.
enum RowToggle {
    /// An entity row.
    Entity(Entity, bool),
    /// A category heading.
    Category(String, bool),
    /// An open level's heading (document id).
    Level(u128, bool),
    /// A folder row.
    Folder(String, bool),
    /// An entity row and every row below it (Shift+click on the arrow, Shift+Left/Right).
    Branch(Entity, bool),
    /// Expand all (`true`) or Collapse all.
    All(bool),
    /// Start editing a row's name in place.
    BeginRename(RenameTarget),
    /// Stop editing (Enter, Esc, a click elsewhere).
    EndRename,
    /// Tab in the field: edit the next row's name.
    RenameNext(Entity),
}

/// What an in-place rename edits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RenameTarget {
    /// An entity's name.
    Entity(Entity),
    /// A folder's last path segment.
    Folder(String),
}

/// The in-place rename (egui memory, [`INLINE_RENAME`]).
#[derive(Clone, Debug)]
struct InlineRename {
    /// What is renamed.
    target: RenameTarget,
    /// The text being typed.
    draft: String,
    /// Focus the field on its next draw.
    focus: bool,
}

/// egui memory key of the in-place rename.
const INLINE_RENAME: &str = "outliner-inline-rename";

/// egui id of the search box (Ctrl+F focuses it).
pub const SEARCH_BOX: &str = "outliner-search-box";

/// The open in-place rename.
fn inline_rename(ctx: &egui::Context) -> Option<InlineRename> {
    ctx.data(|d| d.get_temp::<InlineRename>(egui::Id::new(INLINE_RENAME)))
}

/// Starts editing `target`'s name in place, with its current name as the draft.
fn begin_rename(ctx: &egui::Context, world: &World, target: RenameTarget) {
    let draft = match &target {
        RenameTarget::Entity(entity) => match world.get::<Name>(*entity) {
            Some(name) => name.as_str().to_string(),
            None => return,
        },
        RenameTarget::Folder(path) => crate::outliner_folders::folder_name(path).to_string(),
    };
    ctx.data_mut(|d| {
        d.insert_temp(
            egui::Id::new(INLINE_RENAME),
            InlineRename {
                target,
                draft,
                focus: true,
            },
        );
    });
}

/// Draws the in-place name field when `target` is being renamed: Enter (or a click elsewhere)
/// commits as one undo step, Esc cancels, Tab commits and moves to the next row. Returns the
/// field's response, `None` when `target` is not being renamed.
fn inline_rename_field(
    ui: &mut egui::Ui,
    target: &RenameTarget,
    actions: &mut Vec<OutlinerAction>,
    toggles: &mut Vec<RowToggle>,
) -> Option<egui::Response> {
    let mut state = inline_rename(ui.ctx()).filter(|state| state.target == *target)?;
    let field = ui.add(
        egui::TextEdit::singleline(&mut state.draft)
            .desired_width(ui.available_width().max(ui.spacing().interact_size.x * 2.0)),
    );
    let field = crate::a11y_audit::name(field, egui::WidgetType::TextEdit, "Rename");
    if state.focus {
        field.request_focus();
        state.focus = false;
    }
    // Tab is seen while the field still has focus (egui moves the focus at the end of the frame).
    let tabbed = field.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Tab));
    if tabbed || field.lost_focus() {
        let (escape, tab) = ui.input_mut(|i| {
            // The Enter that commits must not also frame the selection (the panel's Enter key).
            i.consume_key(egui::Modifiers::NONE, egui::Key::Enter);
            let escape = i.key_pressed(egui::Key::Escape);
            i.consume_key(egui::Modifiers::NONE, egui::Key::Escape);
            (escape, tabbed || i.key_pressed(egui::Key::Tab))
        });
        if !escape && !state.draft.trim().is_empty() {
            match target {
                RenameTarget::Entity(entity) => actions.push(OutlinerAction::Rename {
                    entity: *entity,
                    name: state.draft.trim().to_string(),
                }),
                RenameTarget::Folder(path) => {
                    let name = state.draft.trim().replace('/', "-");
                    let to = match crate::outliner_folders::parent_path(path) {
                        Some(parent) => format!("{parent}/{name}"),
                        None => name,
                    };
                    if to != *path {
                        actions.push(OutlinerAction::RenameFolder {
                            path: path.clone(),
                            to,
                        });
                    }
                }
            }
        }
        match target {
            RenameTarget::Entity(entity) if tab && !escape => toggles.push(RowToggle::RenameNext(*entity)),
            _ => toggles.push(RowToggle::EndRename),
        }
    } else {
        ui.ctx()
            .data_mut(|d| d.insert_temp(egui::Id::new(INLINE_RENAME), state));
    }
    Some(field)
}

/// A keyboard move through the listed rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Nav {
    /// Up arrow.
    Up,
    /// Down arrow.
    Down,
    /// Home: the first row.
    Home,
    /// End: the last row.
    End,
    /// Page Up: a screenful up.
    PageUp,
    /// Page Down: a screenful down.
    PageDown,
}

/// The entity row a navigation key moves the selection to (`page` = rows per screen).
fn nav_target(rows: &[FlatRow], primary: Option<Entity>, nav: Nav, page: usize) -> Option<(usize, Entity)> {
    let entity_rows: Vec<(usize, Entity)> = rows
        .iter()
        .enumerate()
        .filter_map(|(i, row)| row.entity().map(|e| (i, e)))
        .collect();
    let current = primary.and_then(|p| rows.iter().position(|row| row.entity() == Some(p)));
    match nav {
        Nav::Up => arrow_target(rows, primary, false),
        Nav::Down => arrow_target(rows, primary, true),
        Nav::Home => entity_rows.first().copied(),
        Nav::End => entity_rows.last().copied(),
        Nav::PageDown => {
            let goal = current.map_or(0, |at| at + page.max(1));
            entity_rows
                .iter()
                .find(|(i, _)| *i >= goal)
                .or(entity_rows.last())
                .copied()
        }
        Nav::PageUp => {
            let goal = current.map_or(0, |at| at.saturating_sub(page.max(1)));
            entity_rows
                .iter()
                .rev()
                .find(|(i, _)| *i <= goal)
                .or(entity_rows.first())
                .copied()
        }
    }
}

/// What Left / Right do on the primary's row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SideStep {
    /// Open (`true`) or close the row.
    Open(Entity, bool),
    /// Move the selection to this row (Right on an open row: its first child; Left on a closed
    /// or childless row: its parent).
    Select(usize, Entity),
}

/// Right on an open row selects its first child, on a closed one opens it; Left on an open row
/// closes it, else selects the row it hangs under.
fn side_step(rows: &[FlatRow], primary: Option<Entity>, right: bool) -> Option<SideStep> {
    let at = primary.and_then(|p| rows.iter().position(|row| row.entity() == Some(p)))?;
    let FlatRow::Entity {
        entity,
        depth,
        has_children,
        open,
        ..
    } = &rows[at]
    else {
        return None;
    };
    match (right, *has_children, *open) {
        (true, true, true) => match rows.get(at + 1) {
            Some(FlatRow::Entity {
                entity: child,
                depth: child_depth,
                ..
            }) if *child_depth == depth + 1 => Some(SideStep::Select(at + 1, *child)),
            _ => None,
        },
        (true, true, false) => Some(SideStep::Open(*entity, true)),
        (true, false, _) => None,
        (false, true, true) => Some(SideStep::Open(*entity, false)),
        (false, _, _) => rows[..at]
            .iter()
            .enumerate()
            .rev()
            .find_map(|(i, row)| match row {
                FlatRow::Entity {
                    entity: parent,
                    depth: d,
                    ..
                } if d < depth => Some(SideStep::Select(i, *parent)),
                _ => None,
            }),
    }
}

/// Draws the Outliner panel for `world`.
///
/// Rows: click selects (Ctrl toggles, Shift selects the range from the primary, Ctrl+Shift adds
/// it); a click on the only selected row, or F2, edits its name in place (Enter commits as one
/// undo step, Esc cancels, Tab moves to the next row); dragging a row carries the whole selection
/// when the row is selected, else the row alone; dropping on a row reparents under it, on a
/// folder row moves top-level entities into the folder, and on the space under the tree moves to
/// the root and out of folders, keeping world poses (Alt held at the drop: the local transforms),
/// as one undo entry. A drag that starts on empty space draws a rubber band that selects the rows
/// it touches (Ctrl adds). While searching, Select all, Shift ranges and the band select only the
/// rows the search matches (ADR-7757).
///
/// With the pointer over the panel: Delete, Ctrl+D (duplicate), Ctrl+G (group), Ctrl+Z /
/// Ctrl+Shift+Z / Ctrl+Y (undo / redo), Up / Down / Home / End / Page Up / Page Down (move the
/// selection; Shift extends it), Right (open the row, then go to its first child), Left (close
/// the row, then go to its parent), Shift+Right / Shift+Left (open / close the whole branch),
/// Ctrl+Shift+Plus / Ctrl+Shift+Minus (expand all / collapse all), F2 (rename), Enter (frame the
/// selection), Ctrl+A (select all matching rows), Ctrl+F (focus the search box), Ctrl+Shift+N (a
/// new folder holding the selected top-level entities).
///
/// Only the rows on screen are laid out ([`OutlinerView`], ADR-7306). A selection made elsewhere
/// (a viewport pick, an agent call) opens the new primary's ancestors and scrolls its row into view.
#[tracing::instrument(target = "regia_editor", skip_all, name = "outliner_panel")]
pub fn outliner_panel(ui: &mut egui::Ui, world: &mut World) {
    // The open level variant's marks (ADR-7823): refreshed after edits, read by each row.
    crate::level_variant::refresh_marks(world, ui.ctx().cumulative_frame_nr());
    let mut actions: Vec<OutlinerAction> = Vec::new();
    let mut toggles: Vec<RowToggle> = Vec::new();
    let mut rows = OutlinerRows::default();
    let selection = selection_of(world);
    let dragged = egui::DragAndDrop::payload::<crate::dnd::EntityDrag>(ui.ctx())
        .and_then(|payload| payload.entity())
        .filter(|entity| world.get_entity(*entity).is_ok())
        .map(|entity| hierarchy_ops::drag_set(&selection, entity))
        .unwrap_or_default();
    // Each Outliner instance has its own search, grouping, search box and band (P-003): the ids
    // are the plain ones for instance 0 (`crate::panel_instances::scoped_id`).
    let filter_id = crate::panel_instances::scoped_id(world, crate::panel_instances::OUTLINER_FILTER_KEY);
    let search_box = crate::panel_instances::scoped_id(world, SEARCH_BOX);
    let band_id = crate::panel_instances::scoped_id(world, "outliner-rubber-band");
    let mut filter = ui
        .ctx()
        .data(|d| d.get_temp::<String>(filter_id))
        .unwrap_or_default();
    let top_level = selection
        .entities()
        .iter()
        .chain(&dragged)
        .copied()
        .filter(|e| world.get::<ChildOf>(*e).is_none())
        .collect();
    let state = DrawState {
        selection,
        dragged,
        filtering: crate::outliner_query::Query::parse(&filter).is_ok_and(|q| !q.is_empty()),
        top_level,
    };

    let group_id = crate::panel_instances::scoped_id(world, "outliner-group-by-category");
    let mut grouped = ui.ctx().data(|d| d.get_temp::<bool>(group_id)).unwrap_or(false);
    let mut select_all = false;
    // Wraps at a large text scale instead of running past the panel (ADR-7838).
    ui.horizontal_wrapped(|ui| {
        ui.strong("Outliner");
        if ui
            .small_button("Select all")
            .on_hover_text("Select every row the search matches (Ctrl+A)")
            .clicked()
        {
            select_all = true;
        }
        if ui
            .toggle_value(&mut grouped, "Group by category")
            .on_hover_text(
                "One section per kind category (spec 101); search with kind:<name>, kind:none, kind:?",
            )
            .changed()
        {
            ui.ctx().data_mut(|d| d.insert_temp(group_id, grouped));
        }
        if ui
            .small_button("Expand all")
            .on_hover_text("Open every row and folder (Ctrl+Shift+Plus)")
            .clicked()
        {
            toggles.push(RowToggle::All(true));
        }
        if ui
            .small_button("Collapse all")
            .on_hover_text("Close every row and folder (Ctrl+Shift+Minus)")
            .clicked()
        {
            toggles.push(RowToggle::All(false));
        }
        if ui
            .small_button("New folder")
            .on_hover_text("A folder holding the selected top-level entities (Ctrl+Shift+N)")
            .clicked()
        {
            actions.push(OutlinerAction::NewFolder {
                entities: state.selection.entities().to_vec(),
                parent: None,
            });
        }
        let undoable = world.get_resource::<UndoStack>().map_or(0, UndoStack::undo_depth);
        if undoable > 0 {
            ui.weak(format!("· {undoable} undoable"));
        }
    });
    let search = ui.add(
        egui::TextEdit::singleline(&mut filter)
            .id(search_box)
            .hint_text("Search: name, kind:, type:, tag:, has:, under:, Light.intensity>1000, and / or / not")
            .desired_width(f32::INFINITY),
    );
    let search = crate::a11y_audit::name(search, egui::WidgetType::TextEdit, "Search the Outliner (Ctrl+F)");
    // The hint text is no name: a screen reader speaks this (ADR-7746).
    crate::a11y_bridge::name_node(&search, "Search the Outliner (Ctrl+F)");
    if search.lost_focus() {
        // A single-line edit gives up the focus on Enter but leaves the key in the input: the
        // panel's Enter (frame the selection) must not run too (review sprint-w12-rev-early-k row 9).
        ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
    }
    // The full syntax as the field's name and hover text: a long hint ran past a narrow panel.
    let search = crate::a11y_audit::name(
        search,
        egui::WidgetType::TextEdit,
        "Search by name (kind:<name>, kind:none, kind:?)",
    );
    rows.search = Some(search.rect);
    // The open level's revision badge (ADR-7837).
    crate::vcs::outliner_level_row(ui, world);
    // The column header: the Columns menu and the shown columns' titles (ADR-7772).
    crate::outliner_columns::header(ui, world);

    let mut view = world.remove_resource::<OutlinerView>().unwrap_or_default();
    view.update(world, &filter, grouped);
    // A malformed query filters nothing; its error shows under the box with the column.
    if let Some(error) = &view.query_error {
        ui.colored_label(
            ui.visuals().error_fg_color,
            format!("Column {}: {}", error.column, error.message),
        )
        .on_hover_text(format!("Agent error code: {}", error.code));
    }
    ui.separator();
    // A selection made outside the panel (viewport pick, agent): open its ancestors, scroll to it.
    let primary = state.selection.primary();
    if primary != view.last_primary {
        if let Some(entity) = primary {
            view.reveal(world, entity);
        }
        view.last_primary = primary;
    }

    let row_height = ui.spacing().interact_size.y;
    let row_step = row_height + ui.spacing().item_spacing.y;
    let zone_height = ui.spacing().interact_size.y * 1.5;
    let mut area = egui::ScrollArea::vertical()
        .id_salt("outliner-rows")
        .auto_shrink([false, true])
        .max_height((ui.available_height() - zone_height - ui.spacing().item_spacing.y).max(row_height));
    if let Some((index, center)) = view.scroll_to.take()
        && let Some(offset) = view.offset_for(index, center, row_step)
    {
        area = area.vertical_scroll_offset(offset);
    }
    let listed = view.rows.len();
    let mut band: Option<egui::Response> = None;
    let output = area.show_rows(ui, row_height, listed, |ui, range| {
        // The rubber band's surface: under every row widget, so it only gets drags that start
        // on empty space.
        band = Some(ui.interact(ui.clip_rect(), band_id, egui::Sense::drag()));
        for row in &view.rows[range] {
            draw_row(
                ui,
                world,
                row,
                row_height,
                &state,
                &mut rows,
                &mut actions,
                &mut toggles,
            );
        }
    });
    view.viewport = (output.state.offset.y, output.inner_rect.height());
    if let Some(band) = band {
        rubber_band(ui, &band, &rows, view.matched.as_ref(), &mut actions);
    }
    if view.tree.is_empty() && view.folders.is_empty() {
        ui.weak("(empty level — create an entity)");
    } else if !view.rows.iter().any(|row| row.entity().is_some()) && state.filtering {
        ui.weak("(no entity matches the search)");
    }

    // The root drop zone: the rest of the panel (at least one and a half rows high).
    let height = ui.available_height().max(zone_height);
    let zone = ui.allocate_response(egui::vec2(ui.available_width(), height), egui::Sense::hover());
    rows.root_drop = Some(zone.rect);
    if !state.dragged.is_empty() && zone.contains_pointer() {
        ui.painter().rect_stroke(
            zone.rect.shrink(1.0),
            2.0,
            egui::Stroke::new(1.5, ui.visuals().selection.stroke.color),
            egui::StrokeKind::Inside,
        );
        ui.painter().text(
            zone.rect.left_top() + egui::vec2(6.0, 4.0),
            egui::Align2::LEFT_TOP,
            "Move to root",
            egui::TextStyle::Small.resolve(ui.style()),
            ui.visuals().weak_text_color(),
        );
    }
    if let Some(payload) = zone.dnd_release_payload::<crate::dnd::EntityDrag>()
        && let Some(entity) = payload.entity()
    {
        let entities = hierarchy_ops::drag_set(&state.selection, entity);
        // Top-level entities in a folder leave it (ADR-7757).
        let foldered: Vec<Entity> = entities
            .iter()
            .copied()
            .filter(|e| crate::outliner_folders::folder_of(world, *e).is_some())
            .collect();
        actions.push(OutlinerAction::Reparent {
            entities,
            parent: None,
            keep_world: !ui.input(|i| i.modifiers.alt),
        });
        if !foldered.is_empty() {
            actions.push(OutlinerAction::FolderMove {
                entities: foldered,
                path: None,
            });
        }
    }

    // The drag ghost: how many entities move.
    if !state.dragged.is_empty()
        && let Some(pointer) = ui.ctx().pointer_latest_pos()
    {
        let label = match state.dragged.len() {
            1 => display_name(world, state.dragged[0]),
            n => format!("{n} entities"),
        };
        ui.ctx()
            .layer_painter(egui::LayerId::new(
                egui::Order::Tooltip,
                egui::Id::new("outliner-drag"),
            ))
            .text(
                pointer + egui::vec2(14.0, 4.0),
                egui::Align2::LEFT_TOP,
                label,
                egui::TextStyle::Small.resolve(ui.style()),
                ui.visuals().strong_text_color(),
            );
    }

    // Hotkeys while the pointer is over the panel and no text field has focus.
    if ui.ui_contains_pointer() && !ui.ctx().egui_wants_keyboard_input() {
        let selected = state.selection.entities().to_vec();
        let ctrl = egui::Modifiers::COMMAND;
        let ctrl_shift = egui::Modifiers::COMMAND | egui::Modifiers::SHIFT;
        // A user shortcut for one of these commands replaces its built-in key (ADR-2696).
        let own = |id: &str| !crate::command_palette::user_bound(world, id);
        let (own_delete, own_duplicate, own_group) = (
            own("outliner.delete"),
            own("outliner.duplicate"),
            own("outliner.group"),
        );
        let mut step: Option<(Nav, bool)> = None;
        let mut side: Option<(bool, bool)> = None;
        let mut focus_search = false;
        let mut rename = false;
        let mut frame = false;
        ui.input_mut(|input| {
            if input.consume_key(ctrl_shift, egui::Key::Z) || input.consume_key(ctrl, egui::Key::Y) {
                actions.push(OutlinerAction::Redo);
            } else if input.consume_key(ctrl, egui::Key::Z) {
                actions.push(OutlinerAction::Undo);
            }
            if input.consume_key(ctrl_shift, egui::Key::N) {
                actions.push(OutlinerAction::NewFolder {
                    entities: selected.clone(),
                    parent: None,
                });
            }
            if input.consume_key(ctrl_shift, egui::Key::Plus)
                || input.consume_key(ctrl_shift, egui::Key::Equals)
            {
                toggles.push(RowToggle::All(true));
            }
            if input.consume_key(ctrl_shift, egui::Key::Minus) {
                toggles.push(RowToggle::All(false));
            }
            if input.consume_key(ctrl, egui::Key::A) {
                select_all = true;
            }
            if input.consume_key(ctrl, egui::Key::F) {
                focus_search = true;
            }
            if !selected.is_empty() {
                if own_delete && input.consume_key(egui::Modifiers::NONE, egui::Key::Delete) {
                    actions.push(OutlinerAction::Delete {
                        entities: selected.clone(),
                    });
                } else if own_duplicate && input.consume_key(ctrl, egui::Key::D) {
                    actions.push(OutlinerAction::Duplicate {
                        entities: selected.clone(),
                    });
                } else if own_group && input.consume_key(ctrl, egui::Key::G) {
                    actions.push(OutlinerAction::Group {
                        entities: selected.clone(),
                    });
                }
                if input.consume_key(egui::Modifiers::NONE, egui::Key::F2) {
                    rename = true;
                }
                if input.consume_key(egui::Modifiers::NONE, egui::Key::Enter) {
                    frame = true;
                }
            }
            for (key, nav) in [
                (egui::Key::ArrowDown, Nav::Down),
                (egui::Key::ArrowUp, Nav::Up),
                (egui::Key::Home, Nav::Home),
                (egui::Key::End, Nav::End),
                (egui::Key::PageUp, Nav::PageUp),
                (egui::Key::PageDown, Nav::PageDown),
            ] {
                // Shift first: egui's NONE pattern also matches a held Shift.
                if input.consume_key(egui::Modifiers::SHIFT, key) {
                    step = Some((nav, true));
                } else if input.consume_key(egui::Modifiers::NONE, key) {
                    step = Some((nav, false));
                }
            }
            for (key, right) in [(egui::Key::ArrowLeft, false), (egui::Key::ArrowRight, true)] {
                if input.consume_key(egui::Modifiers::SHIFT, key) {
                    side = Some((right, true));
                } else if input.consume_key(egui::Modifiers::NONE, key) {
                    side = Some((right, false));
                }
            }
        });
        if focus_search {
            ui.ctx().memory_mut(|m| m.request_focus(search_box));
        }
        if let (true, Some(primary)) = (rename, state.selection.primary()) {
            toggles.push(RowToggle::BeginRename(RenameTarget::Entity(primary)));
        }
        if frame {
            actions.push(OutlinerAction::Focus { entities: selected });
        }
        let page = ((view.viewport.1 / row_step).floor() as usize).max(1);
        if let Some((nav, extend)) = step
            && let Some((index, entity)) = nav_target(&view.rows, state.selection.primary(), nav, page)
        {
            actions.push(OutlinerAction::Click {
                entity,
                mode: if extend {
                    ClickMode::Range
                } else {
                    ClickMode::Replace
                },
            });
            view.scroll_to = Some((index, false));
        }
        if let Some((right, branch)) = side
            && let Some(primary) = state.selection.primary()
        {
            if branch {
                toggles.push(RowToggle::Branch(primary, right));
            } else {
                match side_step(&view.rows, Some(primary), right) {
                    Some(SideStep::Open(entity, open)) => toggles.push(RowToggle::Entity(entity, open)),
                    Some(SideStep::Select(index, entity)) => {
                        actions.push(OutlinerAction::Click {
                            entity,
                            mode: ClickMode::Replace,
                        });
                        view.scroll_to = Some((index, false));
                    }
                    // Several open levels (P-097): Left on a root with nothing left to close
                    // closes its level's group, and Right on a selection hidden in a closed group
                    // opens it again.
                    None => {
                        let root = world.get::<ChildOf>(primary).is_none();
                        if let Some(document) = view.level_of(world, primary) {
                            let open = view.is_level_open(document);
                            if !right && open && root {
                                toggles.push(RowToggle::Level(document, false));
                            } else if right && !open {
                                toggles.push(RowToggle::Level(document, true));
                            }
                        }
                    }
                }
            }
        }
    }
    if select_all {
        actions.push(OutlinerAction::SelectAll {
            only: view.matched.as_ref().map(|matched| {
                view.rows
                    .iter()
                    .filter_map(FlatRow::entity)
                    .filter(|e| matched.contains(e))
                    .collect()
            }),
        });
    }

    for toggle in toggles {
        match toggle {
            RowToggle::Entity(entity, open) => view.set_open(entity, open),
            RowToggle::Category(name, open) => view.set_category_open(&name, open),
            RowToggle::Level(document, open) => view.set_level_open(document, open),
            RowToggle::Folder(path, open) => view.set_folder_open(&path, open),
            RowToggle::Branch(entity, open) => {
                view.set_open_recursive(entity, open);
            }
            RowToggle::All(open) => view.set_all_open(open),
            RowToggle::BeginRename(target) => begin_rename(ui.ctx(), world, target),
            RowToggle::EndRename => ui
                .ctx()
                .data_mut(|d| d.remove::<InlineRename>(egui::Id::new(INLINE_RENAME))),
            RowToggle::RenameNext(entity) => {
                let next = arrow_target(&view.rows, Some(entity), true);
                match next {
                    Some((index, next)) => {
                        actions.push(OutlinerAction::Click {
                            entity: next,
                            mode: ClickMode::Replace,
                        });
                        view.scroll_to = Some((index, false));
                        begin_rename(ui.ctx(), world, RenameTarget::Entity(next));
                    }
                    None => ui
                        .ctx()
                        .data_mut(|d| d.remove::<InlineRename>(egui::Id::new(INLINE_RENAME))),
                }
            }
        }
    }
    if view
        .view_key
        .as_ref()
        .is_some_and(|key| key.2 != view.open_revision)
    {
        view.flatten(world, &filter, grouped);
    }
    rows.listed = view.rows.iter().filter_map(FlatRow::entity).collect();
    let matched = view.matched.clone();
    world.insert_resource(view);

    ui.ctx().data_mut(|d| d.insert_temp(filter_id, filter.clone()));
    rows.filter = filter;
    let order = rows.order();
    world.insert_resource(rows);
    let acted = !actions.is_empty();
    let mut rename_next: Option<RenameTarget> = None;
    for action in actions {
        if let Some(target) = perform(action, world, &order, matched.as_ref()) {
            rename_next = Some(target);
        }
    }
    if let Some(target) = rename_next {
        begin_rename(ui.ctx(), world, target);
    }
    if acted {
        // The panel's own selection changes never scroll (the row is already on screen, or the
        // arrow key asked for its own scroll).
        let primary = selection_of(world).primary();
        if let Some(mut view) = world.get_resource_mut::<OutlinerView>() {
            view.last_primary = primary;
        }
    }

    create_prefab_dialog(ui.ctx(), world);
    crate::placed_break::outliner_frame(ui.ctx(), world);
    // Row menu actions, unique names and configuration warnings (on change only, ADR-7772).
    crate::outliner_columns::apply_row_menu_actions(ui.ctx(), world);
}

/// egui memory key of the Create prefab dialog's state ([`CreatePrefabDialog`]).
const CREATE_PREFAB_DIALOG: &str = "outliner-create-prefab";

/// The open Create prefab dialog: which entities, the path being typed, the last refusal.
#[derive(Clone, Debug, Default)]
struct CreatePrefabDialog {
    /// The entities the prefab is made from (the context menu's targets).
    entities: Vec<Entity>,
    /// Project-relative path of the new file.
    path: String,
    /// Why the last Create was refused (shown under the field).
    error: Option<String>,
}

/// Opens the Create prefab dialog for `entities` with a default path after the first one's name.
fn open_create_prefab_dialog(ctx: &egui::Context, world: &World, entities: &[Entity]) {
    let dialog = CreatePrefabDialog {
        entities: entities.to_vec(),
        path: crate::prefab_create::default_prefab_path(world, entities),
        error: None,
    };
    ctx.data_mut(|d| d.insert_temp(egui::Id::new(CREATE_PREFAB_DIALOG), dialog));
}

/// Creates the prefab the dialog names: the path is confined to the project's `assets/` like the
/// agent method's (`path_outside_project` otherwise).
fn create_prefab_from_dialog(world: &mut World, dialog: &CreatePrefabDialog) -> Result<(), String> {
    let root = crate::agent_ext::project_root(world).map_err(|e| e.message.clone())?;
    let full =
        crate::prefab_create::confined_prefab_path(&root, &dialog.path).map_err(|e| e.message.clone())?;
    crate::prefab_create::create_prefab(world, &dialog.entities, &full)
        .map(drop)
        .map_err(|err| format!("{err} ({})", err.code()))
}

/// Draws the Create prefab dialog while it is open.
fn create_prefab_dialog(ctx: &egui::Context, world: &mut World) {
    let id = egui::Id::new(CREATE_PREFAB_DIALOG);
    let Some(mut dialog) = ctx.data(|d| d.get_temp::<CreatePrefabDialog>(id)) else {
        return;
    };
    let mut close = false;
    egui::Window::new("Create prefab from selection")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .show(ctx, |ui| {
            ui.label(format!(
                "{} entit{} become a prefab and are replaced by an instance of it.",
                dialog.entities.len(),
                if dialog.entities.len() == 1 { "y" } else { "ies" }
            ));
            ui.horizontal(|ui| {
                let label = ui.label("File");
                ui.text_edit_singleline(&mut dialog.path).labelled_by(label.id);
            });
            if let Some(error) = &dialog.error {
                ui.colored_label(ui.visuals().error_fg_color, error);
            }
            ui.horizontal(|ui| {
                if crate::agent_parity::action_button(
                    ui,
                    &crate::agent_parity::OUTLINER_CREATE_PREFAB_CONFIRM,
                )
                .clicked()
                {
                    match create_prefab_from_dialog(world, &dialog) {
                        Ok(()) => close = true,
                        Err(message) => dialog.error = Some(message),
                    }
                }
                if crate::agent_parity::action_button(ui, &crate::agent_parity::OUTLINER_CREATE_PREFAB_CANCEL)
                    .clicked()
                {
                    close = true;
                }
            });
        });
    if close {
        ctx.data_mut(|d| d.remove::<CreatePrefabDialog>(id));
    } else {
        ctx.data_mut(|d| d.insert_temp(id, dialog));
    }
}

/// The tree's rubber band (ADR-7757): a drag that starts on empty space between or beside the
/// rows draws a rectangle; on release the drawn rows it touches are selected (only the rows the
/// search matches while searching; Ctrl adds them to the selection).
fn rubber_band(
    ui: &egui::Ui,
    band: &egui::Response,
    rows: &OutlinerRows,
    matched: Option<&HashSet<Entity>>,
    actions: &mut Vec<OutlinerAction>,
) {
    let id = egui::Id::new("outliner-rubber-band-start");
    if band.drag_started()
        && let Some(origin) = ui.input(|i| i.pointer.press_origin())
    {
        ui.ctx().data_mut(|d| d.insert_temp(id, origin));
    }
    let Some(start) = ui.ctx().data(|d| d.get_temp::<egui::Pos2>(id)) else {
        return;
    };
    let Some(now) = ui.ctx().pointer_latest_pos() else {
        return;
    };
    let rect = egui::Rect::from_two_pos(start, now);
    if band.dragged() {
        ui.painter().rect(
            rect,
            0.0,
            ui.visuals().selection.bg_fill.gamma_multiply(0.25),
            egui::Stroke::new(1.0, ui.visuals().selection.stroke.color),
            egui::StrokeKind::Inside,
        );
    }
    if band.drag_stopped() || !band.dragged() {
        ui.ctx().data_mut(|d| d.remove::<egui::Pos2>(id));
    }
    if band.drag_stopped() {
        let entities = band_rows(&rows.rows, rect, matched);
        actions.push(OutlinerAction::Band {
            entities,
            add: ui.input(|i| i.modifiers.command),
        });
    }
}

/// The drawn rows a rubber band `rect` touches (by height), only the matching ones while
/// searching.
fn band_rows(rows: &[OutlinerRow], rect: egui::Rect, matched: Option<&HashSet<Entity>>) -> Vec<Entity> {
    rows.iter()
        .filter(|row| row.rect.top() <= rect.bottom() && row.rect.bottom() >= rect.top())
        .map(|row| row.entity)
        .filter(|e| matched.is_none_or(|m| m.contains(e)))
        .collect()
}

/// Draws a folder row (ADR-7757): its arrow, its name (a button: a click or Enter opens or
/// closes it; F2 renames it in place and Delete deletes it while it has keyboard focus), a drop
/// target that moves dragged entities into the folder, and a context menu.
/// The fields of a folder row, borrowed from its [`FlatRow`].
struct FolderRow<'a> {
    path: &'a str,
    name: &'a str,
    depth: usize,
    count: usize,
    open: bool,
}

fn draw_folder_row(
    ui: &mut egui::Ui,
    row: FolderRow<'_>,
    state: &DrawState,
    rows: &mut OutlinerRows,
    actions: &mut Vec<OutlinerAction>,
    toggles: &mut Vec<RowToggle>,
) {
    let FolderRow {
        path,
        name,
        depth,
        count,
        open,
    } = row;
    ui.add_space(depth as f32 * ui.spacing().indent);
    let toggle = ui.add_enabled(
        !state.filtering,
        egui::Button::new(if open { "\u{25be}" } else { "\u{25b8}" }).small(),
    );
    let toggle = crate::a11y_audit::name(
        toggle,
        egui::WidgetType::Button,
        if open { "Collapse folder" } else { "Expand folder" },
    );
    if toggle.clicked() {
        toggles.push(RowToggle::Folder(path.to_string(), !open));
    }
    if let Some(field) = inline_rename_field(ui, &RenameTarget::Folder(path.to_string()), actions, toggles) {
        rows.folders.push((path.to_string(), field.rect));
        return;
    }
    let label = ui
        .add(egui::Button::new(format!("\u{1f4c1} {name} ({count})")).frame(false))
        .on_hover_text(format!(
            "Folder {path}: drop entities here; F2 renames, Delete deletes it"
        ));
    rows.folders.push((path.to_string(), label.rect));
    if label.clicked() {
        toggles.push(RowToggle::Folder(path.to_string(), !open));
    }
    if label.has_focus() {
        let (rename, delete) = ui.input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::NONE, egui::Key::F2),
                i.consume_key(egui::Modifiers::NONE, egui::Key::Delete),
            )
        });
        if rename {
            toggles.push(RowToggle::BeginRename(RenameTarget::Folder(path.to_string())));
        }
        if delete {
            actions.push(OutlinerAction::FolderDelete {
                path: path.to_string(),
            });
        }
    }
    if !state.dragged.is_empty() && label.contains_pointer() {
        ui.painter().rect_stroke(
            label.rect,
            2.0,
            egui::Stroke::new(1.5, ui.visuals().selection.stroke.color),
            egui::StrokeKind::Middle,
        );
    }
    if let Some(payload) = label.dnd_release_payload::<crate::dnd::EntityDrag>()
        && let Some(dragged) = payload.entity()
    {
        let entities = hierarchy_ops::drag_set(&state.selection, dragged);
        // A child entity leaves its parent first (folders hold top-level entities).
        let children: Vec<Entity> = entities
            .iter()
            .copied()
            .filter(|e| !state.top_level.contains(e))
            .collect();
        if !children.is_empty() {
            actions.push(OutlinerAction::Reparent {
                entities: children,
                parent: None,
                keep_world: true,
            });
        }
        actions.push(OutlinerAction::FolderMove {
            entities,
            path: Some(path.to_string()),
        });
    }
    label.context_menu(|ui| {
        if ui.button("Rename folder").clicked() {
            toggles.push(RowToggle::BeginRename(RenameTarget::Folder(path.to_string())));
            ui.close();
        }
        if ui.button("New folder inside").clicked() {
            actions.push(OutlinerAction::NewFolder {
                entities: Vec::new(),
                parent: Some(path.to_string()),
            });
            ui.close();
        }
        if ui.button("Move selection here").clicked() {
            actions.push(OutlinerAction::FolderMove {
                entities: state
                    .selection
                    .entities()
                    .iter()
                    .copied()
                    .filter(|e| state.top_level.contains(e))
                    .collect(),
                path: Some(path.to_string()),
            });
            ui.close();
        }
        ui.separator();
        if ui.button("Delete folder (keeps its entities)").clicked() {
            actions.push(OutlinerAction::FolderDelete {
                path: path.to_string(),
            });
            ui.close();
        }
    });
}

/// The entity row an arrow key moves the selection to: the next (or previous) entity row after
/// the primary's, or the first (last) row when nothing is selected or the primary is not listed.
fn arrow_target(rows: &[FlatRow], primary: Option<Entity>, down: bool) -> Option<(usize, Entity)> {
    let entity_rows = || {
        rows.iter()
            .enumerate()
            .filter_map(|(i, row)| row.entity().map(|e| (i, e)))
    };
    let current = primary.and_then(|p| rows.iter().position(|row| row.entity() == Some(p)));
    match (current, down) {
        (Some(at), true) => entity_rows().find(|(i, _)| *i > at),
        (Some(at), false) => entity_rows().rev().find(|(i, _)| *i < at),
        (None, true) => entity_rows().next(),
        (None, false) => entity_rows().next_back(),
    }
}

/// The row's kind or role glyph (tinted with the kind's color), its hover text, and the orange
/// `?` of an unclassified entity (spec 101 § 4.1).
fn draw_row_icon(ui: &mut egui::Ui, icon: &crate::kind_icons::RowIcon) {
    if icon.glyph.is_empty() {
        return;
    }
    let color = icon.color.map_or_else(
        || ui.visuals().text_color(),
        |[r, g, b]| egui::Color32::from_rgb(r, g, b),
    );
    let response = ui.label(egui::RichText::new(icon.glyph).color(color));
    let response = if icon.unclassified {
        response.union(
            ui.label(
                egui::RichText::new("?")
                    .strong()
                    .color(egui::Color32::from_rgb(0xff, 0x9a, 0x2e)),
            ),
        )
    } else {
        response
    };
    if !icon.tooltip.is_empty() {
        response.on_hover_text(icon.tooltip.as_str());
    }
}

/// Whether a plain click on `entity`'s label is a slow second click (ADR-7757): the row is
/// already the only selection, the click is not part of a double click, and the last click on
/// it (if any) is older than the double-click delay. That click edits the name in place.
fn slow_second_click(ui: &egui::Ui, state: &DrawState, entity: Entity, response: &egui::Response) -> bool {
    let id = egui::Id::new("outliner-last-click");
    let now = ui.input(|i| i.time);
    let last = ui.ctx().data(|d| d.get_temp::<(Entity, f64)>(id));
    ui.ctx().data_mut(|d| d.insert_temp(id, (entity, now)));
    let delay = ui.ctx().options(|o| o.input_options.max_double_click_delay);
    state.selection.len() == 1
        && state.selection.primary() == Some(entity)
        && !response.double_clicked()
        && last.is_none_or(|(e, at)| e != entity || now - at >= delay)
}

/// Whether dropping the drag set on `target` is allowed (no entity would land in its own subtree).
fn drop_allowed(world: &World, dragged: &[Entity], target: Entity) -> bool {
    !dragged.is_empty()
        && dragged
            .iter()
            .all(|entity| !reparent_creates_cycle(world, *entity, target))
}

/// Draws one flat row in a rect of exactly `row_height` (the scroll area's row math relies on it).
#[allow(clippy::too_many_arguments)] // the per-draw buffers of one panel, split for borrows
fn draw_row(
    ui: &mut egui::Ui,
    world: &World,
    row: &FlatRow,
    row_height: f32,
    state: &DrawState,
    rows: &mut OutlinerRows,
    actions: &mut Vec<OutlinerAction>,
    toggles: &mut Vec<RowToggle>,
) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), row_height), egui::Sense::hover());
    let salt = match row {
        FlatRow::Entity { entity, .. } => egui::Id::new(("outliner-row", entity.to_bits())),
        FlatRow::Category { name, .. } => egui::Id::new(("outliner-category", name.as_str())),
        FlatRow::Level { document, .. } => egui::Id::new(("outliner-level", *document)),
        FlatRow::Folder { path, .. } => egui::Id::new(("outliner-folder", path.as_str())),
    };
    let mut ui = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .id_salt(salt)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    let ui = &mut ui;
    match row {
        FlatRow::Category { name, count, open } => {
            let heading = format!("{} {name} ({count})", if *open { "\u{25be}" } else { "\u{25b8}" });
            if ui
                .add(egui::Button::new(egui::RichText::new(heading).strong()).frame(false))
                .clicked()
            {
                toggles.push(RowToggle::Category(name.clone(), !*open));
            }
        }
        FlatRow::Level {
            document,
            label,
            count,
            open,
            active,
        } => {
            // A focusable button: Tab reaches it and Enter or Space opens or closes the group.
            let tokens = crate::theme::tokens(world);
            let heading = format!(
                "{} {label} ({count})",
                if *open { "\u{25be}" } else { "\u{25b8}" }
            );
            let color = if *active { tokens.signal } else { tokens.strong };
            if ui
                .add(egui::Button::new(egui::RichText::new(heading).strong().color(color)).frame(false))
                .clicked()
            {
                toggles.push(RowToggle::Level(*document, !*open));
            }
        }
        FlatRow::Folder {
            path,
            name,
            depth,
            count,
            open,
        } => draw_folder_row(
            ui,
            FolderRow {
                path,
                name,
                depth: *depth,
                count: *count,
                open: *open,
            },
            state,
            rows,
            actions,
            toggles,
        ),
        FlatRow::Entity {
            entity,
            label,
            icon,
            depth,
            has_children,
            open,
        } => draw_entity_row(
            ui,
            world,
            EntityRow {
                entity: *entity,
                label,
                icon,
                depth: *depth,
                has_children: *has_children,
                open: *open,
            },
            state,
            rows,
            actions,
            toggles,
        ),
    }
}

/// The fields of an entity row, borrowed from its [`FlatRow`].
struct EntityRow<'a> {
    entity: Entity,
    label: &'a str,
    icon: &'a crate::kind_icons::RowIcon,
    depth: usize,
    has_children: bool,
    open: bool,
}

fn draw_entity_row(
    ui: &mut egui::Ui,
    world: &World,
    row: EntityRow<'_>,
    state: &DrawState,
    rows: &mut OutlinerRows,
    actions: &mut Vec<OutlinerAction>,
    toggles: &mut Vec<RowToggle>,
) {
    let entity = row.entity;
    let selected = state.selection.contains(entity);
    // The row's user color (ADR-7772).
    crate::outliner_columns::paint_row_tint(ui, world, entity, ui.max_rect());
    let shows_visible_locked = crate::outliner_columns::shows_visible_locked(world);
    ui.add_space(row.depth as f32 * ui.spacing().indent);
    // The disclosure toggle (disabled without children or while searching), the eye and the lock
    // (ADR-7204), then the prefab link and its override marker.
    let toggle = ui.add_enabled(
        row.has_children && !state.filtering,
        egui::Button::new(if row.open { "\u{25be}" } else { "\u{25b8}" }).small(),
    );
    // The glyph is the only text: give the button a spoken name (ADR-9810, E-2).
    let toggle = crate::a11y_audit::name(
        toggle,
        egui::WidgetType::Button,
        if row.open {
            "Collapse children"
        } else {
            "Expand children"
        },
    );
    if toggle.clicked() {
        // Shift+click opens or closes the whole branch (ADR-7757).
        if ui.input(|i| i.modifiers.shift) {
            toggles.push(RowToggle::Branch(entity, !row.open));
        } else {
            toggles.push(RowToggle::Entity(entity, !row.open));
        }
    }
    let visible = crate::viewport_ops::is_visible(world, entity);
    // An entity hidden in the editor only (H, isolate; ADR-7770) shows a dimmed eye; the eye
    // itself stays the saved visibility.
    let editor_hidden = visible && crate::view_state::has_view_hidden(world, entity);
    let glyph = egui::RichText::new(if visible { "\u{1f441}" } else { "\u{2013}" });
    let glyph = if editor_hidden {
        glyph.color(crate::theme::tokens(world).muted)
    } else {
        glyph
    };
    let eye = ui.add_visible(
        shows_visible_locked,
        egui::Button::new(glyph).small().frame(false),
    );
    // The glyph alone is no name a screen reader can speak: say what it does, to which row (ADR-7746).
    let spoken = if visible {
        format!("Hide {} in the viewport", row.label)
    } else {
        format!("Show {} in the viewport", row.label)
    };
    let enabled = eye.enabled();
    eye.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, &spoken));
    if eye
        .on_hover_text(if editor_hidden {
            "Hidden in the editor only (Alt+H shows it); click to hide it in the saved level"
        } else if visible {
            "Hide in the viewport"
        } else {
            "Show in the viewport"
        })
        .clicked()
    {
        actions.push(OutlinerAction::SetVisible {
            entity,
            visible: !visible,
        });
    }
    let locked = world.get::<crate::viewport_ops::EditorLocked>(entity).is_some();
    let lock = ui.add_visible(
        shows_visible_locked,
        egui::Button::new(if locked { "\u{1f512}" } else { "\u{1f513}" })
            .small()
            .frame(false),
    );
    let spoken = if locked {
        format!("Unlock {}: pickable in the viewport", row.label)
    } else {
        format!("Lock {}: not pickable in the viewport", row.label)
    };
    let enabled = lock.enabled();
    lock.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, &spoken));
    if lock
        .on_hover_text(if locked {
            "Unlock: pickable in the viewport"
        } else {
            "Lock: not pickable in the viewport"
        })
        .clicked()
    {
        actions.push(OutlinerAction::SetLocked {
            entity,
            locked: !locked,
        });
    }
    draw_row_icon(ui, row.icon);
    crate::collab::outliner_indicator(ui, world, entity);
    let (prefab, overridden) = crate::viewport_ops::prefab_marks(world, entity);
    if prefab {
        ui.label("\u{1f517}").on_hover_text("Prefab instance");
    }
    if overridden {
        // The override dot opens the instance's Overrides window (ADR-7790).
        let dot = crate::agent_parity::action_button_text(
            ui,
            &crate::agent_parity::OUTLINER_OPEN_OVERRIDES,
            egui::RichText::new("\u{25cf}").strong(),
        );
        if crate::a11y_audit::name(dot, egui::WidgetType::Button, "Overrides")
            .on_hover_text("This prefab instance has overrides: show them")
            .clicked()
        {
            actions.push(OutlinerAction::OpenOverrides { entity });
        }
    }
    // The in-place rename field replaces the label while the row is renamed (ADR-7757).
    if let Some(field) = inline_rename_field(ui, &RenameTarget::Entity(entity), actions, toggles) {
        rows.rows.push(OutlinerRow {
            entity,
            rect: field.rect,
            toggle: toggle.rect,
            depth: row.depth,
        });
        return;
    }
    crate::level_variant::outliner_mark(ui, world, entity);
    // An entity without a Name shows `<Kind> (unnamed)`, its id in the tooltip (ADR-8586).
    let hex_id = format!("{:016x}", entity.to_bits());
    let unnamed = !has_name(world, entity);
    let shown = if unnamed {
        row.label.replacen(&hex_id, &unnamed_label(row.icon), 1)
    } else {
        row.label.to_owned()
    };
    let row = EntityRow { label: &shown, ..row };
    let response = ui.add(egui::Button::selectable(selected, row.label).sense(egui::Sense::click_and_drag()));
    let response = if unnamed {
        response.on_hover_text(format!("Entity id {hex_id}"))
    } else {
        response
    };
    // Badges, the configuration-warning badge and the shown column cells (ADR-7772).
    crate::outliner_columns::draw_row_extras(ui, world, entity);
    rows.rows.push(OutlinerRow {
        entity,
        rect: response.rect,
        toggle: toggle.rect,
        depth: row.depth,
    });

    // Selection on click with modifier support.
    if response.clicked() {
        let modifiers = response.ctx.input(|i| i.modifiers);
        let mode = match (modifiers.command, modifiers.shift) {
            (true, true) => ClickMode::AddRange,
            (false, true) => ClickMode::Range,
            (true, false) => ClickMode::Toggle,
            (false, false) => ClickMode::Replace,
        };
        if mode == ClickMode::Replace && slow_second_click(ui, state, entity, &response) {
            toggles.push(RowToggle::BeginRename(RenameTarget::Entity(entity)));
        } else {
            actions.push(OutlinerAction::Click { entity, mode });
        }
    }

    // Drag to reparent: carry the row's entity; the drop resolves the drag set (the selection
    // when the row is selected).
    response.dnd_set_drag_payload(crate::dnd::EntityDrag {
        entity: entity.to_bits(),
    });
    if !state.dragged.is_empty() && response.contains_pointer() && !state.dragged.contains(&entity) {
        let color = if drop_allowed(world, &state.dragged, entity) {
            ui.visuals().selection.stroke.color
        } else {
            ui.visuals().error_fg_color
        };
        ui.painter().rect_stroke(
            response.rect,
            2.0,
            egui::Stroke::new(1.5, color),
            egui::StrokeKind::Middle,
        );
    }
    if let Some(payload) = response.dnd_release_payload::<crate::dnd::EntityDrag>()
        && let Some(dragged) = payload.entity()
    {
        let entities = hierarchy_ops::drag_set(&state.selection, dragged);
        if !entities.contains(&entity) {
            actions.push(OutlinerAction::Reparent {
                entities,
                parent: Some(entity),
                keep_world: !ui.input(|i| i.modifiers.alt),
            });
        }
    }

    // Context menu: acts on the selection when the row is part of it, else on the row.
    response.context_menu(|ui| {
        let targets = if selected && state.selection.len() > 1 {
            state.selection.entities().to_vec()
        } else {
            vec![entity]
        };
        let many = targets.len() > 1;
        let suffix = if many {
            format!(" ({} entities)", targets.len())
        } else {
            String::new()
        };
        if !many && ui.button("Rename (F2)").clicked() {
            toggles.push(RowToggle::BeginRename(RenameTarget::Entity(entity)));
            ui.close();
        }
        if !many && row.has_children {
            if ui.button("Expand all below").clicked() {
                toggles.push(RowToggle::Branch(entity, true));
                ui.close();
            }
            if ui.button("Collapse all below").clicked() {
                toggles.push(RowToggle::Branch(entity, false));
                ui.close();
            }
        }
        let clip = world
            .get_resource::<crate::viewport_ops::EditorClipboard>()
            .is_some_and(|c| c.file.is_some());
        if !many
            && ui
                .add_enabled(clip, egui::Button::new("Paste as child"))
                .clicked()
        {
            actions.push(OutlinerAction::PasteAsChild { entity });
            ui.close();
        }
        if !many && ui.button("Create empty child").clicked() {
            actions.push(OutlinerAction::CreateChild { entity });
            ui.close();
        }
        if ui.button(format!("Focus{suffix}")).clicked() {
            actions.push(OutlinerAction::Focus {
                entities: targets.clone(),
            });
            ui.close();
        }
        if ui.button(format!("Duplicate{suffix}")).clicked() {
            actions.push(OutlinerAction::Duplicate {
                entities: targets.clone(),
            });
            ui.close();
        }
        if ui.button(format!("Group{suffix}")).clicked() {
            actions.push(OutlinerAction::Group {
                entities: targets.clone(),
            });
            ui.close();
        }
        // Create prefab from selection… (ADR-7468): asks for the file, then `editor.create_prefab`.
        if crate::agent_parity::action_menu_item(ui, &crate::agent_parity::OUTLINER_CREATE_PREFAB).clicked() {
            open_create_prefab_dialog(ui.ctx(), world, &targets);
            ui.close();
        }
        // Edit in grid (ADR-7787): the targets as rows of the Bulk Edit grid.
        if crate::agent_parity::action_menu_item(ui, &crate::agent_parity::OUTLINER_EDIT_IN_GRID).clicked() {
            actions.push(OutlinerAction::EditInGrid {
                entities: targets.clone(),
            });
            ui.close();
        }
        // Edit / Break placed level, Create variant… (spec 37 § 6.5, spec 83 § 2.4, ADR-7614).
        if !many {
            crate::placed_break::outliner_row_menu(ui, world, entity);
        }
        let has_parent = targets.iter().any(|e| world.get::<ChildOf>(*e).is_some());
        if ui
            .add_enabled(has_parent, egui::Button::new(format!("Unparent{suffix}")))
            .clicked()
        {
            actions.push(OutlinerAction::Reparent {
                entities: targets.clone(),
                parent: None,
                keep_world: true,
            });
            ui.close();
        }
        // Extension actions that joined this menu, under their selection predicates (ADR-7791).
        let selection = crate::extension::actions::MenuSelection {
            entities: targets.clone(),
            assets: Vec::new(),
        };
        if let Some(id) = crate::extension::actions::context_entries(
            ui,
            world,
            crate::extension::actions::ActionPlace::Outliner,
            &selection,
        ) {
            actions.push(OutlinerAction::Extension {
                id,
                entities: targets.clone(),
            });
        }
        ui.separator();
        if ui.button("Select subtree").clicked() {
            actions.push(OutlinerAction::SelectScope {
                entities: targets.clone(),
                scope: SelectScope::Subtree,
            });
            ui.close();
        }
        if ui.button("Select children").clicked() {
            actions.push(OutlinerAction::SelectScope {
                entities: targets.clone(),
                scope: SelectScope::Children,
            });
            ui.close();
        }
        // Select similar and the selection root (ADR-7769).
        crate::agent_parity::menu(ui, "Select similar", |ui| {
            for (action, by) in crate::selection::OUTLINER_SIMILAR {
                if crate::agent_parity::action_menu_item(ui, action).clicked() {
                    actions.push(OutlinerAction::SelectSimilar {
                        entities: targets.clone(),
                        by,
                    });
                    ui.close();
                }
            }
        });
        let mut root = targets
            .iter()
            .all(|e| world.get::<crate::selection::EditorSelectionRoot>(*e).is_some());
        if crate::agent_parity::action_toggle(ui, &crate::agent_parity::OUTLINER_SELECTION_ROOT, &mut root)
            .clicked()
        {
            actions.push(OutlinerAction::SelectionRoot {
                entities: targets.clone(),
                on: root,
            });
            ui.close();
        }
        // Tags, Unique name, Change kind, Row color (ADR-7772).
        ui.separator();
        crate::outliner_columns::row_menu(ui, world, entity, &targets);
        ui.separator();
        if ui.button(format!("Delete{suffix}")).clicked() {
            actions.push(OutlinerAction::Delete { entities: targets });
            ui.close();
        }
    });
}

/// Entity ids from an optional array parameter.
fn entity_list(
    params: &serde_json::Value,
    name: &str,
) -> Result<Vec<Entity>, regia_agent::protocol::RpcError> {
    match params.get(name) {
        None | Some(serde_json::Value::Null) => Ok(Vec::new()),
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_u64()
                    .and_then(crate::command::entity_from_bits)
                    .ok_or_else(|| {
                        regia_agent::protocol::RpcError::wrong_type(name, "an array of entity ids")
                    })
            })
            .collect(),
        Some(_) => Err(regia_agent::protocol::RpcError::wrong_type(
            name,
            "an array of entity ids",
        )),
    }
}

/// Document ids (guid text) from an optional array parameter.
fn document_list(
    params: &serde_json::Value,
    name: &str,
) -> Result<Vec<u128>, regia_agent::protocol::RpcError> {
    let wrong = || regia_agent::protocol::RpcError::wrong_type(name, "an array of document ids");
    match params.get(name) {
        None | Some(serde_json::Value::Null) => Ok(Vec::new()),
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .and_then(|text| text.parse::<regia_core::id::Guid>().ok())
                    .map(regia_core::id::Guid::as_u128)
                    .ok_or_else(wrong)
            })
            .collect(),
        Some(_) => Err(wrong()),
    }
}

/// `editor.outliner_rows`: opens, closes or reveals rows, then returns a window of the listed rows.
fn outliner_rows_method(
    world: &mut World,
    params: &serde_json::Value,
) -> Result<serde_json::Value, regia_agent::protocol::RpcError> {
    use regia_agent::protocol::RpcError;
    use serde_json::{Value, json};
    let open = entity_list(params, "open")?;
    let close = entity_list(params, "close")?;
    let reveal = match params.get("reveal") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_u64()
                .and_then(crate::command::entity_from_bits)
                .ok_or_else(|| RpcError::wrong_type("reveal", "an entity id"))?,
        ),
    };
    let count = |name: &str, default: u64, max: u64| match params.get(name) {
        None | Some(Value::Null) => Ok(default),
        Some(value) => value
            .as_u64()
            .filter(|n| *n <= max)
            .ok_or_else(|| RpcError::wrong_type(name, &format!("an integer from 0 to {max}"))),
    };
    let offset = count("offset", 0, u64::MAX)? as usize;
    let limit = count("limit", 100, 1_000)? as usize;
    // Every parameter is read before the view leaves the world: an error must not drop it.
    let open_levels = document_list(params, "open_levels")?;
    let close_levels = document_list(params, "close_levels")?;
    let mut view = world.remove_resource::<OutlinerView>().unwrap_or_default();
    // The search and grouping of the last draw (none before the panel was drawn).
    let (filter, grouped) = view
        .view_key
        .as_ref()
        .map_or((String::new(), false), |key| (key.0.clone(), key.1));
    for entity in open {
        view.set_open(entity, true);
    }
    for entity in close {
        view.set_open(entity, false);
    }
    for document in &open_levels {
        view.set_level_open(*document, true);
    }
    for document in &close_levels {
        view.set_level_open(*document, false);
    }
    view.update(world, &filter, grouped);
    let revealed = reveal.map(|entity| view.reveal(world, entity));
    let rows: Vec<Value> = view
        .rows
        .iter()
        .skip(offset)
        .take(limit)
        .map(|row| match row {
            FlatRow::Category { name, count, open } => {
                json!({ "category": name, "count": count, "open": open })
            }
            FlatRow::Level {
                document,
                label,
                count,
                open,
                active,
            } => json!({
                "level": regia_core::id::Guid::from_u128(*document).to_string(),
                "label": label,
                "count": count,
                "open": open,
                "active": active,
            }),
            FlatRow::Folder {
                path,
                name,
                depth,
                count,
                open,
            } => json!({ "folder": path, "label": name, "depth": depth, "count": count, "open": open }),
            FlatRow::Entity {
                entity,
                label,
                depth,
                has_children,
                open,
                ..
            } => json!({
                "entity": entity.to_bits(),
                "label": label,
                "depth": depth,
                "has_children": has_children,
                "open": open,
            }),
        })
        .collect();
    let reply = json!({
        "listed": view.rows.len(),
        "filter": filter,
        "grouped": grouped,
        "offset": offset,
        "rows": rows,
        "revealed": revealed,
        "reveal_index": reveal.and_then(|entity| view.index_of(entity)),
        "tree_builds": view.tree_builds,
        "flattens": view.flattens,
        "filter_error": view.query_error.as_ref().map(query_error_json),
    });
    world.insert_resource(view);
    Ok(reply)
}

/// A query error as JSON: `{ code, column, message }`.
#[must_use]
pub fn query_error_json(error: &crate::outliner_query::QueryError) -> serde_json::Value {
    serde_json::json!({ "code": error.code, "column": error.column, "message": error.message })
}

/// An entity parameter that must be present.
fn entity_param(params: &serde_json::Value, name: &str) -> Result<Entity, regia_agent::protocol::RpcError> {
    params
        .get(name)
        .and_then(serde_json::Value::as_u64)
        .and_then(crate::command::entity_from_bits)
        .ok_or_else(|| regia_agent::protocol::RpcError::wrong_type(name, "an entity id"))
}

/// An optional boolean parameter.
fn bool_param(
    params: &serde_json::Value,
    name: &str,
    default: bool,
) -> Result<bool, regia_agent::protocol::RpcError> {
    match params.get(name) {
        None | Some(serde_json::Value::Null) => Ok(default),
        Some(value) => value
            .as_bool()
            .ok_or_else(|| regia_agent::protocol::RpcError::wrong_type(name, "a boolean")),
    }
}

/// An `F0002`/`F0003` error with a lowercase code in `data.outliner_error`.
fn outliner_error(found: bool, code: &str, message: String) -> regia_agent::protocol::RpcError {
    use regia_agent::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};
    let mut error = if found {
        RpcError::new(INVALID_PARAMS, "F0002", message, "see book/agents/outliner.md")
    } else {
        RpcError::new(NOT_FOUND, "F0003", message, "take ids from editor.hierarchy")
    };
    if let Some(data) = error.data.as_mut() {
        data["outliner_error"] = serde_json::json!(code);
    }
    error
}

/// `editor.outliner_rename`: the in-place rename's commit.
fn outliner_rename_method(
    world: &mut World,
    params: &serde_json::Value,
) -> Result<serde_json::Value, regia_agent::protocol::RpcError> {
    let bits = params.get("entity").and_then(serde_json::Value::as_u64);
    let entity = entity_param(params, "entity")?;
    let entity = bits
        .and_then(|bits| crate::command::resolve_entity(world, bits))
        .unwrap_or(entity);
    let name = params
        .get("name")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| regia_agent::protocol::RpcError::wrong_type("name", "a string"))?;
    if world.get_entity(entity).is_err() {
        return Err(outliner_error(
            false,
            "rename_entity_missing",
            format!("entity {} is not in the level", entity.to_bits()),
        ));
    }
    rename_entity(world, entity, name).map_err(|(code, message)| outliner_error(true, code, message))?;
    Ok(serde_json::json!({
        "entity": entity.to_bits(),
        "name": display_name(world, entity),
        "undoable": world.get_resource::<UndoStack>().map_or(0, UndoStack::undo_depth),
    }))
}

/// `editor.outliner_expand`: Expand all / Collapse all, or one row (recursively).
fn outliner_expand_method(
    world: &mut World,
    params: &serde_json::Value,
) -> Result<serde_json::Value, regia_agent::protocol::RpcError> {
    let all = bool_param(params, "all", false)?;
    let recursive = bool_param(params, "recursive", false)?;
    let open = bool_param(params, "open", true)?;
    let entity = match params.get("entity") {
        None | Some(serde_json::Value::Null) => None,
        Some(_) => Some(entity_param(params, "entity")?),
    };
    let mut view = world.remove_resource::<OutlinerView>().unwrap_or_default();
    let (filter, grouped) = view
        .view_key
        .as_ref()
        .map_or((String::new(), false), |key| (key.0.clone(), key.1));
    view.update(world, &filter, grouped);
    let folder = match params.get("folder") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(path)) => Some(path.clone()),
        Some(_) => {
            world.insert_resource(view);
            return Err(regia_agent::protocol::RpcError::wrong_type(
                "folder",
                "a folder path string",
            ));
        }
    };
    let outcome = match (all, entity, folder) {
        (true, _, _) => {
            view.set_all_open(open);
            Ok(())
        }
        (false, Some(entity), _) => {
            if recursive {
                if view.set_open_recursive(entity, open) {
                    Ok(())
                } else {
                    Err(outliner_error(
                        false,
                        "expand_entity_missing",
                        format!("entity {} is not in the Outliner", entity.to_bits()),
                    ))
                }
            } else if find_in(&view.tree, entity) {
                view.set_open(entity, open);
                Ok(())
            } else {
                Err(outliner_error(
                    false,
                    "expand_entity_missing",
                    format!("entity {} is not in the Outliner", entity.to_bits()),
                ))
            }
        }
        (false, None, Some(path)) => {
            if view.folders.contains(&path) {
                let inside: Vec<String> = view
                    .folders
                    .iter()
                    .filter(|p| *p == &path || (recursive && crate::outliner_folders::is_within(p, &path)))
                    .cloned()
                    .collect();
                for member in inside {
                    view.set_folder_open(&member, open);
                }
                Ok(())
            } else {
                Err(outliner_error(
                    false,
                    "expand_folder_missing",
                    format!("no folder {path:?} is listed"),
                ))
            }
        }
        (false, None, None) => Err(outliner_error(
            true,
            "expand_no_target",
            "pass `entity`, `folder`, or `all: true`".to_string(),
        )),
    };
    if outcome.is_ok() {
        view.update(world, &filter, grouped);
    }
    let reply = serde_json::json!({ "listed": view.rows.len(), "open": open });
    world.insert_resource(view);
    outcome.map(|()| reply)
}

/// Whether `entity` is anywhere in `nodes`.
fn find_in(nodes: &[HierarchyNode], entity: Entity) -> bool {
    nodes
        .iter()
        .any(|node| node.entity == entity || find_in(&node.children, entity))
}

/// `editor.paste`: paste the clipboard, optionally as children of `into` and at the original
/// world position.
fn paste_method(
    world: &mut World,
    params: &serde_json::Value,
) -> Result<serde_json::Value, regia_agent::protocol::RpcError> {
    let into = match params.get("into") {
        None | Some(serde_json::Value::Null) => None,
        Some(_) => {
            let bits = params.get("into").and_then(serde_json::Value::as_u64);
            let entity = entity_param(params, "into")?;
            Some(
                bits.and_then(|bits| crate::command::resolve_entity(world, bits))
                    .unwrap_or(entity),
            )
        }
    };
    let at_original = bool_param(params, "at_original", false)?;
    let pasted =
        crate::viewport_ops::paste_with(world, crate::viewport_ops::PasteOptions { into, at_original })
            .map_err(|err| match err {
                HierarchyError::NoEntities => outliner_error(
                    true,
                    "paste_clipboard_empty",
                    "the clipboard is empty".to_string(),
                ),
                other => crate::hierarchy_agent::hierarchy_rpc_error(&other),
            })?;
    Ok(serde_json::json!({
        "pasted": pasted.iter().map(|e| e.to_bits()).collect::<Vec<_>>(),
        "parent": into.map(Entity::to_bits),
        "undoable": world.get_resource::<UndoStack>().map_or(0, UndoStack::undo_depth),
    }))
}

/// The Outliner's view surface (`editor.outliner_rows`, ADR-7306), its in-place rename and
/// expand state, Paste as child, and the folder methods (ADR-7757).
#[must_use]
pub fn methods() -> Vec<(regia_agent::schema::MethodSchema, crate::viewport_agent::Handler)> {
    use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
    let mut methods = crate::outliner_folders::methods();
    methods.extend(vec![
        (
            MethodSchema {
                name: "editor.outliner_rename",
                tool: "outliner_rename",
                description: "Rename an entity, as the Outliner's in-place rename commits it (F2 or a click on the \
                    selected row): one undo step.",
                params: &[
                    ParamSchema {
                        name: "entity",
                        kind: "integer",
                        required: true,
                        description: "The entity id.",
                    },
                    ParamSchema {
                        name: "name",
                        kind: "string",
                        required: true,
                        description: "The new name (not blank).",
                    },
                ],
                result: "{ entity, name, undoable }",
                mutating: true,
                permission: Permission::EditScene,
            },
            outliner_rename_method as crate::viewport_agent::Handler,
        ),
        (
            MethodSchema {
                name: "editor.outliner_expand",
                tool: "outliner_expand",
                description: "Expand or collapse Outliner rows: every row and folder (all), or one entity's row, \
                    with its whole branch when recursive. View state only: edits nothing in the level.",
                params: &[
                    ParamSchema {
                        name: "entity",
                        kind: "integer",
                        required: false,
                        description: "The row to open or close (needed unless all is true).",
                    },
                    ParamSchema {
                        name: "folder",
                        kind: "string",
                        required: false,
                        description: "A folder path to open or close instead of an entity row.",
                    },
                    ParamSchema {
                        name: "all",
                        kind: "boolean",
                        required: false,
                        description: "Expand all / Collapse all (default false).",
                    },
                    ParamSchema {
                        name: "recursive",
                        kind: "boolean",
                        required: false,
                        description: "Also every row below the entity (default false).",
                    },
                    ParamSchema {
                        name: "open",
                        kind: "boolean",
                        required: false,
                        description: "true (default) expands, false collapses.",
                    },
                ],
                result: "{ listed, open }",
                mutating: false,
                permission: Permission::Read,
            },
            outliner_expand_method as crate::viewport_agent::Handler,
        ),
        (
            MethodSchema {
                name: "editor.paste",
                tool: "paste",
                description: "Paste the editor clipboard (Edit > Copy or Cut) with fresh ids as one undo step: at the \
                    level's top level, or as children of `into` (Edit > Paste as child); with at_original each \
                    pasted root keeps the world position it had when copied (Edit > Paste at original position). \
                    The pasted roots become the selection.",
                params: &[
                    ParamSchema {
                        name: "into",
                        kind: "integer",
                        required: false,
                        description: "The new parent's entity id; absent or null = the top level.",
                    },
                    ParamSchema {
                        name: "at_original",
                        kind: "boolean",
                        required: false,
                        description: "Keep each root's world position from copy time (default false).",
                    },
                ],
                result: "{ pasted, parent, undoable }",
                mutating: true,
                permission: Permission::EditScene,
            },
            paste_method as crate::viewport_agent::Handler,
        ),
    ]);
    methods.push((
        MethodSchema {
            name: "editor.outliner_rows",
            tool: "outliner_rows",
            description: "The Outliner's listed rows, as the panel lists them (the search and grouping of its last \
                draw, closed rows' children left out), a window at a time: open or close rows, or reveal an \
                entity (open its parents; the panel scrolls to it on its next draw). Category headings of the \
                grouped view are rows too; while several levels are open, each open level's (or layer file's) \
                heading is a row (`level`: its document id, `active`) and its roots are listed under it \
                (`open_levels` / `close_levels` open or close a group). Edits nothing in the level.",
            params: &[
                ParamSchema {
                    name: "open",
                    kind: "array",
                    required: false,
                    description: "Entity ids whose rows to open (list their children).",
                },
                ParamSchema {
                    name: "close",
                    kind: "array",
                    required: false,
                    description: "Entity ids whose rows to close.",
                },
                ParamSchema {
                    name: "reveal",
                    kind: "integer",
                    required: false,
                    description: "An entity id: open its parents and scroll the panel to its row.",
                },
                ParamSchema {
                    name: "offset",
                    kind: "integer",
                    required: false,
                    description: "First row to return (default 0).",
                },
                ParamSchema {
                    name: "limit",
                    kind: "integer",
                    required: false,
                    description: "Rows to return (default 100, at most 1000).",
                },
                ParamSchema {
                    name: "open_levels",
                    kind: "array",
                    required: false,
                    description: "Document ids (`editor.levels_open` ids) of level groups to open.",
                },
                ParamSchema {
                    name: "close_levels",
                    kind: "array",
                    required: false,
                    description: "Document ids of level groups to close (their roots are not listed).",
                },
            ],
            result: "{ listed, filter, grouped, offset, rows: [{ entity, label, depth, has_children, open } | \
                { category, count, open } | { folder, label, depth, count, open }], revealed, reveal_index, \
                tree_builds, flattens, filter_error: { code, column, message } | null }",
            mutating: false,
            permission: Permission::Read,
        },
        outliner_rows_method as crate::viewport_agent::Handler,
    ));
    methods
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_scene::prelude::EntityGuid;

    fn world_with_hierarchy() -> World {
        let mut world = World::new();
        let root = world
            .spawn((Name::new("Root"), Transform::default(), EntityGuid::new_random()))
            .id();
        let child = world
            .spawn((Name::new("Child"), Transform::default(), EntityGuid::new_random()))
            .id();
        world.entity_mut(child).insert(ChildOf(root));
        let _ = world
            .spawn((Name::new("Other"), Transform::default(), EntityGuid::new_random()))
            .id();
        // Content-less entities (ECS bookkeeping) join the hidden set, as in the editor.
        crate::visibility::mark_internal_entities(&mut world);
        world
    }

    fn scene_app() -> regia_app::App {
        let mut app = regia_app::App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        app
    }

    #[test]
    fn create_child_parents_the_new_entity() {
        let mut app = scene_app();
        let parent = app
            .world_mut()
            .spawn((
                Name::new("Holder"),
                Transform::default(),
                EntityGuid::new_random(),
            ))
            .id();
        create_child(app.world_mut(), parent);
        let child = app
            .world_mut()
            .query::<(Entity, &Name)>()
            .iter(app.world())
            .find(|(_, n)| n.as_str() == "New entity")
            .map(|(e, _)| e)
            .expect("child spawned");
        assert_eq!(
            app.world().get::<ChildOf>(child).map(ChildOf::parent),
            Some(parent)
        );
        assert_eq!(
            app.world().resource::<UndoStack>().undo_depth(),
            1,
            "spawn + reparent, one entry"
        );
    }

    #[test]
    fn duplicate_of_a_child_stays_a_sibling() {
        let mut app = scene_app();
        let parent = app
            .world_mut()
            .spawn((
                Name::new("Holder"),
                Transform::from_translation(Vec3::new(10.0, 0.0, 0.0)),
                EntityGuid::new_random(),
            ))
            .id();
        let child = app
            .world_mut()
            .spawn((Name::new("Kid"), Transform::default(), EntityGuid::new_random()))
            .id();
        app.world_mut().entity_mut(child).insert(ChildOf(parent));
        let copies = hierarchy_ops::duplicate_entities(app.world_mut(), &[child], Vec3::X).expect("copy");
        let copy = copies[0];
        assert_ne!(copy, child);
        assert_eq!(
            app.world().get::<ChildOf>(copy).map(ChildOf::parent),
            Some(parent)
        );
        let local = app.world().get::<Transform>(copy).expect("transform").translation;
        assert_eq!(
            local,
            Vec3::new(1.0, 0.0, 0.0),
            "one metre beside the original, in the parent's space"
        );
    }

    #[test]
    fn create_child_and_duplicate_are_one_undo_step_each() {
        let mut app = regia_app::App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let world = app.world_mut();
        let parent = world
            .spawn((
                Name::new("Parent"),
                Transform::default(),
                EntityGuid::new_random(),
            ))
            .id();
        let child = create_child(world, parent).expect("child");
        assert_eq!(world.get::<ChildOf>(child).map(ChildOf::parent), Some(parent));
        assert_eq!(
            world.resource::<UndoStack>().undo_depth(),
            1,
            "spawn + reparent = one entry"
        );
        let child_guid = world.get::<EntityGuid>(child).expect("guid").0;

        let copies = hierarchy_ops::duplicate_entities(world, &[child], Vec3::X).expect("copy");
        let copy = copies[0];
        assert_ne!(copy, child);
        assert_eq!(
            world.get::<ChildOf>(copy).map(ChildOf::parent),
            Some(parent),
            "a duplicated child stays a sibling of the original"
        );
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 2);

        assert!(crate::command::undo_world(world));
        assert!(world.get_entity(copy).is_err(), "one undo removed the copy");
        assert!(crate::command::undo_world(world));
        assert!(
            hierarchy_ops::guid_entity(world, child_guid).is_none(),
            "one undo removed the child"
        );
        assert!(crate::command::redo_world(world));
        let child = hierarchy_ops::guid_entity(world, child_guid).expect("redone");
        assert_eq!(
            world.get::<ChildOf>(child).map(ChildOf::parent),
            Some(parent),
            "the redone child is parented again"
        );
    }

    #[test]
    fn filter_keeps_matches_and_their_ancestors() {
        let world = world_with_hierarchy();
        let tree = hierarchy_tree(&world);
        fn labels(nodes: &[HierarchyNode]) -> Vec<String> {
            nodes.iter().map(|n| n.label.clone()).collect()
        }
        assert_eq!(
            labels(&filter_tree(&tree, "")),
            ["Other", "Root"],
            "empty keeps all"
        );
        assert_eq!(
            labels(&filter_tree(&tree, "  ")),
            ["Other", "Root"],
            "blank keeps all"
        );
        let child = filter_tree(&tree, "CHI");
        assert_eq!(labels(&child), ["Root"], "the ancestor of a match stays");
        assert_eq!(labels(&child[0].children), ["Child"]);
        let root = filter_tree(&tree, "root");
        assert!(
            root[0].children.is_empty(),
            "a match keeps only matching children"
        );
        assert!(filter_tree(&tree, "zzz").is_empty());
    }

    #[test]
    fn range_between_walks_draw_order() {
        let world = world_with_hierarchy();
        let order: Vec<Entity> = ["Other", "Root", "Child"]
            .iter()
            .map(|n| entity(&world, n))
            .collect();
        assert_eq!(range_between(&order, Some(order[2]), order[0]), order);
        assert_eq!(range_between(&order, Some(order[1]), order[1]), vec![order[1]]);
        assert_eq!(
            range_between(&order, None, order[1]),
            vec![order[1]],
            "no anchor: the row"
        );
        let hidden = entity(&world, "Child");
        assert_eq!(
            range_between(&order[..2], Some(hidden), order[0]),
            vec![order[0]],
            "an anchor that is not drawn: the row"
        );
    }

    #[test]
    fn tree_has_roots_and_children() {
        let world = world_with_hierarchy();
        let tree = hierarchy_tree(&world);
        let labels: Vec<&str> = tree.iter().map(|n| n.label.as_str()).collect();
        assert_eq!(labels, ["Other", "Root"], "roots sorted by name");
        let root = tree.iter().find(|n| n.label == "Root").expect("root");
        assert_eq!(root.children.len(), 1);
        assert_eq!(root.children[0].label, "Child");
    }

    #[test]
    fn lists_every_user_entity_and_hides_internal_ones() {
        let mut world = world_with_hierarchy();
        // An agent `scene.spawn` entity: ECS Name + Transform, no EntityGuid — it used to be
        // invisible to the outliner while the inspector showed it.
        let agent_spawned = world
            .spawn((
                regia_ecs::ecs::name::Name::new("Agent spawn"),
                Transform::default(),
            ))
            .id();
        // Editor-internal bookkeeping (e.g. the window marker).
        let window = world.spawn(regia_platform::PrimaryWindow).id();
        crate::visibility::mark_internal_entities(&mut world);

        let tree = hierarchy_tree(&world);
        let labels: Vec<&str> = tree.iter().map(|n| n.label.as_str()).collect();
        assert!(
            labels.contains(&"Agent spawn"),
            "agent-spawned entities are listed: {labels:?}"
        );
        assert_eq!(tree.iter().filter(|n| n.entity == window).count(), 0);
        assert!(
            tree.iter().any(|n| n.entity == agent_spawned),
            "the agent entity is in the tree"
        );
    }

    #[test]
    fn cycle_detection_rejects_ancestry() {
        let world = world_with_hierarchy();
        let root = entity(&world, "Root");
        let child = entity(&world, "Child");
        assert!(
            reparent_creates_cycle(&world, root, child),
            "child under own descendant"
        );
        assert!(
            !reparent_creates_cycle(&world, child, root),
            "child under parent is fine"
        );
        assert!(reparent_creates_cycle(&world, root, root), "self-parenting");
    }

    #[test]
    fn cycle_detection_sees_the_top_of_a_chain_deeper_than_the_walk_limit() {
        // A legitimate chain longer than `MAX_DEPTH`: the old step limit gave up before reaching
        // the top and answered "no cycle", so parenting the top under the leaf closed a loop.
        let mut world = World::new();
        let top = world.spawn_empty().id();
        let mut leaf = top;
        for _ in 0..crate::hierarchy_ops::MAX_DEPTH + 2 {
            leaf = world.spawn(ChildOf(leaf)).id();
        }
        assert!(
            reparent_creates_cycle(&world, top, leaf),
            "top under its own deep leaf"
        );
        assert!(
            !reparent_creates_cycle(&world, leaf, top),
            "leaf under the top is fine"
        );
        // A malformed loop above the target that does not hold the moved entity still ends.
        let a = world.spawn_empty().id();
        let b = world.spawn(ChildOf(a)).id();
        world.entity_mut(a).insert(ChildOf(b));
        let loose = world.spawn_empty().id();
        assert!(!reparent_creates_cycle(&world, loose, b));
    }

    #[test]
    fn subtree_includes_descendants() {
        let world = world_with_hierarchy();
        let root = entity(&world, "Root");
        let child = entity(&world, "Child");
        let other = entity(&world, "Other");
        let subtree = subtree_of(&world, root);
        assert!(subtree.contains(&root) && subtree.contains(&child));
        assert!(!subtree.contains(&other));
    }

    fn entity(world: &World, name: &str) -> Entity {
        for entity_ref in world.iter_entities() {
            let entity = entity_ref.id();
            if let Some(found) = world.get::<Name>(entity)
                && found.as_str() == name
            {
                return entity;
            }
        }
        panic!("entity {name:?} not found");
    }
    #[test]
    fn equal_labels_order_by_entity_id() {
        let mut world = World::new();
        let a = world.spawn((Name::new("Prop"), Transform::default())).id();
        let b = world.spawn((Name::new("Prop"), Transform::default())).id();
        let c = world.spawn((Name::new("Prop"), Transform::default())).id();
        // Moving `a` to another archetype changes query order, not the listed order.
        world.entity_mut(a).insert(EntityGuid::new_random());
        let ids: Vec<Entity> = hierarchy_tree(&world).iter().map(|n| n.entity).collect();
        // `Entity`'s own order, which is not spawn order in bevy_ecs 0.20 (ADR-1389).
        let mut expected = [a, b, c];
        expected.sort();
        assert_eq!(ids, expected);
    }

    /// A code kind for the row-icon tests.
    #[derive(Component, Reflect, Default)]
    #[reflect(Component, Default)]
    struct Lamp;

    regia_scene::entity_kind!(
        Lamp,
        "Lamp",
        icon = "light",
        color = [0xff, 0xc8, 0x00],
        description = "A lamp post",
        category = "Props"
    );

    #[test]
    fn rows_show_kinds_roles_and_the_unclassified_marker() {
        use regia_scene::kind::KindAppExt as _;
        use regia_scene::kind::roles::Role;
        let mut app = regia_app::App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        app.register_kind::<Lamp>();
        let world = app.world_mut();
        world.spawn((Name::new("Lamp A"), Lamp, regia_render::PointLight::default()));
        world.spawn((
            Name::new("Odd"),
            Transform::default(),
            regia_render::PointLight::default(),
            regia_audio::AudioEmitter::default(),
        ));
        world.spawn((Name::new("Sun"), regia_render::DirectionalLight::default()));
        world.spawn((Name::new("Group"), Transform::default()));
        let tree = hierarchy_tree(app.world());
        let icon = |name: &str| tree.iter().find(|n| n.label == name).expect(name).icon.clone();

        let lamp = icon("Lamp A");
        assert_eq!(lamp.kind.as_deref(), Some("Lamp"));
        assert_eq!(lamp.color, Some([0xff, 0xc8, 0x00]));
        assert_eq!(lamp.glyph, crate::kind_icons::glyph("light"));
        assert!(
            lamp.tooltip.starts_with("kind: Lamp\nA lamp post"),
            "{}",
            lamp.tooltip
        );
        assert!(!lamp.unclassified);

        let odd = icon("Odd");
        assert_eq!(odd.kind, None);
        assert_eq!(odd.role, Some(Role::Point), "the first role by priority");
        assert!(odd.unclassified, "a light and an audio emitter: two families");
        assert!(odd.tooltip.contains("consider a child"));

        let sun = icon("Sun");
        assert_eq!(sun.role, Some(Role::Sun));
        assert!(!sun.unclassified);
        assert_eq!(sun.glyph, crate::kind_icons::glyph("sun"));

        let group = icon("Group");
        assert_eq!(group.glyph, "", "a group node has no icon");

        let names = |nodes: &[HierarchyNode]| nodes.iter().map(|n| n.label.clone()).collect::<Vec<_>>();
        assert_eq!(names(&filter_tree(&tree, "kind:LAMP")), ["Lamp A"]);
        assert_eq!(names(&filter_tree(&tree, "kind:?")), ["Odd"]);
        assert_eq!(names(&filter_tree(&tree, "kind:none su")), ["Sun"]);
        let groups: Vec<(String, Vec<String>)> = group_by_category(&tree)
            .into_iter()
            .map(|(c, rows)| (c, names(&rows)))
            .collect();
        assert_eq!(
            groups,
            [
                ("Props".to_string(), vec!["Lamp A".to_string()]),
                (
                    crate::kind_icons::NO_KIND.to_string(),
                    vec!["Group".to_string(), "Odd".to_string(), "Sun".to_string()]
                ),
            ]
        );
        // The viewport's gizmo rule is unchanged: a kinded lamp is still a point-light icon.
        let lamp_entity = tree.iter().find(|n| n.label == "Lamp A").expect("lamp").entity;
        assert_eq!(
            crate::overlays::icon_of(app.world(), lamp_entity),
            Some(crate::overlays::IconKind::Point)
        );
    }

    /// `type:<Component>` and `tag:<name>` filter terms (spec 19 § 2): a term keeps the entities
    /// that have the component (short name, case-insensitive) or the tag, plus the ancestors
    /// that lead to them.
    #[test]
    fn filter_tree_keeps_component_types_and_tags() {
        use regia_scene::components::Tags;
        let mut app = regia_app::App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let world = app.world_mut();
        let props = world.spawn(Name::new("Props")).id();
        world.spawn((
            Name::new("Crate A"),
            regia_render::MeshSource::new("assets/props/a.glb", ""),
            ChildOf(props),
        ));
        world.spawn((
            Name::new("Crate B"),
            regia_render::MeshSource::new("assets/props/b.glb", ""),
            ChildOf(props),
        ));
        world.spawn((
            Name::new("Siren"),
            regia_audio::AudioEmitter::default(),
            Tags::from_iter_str(["loot", "loud"]),
            ChildOf(props),
        ));
        world.spawn((Name::new("Bare"), Transform::default()));
        let tree = hierarchy_tree(app.world());
        let names = |nodes: &[HierarchyNode]| {
            fn walk(node: &HierarchyNode, out: &mut Vec<String>) {
                out.push(node.label.clone());
                for child in &node.children {
                    walk(child, out);
                }
            }
            let mut found = Vec::new();
            for node in nodes {
                walk(node, &mut found);
            }
            found.sort();
            found
        };
        assert_eq!(
            names(&filter_tree(&tree, "type:MeshSource")),
            ["Crate A", "Crate B", "Props"],
            "both meshes and the parent that leads to them"
        );
        assert_eq!(
            names(&filter_tree(&tree, "type:AudioEmitter tag:loot")),
            ["Props", "Siren"],
            "the terms combine (AND)"
        );
        assert_eq!(names(&filter_tree(&tree, "tag:loud")), ["Props", "Siren"]);
        assert_eq!(
            names(&filter_tree(&tree, "tag:absent")),
            Vec::<String>::new(),
            "no entity carries the tag"
        );
        assert_eq!(
            names(&filter_tree(&tree, "type:MeshSource type:AudioEmitter")),
            Vec::<String>::new(),
            "no entity has both components"
        );
        assert_eq!(
            names(&filter_tree(&tree, "")),
            ["Bare", "Crate A", "Crate B", "Props", "Siren"]
        );
    }

    /// Spec 101 § 4.2: Group by category lists kind categories in name order, then `Uncategorized`
    /// (kinds without a category and undeclared tags), then `No kind`; children are flat entries.
    #[test]
    fn group_by_category_orders_named_then_uncategorized_then_no_kind() {
        use regia_scene::kind::{KindInfo, KindRegistry, KindSource, KindTag, UNCATEGORIZED};
        let data = |name: &str, category: Option<&str>| KindInfo {
            name: name.into(),
            icon: "box".into(),
            color: [1, 2, 3],
            description: String::new(),
            category: category.map(str::to_string),
            required: Vec::new(),
            source: KindSource::Data {
                path: format!("assets/kinds/{name}.kind.ron"),
                marker: None,
            },
        };
        let mut app = regia_app::App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let world = app.world_mut();
        {
            let mut registry = world.resource_mut::<KindRegistry>();
            registry.register_data(data("Zombie", Some("Props")));
            registry.register_data(data("Archer", Some("AI")));
            registry.register_data(data("Rock", None));
        }
        let parent = world
            .spawn((Name::new("Zombie 1"), KindTag("Zombie".into())))
            .id();
        world.spawn((Name::new("Archer 1"), KindTag("Archer".into()), ChildOf(parent)));
        world.spawn((Name::new("Rock 1"), KindTag("Rock".into())));
        world.spawn((Name::new("Ghost 1"), KindTag("Ghost".into())));
        world.spawn((Name::new("Plain"), Transform::default()));
        let tree = hierarchy_tree(app.world());
        let groups: Vec<(String, Vec<String>)> = group_by_category(&tree)
            .into_iter()
            .map(|(c, rows)| {
                assert!(rows.iter().all(|r| r.children.is_empty()), "flat entries in {c}");
                (c, rows.iter().map(|n| n.label.clone()).collect())
            })
            .collect();
        assert_eq!(
            groups,
            [
                ("AI".to_string(), vec!["Archer 1".to_string()]),
                ("Props".to_string(), vec!["Zombie 1".to_string()]),
                (
                    UNCATEGORIZED.to_string(),
                    vec!["Ghost 1".to_string(), "Rock 1".to_string()]
                ),
                (crate::kind_icons::NO_KIND.to_string(), vec!["Plain".to_string()]),
            ]
        );
    }

    #[test]
    fn flat_rows_follow_open_state_search_and_groups() {
        let world = world_with_hierarchy();
        let tree = hierarchy_tree(&world);
        let root = entity(&world, "Root");
        let child = entity(&world, "Child");
        let other = entity(&world, "Other");
        let ids = |rows: &[FlatRow]| rows.iter().map(|r| r.entity()).collect::<Vec<_>>();
        let open = flatten_tree(&tree, &HashSet::new(), false);
        assert_eq!(ids(&open), [Some(other), Some(root), Some(child)]);
        assert!(matches!(
            open[2],
            FlatRow::Entity {
                depth: 1,
                has_children: false,
                ..
            }
        ));
        let closed = flatten_tree(&tree, &HashSet::from([root]), false);
        assert_eq!(
            ids(&closed),
            [Some(other), Some(root)],
            "a closed row hides its children"
        );
        assert!(matches!(
            closed[1],
            FlatRow::Entity {
                open: false,
                has_children: true,
                ..
            }
        ));
        let searching = flatten_tree(&filter_tree(&tree, "chi"), &HashSet::from([root]), true);
        assert_eq!(
            ids(&searching),
            [Some(root), Some(child)],
            "a search opens every kept row"
        );
        let groups = group_by_category(&tree);
        let grouped = flatten_groups(&groups, &HashSet::new(), &HashSet::new(), false);
        assert!(matches!(
            &grouped[0],
            FlatRow::Category {
                count: 3,
                open: true,
                ..
            }
        ));
        assert_eq!(grouped.len(), 4);
        let name = match &grouped[0] {
            FlatRow::Category { name, .. } => name.clone(),
            FlatRow::Entity { .. } | FlatRow::Folder { .. } | FlatRow::Level { .. } => String::new(),
        };
        let shut = flatten_groups(&groups, &HashSet::from([name]), &HashSet::new(), false);
        assert_eq!(shut.len(), 1, "a closed section lists only its heading");
    }

    #[test]
    fn arrow_targets_and_scroll_offsets() {
        let world = world_with_hierarchy();
        let rows = flatten_tree(&hierarchy_tree(&world), &HashSet::new(), false);
        let (other, root, child) = (
            entity(&world, "Other"),
            entity(&world, "Root"),
            entity(&world, "Child"),
        );
        assert_eq!(arrow_target(&rows, None, true), Some((0, other)));
        assert_eq!(arrow_target(&rows, None, false), Some((2, child)));
        assert_eq!(arrow_target(&rows, Some(other), true), Some((1, root)));
        assert_eq!(arrow_target(&rows, Some(child), true), None, "the last row stays");
        assert_eq!(arrow_target(&rows, Some(root), false), Some((0, other)));

        let view = OutlinerView {
            viewport: (100.0, 200.0),
            ..Default::default()
        };
        assert_eq!(view.offset_for(6, false, 20.0), None, "rows 5..=14 are in view");
        assert_eq!(view.offset_for(2, false, 20.0), Some(40.0), "above: its top");
        assert_eq!(view.offset_for(20, false, 20.0), Some(220.0), "below: its bottom");
        assert_eq!(view.offset_for(20, true, 20.0), Some(310.0), "centred");
    }

    /// w11 item 2 (sprint-w9-rev-v06-lanes § 2 finding 2): at 100 000 entities the Outliner
    /// computes no `type:`/`tag:` terms while no search uses them, and once one does, each
    /// entity's terms once per tree generation: repeated frames and other `type:`/`tag:` searches
    /// reuse them; a rename (a new generation) computes them once more. Counted, no wall clock.
    #[test]
    fn type_and_tag_terms_are_computed_once_per_entity_per_tree_generation() {
        use regia_scene::components::Tags;
        const N: u64 = 100_000;
        let mut world = World::new();
        let mut first = None;
        for i in 0..N {
            let mut e = world.spawn(Name::new(format!("e{i}")));
            if i % 10 == 0 {
                e.insert(Tags::from_iter_str(["loot"]));
            }
            first.get_or_insert(e.id());
        }
        let mut view = OutlinerView::default();
        for _ in 0..3 {
            view.update(&mut world, "", false);
        }
        assert_eq!(view.tree_builds(), 1);
        assert_eq!(view.term_evaluations(), 0, "no type:/tag: search: no terms");
        for _ in 0..3 {
            view.update(&mut world, "type:name", false);
        }
        assert_eq!(view.rows().len() as u64, N, "every entity has a Name");
        assert_eq!(view.term_evaluations(), N, "once per entity");
        view.update(&mut world, "tag:loot", false);
        view.update(&mut world, "type:name tag:loot", false);
        view.update(&mut world, "tag:loot", false);
        assert_eq!(view.rows().len() as u64, N / 10, "the tagged tenth");
        assert_eq!(
            view.term_evaluations(),
            N,
            "other searches reuse the generation's terms"
        );
        // A rename is a new tree generation: the terms are computed once more, on demand.
        if let Some(mut name) = first.and_then(|e| world.get_mut::<Name>(e)) {
            *name = Name::new("renamed");
        }
        view.update(&mut world, "", false);
        assert_eq!(view.tree_builds(), 2);
        assert_eq!(view.term_evaluations(), N, "the plain search needs no terms");
        for _ in 0..3 {
            view.update(&mut world, "tag:loot", false);
        }
        assert_eq!(view.term_evaluations(), 2 * N);
        assert_eq!(view.rows().len() as u64, N / 10);
    }

    /// w10 (sprint-w9-rev-v06-lanes § 2 finding 1, the review's proof test): a tag added in place
    /// on an entity whose archetype does not change reaches the cached tree's `tag:` filter, and a
    /// removed tag leaves no ghost match.
    #[test]
    fn a_tag_edited_in_place_reaches_the_cached_tag_filter() {
        use regia_scene::components::Tags;
        let mut app = regia_app::App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let world = app.world_mut();
        let siren = world
            .spawn((Name::new("Siren"), Tags::from_iter_str(["loot"])))
            .id();
        let mut view = OutlinerView::default();
        // w11 item 2: the cached tree carries its tag terms once a `tag:` search ran on it (they
        // are computed on demand); the searches below are the ones the assertions check.
        view.update(world, "tag:boss", false);
        assert!(filter_tree(&view.tree, "tag:boss").is_empty(), "no boss tag yet");
        let builds = view.tree_builds();
        if let Some(mut tags) = world.get_mut::<Tags>(siren) {
            *tags = Tags::from_iter_str(["loot", "boss"]);
        }
        view.update(world, "tag:boss", false);
        assert_eq!(
            filter_tree(&view.tree, "tag:boss").len(),
            1,
            "the in-place tag edit shows in the tag: filter (tree builds {builds} -> {})",
            view.tree_builds()
        );
        if let Some(mut tags) = world.get_mut::<Tags>(siren) {
            *tags = Tags::from_iter_str(["boss"]);
        }
        view.update(world, "tag:loot", false);
        assert!(
            filter_tree(&view.tree, "tag:loot").is_empty(),
            "a removed tag leaves no ghost match"
        );
    }
}
