// SPDX-License-Identifier: MPL-2.0
//! Per-entity render overrides: tint, ghost, x-ray, unlit (Tincal engine request F5).
//!
//! A [`RenderOverride`] component changes how one entity draws without touching its material:
//! the shared [`crate::CpuMaterial`] `Arc` (and its GPU bind group) stays the one every other
//! instance uses. The override rides the instance's object data instead: the two unused lanes
//! of the object block's `skin` vector (`skin.z`, `skin.w`, `lab_forward.wgsl`), written with
//! the model matrix. An entity without an override writes zeros there, which the shader reads
//! as "no override", so frames without overrides are unchanged.
//!
//! - **tint** multiplies the surface's base colour (linear RGB, 0–1); its alpha multiplies the
//!   coverage like [`RenderOverride::alpha`].
//! - **alpha** (ghost) below 1 draws the instance through the alpha-blend pass, sorted with the
//!   other blended draws. It casts no shadow and leaves the depth buffer alone.
//! - **xray** draws through the blend pass with the depth test off, so the instance shows
//!   through whatever is in front of it.
//! - **unlit** shows the (tinted) base colour without lighting, like `KHR_materials_unlit`.
//! - **emissive** adds a display-referred colour (0–1) on top of the lit result, divided by the
//!   frame's exposure, so a highlight reads the same in a dark and a bright scene.
//!
//! An override applies to its entity and every descendant (`ChildOf`) without one of its own,
//! so overriding a glTF node also covers its material parts; [`RenderOverride::default`] (no
//! change) on a child stops the inheritance there.
//!
//! Extraction: [`extract_render_overrides`] (a `Render` system after
//! [`crate::extract_frame`]) fills [`ExtractedRenderOverrides`] with the object keys of the
//! frame's extracted meshes. A host that renders an [`crate::ExtractedFrame`] itself passes
//! [`ExtractedRenderOverrides::keys`] to [`crate::FrameRenderer::set_render_overrides`] every
//! frame (an empty list clears them). The packing is Regia Engine-original (`docs/provenance.md`).

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

use regia_app::prelude::*;

use crate::runtime::{ExtractedFrame, ExtractedMesh};

/// Object `skin.w` bit: this instance carries an override (`skin.z` holds its tint).
pub(crate) const OVERRIDE_ACTIVE: u32 = 1;
/// Object `skin.w` bit: shade unlit.
pub(crate) const OVERRIDE_UNLIT: u32 = 2;
/// Object `skin.w` bit: drawn with the depth test off (x-ray).
pub(crate) const OVERRIDE_XRAY: u32 = 4;
/// Object `skin.w` bit: drawn through the blend pass (coverage below 1, or x-ray).
pub(crate) const OVERRIDE_BLEND: u32 = 8;
/// Object `skin.w` bit: bits 8–31 hold an emissive colour (unorm8 R, G, B).
pub(crate) const OVERRIDE_EMISSIVE: u32 = 16;

/// How one entity draws, on top of its material (see the module docs).
#[derive(Component, Debug, Clone, Copy, PartialEq, Default)]
pub struct RenderOverride {
    /// Base-colour multiplier, linear RGBA in 0–1 (alpha multiplies the coverage).
    pub tint: Option<[f32; 4]>,
    /// Added display-referred colour, linear RGB in 0–1 (exposure-independent).
    pub emissive: Option<[f32; 3]>,
    /// Coverage (ghost) in 0–1. Below 1 the instance draws alpha-blended.
    pub alpha: Option<f32>,
    /// Draw on top of everything (depth test off), alpha-blended.
    pub xray: bool,
    /// Shade unlit: the (tinted) base colour as is.
    pub unlit: bool,
}

impl RenderOverride {
    /// A tint (linear RGBA multiplier).
    #[must_use]
    pub fn tinted(tint: [f32; 4]) -> Self {
        Self {
            tint: Some(tint),
            ..Self::default()
        }
    }

