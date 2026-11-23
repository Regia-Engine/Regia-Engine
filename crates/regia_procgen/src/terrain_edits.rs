// SPDX-License-Identifier: MPL-2.0
//! Terrain edits of a published output (spec 64 § 7.3): writing a run's records into the terrain,
//! restoring the originals, and keeping a generator's own edit out of what it captures.
//!
//! [`ProcTerrainApplied`] on a generator names the records its terrain currently holds. A run
//! captures with those records lifted (`lifted`), so repeated bakes never accumulate and `verify`
//! compares against the terrain as it was; the old samples are only written back when a run
//! publishes ([`publish`]) or the generator is cleared ([`restore`]), so a failed or cancelled run
//! leaves the published ground in place. Every write is tagged with the generator's guid
//! ([`crate::world::ProcTerrainEdited::source`]): a generator's own edit does not mark it stale.
//!
//! A record's rectangle is the bounding box of what it changed, so it can cover ground the record
//! has nothing to do with. A record therefore **owns** only the samples it changed
//! (`samples != original`), and gives one back only while it still holds the record's value: an
//! author's sculpt inside the box, or a terrain replaced under the generator, stays as it is. Two
//! generators may not own the same sample: each one's originals would hold the other's edit, and
//! clearing them in the wrong order would leave an edit behind for good, so the second publish
//! fails with `terrain_edit_overlap`.

use std::sync::Arc;

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_scene::components::EntityGuid;
use regia_terrain::edit::{TerrainEdits, apply_height_patch, write_height_samples};
use regia_terrain::{DirtyRect, Terrain, TerrainEditLayers, TerrainMap};

use crate::exec::TerrainEditRecord;
use crate::world::{ProcGenState, ProcGenTerrain, ProcGenerator, ProcTerrainEdited};

/// The records a generator's output asked for, and whether the terrain holds them yet (a bake
/// loaded before its terrain waits here until the terrain exists).
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct ProcTerrainApplied {
    /// The records in apply order.
    pub records: Arc<Vec<TerrainEditRecord>>,
    /// The terrain holds them.
    pub applied: bool,
}

/// Names the edit layer a generator publishes into (ADR-8381). Absent: publish writes the base,
/// which is what a generator did before layers could be targeted.
#[derive(Component, Clone, Debug, PartialEq, Eq)]
pub struct ProcTerrainOutput {
    /// Procgen graph asset id or project-relative graph path. Matched as text.
    pub graph: String,
    /// Output name inside `graph`.
    pub output: String,
}

/// One sample a layer-targeted publish changed, so restore can put that layer back and leave
/// every other layer and the base alone.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProcLayerSample {
    /// Row-major sample index.
    pub index: u32,
    /// Layer value before the publish.
    pub before_value: f32,
    /// Layer coverage before the publish.
    pub before_coverage: f32,
    /// Layer value the publish wrote.
    pub after_value: f32,
    /// Layer coverage the publish wrote.
    pub after_coverage: f32,
}

/// The layer samples [`publish`] wrote for a generator that carries [`ProcTerrainOutput`].
/// Restore and the capture lift put these samples back and do not touch the base.
#[derive(Component, Clone, Debug, PartialEq)]
pub struct ProcTerrainLayerRestore {
    /// The edit layer the samples belong to.
    pub layer: u32,
    /// Samples this generator changed.
    pub samples: Arc<Vec<ProcLayerSample>>,
}

/// What changed in the records a generator's terrain holds.
#[derive(Clone, Debug, PartialEq)]
pub struct TerrainEditChange {
    /// The generator.
    pub generator: Guid,
    /// The records it held before.
    pub before: Arc<Vec<TerrainEditRecord>>,
    /// The records it holds now.
    pub after: Arc<Vec<TerrainEditRecord>>,
}

/// Terrain changes since the last drain, recorded only while this resource exists: the editor
/// inserts it and makes each change one undo entry (spec 64 § 7.3). A run asked for and a `clear`
/// are logged. A loaded bake is not, and neither is a re-run staleness started by itself: the
/// edit that caused it is the entry, undoing that edit re-runs the generator, and an entry for the
/// re-run would drop the redo branch.
#[derive(Resource, Debug, Default)]
pub struct ProcTerrainEditLog(pub Vec<TerrainEditChange>);

