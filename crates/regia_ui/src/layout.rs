// SPDX-License-Identifier: MPL-2.0
//! Layout: reconciles a persistent taffy tree with the ECS hierarchy each dirty frame (only
//! changed styles, children and text measures mark taffy nodes dirty, so unchanged subtrees reuse
//! their cached layout), computes flexbox/grid layout in physical pixels and writes
//! [`ComputedNode`]s (`specs/18` § 1).
//!
//! Text nodes are measured by pre-shaping them (natural size) and handing the size to taffy as a
//! leaf measure; after the first compute pass, wrapped text whose final width differs from the
//! measured width triggers one more pass (two-pass fixed point — enough for HUD-grade layouts).

use std::collections::{HashMap, HashSet};

use regia_app::prelude::*;

use crate::node::{Border, ClipChildren, ComputedNode, NodeStyle, UiNode, UiRoot, ZOffset};
use crate::scale::{UiScales, resolve_window_size};
use crate::scroll::ScrollOffset;
use crate::style::{Display, Overflow};
use crate::text::{Text, TextSystem};

/// Entities whose own layout inputs (style, text, widget state) changed since the last layout:
/// the next [`run_ui_layout`] reconciles just these taffy nodes. Structural changes (hierarchy,
/// scale, safe area, locale) go through [`LayoutDirty`] instead and reconcile everything.
#[derive(Resource, Debug, Default)]
pub struct LayoutChanges(pub Vec<Entity>);

/// Set when the UI needs a full relayout (hierarchy, scale, safe area or locale changed, or a
/// widget asked for it); the next [`run_ui_layout`] reconciles every node. Per-node style and
/// text edits are tracked more finely in [`LayoutChanges`].
#[derive(Resource, Debug)]
pub struct LayoutDirty(pub bool);

impl Default for LayoutDirty {
    fn default() -> Self {
        // The first frame always lays out.
        Self(true)
    }
}

impl LayoutDirty {
    /// Forces a relayout on the next frame.
    pub fn mark(&mut self) {
        self.0 = true;
    }
}

/// Set when scroll offsets moved (ADR-520). When nothing else changed, the next [`run_ui_layout`]
/// re-walks the existing layout so positions and clip rects follow the offsets, without
/// reconciling or recomputing the taffy tree: a scrolled frame costs a walk over the nodes, not a
/// flexbox pass.
#[derive(Resource, Debug, Default)]
pub struct ScrollDirty(pub bool);

impl ScrollDirty {
    /// Asks for a scroll-only re-walk on the next frame.
    pub fn mark(&mut self) {
        self.0 = true;
    }
}

/// Cumulative counters for UI frame hot paths. A test reads the delta across a fixed number of
/// idle frames. Gameplay code does not.
#[derive(Resource, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct UiHotPathCounts {
    /// `CaretMap` clones on a cache hit (a focused idle field should stop after the first shape).
    pub caret_map_clones: u64,
    /// `TextInputLayout` inserts (a cache hit must not rewrite the component).
    pub text_input_layout_writes: u64,
    /// Text inputs whose label string was rebuilt by `sync_parts`.
    pub text_input_syncs: u64,
    /// Label strings the dropdown, tab bar and tooltip syncs cloned to write a label (one `String`
    /// allocation each). Idle syncs compare the shown string in place and do not increment this.
    pub widget_text_clones: u64,
    /// Nodes `extract::walk` visited on the latest extract. A clean frame stores 0.
    pub extract_nodes_walked: u64,
    /// Frames that scanned every `ComputedNode` looking for a transform to apply.
    pub transform_node_scans: u64,
}

/// Counters of [`run_ui_layout`] by kind of run (diagnostics; the 10,000-row scroll test reads
/// them).
#[derive(Resource, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct UiLayoutStats {
    /// Runs that reconciled every node (structure, scale, locale changed).
    pub full_runs: u64,
    /// Runs that reconciled only changed nodes and their ancestors.
    pub incremental_runs: u64,
    /// Scroll-only runs: the existing layout re-walked at new offsets, taffy untouched.
    pub scroll_runs: u64,
    /// Nodes the last run wrote a layout for.
    pub nodes_walked: u64,
    /// Nodes whose `ComputedNode` the last run changed.
    pub nodes_changed: u64,
}

/// Per-run layout state: the (persistent) taffy tree plus entity ↔ node maps.
pub struct UiLayoutContext {
    /// Reconcile every node (structural change) instead of only `changed` ones.
    full: bool,
    /// Entities whose style / text / widget state changed (see [`LayoutChanges`]).
    changed: HashSet<Entity>,
    /// `changed` plus all their ancestors: the only nodes an incremental run descends into.
    affected: HashSet<Entity>,
    tree: taffy::TaffyTree<TextMeasure>,
    entity_to_node: HashMap<Entity, taffy::tree::NodeId>,
    /// Nodes of the previous run, reused by entity (stale ones are removed after the run).
    prev_nodes: HashMap<Entity, taffy::tree::NodeId>,
    /// Natural (unwrapped) text sizes from the previous layout run, keyed by entity and
    /// validated by (content digest, scale): unchanged texts are not re-shaped on relayout.
    natural_prev: HashMap<Entity, NaturalMeasure>,
    natural_next: HashMap<Entity, NaturalMeasure>,
    /// Localization generation (part of the text digest).
    loc_generation: u64,
}

/// A cached natural text measure.
#[derive(Debug, Clone, Copy, PartialEq)]
struct NaturalMeasure {
    digest: u64,
    scale_bits: u32,
    size: [f32; 2],
}

/// Layout state carried between runs: the taffy tree (with its per-node layout caches), the
/// entity → node map and natural text measures. Held as a non-send resource: taffy's styles
/// are not `Send` (tagged-pointer lengths), and [`run_ui_layout`] is an exclusive system.
pub struct LayoutTree {
    tree: taffy::TaffyTree<TextMeasure>,
    nodes: HashMap<Entity, taffy::tree::NodeId>,
    natural: HashMap<Entity, NaturalMeasure>,
}

