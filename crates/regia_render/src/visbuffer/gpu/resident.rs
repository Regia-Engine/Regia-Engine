// SPDX-License-Identifier: MPL-2.0
//! Cluster residency scheduling against the cluster arena (ADR-3118; the plan, the demand test and
//! the knobs are in `visbuffer/residency.rs`).
//!
//! Per frame, after the instance lists are complete and before the arena uploads: each managed
//! mesh's demand is the finest band a visible instance's cut can select (the last main view);
//! bands in flight continue within the frame's upload bound and switch on once whole; then the
//! meshes that want a finer band ask for the next one, largest on screen first, evicting the least
//! recently needed bands of meshes that do not need them when the budget is full. Records and
//! level runs are rewritten only where they change, and every byte counts toward the bound.

use std::sync::Arc;

use super::super::residency::{
    BandDelivery, BandDisk, BandRam, ClusterResidency, Managed, MeshPlan, Pending, RESIDENCY_BANDS,
    ResidencyView, VisBufferResidencyStats, validate_band,
};
use super::{MeshSlot, VERTEX_WORDS, VisBufferPass, levels, push_vertex};
use crate::LabVertex;
use crate::visbuffer::meshlet::{Meshlet, MeshletMesh};

/// Bytes of one cluster record.
const RECORD_BYTES: u64 = std::mem::size_of::<Meshlet>() as u64;
/// Bytes of one level run.
const RUN_BYTES: u64 = std::mem::size_of::<levels::LevelRun>() as u64;

/// What residency did with a mesh made resident.
pub(super) enum Admission {
    /// Managed: its base is resident.
    Managed,
    /// Its base does not fit the budget or the arena: it draws forward for now.
    Refused,
    /// One band, or not stored level by level: it takes the whole-DAG path.
    Whole,
}

/// A rewrite of a mesh's records and runs for a new step.
struct Flip {
    records: Vec<Meshlet>,
    /// `[start, end)` runs of changed records.
    changed: Vec<(usize, usize)>,
    changed_records: usize,
    runs: Vec<levels::LevelRun>,
    runs_changed: bool,
}

impl Flip {
    fn bytes(&self) -> u64 {
        self.changed_records as u64 * RECORD_BYTES
            + if self.runs_changed {
                self.runs.len() as u64 * RUN_BYTES
            } else {
                0
            }
    }
}

impl VisBufferPass {
    /// The residency knobs in use: the runtime override ([`super::super::VisBufferSettings::residency`]),
    /// else the profile's.
    pub(crate) fn residency_config(&self) -> ClusterResidency {
        self.settings.residency.unwrap_or(self.residency.profile)
    }

    /// The profile's residency knobs (`RenderProfile::cluster_residency`; the frame's hookup calls
    /// it each main-view frame).
    pub(crate) fn set_profile_residency(&mut self, residency: ClusterResidency) {
        self.residency.profile = residency;
    }

    /// What residency did.
    pub(crate) fn residency_stats(&self) -> VisBufferResidencyStats {
        let r = &self.residency;
        let mut stats = VisBufferResidencyStats {
            config: self.residency_config(),
            meshes: r.managed.len() as u32,
            unmanaged_meshes: self.meshes.len().saturating_sub(r.managed.len()) as u32,
            resident_bytes: r.resident_bytes,
            refused_meshes: (r.refused.len() + r.failed.iter().filter(|k| !r.refused.contains_key(k)).count())
                as u32,
            ram_bytes: r.ram_bytes,
            ..r.stats
        };
        for m in r.managed.values() {
            stats.full_bytes += m.plan.full_bytes(RECORD_BYTES);
            stats.pending_bands += u32::from(m.pending.is_some());
            for band in 0..m.plan.band_count().min(RESIDENCY_BANDS) {
                stats.bands_total[band] += 1;
                stats.bands_resident[band] += u32::from(band >= m.step);
            }
            for band in 0..m.plan.band_count() {
                match &m.band_ram[band] {
                    BandRam::Disk => stats.bands_on_disk += 1,
                    BandRam::Reading => {
                        stats.bands_loading += 1;
                    }
                    BandRam::Ram { .. } => stats.bands_in_ram += 1,
                }
            }
        }
        stats.read_queue_depth = stats.bands_loading;
        stats
    }

    /// The frame's upload allowance (each frame's start).
    pub(super) fn residency_begin_frame(&mut self) {
        let config = self.residency_config();
        self.residency.upload_left = if config.enabled {
            config.upload_bytes_per_frame
        } else {
            u64::MAX
        };
        self.residency.bound = self.residency.upload_left;
        self.residency.stats.upload_bytes = 0;
        self.residency.stats.base_upload_bytes = 0;
        self.residency.io_issued = 0;
        self.residency.stats.io_bytes = 0;
    }

    /// Test hook: the main view the next frame's demand reads (the cull writes it in
    /// `geometry_passes`).
    #[cfg(test)]
    pub(super) fn note_residency_view(&mut self, view: super::super::residency::ResidencyView) {
        self.residency.view = Some(view);
    }

    /// True when `mesh_key`'s base did not fit the budget, or its pak would not read (it draws
    /// forward).
    pub(super) fn residency_refused(&self, mesh_key: u64) -> bool {
        self.residency.refused.contains_key(&mesh_key) || self.residency.failed.contains(&mesh_key)
    }

    /// True when residency manages `mesh_key` (its vertices are not contiguous: no UV2).
    pub(super) fn residency_manages(&self, mesh_key: u64) -> bool {
        self.residency.managed.contains_key(&mesh_key)
    }

    /// Forgets a refusal (the mesh was re-inserted or released).
    pub(super) fn residency_forget(&mut self, mesh_key: u64) {
        self.residency.refused.remove(&mesh_key);
        self.residency.retry_need.remove(&mesh_key);
        self.residency.failed.remove(&mesh_key);
    }

    /// Makes `mesh_key`'s base resident under the budget: its records (every cluster) and its
    /// coarsest band's block. A stream-shaped mesh reads its sections from the pak now (the base
    /// must be resident before the first draw; ADR-3708).
    pub(super) fn residency_admit(
        &mut self,
        mesh_key: u64,
        mesh: &Arc<MeshletMesh>,
        vertices: &[LabVertex],
        index_count: usize,
    ) -> Admission {
        let _span =
            tracing::info_span!("visbuffer_residency_admit", clusters = mesh.meshlets.len()).entered();
        // A retried refusal makes its room first: when the evictions cannot happen this frame,
        // it is refused again without reading the pak or building the plan (before, that read
        // and rebuild ran every frame until the evictions fit; corr-render-visbuffer N).
        if let Some(need) = self.residency.retry_need.remove(&mesh_key) {
            let budget = self.residency_config().budget_bytes;
            if self.residency.resident_bytes + need > budget && !self.residency_evict_for(need) {
                self.residency.refused.insert(mesh_key, need);
                return Admission::Refused;
            }
        }
        let appended = &mesh.appended_vertices.0;
        let mut packed = Vec::with_capacity((vertices.len() + appended.len()) * VERTEX_WORDS as usize);
        for v in vertices.iter().chain(appended) {
            push_vertex(&mut packed, v);
        }
        // A stream-shaped mesh: the plan build and the base band need the cooked sections once
        // (transient; finer bands arrive band by band from the disk).
        let stream_sections = match &mesh.disk {
            Some(disk) => match disk.read_sections() {
                Ok(sections) => Some(sections),
                Err(err) => {
                    tracing::warn!(
                        target: "regia_render",
                        mesh_key,
                        code = err.code,
                        "meshlet band sections not read ({}); the mesh draws forward",
                        err.message
                    );
                    self.residency.stats.read_errors += 1;
                    self.residency.stats.last_read_error_code = Some(err.code);
                    self.residency.failed.insert(mesh_key);
                    return Admission::Refused;
                }
            },
            None => None,
        };
        let plan = match (&stream_sections, &mesh.disk) {
            (Some(sections), Some(_)) => MeshPlan::build_with(
                &mesh.meshlets,
                mesh.bounds,
                &sections.vertices,
                packed,
                VERTEX_WORDS,
            ),
            _ => MeshPlan::build(mesh, packed, VERTEX_WORDS),
        };
        let Some(plan) = plan else {
            return Admission::Whole;
        };
        let Ok(clusters) = u32::try_from(mesh.meshlets.len()) else {
            return Admission::Whole;
        };
        let base = plan.base();
        let base_words = plan.bands[base].words;
        let base_bytes = plan.bands[base].bytes() + u64::from(clusters) * RECORD_BYTES;
        let config = self.residency_config();
        if self.residency.resident_bytes + base_bytes > config.budget_bytes
            && !self.residency_evict_for(base_bytes)
        {
            self.residency.refused.insert(mesh_key, base_bytes);
            return Admission::Refused;
        }
        let Some((key, ranges)) = self.arena.alloc(base_words, clusters) else {
            self.arena_refused.insert(mesh_key);
            return Admission::Refused;
        };
        let start = ranges.geo.0;
        let vertex_count = (vertices.len() + appended.len()).max(mesh.base_vertex_count());
        let mut addr = vec![0u32; vertex_count];
        let block = match (&stream_sections, &mesh.disk) {
            (Some(sections), Some(_)) => {
                let Some((lists, tris)) = plan.band_parts(&mesh.meshlets, sections, base) else {
                    self.arena.free(key);
                    return Admission::Whole;
                };
                if let Err(reason) = validate_band(
                    &mesh.meshlets,
                    mesh.source_vertex_count,
                    &plan.bands[base].clusters,
                    &lists,
                    &tris,
                ) {
                    tracing::warn!(
                        target: "regia_render",
                        mesh_key,
                        "meshlet base band corrupt ({}); the mesh draws forward",
                        reason
                    );
                    self.residency.stats.read_errors += 1;
                    self.residency.stats.last_read_error_code = Some("band_corrupt");
                    self.arena.free(key);
                    // Sticky, same as a missing pak: the next frame must not read it again.
                    self.residency.failed.insert(mesh_key);
                    return Admission::Refused;
                }
                plan.block_from_parts(base, start, &mut addr, &lists, &tris)
            }
            _ => plan.block(mesh, base, start, &mut addr),
        };
        self.arena.write_geo(start, &block);
        let mut starts = vec![None; plan.band_count()];
        starts[base] = Some(start);
        // Voxel far levels do not sway (ADR-3916): the same flag and start as a whole-mesh upload.
        let switch = crate::visbuffer::sway::voxel_switch(mesh);
        let voxel = super::voxel_start(mesh, switch);
        let records: Vec<Meshlet> = mesh
            .meshlets
            .iter()
            .enumerate()
            .map(|(i, m)| super::sway_flagged(plan.record(m, i, base, &starts), m.level, switch))
            .collect();
        self.arena.write_clusters(ranges.clusters.0, &records);
        let runs = levels::runs_of(&records, mesh.bounds);
        #[cfg(test)]
        let runs = if self.levels_off { Vec::new() } else { runs };
        let run_range = self.levels.insert(&runs);
        let run_bytes = runs.len() as u64 * RUN_BYTES;
        self.mesh_runs.insert(mesh_key, runs);
        if self.set_draws(mesh_key) {
            self.persistent_dirty = true;
        }
        self.meshes.insert(
            mesh_key,
            MeshSlot {
                meshlet_base: ranges.clusters.0,
                meshlet_count: clusters,
                vertex_count: vertices.len(),
                index_count,
                bounds: mesh.bounds,
                key,
                vertex_base: start,
                runs: run_range,
                voxel,
            },
        );
        let bands = plan.band_count();
        let plan_bytes = plan_ram_bytes(&plan, mesh);
        self.residency.managed.insert(
            mesh_key,
            Managed {
                plan,
                source: Arc::clone(mesh),
                disk: mesh.disk.clone(),
                band_ram: vec![BandRam::Disk; bands],
                band_failed: vec![false; bands],
                plan_bytes,
                step: base,
                starts,
                keys: vec![None; bands],
                addr,
                records,
                pending: None,
                last_needed: vec![self.residency.frame; bands],
                demand: base,
                priority: 0.0,
                cluster_base: ranges.clusters.0,
                bytes: base_bytes,
            },
        );
        self.residency.by_base.insert(ranges.clusters.0, mesh_key);
        self.residency.resident_bytes += base_bytes;
        self.residency.stats.base_upload_bytes += base_bytes + run_bytes;
        self.residency.stats.peak_resident_bytes = self
            .residency
            .stats
            .peak_resident_bytes
            .max(self.residency.resident_bytes);
        self.refresh_ram_stats();
        Admission::Managed
    }

    /// Frees the finer bands (and a band in flight) of a managed mesh that leaves the arena; its
    /// base goes with the mesh's own range.
    pub(super) fn residency_drop(&mut self, mesh_key: u64) {
        let Some(m) = self.residency.managed.remove(&mesh_key) else {
            return;
        };
        self.residency.by_base.remove(&m.cluster_base);
        for key in m.keys.iter().flatten() {
            self.arena.free(*key);
        }
        if let Some(p) = &m.pending {
            self.arena.free(p.key);
        }
        self.residency.resident_bytes = self.residency.resident_bytes.saturating_sub(m.bytes);
        // The in-flight reads belong to this admission. A later delivery must not land on a
        // mesh that reuses the key (a shorter DAG panics on the old band index; an error
        // sticks band_failed on the new admission).
        self.residency.req_map.retain(|_, entry| entry.0 != mesh_key);
        // Room was made: refused bases may fit now.
        self.residency.refused.clear();
        self.refresh_ram_stats();
    }