fn include_sample(rect: Option<DirtyRect>, col: u32, row: u32) -> DirtyRect {
    match rect {
        None => DirtyRect {
            col_start: col,
            col_end: col + 1,
            row_start: row,
            row_end: row + 1,
        },
        Some(rect) => DirtyRect {
            col_start: rect.col_start.min(col),
            col_end: rect.col_end.max(col + 1),
            row_start: rect.row_start.min(row),
            row_end: rect.row_end.max(row + 1),
        },
    }
}

fn dirty(rect: [u32; 4]) -> DirtyRect {
    DirtyRect {
        col_start: rect[0],
        row_start: rect[1],
        col_end: rect[2],
        row_end: rect[3],
    }
}

fn terrain_entity(world: &mut World) -> Option<Entity> {
    world.query_filtered::<Entity, With<Terrain>>().iter(world).next()
}

fn map_of(world: &World, terrain: Option<Entity>) -> Option<&TerrainMap> {
    match terrain {
        Some(e) => world.get::<Terrain>(e).map(|t| &t.map),
        None => world.get_resource::<ProcGenTerrain>().map(|t| &t.0),
    }
}

/// Whether the map holds exactly `values` over `rect`.
fn holds(map: &TerrainMap, rect: [u32; 4], values: &[f32]) -> bool {
    let w = rect[2].saturating_sub(rect[0]) as usize;
    values.iter().enumerate().all(|(i, v)| {
        let (c, r) = (rect[0] as usize + i % w, rect[1] as usize + i / w);
        map.height_at_sample(c as i64, r as i64).to_bits() == v.to_bits()
    })
}

/// Why `records` cannot be written into a `width × height` map.
fn invalid(records: &[TerrainEditRecord], width: u32, height: u32) -> Option<String> {
    records.iter().find_map(|t| {
        let [c0, r0, c1, r1] = t.rect;
        let area = u64::from(c1.saturating_sub(c0)) * u64::from(r1.saturating_sub(r0));
        if c0 >= c1 || r0 >= r1 || c1 > width || r1 > height {
            Some(format!(
                "edit rectangle {:?} is outside the {width} x {height} terrain",
                t.rect
            ))
        } else if t.samples.len() as u64 != area || t.original.len() as u64 != area {
            Some(format!("edit rectangle {:?} has the wrong sample count", t.rect))
        } else if t.samples.iter().chain(&t.original).any(|v| !v.is_finite()) {
            Some(format!(
                "edit rectangle {:?} has a sample that is not finite",
                t.rect
            ))
        } else {
            None
        }
    })
}

/// Whether `records` still fit the terrain (a terrain replaced after they were applied may not).
fn fits(world: &World, terrain: Option<Entity>, records: &[TerrainEditRecord]) -> bool {
    map_of(world, terrain).is_some_and(|m| invalid(records, m.width(), m.height()).is_none())
}

/// World XZ rectangle a sample rectangle can change, for staleness: one sample wider on each side,
/// because a height read between samples interpolates the changed ones.
fn world_rect(map: &TerrainMap, rect: [u32; 4]) -> [f32; 4] {
    let lo = map.sample_to_world(rect[0].saturating_sub(1), rect[1].saturating_sub(1));
    let hi = map.sample_to_world(
        rect[2].min(map.width().saturating_sub(1)),
        rect[3].min(map.height().saturating_sub(1)),
    );
    [lo[0], lo[1], hi[0], hi[1]]
}

/// Writes `values` with an edit noted (GPU re-upload, re-cook), tagged with the generator.
fn write_noted(world: &mut World, terrain: Option<Entity>, guid: Guid, rect: [u32; 4], values: &[f32]) {
    if map_of(world, terrain).is_none_or(|m| holds(m, rect, values)) {
        return;
    }
    let mut tagged = None;
    match terrain {
        Some(e) => {
            if world.get::<TerrainEdits>(e).is_none()
                && let Ok(mut m) = world.get_entity_mut(e)
            {
                m.insert(TerrainEdits::default());
            }
            // A sculpt noted before this write is not ours; note it before ours is mixed in.
            take_author_edit(world);
            if let Ok(mut m) = world.get_entity_mut(e)
                && let Some(mut edits) = m.take::<TerrainEdits>()
            {
                let mut stack = m.take::<regia_terrain::TerrainEditLayers>();
                if let Some(mut t) = m.get_mut::<Terrain>() {
                    match apply_height_patch(&mut t, &mut edits, dirty(rect), values) {
                        Ok(_) => {
                            if let Some(stack) = &mut stack
                                && let Err(err) =
                                    stack.recompute_terrain(&mut t, &mut edits, Some(dirty(rect)))
                            {
                                tracing::warn!(target: "regia_procgen", "terrain edit layers: {err}");
                            }
                            tagged = Some(world_rect(&t.map, rect));
                        }
                        Err(e) => {
                            tracing::warn!(target: "regia_procgen", code = "terrain_edit_invalid", "{e}");
                        }
                    }
                }
                edits.take_height_changes();
                m.insert(edits);
                if let Some(stack) = stack {
                    m.insert(stack);
                }
            }
        }
        None => {
            if let Some(mut t) = world.get_resource_mut::<ProcGenTerrain>() {
                write_height_samples(&mut t.0, dirty(rect), values);
                tagged = Some(world_rect(&t.0, rect));
            }
        }
    }
    if let Some(rect) = tagged {
        world.write_message(ProcTerrainEdited {
            rect,
            source: Some(guid),
        });
    }
}

