// SPDX-License-Identifier: MPL-2.0
//! ADR-3079: the sun page cache and the atlas tile cache cost what changed, not the caster
//! count, and decide exactly what the full loop decided.
//!
//! `Reference` and `ReferenceAtlas` are the full loops as they were before ADR-3079, kept here
//! verbatim (plus a visit counter) so the differential tests compare against them and the
//! release bench times old and new in one binary:
//!
//! ```text
//! cargo test --release -p regia_render --lib sun_page_cache_cost_bench -- --ignored --nocapture
//! ```

use super::*;

use std::collections::HashMap;

/// The pre-ADR-3079 sun page cache: every caster compared in every cascade, every update.
struct Reference {
    pages_per_axis: u32,
    cascades: Vec<RefCascade>,
    /// Footprints read by the last update (one per caster and cascade compared or stored).
    visits: u32,
}

struct RefCascade {
    light_vp: [[f32; 4]; 4],
    has_vp: bool,
    casters: HashMap<u64, (Aabb2, CasterPose)>,
}

impl Reference {
    fn new(cascades: u32, pages_per_axis: u32) -> Self {
        Self {
            pages_per_axis: pages_per_axis.max(1),
            cascades: (0..cascades.max(1)).map(|_| RefCascade::new()).collect(),
            visits: 0,
        }
    }

    fn update(
        &mut self,
        casters: &[ShadowCaster],
        light_vps: &[[[f32; 4]; 4]],
        dynamic_mask: u8,
        footprint: impl Fn(usize, usize) -> ([f32; 2], [f32; 2]),
    ) -> PageUpdate {
        if self.cascades.len() != light_vps.len() {
            self.cascades = (0..light_vps.len()).map(|_| RefCascade::new()).collect();
        }
        let pages = self.pages_per_axis;
        let mut dirty = Vec::new();
        let mut full = Vec::new();
        let mut events = vec![[0u32; 3]; light_vps.len()];
        let mut dynamic_pages = 0;
        self.visits = 0;
        for (index, (cascade, vp)) in self.cascades.iter_mut().zip(light_vps).enumerate() {
            let (f, e, d, v) =
                cascade.update((index, vp), casters, dynamic_mask, &footprint, pages, &mut dirty);
            full.push(f);
            events[index] = e;
            dynamic_pages += d;
            self.visits += v;
        }
        dirty.sort();
        dirty.dedup();
        PageUpdate {
            dirty,
            cascade_full_redraw: full,
            caster_events: events,
            dynamic_pages,
            casters_visited: self.visits,
            full_passes: 0,
        }
    }
}

impl RefCascade {
    fn new() -> Self {
        Self {
            light_vp: [[0.0; 4]; 4],
            has_vp: false,
            casters: HashMap::new(),
        }
    }

    fn update(
        &mut self,
        (cascade_index, vp): (usize, &[[f32; 4]; 4]),
        casters: &[ShadowCaster],
        dynamic_mask: u8,
        footprint: &impl Fn(usize, usize) -> ([f32; 2], [f32; 2]),
        pages_per_axis: u32,
        dirty: &mut Vec<PageId>,
    ) -> (bool, [u32; 3], u32, u32) {
        let refresh_dynamic = dynamic_mask & (1u8 << cascade_index.min(7)) != 0;
        let mut events = [0u32; 3];
        let mut visits = 0;
        let matrix_changed = !self.has_vp || !matrices_match(self.light_vp, *vp);
        if matrix_changed {
            self.has_vp = true;
            self.light_vp = *vp;
            self.casters.clear();
            for (index, caster) in casters
                .iter()
                .enumerate()
                .filter(|(_, c)| c.cascades & (1u8 << cascade_index.min(7)) != 0)
            {
                let (min, max) = footprint(cascade_index, index);
                visits += 1;
                self.casters.insert(caster.id, (Aabb2 { min, max }, caster.pose));
            }
            for y in 0..pages_per_axis {
                for x in 0..pages_per_axis {
                    dirty.push(PageId {
                        cascade: cascade_index as u32,
                        x,
                        y,
                    });
                }
            }
            return (true, events, 0, visits);
        }
        let mut seen = HashMap::new();
        let mut cascade_dirty = Vec::new();
        let mut dynamic_dirty = Vec::new();
        let tolerance_m = PAGE_MOVE_TOLERANCE_UV / uv_per_metre(vp);
        for (index, caster) in casters
            .iter()
            .enumerate()
            .filter(|(_, c)| c.cascades & (1u8 << cascade_index.min(7)) != 0)
        {
            let (min, max) = footprint(cascade_index, index);
            visits += 1;
            let uv = Aabb2 { min, max };
            if caster.dynamic && refresh_dynamic {
                let covered = self.casters.get(&caster.id).map_or(uv, |&(old, _)| old.union(uv));
                dynamic_dirty.extend(pages_covering(covered, pages_per_axis));
                seen.insert(caster.id, (uv, caster.pose));
                continue;
            }
            if let Some(&(old, old_pose)) = self.casters.get(&caster.id) {
                if old.nearly_same(uv) && old_pose.displacement_bound(&caster.pose) <= tolerance_m {
                    seen.insert(caster.id, (old, old_pose));
                } else {
                    events[0] += 1;
                    cascade_dirty.extend(pages_covering(old.union(uv), pages_per_axis));
                    seen.insert(caster.id, (uv, caster.pose));
                }
            } else {
                events[1] += 1;
                cascade_dirty.extend(pages_covering(uv, pages_per_axis));
                seen.insert(caster.id, (uv, caster.pose));
            }
        }
        for (id, (old, _)) in &self.casters {
            if !seen.contains_key(id) {
                events[2] += 1;
                cascade_dirty.extend(pages_covering(*old, pages_per_axis));
            }
        }
        self.casters = seen;
        dynamic_dirty.sort_unstable();
        dynamic_dirty.dedup();
        let dynamic_pages = dynamic_dirty.len() as u32;
        cascade_dirty.append(&mut dynamic_dirty);
        cascade_dirty.sort_unstable();
        cascade_dirty.dedup();
        let page_count = pages_per_axis * pages_per_axis;
        let all = cascade_dirty.len() as u32 >= page_count && page_count > 0;
        for (x, y) in cascade_dirty {
            dirty.push(PageId {
                cascade: cascade_index as u32,
                x,
                y,
            });
        }
        (all, events, dynamic_pages, visits)
    }
}