    /// One frame of residency: demand, deliveries from the pak, uploads in flight, budget,
    /// requests. Runs once per frame before the arena's upload.
    pub(super) fn residency_frame(&mut self) {
        let config = self.residency_config();
        if self.residency.managed.is_empty() {
            if config.enabled {
                self.residency.stats.frames += 1;
                self.residency.stats.degraded_meshes = 0;
            }
            return;
        }
        let _span =
            tracing::info_span!("visbuffer_residency", meshes = self.residency.managed.len()).entered();
        self.residency.frame += 1;
        if !config.enabled {
            self.residency_drain();
            return;
        }
        self.residency_demand();
        self.residency_retry_refused(config);
        let pending: Vec<u64> = self
            .residency
            .managed
            .iter()
            .filter(|(_, m)| m.pending.is_some())
            .map(|(k, _)| *k)
            .collect();
        // Bands written last frame switch on first: their records need the frame's allowance.
        for &key in &pending {
            self.flip_refine(key);
        }
        for key in pending {
            if self.write_pending(key) {
                self.flip_refine(key);
            }
        }
        // Deliveries from the pak: payloads to RAM, errors to the counters (ADR-3708).
        self.residency_deliveries();
        // A budget lowered at runtime: evict down to it (needed bands stay).
        while self.residency.resident_bytes > config.budget_bytes {
            if !self.evict_one() {
                break;
            }
        }
        self.residency_requests(config);
        // RAM budget: LRU eviction of delivered payloads back to the on-disk state.
        self.residency_ram_evict(config);
        self.residency_fallbacks();
        self.refresh_ram_stats();
        let r = &mut self.residency;
        r.stats.frames += 1;
        r.stats.peak_upload_bytes = r.stats.peak_upload_bytes.max(r.stats.upload_bytes);
        r.stats.peak_resident_bytes = r.stats.peak_resident_bytes.max(r.resident_bytes);
        r.stats.peak_ram_bytes = r.stats.peak_ram_bytes.max(r.ram_bytes);
    }

    /// Forgets the budget refusals whose base now fits: the free budget plus the resident bands
    /// no instance needs this frame (what `evict_one` may take) covers the base. The next
    /// `ensure_mesh` admits the mesh again and evicts for it. A refusal is otherwise only lifted
    /// when an eviction happens, and with the budget not exceeded none does, so a mesh refused
    /// while another's fine bands were needed drew forward for good after they stopped being
    /// needed.
    fn residency_retry_refused(&mut self, config: ClusterResidency) {
        if self.residency.refused.is_empty() {
            return;
        }
        let r = &self.residency;
        let mut room = config.budget_bytes.saturating_sub(r.resident_bytes);
        for m in r.managed.values() {
            if m.pending.is_some() {
                continue;
            }
            // `evict_one` takes bands `step..` while they are finer than the demand and the base.
            let end = m.demand.min(m.plan.base());
            for band in m.plan.bands.get(m.step..end).unwrap_or_default() {
                room = room.saturating_add(band.bytes());
            }
        }
        let r = &mut self.residency;
        let retry_need = &mut r.retry_need;
        r.refused.retain(|key, need| {
            if *need > room {
                return true;
            }
            retry_need.insert(*key, *need);
            false
        });
    }

    /// Each managed mesh's demand from the frame's instances and the last main view, merged with
    /// the shadow jobs' recorded needs (ADR-3708).
    ///
    /// The needs are those of the last shadow cull, kept until the next cull replaces them
    /// (ADR-7672): the renderer culls the jobs only on frames that redraw a shadow map, and a map
    /// cached in between still shows that cut, so its bands stay wanted. Consuming them after one
    /// frame left a band only the shadow cut needs stuck in RAM: its read is delivered a frame
    /// after the request, when no cull had run. With cluster shadows off no map draws clusters,
    /// and the needs clear.
    fn residency_demand(&mut self) {
        let frame = self.residency.frame;
        if !self.settings.cluster_shadows {
            self.residency.shadow_needs.clear();
        }
        let (persistent, instances, residency) = (&self.persistent, &self.instances, &mut self.residency);
        for m in residency.managed.values_mut() {
            m.demand = m.plan.base();
            m.priority = 0.0;
        }
        if let Some(view) = residency.view {
            for inst in persistent.iter().chain(instances) {
                let Some(m) = residency
                    .by_base(inst.info[0])
                    .and_then(|key| residency.managed.get_mut(&key))
                else {
                    continue;
                };
                let center = [inst.sphere[0], inst.sphere[1], inst.sphere[2]];
                let radius = inst.sphere[3];
                if !view.inside(center, radius) {
                    continue;
                }
                let distance = view.distance(center);
                m.demand = m.demand.min(m.plan.demand(&view, distance, inst.scale[0]));
                let px = radius * view.pixels_per_unit / (distance - radius).max(view.near.max(1e-4));
                if px.is_finite() {
                    m.priority = m.priority.max(px);
                }
            }
        }
        // Shadow cuts ask for their own bands at their own priority (ADR-3708).
        for (key, &(demand, priority)) in &residency.shadow_needs {
            if let Some(m) = residency.managed.get_mut(key) {
                m.demand = m.demand.min(demand);
                m.priority = m.priority.max(priority);
            }
        }
        for m in residency.managed.values_mut() {
            for band in m.demand..m.plan.band_count() {
                m.last_needed[band] = frame;
            }
        }
    }

    /// Drains every disk's delivery queue: payloads land in RAM (counted against the RAM budget),
    /// errors mark the band failed (sticky) and are counted (ADR-3708).
    fn residency_deliveries(&mut self) {
        let frame = self.residency.frame;
        // A disk can be shared by several mesh keys: drain each distinct disk once.
        let mut seen: Vec<usize> = Vec::new();
        let disks: Vec<Arc<dyn BandDisk>> = self
            .residency
            .managed
            .values()
            .filter_map(|m| m.disk.clone())
            .collect();
        for disk in disks {
            let ptr = Arc::as_ptr(&disk) as *const () as usize;
            if seen.contains(&ptr) {
                continue;
            }
            seen.push(ptr);
            while let Some(delivery) = disk.try_deliver() {
                self.land_delivery(delivery, frame);
            }
        }
    }

    /// Routes one delivery by its serial: a payload becomes the band's RAM state, an error marks
    /// the band failed (the mesh keeps its coarsest resident level).
    fn land_delivery(&mut self, delivery: BandDelivery, frame: u64) {
        let BandDelivery { tag, result } = delivery;
        let Some((key, band)) = self.residency.req_map.remove(&tag) else {
            return;
        };
        // A later read of this mesh and band is still outstanding. This delivery belongs to
        // an admission that was replaced.
        if self
            .residency
            .req_map
            .values()
            .any(|entry| entry.0 == key && entry.1 == band)
        {
            return;
        }
        let Some(m) = self.residency.managed.get_mut(&key) else {
            return;
        };
        // Stale: the key was re-admitted with fewer bands than the in-flight read, or the band
        // is not the read this admission is waiting on.
        if band >= m.band_ram.len()
            || band >= m.band_failed.len()
            || band >= m.plan.bands.len()
            || !matches!(m.band_ram[band], BandRam::Reading)
        {
            return;
        }
        match result {
            Ok(payload) => {
                let bytes = (payload.lists.len() + payload.tris.len()) as u64 * 4;
                match validate_band(
                    &m.source.meshlets,
                    m.source.source_vertex_count,
                    &m.plan.bands[band].clusters,
                    &payload.lists,
                    &payload.tris,
                ) {
                    Ok(()) => {
                        m.band_ram[band] = BandRam::Ram {
                            lists: payload.lists,
                            tris: payload.tris,
                            arrived: frame,
                        };
                        self.residency.stats.io_bytes += bytes;
                        self.residency.stats.io_bytes_total += bytes;
                    }
                    Err(reason) => {
                        m.band_failed[band] = true;
                        m.band_ram[band] = BandRam::Disk;
                        self.residency.stats.read_errors += 1;
                        self.residency.stats.last_read_error_code = Some("band_corrupt");
                        tracing::warn!(
                            target: "regia_render",
                            key,
                            band,
                            "meshlet band corrupt ({}): the mesh draws its coarse level",
                            reason
                        );
                    }
                }
            }
            Err(err) => {
                m.band_ram[band] = BandRam::Disk;
                m.band_failed[band] = true;
                self.residency.stats.read_errors += 1;
                self.residency.stats.last_read_error_code = Some(err.code);
                tracing::warn!(
                    target: "regia_render",
                    key,
                    band,
                    code = err.code,
                    "meshlet band not read ({}): the mesh draws its coarse level",
                    err.message
                );
            }
        }
    }

    /// Records the shadow jobs' per-mesh band demand (ADR-3708): the finest band any caster
    /// inside a job's region can select under the job's texel cut (`vis_shadow.wgsl`'s
    /// `projected_error`, mirrored conservatively), at the caster's projected size as its
    /// priority. Consumed by the next frame's `residency_demand`.
    pub(super) fn residency_note_shadow_demand(&mut self, jobs: &[super::VisShadowJob]) {
        self.residency.shadow_needs.clear();
        if jobs.is_empty() || self.residency.managed.is_empty() {
            return;
        }
        let threshold = self.settings.shadow_lod_threshold_px.max(0.0);
        for job in jobs {
            let (texels, perspective, light) = super::shadow::job_lod(&job.view_proj, job.resolution);
            let sides = super::shadow::region_planes(&job.view_proj, job.ndc_rect);
            let mut planes = [[0.0f32, 0.0, 0.0, 1.0]; 6];
            planes[..4].copy_from_slice(&sides);
            let view = ResidencyView {
                eye: light,
                planes,
                pixels_per_unit: texels,
                near: super::shadow::PERSPECTIVE_NEAR,
                threshold_px: threshold,
            };
            for inst in self.persistent.iter().chain(&self.instances) {
                // Only cluster-shadow casters draw into the jobs.
                if inst.flags[0] & super::SHADOW_CASTS == 0 {
                    continue;
                }
                let Some(key) = self.residency.by_base(inst.info[0]) else {
                    continue;
                };
                let center = [inst.sphere[0], inst.sphere[1], inst.sphere[2]];
                let radius = inst.sphere[3];
                // The job tests its region's side planes only; near/far accept.
                let inside = sides
                    .iter()
                    .all(|p| p[0] * center[0] + p[1] * center[1] + p[2] * center[2] + p[3] >= -radius);
                if !inside {
                    continue;
                }
                let shadow_scale = if inst.shadow[0].is_finite() {
                    inst.shadow[0]
                } else {
                    1.0
                };
                let scale = inst.scale[0] * shadow_scale;
                let distance = perspective.then(|| view.distance(center));
                let demand = self.residency.managed.get(&key).map(|m| {
                    m.plan
                        .demand_shadow(texels, threshold, distance, view.near, scale)
                });
                let Some(demand) = demand else {
                    continue;
                };
                // The caster's projected size in shadow texels: the request order.
                let d = distance.unwrap_or(1.0);
                let px = radius * texels / (d - radius).max(super::shadow::PERSPECTIVE_NEAR);
                let priority = if px.is_finite() { px.max(0.0) } else { 0.0 };
                let entry = self
                    .residency
                    .shadow_needs
                    .entry(key)
                    .or_insert((demand, priority));
                entry.0 = entry.0.min(demand);
                entry.1 = entry.1.max(priority);
            }
        }
    }

    /// Starts the next finer band of each mesh that wants one, largest on screen first: a
    /// delivered payload uploads within the frame's bounds; a missing one is requested from the
    /// pak within the IO bound (stream-shaped meshes), or cut from RAM (ADR-3708).
    fn residency_requests(&mut self, config: ClusterResidency) {
        let mut asks: Vec<(f32, u64)> = self
            .residency
            .managed
            .iter()
            .filter(|(_, m)| m.pending.is_none() && m.demand < m.step)
            .map(|(key, m)| (m.priority, *key))
            .collect();
        asks.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
        let mut degraded = 0u32;
        for (_, key) in asks {
            let Some(m) = self.residency.managed.get(&key) else {
                continue;
            };
            let band = m.step - 1;
            let words = m.plan.bands[band].words;
            let bytes = m.plan.bands[band].bytes();
            let streamed = m.disk.is_some();
            let ram_ready = matches!(m.band_ram[band], BandRam::Ram { .. });
            if streamed && !ram_ready {
                // No payload yet: issue the read within the frame's IO bound (one per mesh). The
                // estimate is what the read moves (lists and triangles, not the owned vertices
                // already in RAM). A band larger than the whole bound still goes as the frame's
                // first read, as `fits` lets an oversized flip through; otherwise it never loads.
                let estimate = m.plan.read_bytes(band);
                let over = self.residency.io_issued > 0
                    && self.residency.io_issued.saturating_add(estimate) > config.io_bytes_per_frame;
                if m.band_failed[band] || matches!(m.band_ram[band], BandRam::Reading) || over {
                    degraded += 1;
                    continue;
                }
                let Some(disk) = m.disk.clone() else {
                    continue;
                };
                let serial = self.residency.req_serial.wrapping_add(1);
                self.residency.req_serial = serial;
                let clusters = m.plan.bands[band].clusters.clone();
                disk.request_band(serial, &clusters);
                self.residency.req_map.insert(serial, (key, band));
                self.residency.io_issued += estimate;
                if let Some(m) = self.residency.managed.get_mut(&key) {
                    m.band_ram[band] = BandRam::Reading;
                }
                continue;
            }
            // The upload allowance is spent: this mesh waits, but a later one may still issue its
            // pak read (above) this frame.
            if self.residency.upload_left == 0 {
                continue;
            }
            // Upload start: the budget, then the arena.
            if self.residency.resident_bytes + bytes > config.budget_bytes && !self.residency_evict_for(bytes)
            {
                degraded += 1;
                continue;
            }
            let Some((range, start)) = self.arena.alloc_geo(words) else {
                degraded += 1;
                continue;
            };
            // Take the payload out of RAM (or cut it from the cooked mesh) and assemble the block.
            let assembled = self.residency.managed.get_mut(&key).map(|m| {
                let block = match m.take_ram(band) {
                    Some((lists, tris)) => m.plan.block_from_parts(band, start, &mut m.addr, &lists, &tris),
                    None => m.plan.block(&m.source, band, start, &mut m.addr),
                };
                (block, m.plan.bands[band].bytes())
            });
            let Some((block, band_bytes)) = assembled else {
                self.arena.free(range);
                continue;
            };
            let Some(m) = self.residency.managed.get_mut(&key) else {
                self.arena.free(range);
                continue;
            };
            m.pending = Some(Pending {
                band,
                key: range,
                start,
                block,
                written: 0,
            });
            m.bytes += band_bytes;
            self.residency.resident_bytes += band_bytes;
            if self.write_pending(key) {
                self.flip_refine(key);
            }
        }
        self.residency.stats.degraded_meshes = degraded;
    }