fn guid_of(world: &World, entity: Entity) -> Guid {
    world.get::<EntityGuid>(entity).map_or(Guid::NIL, |g| g.0)
}

/// Turns a sculpt noted on the terrain since the last look into an author's
/// [`ProcTerrainEdited`], so generators reading that ground go stale (spec 64 § 7.3).
pub(crate) fn take_author_edit(world: &mut World) {
    let Some(e) = terrain_entity(world) else {
        return;
    };
    let Some(rect) = world
        .get_mut::<TerrainEdits>(e)
        .and_then(|mut edits| edits.bypass_change_detection().take_height_changes())
    else {
        return;
    };
    let rect = [rect.col_start, rect.row_start, rect.col_end, rect.row_end];
    let Some(rect) = world.get::<Terrain>(e).map(|t| world_rect(&t.map, rect)) else {
        return;
    };
    world.write_message(ProcTerrainEdited { rect, source: None });
}

/// Whether a record owns sample `k` of its rectangle: it changed it.
fn owns(t: &TerrainEditRecord, k: usize) -> bool {
    t.samples[k].to_bits() != t.original[k].to_bits()
}

/// The heights of `rect`, row by row.
fn read_rect(map: &TerrainMap, rect: [u32; 4]) -> Vec<f32> {
    (rect[1]..rect[3])
        .flat_map(|r| (rect[0]..rect[2]).map(move |c| (c, r)))
        .map(|(c, r)| map.height_at_sample(i64::from(c), i64::from(r)))
        .collect()
}

/// `now` with the record's samples written in.
fn with_samples(t: &TerrainEditRecord, now: &[f32]) -> Vec<f32> {
    (0..now.len())
        .map(|k| if owns(t, k) { t.samples[k] } else { now[k] })
        .collect()
}

/// `now` with the record's originals written back, where it still holds its own value.
fn with_originals(t: &TerrainEditRecord, now: &[f32]) -> Vec<f32> {
    (0..now.len())
        .map(|k| {
            if owns(t, k) && now[k].to_bits() == t.samples[k].to_bits() {
                t.original[k]
            } else {
                now[k]
            }
        })
        .collect()
}

fn write_records(world: &mut World, entity: Entity, records: &[TerrainEditRecord], original: bool) {
    let terrain = terrain_entity(world);
    if !fits(world, terrain, records) {
        return;
    }
    let guid = guid_of(world, entity);
    // Reverse order on the way back: overlapping records of one generator unwind exactly.
    let order: Vec<&TerrainEditRecord> = if original {
        records.iter().rev().collect()
    } else {
        records.iter().collect()
    };
    for t in order {
        let Some(now) = map_of(world, terrain).map(|m| read_rect(m, t.rect)) else {
            return;
        };
        let values = if original {
            with_originals(t, &now)
        } else {
            with_samples(t, &now)
        };
        write_noted(world, terrain, guid, t.rect, &values);
    }
}

