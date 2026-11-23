// SPDX-License-Identifier: MPL-2.0
//! Collection item templates (ADR-2489, `docs/specs/81-ui-designer.md` § 2.7): a node with
//! `repeat: (source: "InventoryVm.items", key: "id")` instantiates its `children` once per element
//! of a reflected `Vec`, bindings whose source starts with a dot (`.name`) read relative to the
//! element, and the instances follow the collection by keyed diff: an unchanged key keeps its
//! entities (and every piece of widget state on them), a new key spawns, a gone key despawns, a
//! moved key only reorders the host's children. The technique is keyed list reconciliation as
//! documented publicly for UI libraries (React documentation, "Reconciliation", "Keys").
//!
//! The refresh runs at the top of [`crate::binding::update_bindings`], not as a system of its own,
//! so the schedule does not change, and only after the first repeat node spawned ([`RepeatHosts`]
//! does not exist before that). A host does nothing on a frame where the model's change tick did
//! not move; only then is the list read, so the idle cost is one tick check per host and a changed
//! model costs one pass over its items and their item bindings (numbers in the spec, § 9).
//!
//! **Virtualized repeats (ADR-2715, spec § 2.8).** A repeat on a `list` widget materializes only
//! the items in the list's visible range plus its overscan, each inside a row entity that carries
//! [`crate::list::ListRow`]. An item that scrolls out gives its row to an item that scrolls in
//! (the row is re-keyed and its item bindings rewritten), so scrolling and a removal inside the
//! view spawn nothing. The first visible item's key anchors the scroll offset: when the
//! collection changes, the offset moves so that item keeps its place on screen.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use regia_app::prelude::*;
use regia_ecs::ecs::change_detection::{ComponentTicks, Tick};
use regia_ecs::reflect::{PartialReflect, ReflectRef, TypeRegistry};
use serde_json::{Value, json};

use crate::authoring::{RepeatDef, UiNodeDef, spawn_node};
use crate::binding::{Bind, BindSource, BindingChange, reflect_to_string, resource_field, write_changes};
use crate::list::{ListGeometry, ListHeights, ListRow, ListTiles, RepeatRows};
use crate::node::{Classes, ComputedNode, Interactable, NodeName, NodeStyle, UiNode};
use crate::scroll::{ItemCount, ListView, ScrollOffset};
use crate::viewmodel::{UiCommandName, ViewBindings};

/// Most instances one repeat node keeps. A longer collection is cut here and reports
/// `repeat_capped`, so a runaway collection cannot exhaust memory with entities.
pub const MAX_REPEAT_ITEMS: usize = 65_536;

/// The repeat nodes alive, parents before the repeats inside their templates. Absent until the
/// first one spawns, so a game without collections pays one resource lookup per frame.
#[derive(Resource, Debug, Default)]
pub struct RepeatHosts(Vec<Entity>);

/// A node generated for a collection item that needs to know its row: it has item bindings or a
/// command.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct RepeatBound {
    /// The repeat node.
    pub host: Entity,
    /// The instance's stable key.
    pub key: Arc<str>,
}

/// What the last pass of a host did (`ui.collection_state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RepeatStats {
    /// Passes run: one per frame the model changed.
    pub passes: u64,
    /// Instances spawned by the last pass.
    pub spawned: usize,
    /// Instances despawned by the last pass.
    pub despawned: usize,
    /// Whether the last pass reordered the instances.
    pub moved: bool,
    /// Existing instances whose widgets the last pass wrote.
    pub touched: usize,
    /// Rows of a virtualized repeat the last pass gave to another item instead of spawning one.
    pub recycled: usize,
}

/// The `ListRow.index` of a row that shows no item.
const NO_ITEM: usize = usize::MAX;

/// What a virtualized repeat (a repeat on a `list` widget) shows of its collection.
#[derive(Debug, Default)]
struct Window {
    /// Every key of the collection, in order; the instances are `keys[start..start + n]`.
    keys: Vec<Arc<str>>,
    /// Collection index of the first instance.
    start: usize,
    /// The first visible item and the scroll offset past its top (UI px).
    anchor: Option<(Arc<str>, f32)>,
    /// The view the last pass laid out; a frame with the same view and an unchanged model passes.
    view: Option<ViewKey>,
    /// Hidden rows kept for the next item that scrolls in: the range is shorter at the ends of the
    /// list (no overscan past the edge), and scrolling back must not spawn.
    spare: Vec<Instance>,
    /// Rows spawned over the host's lifetime.
    spawned_total: u64,
    /// Rows recycled over the host's lifetime.
    recycled_total: u64,
}

/// Everything the visible range of a list depends on besides the collection.
#[derive(Debug, Clone, Copy, PartialEq)]
struct ViewKey {
    rect: [f32; 4],
    scale: f32,
    offset: f32,
    item_height: f32,
    overscan: u32,
    heights: Option<Tick>,
    tiles: bool,
}

/// One generated copy of the template.
#[derive(Debug)]
struct Instance {
    key: Arc<str>,
    /// The template's top nodes as spawned for this item.
    roots: Vec<Entity>,
    /// The nodes with item bindings (tree order).
    bound: Vec<Entity>,
}

/// A node with a `repeat`: its template, its live instances, its last error.
#[derive(Component, Debug)]
pub struct RepeatHost {
    /// `"Model.field"`, or `".field"` relative to the enclosing repeat's item.
    source: String,
    /// The key path inside an item; `None` keys by position.
    key: Option<String>,
    template: Arc<[UiNodeDef]>,
    /// The enclosing instance, when this host sits in another repeat's template.
    scope: Option<(Entity, Arc<str>)>,
    /// Short name of the resource at the root of the source path.
    root: String,
    instances: Vec<Instance>,
    positions: HashMap<Arc<str>, u32>,
    /// World tick after the last pass; `None` before the first.
    last_seen: Option<Tick>,
    error: Option<String>,
    /// The last pass could not read the collection (`error` holds why); the host keeps its
    /// instances and waits for the model to change.
    failed: bool,
    stats: RepeatStats,
    /// `Some` on a `list` widget: only the visible items have rows.
    window: Option<Window>,
    /// Where a virtualized repeat's rows get their height (ADR-7751).
    row_height: crate::authoring::RepeatRowHeight,
}