    /// Drops delivered payloads, least recently arrived first, until the managed meshes fit the
    /// RAM budget. A payload mid-upload (pending) and the retained plans stay (ADR-3708).
    fn residency_ram_evict(&mut self, config: ClusterResidency) {
        while self.residency.ram_bytes > config.ram_budget_bytes {
            let mut best: Option<(u64, usize, u64)> = None;
            for (key, m) in &self.residency.managed {
                if m.pending.is_some() {
                    continue;
                }
                for (band, state) in m.band_ram.iter().enumerate() {
                    if let BandRam::Ram { arrived, .. } = state
                        && best.is_none_or(|(_, _, at)| *arrived < at)
                    {
                        best = Some((*key, band, *arrived));
                    }
                }
            }
            let Some((key, band, _)) = best else {
                break;
            };
            if let Some(m) = self.residency.managed.get_mut(&key) {
                let _ = m.take_ram(band);
            }
            self.residency.stats.ram_evictions += 1;
            self.refresh_ram_stats();
        }
    }

    /// Counts the frames a visible mesh draws coarser than its demand (its finer band loads,
    /// failed, or waits for budget): the coarsest resident level covered it (ADR-3708).
    fn residency_fallbacks(&mut self) {
        let mut fallbacks = 0u64;
        for m in self.residency.managed.values() {
            if m.priority > 0.0 && m.demand < m.step {
                fallbacks += 1;
            }
        }
        self.residency.stats.fallbacks += fallbacks;
    }

    /// Recomputes [`Residency::ram_bytes`] (retained plans, delivered payloads, blocks in
    /// flight) after any change.
    fn refresh_ram_stats(&mut self) {
        let mut bytes = 0u64;
        for m in self.residency.managed.values() {
            bytes += m.plan_bytes;
            for band in &m.band_ram {
                bytes += band.bytes();
            }
            if let Some(p) = &m.pending {
                bytes += p.block.len() as u64 * 4;
            }
        }
        self.residency.ram_bytes = bytes;
    }

    /// Writes as much of `key`'s band in flight as the frame's bound allows. True once all of it is
    /// written.
    fn write_pending(&mut self, key: u64) -> bool {
        let left_words = usize::try_from(self.residency.upload_left / 4).unwrap_or(usize::MAX);
        let Some(p) = self
            .residency
            .managed
            .get_mut(&key)
            .and_then(|m| m.pending.as_mut())
        else {
            return false;
        };
        let n = (p.block.len() - p.written).min(left_words);
        if n > 0 {
            self.arena
                .write_geo(p.start + p.written as u32, &p.block[p.written..p.written + n]);
            p.written += n;
        }
        let done = p.written == p.block.len();
        if n > 0 {
            self.residency.charge(n as u64 * 4);
        }
        done
    }

    /// The records and runs of `key` at `step` with `starts`, against what is uploaded.
    fn flip_for(&self, key: u64, step: usize, starts: &[Option<u32>]) -> Option<Flip> {
        let m = self.residency.managed.get(&key)?;
        let switch = crate::visbuffer::sway::voxel_switch(&m.source);
        let records: Vec<Meshlet> = m
            .source
            .meshlets
            .iter()
            .enumerate()
            .map(|(i, src)| super::sway_flagged(m.plan.record(src, i, step, starts), src.level, switch))
            .collect();
        let mut changed: Vec<(usize, usize)> = Vec::new();
        let mut changed_records = 0;
        for (i, (new, old)) in records.iter().zip(&m.records).enumerate() {
            if bytemuck::bytes_of(new) != bytemuck::bytes_of(old) {
                changed_records += 1;
                match changed.last_mut() {
                    Some((_, end)) if *end == i => *end = i + 1,
                    _ => changed.push((i, i + 1)),
                }
            }
        }
        let old_runs = self.mesh_runs.get(&key).map_or(&[][..], Vec::as_slice);
        let runs = if old_runs.is_empty() {
            Vec::new()
        } else {
            levels::runs_of(&records, m.source.bounds)
        };
        let runs_changed = runs.as_slice() != old_runs;
        Some(Flip {
            records,
            changed,
            changed_records,
            runs,
            runs_changed,
        })
    }

    /// Uploads `flip` for `key` (its bytes already checked).
    fn apply_flip(&mut self, key: u64, flip: Flip) {
        let bytes = flip.bytes();
        let Some(slot) = self.meshes.get(&key).copied() else {
            return;
        };
        for &(start, end) in &flip.changed {
            self.arena
                .write_clusters(slot.meshlet_base + start as u32, &flip.records[start..end]);
        }
        if flip.runs_changed && flip.runs.len() == slot.runs.1 as usize {
            self.levels.overwrite(slot.runs.0, &flip.runs);
            self.mesh_runs.insert(key, flip.runs);
        }
        if let Some(m) = self.residency.managed.get_mut(&key) {
            m.records = flip.records;
        }
        self.residency.charge(bytes);
        // The mesh's drawn geometry changed with no draw-list change, so the static-camera
        // screen reuse misses from the next frame (`sets_generation`). The cached shadow maps
        // and pages hold cluster casters: they are redrawn (`take_sets_changed`) only when an
        // instance of this mesh casts — a non-casting flip (a receiver, or shadows off) leaves
        // them untouched.
        let base = self.residency.managed.get(&key).map(|m| m.cluster_base);
        let casts_shadow = base.is_some_and(|base| {
            self.persistent
                .iter()
                .chain(&self.instances)
                .any(|inst| inst.info[0] == base && inst.flags[0] & super::SHADOW_CASTS != 0)
        });
        self.sets_changed |= casts_shadow;
        self.sets_generation = self.sets_generation.wrapping_add(1);
    }

    /// Switches `key`'s fully written band in flight on, when its records fit the frame's bound.
    fn flip_refine(&mut self, key: u64) -> bool {
        let Some((band, start, range)) = self
            .residency
            .managed
            .get(&key)
            .and_then(|m| m.pending.as_ref())
            .filter(|p| p.written == p.block.len())
            .map(|p| (p.band, p.start, p.key))
        else {
            return false;
        };
        let Some(mut starts) = self.residency.managed.get(&key).map(|m| m.starts.clone()) else {
            return false;
        };
        starts[band] = Some(start);
        let Some(flip) = self.flip_for(key, band, &starts) else {
            return false;
        };
        if !self.residency.fits(flip.bytes()) {
            return false;
        }
        self.apply_flip(key, flip);
        if let Some(m) = self.residency.managed.get_mut(&key) {
            m.pending = None;
            m.starts = starts;
            m.keys[band] = Some(range);
            m.step = band;
        }
        self.residency.stats.refinements += 1;
        true
    }

    /// Evicts until `bytes` more fit the budget. False when not enough can go.
    fn residency_evict_for(&mut self, bytes: u64) -> bool {
        let budget = self.residency_config().budget_bytes;
        while self.residency.resident_bytes + bytes > budget {
            if !self.evict_one() {
                return false;
            }
        }
        true
    }

    /// Evicts the least recently needed finest band of a mesh that does not need it this frame.
    /// False when there is none (or its records do not fit the frame's bound).
    fn evict_one(&mut self) -> bool {
        let Some((_, key)) = self
            .residency
            .managed
            .iter()
            .filter(|(_, m)| m.pending.is_none() && m.step < m.plan.base() && m.step < m.demand)
            .map(|(key, m)| (m.last_needed[m.step], *key))
            .min()
        else {
            return false;
        };
        let Some((band, mut starts, range, band_bytes, demand)) = self.residency.managed.get(&key).map(|m| {
            (
                m.step,
                m.starts.clone(),
                m.keys[m.step],
                m.plan.bands[m.step].bytes(),
                m.demand,
            )
        }) else {
            return false;
        };
        starts[band] = None;
        let Some(flip) = self.flip_for(key, band + 1, &starts) else {
            return false;
        };
        if !self.residency.fits(flip.bytes()) {
            return false;
        }
        self.apply_flip(key, flip);
        // The records no longer name the band: its range is free for this frame's writes (queue
        // writes land before the next submit, after the frames that drew it).
        if let Some(range) = range {
            self.arena.free(range);
        }
        if let Some(m) = self.residency.managed.get_mut(&key) {
            m.starts = starts;
            m.keys[band] = None;
            m.step = band + 1;
            m.bytes = m.bytes.saturating_sub(band_bytes);
        }
        let r = &mut self.residency;
        r.resident_bytes = r.resident_bytes.saturating_sub(band_bytes);
        r.stats.evictions += 1;
        r.stats.evicted_bytes += band_bytes;
        r.refused.clear();
        #[cfg(test)]
        r.eviction_log.push((key, band, demand));
        #[cfg(not(test))]
        let _ = demand;
        true
    }

    /// Residency turned off with managed meshes: every band of each, at once (the whole-DAG
    /// records), outside any budget or bound. Stream-shaped meshes read their sections once,
    /// synchronously (the whole DAG is requested at once; ADR-3708).
    fn residency_drain(&mut self) {
        let keys: Vec<u64> = self.residency.managed.keys().copied().collect();
        for key in keys {
            // Nothing left to drain: every band is resident (or the next one failed), and no band
            // is in flight. Managed meshes stay managed while residency is off, so without this
            // a stream-shaped mesh re-read its whole pak, blocking, on every frame.
            let Some(m) = self.residency.managed.get(&key) else {
                continue;
            };
            let wants_band = m.step > 0 && !m.band_failed[m.step - 1];
            if m.pending.is_none() && !wants_band {
                continue;
            }
            // A stream-shaped mesh reads its pak only when the next band fits the arena: with the
            // arena full, it re-read the whole pak (blocking) every frame and then found no room
            // (corr-render-visbuffer M). The probe takes nothing: the range goes straight back.
            let next_words = if wants_band {
                m.plan.bands[m.step - 1].words
            } else {
                0
            };
            let has_disk = m.disk.is_some();
            let room = !wants_band
                || !has_disk
                || self
                    .arena
                    .alloc_geo(next_words)
                    .is_some_and(|(probe, _)| self.arena.free(probe));
            let Some(m) = self.residency.managed.get(&key) else {
                continue;
            };
            let disk = if wants_band && room { m.disk.clone() } else { None };
            let streamed = m.disk.is_some();
            // Materialize the sections of a stream-shaped mesh for the whole loop.
            let mut sections_failed = false;
            let stream_sections = match disk {
                Some(disk) => match disk.read_sections() {
                    Ok(sections) => Some(sections),
                    Err(err) => {
                        self.residency.stats.read_errors += 1;
                        self.residency.stats.last_read_error_code = Some(err.code);
                        tracing::warn!(
                            target: "regia_render",
                            key,
                            code = err.code,
                            "meshlet sections not read while draining ({}); the mesh keeps its resident bands",
                            err.message
                        );
                        // Sticky, as a failed band read: the next frame must not read it again.
                        if let Some(m) = self.residency.managed.get_mut(&key) {
                            let step = m.step;
                            m.band_failed[..step].fill(true);
                        }
                        sections_failed = true;
                        None
                    }
                },
                None => None,
            };
            if self.write_pending(key) {
                self.flip_refine(key);
            }
            // A streamed mesh's lists are not in `source`. Building the finer bands from that
            // empty mesh publishes a zero block and switches those bands on.
            if sections_failed || (streamed && stream_sections.is_none()) {
                continue;
            }
            while let Some(m) = self.residency.managed.get(&key) {
                if m.step == 0 || m.pending.is_some() || m.band_failed[m.step - 1] {
                    break;
                }
                let band = m.step - 1;
                let words = m.plan.bands[band].words;
                let bytes = m.plan.bands[band].bytes();
                let Some((range, start)) = self.arena.alloc_geo(words) else {
                    break;
                };
                // A streamed band is checked as a delivered one is (`residency_deliveries`): a
                // corrupt band stays off and the mesh keeps drawing its coarser level.
                let mut corrupt = None;
                let block = self
                    .residency
                    .managed
                    .get_mut(&key)
                    .and_then(|m| match &stream_sections {
                        Some(sections) => {
                            let (lists, tris) = m.plan.band_parts(&m.source.meshlets, sections, band)?;
                            if let Err(reason) = validate_band(
                                &m.source.meshlets,
                                m.source.source_vertex_count,
                                &m.plan.bands[band].clusters,
                                &lists,
                                &tris,
                            ) {
                                m.band_failed[band] = true;
                                corrupt = Some(reason);
                                return None;
                            }
                            Some(m.plan.block_from_parts(band, start, &mut m.addr, &lists, &tris))
                        }
                        None => Some(m.plan.block(&m.source, band, start, &mut m.addr)),
                    });
                if let Some(reason) = corrupt {
                    self.residency.stats.read_errors += 1;
                    self.residency.stats.last_read_error_code = Some("band_corrupt");
                    tracing::warn!(
                        target: "regia_render",
                        key,
                        band,
                        "meshlet band corrupt while draining ({}): the mesh draws its coarse level",
                        reason
                    );
                }
                let Some(block) = block else {
                    self.arena.free(range);
                    break;
                };
                let Some(m) = self.residency.managed.get_mut(&key) else {
                    self.arena.free(range);
                    break;
                };
                m.pending = Some(Pending {
                    band,
                    key: range,
                    start,
                    block,
                    written: 0,
                });
                m.bytes += bytes;
                self.residency.resident_bytes += bytes;
                if !(self.write_pending(key) && self.flip_refine(key)) {
                    break;
                }
            }
        }
    }
}