impl Default for LayoutTree {
    fn default() -> Self {
        Self {
            tree: taffy::TaffyTree::new(),
            nodes: HashMap::new(),
            natural: HashMap::new(),
        }
    }
}

/// Leaf measure data: the pre-shaped text size.
#[derive(Debug, Clone, Copy, Default)]
pub struct TextMeasure {
    /// Measured (width, height) in physical px.
    pub size: [f32; 2],
}

/// `PostUpdate`: flag relayout when styles, text, hierarchy, scale, safe area or widget state
/// changed, or when the localization generation moved (text extents may change).
///
/// A new node, or a parent whose child list changed, is an incremental run: the parent is
/// rebuilt and taffy keeps every other subtree. A virtual list grows its pool by a row or two
/// as the window slides, and that must not be a full flexbox pass. A new or edited root, and
/// any despawn, is still a full run. The incremental path does not drop a taffy node whose
/// entity is gone.
pub fn mark_layout_dirty(
    mut dirty: ResMut<LayoutDirty>,
    mut geometry: Local<LastGeometry>,
    mut last_gen: Local<u64>,
    scales: Res<UiScales>,
    safe: Option<Res<regia_platform::SafeArea>>,
    loc: Option<Res<crate::localization::Localization>>,
    mut changes: ResMut<LayoutChanges>,
    structural: Query<(), Or<(Added<UiRoot>, Changed<UiRoot>)>>,
    added: Query<Entity, Added<UiNode>>,
    child_lists: Query<Entity, Changed<Children>>,
    nodes: Query<
        Entity,
        Or<(
            Changed<NodeStyle>,
            Changed<Text>,
            Changed<Border>,
            Changed<crate::scroll::ListView>,
            Changed<crate::scroll::ListItems>,
            // ProgressBar / Slider values only move extract-drawn chrome (draw-list damage), not
            // layout.
        )>,
    >,
    mut despawned: RemovedComponents<UiNode>,
    resolver: Option<ResMut<crate::theme_resolve::ThemeResolver>>,
) {
    let _span = tracing::debug_span!("mark_layout_dirty").entered();
    // Theme padding or min/max size moved (ADR-7208): those nodes re-read their layout box.
    if let Some(mut resolver) = resolver {
        changes
            .0
            .extend(resolver.bypass_change_detection().take_layout_changes());
    }
    if (geometry.size != scales.window_size || geometry.factor != scales.scale_factor) && scales.is_changed()
    {
        geometry.size = scales.window_size;
        geometry.factor = scales.scale_factor;
        dirty.mark();
    }
    if let Some(safe) = safe
        && safe.is_changed()
    {
        dirty.mark();
    }
    if let Some(loc) = loc
        && loc.generation.0 != *last_gen
    {
        *last_gen = loc.generation.0;
        dirty.mark();
    }
    if !structural.is_empty() {
        dirty.mark();
    }
    // The new node and its parent both have to be in the incremental set. The parent is the
    // one whose taffy child list is rewritten; the node itself is built.
    changes.0.extend(added.iter());
    changes.0.extend(child_lists.iter());
    changes.0.extend(nodes.iter());
    if despawned.read().next().is_some() {
        dirty.mark();
    }
}

/// Last window geometry layout ran with.
#[derive(Debug, Default, Clone, Copy)]
pub struct LastGeometry {
    size: [f32; 2],
    factor: f64,
}