/// The first generator other than `entity` whose applied records own a sample `records` would.
fn overlapping(world: &mut World, entity: Entity, records: &[TerrainEditRecord]) -> Option<Guid> {
    let others: Vec<(Entity, Arc<Vec<TerrainEditRecord>>)> = world
        .query::<(Entity, &ProcTerrainApplied)>()
        .iter(world)
        .filter(|(e, a)| *e != entity && a.applied)
        .map(|(e, a)| (e, a.records.clone()))
        .collect();
    let index = |t: &TerrainEditRecord, c: u32, r: u32| {
        (r - t.rect[1]) as usize * (t.rect[2] - t.rect[0]) as usize + (c - t.rect[0]) as usize
    };
    for (other, theirs) in others {
        for (a, b) in records.iter().flat_map(|a| theirs.iter().map(move |b| (a, b))) {
            let (c0, r0) = (a.rect[0].max(b.rect[0]), a.rect[1].max(b.rect[1]));
            let (c1, r1) = (a.rect[2].min(b.rect[2]), a.rect[3].min(b.rect[3]));
            let shared = (r0..r1)
                .flat_map(|r| (c0..c1).map(move |c| (c, r)))
                .any(|(c, r)| owns(a, index(a, c, r)) && owns(b, index(b, c, r)));
            if shared {
                return Some(guid_of(world, other));
            }
        }
    }
    None
}

/// Writes the originals of the records `entity` has applied and forgets them (`clear`).
/// A generator that published into its own edit layer restores only that layer.
pub fn restore(world: &mut World, entity: Entity) {
    let Some(applied) = world.get::<ProcTerrainApplied>(entity).cloned() else {
        return;
    };
    if world.get::<ProcTerrainLayerRestore>(entity).is_some() {
        restore_owned_layer(world, entity);
    } else if applied.applied {
        write_records(world, entity, &applied.records, true);
    }
    if let Ok(mut e) = world.get_entity_mut(entity) {
        e.remove::<ProcTerrainApplied>();
        e.remove::<ProcTerrainLayerRestore>();
    }
}

/// Makes `records` what `entity`'s terrain holds: the old records' originals back, then the new
/// samples. Both are skipped when the records did not change. No terrain yet: the records wait.
///
/// # Errors
///
/// `(terrain_edit_invalid, message)` when the records do not fit the terrain, and
/// `(terrain_edit_overlap, message)` when another generator's applied edit owns one of the same
/// samples. Nothing is written.
pub fn publish(
    world: &mut World,
    entity: Entity,
    records: &[TerrainEditRecord],
) -> Result<(), (String, String)> {
    let previous = world.get::<ProcTerrainApplied>(entity).cloned();
    if records.is_empty() {
        restore(world, entity);
        return Ok(());
    }
    if previous
        .as_ref()
        .is_some_and(|p| p.applied && p.records.as_slice() == records)
    {
        return Ok(());
    }
    let terrain = terrain_entity(world);
    if let Some(map) = map_of(world, terrain) {
        if let Some(message) = invalid(records, map.width(), map.height()) {
            return Err(("terrain_edit_invalid".to_string(), message));
        }
        if records
            .iter()
            .any(|t| !holds(map, t.rect, &t.original) && !holds(map, t.rect, &t.samples))
            && previous.as_ref().is_none_or(|p| !p.applied)
        {
            tracing::warn!(target: "regia_procgen", code = "procgen_terrain_mismatch", "the terrain under a baked edit is neither the bake's original nor its result; re-bake");
        }
    }
    if let Some(other) = overlapping(world, entity, records) {
        return Err((
            "terrain_edit_overlap".to_string(),
            format!("generator {other} already edits the same terrain samples"),
        ));
    }
    if world.get::<ProcTerrainOutput>(entity).is_some() {
        return publish_owned_layer(world, entity, records, previous);
    }
    if let Some(p) = previous.filter(|p| p.applied) {
        write_records(world, entity, &p.records, true);
    }
    let have_terrain = map_of(world, terrain).is_some();
    if have_terrain {
        write_records(world, entity, records, false);
    }
    if let Ok(mut e) = world.get_entity_mut(entity) {
        e.insert(ProcTerrainApplied {
            records: Arc::new(records.to_vec()),
            applied: have_terrain,
        });
    }
    Ok(())
}

