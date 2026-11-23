// SPDX-License-Identifier: MPL-2.0
//! The visibility path's per-frame sway state (ADR-3916): the record table the rasters, the resolve
//! and the velocity pass read, the clock and wind written into it, the standard twins of adopted
//! materials, and what the frame reports. `visbuffer/sway.rs` has the gate and the fade.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use super::super::sway::{SWAY_TABLE_RECORDS, SwayGate, SwaySkip, sway_gate};
use super::{GpuInstance, VisBufferPass, make_buffer};
use crate::LabMaterial;
use crate::device::{TrackedDevice, TrackedQueue};
use crate::foliage_wind::{FOLIAGE_SWAY_LIMIT_SLOT, FOLIAGE_SWAY_SLOT, FoliageSway};

/// Below this wind speed the sway stage returns no offset (`regia::foliage_sway::offset`).
const CALM_SPEED: f32 = 1.0e-4;

/// Tag of an adopted material's standard twin in the renderer's material table.
pub(super) const SWAY_TWIN_TAG: u64 = 0x5357_4159_5457_494e;

/// Frames a twin stays in memory after its material was last drawn: the twin holds a copy of the
/// material's maps, and a plant coming back within this many frames does not copy them again.
const TWIN_KEEP_FRAMES: u64 = 256;

/// A standard twin and the last frame a draw asked for it.
struct Twin {
    material: Arc<LabMaterial>,
    last_seen: u64,
}

/// `SwayRecord` in `shaders/foliage_sway.wgsl`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub(super) struct GpuSwayRecord {
    sway: [f32; 4],
    limit: [f32; 4],
    /// Time, previous time, wind x, wind z.
    clock: [f32; 4],
}

/// Bytes of the table's uniform buffer: the frame word and the records.
pub(super) const SWAY_TABLE_BYTES: u64 =
    16 + SWAY_TABLE_RECORDS as u64 * std::mem::size_of::<GpuSwayRecord>() as u64;

// Uniform bindings up to 16 KiB are valid on every adapter the path runs on.
const _: () = assert!(SWAY_TABLE_BYTES <= 16 * 1024);

/// A material's sway as the instance records it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct InstanceSway {
    /// The record in the table.
    pub(super) record: u32,
    /// How far (metres) a vertex can leave its rest position: the bounds grow by it.
    pub(crate) reach: f32,
}

/// What the draw list needs to send a swaying mesh down the path.
pub(crate) struct SwayPlan {
    /// The material key the instance draws under: its standard twin's.
    pub(crate) twin_key: u64,
    /// The twin's material, for `ensure_material`.
    pub(crate) twin: Arc<LabMaterial>,
    /// The first ask for this twin this frame: its parameters may have changed.
    pub(crate) fresh: bool,
    /// The record and reach for [`VisBufferPass::set_instance_sway`].
    pub(crate) sway: InstanceSway,
}

/// A material's values in the table's key form: slots 15, 14 and 0 by bits, so one record serves
/// every material with the same sway (a scatter field's tint variants share one).
type RecordKey = [u32; 12];

fn record_key(graph: &crate::surface_graph::SurfaceGraph) -> RecordKey {
    let mut key = [0u32; 12];
    for (word, value) in key.iter_mut().zip(
        graph.params[FOLIAGE_SWAY_SLOT]
            .iter()
            .chain(&graph.params[FOLIAGE_SWAY_LIMIT_SLOT])
            .chain(&graph.params[0]),
    ) {
        *word = value.to_bits();
    }
    key
}