/// `PostUpdate` (in `PostUpdateSet::UiLayout`): run taffy over every UI root and write
/// [`ComputedNode`]s. Errors are logged, never panicked on (user content).
pub fn run_ui_layout(world: &mut World) -> Result<(), taffy::TaffyError> {
    let _span = tracing::trace_span!("ui_layout").entered();
    let changed: HashSet<Entity> = world
        .get_resource_mut::<LayoutChanges>()
        .map(|mut c| c.0.drain(..).collect())
        .unwrap_or_default();
    let full = world.resource::<LayoutDirty>().0;
    let scrolled = world
        .get_resource_mut::<ScrollDirty>()
        .is_some_and(|mut s| std::mem::take(&mut s.0));
    if !full && changed.is_empty() {
        if scrolled {
            rewalk_scrolled(world);
        }
        return Ok(());
    }
    world.resource_mut::<LayoutDirty>().0 = false;

    let scales = *world.resource::<UiScales>();
    let loc = world
        .remove_resource::<crate::localization::Localization>()
        .unwrap_or_default();
    let Some(mut text_system) = world.remove_resource::<TextSystem>() else {
        world.insert_resource(loc);
        world.resource_mut::<LayoutDirty>().0 = true;
        return Ok(());
    };
    let persisted = world.remove_non_send::<LayoutTree>().unwrap_or_default();
    // Incremental runs only reach changed entities and their ancestors; everything else keeps
    // its node and measure, so the maps start as last run's.
    let mut affected: HashSet<Entity> = HashSet::new();
    if !full {
        for &e in &changed {
            let mut cur = Some(e);
            while let Some(c) = cur {
                if !affected.insert(c) {
                    break;
                }
                cur = world.get::<ChildOf>(c).map(ChildOf::parent);
            }
        }
    }
    // A full run keeps the previous ids in `prev_nodes` so entities that left the tree can be
    // removed. An incremental run moves the maps in place: a despawn already forced a full run,
    // so a second copy is only an allocation. The write walk still visits every node. Content
    // size is the max child reach, and skipping a sibling would keep a stale larger size after
    // that child shrinks.
    let (entity_to_node, natural_next, prev_nodes, natural_prev) = if full {
        (
            HashMap::with_capacity(persisted.nodes.len()),
            HashMap::with_capacity(persisted.natural.len()),
            persisted.nodes,
            persisted.natural,
        )
    } else {
        (persisted.nodes, persisted.natural, HashMap::new(), HashMap::new())
    };
    let mut ctx = UiLayoutContext {
        full,
        changed,
        affected,
        tree: persisted.tree,
        entity_to_node,
        prev_nodes,
        natural_next,
        natural_prev,
        loc_generation: loc.generation.0,
    };

    let roots = sorted_roots(world);

    let mut layouts: Vec<(Entity, ComputedNode)> = Vec::new();
    let mut next_order = 0u32;
    // A handheld policy fits 16:9-authored reference layouts to a 16:10 panel (ADR-526).
    let policy = world.get_resource::<crate::scale::UiDevicePolicy>().copied();
    let scale_settings = world
        .get_resource::<crate::scale::UiScaleSettings>()
        .cloned()
        .unwrap_or_default();
    for (order, root_entity) in roots {
        let order_start = next_order;
        let root = world.get::<UiRoot>(root_entity).cloned().unwrap_or_default();
        let scale = scale_settings.adjust(
            &root.scale_mode,
            scales.scale_factor,
            scales.scale_with_policy(&root.scale_mode, policy.as_ref()),
        ) * crate::output::render_out_boost(world, &root.scale_mode);
        let (window, mut safe_padding) = resolve_window_size(world, root_entity, &scales);
        // Integer scale (ADR-2866): the layout area is centred, the rest is letterbox.
        for (p, l) in safe_padding
            .iter_mut()
            .zip(root.scale_mode.letterbox(window, scale))
        {
            *p = p.max(l);
        }
        // A garbage window size or platform inset (NaN, negative) must not reach taffy.
        let window = window.map(finite_non_negative);
        let frame = RootFrame {
            window,
            safe_ltrb: fit_insets(safe_padding, window),
        };
        let locale_rtl = crate::style::locale_is_rtl(loc.locale());
        let build = tracing::trace_span!("ui_layout_build").entered();
        build_tree(
            world,
            &mut ctx,
            &mut text_system,
            &loc,
            root_entity,
            Some(frame),
            locale_rtl,
            scale,
            ItemCross::NONE,
        );
        let Some(root_node) = ctx.entity_to_node.get(&root_entity).copied() else {
            continue;
        };
        drop(build);
        let available = taffy::geometry::Size {
            width: taffy::style::AvailableSpace::Definite(window[0]),
            height: taffy::style::AvailableSpace::Definite(window[1]),
        };
        {
            let _compute = tracing::trace_span!("ui_layout_compute").entered();
            ctx.tree
                .compute_layout_with_measure(root_node, available, measure_fn)?;
        }
        let walk = tracing::trace_span!("ui_layout_write").entered();
        let mut writer = Writer {
            entity_to_node: &ctx.entity_to_node,
            tree: &ctx.tree,
            out: &mut layouts,
            scale,
            next_order: order_start,
        };
        writer.walk(world, root_entity, f64::from(order), 0, [0.0; 2], NONE_CLIP);
        next_order = writer.next_order;
        drop(walk);
        let refine = tracing::trace_span!("ui_layout_refine").entered();
        let refined = refine_wrapped_text(world, &mut ctx, &mut text_system, &loc, scale);
        drop(refine);
        if refined {
            let _second = tracing::trace_span!("ui_layout_second_pass").entered();
            ctx.tree
                .compute_layout_with_measure(root_node, available, measure_fn)?;
            layouts.retain(|(e, _)| *e != root_entity);
            let mut writer = Writer {
                entity_to_node: &ctx.entity_to_node,
                tree: &ctx.tree,
                out: &mut layouts,
                scale,
                next_order: order_start,
            };
            writer.walk(world, root_entity, f64::from(order), 0, [0.0; 2], NONE_CLIP);
            next_order = writer.next_order;
        }
    }
    let (walked, moved) = write_layouts(world, layouts);
    if let Some(mut stats) = world.get_resource_mut::<UiLayoutStats>() {
        if full {
            stats.full_runs += 1;
        } else {
            stats.incremental_runs += 1;
        }
        stats.nodes_walked = walked;
        stats.nodes_changed = moved;
    }
    // Drop nodes of entities that left the UI.
    for (entity, node) in &ctx.prev_nodes {
        if !ctx.entity_to_node.contains_key(entity) {
            let _ = ctx.tree.remove(*node);
        }
    }
    world.insert_non_send(LayoutTree {
        tree: ctx.tree,
        nodes: ctx.entity_to_node,
        natural: ctx.natural_next,
    });
    world.insert_resource(text_system);
    world.insert_resource(loc);
    Ok(())
}

/// Active UI roots, back to front (`sort_order`).
fn sorted_roots(world: &mut World) -> Vec<(i32, Entity)> {
    let mut roots: Vec<(i32, Entity)> = world
        .query::<(Entity, &UiRoot)>()
        .iter(world)
        .map(|(e, r)| (r.sort_order, e))
        .collect();
    let active = crate::output::active_variant(world);
    roots.retain(|(_, e)| crate::output::root_active(world, *e, active));
    // Equal sort orders tie-break on the entity so the paint order is the same on every run.
    roots.sort_by_key(|(order, e)| (*order, e.to_bits()));
    roots
}

/// Writes the walked layouts; returns (nodes walked, nodes changed).
fn write_layouts(world: &mut World, layouts: Vec<(Entity, ComputedNode)>) -> (u64, u64) {
    let walked = layouts.len() as u64;
    let mut moved = 0u64;
    for (entity, computed) in layouts {
        // Unchanged rects are not rewritten, so change detection downstream (shaping, draw-list
        // damage) only sees nodes that actually moved or resized.
        if world.get::<ComputedNode>(entity) != Some(&computed) {
            world.entity_mut(entity).insert(computed);
            moved += 1;
        }
    }
    (walked, moved)
}