/// The pre-ADR-3079 atlas tile cache.
#[derive(Default)]
struct ReferenceAtlas {
    lights: Vec<(u64, [f32; 3], f32)>,
    casters: HashMap<u64, (Aabb3, CasterPose)>,
    visits: u32,
}

impl ReferenceAtlas {
    fn update_refreshing(
        &mut self,
        lights: &[AtlasLight],
        casters: &[ShadowCaster],
        refresh_dynamic: bool,
    ) -> (Vec<u32>, u32) {
        self.visits = 0;
        if lights.is_empty() {
            self.lights.clear();
            self.casters.clear();
            return (Vec::new(), 0);
        }
        let mut dirty = Vec::new();
        let light_moved = lights.len() != self.lights.len()
            || lights.iter().enumerate().any(|(i, light)| {
                let (id, center, radius) = self.lights[i];
                id != light.id
                    || (Vec3::from_array(center) - Vec3::from_array(light.center)).length_squared() > 1.0e-8
                    || (radius - light.radius).abs() > 1.0e-4
            });
        if light_moved || self.casters.is_empty() && !casters.is_empty() && self.lights.is_empty() {
            self.lights = lights
                .iter()
                .map(|light| (light.id, light.center, light.radius))
                .collect();
            self.casters = casters.iter().map(|c| (c.id, (c.aabb, c.pose))).collect();
            self.visits = casters.len() as u32;
            return ((0..lights.len() as u32).collect(), 0);
        }
        let mut moved = Vec::new();
        let mut seen = HashMap::new();
        let mut dynamic = Vec::new();
        for caster in casters {
            self.visits += 1;
            if refresh_dynamic && caster.dynamic {
                if let Some(&(old, _)) = self.casters.get(&caster.id) {
                    dynamic.push(old);
                }
                dynamic.push(caster.aabb);
                seen.insert(caster.id, (caster.aabb, caster.pose));
                continue;
            }
            match self.casters.get(&caster.id) {
                Some(&(old, old_pose))
                    if aabb_same(old, caster.aabb)
                        && old_pose.displacement_bound(&caster.pose) <= ATLAS_MOVE_TOLERANCE_M =>
                {
                    seen.insert(caster.id, (old, old_pose));
                }
                Some(&(old, _)) => {
                    moved.push(old);
                    moved.push(caster.aabb);
                    seen.insert(caster.id, (caster.aabb, caster.pose));
                }
                None => {
                    moved.push(caster.aabb);
                    seen.insert(caster.id, (caster.aabb, caster.pose));
                }
            }
        }
        for (id, (old, _)) in &self.casters {
            if !seen.contains_key(id) {
                moved.push(*old);
            }
        }
        self.casters = seen;
        self.lights.clear();
        self.lights
            .extend(lights.iter().map(|light| (light.id, light.center, light.radius)));
        let mut dynamic_only = 0;
        for (index, light) in lights.iter().enumerate() {
            let reaches = |boxes: &[Aabb3]| {
                boxes
                    .iter()
                    .any(|aabb| sphere_hits_aabb(light.center, light.radius, *aabb))
            };
            if reaches(&moved) {
                dirty.push(index as u32);
            } else if reaches(&dynamic) {
                dirty.push(index as u32);
                dynamic_only += 1;
            }
        }
        (dirty, dynamic_only)
    }
}

/// SplitMix64: a fixed-seed generator, so every run sees the same scenes.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    /// Uniform in `lo..hi`.
    fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * (self.next() >> 40) as f32 / (1u64 << 24) as f32
    }
}

/// A box caster at `center`, half size `half`, turned `yaw` about its centre.
fn caster(id: u64, center: [f32; 3], half: f32, yaw: f32, cascades: u8, dynamic: bool) -> ShadowCaster {
    let model = regia_core::glam::Mat4::from_translation(Vec3::from_array(center))
        * regia_core::glam::Mat4::from_rotation_y(yaw);
    ShadowCaster {
        is_static: false,
        id,
        aabb: Aabb3::from_center_half(center, [half; 3]),
        cascades,
        pose: CasterPose {
            model: model.to_cols_array_2d(),
            center: [0.0; 3],
            radius: half * 3.0f32.sqrt(),
        },
        dynamic,
    }
}