impl RepeatHost {
    fn new(def: &RepeatDef, template: Arc<[UiNodeDef]>) -> Self {
        let root = def.source.split_once('.').map_or("", |(m, _)| m).to_owned();
        Self {
            source: def.source.clone(),
            key: def.key.clone(),
            template,
            scope: None,
            root,
            instances: Vec::new(),
            positions: HashMap::new(),
            last_seen: None,
            error: None,
            failed: false,
            stats: RepeatStats::default(),
            window: None,
            row_height: def.row_height,
        }
    }

    /// Where a virtualized repeat's rows get their height: `list` or `template` (ADR-7751).
    pub fn row_height_mode(&self) -> crate::authoring::RepeatRowHeight {
        self.row_height
    }

    /// Whether the host is a `list` widget that materializes only its visible items.
    pub fn is_virtual(&self) -> bool {
        self.window.is_some()
    }

    /// Items in the collection: the instance count, or for a virtualized repeat every item,
    /// materialized or not.
    pub fn item_count(&self) -> usize {
        self.window
            .as_ref()
            .map_or(self.instances.len(), |w| w.keys.len())
    }

    /// Collection index of the first materialized item (0 unless virtualized).
    pub fn first_index(&self) -> usize {
        self.window.as_ref().map_or(0, |w| w.start)
    }

    /// The key of the first visible item of a virtualized repeat: the anchor that keeps the scroll
    /// position when the collection changes.
    pub fn anchor_key(&self) -> Option<&str> {
        self.window.as_ref()?.anchor.as_ref().map(|(k, _)| &**k)
    }

    /// Rows a virtualized repeat spawned over its lifetime (an ordinary repeat: 0).
    pub fn spawned_total(&self) -> u64 {
        self.window.as_ref().map_or(0, |w| w.spawned_total)
    }

    /// Hidden rows a virtualized repeat keeps for items that scroll in.
    pub fn spare_rows(&self) -> usize {
        self.window.as_ref().map_or(0, |w| w.spare.len())
    }

    /// Rows a virtualized repeat gave to another item over its lifetime.
    pub fn recycled_total(&self) -> u64 {
        self.window.as_ref().map_or(0, |w| w.recycled_total)
    }

    /// The source path as authored.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// The key path as authored.
    pub fn key_path(&self) -> Option<&str> {
        self.key.as_deref()
    }

    /// Live instances (for a virtualized repeat, the materialized rows).
    pub fn instance_count(&self) -> usize {
        self.instances.len()
    }

    /// Each live instance's key, in collection order.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.instances.iter().map(|i| &*i.key)
    }

    /// The top nodes generated for the item at collection index `index` (empty when the item has
    /// no row). A virtualized repeat's instance has one top node: its row.
    pub fn roots(&self, index: usize) -> &[Entity] {
        index
            .checked_sub(self.first_index())
            .and_then(|i| self.instances.get(i))
            .map_or(&[][..], |i| i.roots.as_slice())
    }

    /// The instance's position in the collection.
    pub fn index_of(&self, key: &str) -> Option<usize> {
        self.positions.get(key).map(|&i| i as usize)
    }

    /// The last error (`repeat_*` code, a colon, a message); `None` when healthy.
    pub fn last_error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// What the last pass did.
    pub fn stats(&self) -> RepeatStats {
        self.stats
    }

    fn is_relative(&self) -> bool {
        self.source.starts_with('.')
    }
}

/// Called by `spawn_node`: turns the node into a repeat host (its `children` are the template and
/// are not spawned here).
pub(crate) fn make_host(world: &mut World, entity: Entity, def: &RepeatDef, template: &[UiNodeDef]) {
    let mut host = RepeatHost::new(def, template.to_vec().into());
    let mut node = world.entity_mut(entity);
    if node.contains::<ListView>() {
        host.window = Some(Window::default());
        // `row_height: template` (ADR-7751): rows take the template's fixed height.
        if def.row_height == crate::authoring::RepeatRowHeight::Template
            && let Some(height) = RepeatDef::template_row_height(template)
            && let Some(mut list) = node.get_mut::<ListView>()
        {
            list.item_height = height;
        }
        node.insert(RepeatRows);
        if !node.contains::<ScrollOffset>() {
            node.insert(ScrollOffset::default());
        }
    }
    node.insert(host);
    world
        .get_resource_or_insert_with(RepeatHosts::default)
        .0
        .push(entity);
}

/// How the old instances become the new list (a pure function of the two key lists).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct KeyedPlan {
    /// For each new position: the old position it keeps, `None` when it spawns.
    pub sources: Vec<Option<usize>>,
    /// Old positions that despawn, ascending.
    pub removed: Vec<usize>,
    /// Whether the children must be reordered: a kept item changed its relative order, or a new
    /// item lands before a kept one.
    pub reorder: bool,
}

/// Plans a keyed diff. `new` holds unique keys ([`unique_keys`]); so does `old`. O(old + new).
pub fn plan_keyed_diff(old: &[&str], new: &[&str]) -> KeyedPlan {
    let by_key: HashMap<&str, usize> = old.iter().enumerate().map(|(i, k)| (*k, i)).collect();
    let mut kept = vec![false; old.len()];
    let mut sources = Vec::with_capacity(new.len());
    let mut reorder = false;
    let mut last_kept = None;
    let mut new_before_kept = false;
    let mut seen_new = false;
    for key in new {
        let source = by_key.get(key).copied();
        if let Some(i) = source {
            kept[i] = true;
            reorder |= last_kept.is_some_and(|prev| prev > i);
            last_kept = Some(i);
            new_before_kept |= seen_new;
        } else {
            seen_new = true;
        }
        sources.push(source);
    }
    let removed = kept
        .iter()
        .enumerate()
        .filter_map(|(i, k)| (!k).then_some(i))
        .collect();
    KeyedPlan {
        sources,
        removed,
        reorder: reorder || new_before_kept,
    }
}