fn publish_owned_layer(
    world: &mut World,
    entity: Entity,
    records: &[TerrainEditRecord],
    previous: Option<ProcTerrainApplied>,
) -> Result<(), (String, String)> {
    let Some(output) = world.get::<ProcTerrainOutput>(entity).cloned() else {
        return Err((
            "edit_layer_not_found".to_string(),
            "the generator has no procgen layer target".to_string(),
        ));
    };
    let terrain_e = terrain_entity(world);
    let have_terrain = map_of(world, terrain_e).is_some();
    if have_terrain {
        let Some(terrain_e) = terrain_e else {
            return Err(("edit_layer_not_found".to_string(), "no terrain".to_string()));
        };
        match world.get::<TerrainEditLayers>(terrain_e) {
            None => {
                return Err((
                    "edit_layer_not_found".to_string(),
                    "the terrain has no edit layers".to_string(),
                ));
            }
            Some(stack) if stack.layer_for_procgen(&output.graph, &output.output).is_none() => {
                return Err((
                    "edit_layer_not_found".to_string(),
                    format!(
                        "no edit layer is owned by procgen graph {} output {}",
                        output.graph, output.output
                    ),
                ));
            }
            Some(_) => {}
        }
    }
    if let Some(previous) = previous.filter(|item| item.applied) {
        if world.get::<ProcTerrainLayerRestore>(entity).is_some() {
            restore_owned_layer(world, entity);
        } else {
            write_records(world, entity, &previous.records, true);
        }
    }
    if have_terrain {
        write_owned(world, entity, &output, records)?;
    }
    if let Ok(mut generator) = world.get_entity_mut(entity) {
        generator.insert(ProcTerrainApplied {
            records: Arc::new(records.to_vec()),
            applied: have_terrain,
        });
    }
    Ok(())
}

fn write_owned(
    world: &mut World,
    entity: Entity,
    output: &ProcTerrainOutput,
    records: &[TerrainEditRecord],
) -> Result<(), (String, String)> {
    let Some(terrain_e) = terrain_entity(world) else {
        return Err(("edit_layer_not_found".to_string(), "no terrain".to_string()));
    };
    let Some(mut stack) = world.get::<TerrainEditLayers>(terrain_e).cloned() else {
        return Err((
            "edit_layer_not_found".to_string(),
            "the terrain has no edit layers".to_string(),
        ));
    };
    let Some(layer) = stack.layer_for_procgen(&output.graph, &output.output) else {
        return Err((
            "edit_layer_not_found".to_string(),
            format!(
                "no edit layer is owned by procgen graph {} output {}",
                output.graph, output.output
            ),
        ));
    };
    let width = stack.size()[0];
    let mut samples = Vec::new();
    let mut rect = None;
    for record in records {
        let span = (record.rect[2].saturating_sub(record.rect[0])) as usize;
        if span == 0 {
            continue;
        }
        for (k, &wanted) in record.samples.iter().enumerate() {
            if !owns(record, k) {
                continue;
            }
            let c = record.rect[0] + (k % span) as u32;
            let r = record.rect[1] + (k / span) as u32;
            let index = r as usize * width as usize + c as usize;
            let wrote = stack
                .compose_into_layer(layer, index, wanted)
                .map_err(|err| (err.code().to_string(), err.to_string()))?;
            rect = Some(include_sample(rect, c, r));
            samples.push(ProcLayerSample {
                index: wrote.index,
                before_value: wrote.before_value,
                before_coverage: wrote.before_coverage,
                after_value: wrote.after_value,
                after_coverage: wrote.after_coverage,
            });
        }
    }
    apply_stack(world, entity, terrain_e, stack, rect, true);
    if let Ok(mut generator) = world.get_entity_mut(entity) {
        generator.insert(ProcTerrainLayerRestore {
            layer,
            samples: Arc::new(samples),
        });
    }
    Ok(())
}

fn restore_owned_layer(world: &mut World, entity: Entity) {
    let Some(saved) = world.get::<ProcTerrainLayerRestore>(entity).cloned() else {
        return;
    };
    let Some(terrain_e) = terrain_entity(world) else {
        return;
    };
    let Some(mut stack) = world.get::<TerrainEditLayers>(terrain_e).cloned() else {
        return;
    };
    let width = stack.size()[0];
    let mut rect = None;
    for sample in saved.samples.iter() {
        let Some((value, coverage)) = stack.layer_sample(saved.layer, sample.index as usize) else {
            continue;
        };
        if value.to_bits() != sample.after_value.to_bits()
            || coverage.to_bits() != sample.after_coverage.to_bits()
        {
            continue;
        }
        if stack
            .set_layer_sample(
                saved.layer,
                sample.index as usize,
                sample.before_value,
                sample.before_coverage,
            )
            .is_err()
        {
            continue;
        }
        rect = Some(include_sample(rect, sample.index % width, sample.index / width));
    }
    apply_stack(world, entity, terrain_e, stack, rect, true);
    if let Ok(mut generator) = world.get_entity_mut(entity) {
        generator.remove::<ProcTerrainLayerRestore>();
    }
}