    /// A ghost: the instance at `alpha` coverage.
    #[must_use]
    pub fn ghost(alpha: f32) -> Self {
        Self {
            alpha: Some(alpha),
            ..Self::default()
        }
    }

    /// X-ray at `alpha` coverage: visible through what is in front of it.
    #[must_use]
    pub fn xray(alpha: f32) -> Self {
        Self {
            alpha: Some(alpha),
            xray: true,
            ..Self::default()
        }
    }

    /// The final coverage multiplier: the tint's alpha times [`Self::alpha`], in 0–1.
    #[must_use]
    pub fn coverage(&self) -> f32 {
        let tint = self.tint.map_or(1.0, |t| unit(t[3]));
        tint * self.alpha.map_or(1.0, unit)
    }

    /// Whether the instance draws through the blend pass (coverage below 1, or x-ray).
    #[must_use]
    pub fn blends(&self) -> bool {
        self.xray || self.coverage() < 1.0
    }

    /// Whether this override changes nothing (it still stops inheritance from a parent).
    #[must_use]
    pub fn is_identity(&self) -> bool {
        self.pack() == [0, 0]
    }

    /// The object-data lanes (`skin.z`, `skin.w`): `[0, 0]` when nothing changes.
    ///
    /// `skin.z` is the tint as unorm8 RGBA (A = [`Self::coverage`]); `skin.w` holds the flags in
    /// its low byte and the emissive colour as unorm8 RGB in bytes 1–3.
    #[must_use]
    pub fn pack(&self) -> [u32; 2] {
        let tint = self.tint.unwrap_or([1.0; 4]);
        let coverage = self.coverage();
        let emissive = self
            .emissive
            .map(|e| e.map(unit))
            .filter(|e| e.iter().any(|c| *c > 0.0));
        let changes_colour = tint[..3].iter().any(|c| unit(*c) < 1.0);
        let mut flags = 0;
        if self.unlit {
            flags |= OVERRIDE_UNLIT;
        }
        if self.xray {
            flags |= OVERRIDE_XRAY;
        }
        if self.blends() {
            flags |= OVERRIDE_BLEND;
        }
        let mut high = 0;
        if let Some(e) = emissive {
            flags |= OVERRIDE_EMISSIVE;
            high = (u32::from(unorm8(e[0])) << 8)
                | (u32::from(unorm8(e[1])) << 16)
                | (u32::from(unorm8(e[2])) << 24);
        }
        if flags == 0 && !changes_colour {
            return [0, 0];
        }
        let z = u32::from_le_bytes([
            unorm8(tint[0]),
            unorm8(tint[1]),
            unorm8(tint[2]),
            unorm8(coverage),
        ]);
        [z, flags | OVERRIDE_ACTIVE | high]
    }
}

/// A finite value clamped to 0–1 (NaN reads as 1: no change).
fn unit(value: f32) -> f32 {
    if value.is_nan() {
        1.0
    } else {
        value.clamp(0.0, 1.0)
    }
}

/// WGSL `pack4x8unorm` rounding for one lane.
fn unorm8(value: f32) -> u8 {
    (unit(value) * 255.0).round() as u8
}

/// The override `entity` draws with: its own, else the nearest ancestor's (`ChildOf`).
/// An identity override stops the walk (and returns `None`).
#[must_use]
pub fn resolve_render_override(world: &World, entity: Entity) -> Option<RenderOverride> {
    resolve_with(
        entity,
        |e| world.get::<RenderOverride>(e).copied(),
        |e| world.get::<ChildOf>(e).map(ChildOf::parent),
    )
}