/// An orthographic sun cascade looking down Y that covers world X and Z in `[-r, r]` around
/// `(cx, cz)`.
fn cascade_vp(r: f32, cx: f32, cz: f32) -> [[f32; 4]; 4] {
    let s = 1.0 / r;
    [
        [s, 0.0, 0.0, 0.0],
        [0.0, 0.0, 0.01, 0.0],
        [0.0, s, 0.0, 0.0],
        [-cx * s, -cz * s, 0.5, 1.0],
    ]
}

/// A randomized scene that changes step by step: casters move a little or far, creep below the
/// tolerance, turn in place, appear, vanish, turn dynamic and back, change their cascade mask,
/// swap places in the list, repeat an id, and carry a non-finite pose; the cascades re-snap,
/// jitter below the matrix tolerance and change in number; the dynamic mask changes.
struct Scene {
    rng: Rng,
    casters: Vec<ShadowCaster>,
    next_id: u64,
    vps: Vec<[[f32; 4]; 4]>,
    dynamic_mask: u8,
    lights: Vec<AtlasLight>,
    /// Steps left in a run where the cascades of `orbit_cascades` re-snap every step (a moving
    /// camera), whatever else the step does.
    orbit_left: u32,
    orbit_cascades: u8,
}

const RADII: [f32; 4] = [6.0, 14.0, 30.0, 60.0];

impl Scene {
    fn new(seed: u64, count: usize) -> Self {
        let mut scene = Self {
            rng: Rng(seed),
            casters: Vec::new(),
            next_id: 1,
            vps: RADII.iter().map(|&r| cascade_vp(r, 0.0, 0.0)).collect(),
            dynamic_mask: 0b0011,
            lights: Vec::new(),
            orbit_left: 0,
            orbit_cascades: 0,
        };
        for _ in 0..count {
            let c = scene.fresh();
            scene.casters.push(c);
        }
        let lights = (0..3).map(|i| scene.light(i)).collect();
        scene.lights = lights;
        scene
    }

    fn mask(&mut self) -> u8 {
        match self.rng.below(4) {
            0 => u8::MAX,
            1 => 0b0001 | 1 << self.rng.below(4),
            _ => (self.rng.below(15) + 1) as u8,
        }
    }

    fn fresh(&mut self) -> ShadowCaster {
        let id = self.next_id;
        self.next_id += 1;
        let center = [
            self.rng.range(-45.0, 45.0),
            self.rng.range(-2.0, 2.0),
            self.rng.range(-45.0, 45.0),
        ];
        let half = self.rng.range(0.05, 3.0);
        let mask = self.mask();
        let dynamic = self.rng.chance(8);
        caster(id, center, half, 0.0, mask, dynamic)
    }

    fn light(&mut self, id: u64) -> AtlasLight {
        AtlasLight {
            id,
            center: [self.rng.range(-20.0, 20.0), 2.0, self.rng.range(-20.0, 20.0)],
            radius: self.rng.range(2.0, 12.0),
        }
    }

