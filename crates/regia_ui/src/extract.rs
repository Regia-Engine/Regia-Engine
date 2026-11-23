// SPDX-License-Identifier: MPL-2.0
//! Extract (`specs/18` § 3): walks every UI root's tree in draw order, rasterizes shaped glyphs
//! into the atlas, and builds the sorted quad list the overlay draws. Rects and glyphs are
//! CPU-clipped to their ancestor clip rects; the focus ring and scrollbars are emitted here too.

use std::collections::{HashMap, HashSet};

use regia_app::prelude::*;

use crate::atlas::PAGE_SIZE;
use crate::color::UiColor;
use crate::draw::{self, NO_CLIP, TextureRef, UiQuad, intersect};
use crate::focus::{DEFAULT_FOCUS_RING_WIDTH, focus_ring_geometry};
use crate::node::{
    Background, BackgroundKind, Border, ClipChildren, ComputedNode, Opacity, Shadow, UiCanvas, UiImage,
    UiRoot,
};
use crate::render::{SharedUiRenderer, UiRenderer};
use crate::scale::UiScales;
use crate::scroll::{ScrollOffset, ScrollView};
use crate::text::{ShapedText, TextOverflow, TextSystem};
use crate::theme::Themed;
use crate::widgets::{ProgressBar, Slider, Toggle};

/// Whether the UI changed since the last extract (set by [`detect_ui_damage`]; extract keeps
/// the previous draw list when clean). [`UiDamage::mark`] forces a rebuild.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq)]
pub struct UiDamage(pub bool);

impl Default for UiDamage {
    fn default() -> Self {
        Self(true)
    }
}

impl UiDamage {
    /// Forces the next extract to rebuild the draw list.
    pub fn mark(&mut self) {
        self.0 = true;
    }
}

/// Which entities changed since the last extract (filled by [`detect_ui_damage`]); `full` when
/// something was removed (removed entities cannot be traced to their subtree).
#[derive(Resource, Debug, Default)]
pub struct UiDamageSet {
    /// Rebuild every subtree.
    pub full: bool,
    /// Changed entities (mapped to their root's top-level subtree by extract).
    pub entities: Vec<Entity>,
}

/// `PostUpdate`, after layout and shaping: record what extract must redraw — the changed
/// entities (geometry, text, visuals, widget state, hierarchy, scroll), or a full rebuild when
/// anything UI-related was removed — and flag [`UiDamage`].
pub fn detect_ui_damage(
    mut damage: ResMut<UiDamage>,
    mut set: ResMut<UiDamageSet>,
    changed: Query<
        Entity,
        Or<(
            Or<(
                Changed<ComputedNode>,
                Changed<ShapedText>,
                Changed<crate::text::Text>,
                Changed<crate::node::NodeStyle>,
                Changed<Background>,
                Changed<Border>,
                Changed<Shadow>,
                Changed<crate::node::BackdropBlur>,
                Changed<UiImage>,
                Changed<Opacity>,
                Changed<Themed>,
                Changed<crate::ui_material::UiMaterial>,
            )>,
            Or<(
                Changed<ClipChildren>,
                Changed<crate::node::ZOffset>,
                Changed<ScrollOffset>,
                Changed<ScrollView>,
                Changed<Toggle>,
                Changed<Slider>,
                Changed<ProgressBar>,
                Changed<Children>,
                Changed<UiRoot>,
                Changed<UiCanvas>,
                Changed<crate::group_layer::GroupOpacity>,
                Changed<crate::group_layer::RoundedClip>,
                Changed<crate::group_layer::UiMask>,
            )>,
        )>,
    >,
    mut removed: (
        RemovedComponents<crate::node::UiNode>,
        RemovedComponents<UiRoot>,
        RemovedComponents<Background>,
        RemovedComponents<Border>,
        RemovedComponents<Shadow>,
        RemovedComponents<UiImage>,
        RemovedComponents<Opacity>,
        RemovedComponents<ShapedText>,
        RemovedComponents<Themed>,
        RemovedComponents<ClipChildren>,
        RemovedComponents<UiCanvas>,
        RemovedComponents<crate::node::BackdropBlur>,
        RemovedComponents<crate::ui_material::UiMaterial>,
        RemovedComponents<crate::group_layer::GroupOpacity>,
        RemovedComponents<crate::group_layer::RoundedClip>,
        RemovedComponents<crate::group_layer::UiMask>,
    ),
) {
    let _span = tracing::debug_span!("detect_ui_damage").entered();
    let any_removed = removed.0.read().count()
        + removed.1.read().count()
        + removed.2.read().count()
        + removed.3.read().count()
        + removed.4.read().count()
        + removed.5.read().count()
        + removed.6.read().count()
        + removed.7.read().count()
        + removed.8.read().count()
        + removed.9.read().count()
        + removed.10.read().count()
        + removed.11.read().count()
        + removed.12.read().count()
        + removed.13.read().count()
        + removed.14.read().count()
        + removed.15.read().count()
        > 0;
    if any_removed {
        set.full = true;
        damage.0 = true;
    }
    let before = set.entities.len();
    set.entities.extend(changed.iter());
    if set.entities.len() > before {
        damage.0 = true;
    }
}

/// Per-subtree quad cache: the quads of each top-level child of every root, from the last
/// extract that walked it.
#[derive(Resource, Default)]
struct ExtractCache {
    subtrees: HashMap<Entity, Vec<UiQuad>>,
    /// The group-layer decisions of each cached subtree (ADR-2866).
    plans: HashMap<Entity, crate::group_layer::LayerPlan>,
    /// The layer budget the cache was built under (a change rebuilds everything).
    budget: Option<crate::group_layer::UiLayerBudget>,
    /// The window size the cache was built under: layers are cut to the frame, so a resize that
    /// moves no node still rebuilds the groups (ADR-2866).
    window: Option<[f32; 2]>,
    /// Image registrations seen (9-slice splits depend on image sizes).
    images_generation: u64,
    /// Debug-overlay and soft-cursor quads appended after the sort (the window list's tail).
    overlay_len: usize,
    /// Video seek sliders whose chrome is patched on a clean frame.
    seek_slots: Vec<SeekSlot>,
}

/// Chrome of one video seek slider, so a clean frame can move the thumb without walking the subtree.
struct SeekSlot {
    entity: Entity,
    /// Top-level child the quads live in. `None` when the slider is the root itself.
    subtree: Option<Entity>,
    /// Index of the chrome in that subtree's cached quad list (unused for a root slider).
    start: usize,
    count: usize,
    fraction: f32,
    rect: [f32; 4],
    scale: f32,
    themed: Option<Themed>,
    clip: [f32; 4],
    z: f32,
    alpha: f32,
    /// Position and size of each emitted chrome quad, for the sorted draw list.
    drawn: Vec<([f32; 2], [f32; 2])>,
}

/// The top-level subtree (child of a root) `entity` belongs to: `Ok(Some(child))`, `Ok(None)`
/// when `entity` is not under any root, `Err(())` when it is a root itself.
fn subtree_of(world: &World, mut entity: Entity) -> Result<Option<Entity>, ()> {
    if world.get::<UiRoot>(entity).is_some() {
        return Err(());
    }
    // Same bound as `in_scope`: a corrupt `ChildOf` cycle must not spin extract. Past the bound
    // the entity is not under a root (`Ok(None)`), so it does not force a full rebuild.
    for _ in 0..crate::focus::MAX_ANCESTOR_STEPS {
        let Some(parent) = world.get::<ChildOf>(entity).map(ChildOf::parent) else {
            return Ok(None);
        };
        if world.get::<UiRoot>(parent).is_some() {
            return Ok(Some(entity));
        }
        entity = parent;
    }
    Ok(None)
}