/// Scroll-only run (ADR-520): walk the persisted taffy layout again so rects and clip rects follow
/// the new scroll offsets. Nothing is reconciled, measured or computed. A root that was never laid
/// out asks for a full run instead.
fn rewalk_scrolled(world: &mut World) {
    let _span = tracing::trace_span!("ui_layout_scroll").entered();
    let Some(persisted) = world.remove_non_send::<LayoutTree>() else {
        world.resource_mut::<LayoutDirty>().0 = true;
        return;
    };
    let scales = *world.resource::<UiScales>();
    let scale_settings = world
        .get_resource::<crate::scale::UiScaleSettings>()
        .cloned()
        .unwrap_or_default();
    let roots = sorted_roots(world);
    let mut layouts: Vec<(Entity, ComputedNode)> = Vec::new();
    let mut missing = false;
    let mut next_order = 0u32;
    for (order, root_entity) in roots {
        if !persisted.nodes.contains_key(&root_entity) {
            missing = true;
            continue;
        }
        let root = world.get::<UiRoot>(root_entity).cloned().unwrap_or_default();
        let scale = scale_settings.adjust(
            &root.scale_mode,
            scales.scale_factor,
            scales.scale_of(&root.scale_mode),
        ) * crate::output::render_out_boost(world, &root.scale_mode);
        let mut writer = Writer {
            entity_to_node: &persisted.nodes,
            tree: &persisted.tree,
            out: &mut layouts,
            scale,
            next_order,
        };
        writer.walk(world, root_entity, f64::from(order), 0, [0.0; 2], NONE_CLIP);
        next_order = writer.next_order;
    }
    world.insert_non_send(persisted);
    let (walked, moved) = write_layouts(world, layouts);
    if missing {
        world.resource_mut::<LayoutDirty>().0 = true;
    }
    if let Some(mut stats) = world.get_resource_mut::<UiLayoutStats>() {
        stats.scroll_runs += 1;
        stats.nodes_walked = walked;
        stats.nodes_changed = moved;
    }
}

/// Sentinel "no clip yet": a rect at -inf covering everything.
const NONE_CLIP: [f32; 4] = [f32::MIN / 4.0, f32::MIN / 4.0, f32::MAX, f32::MAX];

/// The taffy leaf measure function: returns the pre-shaped text size, honouring known dimensions.
fn measure_fn(
    inputs: taffy::tree::LayoutInput,
    _node: taffy::tree::NodeId,
    context: Option<&mut TextMeasure>,
    _style: &taffy::style::Style,
) -> taffy::tree::LayoutOutput {
    let m = context.map(|c| c.size).unwrap_or([0.0, 0.0]);
    let w = inputs.known_dimensions.width.unwrap_or(m[0]);
    let h = inputs.known_dimensions.height.unwrap_or(m[1]);
    taffy::tree::LayoutOutput {
        size: taffy::geometry::Size {
            width: w.max(0.0),
            height: h.max(0.0),
        },
        ..taffy::tree::LayoutOutput::DEFAULT
    }
}

/// What a root node adds to its style: the window it fills and the physical safe-area insets.
#[derive(Debug, Clone, Copy)]
struct RootFrame {
    /// Window (or render-out) size in physical px.
    window: [f32; 2],
    /// Safe-area insets left, top, right, bottom in physical px (never mirrored for RTL: a notch
    /// stays on its side of the screen).
    safe_ltrb: [f32; 4],
}

/// `v` if it is finite and non-negative, else 0 (window sizes, platform insets, box sizes).
fn finite_non_negative(v: f32) -> f32 {
    if v.is_finite() { v.max(0.0) } else { 0.0 }
}

/// `v` if it is finite, else 0 (positions and scroll offsets).
fn finite_or_zero(v: f32) -> f32 {
    if v.is_finite() { v } else { 0.0 }
}

/// Safe-area insets made usable: finite, non-negative, and each opposite pair scaled down to fit
/// the window. An inset pair wider than the screen (a bogus platform report, a forced render-out
/// safe area on a tiny output) would otherwise grow the root past the window, since taffy never
/// lets a box be smaller than its padding.
fn fit_insets(ltrb: [f32; 4], window: [f32; 2]) -> [f32; 4] {
    let [mut l, mut t, mut r, mut b] = ltrb.map(finite_non_negative);
    let fit = |start: &mut f32, end: &mut f32, extent: f32| {
        let sum = *start + *end;
        if sum > extent && sum > 0.0 {
            let k = extent / sum;
            *start *= k;
            *end *= k;
        }
    };
    fit(&mut l, &mut r, window[0]);
    fit(&mut t, &mut b, window[1]);
    [l, t, r, b]
}

/// The taffy node `entity` had at the start of this run. A full run keeps that map in
/// `prev_nodes`. An incremental run moved it into `entity_to_node` instead of copying it.
fn previous_node(ctx: &UiLayoutContext, entity: Entity) -> Option<taffy::tree::NodeId> {
    ctx.prev_nodes
        .get(&entity)
        .copied()
        .or_else(|| ctx.entity_to_node.get(&entity).copied())
}

/// The cached natural measure for `entity`: this run's entry if one was recorded, else the
/// previous run's.
fn cached_natural(ctx: &UiLayoutContext, entity: Entity) -> Option<NaturalMeasure> {
    ctx.natural_next
        .get(&entity)
        .copied()
        .or_else(|| ctx.natural_prev.get(&entity).copied())
}