fn lifted_layer<R>(world: &mut World, entity: Entity, f: impl FnOnce(&mut World) -> R) -> R {
    let Some(saved) = world.get::<ProcTerrainLayerRestore>(entity).cloned() else {
        return f(world);
    };
    let Some(terrain_e) = terrain_entity(world) else {
        return f(world);
    };
    let Some(mut stack) = world.get::<TerrainEditLayers>(terrain_e).cloned() else {
        return f(world);
    };
    let width = stack.size()[0];
    let mut snapshot = Vec::new();
    let mut rect = None;
    for sample in saved.samples.iter() {
        let Some((value, coverage)) = stack.layer_sample(saved.layer, sample.index as usize) else {
            continue;
        };
        snapshot.push((sample.index, value, coverage));
        if value.to_bits() == sample.after_value.to_bits()
            && coverage.to_bits() == sample.after_coverage.to_bits()
            && stack
                .set_layer_sample(
                    saved.layer,
                    sample.index as usize,
                    sample.before_value,
                    sample.before_coverage,
                )
                .is_ok()
        {
            rect = Some(include_sample(rect, sample.index % width, sample.index / width));
        }
    }
    apply_stack(world, entity, terrain_e, stack, rect, false);
    let out = f(world);
    if let Some(mut stack) = world.get::<TerrainEditLayers>(terrain_e).cloned() {
        let mut rect = None;
        for (index, value, coverage) in &snapshot {
            if stack
                .set_layer_sample(saved.layer, *index as usize, *value, *coverage)
                .is_ok()
            {
                rect = Some(include_sample(rect, *index % width, *index / width));
            }
        }
        apply_stack(world, entity, terrain_e, stack, rect, false);
    }
    out
}

fn apply_stack(
    world: &mut World,
    generator: Entity,
    terrain_e: Entity,
    mut stack: TerrainEditLayers,
    rect: Option<DirtyRect>,
    note: bool,
) {
    if note {
        take_author_edit(world);
    }
    let guid = guid_of(world, generator);
    let mut tagged = None;
    if let Ok(mut terrain_entity) = world.get_entity_mut(terrain_e) {
        if !terrain_entity.contains::<TerrainEdits>() {
            terrain_entity.insert(TerrainEdits::default());
        }
        if let Some(mut edits) = terrain_entity.take::<TerrainEdits>() {
            if let Some(mut terrain) = terrain_entity.get_mut::<Terrain>()
                && let Some(rect) = rect
            {
                match stack.recompute_terrain(&mut terrain, &mut edits, Some(rect)) {
                    Ok(_) if note => {
                        tagged = Some(world_rect(
                            &terrain.map,
                            [rect.col_start, rect.row_start, rect.col_end, rect.row_end],
                        ));
                    }
                    Ok(_) => {}
                    Err(err) => {
                        tracing::warn!(target: "regia_procgen", "terrain edit layers: {err}");
                    }
                }
            }
            edits.take_height_changes();
            terrain_entity.insert(edits);
        }
        terrain_entity.insert(stack);
    }
    if let Some(rect) = tagged {
        world.write_message(ProcTerrainEdited {
            rect,
            source: Some(guid),
        });
    }
}

/// The records `entity`'s terrain holds now.
fn held(world: &World, entity: Entity) -> Arc<Vec<TerrainEditRecord>> {
    world
        .get::<ProcTerrainApplied>(entity)
        .filter(|a| a.applied)
        .map(|a| a.records.clone())
        .unwrap_or_default()
}

/// Runs `f`, noting in [`ProcTerrainEditLog`] what it changed in `entity`'s records.
pub(crate) fn logged<R>(world: &mut World, entity: Entity, f: impl FnOnce(&mut World) -> R) -> R {
    if !world.contains_resource::<ProcTerrainEditLog>() {
        return f(world);
    }
    let before = held(world, entity);
    let out = f(world);
    let after = held(world, entity);
    if before != after {
        let generator = guid_of(world, entity);
        world
            .resource_mut::<ProcTerrainEditLog>()
            .0
            .push(TerrainEditChange {
                generator,
                before,
                after,
            });
    }
    out
}