    /// One step of change. About a third of the steps hold everything.
    fn step(&mut self) {
        if self.orbit_left == 0 && self.rng.chance(4) {
            self.orbit_left = 2 + self.rng.below(12) as u32;
            self.orbit_cascades = (self.rng.below(15) + 1) as u8;
        }
        if self.orbit_left > 0 {
            self.orbit_left -= 1;
            for k in 0..self.vps.len() {
                if self.orbit_cascades & (1 << k) != 0 {
                    let cx = self.rng.range(-3.0, 3.0);
                    self.vps[k] = cascade_vp(RADII[k % RADII.len()], cx, 0.0);
                }
            }
        }
        if self.rng.chance(35) {
            return;
        }
        let n = self.casters.len();
        let edits = 1 + self.rng.below(6);
        for _ in 0..edits {
            if self.casters.is_empty() {
                break;
            }
            let i = self.rng.below(self.casters.len() as u64) as usize;
            let c = self.casters[i];
            let mid = [0, 1, 2].map(|a| 0.5 * (c.aabb.min[a] + c.aabb.max[a]));
            let half = 0.5 * (c.aabb.max[0] - c.aabb.min[0]);
            let half = if half.is_finite() { half } else { 0.5 };
            let rebuild = |mid: [f32; 3], yaw: f32, c: &ShadowCaster| {
                caster(c.id, mid, half, yaw, c.cascades, c.dynamic)
            };
            match self.rng.below(12) {
                0 | 1 => {
                    let d = self.rng.range(-0.6, 0.6);
                    self.casters[i] = rebuild([mid[0] + d, mid[1], mid[2]], 0.0, &c);
                }
                2 => {
                    let d = self.rng.range(-30.0, 30.0);
                    self.casters[i] = rebuild([mid[0], mid[1], mid[2] + d], 0.0, &c);
                }
                3 => {
                    // Creep: far below a texel per step.
                    self.casters[i] = rebuild([mid[0] + 2.0e-6, mid[1], mid[2]], 0.0, &c);
                }
                4 => {
                    let yaw = self.rng.range(0.0, 3.0);
                    self.casters[i] = rebuild(mid, yaw, &c);
                }
                5 => self.casters[i].dynamic = !c.dynamic,
                6 => self.casters[i].cascades = self.mask(),
                7 => {
                    self.casters.remove(i);
                }
                8 => {
                    let j = self.rng.below(n.max(1) as u64) as usize % self.casters.len();
                    self.casters.swap(i, j);
                }
                9 => {
                    let fresh = self.fresh();
                    self.casters.insert(i, fresh);
                }
                10 => {
                    // A non-finite pose: the full loop counts it as moved every frame.
                    if self.rng.chance(50) {
                        self.casters[i].pose.radius = f32::INFINITY;
                    } else {
                        self.casters[i].pose.model[3][0] = f32::NAN;
                    }
                }
                _ => {
                    let fresh = self.fresh();
                    self.casters.push(fresh);
                }
            }
        }
        if self.rng.chance(4) && !self.casters.is_empty() {
            // An id twice in the list.
            let i = self.rng.below(self.casters.len() as u64) as usize;
            let mut twin = self.casters[i];
            twin.aabb.min[0] += 1.0;
            twin.aabb.max[0] += 1.0;
            let at = self.rng.below(self.casters.len() as u64 + 1) as usize;
            self.casters.insert(at, twin);
        }
        if self.rng.chance(3) {
            // Every duplicate goes again.
            let mut ids = std::collections::HashSet::new();
            self.casters.retain(|c| ids.insert(c.id));
        }
        if self.rng.chance(10) {
            let k = self.rng.below(self.vps.len() as u64) as usize;
            let r = RADII[k % RADII.len()];
            let cx = self.rng.range(-3.0, 3.0);
            self.vps[k] = cascade_vp(r, cx, 0.0);
        }
        if self.rng.chance(10) {
            // Below the matrix tolerance: the same pages, a different matrix.
            let k = self.rng.below(self.vps.len() as u64) as usize;
            self.vps[k][3][2] += 2.0e-6;
        }
        if self.rng.chance(2) {
            self.vps = if self.vps.len() == 4 {
                self.vps[..3].to_vec()
            } else {
                RADII.iter().map(|&r| cascade_vp(r, 0.0, 0.0)).collect()
            };
        }
        if self.rng.chance(10) {
            self.dynamic_mask = [0u8, 0b0001, 0b0011, 0b1111][self.rng.below(4) as usize];
        }
        if self.rng.chance(8) {
            match self.rng.below(3) {
                0 => self.lights.clear(),
                1 => {
                    let id = self.rng.below(4);
                    let light = self.light(id);
                    self.lights.push(light);
                }
                _ => {
                    if let Some(light) = self.lights.first_mut() {
                        light.center[0] += 0.5;
                    }
                }
            }
        }
    }
}

fn footprint<'a>(
    casters: &'a [ShadowCaster],
    vps: &'a [[[f32; 4]; 4]],
) -> impl Fn(usize, usize) -> ([f32; 2], [f32; 2]) + Sync + 'a {
    |cascade, index| project_aabb_to_shadow_uv(casters[index].aabb, vps[cascade])
}

fn assert_same_decisions(step: usize, what: &str, want: &PageUpdate, got: &PageUpdate) {
    assert_eq!(want.dirty, got.dirty, "{what}: step {step} dirty pages");
    assert_eq!(
        want.cascade_full_redraw, got.cascade_full_redraw,
        "{what}: step {step} full redraws"
    );
    assert_eq!(
        want.caster_events, got.caster_events,
        "{what}: step {step} caster events"
    );
    assert_eq!(
        want.dynamic_pages, got.dynamic_pages,
        "{what}: step {step} dynamic pages"
    );
}

/// (a) On randomized scenes over many steps the change-tracked update decides exactly what the
/// full loop decides: dirty pages, full redraws, caster events and dynamic pages, on the frame
/// thread and on tasks; and the atlas cache its tiles. The run also proves it skipped work: the
/// full loop reads more footprints than the tracked one.
#[test]
fn the_tracked_update_decides_what_the_full_loop_decides() {
    let mut new_visits = 0u64;
    let mut old_visits = 0u64;
    let mut changes = [0u32; 3];
    let mut held_steps = 0;
    for seed in 0..6u64 {
        let count = [40usize, 300, 600][seed as usize % 3];
        let mut scene = Scene::new(0xD1CE + seed, count);
        let mut reference = Reference::new(4, 8);
        let mut serial = SunPageCache::new(4, 8);
        let mut parallel = SunPageCache::new(4, 8);
        parallel.set_parallel(true);
        let mut atlas_reference = ReferenceAtlas::default();
        let mut atlas = AtlasTileCache::new();
        for step in 0..300 {
            scene.step();
            let casters = &scene.casters;
            let vps = &scene.vps;
            let mask = scene.dynamic_mask;
            let want = reference.update(casters, vps, mask, footprint(casters, vps));
            let one = serial.update_with_footprints(casters, vps, mask, footprint(casters, vps));
            let two = parallel.update_with_footprints(casters, vps, mask, footprint(casters, vps));
            assert_same_decisions(step, "frame thread", &want, &one);
            assert_same_decisions(step, "tasks", &want, &two);
            assert_eq!(one.casters_visited, two.casters_visited, "step {step}");
            new_visits += u64::from(one.casters_visited);
            old_visits += u64::from(want.casters_visited);
            for (sum, e) in changes.iter_mut().zip(
                want.caster_events
                    .iter()
                    .fold([0; 3], |a, e| [a[0] + e[0], a[1] + e[1], a[2] + e[2]]),
            ) {
                *sum += e;
            }
            // The random scenes always carry dynamic and non-finite casters, so a held step still
            // visits those; count the steps that skipped nine tenths of the full loop's work.
            held_steps += usize::from(one.casters_visited * 10 < want.casters_visited);
            let refresh = mask & 1 != 0;
            let tiles_want = atlas_reference.update_refreshing(&scene.lights, casters, refresh);
            let tiles = atlas.update_refreshing(&scene.lights, casters, refresh);
            assert_eq!(tiles_want, tiles, "atlas: seed {seed} step {step}");
            assert!(
                atlas.casters_visited() <= atlas_reference.visits,
                "atlas step {step}"
            );
        }
    }
    assert!(
        changes.iter().all(|&n| n > 50),
        "the scenes moved, added and removed too few casters to prove much: {changes:?}"
    );
    // These scenes shift the list, repeat ids and re-snap cascades on purpose, to test the
    // decisions; the counted tests below hold the cost. Here it is enough that work was skipped.
    println!("tracked {new_visits} footprints, full loop {old_visits}, {held_steps} steps under a tenth");
    assert!(
        held_steps > 20,
        "too few steps skipped most casters: {held_steps}"
    );
    assert!(
        new_visits < old_visits,
        "the tracked update read {new_visits} footprints, the full loop {old_visits}"
    );
}