/// Makes repeated keys unique: the second `a` becomes `a#2`, and so on (skipping a text that is
/// already taken). Returns how many keys it changed (`repeat_duplicate_key`).
pub fn unique_keys(keys: &mut [String]) -> usize {
    let mut seen: HashSet<String> = HashSet::with_capacity(keys.len());
    let mut changed = 0;
    // Plain keys first claim their own text, so a real `a#2` keeps it over a generated one.
    let all: HashSet<String> = keys.iter().cloned().collect();
    for key in keys.iter_mut() {
        if seen.insert(key.clone()) {
            continue;
        }
        let mut n = 2usize;
        let unique = loop {
            let candidate = format!("{key}#{n}");
            if !all.contains(&candidate) && !seen.contains(&candidate) {
                break candidate;
            }
            n += 1;
        };
        seen.insert(unique.clone());
        *key = unique;
        changed += 1;
    }
    changed
}

/// A list or array read through reflection.
#[derive(Clone, Copy)]
enum ListRef<'a> {
    List(&'a dyn regia_ecs::reflect::list::List),
    Array(&'a dyn regia_ecs::reflect::array::Array),
}

impl<'a> ListRef<'a> {
    fn of(value: &'a dyn PartialReflect) -> Option<Self> {
        match value.reflect_ref() {
            ReflectRef::List(l) => Some(Self::List(l)),
            ReflectRef::Array(a) => Some(Self::Array(a)),
            _ => None,
        }
    }

    fn len(self) -> usize {
        match self {
            Self::List(l) => l.len(),
            Self::Array(a) => a.len(),
        }
    }

    fn get(self, index: usize) -> Option<&'a dyn PartialReflect> {
        match self {
            Self::List(l) => l.get(index),
            Self::Array(a) => a.get(index),
        }
    }
}

/// `value` at a path relative to it: empty for the value itself.
fn relative<'a>(value: &'a dyn PartialReflect, path: &str) -> Result<&'a dyn PartialReflect, String> {
    if path.is_empty() {
        Ok(value)
    } else {
        crate::binding::resolve_path(value, path)
    }
}

/// The collection a host repeats over. A relative source goes through the parent's item.
fn resolve_list<'w>(
    world: &'w World,
    registry: &TypeRegistry,
    host: &RepeatHost,
) -> Result<ListRef<'w>, String> {
    let value = if let Some(rel) = host.source.strip_prefix('.') {
        let item = enclosing_item(world, registry, host)?;
        relative(item, rel).map_err(|e| format!("repeat_unresolved: {e}"))?
    } else {
        let Some((container, path)) = host.source.split_once('.') else {
            return Err(format!(
                "repeat_unresolved: source {:?} is not `Model.field`",
                host.source
            ));
        };
        resource_field(world, registry, container, path).map_err(|e| format!("repeat_unresolved: {e}"))?
    };
    ListRef::of(value).ok_or_else(|| {
        format!(
            "repeat_not_a_list: {} is a {}, not a list",
            host.source,
            value.reflect_type_path()
        )
    })
}

/// The item of the instance enclosing a relative host.
fn enclosing_item<'w>(
    world: &'w World,
    registry: &TypeRegistry,
    host: &RepeatHost,
) -> Result<&'w dyn PartialReflect, String> {
    let Some((parent, key)) = &host.scope else {
        return Err(format!(
            "repeat_outside_template: source {:?} needs an enclosing repeat",
            host.source
        ));
    };
    let Some(parent_host) = world.get::<RepeatHost>(*parent) else {
        return Err("repeat_unresolved: the enclosing repeat is gone".to_owned());
    };
    let index = parent_host
        .index_of(key)
        .ok_or_else(|| "repeat_unresolved: the enclosing item is gone".to_owned())?;
    resolve_list(world, registry, parent_host)?
        .get(index)
        .ok_or_else(|| "repeat_unresolved: the enclosing item is past the end".to_owned())
}

/// The change ticks of the resource named `name`.
fn resource_ticks(world: &World, registry: &TypeRegistry, name: &str) -> Option<ComponentTicks> {
    crate::binding::registrations_named(registry, name).find_map(|registration| {
        let id = world.components().get_valid_id(registration.type_id())?;
        let entity = world.resource_entities().get(id)?;
        world.get_entity(entity).ok()?.get_change_ticks_by_id(id)
    })
}

/// Refreshes every repeat node (parents first). Called from `update_bindings`.
pub(crate) fn update_repeats(world: &mut World, registry: &TypeRegistry) {
    let _span = tracing::trace_span!("ui_repeat").entered();
    let mut gone = false;
    let mut i = 0;
    // By index: a pass spawns instances, and a repeat inside the template appends to the list.
    while let Some(&host) = world.get_resource::<RepeatHosts>().and_then(|h| h.0.get(i)) {
        i += 1;
        gone |= !refresh_host(world, registry, host);
    }
    if gone {
        let mut hosts = std::mem::take(&mut world.resource_mut::<RepeatHosts>().0);
        hosts.retain(|&e| world.get::<RepeatHost>(e).is_some());
        world.resource_mut::<RepeatHosts>().0 = hosts;
    }
    // A later write must carry a newer tick than the one the hosts recorded, even with no system
    // between this pass and the write.
    world.increment_change_tick();
}