/// The sway state of the pass.
#[derive(Default)]
pub(super) struct SwayTable {
    index: HashMap<RecordKey, u32>,
    /// With the clock each material carries in slot 0.
    records: Vec<GpuSwayRecord>,
    /// Records reserved by a lookup this frame, including plans not yet pushed as instances.
    claimed: Vec<bool>,
    /// Records instances name this frame.
    used: Vec<bool>,
    /// Same for the persistent instances (they are not pushed every frame).
    persistent_used: Vec<bool>,
    buffer: Option<wgpu::Buffer>,
    /// What the buffer's records hold.
    written: Vec<GpuSwayRecord>,
    /// The frame word the buffer holds.
    written_frame: [f32; 4],
    /// The renderer's material clock (`None`: the slot's own).
    clock: Option<(f32, f32)>,
    /// The renderer's scene wind (`None`: the slot's own).
    wind: Option<[f32; 2]>,
    /// Standard twins of adopted materials, by material key.
    twins: HashMap<u64, Twin>,
    twins_seen: HashSet<u64>,
    /// Frames begun, for the twins' ages.
    frame: u64,
    /// Instances on the path this frame and the persistent ones.
    frame_instances: u32,
    persistent_instances: u32,
    /// Material slots of the swaying instances.
    frame_slots: HashSet<u32>,
    persistent_slots: Vec<u32>,
    /// Swaying meshes that stayed forward this frame, per [`SwaySkip::index`].
    forward: [u32; SwaySkip::ALL.len()],
    /// Persistent sets refused a record (they draw at rest).
    persistent_full: u32,
    /// Test hook: instances keep their rest bounds, so the cull can drop what sways into view.
    #[cfg(test)]
    pub(super) reach_off: bool,
}

impl SwayTable {
    /// Records for a new frame's report.
    pub(super) fn begin_frame(&mut self) {
        self.used.iter_mut().for_each(|used| *used = false);
        self.claimed.iter_mut().for_each(|claimed| *claimed = false);
        self.frame_instances = 0;
        self.frame_slots.clear();
        self.forward = [0; SwaySkip::ALL.len()];
        self.twins_seen.clear();
        self.frame += 1;
        if self.frame.is_multiple_of(TWIN_KEEP_FRAMES) {
            let frame = self.frame;
            self.twins
                .retain(|_, twin| frame.saturating_sub(twin.last_seen) < TWIN_KEEP_FRAMES);
        }
    }

    /// Forgets the persistent instances' records (their sets are being rebuilt).
    pub(super) fn begin_persistent(&mut self) {
        self.persistent_instances = 0;
        self.persistent_slots.clear();
        self.persistent_used.iter_mut().for_each(|used| *used = false);
        self.persistent_full = 0;
    }

    /// Claims `graph`'s record for this frame, reusing a stale slot when needed. `None` when every
    /// slot is claimed or belongs to a persistent set.
    fn record_for(&mut self, graph: &crate::surface_graph::SurfaceGraph) -> Option<u32> {
        let key = record_key(graph);
        if let Some(&record) = self.index.get(&key) {
            self.claimed[record as usize] = true;
            return Some(record);
        }
        let values = GpuSwayRecord {
            sway: graph.params[FOLIAGE_SWAY_SLOT],
            limit: graph.params[FOLIAGE_SWAY_LIMIT_SLOT],
            clock: graph.params[0],
        };
        let record = if self.records.len() < SWAY_TABLE_RECORDS {
            let record = self.records.len() as u32;
            self.records.push(values);
            self.claimed.push(true);
            self.used.push(false);
            self.persistent_used.push(false);
            record
        } else {
            // Current-frame plans reserve a slot before their instances are pushed, and persistent
            // sets keep their slot between rebuilds. Only stale records can be replaced (ADR-3916).
            let slot = (0..self.records.len())
                .find(|&slot| !self.claimed[slot] && !self.used[slot] && !self.persistent_used[slot])?;
            let record = slot as u32;
            self.index.retain(|_, index| *index != record);
            self.records[slot] = values;
            self.claimed[slot] = true;
            record
        };
        self.index.insert(key, record);
        Some(record)
    }