fn grid(n: u64, dynamic_every: u64) -> Vec<ShadowCaster> {
    (0..n)
        .map(|i| {
            let x = (i % 100) as f32 * 0.8 - 40.0;
            let z = (i / 100) as f32 * 0.8 - 40.0;
            caster(
                i + 1,
                [x, 0.0, z],
                0.3,
                0.0,
                u8::MAX,
                dynamic_every != 0 && i % dynamic_every == 0,
            )
        })
        .collect()
}

fn vps4() -> Vec<[[f32; 4]; 4]> {
    RADII.iter().map(|&r| cascade_vp(r, 0.0, 0.0)).collect()
}

/// (b) A held scene visits no caster, and the full loop visits every caster in every cascade.
#[test]
fn a_held_scene_visits_no_caster() {
    let casters = grid(2_000, 0);
    let vps = vps4();
    let mut cache = SunPageCache::new(4, 8);
    let mut reference = Reference::new(4, 8);
    let first = cache.update_with_footprints(&casters, &vps, 0b11, footprint(&casters, &vps));
    assert_eq!(first.cascade_full_redraw, vec![true; 4]);
    assert_eq!((first.full_passes, first.casters_visited), (0, 0));
    reference.update(&casters, &vps, 0b11, footprint(&casters, &vps));
    let settling = cache.update_with_footprints(&casters, &vps, 0b11, footprint(&casters, &vps));
    assert_eq!(
        settling.full_passes, 4,
        "the first held update builds and compares"
    );
    assert_eq!(settling.casters_visited, 2 * 2_000 * 4);
    reference.update(&casters, &vps, 0b11, footprint(&casters, &vps));
    for _ in 0..5 {
        let held = cache.update_with_footprints(&casters, &vps, 0b11, footprint(&casters, &vps));
        assert_eq!(held.casters_visited, 0, "{held:?}");
        assert_eq!(held.full_passes, 0);
        assert!(held.dirty.is_empty());
        let base = reference.update(&casters, &vps, 0b11, footprint(&casters, &vps));
        assert_eq!(
            base.casters_visited,
            2_000 * 4,
            "the full loop visits N per cascade"
        );
        assert!(base.dirty.is_empty());
    }
    let mut atlas = AtlasTileCache::new();
    let lights = [AtlasLight {
        id: 1,
        center: [0.0, 2.0, 0.0],
        radius: 10.0,
    }];
    atlas.update_refreshing(&lights, &casters, true);
    assert_eq!(atlas.casters_visited(), 2_000);
    for _ in 0..3 {
        assert_eq!(atlas.update_refreshing(&lights, &casters, true), (vec![], 0));
        assert_eq!(atlas.casters_visited(), 0);
    }
}

/// (b) Visits follow the moved and dynamic casters: each is visited once per cascade it is in,
/// and nothing else is.
#[test]
fn visits_follow_the_moved_and_dynamic_casters() {
    let mut casters = grid(2_000, 250);
    let dynamic = casters.iter().filter(|c| c.dynamic).count() as u32;
    assert_eq!(dynamic, 8);
    let vps = vps4();
    let mut cache = SunPageCache::new(4, 8);
    let mut reference = Reference::new(4, 8);
    for step in 0..7u32 {
        let moved: u32 = if step < 2 { 0 } else { 3 * step };
        for c in casters.iter_mut().skip(7).step_by(97).take(moved as usize) {
            c.aabb.min[0] += 0.5;
            c.aabb.max[0] += 0.5;
            c.pose.model[3][0] += 0.5;
        }
        let update = cache.update_with_footprints(&casters, &vps, 0b0011, footprint(&casters, &vps));
        let base = reference.update(&casters, &vps, 0b0011, footprint(&casters, &vps));
        assert_same_decisions(step as usize, "grid", &base, &update);
        if step < 2 {
            continue;
        }
        // Every caster is in all four cascades; a dynamic one is visited in each of them too
        // (refreshed in two, compared in the other two).
        assert_eq!(update.casters_visited, 4 * (moved + dynamic), "step {step}");
        assert_eq!(update.full_passes, 0);
        assert_eq!(base.casters_visited, 4 * 2_000);
    }
    // Without a refreshed cascade the dynamic casters cache like the rest: their last refresh
    // stored the footprint and pose they still have, which the full loop keeps.
    for _ in 0..2 {
        let held = cache.update_with_footprints(&casters, &vps, 0, footprint(&casters, &vps));
        let base = reference.update(&casters, &vps, 0, footprint(&casters, &vps));
        assert_same_decisions(9, "mask off", &base, &held);
        assert_eq!(held.casters_visited, 0, "{held:?}");
    }
}