/// One host's refresh; `false` when the host is gone.
fn refresh_host(world: &mut World, registry: &TypeRegistry, entity: Entity) -> bool {
    let Some(host) = world.get::<RepeatHost>(entity) else {
        return false;
    };
    let now = world.read_change_tick();
    let changed = match (host.last_seen, resource_ticks(world, registry, &host.root)) {
        (Some(seen), Some(ticks)) => ticks.is_changed(seen, now),
        _ => true,
    };
    if !changed && let Some(window) = &host.window {
        // A host in error keeps its rows as they are until the model changes.
        let moved = !host.failed && window.view != view_key(world, entity);
        if moved {
            // Scrolled or resized: the same keys, another window.
            reconcile_virtual(world, registry, entity, None, now);
        }
        return true;
    }
    if !changed {
        return true;
    }
    let is_virtual = host.window.is_some();
    match read_keys(world, registry, host) {
        Ok(read) if is_virtual => reconcile_virtual(world, registry, entity, Some(read), now),
        Ok((keys, cap_note)) => reconcile(world, registry, entity, keys, cap_note, now),
        Err(error) => {
            if let Some(mut host) = world.get_mut::<RepeatHost>(entity) {
                if host.error.as_deref() != Some(error.as_str()) {
                    host.error = Some(error);
                }
                // The next pass waits for the model's tick to move, as after a good pass
                // (spec 81 § 9); a model that is missing has no tick and is looked up again.
                host.failed = true;
                host.last_seen = Some(now);
            }
        }
    }
    true
}

/// The keys of the items now in the collection, and a warning about them.
fn read_keys(
    world: &World,
    registry: &TypeRegistry,
    host: &RepeatHost,
) -> Result<(Vec<String>, Option<String>), String> {
    let list = resolve_list(world, registry, host)?;
    let count = list.len().min(MAX_REPEAT_ITEMS);
    let mut note = (list.len() > MAX_REPEAT_ITEMS).then(|| {
        format!(
            "repeat_capped: {} has {} items; {MAX_REPEAT_ITEMS} are shown",
            host.source,
            list.len()
        )
    });
    let key_path = host.key.as_deref().map(|k| k.strip_prefix('.').unwrap_or(k));
    let mut keys = Vec::with_capacity(count);
    for index in 0..count {
        let key = match (key_path, list.get(index)) {
            (Some(path), Some(item)) => match relative(item, path) {
                Ok(value) => reflect_to_string(value),
                Err(e) => return Err(format!("repeat_bad_key: item {index}: {e}")),
            },
            _ => index.to_string(),
        };
        keys.push(key);
    }
    let duplicates = unique_keys(&mut keys);
    if duplicates > 0 && note.is_none() {
        note = Some(format!(
            "repeat_duplicate_key: {duplicates} item(s) of {} repeat a key; they follow by occurrence",
            host.source
        ));
    }
    Ok((keys, note))
}

/// Brings the instances to `keys`, then pushes the changed item bindings.
fn reconcile(
    world: &mut World,
    registry: &TypeRegistry,
    entity: Entity,
    keys: Vec<String>,
    note: Option<String>,
    started: Tick,
) {
    let Some(mut host) = world.get_mut::<RepeatHost>(entity) else {
        return;
    };
    let same =
        host.instances.len() == keys.len() && host.instances.iter().zip(&keys).all(|(i, k)| *i.key == **k);
    let mut stats = RepeatStats {
        passes: host.stats.passes + 1,
        ..RepeatStats::default()
    };
    if !same {
        let old: Vec<Arc<str>> = host.instances.iter().map(|i| i.key.clone()).collect();
        let old_keys: Vec<&str> = old.iter().map(|k| &**k).collect();
        let new_keys: Vec<&str> = keys.iter().map(String::as_str).collect();
        let plan = plan_keyed_diff(&old_keys, &new_keys);
        let mut taken: Vec<Option<Instance>> = std::mem::take(&mut host.instances)
            .into_iter()
            .map(Some)
            .collect();
        let template = host.template.clone();
        for &gone in &plan.removed {
            if let Some(instance) = taken.get_mut(gone).and_then(Option::take) {
                for root in instance.roots {
                    if world.get_entity(root).is_ok() {
                        world.despawn(root);
                    }
                }
                stats.despawned += 1;
            }
        }
        let mut instances = Vec::with_capacity(keys.len());
        for (source, key) in plan.sources.iter().zip(&keys) {
            let kept = source.and_then(|i| taken.get_mut(i).and_then(Option::take));
            instances.push(kept.unwrap_or_else(|| {
                stats.spawned += 1;
                spawn_instance(world, entity, &template, Arc::from(key.as_str()))
            }));
        }
        stats.moved = plan.reorder;
        if plan.reorder {
            reorder_children(world, entity, &instances);
        }
        let positions = instances
            .iter()
            .enumerate()
            .map(|(i, instance)| (instance.key.clone(), i as u32))
            .collect();
        if let Some(mut host) = world.get_mut::<RepeatHost>(entity) {
            host.instances = instances;
            host.positions = positions;
        }
    }
    let (changes, touched) = read_item_bindings(world, registry, entity);
    write_changes(world, changes);
    stats.touched = touched;
    if let Some(mut host) = world.get_mut::<RepeatHost>(entity) {
        host.stats = stats;
        host.error = note;
        host.failed = false;
        host.last_seen = Some(started);
    }
}

/// The view a virtualized host's range depends on; `None` before the list is laid out.
fn view_key(world: &World, entity: Entity) -> Option<ViewKey> {
    let node = world.get_entity(entity).ok()?;
    let computed = node.get::<ComputedNode>()?;
    let list = node.get::<ListView>()?;
    Some(ViewKey {
        rect: computed.rect,
        scale: computed.scale,
        offset: node.get::<ScrollOffset>().map_or(0.0, |o| o.0[1]),
        item_height: list.item_height,
        overscan: list.overscan,
        heights: node.get_ref::<ListHeights>().map(|h| h.last_changed()),
        tiles: node.contains::<ListTiles>(),
    })
}