/// Seconds of material time the last tick stored, then cleared so a second extract does not add them again.
fn take_material_dt(world: &mut World) -> f32 {
    let Some(mut tick) = world.get_resource_mut::<crate::ui_material::MaterialTick>() else {
        return 0.0;
    };
    let dt = tick.dt;
    tick.dt = 0.0;
    dt
}

/// A frame with no widget damage: advance material time, move video seek thumbs, and refresh the
/// overlay tail (debug overlay and the soft cursor). The cached subtrees are not walked.
fn refresh_clean_frame(world: &mut World, renderer: &SharedUiRenderer, cache: &mut ExtractCache) {
    let dt = take_material_dt(world);
    if dt > 0.0 {
        renderer.lock().patch_material_angles(dt);
        for sub in cache.subtrees.values_mut() {
            UiRenderer::patch_material_quads(sub, dt);
        }
    }
    patch_seek_slots(world, renderer, cache);
    let mut overlay = crate::debug_overlay::collect_overlay(world);
    overlay.extend(crate::soft_cursor::cursor_quads(world));
    if overlay.is_empty() && cache.overlay_len == 0 {
        return;
    }
    renderer.lock().replace_main_tail(cache.overlay_len, &overlay);
    cache.overlay_len = overlay.len();
}

/// Moves video seek chrome to the slider's current fraction when the quad count is unchanged.
fn patch_seek_slots(world: &World, renderer: &SharedUiRenderer, cache: &mut ExtractCache) {
    let n = cache.seek_slots.len();
    for i in 0..n {
        let entity = cache.seek_slots[i].entity;
        let Some(slider) = world.get::<Slider>(entity) else {
            continue;
        };
        let fraction = slider.fraction();
        if (fraction - cache.seek_slots[i].fraction).abs() <= f32::EPSILON {
            continue;
        }
        let start = cache.seek_slots[i].start;
        let count = cache.seek_slots[i].count;
        let drawn = cache.seek_slots[i].drawn.clone();
        let subtree = cache.seek_slots[i].subtree;
        let mut fresh = Vec::new();
        push_slider_chrome(
            &mut fresh,
            cache.seek_slots[i].rect,
            cache.seek_slots[i].scale,
            fraction,
            cache.seek_slots[i].themed,
            cache.seek_slots[i].clip,
            cache.seek_slots[i].z,
            cache.seek_slots[i].alpha,
        );
        if fresh.len() != count {
            continue;
        }
        if let Some(sub_entity) = subtree
            && let Some(sub) = cache.subtrees.get_mut(&sub_entity)
        {
            let end = start + count;
            let matches = end <= sub.len()
                && sub[start..end]
                    .iter()
                    .zip(drawn.iter())
                    .all(|(quad, prev)| quad.pos == prev.0 && quad.size == prev.1);
            if matches {
                sub[start..end].copy_from_slice(&fresh);
            }
        }
        renderer.lock().replace_quads_matching(&drawn, &fresh);
        cache.seek_slots[i].fraction = fraction;
        cache.seek_slots[i].drawn = fresh.iter().map(|quad| (quad.pos, quad.size)).collect();
    }
}

/// `Extract`: rebuild the draw list when the UI changed — only the top-level subtrees that
/// contain a changed entity are walked again; the rest reuse their cached quads. A clean frame
/// keeps the renderer's previous list.
pub fn extract_ui(world: &mut World) {
    let _span = tracing::trace_span!("ui_extract").entered();
    let Some(renderer) = world.remove_resource::<SharedUiRenderer>() else {
        return;
    };
    let mut cache = world.remove_resource::<ExtractCache>().unwrap_or_default();
    // Camera and world targets (ADR-2406): placements, sizes and panel transforms, every frame.
    crate::world_target::refresh_targets(world, &renderer);
    let images_generation = renderer.lock().images_generation();
    let damaged = world.get_resource::<UiDamage>().is_none_or(|d| d.0);
    // A new group-layer budget (ADR-2866) changes which groups get a layer: rebuild.
    let budget = world
        .get_resource::<crate::group_layer::UiLayerBudget>()
        .copied()
        .unwrap_or_default();
    let window = world.get_resource::<UiScales>().map(|s| s.window_size);
    if !damaged
        && images_generation == cache.images_generation
        && cache.budget == Some(budget)
        && cache.window == window
    {
        refresh_clean_frame(world, &renderer, &mut cache);
        if let Some(mut counts) = world.get_resource_mut::<crate::layout::UiHotPathCounts>() {
            // A clean frame walks nothing; the cached subtrees are not visited.
            counts.extract_nodes_walked = 0;
        }
        world.insert_resource(cache);
        world.insert_resource(renderer);
        return;
    }
    // A rebuild reads material time off the component, which this frame's tick already advanced,
    // but only for the subtrees it walks. Material time and video seek sliders are written past
    // change detection, so the cached subtrees this rebuild reuses still need this frame's delta
    // and seek positions, or an animated panel freezes while another subtree redraws each frame.
    let dt = take_material_dt(world);
    if dt > 0.0 {
        for sub in cache.subtrees.values_mut() {
            UiRenderer::patch_material_quads(sub, dt);
        }
    }
    patch_seek_slots(world, &renderer, &mut cache);
    let Some(mut text_system) = world.remove_resource::<TextSystem>() else {
        world.insert_resource(cache);
        world.insert_resource(renderer);
        return;
    };
    let set = world
        .get_resource_mut::<UiDamageSet>()
        .map(|mut s| std::mem::take(&mut *s))
        .unwrap_or(UiDamageSet {
            full: true,
            entities: Vec::new(),
        });
    let mut full = set.full
        || images_generation != cache.images_generation
        || cache.subtrees.is_empty()
        || cache.budget != Some(budget)
        || cache.window != window;
    cache.budget = Some(budget);
    cache.window = window;
    let mut dirty: HashSet<Entity> = HashSet::new();
    if !full {
        for e in &set.entities {
            match subtree_of(world, *e) {
                Ok(Some(sub)) => {
                    dirty.insert(sub);
                }
                Ok(None) => {}
                Err(()) => {
                    full = true;
                    break;
                }
            }
        }
    }
    let mut walked = 0u64;
    {
        // One lock for the whole rebuild: the glyph atlas and image sizes live in the renderer.
        let mut guard = renderer.lock();
        let wipes_before = guard.atlas.wipes();
        // Pages this rebuild draws from are pinned until it ends: a glyph that needs a page never
        // recycles one this frame's quads already point into (ADR-8832).
        guard.atlas.begin_frame();
        let mut quads = guard.take_quads();
        let mut targets = crate::world_target::TargetLists::new();
        let mut attempt_full = full;
        loop {
            quads.clear();
            targets.clear();
            build_quads(
                world,
                &mut text_system,
                &mut guard,
                &mut cache,
                attempt_full,
                &dirty,
                &mut quads,
                &mut targets,
                &mut walked,
            );
            // A glyph-atlas page recycled mid-rebuild invalidates cached subtrees that sample it.
            if attempt_full || guard.atlas.wipes() == wipes_before {
                break;
            }
            attempt_full = true;
        }
        guard.atlas.end_frame();
        guard.set_quads(quads);
        if !guard.targets.is_empty() {
            guard.set_target_quads(targets);
        }
        // Sampled under the same lock as the rebuild: an image queued by another thread between
        // the early read and here is already drawn, and must not be re-detected next frame.
        cache.images_generation = guard.images_generation();
    }
    if let Some(mut counts) = world.get_resource_mut::<crate::layout::UiHotPathCounts>() {
        counts.extract_nodes_walked = walked;
    }
    if let Some(mut damage) = world.get_resource_mut::<UiDamage>() {
        damage.0 = false;
    }
    world.insert_resource(cache);
    world.insert_resource(text_system);
    world.insert_resource(renderer);
}