/// (b) Appearing and vanishing casters cost themselves; a removal from the middle of the list
/// shifts the casters after it, which are visited (and kept).
#[test]
fn appear_and_vanish_visit_only_what_changed() {
    let mut casters = grid(1_000, 0);
    let vps = vps4();
    let mut cache = SunPageCache::new(4, 8);
    let mut reference = Reference::new(4, 8);
    for _ in 0..2 {
        cache.update_with_footprints(&casters, &vps, 0, footprint(&casters, &vps));
        reference.update(&casters, &vps, 0, footprint(&casters, &vps));
    }
    // Two new casters at the end.
    casters.push(caster(50_000, [1.0, 0.0, 1.0], 0.3, 0.0, u8::MAX, false));
    casters.push(caster(50_001, [-1.0, 0.0, 1.0], 0.3, 0.0, 0b0001, false));
    let update = cache.update_with_footprints(&casters, &vps, 0, footprint(&casters, &vps));
    let base = reference.update(&casters, &vps, 0, footprint(&casters, &vps));
    assert_same_decisions(0, "appear", &base, &update);
    assert_eq!(update.casters_visited, 4 + 1);
    // The last one vanishes: a lookup, no footprint.
    casters.pop();
    let update = cache.update_with_footprints(&casters, &vps, 0, footprint(&casters, &vps));
    let base = reference.update(&casters, &vps, 0, footprint(&casters, &vps));
    assert_same_decisions(1, "vanish", &base, &update);
    assert_eq!(update.casters_visited, 0);
    assert_eq!(update.caster_events[0], [0, 0, 1]);
    // One from the front: the rest shift by one place and are visited, in all four cascades.
    casters.remove(0);
    let update = cache.update_with_footprints(&casters, &vps, 0, footprint(&casters, &vps));
    let base = reference.update(&casters, &vps, 0, footprint(&casters, &vps));
    assert_same_decisions(2, "shift", &base, &update);
    assert_eq!(update.casters_visited, 4 * casters.len() as u32);
    assert_eq!(
        update.full_passes, 0,
        "a shifted list is not a full pass, it is many visits"
    );
}

/// A cascade that re-snaps is redrawn without reading a caster; the first update it holds builds
/// its references and compares, a counted full pass. A matrix moved below the tolerance compares
/// in full too. The other cascades stay quiet throughout.
#[test]
fn a_moved_cascade_takes_a_counted_full_pass_when_it_holds() {
    let casters = grid(1_000, 0);
    let mut vps = vps4();
    let mut cache = SunPageCache::new(4, 8);
    for _ in 0..2 {
        cache.update_with_footprints(&casters, &vps, 0, footprint(&casters, &vps));
    }
    vps[0] = cascade_vp(RADII[0], 1.5, 0.0);
    let snapped = cache.update_with_footprints(&casters, &vps, 0, footprint(&casters, &vps));
    assert_eq!((snapped.full_passes, snapped.casters_visited), (0, 0));
    assert_eq!(snapped.cascade_full_redraw, vec![true, false, false, false]);
    let settling = cache.update_with_footprints(&casters, &vps, 0, footprint(&casters, &vps));
    assert_eq!(settling.full_passes, 1);
    assert_eq!(settling.casters_visited, 2 * 1_000, "built, then compared");
    assert!(settling.dirty.is_empty(), "and its pages stay");
    vps[2][3][2] += 1.0e-6;
    let jittered = cache.update_with_footprints(&casters, &vps, 0, footprint(&casters, &vps));
    assert_eq!(
        jittered.full_passes, 1,
        "a matrix within tolerance compares in full"
    );
    assert_eq!(jittered.casters_visited, 1_000);
    assert!(jittered.dirty.is_empty(), "and keeps its pages");
    let held = cache.update_with_footprints(&casters, &vps, 0, footprint(&casters, &vps));
    assert_eq!((held.full_passes, held.casters_visited), (0, 0));
}