/// Builds (or reuses) the taffy node of `entity` and its subtree. `root` is set for a [`UiRoot`];
/// `inherited_rtl` is the parent's resolved direction (the locale's at a root), which a
/// `TextDir::Auto` node inherits (ADR-518). `item` is how the parent lays this node out (see
/// [`ItemCross`]); a root passes `ItemCross::NONE`.
#[allow(clippy::too_many_arguments)]
fn build_tree(
    world: &World,
    ctx: &mut UiLayoutContext,
    text_system: &mut TextSystem,
    loc: &crate::localization::Localization,
    entity: Entity,
    root: Option<RootFrame>,
    inherited_rtl: bool,
    scale: f32,
    item: ItemCross,
) -> Option<taffy::tree::NodeId> {
    // Right-to-left mirroring (ADR-341, ADR-518): the node's `text_dir`, with `Auto` inheriting.
    let style = world.get::<NodeStyle>(entity).map(|s| &s.0);
    let rtl = style
        .map_or(crate::style::TextDir::Auto, |s| s.text_dir)
        .is_rtl(inherited_rtl);
    let direction = if rtl {
        taffy::style::Direction::Rtl
    } else {
        taffy::style::Direction::Ltr
    };
    // Unchanged node from the previous run: keep its taffy node (and layout cache) as is and only
    // descend for changed descendants. A node whose inherited direction flipped (an ancestor's
    // `text_dir` changed) is not unchanged: it is rebuilt, and so is every `Auto` node below it.
    // A child of a rebuilt parent whose flex cross axis moved is rebuilt too (its cross minimum).
    if !ctx.full
        && root.is_none()
        && !item.moved
        && !ctx.changed.contains(&entity)
        && let Some(node) = previous_node(ctx, entity)
        && ctx.tree.style(node).is_ok_and(|s| s.direction == direction)
    {
        if ctx.affected.contains(&entity)
            && let Some(children) = world.get::<Children>(entity)
        {
            let child_item = ItemCross {
                cross: ctx.tree.style(node).ok().and_then(flex_item_cross),
                moved: false,
            };
            for c in children.iter() {
                build_tree(world, ctx, text_system, loc, c, None, rtl, scale, child_item);
            }
        }
        return Some(node);
    }
    // Theme padding and min/max size (ADR-7208) replace the style's values where the theme sets
    // them; the merged style mirrors and sanitizes like any other.
    let boxed = themed_box(style, world.get::<crate::theme::Themed>(entity));
    let mut tstyle = match boxed.as_ref().or(style) {
        Some(s) => s.to_taffy_dir(scale, rtl),
        None => crate::style::Style::default().to_taffy_dir(scale, rtl),
    };
    floor_flex_item_cross_min(&mut tstyle, item.cross);
    if let Some(frame) = root {
        // Default the root to filling the window, plus safe-area padding. The insets are
        // physical and added after mirroring; percentages resolve against the window width (the
        // root's containing block), as taffy resolves percentage padding.
        tstyle.size.width = taffy::style::Dimension::percent(1.0);
        tstyle.size.height = taffy::style::Dimension::percent(1.0);
        let pad = &mut tstyle.padding;
        let basis = frame.window[0];
        pad.left = length_plus(pad.left, frame.safe_ltrb[0], basis);
        pad.top = length_plus(pad.top, frame.safe_ltrb[1], basis);
        pad.right = length_plus(pad.right, frame.safe_ltrb[2], basis);
        pad.bottom = length_plus(pad.bottom, frame.safe_ltrb[3], basis);
    }
    let children: Vec<Entity> = world
        .get::<Children>(entity)
        .map(|c| c.iter().collect())
        .unwrap_or_default();
    let node = if children.is_empty() {
        let measure = world.get::<Text>(entity).map(|text| {
            let digest = crate::text::text_digest(text, ctx.loc_generation);
            let scale_bits = scale.to_bits();
            let size = match cached_natural(ctx, entity) {
                Some(m) if m.digest == digest && m.scale_bits == scale_bits => m.size,
                _ => text_system.shape(text, scale, None, loc).size,
            };
            ctx.natural_next.insert(
                entity,
                NaturalMeasure {
                    digest,
                    scale_bits,
                    size,
                },
            );
            TextMeasure { size }
        });
        sync_node(ctx, entity, tstyle, &[], Some(measure.unwrap_or_default()))
    } else {
        let cross = flex_item_cross(&tstyle);
        let before = previous_node(ctx, entity)
            .and_then(|n| ctx.tree.style(n).ok())
            .map(flex_item_cross);
        let child_item = ItemCross {
            cross,
            moved: before != Some(cross),
        };
        let present: Vec<taffy::tree::NodeId> = children
            .iter()
            .filter_map(|&c| build_tree(world, ctx, text_system, loc, c, None, rtl, scale, child_item))
            .collect();
        sync_node(ctx, entity, tstyle, &present, None)
    };
    match node {
        Ok(node) => {
            ctx.entity_to_node.insert(entity, node);
            Some(node)
        }
        Err(_) => None,
    }
}

/// `style` with the theme's layout box laid over it (ADR-7208, ADR-9340): the theme's height,
/// inline padding (left and right), full padding (all four edges, over the inline padding), gap
/// and min/max size, in UI px, replace the style's own values. `None` when the node has no theme
/// layout box, so the style is used as is.
fn themed_box(
    style: Option<&crate::style::Style>,
    themed: Option<&crate::theme::Themed>,
) -> Option<crate::style::Style> {
    use crate::style::Dimension;
    let t = themed?;
    if t.padding.is_none()
        && t.min_width.is_none()
        && t.min_height.is_none()
        && t.max_width.is_none()
        && t.max_height.is_none()
        && t.layout_height.is_none()
        && t.layout_padding_x.is_none()
        && t.gap.is_none()
    {
        return None;
    }
    let mut out = style.cloned().unwrap_or_default();
    if let Some(px) = t.layout_padding_x {
        out.padding.left = Dimension::Points(px);
        out.padding.right = Dimension::Points(px);
    }
    if let Some(g) = t.gap {
        out.gap = [Dimension::Points(g), Dimension::Points(g)];
    }
    if let Some(h) = t.layout_height {
        out.size[1] = Dimension::Points(h);
    }
    if let Some([l, top, r, b]) = t.padding {
        out.padding = crate::style::Edges {
            left: Dimension::Points(l),
            top: Dimension::Points(top),
            right: Dimension::Points(r),
            bottom: Dimension::Points(b),
        };
    }
    let set = |slot: &mut Dimension, v: Option<f32>| {
        if let Some(v) = v {
            *slot = Dimension::Points(v);
        }
    };
    set(&mut out.min_size[0], t.min_width);
    set(&mut out.min_size[1], t.min_height);
    set(&mut out.max_size[0], t.max_width);
    set(&mut out.max_size[1], t.max_height);
    Some(out)
}