/// Assembles the sorted draw lists from every root: the root's own quads, then each top-level
/// subtree from the cache or a fresh walk. Window roots (and window cameras' roots, offset to
/// their viewport) go to `quads`; camera-texture and world roots to their target's list in
/// `targets` (ADR-2406).
fn build_quads(
    world: &mut World,
    text_system: &mut TextSystem,
    renderer: &mut UiRenderer,
    cache: &mut ExtractCache,
    full: bool,
    dirty: &HashSet<Entity>,
    quads: &mut Vec<UiQuad>,
    targets: &mut crate::world_target::TargetLists,
    walked: &mut u64,
) {
    *walked = 0;
    let mut seek_slots = std::mem::take(&mut cache.seek_slots);
    if full {
        seek_slots.clear();
    } else {
        // The root is walked every rebuild, so its slots are recorded again below.
        seek_slots.retain(|slot| slot.subtree.is_some());
    }
    let mut roots: Vec<(i32, Entity)> = world
        .query::<(Entity, &UiRoot)>()
        .iter(world)
        .map(|(e, r)| (r.sort_order, e))
        .collect();
    let active = crate::output::active_variant(world);
    roots.retain(|(_, e)| crate::output::root_active(world, *e, active));
    roots.sort_by_key(|(order, e)| (*order, e.to_bits()));
    let mut overlay = crate::debug_overlay::collect_overlay(world);
    overlay.extend(crate::soft_cursor::cursor_quads(world));
    let world: &World = world;
    let mut live: HashSet<Entity> = HashSet::new();
    // Group layers (ADR-2866): one budget across every list of this rebuild.
    let budget = cache.budget.unwrap_or_default().max_bytes;
    let mut account = LayerAccount {
        left: budget,
        plan: crate::group_layer::LayerPlan::default(),
        frame: NO_CLIP,
    };
    let scales = world.get_resource::<UiScales>().copied();
    for (order, root) in roots {
        let placement = crate::world_target::root_placement(world, root);
        let (origin, clip) = crate::world_target::walk_frame(placement);
        // A layer never covers more than its list can show.
        account.frame = match (clip == NO_CLIP, scales) {
            (false, _) => clip,
            (true, Some(scales)) => {
                let (size, _) = crate::scale::resolve_window_size(world, root, &scales);
                if size[0] > 0.0 && size[1] > 0.0 {
                    [0.0, 0.0, size[0], size[1]]
                } else {
                    NO_CLIP
                }
            }
            (true, None) => NO_CLIP,
        };
        let out: &mut Vec<UiQuad> = match placement {
            crate::world_target::Placement::Hidden => continue,
            crate::world_target::Placement::Target(key) => targets.entry(key).or_default(),
            _ => &mut *quads,
        };
        let Some(ctx) = walk(
            world,
            text_system,
            renderer,
            root,
            f64::from(order),
            origin,
            clip,
            1.0,
            false,
            &mut account,
            out,
            None,
            &mut seek_slots,
            walked,
        ) else {
            continue;
        };
        let Some(children) = world.get::<Children>(root) else {
            continue;
        };
        for child in children.iter() {
            live.insert(child);
            // A child fully outside the clip emits nothing this frame: drop its cached subtree
            // too, so a later clean frame cannot replay quads the clip now throws away.
            if offscreen(world, child, &ctx) {
                cache.subtrees.remove(&child);
                cache.plans.remove(&child);
                seek_slots.retain(|slot| slot.subtree != Some(child));
                continue;
            }
            let cached = cache
                .subtrees
                .get_mut(&child)
                .filter(|_| !full && !dirty.contains(&child));
            match cached {
                Some(sub) => {
                    out.extend_from_slice(sub);
                    let plan = cache.plans.get(&child).copied().unwrap_or_default();
                    account.left = account.left.saturating_sub(plan.bytes);
                    account.plan.add(&plan);
                }
                None => {
                    renderer.note_subtree_walk();
                    seek_slots.retain(|slot| slot.subtree != Some(child));
                    let mut sub = cache.subtrees.remove(&child).unwrap_or_default();
                    sub.clear();
                    let mut sub_account = LayerAccount {
                        left: account.left,
                        plan: crate::group_layer::LayerPlan::default(),
                        frame: account.frame,
                    };
                    walk(
                        world,
                        text_system,
                        renderer,
                        child,
                        ctx.z_base,
                        ctx.origin,
                        ctx.clip,
                        ctx.alpha,
                        true,
                        &mut sub_account,
                        &mut sub,
                        Some(child),
                        &mut seek_slots,
                        walked,
                    );
                    account.left = sub_account.left;
                    account.plan.add(&sub_account.plan);
                    out.extend_from_slice(&sub);
                    cache.subtrees.insert(child, sub);
                    cache.plans.insert(child, sub_account.plan);
                }
            }
        }
    }
    cache.subtrees.retain(|e, _| live.contains(e));
    cache.plans.retain(|e, _| live.contains(e));
    seek_slots.retain(|slot| slot.subtree.is_none_or(|entity| live.contains(&entity)));
    cache.seek_slots = seek_slots;
    cache.overlay_len = overlay.len();
    renderer.layer_plan = account.plan;
    // Stable sort by z (root order + `ZOffset`): preorder paint order breaks ties.
    quads.sort_by(|a, b| a.z.partial_cmp(&b.z).unwrap_or(std::cmp::Ordering::Equal));
    for list in targets.values_mut() {
        list.sort_by(|a, b| a.z.partial_cmp(&b.z).unwrap_or(std::cmp::Ordering::Equal));
    }
    // The layout debug overlay (U02) goes on top of everything, uncached.
    quads.extend(overlay);
}

/// The group-layer budget left in this rebuild, and what was decided so far (ADR-2866).
#[derive(Debug, Clone, Copy)]
struct LayerAccount {
    left: u64,
    plan: crate::group_layer::LayerPlan,
    /// What the current list can show: every layer is cut to it.
    frame: [f32; 4],
}

impl LayerAccount {
    /// Wraps `out[start..]` as one group and records the decision.
    fn wrap(&mut self, out: &mut Vec<UiQuad>, start: usize, spec: &crate::group_layer::GroupSpec) {
        let spec = crate::group_layer::GroupSpec {
            limit: intersect(spec.limit, self.frame),
            ..*spec
        };
        let outcome = crate::group_layer::wrap_group(out, start, &spec, self.left);
        if let crate::group_layer::WrapOutcome::Layer(bytes) = outcome {
            self.left = self.left.saturating_sub(bytes);
        }
        outcome.record(&mut self.plan);
    }
}

/// What a node passes down to its children.
#[derive(Debug, Clone, Copy)]
struct ChildCtx {
    z_base: f64,
    origin: [f32; 2],
    clip: [f32; 4],
    alpha: f32,
}

/// `true` when `rect` (x, y, w, h) misses `clip`.
fn misses(rect: [f32; 4], clip: [f32; 4]) -> bool {
    let x0 = rect[0].max(clip[0]);
    let y0 = rect[1].max(clip[1]);
    let x1 = (rect[0] + rect[2]).min(clip[0] + clip[2]);
    let y1 = (rect[1] + rect[3]).min(clip[1] + clip[3]);
    x1 <= x0 || y1 <= y0
}

/// Union of two rects (x, y, w, h).
fn union_rect(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    let x0 = a[0].min(b[0]);
    let y0 = a[1].min(b[1]);
    let x1 = (a[0] + a[2]).max(b[0] + b[2]);
    let y1 = (a[1] + a[3]).max(b[1] + b[3]);
    [x0, y0, (x1 - x0).max(0.0), (y1 - y0).max(0.0)]
}