/// (b) A camera that re-snaps every cascade every update, the common case in a game and the bench
/// the cost was measured on, reads no caster, with casters moving or not. When it stops, each
/// cascade pays one counted pass, decides what the full loop decides and goes quiet.
#[test]
fn an_orbiting_camera_visits_no_caster() {
    let mut casters = grid(2_000, 0);
    let mut cache = SunPageCache::new(4, 8);
    let mut reference = Reference::new(4, 8);
    let mut vps = vps4();
    for step in 0..12usize {
        for (k, vp) in vps.iter_mut().enumerate() {
            *vp = cascade_vp(RADII[k], 0.5 * (step % 5) as f32, 0.0);
        }
        // Half of the run has casters moving too: the held list stays copied by what changed.
        if step >= 6 {
            for c in casters.iter_mut().skip(step).step_by(101) {
                c.aabb.min[0] += 0.5;
                c.aabb.max[0] += 0.5;
                c.pose.model[3][0] += 0.5;
            }
        }
        let update = cache.update_with_footprints(&casters, &vps, 0, footprint(&casters, &vps));
        let base = reference.update(&casters, &vps, 0, footprint(&casters, &vps));
        assert_same_decisions(step, "orbit", &base, &update);
        assert_eq!(update.cascade_full_redraw, vec![true; 4]);
        assert_eq!(
            (update.full_passes, update.casters_visited),
            (0, 0),
            "step {step}"
        );
        assert_eq!(
            base.casters_visited,
            2_000 * 4,
            "the full loop stores every caster"
        );
    }
    let stopped = cache.update_with_footprints(&casters, &vps, 0, footprint(&casters, &vps));
    let base = reference.update(&casters, &vps, 0, footprint(&casters, &vps));
    assert_same_decisions(12, "stopped", &base, &stopped);
    assert_eq!(stopped.full_passes, 4);
    assert!(stopped.dirty.is_empty());
    let held = cache.update_with_footprints(&casters, &vps, 0, footprint(&casters, &vps));
    assert_eq!((held.full_passes, held.casters_visited), (0, 0));
}

/// A caster that moved on a held matrix, then a cascade snap on a later frame, must not leave
/// the snap's drawn list at the pre-move box. The held frame after the snap matches the full
/// loop: nothing dirty.
#[test]
fn audfix_move_between_snaps_does_not_dirty_the_held_frame() {
    let mut cache = SunPageCache::new(1, 8);
    let mut reference = Reference::new(1, 8);
    let mut casters = vec![
        caster(1, [1.5, 0.0, 0.0], 0.2, 0.0, u8::MAX, false),
        caster(2, [-1.5, 0.0, 0.0], 0.2, 0.0, u8::MAX, false),
    ];
    let mut vps = vec![cascade_vp(4.0, 0.0, 0.0)];
    for step in 0..2 {
        let update = cache.update_with_footprints(&casters, &vps, 0, footprint(&casters, &vps));
        let base = reference.update(&casters, &vps, 0, footprint(&casters, &vps));
        assert_same_decisions(step, "warm", &base, &update);
    }
    casters[1].aabb.min[0] += 2.0;
    casters[1].aabb.max[0] += 2.0;
    casters[1].pose.model[3][0] += 2.0;
    let update = cache.update_with_footprints(&casters, &vps, 0, footprint(&casters, &vps));
    let base = reference.update(&casters, &vps, 0, footprint(&casters, &vps));
    assert_same_decisions(2, "move", &base, &update);
    vps[0] = cascade_vp(4.0, 0.4, 0.0);
    let update = cache.update_with_footprints(&casters, &vps, 0, footprint(&casters, &vps));
    let base = reference.update(&casters, &vps, 0, footprint(&casters, &vps));
    assert_same_decisions(3, "snap", &base, &update);
    let update = cache.update_with_footprints(&casters, &vps, 0, footprint(&casters, &vps));
    let base = reference.update(&casters, &vps, 0, footprint(&casters, &vps));
    assert_same_decisions(4, "held after snap", &base, &update);
    assert!(
        update.dirty.is_empty(),
        "held frame after the snap dirtied {} pages",
        update.dirty.len()
    );
}

/// A caster with a non-finite pose is redrawn every frame by the full loop; the tracked update
/// keeps visiting it (and only it) rather than skipping it as unchanged.
#[test]
fn a_non_finite_caster_keeps_being_visited() {
    let mut casters = grid(100, 0);
    casters[5].pose.radius = f32::INFINITY;
    let vps = vps4();
    let mut cache = SunPageCache::new(4, 8);
    let mut reference = Reference::new(4, 8);
    for step in 0..5 {
        let update = cache.update_with_footprints(&casters, &vps, 0, footprint(&casters, &vps));
        let base = reference.update(&casters, &vps, 0, footprint(&casters, &vps));
        assert_same_decisions(step, "non-finite", &base, &update);
        if step > 1 {
            assert_eq!(update.casters_visited, 4);
            assert_eq!(update.caster_events.iter().map(|e| e[0]).sum::<u32>(), 4);
        }
    }
}