/// Reuses `entity`'s node from the previous run (touching only what changed, so taffy keeps the
/// caches of untouched subtrees) or creates it.
fn sync_node(
    ctx: &mut UiLayoutContext,
    entity: Entity,
    style: taffy::style::Style,
    children: &[taffy::tree::NodeId],
    context: Option<TextMeasure>,
) -> taffy::TaffyResult<taffy::tree::NodeId> {
    // `previous_node` borrows `ctx`. The style check has to finish before the tree is mutated.
    let reused = previous_node(ctx, entity).filter(|n| ctx.tree.style(*n).is_ok());
    let Some(node) = reused else {
        let node = ctx.tree.new_with_children(style, children)?;
        if context.is_some() {
            ctx.tree.set_node_context(node, context)?;
        }
        return Ok(node);
    };
    if ctx.tree.style(node)? != &style {
        ctx.tree.set_style(node, style)?;
    }
    if ctx.tree.children(node)? != children {
        ctx.tree.set_children(node, children)?;
    }
    if ctx.tree.get_node_context(node).map(|m| m.size) != context.map(|m| m.size) {
        ctx.tree.set_node_context(node, context)?;
    }
    Ok(node)
}

/// LengthPercentage + definite points (root padding): a percentage resolves against `basis` (the
/// window width) first, so a root's percentage padding survives the safe-area inset.
fn length_plus(d: taffy::style::LengthPercentage, extra: f32, basis: f32) -> taffy::style::LengthPercentage {
    use taffy::style::ExpandedLengthPercentage;
    match d.expand() {
        ExpandedLengthPercentage::Length(v) => taffy::style::LengthPercentage::length(v + extra),
        ExpandedLengthPercentage::Percent(p) => taffy::style::LengthPercentage::length(p * basis + extra),
        // `calc()` is never produced by `Style::to_taffy`.
        _ => taffy::style::LengthPercentage::length(extra),
    }
}

/// The axis a flex container stretches its items along (its cross axis).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CrossAxis {
    /// A column container: items stretch in width.
    Width,
    /// A row container: items stretch in height.
    Height,
}

/// How the parent lays a node out, for [`floor_flex_item_cross_min`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ItemCross {
    /// The parent's flex cross axis; `None` when the parent is not a flex container (or at a root).
    cross: Option<CrossAxis>,
    /// The parent was rebuilt and its cross axis differs from the previous run's, so a cached
    /// child's cross minimum is stale and the child must be rebuilt.
    moved: bool,
}

impl ItemCross {
    /// A root, or a node outside any flex container.
    const NONE: Self = Self {
        cross: None,
        moved: false,
    };
}

/// The cross axis `parent` stretches its children along, if it is a flex container.
fn flex_item_cross(parent: &taffy::style::Style) -> Option<CrossAxis> {
    use taffy::style::FlexDirection as D;
    if parent.display != taffy::style::Display::Flex {
        return None;
    }
    Some(match parent.flex_direction {
        D::Row | D::RowReverse => CrossAxis::Height,
        D::Column | D::ColumnReverse => CrossAxis::Width,
    })
}

/// Gives a flex item with an `Auto` minimum on its parent's cross axis the CSS used value there:
/// its own padding plus border (the length parts; a box is never smaller than them).
///
/// taffy 0.14 stretches an item to the line's cross size minus its margins without a lower bound
/// when the minimum is `Auto`, so margins wider than the line give a negative stretched size. The
/// laid-out box is floored at its padding and border later, but the cross-axis position was
/// already taken from the negative size. A left-anchored (LTR) position does not depend on the
/// size; a right-anchored one (an RTL column, where cross start is the right edge) does, and the
/// item lands its overflow on the wrong side (a zero-width node 6 px off its mirror, ADR-1411).
/// In CSS the automatic minimum on the cross axis is 0 (only the main axis gets the
/// content-based minimum) and no box is smaller than its padding and border, so the floor only
/// changes a stretched size that was below that. Percentage padding is left out (it resolves
/// against the parent's size, unknown here), and items with an aspect ratio are skipped (taffy
/// would transfer the minimum to the main axis through the ratio).
fn floor_flex_item_cross_min(style: &mut taffy::style::Style, cross: Option<CrossAxis>) {
    use taffy::style::ExpandedLengthPercentage;
    let Some(cross) = cross else {
        return;
    };
    if style.aspect_ratio.is_some() || style.position == taffy::style::Position::Absolute {
        return;
    }
    let edges = match cross {
        CrossAxis::Width => [
            style.padding.left,
            style.padding.right,
            style.border.left,
            style.border.right,
        ],
        CrossAxis::Height => [
            style.padding.top,
            style.padding.bottom,
            style.border.top,
            style.border.bottom,
        ],
    };
    let floor = if style.box_sizing == taffy::style::BoxSizing::BorderBox {
        edges
            .iter()
            .map(|e| match e.expand() {
                ExpandedLengthPercentage::Length(v) => v.max(0.0),
                _ => 0.0,
            })
            .sum()
    } else {
        0.0
    };
    let min = match cross {
        CrossAxis::Width => &mut style.min_size.width,
        CrossAxis::Height => &mut style.min_size.height,
    };
    if min.is_auto() {
        *min = taffy::style::LengthPercentageAuto::length(floor);
    }
}

/// Writeback helper.
struct Writer<'a> {
    entity_to_node: &'a HashMap<Entity, taffy::tree::NodeId>,
    tree: &'a taffy::TaffyTree<TextMeasure>,
    out: &'a mut Vec<(Entity, ComputedNode)>,
    /// The root's resolved UI scale (written to every node so text shapes at it).
    scale: f32,
    /// Next preorder index to hand out (paint order tie-break for hit testing).
    next_order: u32,
}

