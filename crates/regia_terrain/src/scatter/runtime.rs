// SPDX-License-Identifier: MPL-2.0
//! The streamed scatter on the engine's ECS frame path (ADR-1415): a [`RuntimeScatter`] resource
//! holds a [`ScatterField`], and [`extract_runtime_scatter`] (added by [`ScatterRuntimePlugin`])
//! streams it around the primary view's eye and appends its instances to the
//! [`ExtractedFrame`] once per frame, after the frame's own meshes are extracted and LOD-selected.
//!
//! So an ECS app draws a whole map's scatter (hundreds of thousands of instances, a forest) with
//! no entity per instance: the field keeps the instances in tiles around the camera, culls whole
//! tiles and sub-blocks on the CPU, picks each instance's LOD with hysteresis and a dithered
//! cross-fade, fades it out at its class's range, and hands the renderer one draw per visible
//! instance and material part, sharing one mesh and one material per (mesh, LOD, part). The
//! frame renderer instances those runs (GPU-driven path: one indirect draw per batch, culled per
//! instance on the GPU).
//!
//! Everything is a function of the field, the view and the frame count: two runs of the same
//! sequence emit the same draws in the same order (background placement never changes what is
//! emitted, [`ScatterField::set_background_streaming`]).

use std::sync::{Mutex, PoisonError};

use regia_app::prelude::*;
use regia_render::{ExtractedFrame, LodViewport};

use super::control::ScatterControl;
use super::field::{ScatterField, ScatterFrameStats, ScatterResitStats, ScatterStreamStats, ScatterView};
use super::painted::TerrainScatterPaint;
use super::set::ScatterSet;

/// A scatter field drawn by the ECS frame ([`extract_runtime_scatter`]).
#[derive(Resource)]
pub struct RuntimeScatter {
    /// In a mutex only so the resource is `Sync`: the system takes it by `&mut`, so it is never
    /// contended.
    field: Mutex<ScatterField>,
    /// What the last frame emitted.
    pub last_frame: ScatterFrameStats,
    /// What the last frame streamed.
    pub last_stream: ScatterStreamStats,
    /// Frames extracted so far.
    pub frames: u64,
    /// What [`resit_runtime_scatter`] did on the last frame that carried height edits (ADR-7585).
    pub last_resit: ScatterResitStats,
    /// Re-sits since the field was made, summed (`tiles_resident` is the latest).
    pub total_resit: ScatterResitStats,
    /// Draw the field (default on). Off, nothing is streamed or emitted.
    pub enabled: bool,
}

impl RuntimeScatter {
    /// Wraps a built field. Nothing is placed until the first extracted frame.
    #[must_use]
    pub fn new(field: ScatterField) -> Self {
        Self {
            field: Mutex::new(field),
            last_frame: ScatterFrameStats::default(),
            last_stream: ScatterStreamStats::default(),
            frames: 0,
            last_resit: ScatterResitStats::default(),
            total_resit: ScatterResitStats::default(),
            enabled: true,
        }
    }

    /// The field (to toggle classes, set impostors or detail limits).
    pub fn field_mut(&mut self) -> &mut ScatterField {
        self.field.get_mut().unwrap_or_else(PoisonError::into_inner)
    }

    /// The field's scatter set.
    #[must_use]
    pub fn set(&self) -> ScatterSet {
        self.field
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .set()
            .clone()
    }

    /// Instances resident in the field.
    #[must_use]
    pub fn resident(&self) -> usize {
        self.field.lock().unwrap_or_else(PoisonError::into_inner).len()
    }
}

/// The view the field is culled and LOD-selected against for `frame`'s primary view, at the
/// `width`×`height` the frame is rendered at (the aspect ratio), with the frame's sun. `None`
/// without a view.
#[must_use]
pub fn scatter_view_of(frame: &ExtractedFrame, width: u32, height: u32) -> Option<ScatterView> {
    let view = frame.primary_view()?;
    let camera = view.camera;
    let aspect = width.max(1) as f32 / height.max(1) as f32;
    let mut out = ScatterView::look_at(
        camera.eye,
        camera.target,
        camera.fov_y,
        aspect,
        camera.near,
        camera.far,
    );
    if let Some(sun) = frame.directional.as_ref() {
        // The uniform holds the light's travel direction; the field wants the way to the sun.
        let d = sun.direction;
        let len = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
        if len > 1e-6 {
            out = out.with_sun([-d[0] / len, -d[1] / len, -d[2] / len]);
        }
    }
    Some(out)
}