/// Bytes a managed mesh's retained plan holds in RAM: the packed vertices, the cluster records
/// and the band metadata (ADR-3708).
fn plan_ram_bytes(plan: &MeshPlan, mesh: &MeshletMesh) -> u64 {
    let bands = plan
        .bands
        .iter()
        .map(|b| (b.clusters.len() + b.vertices.len()) as u64 * 4)
        .sum::<u64>();
    plan.packed_words() as u64 * 4
        + mesh.meshlets.len() as u64 * std::mem::size_of::<Meshlet>() as u64
        + plan.band_of.len() as u64
        + bands
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ShadowCast;
    use crate::visbuffer::residency::{ABSENT_ERROR, ResidencyView};

    /// A dense rock (a UV sphere) and its cooked cluster DAG.
    fn rock() -> (Vec<LabVertex>, Vec<u32>, Arc<MeshletMesh>) {
        let (vertices, indices) = crate::uv_sphere(1.0, 96, 48);
        let positions: Vec<[f32; 3]> = vertices.iter().map(|v| v.position).collect();
        let mesh = regia_asset_pipeline::build_meshlet_mesh(
            &positions,
            &indices,
            &regia_asset_pipeline::MeshletSettings::default(),
        )
        .expect("cook");
        // The cooker links its own copy of this crate (a dev-dependency): cross by the format.
        let mesh = MeshletMesh::from_bytes(&mesh.to_bytes()).expect("round trip");
        assert!(mesh.level_count() >= 3, "levels {}", mesh.level_count());
        (vertices, indices, Arc::new(mesh))
    }

    fn translate(x: f32, y: f32, z: f32) -> [[f32; 4]; 4] {
        [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [x, y, z, 1.0],
        ]
    }

    /// A view that sees everything (frustum planes that accept any sphere), at 1080p with a 0.9 rad
    /// field of view.
    fn view(eye: [f32; 3]) -> ResidencyView {
        ResidencyView {
            eye,
            planes: [[0.0, 0.0, 0.0, 1.0]; 6],
            pixels_per_unit: 1080.0 / (2.0 * 0.45f32.tan()),
            near: 0.1,
            threshold_px: 1.0,
        }
    }

    struct Scene {
        vertices: Vec<LabVertex>,
        indices: Vec<u32>,
        mesh: Arc<MeshletMesh>,
        /// (mesh key, position)
        rocks: Vec<(u64, [f32; 3])>,
    }

    /// A `BandDisk` over RAM: sections at once, band payloads answered in request order through
    /// a queue the test drains by stepping frames (no wall clock, ADR-3708).
    struct MemoryDisk {
        meshlets: Arc<[Meshlet]>,
        sections: crate::visbuffer::BandSections,
        /// When set, `read_sections` fails with this code (a missing pak). Interior mutability
        /// so a test can arm the failure after admission.
        fail_sections: std::sync::Mutex<Option<&'static str>>,
        /// When set, band requests fail with this code (a corrupt band).
        fail_bands: Option<&'static str>,
        /// `read_sections` returns triangles whose local indices are 255.
        ///
        /// Vertex indices stay in range, so the plan still builds. `validate_band` then
        /// rejects the base (a local index past the cluster) and the admit must stick.
        corrupt_triangles: bool,
        /// Like `corrupt_triangles`, but only from the second `read_sections` on (the admission
        /// reads a sound base; a later drain reads corrupt finer bands).
        corrupt_after_first: bool,
        /// How many times `read_sections` ran.
        reads: std::sync::atomic::AtomicU32,
        queue: std::sync::Mutex<std::collections::VecDeque<crate::visbuffer::BandDelivery>>,
    }

    impl MemoryDisk {
        fn new(mesh: &MeshletMesh) -> Self {
            Self {
                meshlets: Arc::from(mesh.meshlets.clone()),
                sections: crate::visbuffer::BandSections {
                    vertices: mesh.vertices.clone(),
                    triangles: mesh.triangles.clone(),
                },
                fail_sections: std::sync::Mutex::new(None),
                fail_bands: None,
                corrupt_triangles: false,
                corrupt_after_first: false,
                reads: std::sync::atomic::AtomicU32::new(0),
                queue: std::sync::Mutex::new(std::collections::VecDeque::new()),
            }
        }

        /// Sections read, but every triangle local index is 255.
        fn corrupt(mut self) -> Self {
            self.corrupt_triangles = true;
            self
        }

        /// Sections read soundly once, then with every triangle local index 255.
        fn corrupt_after_first(mut self) -> Self {
            self.corrupt_after_first = true;
            self
        }

        fn reads(&self) -> u32 {
            self.reads.load(std::sync::atomic::Ordering::Relaxed)
        }

        /// Fails the band requests with `code` (the sections read still succeeds).
        fn failing_bands(mut self, code: &'static str) -> Self {
            self.fail_bands = Some(code);
            self
        }

        /// Fails every read with `code`, the sections too (a missing pak).
        fn failing_sections(self, code: &'static str) -> Self {
            self.fail_sections_from_now(code);
            self
        }

        /// From now on, `read_sections` fails with `code` (a pak that disappears after admission).
        fn fail_sections_from_now(&self, code: &'static str) {
            *self.fail_sections.lock().expect("section flag") = Some(code);
        }

        fn section_failure(&self) -> Option<&'static str> {
            *self.fail_sections.lock().expect("section flag")
        }

        fn error(code: &'static str) -> crate::visbuffer::BandDiskError {
            crate::visbuffer::BandDiskError {
                code,
                message: format!("injected {code}"),
            }
        }
    }

    impl crate::visbuffer::BandDisk for MemoryDisk {
        fn read_sections(&self) -> Result<crate::visbuffer::BandSections, crate::visbuffer::BandDiskError> {
            let earlier = self.reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if let Some(code) = self.section_failure() {
                return Err(Self::error(code));
            }
            let mut triangles = self.sections.triangles.clone();
            if self.corrupt_triangles || (self.corrupt_after_first && earlier > 0) {
                triangles.fill(0x00FF_FFFF);
            }
            Ok(crate::visbuffer::BandSections {
                vertices: self.sections.vertices.clone(),
                triangles,
            })
        }

        fn request_band(&self, tag: u64, clusters: &[u32]) {
            let delivery = if let Some(code) = self.fail_bands.or(self.section_failure()) {
                crate::visbuffer::BandDelivery {
                    tag,
                    result: Err(Self::error(code)),
                }
            } else {
                let mut lists = Vec::new();
                let mut tris = Vec::new();
                for &c in clusters {
                    let m = &self.meshlets[c as usize];
                    lists.extend_from_slice(
                        &self.sections.vertices
                            [m.vertex_offset as usize..(m.vertex_offset + m.vertex_count) as usize],
                    );
                    tris.extend_from_slice(
                        &self.sections.triangles
                            [m.triangle_offset as usize..(m.triangle_offset + m.triangle_count) as usize],
                    );
                }
                crate::visbuffer::BandDelivery {
                    tag,
                    result: Ok(crate::visbuffer::BandPayload { lists, tris }),
                }
            };
            if let Ok(mut queue) = self.queue.lock() {
                queue.push_back(delivery);
            }
        }

        fn try_deliver(&self) -> Option<crate::visbuffer::BandDelivery> {
            self.queue.lock().ok()?.pop_front()
        }
    }

    /// The cooked rock as a stream-shaped mesh backed by a memory disk (its lists stay off the
    /// "RAM" mesh; the disk serves them).
    fn stream_rock(mesh: &Arc<MeshletMesh>) -> (Arc<MemoryDisk>, Arc<MeshletMesh>) {
        let head = MeshletMesh::parse_head(&mesh.to_bytes()).expect("head");
        let disk = Arc::new(MemoryDisk::new(mesh));
        let stream = MeshletMesh::from_head(head, disk.clone());
        assert!(stream.vertices.is_empty() && stream.disk.is_some());
        (disk, Arc::new(stream))
    }

    /// `n` rocks (each its own mesh key) down −Z, 3 m apart.
    fn scene(n: u64) -> Scene {
        let (vertices, indices, mesh) = rock();
        Scene {
            vertices,
            indices,
            mesh,
            rocks: (0..n).map(|i| (10 + i, [0.0, 0.0, -3.0 * i as f32])).collect(),
        }
    }

    fn pass_with(config: ClusterResidency, scene: &Scene) -> VisBufferPass {
        let mut pass = VisBufferPass::default();
        pass.settings.residency = Some(config);
        for &(key, _) in &scene.rocks {
            pass.insert_cooked(key, Arc::clone(&scene.mesh));
        }
        pass
    }

    /// One frame as the renderer runs it: frame start, the draw list (residency admits meshes), the
    /// prepare step's residency pass with the last view.
    fn frame(pass: &mut VisBufferPass, scene: &Scene, eye: [f32; 3]) -> VisBufferResidencyStats {
        pass.begin_frame();
        for &(key, at) in &scene.rocks {
            if pass.ensure_mesh(key, &scene.vertices, &scene.indices) {
                let model = translate(at[0], at[1], at[2]);
                pass.push_instance(key, 1, model, model, ShadowCast::Off, false);
            }
        }
        pass.note_residency_view(view(eye));
        pass.residency_frame();
        pass.residency_stats()
    }

    /// The eye down the row and back: 240 frames from z = 6 to z = −40 and back.
    fn path() -> Vec<[f32; 3]> {
        (0..240)
            .map(|f| {
                let t = if f < 120 {
                    f as f32 / 119.0
                } else {
                    (239 - f) as f32 / 119.0
                };
                [0.5, 0.3, 6.0 - 46.0 * t]
            })
            .collect()
    }

    fn full_bytes(scene: &Scene) -> u64 {
        let mut pass = pass_with(
            ClusterResidency {
                enabled: true,
                budget_bytes: u64::MAX,
                upload_bytes_per_frame: u64::MAX,
                ram_budget_bytes: u64::MAX / 4,
                io_bytes_per_frame: u64::MAX / 4,
            },
            scene,
        );
        frame(&mut pass, scene, [0.0, 0.0, 1.0e6]).full_bytes
    }

    /// Across a fixed camera path with a budget of a third of the whole-DAG bytes, the managed
    /// bytes never pass the budget, no frame uploads more than its bound, bands both load and are
    /// evicted, and no eviction takes a band the mesh's visible instances needed that frame.
    #[test]
    fn budget_and_upload_bound_hold_over_a_camera_path() {
        let scene = scene(14);
        let full = full_bytes(&scene);
        let config = ClusterResidency {
            enabled: true,
            budget_bytes: full / 3,
            upload_bytes_per_frame: 96 * 1024,
            ram_budget_bytes: u64::MAX / 4,
            io_bytes_per_frame: u64::MAX / 4,
        };
        let mut pass = pass_with(config, &scene);
        for (f, eye) in path().into_iter().enumerate() {
            let stats = frame(&mut pass, &scene, eye);
            assert!(
                stats.resident_bytes <= config.budget_bytes,
                "frame {f}: {} > {}",
                stats.resident_bytes,
                config.budget_bytes
            );
            assert!(
                stats.upload_bytes <= config.upload_bytes_per_frame,
                "frame {f}: uploaded {}",
                stats.upload_bytes
            );
            assert_eq!(stats.refused_meshes, 0, "every base fits");
        }
        let stats = pass.residency_stats();
        assert!(stats.refinements > 20, "bands loaded: {stats:?}");
        assert!(stats.evictions > 5, "bands evicted: {stats:?}");
        assert!(stats.peak_upload_bytes > 0 && stats.peak_upload_bytes <= config.upload_bytes_per_frame);
        assert_eq!(stats.meshes, 14);
        assert_eq!(stats.bound_overruns, 0, "every flip fit the bound");
        for &(key, band, demand) in &pass.residency.eviction_log {
            assert!(
                band < demand,
                "mesh {key}: evicted band {band} while it needed {demand}"
            );
        }
    }

    /// With room, the bands settle to exactly what the view needs; far away only the base stays
    /// needed and nothing more loads.
    #[test]
    fn bands_settle_to_the_demand_with_room() {
        let scene = scene(3);
        let config = ClusterResidency {
            enabled: true,
            budget_bytes: u64::MAX / 4,
            upload_bytes_per_frame: 256 * 1024,
            ram_budget_bytes: u64::MAX / 4,
            io_bytes_per_frame: u64::MAX / 4,
        };
        let mut pass = pass_with(config, &scene);
        for _ in 0..200 {
            frame(&mut pass, &scene, [0.0, 0.0, 2.5]);
        }
        let stats = pass.residency_stats();
        assert_eq!(stats.pending_bands, 0);
        assert_eq!(stats.degraded_meshes, 0);
        for m in pass.residency.managed.values() {
            assert_eq!(m.step, m.demand, "settled at the demand");
        }
        let near = &pass.residency.managed[&10];
        assert_eq!(near.step, 0, "the nearest rock needs its finest band");
        assert!(stats.bands_resident[0] >= 1);
        // Far away: nothing new loads, nothing is evicted (the budget has room).
        let mut far = pass_with(config, &scene);
        for _ in 0..50 {
            frame(&mut far, &scene, [0.0, 0.0, 5_000.0]);
        }
        let stats = far.residency_stats();
        assert_eq!(stats.refinements, 0);
        for m in far.residency.managed.values() {
            assert_eq!(m.step, m.plan.base());
        }
    }

    /// A band switches on only once its whole block is written: while in flight its clusters stay
    /// absent in the uploaded records, so no frame draws a hole.
    #[test]
    fn a_band_switches_on_only_when_whole() {
        let scene = scene(1);
        let config = ClusterResidency {
            enabled: true,
            budget_bytes: u64::MAX / 4,
            upload_bytes_per_frame: 2048,
            ram_budget_bytes: u64::MAX / 4,
            io_bytes_per_frame: u64::MAX / 4,
        };
        let mut pass = pass_with(config, &scene);
        let mut saw_pending = false;
        for _ in 0..4000 {
            frame(&mut pass, &scene, [0.0, 0.0, 2.5]);
            let m = &pass.residency.managed[&10];
            if let Some(p) = &m.pending {
                saw_pending |= p.written < p.block.len();
                for &i in &m.plan.bands[p.band].clusters {
                    assert_eq!(m.records[i as usize].lod_error, ABSENT_ERROR, "in flight: absent");
                    assert_eq!(m.records[i as usize].triangle_count, 0);
                }
            }
            if m.step == 0 {
                break;
            }
        }
        assert!(saw_pending, "a band took several frames");
        let m = &pass.residency.managed[&10];
        assert_eq!(m.step, 0, "it got there");
        for (i, (r, src)) in m.records.iter().zip(&m.source.meshlets).enumerate() {
            assert_eq!(
                r.lod_error, src.lod_error,
                "cluster {i}: the cooked error at step 0"
            );
            assert_eq!(r.triangle_count, src.triangle_count);
        }
    }

    /// A mesh larger than the budget stays at its base and says so; a base larger than the budget
    /// is refused and the mesh draws forward. Neither panics.
    #[test]
    fn a_mesh_larger_than_the_budget_degrades_and_a_base_too_large_is_refused() {
        let scene = scene(1);
        let mut probe = pass_with(
            ClusterResidency {
                enabled: true,
                budget_bytes: u64::MAX / 4,
                upload_bytes_per_frame: u64::MAX / 4,
                ram_budget_bytes: u64::MAX / 4,
                io_bytes_per_frame: u64::MAX / 4,
            },
            &scene,
        );
        let base = frame(&mut probe, &scene, [0.0, 0.0, 1.0e6]).resident_bytes;
        let config = ClusterResidency {
            enabled: true,
            budget_bytes: base + 16,
            upload_bytes_per_frame: 64 * 1024,
            ram_budget_bytes: u64::MAX / 4,
            io_bytes_per_frame: u64::MAX / 4,
        };
        let mut pass = pass_with(config, &scene);
        for _ in 0..20 {
            let stats = frame(&mut pass, &scene, [0.0, 0.0, 2.5]);
            assert!(stats.resident_bytes <= config.budget_bytes);
        }
        let stats = pass.residency_stats();
        assert_eq!(stats.degraded_meshes, 1, "{stats:?}");
        let m = &pass.residency.managed[&10];
        assert_eq!(m.step, m.plan.base(), "the coarsest band");
        // Smaller than the base: refused, forward.
        let tiny = ClusterResidency {
            enabled: true,
            budget_bytes: base / 2,
            upload_bytes_per_frame: 64 * 1024,
            ram_budget_bytes: u64::MAX / 4,
            io_bytes_per_frame: u64::MAX / 4,
        };
        let mut pass = pass_with(tiny, &scene);
        pass.set_active(true);
        let stats = frame(&mut pass, &scene, [0.0, 0.0, 2.5]);
        assert_eq!(stats.refused_meshes, 1);
        assert_eq!(stats.meshes, 0);
        assert!(!pass.ensure_mesh(10, &scene.vertices, &scene.indices));
        assert!(
            !pass.takes_cluster_lod(10),
            "a refused mesh's LOD chain draws it forward"
        );
        assert!(pass.has_meshlets(10), "its meshlets are kept for a retry");
    }

    /// Churn back and forth along the path grows the arena only during the first pass: later
    /// passes reuse the freed ranges (each growth is one buffer create at the next upload).
    #[test]
    fn churn_grows_the_arena_only_while_warming() {
        let scene = scene(14);
        let full = full_bytes(&scene);
        let config = ClusterResidency {
            enabled: true,
            budget_bytes: full / 3,
            upload_bytes_per_frame: 128 * 1024,
            ram_budget_bytes: u64::MAX / 4,
            io_bytes_per_frame: u64::MAX / 4,
        };
        let mut pass = pass_with(config, &scene);
        let mut growths = Vec::new();
        for _ in 0..3 {
            for eye in path() {
                frame(&mut pass, &scene, eye);
            }
            growths.push(pass.arena.stats().growths);
        }
        assert!(pass.residency_stats().evictions > 0);
        assert_eq!(growths[1], growths[0], "second pass grew the arena: {growths:?}");
        assert_eq!(growths[2], growths[0], "third pass grew the arena: {growths:?}");
    }

    /// Two runs of the same path give the same counters and records frame for frame.
    #[test]
    fn residency_is_deterministic() {
        let scene = scene(8);
        let full = full_bytes(&scene);
        let config = ClusterResidency {
            enabled: true,
            budget_bytes: full / 3,
            upload_bytes_per_frame: 64 * 1024,
            ram_budget_bytes: u64::MAX / 4,
            io_bytes_per_frame: u64::MAX / 4,
        };
        let run = || {
            let mut pass = pass_with(config, &scene);
            let stats: Vec<VisBufferResidencyStats> = path()
                .into_iter()
                .map(|eye| frame(&mut pass, &scene, eye))
                .collect();
            let records: Vec<Vec<u8>> = pass
                .residency
                .managed
                .values()
                .map(|m| bytemuck::cast_slice(&m.records).to_vec())
                .collect();
            (stats, records, pass.residency.eviction_log.clone())
        };
        let (a, b) = (run(), run());
        assert_eq!(a.0, b.0);
        assert_eq!(a.1, b.1);
        assert_eq!(a.2, b.2);
    }

    /// Turned off at runtime, managed meshes take every band at once (the cooked records), and a
    /// managed mesh refuses UV2 (its vertices are not contiguous).
    #[test]
    fn turning_off_loads_every_band_and_managed_meshes_refuse_uv2() {
        let scene = scene(2);
        let config = ClusterResidency {
            enabled: true,
            budget_bytes: u64::MAX / 4,
            upload_bytes_per_frame: 64 * 1024,
            ram_budget_bytes: u64::MAX / 4,
            io_bytes_per_frame: u64::MAX / 4,
        };
        let mut pass = pass_with(config, &scene);
        frame(&mut pass, &scene, [0.0, 0.0, 1.0e6]);
        let uv2 = vec![[0.0f32; 2]; scene.vertices.len()];
        assert!(!pass.ensure_uv2(10, &uv2), "managed: no UV2");
        pass.settings.residency = Some(ClusterResidency::OFF);
        frame(&mut pass, &scene, [0.0, 0.0, 1.0e6]);
        for m in pass.residency.managed.values() {
            assert_eq!(m.step, 0);
            for (r, src) in m.records.iter().zip(&m.source.meshlets) {
                assert_eq!(r.lod_error, src.lod_error);
            }
        }
        // Meshes made resident while off are whole and unmanaged.
        pass.insert_cooked(99, Arc::clone(&scene.mesh));
        assert!(pass.ensure_mesh(99, &scene.vertices, &scene.indices));
        assert_eq!(pass.residency_stats().unmanaged_meshes, 1);
    }

    /// With no runtime override the pass follows the profile's knobs; an override wins, and
    /// `Some(OFF)` turns residency off under a profile that has it on.
    #[test]
    fn the_override_wins_over_the_profile() {
        let scene = scene(1);
        let on = ClusterResidency::for_tier(crate::memory_budget::BudgetTier::Deck);
        let mut pass = VisBufferPass::default();
        pass.insert_cooked(10, Arc::clone(&scene.mesh));
        pass.set_profile_residency(on);
        assert_eq!(pass.residency_config(), on);
        frame(&mut pass, &scene, [0.0, 0.0, 2.5]);
        assert_eq!(pass.residency_stats().meshes, 1, "the profile turned it on");
        pass.settings.residency = Some(ClusterResidency::OFF);
        assert_eq!(pass.residency_config(), ClusterResidency::OFF);
        pass.settings.residency = None;
        assert_eq!(pass.residency_stats().config, on);
    }

    /// Residency off from the start manages nothing and counts nothing.
    #[test]
    fn off_manages_nothing() {
        let scene = scene(2);
        let mut pass = VisBufferPass::default();
        for &(key, _) in &scene.rocks {
            pass.insert_cooked(key, Arc::clone(&scene.mesh));
        }
        frame(&mut pass, &scene, [0.0, 0.0, 2.0]);
        let stats = pass.residency_stats();
        assert_eq!(stats.meshes, 0);
        assert_eq!(stats.unmanaged_meshes, 2);
        assert_eq!(stats.upload_bytes_total, 0);
        assert_eq!(stats.frames, 0);
    }

    /// A pass with the rocks inserted stream-shaped (a memory disk behind each).
    fn stream_pass(config: ClusterResidency, scene: &Scene, disk: &Arc<MeshletMesh>) -> VisBufferPass {
        let mut pass = VisBufferPass::default();
        pass.settings.residency = Some(config);
        for &(key, _) in &scene.rocks {
            pass.insert_cooked(key, Arc::clone(disk));
        }
        pass
    }

    /// Streamed bands load from the disk within the upload bound and settle at the demand; the
    /// disk states and IO bytes are reported.
    #[test]
    fn streamed_bands_load_from_the_disk_within_the_bounds() {
        let scene = scene(3);
        let (_, stream) = stream_rock(&scene.mesh);
        let config = ClusterResidency {
            enabled: true,
            budget_bytes: u64::MAX / 4,
            upload_bytes_per_frame: 64 * 1024,
            ram_budget_bytes: u64::MAX / 4,
            io_bytes_per_frame: 1024 * 1024,
        };
        let mut pass = stream_pass(config, &scene, &stream);
        let mut saw_loading = false;
        for f in 0..600 {
            let stats = frame(&mut pass, &scene, [0.0, 0.0, 2.5]);
            assert!(
                stats.resident_bytes <= config.budget_bytes,
                "frame {f}: {} resident",
                stats.resident_bytes
            );
            assert!(
                stats.upload_bytes <= config.upload_bytes_per_frame,
                "frame {f}: {} uploaded",
                stats.upload_bytes
            );
            saw_loading |= stats.bands_loading > 0;
        }
        let stats = pass.residency_stats();
        assert_eq!(stats.meshes, 3, "{stats:?}");
        assert!(stats.io_bytes_total > 0, "bands came over the disk: {stats:?}");
        assert!(stats.refinements > 0, "{stats:?}");
        assert!(saw_loading, "a read was in flight: {stats:?}");
        assert_eq!(stats.bound_overruns, 0, "{stats:?}");
        for m in pass.residency.managed.values() {
            assert_eq!(m.step, m.demand, "settled at the demand");
            assert!(m.disk.is_some());
        }
        // The nearest rock needs its finest band.
        assert_eq!(pass.residency.managed[&10].step, 0);
        assert!(
            pass.residency.managed[&12].step >= pass.residency.managed[&10].step,
            "the farther rock needs no more"
        );
    }

    /// Over the RAM budget, delivered payloads are evicted back to the on-disk state (and read
    /// again when the band is next requested); the RAM high-water stays bounded.
    #[test]
    fn the_ram_budget_evicts_payloads_back_to_disk() {
        let scene = scene(1);
        let (_, stream) = stream_rock(&scene.mesh);
        // One rock under a near eye wants every band; the VRAM budget (3/4 of the DAG) fills
        // with bands the mesh needs, so a finer payload cannot upload and dwells in RAM until
        // the RAM budget evicts it back to the on-disk state (and it is read again).
        let full = full_bytes(&scene);
        let config = ClusterResidency {
            enabled: true,
            budget_bytes: full * 3 / 4,
            upload_bytes_per_frame: 32 * 1024,
            ram_budget_bytes: 300 * 1024,
            io_bytes_per_frame: 1024 * 1024,
        };
        let mut pass = stream_pass(config, &scene, &stream);
        let mut evicted_at = None;
        for f in 0..400 {
            let stats = frame(&mut pass, &scene, [0.0, 0.0, 2.5]);
            assert!(
                stats.upload_bytes <= config.upload_bytes_per_frame,
                "frame {f}: {} uploaded",
                stats.upload_bytes
            );
            if stats.ram_evictions > 0 && evicted_at.is_none() {
                evicted_at = Some(f);
            }
        }
        let stats = pass.residency_stats();
        assert!(
            stats.ram_evictions > 0,
            "payloads evicted under the RAM budget: {stats:?}"
        );
        assert!(
            stats.io_bytes_total > 0,
            "the evicted bands were read again: {stats:?}"
        );
        // The high-water never exceeds the budget by more than one eviction quantum: the largest
        // band block (a payload mid-assembly) or the largest payload, whichever is in flight.
        let m = &pass.residency.managed[&10];
        let max_block = m.plan.bands.iter().map(|b| b.bytes()).max().unwrap_or(0);
        let payload_bytes = (scene.mesh.vertices.len() + scene.mesh.triangles.len()) as u64 * 4;
        let quantum = max_block.max(payload_bytes);
        assert!(
            stats.peak_ram_bytes <= config.ram_budget_bytes + quantum,
            "RAM high-water {} over the budget {} + one quantum {}",
            stats.peak_ram_bytes,
            config.ram_budget_bytes,
            quantum
        );
        let _ = evicted_at;
    }

    /// A corrupt band read gives an error code, counts, and the coarse level: no panic, no
    /// hole (the base band covers the mesh).
    #[test]
    fn a_corrupt_band_gives_an_error_code_and_the_coarse_level() {
        let scene = scene(1);
        let head = MeshletMesh::parse_head(&scene.mesh.to_bytes()).expect("head");
        let disk = Arc::new(MemoryDisk::new(&scene.mesh).failing_bands("band_corrupt"));
        let stream = Arc::new(MeshletMesh::from_head(head, disk));
        let config = ClusterResidency {
            enabled: true,
            budget_bytes: u64::MAX / 4,
            upload_bytes_per_frame: 64 * 1024,
            ram_budget_bytes: u64::MAX / 4,
            io_bytes_per_frame: u64::MAX / 4,
        };
        let mut pass = stream_pass(config, &scene, &stream);
        for _ in 0..30 {
            frame(&mut pass, &scene, [0.0, 0.0, 2.5]);
        }
        let stats = pass.residency_stats();
        assert_eq!(stats.meshes, 1, "admission read its base fine");
        assert!(stats.read_errors > 0, "{stats:?}");
        assert_eq!(stats.last_read_error_code, Some("band_corrupt"));
        assert!(
            stats.fallbacks > 0,
            "drawn coarse while the band failed: {stats:?}"
        );
        let m = &pass.residency.managed[&10];
        assert_eq!(m.step, m.plan.base(), "the coarsest resident level");
        assert_eq!(stats.refinements, 0, "no band switched on");
        // No hole: exactly the base band's clusters are resident (every chain's cover).
        let resident = m.records.iter().filter(|r| r.lod_error != ABSENT_ERROR).count();
        assert_eq!(resident, m.plan.bands[m.plan.base()].clusters.len());
    }

    /// A missing payload refuses the mesh at admission (it draws forward), reported not panicked.
    #[test]
    fn a_missing_payload_refuses_admission() {
        let scene = scene(1);
        let head = MeshletMesh::parse_head(&scene.mesh.to_bytes()).expect("head");
        let disk = Arc::new(MemoryDisk::new(&scene.mesh).failing_sections("band_not_found"));
        let stream = Arc::new(MeshletMesh::from_head(head, disk));
        let config = ClusterResidency {
            enabled: true,
            budget_bytes: u64::MAX / 4,
            upload_bytes_per_frame: 64 * 1024,
            ram_budget_bytes: u64::MAX / 4,
            io_bytes_per_frame: u64::MAX / 4,
        };
        let mut pass = stream_pass(config, &scene, &stream);
        let stats = frame(&mut pass, &scene, [0.0, 0.0, 2.5]);
        assert_eq!(stats.refused_meshes, 1, "{stats:?}");
        assert_eq!(stats.meshes, 0, "{stats:?}");
        assert_eq!(stats.last_read_error_code, Some("band_not_found"));
        assert!(!pass.ensure_mesh(10, &scene.vertices, &scene.indices));
    }

    /// A caster the main view never sees still gets its bands: the shadow jobs' demand refines
    /// it at the jobs' own priority.
    #[test]
    fn a_shadow_only_caster_gets_its_bands() {
        let scene = scene(1);
        let (_, stream) = stream_rock(&scene.mesh);
        let config = ClusterResidency {
            enabled: true,
            budget_bytes: u64::MAX / 4,
            upload_bytes_per_frame: 64 * 1024,
            ram_budget_bytes: u64::MAX / 4,
            io_bytes_per_frame: 1024 * 1024,
        };
        let mut pass = stream_pass(config, &scene, &stream);
        // A main view that excludes the rock (only z <= 2 - r passes).
        let mut excluded = view([0.0, 0.0, 6.0]);
        excluded.planes[0] = [0.0, 0.0, -1.0, -3.0];
        // A shadow job over the rock: an orthographic cascade looking down -Z at it.
        use regia_core::glam::camera::rh::proj::directx::orthographic;
        use regia_core::glam::camera::rh::view::look_at_mat4;
        let proj = orthographic(-2.0, 2.0, -2.0, 2.0, 0.1, 20.0);
        let look = look_at_mat4(
            regia_core::glam::Vec3::new(0.0, 0.0, 6.0),
            regia_core::glam::Vec3::ZERO,
            regia_core::glam::Vec3::Y,
        );
        let job = crate::visbuffer::VisShadowJob {
            view_proj: (proj * look).to_cols_array_2d(),
            ndc_rect: [-1.0, -1.0, 1.0, 1.0],
            resolution: 2048.0,
            cascade: Some(0),
        };
        // Without the shadow demand the rock stays at its base (the main view excludes it).
        for _ in 0..30 {
            pass.begin_frame();
            assert!(pass.ensure_mesh(10, &scene.vertices, &scene.indices));
            let model = translate(0.0, 0.0, 0.0);
            pass.push_instance(10, 1, model, model, crate::ShadowCast::All, false);
            pass.note_residency_view(excluded);
            pass.residency_frame();
        }
        let base = pass.residency_stats();
        assert_eq!(base.refinements, 0, "no demand, no bands: {base:?}");
        assert_eq!(
            pass.residency.managed[&10].step,
            pass.residency.managed[&10].plan.base()
        );
        // With the shadow demand it refines to the job's cut.
        for _ in 0..300 {
            pass.begin_frame();
            assert!(pass.ensure_mesh(10, &scene.vertices, &scene.indices));
            let model = translate(0.0, 0.0, 0.0);
            pass.push_instance(10, 1, model, model, crate::ShadowCast::All, false);
            pass.note_residency_view(excluded);
            pass.residency_note_shadow_demand(&[job]);
            pass.residency_frame();
            if pass.residency.managed[&10].step == 0 {
                break;
            }
        }
        let stats = pass.residency_stats();
        assert!(stats.refinements > 0, "the shadow cut asked for bands: {stats:?}");
        assert_eq!(pass.residency.managed[&10].step, 0, "finest band resident");
    }

    /// A shadow map cached between redraws keeps asking for its cut's bands (ADR-7672): the renderer
    /// culls the shadow jobs only on the frames that redraw a map (`take_sets_changed` or the first
    /// frame), so a band that only the shadow cut needs must still load, stream-shaped, on the
    /// frames in between. Its read is delivered a frame after the request, when no cull ran; the
    /// band then uploads and its flip redraws the map, whose cull asks for the next band.
    #[test]
    fn a_cached_shadow_map_keeps_asking_for_its_bands() {
        let scene = scene(1);
        let (_, stream) = stream_rock(&scene.mesh);
        let config = ClusterResidency {
            enabled: true,
            budget_bytes: u64::MAX / 4,
            upload_bytes_per_frame: 64 * 1024,
            ram_budget_bytes: u64::MAX / 4,
            io_bytes_per_frame: 1024 * 1024,
        };
        let mut pass = stream_pass(config, &scene, &stream);
        // The main view excludes the rock: only the shadow cut can ask for its bands.
        let mut excluded = view([0.0, 0.0, 6.0]);
        excluded.planes[0] = [0.0, 0.0, -1.0, -3.0];
        use regia_core::glam::camera::rh::proj::directx::orthographic;
        use regia_core::glam::camera::rh::view::look_at_mat4;
        let proj = orthographic(-2.0, 2.0, -2.0, 2.0, 0.1, 20.0);
        let look = look_at_mat4(
            regia_core::glam::Vec3::new(0.0, 0.0, 6.0),
            regia_core::glam::Vec3::ZERO,
            regia_core::glam::Vec3::Y,
        );
        let job = crate::visbuffer::VisShadowJob {
            view_proj: (proj * look).to_cols_array_2d(),
            ndc_rect: [-1.0, -1.0, 1.0, 1.0],
            resolution: 2048.0,
            cascade: Some(0),
        };
        let mut redraw = true;
        let mut culls = 0u32;
        for _ in 0..300 {
            pass.begin_frame();
            assert!(pass.ensure_mesh(10, &scene.vertices, &scene.indices));
            let model = translate(0.0, 0.0, 0.0);
            pass.push_instance(10, 1, model, model, crate::ShadowCast::All, false);
            pass.note_residency_view(excluded);
            pass.residency_frame();
            // The frame's shadow pass: a redraw culls the job (recording its cut's needs); a
            // cached map culls nothing.
            if redraw {
                pass.residency_note_shadow_demand(&[job]);
                culls += 1;
            }
            redraw = pass.take_sets_changed();
            if pass.residency.managed[&10].step == 0 {
                break;
            }
        }
        let stats = pass.residency_stats();
        assert!(
            stats.io_bytes_total > 0,
            "the bands came over the disk: {stats:?}"
        );
        assert_eq!(
            pass.residency.managed[&10].step, 0,
            "the cached shadow map's cut loads to the finest band ({culls} culls): {stats:?}"
        );
        assert!(culls > 1, "each casting flip redrew the map: {culls} culls");
    }

    /// A band flip of a mesh whose instances cast no shadows redraws no cached shadow maps
    /// (`take_sets_changed` stays false); a caster's flip does. A refinement changes one mesh's
    /// records — the whole atlas and CSM must not invalidate for a receiver.
    #[test]
    fn a_non_caster_flip_leaves_the_cached_shadow_maps_alone() {
        let scene = scene(1);
        let (_, stream) = stream_rock(&scene.mesh);
        let config = ClusterResidency {
            enabled: true,
            budget_bytes: u64::MAX / 4,
            upload_bytes_per_frame: 64 * 1024,
            ram_budget_bytes: u64::MAX / 4,
            io_bytes_per_frame: 1024 * 1024,
        };
        // `frame` draws the rocks with `ShadowCast::Off`: a near eye wants every band.
        let mut pass = stream_pass(config, &scene, &stream);
        let mut saw_flip = false;
        for f in 0..300 {
            if f == 0 {
                // Admission (and its first upload) may redraw the caches once; drain it.
                frame(&mut pass, &scene, [0.0, 0.0, 2.5]);
                let _ = pass.take_sets_changed();
                continue;
            }
            let before = pass.residency.managed[&10].step;
            frame(&mut pass, &scene, [0.0, 0.0, 2.5]);
            if pass.residency.managed[&10].step != before {
                saw_flip = true;
                assert!(
                    !pass.take_sets_changed(),
                    "a non-casting band flip must not redraw the cached shadow maps"
                );
            } else {
                assert!(
                    !pass.take_sets_changed(),
                    "frame {f}: nothing shadow-relevant changed"
                );
            }
        }
        let stats = pass.residency_stats();
        assert!(stats.refinements > 0, "the near eye refined bands: {stats:?}");
        assert!(saw_flip, "the run switched a band on in-frame");

        // The same run with a caster: a flip invalidates the caches.
        let mut casting = stream_pass(config, &scene, &stream);
        let mut caster_flip = false;
        for f in 0..300 {
            let before = (f > 0).then(|| casting.residency.managed[&10].step);
            casting.begin_frame();
            assert!(casting.ensure_mesh(10, &scene.vertices, &scene.indices));
            let model = translate(0.0, 0.0, 0.0);
            casting.push_instance(10, 1, model, model, crate::ShadowCast::All, false);
            casting.note_residency_view(view([0.0, 0.0, 2.5]));
            casting.residency_frame();
            let Some(before) = before else {
                // Admission frame: its own redraw, drained.
                let _ = casting.take_sets_changed();
                continue;
            };
            if casting.residency.managed[&10].step != before {
                caster_flip = true;
                assert!(
                    casting.take_sets_changed(),
                    "a casting band flip redraws the cached shadow maps"
                );
                break;
            }
            assert!(!casting.take_sets_changed(), "frame {f}: no flip ran");
        }
        assert!(caster_flip, "the caster run switched a band on in-frame");
    }

    /// The cut's records after eviction and reload through the disk match an all-resident run
    /// settled at the same view (offsets differ; the cut cannot).
    #[test]
    fn streamed_settled_cut_matches_the_all_resident_run() {
        let scene = scene(6);
        // All-resident run (memory mesh, room to spare).
        let settle = [0.0, 0.0, 2.5];
        let whole = ClusterResidency {
            enabled: true,
            budget_bytes: u64::MAX / 4,
            upload_bytes_per_frame: u64::MAX / 4,
            ram_budget_bytes: u64::MAX / 4,
            io_bytes_per_frame: u64::MAX / 4,
        };
        let mut all = pass_with(whole, &scene);
        for _ in 0..400 {
            frame(&mut all, &scene, settle);
        }
        // Streamed run: a third of the DAG, evictions and reloads along the path, then settle.
        let (_, stream) = stream_rock(&scene.mesh);
        let full = full_bytes(&scene);
        let config = ClusterResidency {
            enabled: true,
            budget_bytes: full / 3,
            upload_bytes_per_frame: 96 * 1024,
            ram_budget_bytes: u64::MAX / 4,
            io_bytes_per_frame: 1024 * 1024,
        };
        let mut streamed = stream_pass(config, &scene, &stream);
        for eye in path() {
            frame(&mut streamed, &scene, eye);
        }
        // Settle with room: both runs reach the same demand-driven cut.
        streamed.settings.residency = Some(ClusterResidency {
            budget_bytes: u64::MAX / 4,
            ..config
        });
        for _ in 0..400 {
            frame(&mut streamed, &scene, settle);
        }
        let s = streamed.residency_stats();
        assert!(s.evictions > 0, "the budget forced evictions: {s:?}");
        assert!(s.io_bytes_total > 0 && s.refinements > 0, "{s:?}");
        // The settled cut is the same at every mesh.
        let cut = |pass: &VisBufferPass| -> Vec<Vec<(f32, f32, u32, u32)>> {
            pass.residency
                .managed
                .values()
                .map(|m| {
                    m.records
                        .iter()
                        .map(|r| (r.lod_error, r.parent_error, r.vertex_count, r.triangle_count))
                        .collect()
                })
                .collect()
        };
        assert_eq!(cut(&all), cut(&streamed), "the settled cut matches");
        for (a, b) in all
            .residency
            .managed
            .values()
            .zip(streamed.residency.managed.values())
        {
            assert_eq!(a.step, b.step);
            assert_eq!(a.demand, b.demand);
        }
    }

    /// Two streamed runs of the same path give the same counters frame for frame.
    #[test]
    fn streaming_is_deterministic() {
        let scene = scene(8);
        let (_, stream) = stream_rock(&scene.mesh);
        let full = full_bytes(&scene);
        let config = ClusterResidency {
            enabled: true,
            budget_bytes: full / 3,
            upload_bytes_per_frame: 96 * 1024,
            ram_budget_bytes: 8 * 1024 * 1024,
            io_bytes_per_frame: 256 * 1024,
        };
        let run = || {
            let mut pass = stream_pass(config, &scene, &stream);
            let stats: Vec<VisBufferResidencyStats> = path()
                .into_iter()
                .map(|eye| frame(&mut pass, &scene, eye))
                .collect();
            (stats, pass.residency.eviction_log.clone())
        };
        let (a, b) = (run(), run());
        assert_eq!(a.0, b.0);
        assert_eq!(a.1, b.1);
    }

    /// A corrupt base band is refused once. The next frame must not read the pak again.
    #[test]
    fn audfix_corrupt_base_band_is_refused_once() {
        let scene = scene(1);
        let disk = Arc::new(MemoryDisk::new(&scene.mesh).corrupt());
        let head = MeshletMesh::parse_head(&scene.mesh.to_bytes()).expect("head");
        let stream = Arc::new(MeshletMesh::from_head(head, disk.clone()));
        let config = ClusterResidency {
            enabled: true,
            budget_bytes: u64::MAX / 4,
            upload_bytes_per_frame: u64::MAX / 4,
            ram_budget_bytes: u64::MAX / 4,
            io_bytes_per_frame: u64::MAX / 4,
        };
        let mut pass = VisBufferPass::default();
        pass.settings.residency = Some(config);
        pass.insert_cooked(10, stream);
        for _ in 0..2 {
            frame(&mut pass, &scene, [0.0, 0.0, 2.5]);
        }
        assert!(
            pass.residency_refused(10),
            "a corrupt base stays eligible (managed={}, reads={}, err={:?})",
            pass.residency.managed.contains_key(&10),
            disk.reads(),
            pass.residency.stats.last_read_error_code
        );
        assert_eq!(disk.reads(), 1, "the pak was read {} times", disk.reads());
    }

    /// One cluster per simplification level, so a `bands`-long ladder is a managed plan
    /// (two or more bands). The coarsest cluster is the root.
    fn ladder(bands: u32) -> (Vec<LabVertex>, Vec<u32>, MeshletMesh) {
        let mut mesh = MeshletMesh {
            source_vertex_count: 3,
            source_index_count: 3,
            bounds: [0.0, 0.0, 0.0, 1.0],
            ..MeshletMesh::default()
        };
        for level in 0..bands {
            let parent_error = if level + 1 == bands {
                crate::visbuffer::meshlet::ROOT_ERROR
            } else {
                level as f32 + 0.5
            };
            mesh.meshlets.push(Meshlet {
                bounds: [0.0, 0.0, 0.0, 1.0],
                cone: [0.0, 1.0, 0.0, 1.0],
                lod: [0.0, 0.0, 0.0, 1.0],
                parent: [0.0, 0.0, 0.0, 1.0],
                lod_error: level as f32,
                parent_error,
                vertex_offset: mesh.vertices.len() as u32,
                triangle_offset: mesh.triangles.len() as u32,
                vertex_count: 3,
                triangle_count: 1,
                level,
                pad: 0,
            });
            mesh.vertices.extend([0, 1, 2]);
            mesh.triangles.push(0x0002_0100);
        }
        let vertices = (0..3)
            .map(|i| LabVertex::from_pos_normal_uv([i as f32, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0]))
            .collect();
        (vertices, vec![0, 1, 2], mesh)
    }

    fn stream_of(mesh: &MeshletMesh, disk: Arc<MemoryDisk>) -> Arc<MeshletMesh> {
        let head = MeshletMesh::parse_head(&mesh.to_bytes()).expect("head");
        Arc::new(MeshletMesh::from_head(head, disk))
    }

    /// A band delivered after its mesh was replaced by a shorter plan must not index off
    /// the end of the new `band_ram` (the old disk stays polled by a mesh that still uses it).
    #[test]
    fn audfix_dropped_band_does_not_panic_on_a_shorter_mesh() {
        let (verts, indices, long_mesh) = ladder(4);
        let disk = Arc::new(MemoryDisk::new(&long_mesh));
        let stream = stream_of(&long_mesh, disk.clone());
        let (short_v, short_i, short_mesh) = ladder(2);
        let short_stream = stream_of(&short_mesh, Arc::new(MemoryDisk::new(&short_mesh)));
        let config = ClusterResidency {
            enabled: true,
            budget_bytes: u64::MAX / 4,
            upload_bytes_per_frame: u64::MAX / 4,
            ram_budget_bytes: u64::MAX / 4,
            io_bytes_per_frame: u64::MAX / 4,
        };
        let mut pass = VisBufferPass::default();
        pass.settings.residency = Some(config);
        pass.insert_cooked(10, Arc::clone(&stream));
        pass.insert_cooked(11, stream);
        pass.begin_frame();
        assert!(pass.ensure_mesh(10, &verts, &indices));
        assert!(pass.ensure_mesh(11, &verts, &indices));
        let model = translate(0.0, 0.0, 0.0);
        pass.push_instance(10, 1, model, model, ShadowCast::Off, false);
        pass.push_instance(11, 1, model, model, ShadowCast::Off, false);
        pass.note_residency_view(view([0.0, 0.0, 2.5]));
        pass.residency_frame();
        let long_bands = pass
            .residency
            .managed
            .get(&10)
            .map(|m| m.plan.band_count())
            .unwrap_or(0);
        assert_eq!(long_bands, 4, "the long ladder was not a four-band plan");
        let in_flight = pass
            .residency
            .req_map
            .values()
            .filter(|(key, _)| *key == 10)
            .map(|(_, band)| *band)
            .max();
        assert!(
            in_flight.is_some_and(|band| band >= 2),
            "in-flight band {in_flight:?} still fits a two-band plan"
        );
        pass.release_meshes(&[10]);
        pass.insert_cooked(10, short_stream);
        pass.begin_frame();
        assert!(pass.ensure_mesh(10, &short_v, &short_i));
        assert!(pass.ensure_mesh(11, &verts, &indices));
        pass.note_residency_view(view([0.0, 0.0, 2.5]));
        pass.residency_frame();
        let short_bands = pass
            .residency
            .managed
            .get(&10)
            .map(|m| m.plan.band_count())
            .unwrap_or(0);
        assert_eq!(short_bands, 2, "the replacement was not a two-band plan");
        assert!(
            !pass.residency.req_map.values().any(|(key, _)| *key == 10),
            "a dropped mesh's in-flight tag still names key 10"
        );
    }

    /// A band read queued for an admission must not stick `band_failed` on the mesh that
    /// replaced it. The error belongs to the dropped admission; the new one has not failed.
    #[test]
    fn audfix_stale_band_error_does_not_fail_the_readmitted_mesh() {
        let scene = scene(1);
        let head = MeshletMesh::parse_head(&scene.mesh.to_bytes()).expect("head");
        let disk = Arc::new(MemoryDisk::new(&scene.mesh).failing_bands("band_corrupt"));
        let stream = Arc::new(MeshletMesh::from_head(head, disk.clone()));
        let config = ClusterResidency {
            enabled: true,
            budget_bytes: u64::MAX / 4,
            upload_bytes_per_frame: 64 * 1024,
            ram_budget_bytes: u64::MAX / 4,
            io_bytes_per_frame: u64::MAX / 4,
        };
        let mut pass = stream_pass(config, &scene, &stream);
        let mut queued = false;
        for _ in 0..40 {
            frame(&mut pass, &scene, [0.0, 0.0, 2.5]);
            if !disk.queue.lock().expect("queue").is_empty() {
                queued = true;
                break;
            }
        }
        assert!(queued, "the close view must request a finer band");
        assert_eq!(pass.residency_stats().read_errors, 0, "the error is still queued");
        assert!(pass.remove_meshlets(10), "the mesh was resident");
        pass.insert_cooked(10, Arc::clone(&stream));
        let before = pass.residency_stats().read_errors;
        frame(&mut pass, &scene, [0.0, 0.0, 2.5]);
        let m = &pass.residency.managed[&10];
        assert!(
            m.band_failed.iter().all(|failed| !failed),
            "a delivery from the dropped admission must not stick band_failed: {:?}",
            m.band_failed
        );
        assert_eq!(pass.residency_stats().read_errors, before);
    }

    /// A delivery whose band index belongs to a shorter mesh that reused the key must not panic,
    /// and must not mark the new mesh failed.
    #[test]
    fn audfix_stale_band_index_does_not_panic() {
        let scene = scene(1);
        let (disk, stream) = stream_rock(&scene.mesh);
        let config = ClusterResidency {
            enabled: true,
            budget_bytes: u64::MAX / 4,
            upload_bytes_per_frame: 64 * 1024,
            ram_budget_bytes: u64::MAX / 4,
            io_bytes_per_frame: u64::MAX / 4,
        };
        let mut pass = stream_pass(config, &scene, &stream);
        frame(&mut pass, &scene, [0.0, 0.0, 2.5]);
        assert!(pass.residency.managed.contains_key(&10));
        disk.queue.lock().expect("queue").clear();
        pass.residency
            .req_map
            .insert(7, (10, pass.residency.managed[&10].band_ram.len() + 3));
        disk.queue
            .lock()
            .expect("queue")
            .push_back(crate::visbuffer::BandDelivery {
                tag: 7,
                result: Err(crate::visbuffer::BandDiskError {
                    code: "band_corrupt",
                    message: "stale band".to_string(),
                }),
            });
        pass.residency_deliveries();
        let m = &pass.residency.managed[&10];
        assert!(
            m.band_failed.iter().all(|failed| !failed),
            "an out-of-range delivery is not this mesh's read: {:?}",
            m.band_failed
        );
        assert_eq!(pass.residency_stats().read_errors, 0);
    }

    /// Turning residency off re-reads the pak. A failed read keeps the bands already resident
    /// instead of switching finer bands on with empty index lists.
    #[test]
    fn audfix_failed_section_read_while_draining_keeps_the_resident_bands() {
        let scene = scene(1);
        let (disk, stream) = stream_rock(&scene.mesh);
        let config = ClusterResidency {
            enabled: true,
            budget_bytes: u64::MAX / 4,
            upload_bytes_per_frame: 64 * 1024,
            ram_budget_bytes: u64::MAX / 4,
            io_bytes_per_frame: u64::MAX / 4,
        };
        let mut pass = stream_pass(config, &scene, &stream);
        frame(&mut pass, &scene, [0.0, 0.0, 2.5]);
        let step = pass.residency.managed[&10].step;
        assert!(step > 0, "the base is coarser than the finest band");
        let resident = pass.residency.managed[&10]
            .records
            .iter()
            .filter(|record| record.lod_error != ABSENT_ERROR)
            .count();
        disk.fail_sections_from_now("band_not_found");
        pass.settings.residency = Some(ClusterResidency {
            enabled: false,
            ..config
        });
        frame(&mut pass, &scene, [0.0, 0.0, 2.5]);
        let m = &pass.residency.managed[&10];
        assert_eq!(
            m.step, step,
            "a failed section read must not switch finer bands on"
        );
        assert!(m.pending.is_none(), "no empty block left in flight");
        let still = m
            .records
            .iter()
            .filter(|record| record.lod_error != ABSENT_ERROR)
            .count();
        assert_eq!(still, resident, "the resident cut is unchanged");
    }

    fn roomy() -> ClusterResidency {
        ClusterResidency {
            enabled: true,
            budget_bytes: u64::MAX / 4,
            upload_bytes_per_frame: u64::MAX / 4,
            ram_budget_bytes: u64::MAX / 4,
            io_bytes_per_frame: u64::MAX / 4,
        }
    }

    /// Residency turned off drains each streamed mesh once. Managed meshes stay managed while
    /// it is off, and the drain used to read the whole pak (blocking, on the frame thread) on
    /// every frame, even with every band resident.
    #[test]
    fn corr_visbuffer_residency_off_reads_the_pak_once() {
        let scene = scene(1);
        let (disk, stream) = stream_rock(&scene.mesh);
        let config = roomy();
        let mut pass = stream_pass(config, &scene, &stream);
        frame(&mut pass, &scene, [0.0, 0.0, 2.5]);
        assert!(
            pass.residency.managed[&10].step > 0,
            "the base is coarser than the finest band"
        );
        assert_eq!(disk.reads(), 1, "the admission read");
        pass.settings.residency = Some(ClusterResidency::OFF);
        for _ in 0..6 {
            frame(&mut pass, &scene, [0.0, 0.0, 2.5]);
        }
        assert_eq!(pass.residency.managed[&10].step, 0, "drained");
        assert_eq!(disk.reads(), 2, "one drain read, not one per frame");
    }

    /// With the arena full, a streamed mesh draining (residency off) does not read its pak every
    /// frame (corr-render-visbuffer M): the next band cannot be allocated, so the read is skipped
    /// until room appears. Before, every frame read the whole pak (blocking) and then found no
    /// room.
    #[test]
    fn corr_visbuffer_a_full_arena_does_not_reread_the_pak_while_draining() {
        let scene = scene(1);
        let far = [0.0, 0.0, 1.0e6];
        // The geometry the far view's base takes, measured on an unbounded arena.
        let base_words = {
            let (_, stream) = stream_rock(&scene.mesh);
            let mut pass = stream_pass(roomy(), &scene, &stream);
            frame(&mut pass, &scene, far);
            let m = &pass.residency.managed[&10];
            assert!(m.step > 0, "far: a coarse base");
            pass.arena.stats().geometry_words as u32
        };
        let (disk, stream) = stream_rock(&scene.mesh);
        let mut pass = VisBufferPass {
            arena: crate::visbuffer::arena::ClusterArena::with_limits(
                base_words,
                base_words,
                1 << 12,
                1 << 23,
            ),
            ..VisBufferPass::default()
        };
        pass.settings.residency = Some(roomy());
        pass.insert_cooked(10, stream);
        frame(&mut pass, &scene, far);
        let step = pass.residency.managed[&10].step;
        assert!(
            step > 0,
            "the base is admitted and is coarser than the finest band"
        );
        let reads = disk.reads();
        pass.settings.residency = Some(ClusterResidency::OFF);
        for _ in 0..6 {
            frame(&mut pass, &scene, [0.0, 0.0, 2.5]);
        }
        let m = &pass.residency.managed[&10];
        assert_eq!(m.step, step, "no room: the cut stays");
        assert!(m.pending.is_none());
        assert_eq!(
            disk.reads(),
            reads,
            "the pak was read {} times with no room for a band",
            disk.reads() - reads
        );
    }

    /// A pak that stops reading while residency is off is not read again every frame (and does
    /// not count an error per frame).
    #[test]
    fn corr_visbuffer_failed_drain_read_is_sticky() {
        let scene = scene(1);
        let (disk, stream) = stream_rock(&scene.mesh);
        let mut pass = stream_pass(roomy(), &scene, &stream);
        frame(&mut pass, &scene, [0.0, 0.0, 2.5]);
        let step = pass.residency.managed[&10].step;
        assert!(step > 0);
        disk.fail_sections_from_now("band_not_found");
        pass.settings.residency = Some(ClusterResidency::OFF);
        for _ in 0..6 {
            frame(&mut pass, &scene, [0.0, 0.0, 2.5]);
        }
        assert_eq!(pass.residency.managed[&10].step, step, "the resident cut stays");
        assert_eq!(disk.reads(), 2, "the pak was read {} times", disk.reads());
        assert_eq!(pass.residency_stats().read_errors, 1);
    }

    /// The drain checks a streamed band as a delivery is checked: a corrupt finer band stays off
    /// (the mesh draws its coarse level) instead of uploading triangles that index past their
    /// cluster.
    #[test]
    fn corr_visbuffer_drain_validates_streamed_bands() {
        let scene = scene(1);
        let disk = Arc::new(MemoryDisk::new(&scene.mesh).corrupt_after_first());
        let stream = stream_of(&scene.mesh, disk.clone());
        let mut pass = stream_pass(roomy(), &scene, &stream);
        frame(&mut pass, &scene, [0.0, 0.0, 2.5]);
        let step = pass.residency.managed[&10].step;
        assert!(step > 0);
        assert!(!pass.residency_refused(10), "the base read was sound");
        pass.settings.residency = Some(ClusterResidency::OFF);
        frame(&mut pass, &scene, [0.0, 0.0, 2.5]);
        frame(&mut pass, &scene, [0.0, 0.0, 2.5]);
        let m = &pass.residency.managed[&10];
        assert_eq!(m.step, step, "a corrupt band must not switch on");
        assert!(m.pending.is_none());
        assert!(m.band_failed[step - 1], "the corrupt band is marked failed");
        let stats = pass.residency_stats();
        assert_eq!(stats.read_errors, 1, "{stats:?}");
        assert_eq!(stats.last_read_error_code, Some("band_corrupt"));
        assert_eq!(disk.reads(), 2, "a failed band is not read again");
    }

    /// A corrupt pak's refusal survives another mesh's removal (which clears the budget
    /// refusals, since room was made). Before, the next frame read the bad pak again.
    #[test]
    fn corr_visbuffer_pak_refusal_survives_room_being_made() {
        let scene = scene(2);
        let disk = Arc::new(MemoryDisk::new(&scene.mesh).corrupt());
        let stream = stream_of(&scene.mesh, disk.clone());
        let mut pass = VisBufferPass::default();
        pass.settings.residency = Some(roomy());
        pass.insert_cooked(10, stream);
        pass.insert_cooked(11, Arc::clone(&scene.mesh));
        frame(&mut pass, &scene, [0.0, 0.0, 2.5]);
        frame(&mut pass, &scene, [0.0, 0.0, 2.5]);
        assert!(pass.residency_refused(10));
        assert!(pass.residency_manages(11), "the sound mesh is managed");
        assert_eq!(disk.reads(), 1);
        assert!(pass.remove_meshlets(11));
        for _ in 0..3 {
            frame(&mut pass, &scene, [0.0, 0.0, 2.5]);
        }
        assert!(pass.residency_refused(10), "still refused");
        assert_eq!(pass.residency_stats().refused_meshes, 1);
        assert_eq!(disk.reads(), 1, "the bad pak was read {} times", disk.reads());
        // Inserting it again forgets the refusal (a new pak may read).
        pass.insert_cooked(10, Arc::clone(&scene.mesh));
        assert!(!pass.residency_refused(10));
    }

    /// A band larger than the whole per-frame read bound still loads (as the frame's first read),
    /// and the estimate counts what the read moves, not the owned vertices already in RAM.
    /// Before, such a band was skipped every frame and the mesh never refined.
    #[test]
    fn corr_visbuffer_a_band_over_the_read_bound_still_loads() {
        let scene = scene(1);
        let (_, stream) = stream_rock(&scene.mesh);
        let config = ClusterResidency {
            io_bytes_per_frame: 1,
            ..roomy()
        };
        let mut pass = stream_pass(config, &scene, &stream);
        for _ in 0..200 {
            frame(&mut pass, &scene, [0.0, 0.0, 2.5]);
        }
        let m = &pass.residency.managed[&10];
        assert_eq!(m.demand, 0, "the close view needs the finest band");
        assert_eq!(m.step, 0, "settled at the finest band");
        let finest = &m.plan.bands[0];
        assert!(!finest.vertices.is_empty());
        assert!(
            m.plan.read_bytes(0) < finest.bytes(),
            "the read estimate leaves out the owned vertices"
        );
    }

    /// Once the frame's upload allowance is spent, a later stream-shaped mesh still issues its
    /// pak read. Before, the first RAM mesh that met a spent allowance ended the loop, and no
    /// lower-priority mesh got a read that frame.
    #[test]
    fn corr_visbuffer_spent_uploads_do_not_stop_reads() {
        let scene = scene(3);
        let (_, stream) = stream_rock(&scene.mesh);
        let config = ClusterResidency {
            upload_bytes_per_frame: 4,
            ..roomy()
        };
        let mut pass = VisBufferPass::default();
        pass.settings.residency = Some(config);
        // The eye above the middle rock: it asks first, then rock 10 and rock 12 tie (key order).
        // Rocks 10 and 11 are RAM meshes; rock 12, asked last, is stream-shaped.
        pass.insert_cooked(10, Arc::clone(&scene.mesh));
        pass.insert_cooked(11, Arc::clone(&scene.mesh));
        pass.insert_cooked(12, stream);
        for _ in 0..3 {
            frame(&mut pass, &scene, [0.0, 2.0, -3.0]);
        }
        let far = &pass.residency.managed[&12];
        assert!(far.demand < far.step, "the far rock wants a finer band");
        assert!(
            far.band_ram.iter().any(|b| !matches!(b, BandRam::Disk)),
            "the far rock's band read was issued"
        );
    }

    /// A retried refusal whose evictions do not fit the frame's upload allowance is refused again
    /// without reading its pak (corr-render-visbuffer N). Before, the retry lifted the refusal
    /// every frame, and every frame the admission read the whole pak (blocking) and rebuilt the
    /// plan before finding that no band could be evicted.
    #[test]
    fn corr_visbuffer_a_refused_retry_under_upload_pressure_reads_no_pak() {
        let one = scene(1);
        let full = full_bytes(&one);
        let base = {
            let mut pass = pass_with(roomy(), &one);
            frame(&mut pass, &one, [0.0, 0.0, 1.0e6]);
            pass.residency.managed[&10].bytes
        };
        let two = Scene {
            vertices: one.vertices.clone(),
            indices: one.indices.clone(),
            mesh: Arc::clone(&one.mesh),
            rocks: vec![(10, [0.0, 0.0, 0.0]), (11, [0.0, 0.0, -60.0])],
        };
        let config = ClusterResidency {
            budget_bytes: full + base / 2,
            // Bounded, so spending it every frame does not overflow the upload totals.
            upload_bytes_per_frame: 1 << 30,
            ..roomy()
        };
        let (disk, stream) = stream_rock(&two.mesh);
        let mut pass = VisBufferPass::default();
        pass.settings.residency = Some(config);
        pass.insert_cooked(10, Arc::clone(&two.mesh));
        for _ in 0..60 {
            frame(&mut pass, &two, [0.0, 0.0, 2.5]);
        }
        assert_eq!(pass.residency.managed[&10].step, 0, "rock 10 holds every band");
        pass.insert_cooked(11, stream);
        frame(&mut pass, &two, [0.0, 0.0, 2.5]);
        assert!(
            pass.residency_refused(11),
            "no room while rock 10 needs its bands"
        );
        let reads = disk.reads();
        // The camera moves to rock 11, so rock 10's fine bands could go, but each frame spends
        // the upload allowance before the meshes are listed: no eviction's records fit.
        for _ in 0..8 {
            pass.begin_frame();
            let left = pass.residency.upload_left;
            pass.residency.charge(left.saturating_sub(1));
            for &(key, at) in &two.rocks {
                if pass.ensure_mesh(key, &two.vertices, &two.indices) {
                    let model = translate(at[0], at[1], at[2]);
                    pass.push_instance(key, 1, model, model, ShadowCast::Off, false);
                }
            }
            pass.note_residency_view(view([0.0, 0.0, -57.5]));
            pass.residency_frame();
        }
        // Each frame ends by lifting the refusal for a retry (`residency_retry_refused`), so the
        // mesh is not listed as refused here: it is simply not admitted.
        assert!(!pass.residency_manages(11), "the evictions never fit");
        assert!(
            disk.reads() <= reads + 1,
            "the pak was read {} times while no room could be made",
            disk.reads() - reads
        );
        // Room made for real (rock 10 leaves): rock 11 is admitted.
        assert!(pass.remove_meshlets(10));
        frame(&mut pass, &two, [0.0, 0.0, -57.5]);
        assert!(pass.residency_manages(11), "{:?}", pass.residency_stats());
    }

    /// A mesh refused for budget while another's fine bands were needed is admitted once those
    /// bands stop being needed. Before, the budget was not exceeded, so nothing was evicted and the
    /// refusal (only lifted by an eviction) stayed: the mesh drew forward for good.
    #[test]
    fn corr_visbuffer_a_budget_refusal_retries_once_room_is_reclaimable() {
        let one = scene(1);
        let full = full_bytes(&one);
        let base = {
            let mut pass = pass_with(roomy(), &one);
            frame(&mut pass, &one, [0.0, 0.0, 1.0e6]);
            let m = &pass.residency.managed[&10];
            assert_eq!(m.step, m.plan.base(), "far: the base only");
            m.bytes
        };
        assert!(full > base);
        let two = Scene {
            vertices: one.vertices.clone(),
            indices: one.indices.clone(),
            mesh: Arc::clone(&one.mesh),
            rocks: vec![(10, [0.0, 0.0, 0.0]), (11, [0.0, 0.0, -60.0])],
        };
        let config = ClusterResidency {
            budget_bytes: full + base / 2,
            ..roomy()
        };
        let mut pass = VisBufferPass::default();
        pass.settings.residency = Some(config);
        pass.insert_cooked(10, Arc::clone(&two.mesh));
        for _ in 0..60 {
            frame(&mut pass, &two, [0.0, 0.0, 2.5]);
        }
        assert_eq!(pass.residency.managed[&10].step, 0, "rock 10 holds every band");
        pass.insert_cooked(11, Arc::clone(&two.mesh));
        frame(&mut pass, &two, [0.0, 0.0, 2.5]);
        assert!(
            pass.residency_refused(11),
            "no room while rock 10 needs its bands"
        );
        // The camera moves to rock 11: rock 10's fine bands are no longer needed.
        for _ in 0..10 {
            frame(&mut pass, &two, [0.0, 0.0, -57.5]);
        }
        assert!(
            pass.residency_manages(11),
            "rock 11 still draws forward: {:?}",
            pass.residency_stats()
        );
        assert!(pass.residency_stats().resident_bytes <= config.budget_bytes);
    }
}