impl Writer<'_> {
    /// Walks the entity tree, writing [`ComputedNode`]s and returning this node's content extent
    /// (the scrollable content size: max child box extent including the node's own box).
    #[allow(clippy::too_many_arguments)]
    fn walk(
        &mut self,
        world: &World,
        entity: Entity,
        z_base: f64,
        depth: u32,
        parent_abs: [f32; 2],
        parent_clip: [f32; 4],
    ) -> [f32; 2] {
        let Some(node) = self.entity_to_node.get(&entity).copied() else {
            return [0.0; 2];
        };
        let Ok(layout) = self.tree.layout(node) else {
            return [0.0; 2];
        };
        // `Style::to_taffy` keeps NaN and infinities out of taffy; this is the last line of defense
        // so no rect downstream (hit testing, draw lists, `ui.query`) is ever non-finite.
        let x = parent_abs[0] + finite_or_zero(layout.location.x);
        let y = parent_abs[1] + finite_or_zero(layout.location.y);
        let (w, h) = (
            finite_non_negative(layout.size.width),
            finite_non_negative(layout.size.height),
        );
        let paint_order = self.next_order;
        self.next_order = self.next_order.saturating_add(1);
        let zoff = world.get::<ZOffset>(entity).map_or(0.0, |z| f64::from(z.0));
        let z = z_base + zoff + f64::from(depth) / 64.0;
        // Whole pixels, so scrolled rows stay crisp; the draw list uses these rects as they are.
        let scroll = world
            .get::<ScrollOffset>(entity)
            .map(|s| [finite_or_zero(s.0[0]).round(), finite_or_zero(s.0[1]).round()])
            .unwrap_or([0.0; 2]);
        let rect = [x, y, w, h];
        let hidden = world
            .get::<NodeStyle>(entity)
            .is_some_and(|s| s.0.display == Display::None);
        let clipping = !hidden
            && (world.get::<ClipChildren>(entity).is_some()
                || world.get::<crate::group_layer::RoundedClip>(entity).is_some()
                || world
                    .get::<NodeStyle>(entity)
                    .is_some_and(|s| s.0.overflow[0] == Overflow::Clip || s.0.overflow[1] == Overflow::Clip));
        let clip = if clipping {
            intersect(parent_clip, rect)
        } else {
            parent_clip
        };
        let scale = self.scale.max(0.001);
        let mut content = [w.max(0.0), h.max(0.0)];
        let child_origin = [x - scroll[0], y - scroll[1]];
        if let Some(children) = world.get::<Children>(entity) {
            for c in children.iter() {
                let extent = self.walk(world, c, z, depth + 1, child_origin, clip);
                // `walk` pushes a node after its descendants: the child's own rect is the last entry.
                let child_rect = self.out.last().map(|(_, n)| n.rect).unwrap_or(rect);
                // Child extent measured from this node's origin (scroll applied).
                let reach = child_reach([x, y], scroll, child_rect, extent);
                content[0] = content[0].max(reach[0]);
                content[1] = content[1].max(reach[1]);
            }
        }
        if hidden {
            self.out.push((
                entity,
                ComputedNode {
                    rect: [x, y, 0.0, 0.0],
                    clip_rect: [x, y, 0.0, 0.0],
                    z: z as f32,
                    content_size: [0.0; 2],
                    scale,
                    paint_order,
                },
            ));
            return [0.0; 2];
        }
        self.out.push((
            entity,
            ComputedNode {
                rect,
                clip_rect: clip,
                z: z as f32,
                content_size: content,
                scale,
                paint_order,
            },
        ));
        content
    }
}

/// How far a child's content reaches from its parent's origin `parent_xy`, with the parent's
/// `scroll` offset added back (so the result does not depend on the current scroll).
fn child_reach(parent_xy: [f32; 2], scroll: [f32; 2], child_rect: [f32; 4], extent: [f32; 2]) -> [f32; 2] {
    [
        child_rect[0] - parent_xy[0] + scroll[0] + extent[0],
        child_rect[1] - parent_xy[1] + scroll[1] + extent[1],
    ]
}

/// Intersection of two rects (x, y, w, h); empty boxes stay empty.
fn intersect(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    let x0 = a[0].max(b[0]);
    let y0 = a[1].max(b[1]);
    let x1 = (a[0] + a[2]).min(b[0] + b[2]);
    let y1 = (a[1] + a[3]).min(b[1] + b[3]);
    [x0, y0, (x1 - x0).max(0.0), (y1 - y0).max(0.0)]
}

/// After pass 1: re-shape texts whose final width differs from their measured width; update the
/// taffy measure contexts. Returns true when any measure changed (→ second compute pass).
fn refine_wrapped_text(
    world: &mut World,
    ctx: &mut UiLayoutContext,
    text_system: &mut TextSystem,
    loc: &crate::localization::Localization,
    scale: f32,
) -> bool {
    let mut changed = false;
    let entities: Vec<Entity> = world
        .query::<(Entity, &Text)>()
        .iter(world)
        .map(|(e, _)| e)
        .collect();
    for entity in entities {
        let Some(node) = ctx.entity_to_node.get(&entity).copied() else {
            continue;
        };
        let Ok(layout) = ctx.tree.layout(node) else {
            continue;
        };
        let final_w = layout.size.width;
        let (Some(text), Some(prev)) = (
            world.get::<Text>(entity),
            ctx.tree.get_node_context(node).map(|m| m.size),
        ) else {
            continue;
        };
        // Shape at the content width (matching `shape_text` and glyph placement in extract).
        let [pad_l, pad_r] = crate::text::pad_x_px(world.get::<NodeStyle>(entity), scale);
        let content_w = final_w - pad_l - pad_r;
        // Always re-shape when the final width differs: wrapping may change height, and
        // `WrapMode::None` still needs the width for RTL alignment inside the box.
        if (content_w - prev[0]).abs() < 0.75 {
            continue;
        }
        let wrap = text.wrap;
        let digest = crate::text::text_digest(text, loc.generation.0);
        let width = content_w.max(1.0);
        // Already shaped at this content, scale and width (by `shape_text` or a previous run)?
        let reuse = world.get::<crate::text::ShapedText>(entity).and_then(|s| {
            (s.digest == digest
                && (s.shaped_scale - scale).abs() < 1e-3
                && (s.shaped_width - width).abs() < 0.75)
                .then_some(s.size)
        });
        let new_size = match reuse {
            Some(size) => size,
            None => {
                let mut shaped = text_system.shape(text, scale, Some(width), loc);
                shaped.shaped_width = width;
                // The digest marks it current, so `shape_text` does not shape it again.
                shaped.digest = digest;
                let size = shaped.size;
                world.entity_mut(entity).insert(shaped);
                size
            }
        };
        if wrap != crate::text::WrapMode::None && (new_size[1] - prev[1]).abs() > 0.75 {
            changed = true;
            let _ = ctx
                .tree
                .set_node_context(node, Some(TextMeasure { size: new_size }));
        }
    }
    changed
}