    /// `material`'s record and reach. `None` when the table is full.
    pub(super) fn sway_of(&mut self, material: &LabMaterial) -> Option<InstanceSway> {
        let graph = material.surface_graph.as_deref()?;
        let record = self.record_for(graph)?;
        #[cfg(test)]
        let reach = if self.reach_off {
            0.0
        } else {
            graph.max_vertex_offset.max(0.0)
        };
        #[cfg(not(test))]
        let reach = graph.max_vertex_offset.max(0.0);
        Some(InstanceSway { record, reach })
    }

    /// The record as this frame's clock and wind make it.
    fn live(&self, record: &GpuSwayRecord) -> GpuSwayRecord {
        let mut out = *record;
        if let Some((now, prev)) = self.clock {
            out.clock[0] = now;
            out.clock[1] = prev;
        }
        if let Some([x, z]) = self.wind {
            out.clock[2] = x;
            out.clock[3] = z;
        }
        out
    }

    /// Some instance on the path sways this frame, moving or not.
    pub(super) fn has_instances(&self) -> bool {
        self.frame_instances + self.persistent_instances > 0
    }

    /// Some record an instance names this frame moves: the wind, within its limit, is not calm and
    /// the plant has height. The cull grows its bounds only then, so a calm frame is the frame of the
    /// same material without sway.
    pub(super) fn moving(&self) -> bool {
        self.records.iter().enumerate().any(|(i, record)| {
            if !self.used.get(i).copied().unwrap_or(false)
                && !self.persistent_used.get(i).copied().unwrap_or(false)
            {
                return false;
            }
            let live = self.live(record);
            let speed = (live.clock[2] * live.clock[2] + live.clock[3] * live.clock[3]).sqrt();
            speed.min(live.limit[0].max(0.0)) > CALM_SPEED && live.sway[1] > CALM_SPEED
        })
    }

    /// The uniform buffer, made the first time the path draws. True when it was made.
    pub(super) fn ensure_buffer(&mut self, d: &TrackedDevice) -> bool {
        if self.buffer.is_some() {
            return false;
        }
        self.buffer = Some(make_buffer(
            d,
            "vis sway table",
            SWAY_TABLE_BYTES,
            wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        ));
        true
    }

    pub(super) fn buffer(&self) -> Option<&wgpu::Buffer> {
        self.buffer.as_ref()
    }

    /// Writes the records this frame's clock and wind changed. A frame with no sway writes nothing.
    pub(super) fn write_records(&mut self, queue: &TrackedQueue) {
        let Some(buffer) = self.buffer.as_ref() else {
            return;
        };
        if self.records.is_empty() {
            return;
        }
        let live: Vec<GpuSwayRecord> = self.records.iter().map(|record| self.live(record)).collect();
        if live != self.written {
            queue.write_buffer(buffer, 16, bytemuck::cast_slice(&live));
            self.written = live;
        }
    }

    /// Writes the frame word: the main camera's eye and pixels per unit over the LOD threshold, which
    /// the voxel fade reads, so every pass moves an instance by the same amount.
    pub(super) fn write_frame(&mut self, queue: &TrackedQueue, frame: [f32; 4]) {
        let Some(buffer) = self.buffer.as_ref() else {
            return;
        };
        if self.records.is_empty() || frame == self.written_frame {
            return;
        }
        queue.write_buffer(buffer, 0, bytemuck::cast_slice(&frame));
        self.written_frame = frame;
    }

    /// `(instances, materials, forward per reason)` of this frame.
    pub(super) fn report(&self) -> (u32, u32, [u32; SwaySkip::ALL.len()]) {
        let mut slots = self.frame_slots.clone();
        slots.extend(self.persistent_slots.iter().copied());
        let mut forward = self.forward;
        forward[SwaySkip::TableFull.index()] += self.persistent_full;
        (
            self.frame_instances + self.persistent_instances,
            slots.len() as u32,
            forward,
        )
    }
}

/// Records `sway` on `instance`.
fn apply(instance: &mut GpuInstance, sway: InstanceSway) {
    instance.flags[1] = (instance.flags[1] & 0xff) | ((sway.record + 1) << 8);
    instance.scale[1] = sway.reach;
}