/// Where the anchor item sits after the collection changed: the scroll offset (UI px) that keeps
/// the first visible item where it was. The anchor itself, else the first item after it in the old
/// order that is still there, else the old offset.
fn anchored_offset(
    geometry: &ListGeometry,
    width: f32,
    offset: f32,
    window: &Window,
    positions: &HashMap<Arc<str>, u32>,
) -> f32 {
    let Some((anchor, past)) = &window.anchor else {
        return offset;
    };
    if let Some(&index) = positions.get(anchor) {
        return geometry.item_rect(index as usize, width)[1] + past;
    }
    let Some(old) = window.keys.iter().position(|k| k == anchor) else {
        return offset;
    };
    // The anchor item is gone: the next surviving item takes the anchor's place on screen.
    for key in window.keys.iter().skip(old + 1).take(window.keys.len().min(64)) {
        if let Some(&index) = positions.get(key) {
            return geometry.item_rect(index as usize, width)[1] + past;
        }
    }
    offset
}

/// A laid-out list's geometry for a virtualized pass (UI px).
struct ListLayout {
    geometry: ListGeometry,
    width: f32,
    viewport: f32,
    scale: f32,
    overscan: usize,
    offset: f32,
}

/// The geometry of the list `entity` for `total` items; `None` before it is laid out.
fn list_view(world: &World, entity: Entity, total: usize) -> Option<ListLayout> {
    let node = world.get_entity(entity).ok()?;
    let computed = node.get::<ComputedNode>()?;
    if computed.rect[2] <= 0.0 || computed.rect[3] <= 0.0 {
        return None;
    }
    let list = node.get::<ListView>()?;
    let scale = computed.scale.max(0.001);
    let width = computed.rect[2] / scale;
    Some(ListLayout {
        geometry: crate::list::list_geometry(
            list,
            total,
            width,
            node.get::<ListHeights>(),
            node.get::<ListTiles>(),
        ),
        width,
        viewport: computed.rect[3] / scale,
        scale,
        overscan: list.overscan as usize,
        offset: node.get::<ScrollOffset>().map_or(0.0, |o| o.0[1]) / scale,
    })
}

/// The pass of a virtualized repeat: with `read`, the collection changed (new keys); without, only
/// the view moved. Moves the scroll offset to the anchor, picks the visible range, gives the rows
/// of items that left it to items that entered it, spawns or despawns only the difference, places
/// the rows, then pushes the changed item bindings of the rows.
fn reconcile_virtual(
    world: &mut World,
    registry: &TypeRegistry,
    entity: Entity,
    read: Option<(Vec<String>, Option<String>)>,
    started: Tick,
) {
    let _span = tracing::trace_span!("ui_repeat_virtual").entered();
    let Some(host) = world.get::<RepeatHost>(entity) else {
        return;
    };
    let note = match &read {
        Some((_, note)) => note.clone(),
        None => host.error.clone(),
    };
    let new_keys: Option<Vec<Arc<str>>> = read.map(|(keys, _)| keys.into_iter().map(Arc::from).collect());
    let total = new_keys
        .as_ref()
        .map_or_else(|| host.window.as_ref().map_or(0, |w| w.keys.len()), Vec::len);
    let view = list_view(world, entity, total);

    let Some(mut host) = world.get_mut::<RepeatHost>(entity) else {
        return;
    };
    let mut stats = RepeatStats {
        passes: host.stats.passes + 1,
        ..RepeatStats::default()
    };
    let mut window = host.window.take().unwrap_or_default();
    let instances = std::mem::take(&mut host.instances);
    let template = host.template.clone();
    let root_name = host.root.clone();
    if let Some(keys) = &new_keys {
        host.positions = keys
            .iter()
            .enumerate()
            .map(|(i, k)| (k.clone(), i as u32))
            .collect();
    }
    // Scroll anchoring, then the range.
    let mut offset = view.as_ref().map_or(0.0, |v| v.offset);
    let mut range = 0..0;
    if let Some(v) = &view {
        if new_keys.is_some() {
            let max = (v.geometry.total_height() - v.viewport).max(0.0);
            offset = anchored_offset(&v.geometry, v.width, offset, &window, &host.positions).clamp(0.0, max);
        }
        range = v.geometry.visible_range(offset, v.viewport, v.overscan);
    }
    if let Some(keys) = new_keys {
        window.keys = keys;
    }
    range.end = range.end.min(window.keys.len());
    range.start = range.start.min(range.end);
    window.anchor = view.as_ref().and_then(|v| {
        let first = v.geometry.visible_range(offset, v.viewport, 0).start;
        let key = window.keys.get(first)?;
        Some((key.clone(), offset - v.geometry.item_rect(first, v.width)[1]))
    });

    // Rows: kept by key, recycled from items that left the range, spawned only past that.
    let old_rows: Vec<Entity> = instances.iter().flat_map(|i| i.roots.iter().copied()).collect();
    let wanted = &window.keys[range.clone()];
    let wanted_set: HashSet<&str> = wanted.iter().map(|k| &**k).collect();
    let mut kept: HashMap<Arc<str>, Instance> = HashMap::with_capacity(wanted.len());
    let mut free = Vec::new();
    for instance in instances {
        if wanted_set.contains(&*instance.key) {
            kept.insert(instance.key.clone(), instance);
        } else {
            free.push(instance);
        }
    }
    // Recycle in the old order: the first row to leave is the first to come back; spares last.
    free.reverse();
    free.splice(0..0, std::mem::take(&mut window.spare));
    let mut next = Vec::with_capacity(wanted.len());
    for key in wanted {
        if let Some(instance) = kept.remove(key) {
            next.push(instance);
        } else if let Some(mut instance) = free.pop() {
            rekey(world, entity, &mut instance, key.clone());
            stats.recycled += 1;
            next.push(instance);
        } else {
            stats.spawned += 1;
            next.push(spawn_row(world, entity, &template, key.clone(), &root_name));
        }
    }
    // Rows left over: keep a few hidden for the window's growth, despawn the rest.
    let keep = view.as_ref().map_or(0, |v| 2 * v.overscan + 2);
    for instance in free {
        if window.spare.len() < keep {
            hide_row(world, &instance);
            window.spare.push(instance);
            continue;
        }
        for root in instance.roots {
            if world.get_entity(root).is_ok() {
                world.despawn(root);
            }
        }
        stats.despawned += 1;
    }
    let new_rows: Vec<Entity> = next.iter().flat_map(|i| i.roots.iter().copied()).collect();
    if new_rows != old_rows {
        stats.moved = true;
        reorder_children(world, entity, &next);
    }

    // Place the rows and size the list.
    let mut relayout = false;
    if let Some(v) = &view {
        for (i, instance) in next.iter().enumerate() {
            let index = range.start + i;
            let style = crate::list::abs_style(v.geometry.item_rect(index, v.width));
            let Some(&row) = instance.roots.first() else {
                continue;
            };
            if let Some(mut node_style) = world.get_mut::<NodeStyle>(row)
                && node_style.0 != style
            {
                node_style.0 = style;
                relayout = true;
            }
            if let Some(mut list_row) = world.get_mut::<ListRow>(row)
                && list_row.index != index
            {
                list_row.index = index;
            }
        }
        let physical = offset * v.scale;
        if let Some(mut scroll) = world.get_mut::<ScrollOffset>(entity)
            && (scroll.0[1] - physical).abs() > 1e-3
        {
            scroll.0[1] = physical;
            relayout = true;
        }
    }
    let total = window.keys.len();
    if world.get::<ItemCount>(entity).is_some_and(|c| c.0 != total) {
        world.entity_mut(entity).insert(ItemCount(total));
        relayout = true;
    }
    if relayout && let Some(mut dirty) = world.get_resource_mut::<crate::layout::LayoutDirty>() {
        dirty.mark();
    }
    window.start = range.start;
    window.spawned_total += stats.spawned as u64;
    window.recycled_total += stats.recycled as u64;
    window.view = view_key(world, entity);
    if let Some(mut host) = world.get_mut::<RepeatHost>(entity) {
        host.instances = next;
        host.window = Some(window);
    }

    let (changes, touched) = read_item_bindings(world, registry, entity);
    write_changes(world, changes);
    stats.touched = touched;
    if let Some(mut host) = world.get_mut::<RepeatHost>(entity) {
        host.stats = stats;
        host.error = note;
        host.failed = false;
        host.last_seen = Some(started);
    }
}