/// `true` when walking `entity` would emit nothing this frame, or its clipped subtree cannot
/// meet `ctx.clip`. A parent that does not clip its children is kept: an absolute child, or a
/// child with a negative `ZOffset`, can still land inside the clip. Skipping off-screen
/// children is what keeps a long scrolled list's extract walk on the order of the visible
/// rows instead of every row.
fn offscreen(world: &World, entity: Entity, ctx: &ChildCtx) -> bool {
    // `NO_CLIP` and any unbounded clip: the miss test is not meaningful.
    if !ctx.clip[2].is_finite()
        || !ctx.clip[3].is_finite()
        || ctx.clip[2] >= f32::MAX / 4.0
        || ctx.clip[3] >= f32::MAX / 4.0
        || ctx.clip == NO_CLIP
    {
        return false;
    }
    let Some(computed) = world.get::<ComputedNode>(entity) else {
        return true;
    };
    let style = world.get::<crate::node::NodeStyle>(entity).map(|s| &s.0);
    if style
        .as_ref()
        .is_some_and(|s| s.display == crate::style::Display::None)
    {
        return true;
    }
    let rect = [
        computed.rect[0] + ctx.origin[0],
        computed.rect[1] + ctx.origin[1],
        computed.rect[2],
        computed.rect[3],
    ];
    if rect[2] <= 0.0 || rect[3] <= 0.0 {
        return true;
    }
    let scale = computed.scale.max(0.001);
    // Shadow quads extend past the caster (`UiQuad::shadow_margin` is `(blur * 1.5).ceil()`).
    let mut expanded = rect;
    if let Some(shadow) = world.get::<Shadow>(entity) {
        let margin = ((shadow.blur * scale).max(0.01) * 1.5).ceil();
        let offset = [shadow.offset[0] * scale, shadow.offset[1] * scale];
        expanded = union_rect(
            expanded,
            [
                rect[0] + offset[0] - margin,
                rect[1] + offset[1] - margin,
                rect[2] + margin * 2.0,
                rect[3] + margin * 2.0,
            ],
        );
    }
    // Focus ring grows by `offset + width` outside the border box (negative offset stays inside).
    if let Some(themed) = world.get::<Themed>(entity)
        && themed.focus_visible
        && themed.focus_ring.is_some()
    {
        let grow = themed.focus_ring_offset.unwrap_or(0.0) * scale
            + themed.focus_ring_width.unwrap_or(DEFAULT_FOCUS_RING_WIDTH) * scale;
        if grow > 0.0 {
            expanded = union_rect(
                expanded,
                [
                    rect[0] - grow,
                    rect[1] - grow,
                    rect[2] + grow * 2.0,
                    rect[3] + grow * 2.0,
                ],
            );
        }
    }
    if !misses(expanded, ctx.clip) {
        return false;
    }
    let has_children = world
        .get::<Children>(entity)
        .is_some_and(|children| !children.is_empty());
    if !has_children {
        return true;
    }
    world.get::<ClipChildren>(entity).is_some()
        || world.get::<crate::group_layer::RoundedClip>(entity).is_some()
        || style.is_some_and(|s| {
            s.overflow[0] == crate::style::Overflow::Clip || s.overflow[1] == crate::style::Overflow::Clip
        })
}