/// Makes `records` what `entity`'s terrain holds for an undo or redo, outside the log. The
/// generator's output may no longer match the ground, so it goes stale, unless it is `live`: a
/// stale `live` generator re-runs at once, which would undo the undo.
///
/// # Errors
///
/// As [`publish`]; nothing is written.
pub fn set_records(
    world: &mut World,
    entity: Entity,
    records: &[TerrainEditRecord],
) -> Result<(), (String, String)> {
    publish(world, entity, records)?;
    let live = world.get::<ProcGenerator>(entity).is_some_and(|g| g.live);
    if !live && let Some(mut st) = world.get_mut::<ProcGenState>(entity) {
        st.stale = true;
        st.stale_whole = true;
    }
    Ok(())
}

/// Applies records that were waiting for a terrain (`ProcGenSet::Lifecycle`).
pub(crate) fn apply_waiting(world: &mut World) {
    let terrain = terrain_entity(world);
    if map_of(world, terrain).is_none() {
        return;
    }
    let waiting: Vec<(Entity, Arc<Vec<TerrainEditRecord>>)> = world
        .query::<(Entity, &ProcTerrainApplied)>()
        .iter(world)
        .filter(|(_, a)| !a.applied)
        .map(|(e, a)| (e, a.records.clone()))
        .collect();
    for (e, records) in waiting {
        if let Err((code, message)) = publish(world, e, &records) {
            tracing::warn!(target: "regia_procgen", code = code.as_str(), "{message}");
            if let Ok(mut m) = world.get_entity_mut(e) {
                m.remove::<ProcTerrainApplied>();
            }
        }
    }
}