/// Hides a spare row: it shows no item until it is given one.
fn hide_row(world: &mut World, instance: &Instance) {
    for &row in &instance.roots {
        if let Some(mut style) = world.get_mut::<NodeStyle>(row)
            && style.0.display != crate::style::Display::None
        {
            style.0 = crate::list::hidden_style();
        }
        if let Some(mut list_row) = world.get_mut::<ListRow>(row)
            && list_row.index != NO_ITEM
        {
            list_row.index = NO_ITEM;
        }
    }
}

/// Spawns a row of a virtualized repeat: a list entry node (hidden until placed) holding the
/// template, scoped to the item `key`.
fn spawn_row(
    world: &mut World,
    host: Entity,
    template: &[UiNodeDef],
    key: Arc<str>,
    root_name: &str,
) -> Instance {
    let row = world
        .spawn((
            UiNode,
            NodeStyle::new(crate::list::hidden_style()),
            Classes::one("list-item"),
            Interactable::default(),
            crate::focus::Focusable::default(),
            crate::theme::Themed::default(),
            ListRow {
                list: host,
                index: NO_ITEM,
            },
            ChildOf(host),
        ))
        .id();
    let mut bound = Vec::new();
    for def in template {
        let root = spawn_node(world, row, def);
        scope_subtree(world, root, host, &key, root_name, &mut bound);
    }
    Instance {
        key,
        roots: vec![row],
        bound,
    }
}

/// Gives a row of a virtualized repeat to the item `key`: the row's nodes name the new item, the
/// repeats inside it re-read their collection, and one-time bindings push again. The item bindings
/// then rewrite only what differs from the old item.
fn rekey(world: &mut World, host: Entity, instance: &mut Instance, key: Arc<str>) {
    instance.key = key.clone();
    let mut stack = instance.roots.clone();
    while let Some(node) = stack.pop() {
        let Ok(mut entity) = world.get_entity_mut(node) else {
            continue;
        };
        if let Some(mut bound) = entity.get_mut::<RepeatBound>()
            && bound.host == host
        {
            bound.key = key.clone();
        }
        if let Some(mut nested) = entity.get_mut::<RepeatHost>() {
            if let Some(scope) = &mut nested.scope
                && scope.0 == host
            {
                scope.1 = key.clone();
            }
            nested.last_seen = None;
            if let Some(window) = &mut nested.window {
                window.view = None;
            }
        }
        if let Some(children) = entity.get::<Children>() {
            stack.extend(children.iter());
        }
    }
    for &node in &instance.bound {
        let Ok(mut entity) = world.get_entity_mut(node) else {
            continue;
        };
        if let Some(mut bind) = entity.get_mut::<Bind>() {
            forget_one_time(&mut bind);
        }
        if let Some(mut list) = entity.get_mut::<ViewBindings>() {
            for bind in &mut list.0 {
                forget_one_time(bind);
            }
        }
    }
}

/// A one-time binding pushes once per item, so a recycled row pushes it again.
fn forget_one_time(bind: &mut Bind) {
    if bind.mode == crate::binding::BindMode::OneTime {
        bind.last = None;
    }
}

/// Spawns the template once as children of `host` and scopes it to the item.
fn spawn_instance(world: &mut World, host: Entity, template: &[UiNodeDef], key: Arc<str>) -> Instance {
    let mut roots = Vec::with_capacity(template.len());
    let mut bound = Vec::new();
    let root_name = world
        .get::<RepeatHost>(host)
        .map(|h| h.root.clone())
        .unwrap_or_default();
    for def in template {
        let root = spawn_node(world, host, def);
        roots.push(root);
        scope_subtree(world, root, host, &key, &root_name, &mut bound);
    }
    Instance { key, roots, bound }
}