impl VisBufferPass {
    /// The renderer's material clock and scene wind for this frame: what the forward passes write
    /// into graph slot 0. `None` keeps what each material carries there.
    pub(crate) fn set_sway_clock(&mut self, clock: Option<(f32, f32)>, wind: Option<[f32; 2]>) {
        self.sway.clock = clock;
        self.sway.wind = wind;
    }

    /// The standard twin and record that draw `material` (a swaying material [`sway_gate`] accepts)
    /// on this path. `None` when the table is full. `material_key` is the key the frame knows it by.
    pub(crate) fn sway_plan(&mut self, material_key: u64, material: &LabMaterial) -> Option<SwayPlan> {
        if sway_gate(material) != SwayGate::Clusters {
            return None;
        }
        let sway = self.sway.sway_of(material)?;
        let twin_key = material_key ^ SWAY_TWIN_TAG;
        let fresh = self.sway.twins_seen.insert(twin_key);
        let frame = self.sway.frame;
        let twin = match self.sway.twins.get_mut(&twin_key) {
            Some(twin) => {
                if fresh {
                    refresh_twin(&mut twin.material, material);
                    twin.last_seen = frame;
                }
                Arc::clone(&twin.material)
            }
            None => {
                let twin = Arc::new(FoliageSway::plain(material)?);
                self.sway.twins.insert(
                    twin_key,
                    Twin {
                        material: Arc::clone(&twin),
                        last_seen: frame,
                    },
                );
                twin
            }
        };
        Some(SwayPlan {
            twin_key,
            twin,
            fresh,
            sway,
        })
    }

    /// Marks this frame's instance `index` (from a push) as swaying.
    pub(crate) fn set_instance_sway(&mut self, index: usize, sway: InstanceSway) {
        let Some(instance) = self.instances.get_mut(index) else {
            return;
        };
        apply(instance, sway);
        self.sway.frame_instances += 1;
        self.sway.frame_slots.insert(instance.info[3]);
        if let Some(used) = self.sway.used.get_mut(sway.record as usize) {
            *used = true;
        }
    }

    /// Counts a swaying mesh that stayed forward this frame.
    pub(crate) fn note_sway_forward(&mut self, skip: SwaySkip) {
        self.sway.forward[skip.index()] += 1;
    }

    /// Marks a persistent instance as swaying (`rebuild_persistent`).
    pub(super) fn apply_persistent_sway(&mut self, index: usize, sway: InstanceSway) {
        let Some(instance) = self.persistent.get_mut(index) else {
            return;
        };
        apply(instance, sway);
        self.sway.persistent_instances += 1;
        self.sway.persistent_slots.push(instance.info[3]);
        if let Some(used) = self.sway.persistent_used.get_mut(sway.record as usize) {
            *used = true;
        }
    }

    /// World boxes of the persistent shadow casters that sway (one per set, grown by the reach),
    /// while the main-view path and cluster shadows are active.
    pub(crate) fn sway_set_boxes(&self) -> &[[[f32; 3]; 2]] {
        if self.active && self.settings.cluster_shadows && self.sway.moving() {
            &self.sway_boxes
        } else {
            &[]
        }
    }

    /// The shadow-space world box of the swaying casters, including their scale, lift and sway reach.
    pub(super) fn sway_box_of(instances: &[GpuInstance]) -> Option<[[f32; 3]; 2]> {
        let mut bounds: Option<[[f32; 3]; 2]> = None;
        for instance in instances {
            if instance.flags[1] >> 8 == 0 || instance.flags[0] & super::SHADOW_CASTS == 0 {
                continue;
            }
            let origin = instance.model[3];
            let factor = instance.shadow[0];
            let center: [f32; 3] = std::array::from_fn(|axis| {
                origin[axis]
                    + (instance.sphere[axis] - origin[axis]) * factor
                    + if axis == 1 { instance.shadow[1] } else { 0.0 }
            });
            let radius = instance.sphere[3] * factor.abs() + instance.scale[1];
            let (low, high) = (
                std::array::from_fn(|i| center[i] - radius),
                std::array::from_fn(|i| center[i] + radius),
            );
            bounds = Some(match bounds {
                None => [low, high],
                Some([l, h]) => [
                    std::array::from_fn(|i: usize| l[i].min(low[i])),
                    std::array::from_fn(|i: usize| h[i].max(high[i])),
                ],
            });
        }
        bounds
    }