/// `Render`, after the frame's meshes are extracted and LOD-selected: streams the
/// [`RuntimeScatter`] field around the primary view's eye (one [`ScatterField::update`] per
/// frame, the LOD cross-fade clock) and appends its draws to [`ExtractedFrame::meshes`]
/// ([`ScatterField::extract_meshes`]). Applies the agent's [`ScatterControl`] first and records
/// the frame into it, and hands the field the painted instances of the terrain with the lowest
/// entity id ([`ScatterField::set_painted`]). The aspect ratio is the [`LodViewport`]'s (the
/// filming size).
pub fn extract_runtime_scatter(
    scatter: Option<ResMut<RuntimeScatter>>,
    control: Option<ResMut<ScatterControl>>,
    viewport: Option<Res<LodViewport>>,
    extracted: Option<ResMut<ExtractedFrame>>,
    painted: Query<(Entity, &TerrainScatterPaint)>,
) {
    let (Some(mut scatter), Some(mut extracted)) = (scatter, extracted) else {
        return;
    };
    if !scatter.enabled {
        return;
    }
    let (width, height) = viewport.map_or((1920, 1080), |v| (v.width, v.height));
    let Some(view) = scatter_view_of(&extracted, width, height) else {
        return;
    };
    let _span = tracing::info_span!(target: "regia_terrain", "runtime_scatter").entered();
    let scatter = &mut *scatter;
    let field = scatter.field.get_mut().unwrap_or_else(PoisonError::into_inner);
    if let Some(control) = control.as_deref() {
        control.apply(field, None);
    }
    if let Some((_, paint)) = painted.iter().min_by_key(|(entity, _)| entity.to_bits()) {
        field.set_painted(&paint.0);
    }
    let stream = field.update(view.eye);
    let mut meshes = std::mem::take(&mut extracted.meshes);
    // Inside a cell only the outdoors its portals show is drawn (plan row W-22, ADR-8413).
    let stats = field.extract_meshes_through(&view, &extracted.outdoor_portals, &mut meshes);
    extracted.meshes = meshes;
    if let Some(mut control) = control {
        control.record(field, stats);
    }
    scatter.last_frame = stats;
    scatter.last_stream = stream;
    scatter.frames += 1;
}

/// `Update`, after [`crate::apply_terrain_height_patches`]: hands every
/// [`crate::TerrainHeightsChanged`] of the frame, in order, to the [`RuntimeScatter`] field
/// ([`ScatterField::heights_changed`]) with the edited `Terrain`'s map, so the instances under a
/// crater sit on the new ground and the rest keep their matrices (ADR-7585). A message with no
/// `Terrain` (only a draw linked) is skipped: the field has no edited map to read.
pub fn resit_runtime_scatter(
    scatter: Option<ResMut<RuntimeScatter>>,
    mut changed: MessageReader<crate::TerrainHeightsChanged>,
    terrains: Query<&crate::Terrain>,
) {
    let Some(mut scatter) = scatter else {
        changed.clear();
        return;
    };
    let mut frame = ScatterResitStats::default();
    let mut any = false;
    for message in changed.read() {
        let Some(terrain) = message.entity.and_then(|e| terrains.get(e).ok()) else {
            continue;
        };
        let stats = scatter.field_mut().heights_changed(&terrain.map, &message.rect);
        frame.accumulate(&stats);
        any = true;
    }
    if any {
        scatter.total_resit.accumulate(&frame);
        scatter.last_resit = frame;
    }
}

/// Adds [`extract_runtime_scatter`] to `Render` (between the frame's mesh LOD selection and the
/// glass extraction, so every later render system sees the scatter) and the [`ScatterControl`]
/// the agent's `terrain.scatter` reads. Insert a [`RuntimeScatter`] to draw a field.
pub struct ScatterRuntimePlugin;