/// Release-build CPU cost of one update, old and new interleaved on one binary. Prints only.
#[test]
#[ignore = "bench: cargo test --release -p regia_render --lib sun_page_cache_cost_bench -- --ignored --nocapture"]
fn sun_page_cache_cost_bench() {
    use std::time::Instant;
    const STEPS: usize = 120;
    const ROUNDS: usize = 7;
    // name, moved per step, dynamic every nth, dynamic mask, re-snapped cascades, every nth step.
    let scenarios: [(&str, usize, u64, u8, usize, usize); 7] = [
        ("held", 0, 0, 0, 0, 1),
        ("1% move", 100, 0, 0, 0, 1),
        ("5% dynamic, 2 refreshed", 0, 20, 0b11, 0, 1),
        ("near cascade re-snaps", 0, 0, 0, 1, 1),
        ("all cascades re-snap", 0, 0, 0, 4, 1),
        ("all re-snap, 1% move", 100, 0, 0, 4, 1),
        ("all re-snap every 2nd step", 0, 0, 0, 4, 2),
    ];
    println!("| casters | scenario | full loop ms | tracked ms | ratio | visits old | visits new |");
    println!("|---|---|---|---|---|---|---|");
    for n in [1_600u64, 10_000] {
        for &(name, moved_per_10k, dynamic_every, mask, resnap, every) in &scenarios {
            // The scene and its footprints per step, built outside the timed region (the frame
            // projects them for its page lists anyway).
            let moved = (moved_per_10k as u64 * n / 10_000) as usize;
            let mut casters = grid(n, dynamic_every);
            let mut vps = vps4();
            let mut frames = Vec::with_capacity(STEPS + 1);
            for step in 0..=STEPS {
                for c in casters.iter_mut().skip(step % 13).step_by(13).take(moved) {
                    let d = if step % 2 == 0 { 0.4 } else { -0.4 };
                    c.aabb.min[0] += d;
                    c.aabb.max[0] += d;
                    c.pose.model[3][0] += d;
                }
                if step.is_multiple_of(every) {
                    for (k, vp) in vps.iter_mut().enumerate().take(resnap) {
                        *vp = cascade_vp(RADII[k], (step % 5) as f32 * 0.5, 0.0);
                    }
                }
                let uv: Vec<Vec<([f32; 2], [f32; 2])>> = vps
                    .iter()
                    .map(|vp| {
                        casters
                            .iter()
                            .map(|c| project_aabb_to_shadow_uv(c.aabb, *vp))
                            .collect()
                    })
                    .collect();
                frames.push((casters.clone(), vps.clone(), uv));
            }
            let mut old_ms = Vec::new();
            let mut new_ms = Vec::new();
            let (mut old_visits, mut new_visits) = (0u64, 0u64);
            for _ in 0..ROUNDS {
                let mut reference = Reference::new(4, 8);
                let mut cache = SunPageCache::new(4, 8);
                let (casters, vps, uv) = &frames[0];
                reference.update(casters, vps, mask, |k, i| uv[k][i]);
                cache.update_with_footprints(casters, vps, mask, |k, i| uv[k][i]);
                let start = Instant::now();
                for (casters, vps, uv) in &frames[1..] {
                    let u = reference.update(casters, vps, mask, |k, i| uv[k][i]);
                    old_visits += u64::from(u.casters_visited);
                }
                old_ms.push(start.elapsed().as_secs_f64() * 1000.0 / STEPS as f64);
                let start = Instant::now();
                for (casters, vps, uv) in &frames[1..] {
                    let u = cache.update_with_footprints(casters, vps, mask, |k, i| uv[k][i]);
                    new_visits += u64::from(u.casters_visited);
                }
                new_ms.push(start.elapsed().as_secs_f64() * 1000.0 / STEPS as f64);
            }
            let median = |v: &mut Vec<f64>| {
                v.sort_by(f64::total_cmp);
                v[v.len() / 2]
            };
            let (old, new) = (median(&mut old_ms), median(&mut new_ms));
            let per = (ROUNDS * STEPS) as u64;
            println!(
                "| {n} | {name} | {old:.3} | {new:.3} | {:.1}x | {} | {} |",
                old / new.max(1.0e-9),
                old_visits / per,
                new_visits / per
            );
        }
    }
}

/// `specs/14` A1, shadow caching for `Static` casters (review finding 4, ADR-5906): a caster
/// whose entity carries the marker keeps its cached pages through a move below the cascade's
/// sub-texel tolerance, exactly like a plain caster; the marker never makes a caster redraw.
/// A real move still invalidates it (a moved static must not leave a stale shadow).
#[test]
fn spec14_a1_static_caster_does_not_invalidate_its_pages() {
    let vp = cascade_vp(10.0, 0.0, 0.0);
    // A turn in place by a tiny angle: the bounding box misses it, the pose does not, and
    // the displacement is far below `PAGE_MOVE_TOLERANCE_UV` in this cascade.
    let creep = |c: &mut ShadowCaster| {
        let m = regia_core::glam::Mat4::from_cols_array_2d(&c.pose.model);
        c.pose.model = (regia_core::glam::Mat4::from_rotation_y(1.0e-7) * m).to_cols_array_2d();
    };

    let mut plain = caster(1, [0.0, 0.0, 0.0], 0.5, 0.0, 0b1, false);
    let mut marked = caster(2, [3.0, 0.0, 0.0], 0.5, 0.0, 0b1, false);
    marked.is_static = true;
    let mut cache = SunPageCache::new(1, 4);
    cache.update(&[plain, marked], &[vp]);
    assert!(
        cache.update(&[plain, marked], &[vp]).dirty.is_empty(),
        "a bit-identical held frame redraws nothing"
    );
    creep(&mut plain);
    creep(&mut marked);
    let crept = cache.update(&[plain, marked], &[vp]);
    assert!(
        crept.dirty.is_empty() && crept.caster_events.first() == Some(&[0, 0, 0]),
        "below the move tolerance the static caster keeps its pages like the plain one: {crept:?}"
    );

    // A real move (half a metre) still invalidates the static caster's pages.
    let moved = caster(2, [3.5, 0.0, 0.0], 0.5, 0.0, 0b1, false);
    let moved = ShadowCaster {
        is_static: true,
        ..moved
    };
    let real = cache.update(&[plain, moved], &[vp]);
    assert_eq!(
        real.caster_events.first(),
        Some(&[1, 0, 0]),
        "a static caster that really moves redraws: {real:?}"
    );
    assert!(!real.dirty.is_empty());
}