/// Emits `entity`'s quads into `out` and, when `descend`, its subtree's. Returns the context its
/// children are drawn with (`None` when the node draws nothing: no layout, hidden or empty).
#[allow(clippy::too_many_arguments)]
fn walk(
    world: &World,
    text_system: &mut TextSystem,
    renderer: &mut UiRenderer,
    entity: Entity,
    z_base: f64,
    origin: [f32; 2],
    clip: [f32; 4],
    parent_alpha: f32,
    descend: bool,
    account: &mut LayerAccount,
    out: &mut Vec<UiQuad>,
    cache_key: Option<Entity>,
    seek_slots: &mut Vec<SeekSlot>,
    walked: &mut u64,
) -> Option<ChildCtx> {
    *walked += 1;
    let computed = world.get::<ComputedNode>(entity)?;
    let style = world.get::<crate::node::NodeStyle>(entity).map(|s| &s.0);
    if style
        .as_ref()
        .is_some_and(|s| s.display == crate::style::Display::None)
    {
        return None;
    }
    let rect = [
        computed.rect[0] + origin[0],
        computed.rect[1] + origin[1],
        computed.rect[2],
        computed.rect[3],
    ];
    if rect[2] <= 0.0 || rect[3] <= 0.0 {
        return None;
    }
    let zoff = world
        .get::<crate::node::ZOffset>(entity)
        .map_or(0.0, |z| f64::from(z.0));
    // Paint order is tree preorder (later siblings and their subtrees over earlier ones, like
    // CSS); `ZOffset` raises a whole subtree. Extract emits quads in preorder and the sort is
    // stable, so `z` only carries root order + explicit offsets.
    let z_base = z_base + zoff;
    let z = z_base as f32;
    // Isolated groups and image masks (ADR-2866) draw the node's subtree into a layer: inside it
    // the content starts at full alpha, and the layer composites at the inherited alpha times
    // the group's opacity. Roots draw their children as separate subtrees, so a root is never a
    // group itself.
    let group_opacity = world
        .get::<crate::group_layer::GroupOpacity>(entity)
        .map(|g| if g.0.is_finite() { g.0.clamp(0.0, 1.0) } else { 1.0 });
    let image_mask = world.get::<crate::group_layer::UiMask>(entity).copied();
    let layer_node = descend && (group_opacity.is_some() || image_mask.is_some());
    let group_start = out.len();
    let inherited = if layer_node { 1.0 } else { parent_alpha };
    let alpha = inherited
        * world.get::<Opacity>(entity).map_or(1.0, |o| o.0.clamp(0.0, 1.0))
        * world
            .get::<Themed>(entity)
            .map_or(1.0, |t| t.opacity.unwrap_or(1.0).clamp(0.0, 1.0));
    let rounded_clip = world.get::<crate::group_layer::RoundedClip>(entity).is_some();
    let clipping = world.get::<ClipChildren>(entity).is_some()
        || rounded_clip
        || style.as_ref().is_some_and(|s| {
            s.overflow[0] == crate::style::Overflow::Clip || s.overflow[1] == crate::style::Overflow::Clip
        });
    let child_clip = if clipping { intersect(clip, rect) } else { clip };
    let scale = computed.scale.max(0.001);

    let themed = world.get::<Themed>(entity).copied();

    // Shadow first (behind everything): the caster's own rounded rect, offset and blurred.
    if let Some(shadow) = world.get::<Shadow>(entity) {
        let offset = [shadow.offset[0] * scale, shadow.offset[1] * scale];
        let radius = themed
            .and_then(|t| t.radius)
            .map(|r| r * scale)
            .or_else(|| border_radius(world, entity, scale))
            .unwrap_or(0.0);
        let quad = UiQuad {
            color: shadow.color,
            corner_radius: [radius; 4],
            blur: (shadow.blur * scale).max(0.01),
            shadow_offset: offset,
            alpha,
            z,
            ..UiQuad::fill(
                [rect[0] + offset[0], rect[1] + offset[1]],
                [rect[2], rect[3]],
                shadow.color,
            )
        };
        if let Some(clipped) = quad.clipped(clip) {
            out.push(UiQuad { z, alpha, ..clipped });
        }
    }

    // Frosted glass: the blurred frame under the rect, before the tint paints over it.
    if let Some(blur) = world.get::<crate::node::BackdropBlur>(entity)
        && blur.radius > 0.0
    {
        let quad = UiQuad {
            texture: TextureRef::Backdrop,
            blur: (blur.radius * scale).clamp(0.0, crate::draw::MAX_BACKDROP_SIGMA),
            ..UiQuad::fill([rect[0], rect[1]], [rect[2], rect[3]], UiColor([1.0; 4]))
        };
        let quad = apply_radius(quad, world, entity, themed, scale);
        if let Some(clipped) = quad.clipped(clip) {
            out.push(UiQuad { z, alpha, ..clipped });
        }
    }

    // Background (explicit component beats theme-resolved).
    let bg_color = themed
        .and_then(|t| t.background)
        .or_else(|| match world.get::<Background>(entity) {
            Some(Background {
                kind: BackgroundKind::Color(c),
            }) => Some(*c),
            _ => None,
        });
    if let Some(color) = bg_color
        && color.0[3] > 0.0
    {
        let quad = UiQuad::fill([rect[0], rect[1]], [rect[2], rect[3]], color);
        let quad = apply_radius(quad, world, entity, themed, scale);
        if let Some(clipped) = quad.clipped(clip) {
            out.push(UiQuad { z, alpha, ..clipped });
        }
    }
    if let Some(Background {
        kind: BackgroundKind::Gradient { start, end, angle },
    }) = world.get::<Background>(entity)
    {
        // Explicit gradient quad: kind 4 lerps `color`→`color2` across the rect in the shader.
        let quad = UiQuad {
            color: *start,
            color2: *end,
            gradient: true,
            angle: *angle,
            ..UiQuad::fill([rect[0], rect[1]], [rect[2], rect[3]], *start)
        };
        let quad = apply_radius(quad, world, entity, themed, scale);
        if let Some(clipped) = quad.clipped(clip) {
            out.push(UiQuad { z, alpha, ..clipped });
        }
    }
    if let Some(Background {
        kind: BackgroundKind::Image { id, uv, nine_slice },
    }) = world.get::<Background>(entity)
    {
        let base = UiQuad {
            alpha,
            z,
            ..UiQuad::fill([rect[0], rect[1]], [rect[2], rect[3]], UiColor::WHITE)
        };
        let image_size = renderer.image_size(*id);
        match (nine_slice, image_size) {
            (Some(insets), Some(px)) => {
                for quad in draw::nine_slice(base, *id, *uv, *insets, px, scale) {
                    if let Some(clipped) = quad.clipped(clip) {
                        out.push(UiQuad { z, alpha, ..clipped });
                    }
                }
            }
            _ => {
                let quad = UiQuad {
                    uv: *uv,
                    texture: TextureRef::Image(*id),
                    ..base
                };
                if let Some(clipped) = quad.clipped(clip) {
                    out.push(UiQuad { z, alpha, ..clipped });
                }
            }
        }
    }

    // UI material (ADR-2045): a procedural fill over the background, under the content.
    if let Some(material) = world.get::<crate::ui_material::UiMaterial>(entity) {
        let quad = material.quad(rect, [0.0; 4]);
        let quad = apply_radius(quad, world, entity, themed, scale);
        if let Some(clipped) = quad.clipped(clip) {
            out.push(UiQuad { z, alpha, ..clipped });
        }
    }

    // Canvas rectangles (unit square of the node), over the background.
    if let Some(canvas) = world.get::<UiCanvas>(entity) {
        for r in &canvas.rects {
            let pos = [rect[0] + r.min[0] * rect[2], rect[1] + r.min[1] * rect[3]];
            let size = [(r.max[0] - r.min[0]) * rect[2], (r.max[1] - r.min[1]) * rect[3]];
            if size[0] <= 0.0 || size[1] <= 0.0 || r.color.0[3] <= 0.0 {
                continue;
            }
            if let Some(clipped) = UiQuad::fill(pos, size, r.color).clipped(intersect(clip, rect)) {
                out.push(UiQuad { z, alpha, ..clipped });
            }
        }
    }

    // Image component.
    let tiled = world
        .get::<crate::node::UiImageFit>(entity)
        .is_some_and(|fit| fit.0 == crate::node::ImageFit::Tile);
    if tiled && let Some(image) = world.get::<UiImage>(entity) {
        // `fit: tile` (ADR-7751): native-size copies from the top-left corner, cut to the rect.
        let tiles = image_tiles(rect, image.uv, renderer.image_size(image.id), scale);
        let cut = intersect(child_clip, rect);
        for (pos, size) in tiles {
            let quad = UiQuad {
                uv: image.uv,
                texture: TextureRef::Image(image.id),
                alpha,
                z,
                ..UiQuad::fill(pos, size, image.tint)
            };
            if let Some(clipped) = quad.clipped(cut) {
                out.push(UiQuad { z, alpha, ..clipped });
            }
        }
    } else if let Some(image) = world.get::<UiImage>(entity) {
        let mut size = [rect[2], rect[3]];
        let mut pos = [rect[0], rect[1]];
        let mut uv = image.uv;
        let cover = world
            .get::<crate::node::UiImageFit>(entity)
            .is_some_and(|fit| fit.0 == crate::node::ImageFit::Cover);
        if image.preserve_aspect && cover {
            let aspect = letterbox_aspect(image, renderer.image_size(image.id));
            uv = cover_uv(image.uv, aspect, rect[2], rect[3]);
        } else if image.preserve_aspect {
            let aspect = letterbox_aspect(image, renderer.image_size(image.id));
            if aspect > 0.0 {
                let target = rect[2] / rect[3];
                if aspect > target {
                    let h = rect[2] / aspect;
                    pos[1] += (rect[3] - h) / 2.0;
                    size = [rect[2], h];
                } else {
                    let w = rect[3] * aspect;
                    pos[0] += (rect[2] - w) / 2.0;
                    size = [w, rect[3]];
                }
            }
        }
        let quad = UiQuad {
            uv,
            texture: TextureRef::Image(image.id),
            corner_radius: world
                .get::<Border>(entity)
                .map_or([0.0; 4], |b| b.radius.map(|r| r * scale)),
            alpha,
            z,
            ..UiQuad::fill(pos, size, image.tint)
        };
        if let Some(clipped) = quad.clipped(child_clip) {
            out.push(UiQuad { z, alpha, ..clipped });
        }
    }

    // Widget chrome drawn over the track/fill background.
    if let Some(toggle) = world.get::<Toggle>(entity) {
        push_toggle_chrome(out, rect, scale, toggle.checked, themed, clip, z, alpha);
    }
    if let Some(slider) = world.get::<Slider>(entity) {
        let start = out.len();
        let fraction = slider.fraction();
        push_slider_chrome(out, rect, scale, fraction, themed, clip, z, alpha);
        let seek = world
            .get::<crate::video::UiVideoControl>(entity)
            .is_some_and(|control| control.action == crate::video::UiVideoAction::SeekSlider);
        if seek {
            seek_slots.push(SeekSlot {
                entity,
                subtree: cache_key,
                start,
                count: out.len() - start,
                fraction,
                rect,
                scale,
                themed,
                clip,
                z,
                alpha,
                drawn: out[start..].iter().map(|quad| (quad.pos, quad.size)).collect(),
            });
        }
    }
    if let Some(bar) = world.get::<ProgressBar>(entity) {
        push_progress_chrome(
            out,
            rect,
            scale,
            bar.value.clamp(0.0, 1.0),
            themed,
            clip,
            z,
            alpha,
        );
    }

    // Border + focus ring (explicit `Border` wins; otherwise theme-resolved width/colour).
    if let Some(border) = world.get::<Border>(entity)
        && border.widths.iter().any(|w| *w > 0.0)
    {
        let quad = UiQuad {
            color: UiColor::TRANSPARENT,
            color2: border.color,
            corner_radius: border.radius.map(|r| r * scale),
            border_width: border.widths.map(|w| w * scale),
            ..UiQuad::fill([rect[0], rect[1]], [rect[2], rect[3]], UiColor::TRANSPARENT)
        };
        if let Some(clipped) = quad.clipped(clip) {
            out.push(UiQuad { z, alpha, ..clipped });
        }
    } else if let Some(t) = themed
        && let Some(w) = t.border_width.filter(|w| *w > 0.0)
    {
        let bw = w * scale;
        let color = t.border_color.unwrap_or(UiColor([0.2, 0.22, 0.28, 1.0]));
        let quad = UiQuad {
            color: UiColor::TRANSPARENT,
            color2: color,
            corner_radius: t.radius.map(|r| r * scale).map_or([bw; 4], |r| [r; 4]),
            border_width: [bw; 4],
            ..UiQuad::fill([rect[0], rect[1]], [rect[2], rect[3]], UiColor::TRANSPARENT)
        };
        if let Some(clipped) = quad.clipped(clip) {
            out.push(UiQuad { z, alpha, ..clipped });
        }
    }
    // Focus ring (D9): only while `focus_visible` (navigation mode by default, I05), in the
    // theme's ring colour, `focus_ring_width` wide, `focus_ring_offset` outside the border box
    // (negative = inset), concentric with the widget's corners.
    if let Some(t) = themed
        && t.focus_visible
        && let Some(ring) = t.focus_ring
        && let Some(g) = focus_ring_geometry(
            rect,
            t.radius
                .map(|r| r * scale)
                .or_else(|| border_radius(world, entity, scale))
                .unwrap_or(0.0),
            t.focus_ring_width.unwrap_or(DEFAULT_FOCUS_RING_WIDTH) * scale,
            t.focus_ring_offset.unwrap_or(0.0) * scale,
        )
    {
        let quad = UiQuad {
            color: UiColor::TRANSPARENT,
            color2: ring,
            corner_radius: [g.radius; 4],
            border_width: [g.width; 4],
            ..UiQuad::fill(
                [g.rect[0], g.rect[1]],
                [g.rect[2], g.rect[3]],
                UiColor::TRANSPARENT,
            )
        };
        if let Some(clipped) = quad.clipped(clip) {
            out.push(UiQuad { z, alpha, ..clipped });
        }
    }

    // Text glyphs.
    if let Some(shaped) = world.get::<ShapedText>(entity) {
        let pad = content_padding(world, entity, scale);
        let text_clip = match world.get::<crate::text::Text>(entity).map(|t| t.overflow) {
            Some(TextOverflow::Clip) => intersect(clip, rect),
            _ => clip,
        };
        // Vertically centre the shaped block in the content box (tabs/buttons with Text on the
        // same entity stretch taller than the line; flexbox cannot centre that leaf text).
        let content = [rect[2] - pad[0] - pad[2], rect[3] - pad[1] - pad[3]];
        // Glyph bitmaps are rasterized at their sub-pixel bin; the block origin snaps to whole
        // pixels so every glyph quad samples its atlas texels 1:1 (a fractional origin would
        // bilinear-blur every glyph).
        let x0 = (rect[0] + pad[0]).round();
        let y0 = (rect[1] + pad[1] + ((content[1] - shaped.size[1]) * 0.5).max(0.0)).round();
        // Text effects (ADR-2045): with any span asking for outline or shadow layers, the glyph
        // quads are collected first so every layer draws under every glyph of the block.
        let effects = crate::text_effects::span_effects(world.get::<crate::text::Text>(entity), scale);
        let mut glyph_quads: Vec<(usize, UiQuad)> = Vec::new();
        let sdf = world.get::<crate::sdf_text::SdfText>(entity).is_some();
        for (index, glyph) in shaped.glyphs.iter().enumerate() {
            let key = glyph.cache_key;
            // Distance-field glyphs (ADR-2045): one field per glyph at the reference size,
            // scaled to this size; color glyphs (no field) fall through to the bitmap path.
            if sdf {
                let field = crate::sdf_text::sdf_key(key);
                let found = renderer
                    .atlas_mut()
                    .get_or_insert(field, || crate::sdf_text::sdf_image(text_system, field));
                if let Some((place, offset)) = found {
                    let pen = [x0 + glyph.pen[0] as f32, y0 + glyph.pen[1] as f32];
                    let font_px = f32::from_bits(key.font_size_bits);
                    let quad = UiQuad {
                        color: glyph.color,
                        alpha,
                        z,
                        ..crate::sdf_text::sdf_quad(
                            pen,
                            offset,
                            place.page,
                            place.rect,
                            PAGE_SIZE as f32,
                            font_px,
                        )
                    };
                    if let Some(clipped) = quad.clipped(text_clip) {
                        out.push(UiQuad { z, alpha, ..clipped });
                    }
                    continue;
                }
            }
            let (place, offset) = {
                let atlas = renderer.atlas_mut();
                match atlas.get_or_insert(key, || text_system.glyph_image(key)) {
                    Some(found) => found,
                    None => continue,
                }
            };
            let uv = [
                place.rect[0] as f32 / PAGE_SIZE as f32,
                place.rect[1] as f32 / PAGE_SIZE as f32,
                (place.rect[0] + place.rect[2]) as f32 / PAGE_SIZE as f32,
                (place.rect[1] + place.rect[3]) as f32 / PAGE_SIZE as f32,
            ];
            let gx = x0 + (glyph.pen[0] + offset[0]) as f32;
            let gy = y0 + (glyph.pen[1] + offset[1]) as f32;
            let quad = UiQuad {
                pos: [gx, gy],
                size: [place.rect[2] as f32, place.rect[3] as f32],
                uv,
                color: glyph.color,
                texture: match place.kind {
                    1 => TextureRef::ColorAtlas(place.page),
                    _ => TextureRef::MonoAtlas(place.page),
                },
                alpha,
                z,
                ..UiQuad::fill([gx, gy], [1.0, 1.0], glyph.color)
            };
            if effects.is_empty() {
                if let Some(clipped) = quad.clipped(text_clip) {
                    out.push(UiQuad { z, alpha, ..clipped });
                }
            } else {
                glyph_quads.push((index, quad));
            }
        }
        if !effects.is_empty() {
            crate::text_effects::push_effect_layers(
                &effects,
                &glyph_quads,
                &shaped.glyph_spans,
                PAGE_SIZE as f32,
                out,
                |q| q.clipped(text_clip).map(|c| UiQuad { z, alpha, ..c }),
            );
        }
        // Underlines draw after the glyphs like strikethroughs (descenders cross them, as in
        // most text renderers).
        for deco in &shaped.decorations {
            let quad = UiQuad {
                alpha,
                z,
                ..UiQuad::fill(
                    [x0 + deco.rect[0], y0 + deco.rect[1]],
                    [deco.rect[2], deco.rect[3]],
                    deco.color,
                )
            };
            if let Some(clipped) = quad.clipped(text_clip) {
                out.push(UiQuad { z, alpha, ..clipped });
            }
        }
    }

    // Scrollbar thumb (right edge).
    if let (Some(view), Some(offset)) = (world.get::<ScrollView>(entity), world.get::<ScrollOffset>(entity))
        && view.show_scrollbar
        && computed.content_size[1] > computed.rect[3] + 1.0
    {
        let track = [rect[0] + rect[2] - 4.0 * scale, rect[1], 4.0 * scale, rect[3]];
        let fraction = computed.rect[3] / computed.content_size[1];
        let thumb_h = (track[3] * fraction).max(24.0);
        let max_scroll = computed.content_size[1] - computed.rect[3];
        let t = (offset.0[1] / max_scroll.max(1.0)).clamp(0.0, 1.0);
        let thumb = [track[0], track[1] + t * (track[3] - thumb_h), track[2], thumb_h];
        // The theme's `color.scrollbar-thumb` slot when it sets one (ADR-7208).
        let color = world
            .get_resource::<crate::theme_resolve::ThemeResolver>()
            .and_then(|r| r.resolved_theme())
            .and_then(|t| t.color("color.scrollbar-thumb"))
            .unwrap_or(UiColor([1.0, 1.0, 1.0, 0.35]));
        let quad = UiQuad::fill([thumb[0], thumb[1]], [thumb[2], thumb[3]], color);
        if let Some(clipped) = quad.clipped(child_clip) {
            out.push(UiQuad { z, alpha, ..clipped });
        }
    }

    // Children (in tree order). Their layout rects already carry this node's scroll offset (in
    // whole pixels, so rows stay crisp mid-scroll): moving them again here drew scrolled content
    // twice as far as hit testing, `ui.query` and the screen reader placed it (ADR-4932).
    let ctx = ChildCtx {
        z_base,
        origin,
        clip: child_clip,
        alpha,
    };
    if descend && let Some(children) = world.get::<Children>(entity) {
        let children_start = out.len();
        for child in children.iter() {
            // A child fully outside the clip emits nothing: skip its whole subtree. The clip
            // children check inside `offscreen` keeps parents whose unclipped children can
            // still poke into the clip.
            if offscreen(world, child, &ctx) {
                continue;
            }
            walk(
                world,
                text_system,
                renderer,
                child,
                ctx.z_base,
                ctx.origin,
                ctx.clip,
                ctx.alpha,
                true,
                account,
                out,
                cache_key,
                seek_slots,
                walked,
            );
        }
        // Rounded clip (ADR-2866): the children's layer keeps only the node's rounded rect. With
        // square corners the rect clip the children already have is the whole effect: no layer.
        let radii = crate::group_layer::node_radii(world, entity, scale);
        if rounded_clip && radii.iter().any(|r| *r > 0.0) {
            let _span = tracing::trace_span!("ui_rounded_clip").entered();
            account.wrap(
                out,
                children_start,
                &crate::group_layer::GroupSpec {
                    opacity: 1.0,
                    mask: Some(crate::group_layer::MaskSpec {
                        rect,
                        radii,
                        image: None,
                    }),
                    limit: child_clip,
                    z,
                },
            );
        }
    }
    if layer_node {
        let mask = image_mask.map(|m| crate::group_layer::MaskSpec {
            rect,
            radii: [0.0; 4],
            image: Some((
                m.image,
                m.uv,
                crate::group_layer::mask_placement(m.fit, rect, renderer.image_size(m.image), m.uv, scale),
                m.fit == crate::group_layer::MaskFit::Tile,
            )),
        });
        account.wrap(
            out,
            group_start,
            &crate::group_layer::GroupSpec {
                opacity: parent_alpha * group_opacity.unwrap_or(1.0),
                mask,
                limit: clip,
                z,
            },
        );
    }
    Some(ctx)
}