    /// Counts a persistent set that could not get a record: it draws at rest, since a set has no
    /// forward path.
    pub(super) fn note_persistent_full(&mut self) {
        self.sway.persistent_full += 1;
    }

    /// Test hook: where `mesh_key`'s clusters start in the cluster records.
    #[cfg(test)]
    pub(crate) fn meshlet_base(&self, mesh_key: u64) -> Option<u32> {
        self.meshes.get(&mesh_key).map(|slot| slot.meshlet_base)
    }

    /// Test hook: instances pushed from now on keep their rest bounds.
    #[cfg(test)]
    pub(crate) fn set_sway_reach_off(&mut self, off: bool) {
        self.sway.reach_off = off;
    }
}

/// `twin` with `material`'s current parameters, keeping its maps: the maps left `material` for its
/// graph when it was adopted, and their content is keyed by the material key.
fn refresh_twin(twin: &mut Arc<LabMaterial>, material: &LabMaterial) {
    let twin = Arc::make_mut(twin);
    let mut next = LabMaterial {
        surface_graph: None,
        ..material.clone()
    };
    next.albedo = twin.albedo.take();
    next.normal_map = twin.normal_map.take();
    next.occlusion_map = twin.occlusion_map.take();
    next.emissive_map = twin.emissive_map.take();
    next.compressed = twin.compressed.take();
    next.images = std::mem::replace(&mut twin.images, crate::MaterialImages::EMPTY);
    *twin = next;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adopted() -> LabMaterial {
        FoliageSway::default()
            .adopt(&LabMaterial {
                base_color: [0.4, 0.6, 0.2, 1.0],
                albedo: Some(crate::LabTexture {
                    width: 1,
                    height: 1,
                    rgba: vec![9, 8, 7, 255],
                }),
                ..LabMaterial::default()
            })
            .expect("adopt")
    }

    #[test]
    fn the_record_matches_the_shader_layout() {
        assert_eq!(std::mem::size_of::<GpuSwayRecord>(), 48);
        assert_eq!(SWAY_TABLE_BYTES, 16 + 256 * 48);
        let shader = include_str!("../../../shaders/foliage_sway.wgsl");
        assert!(shader.contains(&format!("array<SwayRecord, {SWAY_TABLE_RECORDS}>")));
    }

    #[test]
    fn one_record_serves_every_material_with_the_same_sway() {
        let mut table = SwayTable::default();
        let a = table.sway_of(&adopted()).expect("first");
        let mut tinted = adopted();
        tinted.base_color = [1.0, 0.0, 0.0, 1.0];
        assert_eq!(table.sway_of(&tinted).expect("same"), a);
        let stiff = FoliageSway {
            stiffness: 0.08,
            ..FoliageSway::default()
        }
        .adopt(&adopted())
        .expect("readopt");
        let b = table.sway_of(&stiff).expect("second");
        assert_ne!(b.record, a.record);
        assert_eq!(a.reach, FoliageSway::default().max_offset());
    }

    #[test]
    fn a_full_table_refuses_and_keeps_what_it_has() {
        let mut table = SwayTable::default();
        for i in 0..SWAY_TABLE_RECORDS {
            let sway = FoliageSway {
                stiffness: 0.01 + i as f32 * 1.0e-4,
                ..FoliageSway::default()
            };
            assert!(
                table
                    .sway_of(&sway.adopt(&LabMaterial::default()).expect("adopt"))
                    .is_some()
            );
        }
        let past = FoliageSway {
            stiffness: 0.9,
            ..FoliageSway::default()
        };
        assert_eq!(
            table.sway_of(&past.adopt(&LabMaterial::default()).expect("adopt")),
            None
        );
        let again = FoliageSway {
            stiffness: 0.01,
            ..FoliageSway::default()
        };
        assert!(
            table
                .sway_of(&again.adopt(&LabMaterial::default()).expect("adopt"))
                .is_some()
        );
    }

    /// Sequential material edits consume no more records than simultaneous users need.
    #[test]
    fn stale_records_are_reused_after_material_churn() {
        let mut table = SwayTable::default();
        for frame in 0..SWAY_TABLE_RECORDS + 16 {
            table.begin_frame();
            let material = FoliageSway {
                stiffness: 0.1 + frame as f32 * 1.0e-4,
                ..FoliageSway::default()
            }
            .adopt(&LabMaterial::default())
            .expect("adopt");
            let record = table
                .sway_of(&material)
                .unwrap_or_else(|| panic!("one live sway exhausted the table at frame {frame}"));
            table.used[record.record as usize] = true;
        }
        assert_eq!(table.records.len(), SWAY_TABLE_RECORDS);
        assert_eq!(table.index.len(), SWAY_TABLE_RECORDS, "old keys are removed");
    }

    /// Reusing old records preserves persistent sets and every plan claimed this frame.
    #[test]
    fn stale_record_reuse_preserves_persistent_and_current_frame_claims() {
        let mut table = SwayTable::default();
        let held = adopted();
        let first = table.sway_of(&held).expect("persistent record");
        table.persistent_used[first.record as usize] = true;
        let material = |index: usize| {
            FoliageSway {
                stiffness: 0.1 + index as f32 * 1.0e-4,
                ..FoliageSway::default()
            }
            .adopt(&LabMaterial::default())
            .expect("adopt")
        };
        for index in 0..SWAY_TABLE_RECORDS - 1 {
            table.sway_of(&material(index)).expect("fill the table");
        }
        table.begin_frame();
        let mut records = HashSet::new();
        records.insert(first.record);
        for index in SWAY_TABLE_RECORDS..2 * SWAY_TABLE_RECORDS - 1 {
            let graph = material(index);
            let fresh = table.sway_of(&graph).expect("reuse a stale record");
            assert!(records.insert(fresh.record), "a claimed record was replaced");
            assert_eq!(table.sway_of(&graph), Some(fresh), "a repeated plan is stable");
        }
        assert_eq!(table.sway_of(&held), Some(first), "the persistent index holds");
        assert!(table.sway_of(&material(2 * SWAY_TABLE_RECORDS)).is_none());
        assert!(
            table.sway_of(&material(0)).is_none(),
            "an old key cannot alias its replacement"
        );
        assert_eq!(records.len(), SWAY_TABLE_RECORDS);
    }

    #[test]
    fn only_a_moving_record_an_instance_names_grows_the_bounds() {
        let mut table = SwayTable::default();
        let sway = table.sway_of(&adopted()).expect("record");
        assert!(!table.moving(), "no instance names it");
        table.used[sway.record as usize] = true;
        assert!(!table.moving(), "the slot's wind is calm");
        table.wind = Some([6.0, 0.0]);
        assert!(table.moving());
        table.wind = Some([0.0, 0.0]);
        assert!(!table.moving());
        table.wind = Some([6.0, 0.0]);
        table.records[0].limit[0] = 0.0;
        assert!(!table.moving(), "a zero limit stops the sway");
    }

    #[test]
    fn the_twin_keeps_its_maps_and_takes_new_parameters() {
        let mut pass = VisBufferPass::default();
        let mut material = adopted();
        let plan = pass.sway_plan(7, &material).expect("plan");
        assert!(plan.fresh);
        assert!(plan.twin.surface_graph.is_none());
        assert_eq!(
            plan.twin.albedo.as_ref().map(|t| t.rgba.clone()),
            Some(vec![9, 8, 7, 255])
        );
        drop(plan);
        material.base_color = [0.1, 0.2, 0.3, 1.0];
        let same_frame = pass.sway_plan(7, &material).expect("plan");
        assert!(!same_frame.fresh, "one refresh per key per frame");
        drop(same_frame);
        pass.sway.begin_frame();
        let next = pass.sway_plan(7, &material).expect("plan");
        assert!(next.fresh);
        assert_eq!(next.twin.base_color, [0.1, 0.2, 0.3, 1.0]);
        assert!(next.twin.albedo.is_some());
        assert_eq!(next.twin_key, 7 ^ SWAY_TWIN_TAG);
        assert_eq!(next.sway.reach, FoliageSway::default().max_offset());
    }

    /// What the draw list calls for a swaying mesh, and what the frame reports of it.
    #[test]
    fn the_report_counts_instances_materials_and_forward_reasons() {
        let mut pass = VisBufferPass::default();
        let material = adopted();
        let plan = pass.sway_plan(7, &material).expect("plan");
        let zeroed: GpuInstance = bytemuck::Zeroable::zeroed();
        for slot in [3, 3, 9] {
            pass.instances.push(GpuInstance {
                info: [0, 0, 0, slot],
                ..zeroed
            });
        }
        for index in 0..3 {
            pass.set_instance_sway(index, plan.sway);
        }
        pass.set_instance_sway(99, plan.sway);
        assert_eq!(pass.instances[0].flags[1] >> 8, plan.sway.record + 1);
        assert_eq!(pass.instances[0].scale[1], plan.sway.reach);
        pass.note_sway_forward(SwaySkip::GraphSurface);
        pass.note_sway_forward(SwaySkip::GraphSurface);
        pass.note_sway_forward(SwaySkip::Other);
        let (instances, materials, forward) = pass.sway.report();
        assert_eq!((instances, materials), (3, 2));
        assert_eq!(forward[SwaySkip::GraphSurface.index()], 2);
        assert_eq!(forward[SwaySkip::Other.index()], 1);
        assert!(pass.sway_set_boxes().is_empty());
        pass.sway.begin_frame();
        assert_eq!(pass.sway.report(), (0, 0, [0; SwaySkip::ALL.len()]));
    }

    /// A set's box holds every swaying instance's sphere grown by its reach, and joins the shadow class
    /// only while the sway moves.
    #[test]
    fn a_set_box_grows_by_the_reach_and_counts_only_while_moving() {
        let zeroed: GpuInstance = bytemuck::Zeroable::zeroed();
        let at = |x: f32, record: u32| GpuInstance {
            sphere: [x, 1.0, 0.0, 2.0],
            scale: [1.0, 0.5, 0.0, 0.0],
            shadow: [1.0, 0.0, 0.0, 0.0],
            flags: [super::super::SHADOW_CASTS | 0xff, record << 8, 0, 0],
            ..zeroed
        };
        assert_eq!(
            VisBufferPass::sway_box_of(&[at(0.0, 0)]),
            None,
            "a still instance"
        );
        let bounds = VisBufferPass::sway_box_of(&[at(0.0, 1), at(10.0, 1), at(99.0, 0)]).expect("a box");
        assert_eq!(bounds, [[-2.5, -1.5, -2.5], [12.5, 3.5, 2.5]]);
        let mut pass = VisBufferPass::default();
        pass.set_active(true);
        pass.sway_boxes.push(bounds);
        let sway = pass.sway.sway_of(&adopted()).expect("record");
        pass.sway.persistent_used[sway.record as usize] = true;
        assert!(pass.sway_set_boxes().is_empty(), "calm air keeps the pages");
        pass.set_sway_clock(None, Some([5.0, 0.0]));
        assert_eq!(pass.sway_set_boxes().len(), 1);
    }

    #[test]
    fn inactive_paths_and_disabled_cluster_shadows_refresh_no_set_pages() {
        let mut pass = VisBufferPass::default();
        pass.sway_boxes.push([[-1.0; 3], [1.0; 3]]);
        let sway = pass.sway.sway_of(&adopted()).expect("record");
        pass.sway.persistent_used[sway.record as usize] = true;
        pass.set_sway_clock(None, Some([5.0, 0.0]));
        pass.set_active(true);
        assert_eq!(pass.sway_set_boxes().len(), 1, "active caster control");
        pass.set_active(false);
        let inactive_empty = pass.sway_set_boxes().is_empty();
        pass.set_active(true);
        pass.settings.cluster_shadows = false;
        let disabled_empty = pass.sway_set_boxes().is_empty();
        assert_eq!(
            (inactive_empty, disabled_empty),
            (true, true),
            "path off and cluster shadows off must each have no caster"
        );
    }

    #[test]
    fn non_casting_swaying_instances_do_not_enlarge_dynamic_shadow_boxes() {
        let zeroed: GpuInstance = bytemuck::Zeroable::zeroed();
        let at = |x: f32, casts: bool| GpuInstance {
            sphere: [x, 1.0, 0.0, 2.0],
            scale: [1.0, 0.5, 0.0, 0.0],
            shadow: [1.0, 0.0, 0.0, 0.0],
            flags: [
                if casts {
                    super::super::SHADOW_CASTS | 0xff
                } else {
                    0
                },
                1 << 8,
                0,
                0,
            ],
            ..zeroed
        };
        assert_eq!(VisBufferPass::sway_box_of(&[at(99.0, false)]), None);
        assert_eq!(
            VisBufferPass::sway_box_of(&[at(0.0, true), at(99.0, false)]),
            Some([[-2.5, -1.5, -2.5], [2.5, 3.5, 2.5]]),
            "an unshadowed or masked instance must not dirty distant pages"
        );
    }

    #[test]
    fn set_shadow_bounds_follow_the_caster_scale_and_lift() {
        let zeroed: GpuInstance = bytemuck::Zeroable::zeroed();
        let instance = GpuInstance {
            model: [
                [1.0, 0.0, 0.0, 0.0],
                [0.0, 1.0, 0.0, 0.0],
                [0.0, 0.0, 1.0, 0.0],
                [10.0, 0.0, -2.0, 1.0],
            ],
            sphere: [11.0, 1.0, -1.0, 2.0],
            scale: [1.0, 0.5, 0.0, 0.0],
            shadow: [2.0, 3.0, 100.0, 0.0],
            flags: [
                super::super::SHADOW_CASTS | super::super::SHADOW_GPU_MASK | (4 << 12),
                1 << 8,
                0,
                0,
            ],
            ..zeroed
        };
        assert_eq!(
            VisBufferPass::sway_box_of(&[instance]),
            Some([[7.5, 0.5, -4.5], [16.5, 9.5, 4.5]]),
            "transform the rest sphere about the object origin, then grow by the world-space sway bound"
        );
    }

    /// The sweep runs every `TWIN_KEEP_FRAMES` frames and takes a twin not seen for that many: one seen
    /// after the last sweep survives the next, and goes at the one after.
    #[test]
    fn a_twin_nobody_draws_is_dropped_after_its_frames() {
        let mut pass = VisBufferPass::default();
        pass.sway.begin_frame();
        pass.sway_plan(7, &adopted()).expect("plan");
        assert_eq!(pass.sway.twins.len(), 1);
        for _ in 1..TWIN_KEEP_FRAMES {
            pass.sway.begin_frame();
        }
        assert_eq!(pass.sway.frame, TWIN_KEEP_FRAMES, "the first sweep ran");
        assert_eq!(pass.sway.twins.len(), 1, "young enough");
        for _ in 0..TWIN_KEEP_FRAMES {
            pass.sway.begin_frame();
        }
        assert!(pass.sway.twins.is_empty());
    }
}