#[cfg(test)]
thread_local! {
    static RESOLVE_CALLS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

fn resolve_with(
    entity: Entity,
    own: impl Fn(Entity) -> Option<RenderOverride>,
    parent: impl Fn(Entity) -> Option<Entity>,
) -> Option<RenderOverride> {
    #[cfg(test)]
    RESOLVE_CALLS.with(|calls| calls.set(calls.get().saturating_add(1)));
    let mut current = entity;
    // Bounded: a malformed hierarchy (a cycle) must not hang extraction.
    for _ in 0..256 {
        if let Some(found) = own(current) {
            return (!found.is_identity()).then_some(found);
        }
        current = parent(current)?;
    }
    None
}

/// The object key [`crate::ExtractedFrame::frame_scene`] gives an extracted mesh.
fn object_key(mesh: &ExtractedMesh) -> u64 {
    mesh.entity.to_bits() ^ mesh.object_key_xor
}

/// This frame's overrides by object key ([`crate::FrameMesh::object_key`]), filled by
/// [`extract_render_overrides`].
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct ExtractedRenderOverrides {
    /// `(object key, override)` of every extracted mesh that draws with one.
    pub keys: Vec<(u64, RenderOverride)>,
}

impl ExtractedRenderOverrides {
    /// The overrides of `meshes`, each resolved with `resolve` (entity → override).
    pub fn collect(meshes: &[ExtractedMesh], resolve: impl Fn(Entity) -> Option<RenderOverride>) -> Self {
        let keys = meshes
            .iter()
            .filter_map(|mesh| resolve(mesh.entity).map(|o| (object_key(mesh), o)))
            .collect();
        Self { keys }
    }

    /// The overrides of `meshes` in `world` ([`resolve_render_override`]).
    #[must_use]
    pub fn from_world(world: &World, meshes: &[ExtractedMesh]) -> Self {
        Self::from_world_with(world, meshes, |_| None)
    }

    /// [`Self::from_world`] where an entity with no override of its own (and no ancestor's)
    /// takes the one `fallback` names (the editor maps a model's hidden part entities to the
    /// entity that owns them).
    #[must_use]
    pub fn from_world_with(
        world: &World,
        meshes: &[ExtractedMesh],
        fallback: impl Fn(Entity) -> Option<Entity>,
    ) -> Self {
        // No `RenderOverride` was ever inserted: nothing to walk.
        if world.component_id::<RenderOverride>().is_none() {
            return Self::default();
        }
        let owned = override_map(world);
        if owned.is_empty() {
            return Self::default();
        }
        let reach = build_reach(&owned, |entity, out| {
            if let Some(list) = world.get::<Children>(entity) {
                out.extend_from_slice(list);
            }
        });
        let mut keys = Vec::new();
        for mesh in meshes {
            let resolved = if reach.contains(&mesh.entity) {
                resolve_render_override(world, mesh.entity)
            } else {
                let Some(owner) = fallback(mesh.entity) else {
                    continue;
                };
                if owner == mesh.entity || !reach.contains(&owner) {
                    continue;
                }
                resolve_render_override(world, owner)
            };
            if let Some(over) = resolved {
                keys.push((object_key(mesh), over));
            }
        }
        Self { keys }
    }
}

/// Entities that own a [`RenderOverride`]. `from_world_with` only has a shared world, so this
/// walks entities once instead of opening a cached query.
fn override_map(world: &World) -> HashMap<Entity, RenderOverride> {
    world
        .iter_entities()
        .filter_map(|entity| {
            let over = entity.get::<RenderOverride>().copied()?;
            Some((entity.id(), over))
        })
        .collect()
}

/// Descendants of every non-identity override, stopping at an entity that has its own
/// override (including an identity one, which blocks inheritance). Depth is capped at 256
/// and a repeated entity is not walked again.
fn build_reach(
    owned: &HashMap<Entity, RenderOverride>,
    mut children_of: impl FnMut(Entity, &mut Vec<Entity>),
) -> HashSet<Entity> {
    let mut reach = HashSet::new();
    let mut stack: Vec<(Entity, u16)> = Vec::new();
    let mut kids = Vec::new();
    for (&entity, over) in owned {
        if over.is_identity() {
            continue;
        }
        stack.push((entity, 0));
        while let Some((current, depth)) = stack.pop() {
            if depth > 256 {
                continue;
            }
            if current != entity && owned.contains_key(&current) {
                continue;
            }
            if !reach.insert(current) {
                continue;
            }
            children_of(current, &mut kids);
            for child in kids.drain(..) {
                stack.push((child, depth + 1));
            }
        }
    }
    reach
}

/// Sorted `(entity, pack)` of every override. The reach cache rebuilds when this changes.
fn override_stamp(overrides: &Query<(Entity, &RenderOverride)>) -> u64 {
    let mut rows: Vec<(u64, u32, u32)> = overrides
        .iter()
        .map(|(entity, over)| {
            let pack = over.pack();
            (entity.to_bits(), pack[0], pack[1])
        })
        .collect();
    rows.sort_unstable();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    rows.hash(&mut hasher);
    hasher.finish()
}

/// Child lists of the entities already in `reach`. A spawned descendant changes this.
fn child_stamp(reach: &HashSet<Entity>, children: &Query<&Children>) -> u64 {
    let mut entities: Vec<Entity> = reach.iter().copied().collect();
    entities.sort_unstable();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for entity in entities {
        entity.hash(&mut hasher);
        match children.get(entity) {
            Ok(list) => {
                list.len().hash(&mut hasher);
                for child in list.iter() {
                    child.hash(&mut hasher);
                }
            }
            Err(_) => 0u8.hash(&mut hasher),
        }
    }
    hasher.finish()
}

/// Reach set kept on the extract system. Rebuilt when the override components or the reached
/// entities' `Children` lists change.
///
/// Public because [`extract_render_overrides`] is public and this type is its `Local` state.
/// The fields stay private; the system fills them.
#[derive(Default)]
pub struct OverrideReach {
    ready: bool,
    override_stamp: u64,
    child_stamp: u64,
    reach: HashSet<Entity>,
}

/// `Render`: fills [`ExtractedRenderOverrides`] for the frame [`crate::extract_frame`] just
/// extracted. Only meshes under a non-identity override are resolved.
pub fn extract_render_overrides(
    extracted: Res<ExtractedFrame>,
    overrides: Query<(Entity, &RenderOverride)>,
    children: Query<&Children>,
    parents: Query<&ChildOf>,
    mut out: ResMut<ExtractedRenderOverrides>,
    mut reach: Local<OverrideReach>,
) {
    if overrides.is_empty() {
        if !out.keys.is_empty() {
            out.keys.clear();
        }
        reach.ready = false;
        reach.reach.clear();
        return;
    }
    let _span = tracing::info_span!("extract_render_overrides", meshes = extracted.meshes.len()).entered();
    let stamp = override_stamp(&overrides);
    let current = reach.ready
        && reach.override_stamp == stamp
        && child_stamp(&reach.reach, &children) == reach.child_stamp;
    if !current {
        let owned: HashMap<Entity, RenderOverride> =
            overrides.iter().map(|(entity, over)| (entity, *over)).collect();
        reach.reach = build_reach(&owned, |entity, kids| {
            if let Ok(list) = children.get(entity) {
                kids.extend_from_slice(list);
            }
        });
        reach.override_stamp = stamp;
        reach.child_stamp = child_stamp(&reach.reach, &children);
        reach.ready = true;
    }
    out.keys.clear();
    for mesh in &extracted.meshes {
        if !reach.reach.contains(&mesh.entity) {
            continue;
        }
        if let Some(over) = resolve_with(
            mesh.entity,
            |entity| overrides.get(entity).ok().map(|(_, over)| *over),
            |entity| parents.get(entity).ok().map(ChildOf::parent),
        ) {
            out.keys.push((object_key(mesh), over));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_packs_to_zero_and_reads_as_no_override() {
        assert_eq!(RenderOverride::default().pack(), [0, 0]);
        assert!(RenderOverride::tinted([1.0; 4]).is_identity());
        assert!(RenderOverride::ghost(1.0).is_identity());
        assert!(!RenderOverride::ghost(0.5).is_identity());
    }

    #[test]
    fn tint_and_flags_pack_into_the_two_lanes() {
        let packed = RenderOverride {
            tint: Some([1.0, 0.0, 0.5, 0.5]),
            alpha: Some(0.5),
            unlit: true,
            ..RenderOverride::default()
        }
        .pack();
        assert_eq!(packed[0].to_le_bytes(), [255, 0, 128, 64]);
        let flags = packed[1];
        assert_ne!(flags & OVERRIDE_ACTIVE, 0);
        assert_ne!(flags & OVERRIDE_UNLIT, 0);
        assert_ne!(flags & OVERRIDE_BLEND, 0);
        assert_eq!(flags & OVERRIDE_XRAY, 0);
        let opaque_tint = RenderOverride::tinted([0.2, 0.4, 0.6, 1.0]);
        assert!(!opaque_tint.blends());
        assert_eq!(opaque_tint.pack()[1], OVERRIDE_ACTIVE);
        let xray = RenderOverride::xray(1.0).pack()[1];
        assert_ne!(xray & OVERRIDE_XRAY, 0);
        assert_ne!(xray & OVERRIDE_BLEND, 0, "x-ray always draws in the blend pass");
    }

    #[test]
    fn emissive_rides_the_high_bytes() {
        let packed = RenderOverride {
            emissive: Some([1.0, 0.5, 0.0]),
            ..RenderOverride::default()
        }
        .pack();
        assert_ne!(packed[1] & OVERRIDE_EMISSIVE, 0);
        assert_eq!(packed[1].to_le_bytes()[1..], [255, 128, 0]);
        assert_eq!(packed[0], u32::MAX, "no tint: white, full coverage");
    }

    #[test]
    fn nan_and_out_of_range_values_are_clamped() {
        let o = RenderOverride {
            tint: Some([f32::NAN, 2.0, -1.0, 1.0]),
            alpha: Some(f32::NAN),
            ..RenderOverride::default()
        };
        assert_eq!(o.coverage(), 1.0);
        assert_eq!(o.pack()[0].to_le_bytes(), [255, 255, 0, 255]);
    }

    #[test]
    fn overrides_inherit_down_the_hierarchy_until_one_stops_them() {
        let mut world = World::new();
        let root = world.spawn(RenderOverride::ghost(0.25)).id();
        let child = world.spawn(ChildOf(root)).id();
        let grandchild = world.spawn(ChildOf(child)).id();
        let stopper = world.spawn((ChildOf(root), RenderOverride::default())).id();
        let below_stopper = world.spawn(ChildOf(stopper)).id();
        let loose = world.spawn_empty().id();
        assert_eq!(
            resolve_render_override(&world, grandchild),
            Some(RenderOverride::ghost(0.25))
        );
        assert_eq!(resolve_render_override(&world, stopper), None);
        assert_eq!(resolve_render_override(&world, below_stopper), None);
        assert_eq!(resolve_render_override(&world, loose), None);
        let _ = child;
    }

    #[test]
    fn the_extraction_system_keys_overridden_meshes_by_object_key() {
        use regia_ecs::ecs::system::RunSystemOnce;
        use std::sync::Arc;
        let mut world = World::new();
        world.init_resource::<ExtractedFrame>();
        world.init_resource::<ExtractedRenderOverrides>();
        let (vertices, indices) = crate::box_mesh([0.5; 3]);
        let mesh = Arc::new(crate::CpuMesh { vertices, indices });
        let material = Arc::new(crate::CpuMaterial::default());
        let renderer = || crate::MeshRenderer::new(Arc::clone(&mesh), Arc::clone(&material));
        let transform = regia_scene::GlobalTransform::IDENTITY;
        let plain = world.spawn((renderer(), transform)).id();
        let tinted = world
            .spawn((
                renderer(),
                transform,
                RenderOverride::tinted([1.0, 0.0, 0.0, 1.0]),
            ))
            .id();
        let parent = world.spawn(RenderOverride::ghost(0.5)).id();
        let child = world.spawn((renderer(), transform, ChildOf(parent))).id();
        let run = |world: &mut World| {
            world.run_system_once(crate::extract_frame).expect("extract");
            world
                .run_system_once(extract_render_overrides)
                .expect("overrides");
            let mut keys = world.resource::<ExtractedRenderOverrides>().keys.clone();
            keys.sort_by_key(|(k, _)| *k);
            keys
        };
        let mut expected = vec![
            (tinted.to_bits(), RenderOverride::tinted([1.0, 0.0, 0.0, 1.0])),
            (child.to_bits(), RenderOverride::ghost(0.5)),
        ];
        expected.sort_by_key(|(k, _)| *k);
        assert_eq!(run(&mut world), expected);
        assert!(!run(&mut world).iter().any(|(k, _)| *k == plain.to_bits()));
        // Removing every override clears the table.
        world.entity_mut(tinted).remove::<RenderOverride>();
        world.entity_mut(parent).remove::<RenderOverride>();
        assert!(run(&mut world).is_empty());
    }

    #[test]
    fn one_override_resolves_only_its_hierarchy() {
        use regia_ecs::ecs::system::RunSystemOnce;
        use std::sync::Arc;
        let mut world = World::new();
        world.init_resource::<ExtractedFrame>();
        world.init_resource::<ExtractedRenderOverrides>();
        let (vertices, indices) = crate::box_mesh([0.5; 3]);
        let mesh = Arc::new(crate::CpuMesh { vertices, indices });
        let material = Arc::new(crate::CpuMaterial::default());
        let renderer = || crate::MeshRenderer::new(Arc::clone(&mesh), Arc::clone(&material));
        let transform = regia_scene::GlobalTransform::IDENTITY;
        let root = world.spawn(RenderOverride::ghost(0.25)).id();
        let child = world.spawn((renderer(), transform, ChildOf(root))).id();
        let stopper = world
            .spawn((renderer(), transform, ChildOf(root), RenderOverride::default()))
            .id();
        let below = world.spawn((renderer(), transform, ChildOf(stopper))).id();
        for _ in 0..40 {
            world.spawn((renderer(), transform));
        }
        let extract = |world: &mut World| {
            world.run_system_once(crate::extract_frame).expect("extract");
            RESOLVE_CALLS.with(|calls| calls.set(0));
            world
                .run_system_cached(extract_render_overrides)
                .expect("overrides");
            RESOLVE_CALLS.with(std::cell::Cell::get)
        };
        // No override at all stays a no-op: nothing is walked.
        let mut bare = World::new();
        bare.init_resource::<ExtractedFrame>();
        bare.init_resource::<ExtractedRenderOverrides>();
        for _ in 0..8 {
            bare.spawn((renderer(), transform));
        }
        assert_eq!(extract(&mut bare), 0);
        assert!(bare.resource::<ExtractedRenderOverrides>().keys.is_empty());

        let calls = extract(&mut world);
        assert_eq!(
            calls, 1,
            "unrelated meshes and a stopper's subtree must not be resolved, got {calls}"
        );
        let keys = &world.resource::<ExtractedRenderOverrides>().keys;
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].0, child.to_bits());
        assert!(
            keys.iter()
                .all(|(k, _)| *k != stopper.to_bits() && *k != below.to_bits())
        );

        // A mesh spawned under the override on a later frame is reached, and the walk stays on that tree.
        let grand = world.spawn((renderer(), transform, ChildOf(child))).id();
        let calls = extract(&mut world);
        assert_eq!(calls, 2, "the new child is resolved with its parent, got {calls}");
        let mut found: Vec<u64> = world
            .resource::<ExtractedRenderOverrides>()
            .keys
            .iter()
            .map(|(k, _)| *k)
            .collect();
        found.sort_unstable();
        let mut expect = vec![child.to_bits(), grand.to_bits()];
        expect.sort_unstable();
        assert_eq!(found, expect);
    }
}