fn push_toggle_chrome(
    out: &mut Vec<UiQuad>,
    rect: [f32; 4],
    scale: f32,
    checked: bool,
    themed: Option<Themed>,
    clip: [f32; 4],
    z: f32,
    alpha: f32,
) {
    let pad = 3.0 * scale;
    let knob_d = (rect[3] - pad * 2.0).max(4.0);
    let travel = (rect[2] - pad * 2.0 - knob_d).max(0.0);
    let kx = rect[0] + pad + if checked { travel } else { 0.0 };
    let ky = rect[1] + pad;
    let fill = if checked {
        themed
            .and_then(|t| t.accent)
            .unwrap_or(UiColor([0.24, 0.62, 0.94, 1.0]))
    } else {
        UiColor([0.35, 0.38, 0.44, 1.0])
    };
    // Checked track tint.
    if checked {
        let track = UiQuad {
            alpha,
            z,
            ..UiQuad::fill(
                [rect[0], rect[1]],
                [rect[2], rect[3]],
                fill.lerp(UiColor::BLACK, 0.45),
            )
        };
        let track = UiQuad {
            corner_radius: [rect[3] * 0.5; 4],
            ..track
        };
        if let Some(clipped) = track.clipped(clip) {
            out.push(clipped);
        }
    }
    let knob = UiQuad {
        alpha,
        z,
        corner_radius: [knob_d * 0.5; 4],
        ..UiQuad::fill([kx, ky], [knob_d, knob_d], UiColor::WHITE)
    };
    if let Some(clipped) = knob.clipped(clip) {
        out.push(clipped);
    }
}