/// Clears the "outside a repeat" error of the item bindings under `top`, records the nodes that
/// have them and gives nested hosts their scope. Only template nodes exist at this point: nested
/// instances spawn later, on their host's own pass.
fn scope_subtree(
    world: &mut World,
    top: Entity,
    host: Entity,
    key: &Arc<str>,
    root_name: &str,
    bound: &mut Vec<Entity>,
) {
    let mut stack = vec![top];
    while let Some(node) = stack.pop() {
        let mut has_item = false;
        let mut entity = world.entity_mut(node);
        if let Some(mut bind) = entity.get_mut::<Bind>() {
            has_item |= claim_item_binding(&mut bind);
        }
        if let Some(mut list) = entity.get_mut::<ViewBindings>() {
            for bind in &mut list.0 {
                has_item |= claim_item_binding(bind);
            }
        }
        if has_item {
            bound.push(node);
        }
        if has_item || entity.contains::<UiCommandName>() {
            entity.insert(RepeatBound {
                host,
                key: key.clone(),
            });
        }
        if let Some(mut nested) = entity.get_mut::<RepeatHost>()
            && nested.is_relative()
        {
            nested.scope = Some((host, key.clone()));
            root_name.clone_into(&mut nested.root);
        }
        if let Some(children) = entity.get::<Children>() {
            stack.extend(children.iter().rev());
        }
    }
}

/// An item binding inside a template is resolved by its host from now on.
fn claim_item_binding(bind: &mut Bind) -> bool {
    if matches!(bind.source, BindSource::Item(_)) {
        bind.error = None;
        true
    } else {
        false
    }
}

/// Orders the host's children: other children (a scrollbar a widget added) first, in their order,
/// then the instances in collection order. Only `Children` changes, so no row is touched.
fn reorder_children(world: &mut World, host: Entity, instances: &[Instance]) {
    let rank: HashMap<Entity, usize> = instances
        .iter()
        .flat_map(|instance| instance.roots.iter().copied())
        .enumerate()
        .map(|(i, e)| (e, i + 1))
        .collect();
    if let Some(mut children) = world.get_mut::<Children>(host) {
        children.sort_by_cached_key(|e| rank.get(e).copied().unwrap_or(0));
    }
}

/// Resolves every item binding of the host against its current items. Returns what changed and
/// how many existing instances changed (a row with a never-pushed binding is new, not touched).
fn read_item_bindings(world: &World, registry: &TypeRegistry, entity: Entity) -> (Vec<BindingChange>, usize) {
    let mut changes = Vec::new();
    let mut touched = 0;
    let Some(host) = world.get::<RepeatHost>(entity) else {
        return (changes, touched);
    };
    let Ok(list) = resolve_list(world, registry, host) else {
        return (changes, touched);
    };
    let first = host.first_index();
    for (offset, instance) in host.instances.iter().enumerate() {
        let Some(item) = list.get(first + offset) else {
            continue;
        };
        let before = changes.len();
        let mut fresh = false;
        for &node in &instance.bound {
            if let Some(bind) = world.get::<Bind>(node) {
                fresh |= refresh_item(node, None, bind, item, &mut changes);
            }
            if let Some(binds) = world.get::<ViewBindings>(node) {
                for (slot, bind) in binds.0.iter().enumerate() {
                    fresh |= refresh_item(node, Some(slot), bind, item, &mut changes);
                }
            }
        }
        if changes.len() > before && !fresh {
            touched += 1;
        }
    }
    (changes, touched)
}

/// One item binding: what changed against `item`. Returns whether the binding had never been
/// pushed (a new row).
fn refresh_item(
    node: Entity,
    slot: Option<usize>,
    bind: &Bind,
    item: &dyn PartialReflect,
    changes: &mut Vec<BindingChange>,
) -> bool {
    let BindSource::Item(source) = &bind.source else {
        return false;
    };
    let fresh = bind.last.is_none();
    if bind.mode == crate::binding::BindMode::OneTime && !fresh {
        return false;
    }
    let path = source.strip_prefix('.').unwrap_or(source);
    let raw = match relative(item, path) {
        Ok(value) => reflect_to_string(value),
        Err(err) => {
            let reason = format!("{}: {source}: {err}", crate::binding::UNRESOLVED);
            if bind.error.as_deref() != Some(reason.as_str()) {
                changes.push(BindingChange::error(node, slot, bind, reason));
            }
            return fresh;
        }
    };
    changes.extend(bind.change_for(node, slot, &raw));
    fresh
}

/// The path a two-way write of `rel` on the item of `node` goes to: `"Vm.items.3.count"`.
pub(crate) fn absolute_item_path(world: &World, node: Entity, source: &str) -> Result<String, String> {
    let bound = world
        .get::<RepeatBound>(node)
        .ok_or_else(|| format!("binding_item_outside_repeat: {source:?} is not inside a repeat"))?;
    let rel = source.strip_prefix('.').unwrap_or(source);
    let item = item_path(world, bound.host, &bound.key)?;
    Ok(if rel.is_empty() {
        item
    } else {
        format!("{item}.{rel}")
    })
}

/// The absolute path of the item `key` of `host`: walks up through relative sources.
fn item_path(world: &World, host: Entity, key: &str) -> Result<String, String> {
    let gone = || "binding_unresolved: the item is gone".to_owned();
    let host_ref = world.get::<RepeatHost>(host).ok_or_else(gone)?;
    let index = host_ref.index_of(key).ok_or_else(gone)?;
    if let Some(rel) = host_ref.source.strip_prefix('.') {
        let (parent, parent_key) = host_ref.scope.as_ref().ok_or_else(gone)?;
        let parent_path = item_path(world, *parent, parent_key)?;
        Ok(if rel.is_empty() {
            format!("{parent_path}.{index}")
        } else {
            format!("{parent_path}.{rel}.{index}")
        })
    } else {
        Ok(format!("{}.{index}", host_ref.source))
    }
}