impl Plugin for ScatterRuntimePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ScatterControl>().add_systems(
            Render,
            extract_runtime_scatter
                .after(regia_render::select_mesh_lods)
                .after(regia_render::viscells::cull_unreachable_cells)
                .before(regia_render::extract_glass),
        );
        // Runtime height edits re-sit the field (ADR-7585); only with the terrain plugin's
        // messages registered.
        app.add_systems(
            Update,
            resit_runtime_scatter
                .after(crate::apply_terrain_height_patches)
                .run_if(resource_exists::<Messages<crate::TerrainHeightsChanged>>),
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::scatter::field::{ScatterLibrary, ScatterSource};
    use crate::scatter::rock::{RockStyle, procedural_rock};
    use crate::scatter::set::{ScatterClass, ScatterMeshRef};

    /// ADR-7585: a patch applied by `apply_terrain_height_patches` reaches the runtime field
    /// through `resit_runtime_scatter` in the same frame: instances on the crater move, the
    /// counters say how many tiles were visited out of how many are resident.
    #[test]
    fn a_patch_re_sits_the_runtime_field_in_the_same_frame() {
        let n = 65u32;
        let hf = regia_asset::Heightfield {
            width: n,
            height: n,
            samples: vec![10240; (n * n) as usize],
        };
        let map = Arc::new(crate::TerrainMap::new(&hf, 1024.0, 64.0, 0.0).expect("map"));
        let set = ScatterSet {
            classes: vec![ScatterClass {
                name: "boulders".into(),
                meshes: vec![ScatterMeshRef {
                    mesh: "rock_a".into(),
                    weight: 1.0,
                    min_scale: 0.5,
                    max_scale: 1.5,
                    scale_bias: 1.0,
                }],
                cell_size: 4.0,
                density: 1.0,
                fade_start: 150.0,
                fade_end: 200.0,
                ..ScatterClass::default()
            }],
            ..ScatterSet::default()
        };
        let mut library = ScatterLibrary::new();
        library.insert(
            "rock_a".into(),
            procedural_rock(3, RockStyle::angular([0.5, 0.4, 0.35])),
        );
        let mut field =
            ScatterField::new(&set, ScatterSource::new(Arc::clone(&map)), &library, 1).expect("field");
        field.set_background_streaming(false);
        field.update([0.0, 12.0, 0.0]);
        let before = field.instance_models();
        let terrain = crate::Terrain::new(
            &hf,
            1024.0,
            64.0,
            0.0,
            crate::CdlodConfig::default(),
            2,
            crate::TerrainLayers::default(),
        )
        .expect("terrain");
        let mut world = World::new();
        world.spawn(terrain);
        world.init_resource::<Messages<crate::TerrainHeightsChanged>>();
        world.insert_resource(RuntimeScatter::new(field));
        let patches = crate::TerrainHeightPatches::default();
        world.insert_resource(patches.clone());
        let mut schedule = Schedule::default();
        schedule.add_systems((crate::apply_terrain_height_patches, resit_runtime_scatter).chain());
        patches.set_target(crate::PatchTarget::of(&map));
        patches
            .request(crate::HeightPatch {
                col: 30,
                row: 30,
                width: 5,
                height: 5,
                heights: vec![4.0; 25],
            })
            .expect("patch");
        schedule.run(&mut world);
        let stats = {
            let mut scatter = world.resource_mut::<RuntimeScatter>();
            let stats = scatter.last_resit;
            assert!(stats.instances_resat > 0, "{stats:?}");
            assert!(
                stats.tiles_touched > 0 && stats.tiles_touched < stats.tiles_resident,
                "{stats:?}"
            );
            assert_eq!(scatter.total_resit.instances_resat, stats.instances_resat);
            let after = scatter.field_mut().instance_models();
            let moved = before.iter().zip(&after).filter(|(a, b)| a != b).count();
            assert_eq!(moved as u32, stats.instances_resat);
            stats
        };
        // A frame without edits re-sits nothing and keeps the last frame's counters.
        schedule.run(&mut world);
        assert_eq!(
            world.resource::<RuntimeScatter>().total_resit.instances_resat,
            stats.instances_resat
        );
    }
}