fn push_slider_chrome(
    out: &mut Vec<UiQuad>,
    rect: [f32; 4],
    scale: f32,
    fraction: f32,
    themed: Option<Themed>,
    clip: [f32; 4],
    z: f32,
    alpha: f32,
) {
    let accent = themed
        .and_then(|t| t.accent)
        .unwrap_or(UiColor([0.24, 0.62, 0.94, 1.0]));
    let fill_w = (rect[2] * fraction.clamp(0.0, 1.0)).max(0.0);
    if fill_w > 0.5 {
        let fill = UiQuad {
            alpha,
            z,
            corner_radius: themed
                .and_then(|t| t.radius)
                .map(|r| [r * scale; 4])
                .unwrap_or([4.0 * scale; 4]),
            ..UiQuad::fill([rect[0], rect[1]], [fill_w, rect[3]], accent)
        };
        if let Some(clipped) = fill.clipped(clip) {
            out.push(clipped);
        }
    }
    let thumb = (rect[3] + 4.0 * scale).max(8.0);
    let tx = rect[0] + fill_w - thumb * 0.5;
    let ty = rect[1] + (rect[3] - thumb) * 0.5;
    let knob = UiQuad {
        alpha,
        z,
        corner_radius: [thumb * 0.5; 4],
        ..UiQuad::fill([tx, ty], [thumb, thumb], UiColor::WHITE)
    };
    if let Some(clipped) = knob.clipped(clip) {
        out.push(clipped);
    }
}

fn push_progress_chrome(
    out: &mut Vec<UiQuad>,
    rect: [f32; 4],
    scale: f32,
    value: f32,
    themed: Option<Themed>,
    clip: [f32; 4],
    z: f32,
    alpha: f32,
) {
    let accent = themed
        .and_then(|t| t.accent)
        .unwrap_or(UiColor([0.24, 0.62, 0.94, 1.0]));
    let fill_w = (rect[2] * value).max(0.0);
    if fill_w <= 0.5 {
        return;
    }
    let fill = UiQuad {
        alpha,
        z,
        corner_radius: themed
            .and_then(|t| t.radius)
            .map(|r| [r * scale; 4])
            .unwrap_or([4.0 * scale; 4]),
        ..UiQuad::fill([rect[0], rect[1]], [fill_w, rect[3]], accent)
    };
    if let Some(clipped) = fill.clipped(clip) {
        out.push(clipped);
    }
}

fn apply_radius(quad: UiQuad, world: &World, entity: Entity, themed: Option<Themed>, scale: f32) -> UiQuad {
    let radii = themed
        .and_then(|t| t.radius)
        .map(|r| [r * scale; 4])
        .or_else(|| world.get::<Border>(entity).map(|b| b.radius.map(|r| r * scale)))
        .unwrap_or([0.0; 4]);
    UiQuad {
        corner_radius: radii,
        ..quad
    }
}

fn border_radius(world: &World, entity: Entity, scale: f32) -> Option<f32> {
    world
        .get::<Border>(entity)
        .map(|b| b.radius[0] * scale)
        .filter(|r| *r > 0.0)
}

/// Content-box padding (l, t, r, b) in physical px.
fn content_padding(world: &World, entity: Entity, scale: f32) -> [f32; 4] {
    world
        .get::<crate::node::NodeStyle>(entity)
        .map(|s| {
            let p = &s.0.padding;
            [
                dim_px(p.left, scale),
                dim_px(p.top, scale),
                dim_px(p.right, scale),
                dim_px(p.bottom, scale),
            ]
        })
        .unwrap_or([0.0; 4])
}

fn dim_px(d: crate::style::Dimension, scale: f32) -> f32 {
    match d {
        crate::style::Dimension::Points(v) => v * scale,
        _ => 0.0,
    }
}

/// Aspect (width / height) used to letterbox an image.
///
/// A full-frame UV uses the registered pixel size when that size is known, so a wide picture
/// stays wide inside a square node. A caller-set sub-rect, or an unknown pixel size, uses the
/// UV rectangle.
fn letterbox_aspect(image: &UiImage, pixels: Option<[u32; 2]>) -> f32 {
    let full = image.uv == [0.0, 0.0, 1.0, 1.0];
    if full
        && let Some([w, h]) = pixels
        && h > 0
    {
        return w as f32 / h as f32;
    }
    image.uv_size()
}