/// Runs `f` with `entity`'s applied records lifted out of the terrain, then puts the terrain back
/// as it was with no edit noted: what a capture sees is the terrain without the generator's own
/// edit (spec 64 § 7.3), with any later sculpt on top of it still there.
pub(crate) fn lifted<R>(world: &mut World, entity: Entity, f: impl FnOnce(&mut World) -> R) -> R {
    if world.get::<ProcTerrainLayerRestore>(entity).is_some() {
        return lifted_layer(world, entity, f);
    }
    let records = world
        .get::<ProcTerrainApplied>(entity)
        .filter(|a| a.applied)
        .map(|a| a.records.clone());
    let Some(records) = records else {
        return f(world);
    };
    let terrain = terrain_entity(world);
    if !fits(world, terrain, &records) {
        return f(world);
    }
    let write = |world: &mut World, rect: [u32; 4], values: &[f32]| match terrain {
        Some(e) => {
            if let Some(mut c) = world.get_mut::<Terrain>(e) {
                write_height_samples(&mut c.map, dirty(rect), values);
            }
        }
        None => {
            if let Some(mut c) = world.get_resource_mut::<ProcGenTerrain>() {
                write_height_samples(&mut c.0, dirty(rect), values);
            }
        }
    };
    let Some(saved) =
        map_of(world, terrain).map(|m| records.iter().map(|t| read_rect(m, t.rect)).collect::<Vec<_>>())
    else {
        return f(world);
    };
    for t in records.iter().rev() {
        let Some(now) = map_of(world, terrain).map(|m| read_rect(m, t.rect)) else {
            continue;
        };
        write(world, t.rect, &with_originals(t, &now));
    }
    let out = f(world);
    for (t, values) in records.iter().zip(&saved) {
        write(world, t.rect, values);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_terrain::{EditLayerBlend, EditLayerOp, EditLayerPatch, TerrainEditLayers, apply_op};

    #[test]
    fn a_procgen_base_edit_survives_layer_recompute_and_restore() {
        let map = TerrainMap::from_heights(3, 3, vec![10.0; 9], 2.0, 100.0, 0.0).expect("map");
        let terrain = Terrain::new(
            &map.to_heightfield(),
            2.0,
            100.0,
            0.0,
            regia_terrain::CdlodConfig::default(),
            1,
            regia_terrain::TerrainLayers::default(),
        )
        .expect("terrain");
        let original = terrain.map.heights()[4];
        let mut stack = TerrainEditLayers::new(&terrain.map);
        let layer = stack.create("Hills", EditLayerBlend::Add).expect("layer");
        let mut world = World::new();
        world.init_resource::<Messages<ProcTerrainEdited>>();
        let target = world.spawn((terrain, stack, TerrainEdits::default())).id();
        let generator = world.spawn_empty().id();
        let record = TerrainEditRecord {
            rect: [1, 1, 2, 2],
            samples: vec![20.0],
            original: vec![original],
        };
        publish(&mut world, generator, &[record]).expect("publish");
        // A full explicit recompute also exercises an empty layer: the writer must update its base.
        let mut query = world.query::<(&mut Terrain, &mut TerrainEditLayers)>();
        let (mut terrain, mut stack) = query.get_mut(&mut world, target).expect("terrain");
        stack
            .recompute(&mut terrain.map, dirty([0, 0, 3, 3]))
            .expect("recompute");
        assert_eq!(terrain.map.heights()[4], 20.0);
        apply_op(
            &mut world,
            target,
            &EditLayerOp::Set {
                id: layer,
                patch: EditLayerPatch {
                    enabled: Some(false),
                    ..Default::default()
                },
            },
        )
        .expect("hide");
        restore(&mut world, generator);
        let mut query = world.query::<(&mut Terrain, &mut TerrainEditLayers)>();
        let (mut terrain, mut stack) = query.get_mut(&mut world, target).expect("terrain");
        stack
            .recompute(&mut terrain.map, dirty([0, 0, 3, 3]))
            .expect("recompute");
        assert_eq!(terrain.map.heights()[4], original);
    }

    #[test]
    fn a_procgen_edit_lands_in_its_layer() {
        let map = TerrainMap::from_heights(3, 3, vec![10.0; 9], 2.0, 100.0, 0.0).expect("map");
        let terrain = Terrain::new(
            &map.to_heightfield(),
            2.0,
            100.0,
            0.0,
            regia_terrain::CdlodConfig::default(),
            1,
            regia_terrain::TerrainLayers::default(),
        )
        .expect("terrain");
        let mut stack = TerrainEditLayers::new(&terrain.map);
        let hills = stack.create("Hills", EditLayerBlend::Add).expect("hills");
        let other = stack.create("Other", EditLayerBlend::Add).expect("other");
        stack
            .set_procgen_target(
                hills,
                Some(regia_terrain::ProcgenLayerTarget {
                    graph: "assets/hills.pgraph.ron".to_owned(),
                    output: "height".to_owned(),
                }),
            )
            .expect("target");
        stack.set_layer_sample(other, 0, 3.0, 0.0).expect("other sample");
        let base = stack.base()[4];
        let mut world = World::new();
        world.init_resource::<Messages<ProcTerrainEdited>>();
        let target = world.spawn((terrain, stack, TerrainEdits::default())).id();
        let generator = world
            .spawn(ProcTerrainOutput {
                graph: "assets/hills.pgraph.ron".to_owned(),
                output: "height".to_owned(),
            })
            .id();
        publish(
            &mut world,
            generator,
            &[TerrainEditRecord {
                rect: [1, 1, 2, 2],
                samples: vec![20.0],
                original: vec![base],
            }],
        )
        .expect("publish");
        let height = world.get::<Terrain>(target).expect("terrain").map.heights()[4];
        assert_eq!(height, 20.0);
        let (base_now, hill, other_kept, other_sample, code, text) = {
            let stack = world.get::<TerrainEditLayers>(target).expect("stack");
            let err = stack.check_target(Some(hills)).expect_err("owned");
            (
                stack.base()[4],
                stack.layer(hills).expect("hills").value(4),
                stack.layer(other).expect("other").value(0),
                stack.layer(other).expect("other").value(4),
                err.code(),
                err.to_string(),
            )
        };
        assert_eq!(base_now, base);
        // The map is quantized, so the layer stores the raise from that base, not from 10.
        assert_eq!(hill.to_bits(), (20.0 - base).to_bits());
        assert_eq!(other_kept, 3.0);
        assert_eq!(other_sample.to_bits(), 0.0f32.to_bits());
        assert_eq!(code, "edit_layer_owned");
        assert!(text.contains("assets/hills.pgraph.ron"), "{text}");
        assert!(text.contains("height"), "{text}");
        restore(&mut world, generator);
        let height = world.get::<Terrain>(target).expect("terrain").map.heights()[4];
        let (base_now, hill, other_kept) = {
            let stack = world.get::<TerrainEditLayers>(target).expect("stack");
            (
                stack.base()[4],
                stack.layer(hills).expect("hills").value(4),
                stack.layer(other).expect("other").value(0),
            )
        };
        assert_eq!(height, base);
        assert_eq!(base_now, base);
        assert_eq!(hill.to_bits(), 0.0f32.to_bits());
        assert_eq!(other_kept, 3.0);
        assert!(world.get::<ProcTerrainLayerRestore>(generator).is_none());
    }
}