/// Whether `entity` lays out right to left: the first explicit `text_dir` (`Ltr` or `Rtl`) on it
/// or an ancestor decides, else `locale_rtl` (whether the active locale is right to left; see
/// [`crate::style::locale_is_rtl`]). This is the inheritance layout applies (ADR-518);
/// `ui.query` reports it as `layout_dir`.
pub fn resolved_rtl(world: &World, entity: Entity, locale_rtl: bool) -> bool {
    let mut cur = Some(entity);
    while let Some(e) = cur {
        match world.get::<NodeStyle>(e).map(|s| s.0.text_dir) {
            Some(crate::style::TextDir::Rtl) => return true,
            Some(crate::style::TextDir::Ltr) => return false,
            _ => {}
        }
        cur = world.get::<ChildOf>(e).map(ChildOf::parent);
    }
    locale_rtl
}

/// System adapter that logs layout errors without panicking (bad user content must not crash).
pub fn log_layout_error(In(result): In<Result<(), taffy::TaffyError>>) {
    if let Err(err) = result {
        tracing::error!(target: "regia_ui", "layout failed: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_reach_uses_the_childs_own_rect() {
        // A padded column at (10, 20) holding 300x500 of content, in a parent at (0, 0)
        // scrolled by 40: the reach is the column origin plus its content, scroll added back.
        let reach = child_reach(
            [0.0, 0.0],
            [0.0, 40.0],
            [10.0, -20.0, 280.0, 400.0],
            [300.0, 500.0],
        );
        assert_eq!(reach, [310.0, 520.0]);
    }

    #[test]
    fn insets_are_sanitized_and_fit_the_window() {
        assert_eq!(
            fit_insets([10.0, 20.0, 30.0, 40.0], [1000.0, 600.0]),
            [10.0, 20.0, 30.0, 40.0]
        );
        assert_eq!(
            fit_insets([f32::NAN, -5.0, f32::INFINITY, 8.0], [1000.0, 600.0]),
            [0.0, 0.0, 0.0, 8.0]
        );
        // A pair wider than the window keeps its proportions and exactly fills it.
        let fitted = fit_insets([300.0, 0.0, 900.0, 0.0], [600.0, 400.0]);
        assert_eq!(fitted, [150.0, 0.0, 450.0, 0.0]);
        // A zero-size window leaves no inset.
        assert_eq!(fit_insets([5.0, 5.0, 5.0, 5.0], [0.0, 0.0]), [0.0; 4]);
    }

    #[test]
    fn root_percent_padding_survives_the_safe_area() {
        use taffy::style::LengthPercentage as LP;
        assert_eq!(length_plus(LP::percent(0.1), 0.0, 800.0), LP::length(80.0));
        assert_eq!(length_plus(LP::percent(0.1), 24.0, 800.0), LP::length(104.0));
        assert_eq!(length_plus(LP::length(8.0), 24.0, 800.0), LP::length(32.0));
    }

    #[test]
    fn intersect_basics() {
        let a = [0.0, 0.0, 100.0, 100.0];
        let b = [50.0, 50.0, 100.0, 100.0];
        assert_eq!(intersect(a, b), [50.0, 50.0, 50.0, 50.0]);
        let c = [200.0, 200.0, 10.0, 10.0];
        assert_eq!(intersect(a, c)[2], 0.0);
        let none = intersect(NONE_CLIP, a);
        assert_eq!(none, a);
    }

    #[test]
    fn a_flex_item_cross_minimum_floors_at_its_length_padding_and_border() {
        use taffy::style::{LengthPercentage as LP, LengthPercentageAuto as LPA};
        let column = taffy::style::Style {
            flex_direction: taffy::style::FlexDirection::Column,
            ..Default::default()
        };
        assert_eq!(flex_item_cross(&column), Some(CrossAxis::Width));
        let row = taffy::style::Style::default();
        assert_eq!(flex_item_cross(&row), Some(CrossAxis::Height));
        let grid = taffy::style::Style {
            display: taffy::style::Display::Grid,
            ..Default::default()
        };
        assert_eq!(flex_item_cross(&grid), None);

        let mut item = taffy::style::Style::default();
        item.padding.left = LP::length(6.0);
        item.padding.right = LP::percent(0.5);
        item.border.right = LP::length(2.0);
        item.padding.top = LP::length(9.0);
        floor_flex_item_cross_min(&mut item, Some(CrossAxis::Width));
        // Lengths only (the percentage resolves against an unknown parent); the main axis keeps
        // its automatic (content-based) minimum.
        assert_eq!(item.min_size.width, LPA::length(8.0));
        assert!(item.min_size.height.is_auto());

        let mut authored = taffy::style::Style::default();
        authored.min_size.width = LPA::length(3.0);
        floor_flex_item_cross_min(&mut authored, Some(CrossAxis::Width));
        assert_eq!(
            authored.min_size.width,
            LPA::length(3.0),
            "an authored minimum stays"
        );

        let mut ratio = taffy::style::Style {
            aspect_ratio: Some(2.0),
            ..Default::default()
        };
        floor_flex_item_cross_min(&mut ratio, Some(CrossAxis::Height));
        assert!(ratio.min_size.height.is_auto(), "aspect-ratio items are skipped");

        let mut outside = taffy::style::Style::default();
        floor_flex_item_cross_min(&mut outside, None);
        assert!(outside.min_size.width.is_auto() && outside.min_size.height.is_auto());
    }
}