/// The stable key of the collection item `node` belongs to (it has item bindings or a command).
pub fn item_key_of(world: &World, node: Entity) -> Option<&str> {
    world.get::<RepeatBound>(node).map(|b| &*b.key)
}

/// Keys listed per repeat node by `ui.collection_state {keys: true}`.
const MAX_LISTED_KEYS: usize = 256;

/// `ui.collection_state`: per repeat node, its source, key path, instance count, what the last pass
/// did and its error code.
pub fn agent_collection_state(world: &World, params: &Value) -> Value {
    let list_keys = params.get("keys").and_then(Value::as_bool).unwrap_or(false);
    let hosts: &[Entity] = world
        .get_resource::<RepeatHosts>()
        .map_or(&[][..], |h| h.0.as_slice());
    let mut out = Vec::new();
    let mut total = 0;
    let mut errors = 0;
    for &entity in hosts {
        let Some(host) = world.get::<RepeatHost>(entity) else {
            continue;
        };
        total += host.instance_count();
        let code = host
            .error
            .as_deref()
            .map(|e| e.split(':').next().unwrap_or(e).to_owned());
        errors += usize::from(code.is_some());
        let mut row = json!({
            "node": entity.to_bits(),
            "name": world.get::<NodeName>(entity).map(|n| n.0.clone()),
            "source": host.source,
            "key": host.key,
            "instances": host.instance_count(),
            "passes": host.stats.passes,
            "spawned": host.stats.spawned,
            "despawned": host.stats.despawned,
            "moved": host.stats.moved,
            "touched": host.stats.touched,
            "recycled": host.stats.recycled,
            "code": code,
            "error": host.error,
        });
        if host.is_virtual() {
            row["virtual"] = json!(true);
            row["items"] = json!(host.item_count());
            row["materialized"] = json!(host.instance_count());
            row["spare_rows"] = json!(host.spare_rows());
            row["first_index"] = json!(host.first_index());
            row["first_visible_key"] = json!(host.anchor_key());
            row["spawned_total"] = json!(host.spawned_total());
            row["recycled_total"] = json!(host.recycled_total());
            row["row_height"] = json!(host.row_height.name());
            row["row_px"] = json!(world.get::<ListView>(entity).map(|l| l.item_height));
        } else {
            row["virtual"] = json!(false);
        }
        if list_keys {
            row["keys"] = json!(host.keys().take(MAX_LISTED_KEYS).collect::<Vec<_>>());
        }
        out.push(row);
    }
    json!({ "hosts": out, "instances": total, "errors": errors })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_keeps_removes_spawns_and_detects_moves() {
        let plan = plan_keyed_diff(&["a", "b", "c"], &["a", "c"]);
        assert_eq!(plan.sources, vec![Some(0), Some(2)]);
        assert_eq!(plan.removed, vec![1]);
        assert!(!plan.reorder, "a removal keeps the order");

        let plan = plan_keyed_diff(&["a", "b"], &["a", "b", "c"]);
        assert_eq!(plan.sources, vec![Some(0), Some(1), None]);
        assert!(!plan.reorder, "an append lands at the end by itself");

        let plan = plan_keyed_diff(&["a", "b"], &["x", "a", "b"]);
        assert!(plan.reorder, "a new first item must move in front");

        let plan = plan_keyed_diff(&["a", "b", "c"], &["c", "a", "b"]);
        assert_eq!(plan.sources, vec![Some(2), Some(0), Some(1)]);
        assert!(plan.removed.is_empty());
        assert!(plan.reorder);

        let plan = plan_keyed_diff(&[], &[]);
        assert_eq!(plan, KeyedPlan::default());
        let plan = plan_keyed_diff(&["a", "b"], &[]);
        assert_eq!(plan.removed, vec![0, 1]);
        assert!(plan.sources.is_empty());
    }

    #[test]
    fn duplicates_become_unique_and_stay_stable() {
        let mut keys: Vec<String> = ["a", "b", "a", "a#2", "a"].map(str::to_owned).to_vec();
        assert_eq!(unique_keys(&mut keys), 2);
        assert_eq!(keys, ["a", "b", "a#3", "a#2", "a#4"]);
        let all: HashSet<&String> = keys.iter().collect();
        assert_eq!(all.len(), keys.len());
        let mut none: Vec<String> = ["x", "y"].map(str::to_owned).to_vec();
        assert_eq!(unique_keys(&mut none), 0);
    }

    #[derive(Resource, Reflect, Debug, Default)]
    #[reflect(Resource, Default)]
    struct CapVm {
        items: Vec<u8>,
    }

    /// Spec 81 § 2.7: a host keeps at most `MAX_REPEAT_ITEMS` (65,536) instances and reports
    /// `repeat_capped` past that; at the cap exactly there is no note.
    #[test]
    fn read_keys_caps_the_collection_and_reports() {
        assert_eq!(MAX_REPEAT_ITEMS, 65_536);
        let types = regia_ecs::prelude::AppTypeRegistry::default();
        types.write().register::<CapVm>();
        let mut world = World::new();
        world.insert_resource(types.clone());
        let host = RepeatHost::new(
            &RepeatDef {
                source: "CapVm.items".to_owned(),
                key: None,
                ..RepeatDef::default()
            },
            Arc::from(Vec::new()),
        );
        for (len, capped) in [(MAX_REPEAT_ITEMS, false), (MAX_REPEAT_ITEMS + 1, true)] {
            world.insert_resource(CapVm { items: vec![0; len] });
            let registry = types.read();
            let (keys, note) = read_keys(&world, &registry, &host).expect("keys");
            assert_eq!(keys.len(), MAX_REPEAT_ITEMS, "{len} items");
            assert_eq!(keys.last().map(String::as_str), Some("65535"));
            assert_eq!(
                note.as_deref().is_some_and(|n| n.starts_with("repeat_capped")),
                capped,
                "{len} items: {note:?}"
            );
        }
    }
}