/// `uv` cropped, centred, to the aspect of a `width` x `height` rect, for a picture whose aspect is
/// `aspect` (w/h): the [`crate::ImageFit::Cover`] draw (ADR-9980). A wider picture loses its left
/// and right, a taller one its top and bottom; an unknown aspect or an empty rect keeps `uv`.
fn cover_uv(uv: [f32; 4], aspect: f32, width: f32, height: f32) -> [f32; 4] {
    let known = aspect > 0.0 && width > 0.0 && height > 0.0;
    if !known {
        return uv;
    }
    let target = width / height;
    let [u0, v0, u1, v1] = uv;
    if aspect > target {
        let cut = (u1 - u0) * (1.0 - target / aspect) / 2.0;
        [u0 + cut, v0, u1 - cut, v1]
    } else {
        let cut = (v1 - v0) * (1.0 - aspect / target) / 2.0;
        [u0, v0 + cut, u1, v1 - cut]
    }
}

/// Most quads one `fit: tile` image draws (ADR-7751). A picture so small that more copies would
/// be needed is drawn at a larger, uniform tile size so the count stays at or under this.
pub const MAX_IMAGE_TILES: usize = 4096;

/// The tiles of a `fit: tile` image (ADR-7751): `(pos, size)` in screen px, row by row from the
/// rect's top-left corner, each the size of the `uv` sub-rect of an `image_px` picture times
/// `scale`. Tiles past the rect's right and bottom edges are whole; the caller clips them. An
/// unknown picture size, an empty rect or a non-finite input gives no tiles; past
/// [`MAX_IMAGE_TILES`] the tile grows uniformly.
pub fn image_tiles(
    rect: [f32; 4],
    uv: [f32; 4],
    image_px: Option<[u32; 2]>,
    scale: f32,
) -> Vec<([f32; 2], [f32; 2])> {
    let Some(px) = image_px else {
        return Vec::new();
    };
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    let mut tw = px[0] as f32 * (uv[2] - uv[0]).abs() * scale;
    let mut th = px[1] as f32 * (uv[3] - uv[1]).abs() * scale;
    let finite = rect.iter().chain([tw, th].iter()).all(|v| v.is_finite());
    if !finite || rect[2] <= 0.0 || rect[3] <= 0.0 || tw < 1e-3 || th < 1e-3 {
        return Vec::new();
    }
    let count = |tw: f32, th: f32| ((rect[2] / tw).ceil() as f64) * ((rect[3] / th).ceil() as f64);
    let n = count(tw, th);
    if n > MAX_IMAGE_TILES as f64 {
        let grow = (n / MAX_IMAGE_TILES as f64).sqrt() as f32;
        tw *= grow;
        th *= grow;
        while count(tw, th) > MAX_IMAGE_TILES as f64 {
            tw *= 1.0625;
            th *= 1.0625;
        }
    }
    let (nx, ny) = ((rect[2] / tw).ceil() as usize, (rect[3] / th).ceil() as usize);
    let mut out = Vec::with_capacity(nx * ny);
    for j in 0..ny {
        for i in 0..nx {
            out.push(([rect[0] + i as f32 * tw, rect[1] + j as f32 * th], [tw, th]));
        }
    }
    out
}

impl UiImage {
    /// Aspect ratio of the uv sub-rect (w/h); 0 when unknown.
    fn uv_size(&self) -> f32 {
        let w = self.uv[2] - self.uv[0];
        let h = self.uv[3] - self.uv[1];
        if h > 0.0 { w / h } else { 0.0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-7751 `fit: tile`: native-size copies row by row from the corner, a UV sub-rect tiles
    /// at its own size, the UI scale multiplies, and a tiny picture in a huge rect stays at or
    /// under [`MAX_IMAGE_TILES`] copies; an unknown size, an empty rect or NaN give none.
    #[test]
    fn image_tiles_cover_the_rect_at_native_size_and_stay_capped() {
        let full = [0.0, 0.0, 1.0, 1.0];
        let tiles = image_tiles([10.0, 20.0, 40.0, 20.0], full, Some([16, 8]), 1.0);
        assert_eq!(tiles.len(), 9);
        assert_eq!(tiles[0], ([10.0, 20.0], [16.0, 8.0]));
        assert_eq!(tiles[8], ([42.0, 36.0], [16.0, 8.0]));
        let half = image_tiles([0.0, 0.0, 16.0, 16.0], [0.0, 0.0, 0.5, 0.5], Some([16, 8]), 2.0);
        assert_eq!(
            half.len(),
            2,
            "8 x 4 px of source at scale 2 = 16 x 8, 1 x 2 in 16 x 16: {half:?}"
        );
        assert_eq!(half[0].1, [16.0, 8.0]);
        let many = image_tiles([0.0, 0.0, 10_000.0, 10_000.0], full, Some([1, 1]), 1.0);
        assert!(
            !many.is_empty() && many.len() <= MAX_IMAGE_TILES,
            "{}",
            many.len()
        );
        let (pos, size) = *many.last().expect("tiles");
        assert!(
            pos[0] + size[0] >= 10_000.0 && pos[1] + size[1] >= 10_000.0,
            "still covers the rect"
        );
        assert!(image_tiles([0.0, 0.0, 10.0, 10.0], full, None, 1.0).is_empty());
        assert!(image_tiles([0.0, 0.0, 0.0, 10.0], full, Some([4, 4]), 1.0).is_empty());
        assert!(image_tiles([f32::NAN, 0.0, 10.0, 10.0], full, Some([4, 4]), 1.0).is_empty());
        assert!(image_tiles([0.0, 0.0, 10.0, 10.0], [0.0, 0.0, 0.0, 1.0], Some([4, 4]), 1.0).is_empty());
    }

    /// ADR-9980: `cover` keeps the centre of the picture at the rect's aspect, on either axis, inside
    /// a sub-rect too, and leaves the UVs alone when the aspect is unknown.
    #[test]
    fn cover_uv_crops_the_centre_to_the_rect_aspect() {
        let close = |a: [f32; 4], b: [f32; 4]| a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-5);
        let full = [0.0, 0.0, 1.0, 1.0];
        assert!(close(cover_uv(full, 2.0, 64.0, 64.0), [0.25, 0.0, 0.75, 1.0]));
        assert!(close(cover_uv(full, 0.5, 64.0, 64.0), [0.0, 0.25, 1.0, 0.75]));
        assert!(close(cover_uv(full, 1.0, 100.0, 50.0), [0.0, 0.25, 1.0, 0.75]));
        assert!(close(
            cover_uv([0.5, 0.0, 1.0, 0.5], 2.0, 10.0, 10.0),
            [0.625, 0.0, 0.875, 0.5]
        ));
        assert!(close(cover_uv(full, 0.0, 64.0, 64.0), full));
        assert!(close(cover_uv(full, 2.0, 0.0, 64.0), full));
    }

    #[test]
    fn preserve_aspect_uses_pixel_size_of_a_wide_image() {
        let image = UiImage::default();
        let pixels = letterbox_aspect(&image, Some([200, 100]));
        assert!((pixels - 2.0).abs() < 1e-5, "{pixels}");
        // A sub-rect keeps the UV aspect even when a pixel size is registered.
        let mut cropped = image;
        cropped.uv = [0.0, 0.0, 0.5, 1.0];
        let uv = letterbox_aspect(&cropped, Some([200, 100]));
        assert!((uv - 0.5).abs() < 1e-5, "{uv}");
        // Unknown pixel size falls back to the full-frame UV, which is square.
        let unknown = letterbox_aspect(&image, None);
        assert!((unknown - 1.0).abs() < 1e-5, "{unknown}");
    }
}
