// SPDX-License-Identifier: MPL-2.0
//! Cached shadow pages and the temporal-flicker metric (M15b).
//!
//! Sun cascades are split into a uniform grid of virtual-texture pages. A page is
//! redrawn only when a caster whose light-space footprint covers it moves, or when
//! the cascade matrix itself changes (the CSM fallback redraws that cascade). Local
//! lights keep the same idea on atlas tiles: a moving caster dirties only the tiles
//! whose light volume overlaps it.
//!
//! ## Provenance
//!
//! Page indirection follows Mittring, "Advanced Virtual Texture Topics", SIGGRAPH
//! 2008. Caster invalidation is Regia Engine's: the union of the caster's previous and
//! current light-space AABBs. The flicker score is a mean-absolute luminance delta
//! with an isolated-pixel term so a shimmering edge fails and a rigid shift does not.

use crate::key_hash::KeyMap;

use regia_core::glam::Vec3;

/// Pages along one edge of a cascade. Resolution should be divisible by this.
///
/// 8 (256-texel pages at 2048²): a moving vehicle dirties a page or two per cascade, and a
/// redrawn page re-rasterises every caster and the terrain under it, so smaller pages
/// redraw less. With 4 the vehicle in the DO27 chase redrew a quarter of three cascades
/// every frame (lane terrain-perf round 4). The pixels are the same either way.
pub const SUN_PAGES_PER_AXIS: u32 = 8;

/// One dirty or cached page.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PageId {
    /// Cascade index.
    pub cascade: u32,
    /// Page column, `0..pages_per_axis`.
    pub x: u32,
    /// Page row, `0..pages_per_axis`.
    pub y: u32,
}

/// Axis-aligned box in world metres.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Aabb3 {
    /// Minimum corner.
    pub min: [f32; 3],
    /// Maximum corner.
    pub max: [f32; 3],
}

impl Aabb3 {
    /// Box from a centre and half extents.
    #[must_use]
    pub fn from_center_half(center: [f32; 3], half: [f32; 3]) -> Self {
        Self {
            min: [
                center[0] - half[0].abs(),
                center[1] - half[1].abs(),
                center[2] - half[2].abs(),
            ],
            max: [
                center[0] + half[0].abs(),
                center[1] + half[1].abs(),
                center[2] + half[2].abs(),
            ],
        }
    }

    /// Eight corners.
    #[must_use]
    pub fn corners(self) -> [[f32; 3]; 8] {
        let (a, b) = (self.min, self.max);
        [
            [a[0], a[1], a[2]],
            [b[0], a[1], a[2]],
            [a[0], b[1], a[2]],
            [b[0], b[1], a[2]],
            [a[0], a[1], b[2]],
            [b[0], a[1], b[2]],
            [a[0], b[1], b[2]],
            [b[0], b[1], b[2]],
        ]
    }

    /// Closest point in the box to `p`.
    #[must_use]
    pub fn clamp_point(self, p: [f32; 3]) -> [f32; 3] {
        [
            p[0].clamp(self.min[0], self.max[0]),
            p[1].clamp(self.min[1], self.max[1]),
            p[2].clamp(self.min[2], self.max[2]),
        ]
    }
}

/// A caster with a stable id. The same id across frames is the same object.
#[derive(Clone, Copy, Debug)]
pub struct ShadowCaster {
    /// Stable object id.
    pub id: u64,
    /// World-space bounds.
    pub aabb: Aabb3,
    /// Sun cascades the caster is drawn into (bit `i` = cascade `i`; `u8::MAX` = all). A
    /// caster outside a cascade neither dirties nor occupies its pages.
    pub cascades: u8,
    /// Where the caster's geometry is. `aabb` alone misses a turn about the bounding
    /// sphere's centre (the box around the sphere does not change), so the caches compare
    /// poses too.
    pub pose: CasterPose,
    /// Dynamic shadow class (ADR-2028): the caster's geometry moves without its bounds or pose
    /// (a time-reading vertex stage such as wind). In the cascades of the update's dynamic
    /// mask its pages are redrawn every frame; elsewhere it caches like any other caster.
    pub dynamic: bool,
    /// The entity carries `regia_scene::components::Static` (`specs/14` A1): its transform is
    /// not expected to change. A static caster caches exactly like any other (ADR-5906): its
    /// pages stay while its pose holds or moves less than the cascade's sub-texel tolerance.
    /// The flag is counted in `ShadowCacheReport::static_casters`; the editor warns when a
    /// static moves in play.
    pub is_static: bool,
}

/// A caster's model matrix and its mesh's bounding sphere in model space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CasterPose {
    /// Model matrix (column-major) the shadow passes draw the caster with.
    pub model: [[f32; 4]; 4],
    /// Bounding-sphere centre, model space.
    pub center: [f32; 3],
    /// Bounding-sphere radius, model space.
    pub radius: f32,
}

impl CasterPose {
    /// Upper bound, in world metres, on how far any point of the bounding sphere moved
    /// between `self` and `other`: the centre's travel plus the radius times the Frobenius
    /// norm of the linear part's change (which bounds its spectral norm). A rotation, a
    /// scale and a move along the light all count; a footprint compare sees none of them
    /// when the box around the sphere stays put.
    #[must_use]
    pub fn displacement_bound(&self, other: &Self) -> f32 {
        let a = regia_core::glam::Mat4::from_cols_array_2d(&self.model);
        let b = regia_core::glam::Mat4::from_cols_array_2d(&other.model);
        let c = Vec3::from_array(self.center);
        let centre = (b.transform_point3(c) - a.transform_point3(c)).length();
        let linear = ((b.x_axis - a.x_axis).truncate().length_squared()
            + (b.y_axis - a.y_axis).truncate().length_squared()
            + (b.z_axis - a.z_axis).truncate().length_squared())
        .sqrt();
        let d = centre + linear * self.radius.max(other.radius);
        // NaN (a broken matrix) counts as moved (callers keep pages only while `d <= tolerance`).
        if d.is_nan() { f32::INFINITY } else { d }
    }
}

/// UV rectangle in the cascade, both axes in `0..1` (shadow-map space, Y down).
#[derive(Clone, Copy, Debug)]
struct Aabb2 {
    min: [f32; 2],
    max: [f32; 2],
}

impl Aabb2 {
    fn union(self, other: Self) -> Self {
        Self {
            min: [self.min[0].min(other.min[0]), self.min[1].min(other.min[1])],
            max: [self.max[0].max(other.max[0]), self.max[1].max(other.max[1])],
        }
    }

    fn nearly_same(self, other: Self) -> bool {
        const E: f32 = 1.0e-4;
        (self.min[0] - other.min[0]).abs() < E
            && (self.min[1] - other.min[1]).abs() < E
            && (self.max[0] - other.max[0]).abs() < E
            && (self.max[1] - other.max[1]).abs() < E
    }
}

/// Caster travel, in cascade UV, below which its pages are kept (about a fifth of a texel
/// at 2048²; the footprint compare's tolerance).
const PAGE_MOVE_TOLERANCE_UV: f32 = 1.0e-4;

/// Shadow-map UV per world metre across an orthographic cascade (the larger of its two
/// axes; `shadow_uv_from_clip` maps clip `[-1, 1]` to UV `[0, 1]`).
fn uv_per_metre(light_vp: &[[f32; 4]; 4]) -> f32 {
    let row = |r: usize| Vec3::new(light_vp[0][r], light_vp[1][r], light_vp[2][r]).length();
    let uv = 0.5 * row(0).max(row(1));
    if uv.is_finite() && uv > 0.0 {
        uv
    } else {
        f32::INFINITY
    }
}

/// Projects a world AABB into shadow-map UV using the cascade view-projection.
///
/// The Y flip matches `shadow_uv_from_clip` in `shaders/shadows.wgsl`.
#[must_use]
pub fn project_aabb_to_shadow_uv(aabb: Aabb3, light_vp: [[f32; 4]; 4]) -> ([f32; 2], [f32; 2]) {
    let m = regia_core::glam::Mat4::from_cols_array_2d(&light_vp);
    let mut min = [f32::MAX; 2];
    let mut max = [f32::MIN; 2];
    for corner in aabb.corners() {
        let clip = m * Vec3::from_array(corner).extend(1.0);
        let inv_w = 1.0 / clip.w.max(1.0e-6);
        let ndc_x = clip.x * inv_w;
        let ndc_y = clip.y * inv_w;
        let u = ndc_x * 0.5 + 0.5;
        let v = 1.0 - (ndc_y * 0.5 + 0.5);
        min[0] = min[0].min(u);
        min[1] = min[1].min(v);
        max[0] = max[0].max(u);
        max[1] = max[1].max(v);
    }
    (min, max)
}

/// Appends the page ids `uv` covers. `out` keeps its allocation across frames.
fn cover_into(out: &mut Vec<(u32, u32)>, uv: Aabb2, n: u32) {
    if n == 0 {
        return;
    }
    let n_f = n as f32;
    let x0 = (uv.min[0] * n_f).floor().clamp(0.0, n_f - 1.0) as u32;
    let y0 = (uv.min[1] * n_f).floor().clamp(0.0, n_f - 1.0) as u32;
    let mut x1 = ((uv.max[0] * n_f).ceil() - 1.0).clamp(0.0, n_f - 1.0) as u32;
    let mut y1 = ((uv.max[1] * n_f).ceil() - 1.0).clamp(0.0, n_f - 1.0) as u32;
    // A zero-area coordinate exactly on an interior boundary crossed the range (floor
    // names the page the boundary texels belong to); keep that page so the footprint
    // dirties what it covers.
    if x0 > x1 {
        x1 = x0;
    }
    if y0 > y1 {
        y1 = y0;
    }
    for y in y0..=y1 {
        for x in x0..=x1 {
            out.push((x, y));
        }
    }
}

#[cfg(test)]
thread_local! {
    static PAGE_LIST_GROWS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn page_list_grows() -> u64 {
    PAGE_LIST_GROWS.with(std::cell::Cell::get)
}

#[cfg(test)]
fn note_page_list_growth() {
    PAGE_LIST_GROWS.with(|cell| cell.set(cell.get() + 1));
}

/// [`cover_into`] into a fresh list (the reference scan in `incremental_tests`).
#[cfg(test)]
fn pages_covering(uv: Aabb2, n: u32) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    cover_into(&mut out, uv, n);
    out
}

pub(crate) fn matrices_match(a: [[f32; 4]; 4], b: [[f32; 4]; 4]) -> bool {
    a.iter()
        .flatten()
        .zip(b.iter().flatten())
        .all(|(p, q)| (p - q).abs() < 1.0e-5)
}

/// Casters below which a parallel update stays on the frame thread: spawning the tasks costs
/// more than the work they would take over.
const PARALLEL_MIN_CASTERS: usize = 256;

/// `casters` is enough work for one task per cascade ([`crate::FramePipelining`]).
pub(crate) fn worth_a_task_per_cascade(casters: usize) -> bool {
    casters >= PARALLEL_MIN_CASTERS
}

/// A caster's footprint in one cascade: shadow-map UV min and max, `None` outside its mask.
pub(crate) type CasterFootprint = Option<([f32; 2], [f32; 2])>;

/// Fills `lists[k]` with each caster's footprint in cascade `k`, one task per cascade: what the
/// frame's serial loop writes, in the same order. A caster outside a cascade's mask gets `None`.
pub(crate) fn project_footprints_parallel(
    lists: &mut [Vec<CasterFootprint>],
    light_vps: &[[[f32; 4]; 4]],
    casters: &[(Option<Aabb3>, u8)],
) {
    regia_core::tasks::scope::<_, ()>(|scope| {
        for (k, (list, vp)) in lists.iter_mut().zip(light_vps).enumerate() {
            scope.spawn(async move {
                let _span = tracing::info_span!("sun_footprints_cascade", cascade = k).entered();
                let bit = 1u8 << k.min(7);
                list.extend(casters.iter().map(|&(aabb, mask)| {
                    aabb.filter(|_| mask & bit != 0)
                        .map(|aabb| project_aabb_to_shadow_uv(aabb, *vp))
                }));
            });
        }
    });
}

/// Every bit of `a` equals `b`'s: the same id, mask, class, bounds and pose. A caster that
/// compares equal here and whose cascade matrix is bit-for-bit last update's gets the footprint
/// it had then (the footprint is a function of the bounds and the matrix), so the caches decide
/// for it what they decided then (ADR-3079).
///
/// Runs once per caster per update and nearly always finds them equal, so the floats go into one
/// word with no early exit. The destructure makes a new field a compile error here.
fn same_bits(a: &ShadowCaster, b: &ShadowCaster) -> bool {
    let ShadowCaster {
        id,
        aabb,
        cascades,
        pose: CasterPose {
            model,
            center,
            radius,
        },
        dynamic,
        is_static,
    } = a;
    let mut diff = radius.to_bits() ^ b.pose.radius.to_bits();
    let mut add = |p: &[f32], q: &[f32]| {
        for (x, y) in p.iter().zip(q) {
            diff |= x.to_bits() ^ y.to_bits();
        }
    };
    add(&aabb.min, &b.aabb.min);
    add(&aabb.max, &b.aabb.max);
    add(model.as_flattened(), b.pose.model.as_flattened());
    add(center, &b.pose.center);
    diff == 0 && *id == b.id && *cascades == b.cascades && *dynamic == b.dynamic && *is_static == b.is_static
}

fn same_matrix_bits(a: &[[f32; 4]; 4], b: &[[f32; 4]; 4]) -> bool {
    a.as_flattened()
        .iter()
        .zip(b.as_flattened())
        .all(|(p, q)| p.to_bits() == q.to_bits())
}

/// Change scan over a caster list (ADR-3079). It keeps the last update's list by position, so an
/// update compares each entry with the one at the same position (no lookup, no footprint) and
/// hands the caches only the positions that differ, plus the ids that left the list. A list in
/// the same order as last frame, the frame's usual case, then costs the caches nothing for its
/// unchanged casters.
#[derive(Clone, Debug, Default)]
struct CasterScan {
    /// Last update's casters, by position.
    prev: Vec<ShadowCaster>,
    /// `prev[i].pose.displacement_bound(&prev[i].pose)`: what a reference drawn with this pose
    /// measures against the same pose (0 for a finite pose, infinity otherwise).
    pose_self: Vec<f32>,
    /// Positions a cache must visit although they did not change: what it stored for them
    /// would not compare as kept against the same caster again (a non-finite footprint or
    /// pose, which the full loop counts as moved every frame).
    unstable: Vec<bool>,
    /// Occurrences of each id in `prev`.
    counts: KeyMap<u64, u32>,
    /// Ids with more than one occurrence in `prev`.
    duplicates: u32,
    /// `prev` and `counts` describe the last update, and so does the owner's state.
    synced: bool,
    /// The last update had duplicate ids. The full loop keeps the last occurrence's reference
    /// for such an id, which an unchanged earlier occurrence must not skip against, so the
    /// update after one compares in full too.
    mixed: bool,
    /// This update's positions to visit, ascending.
    visit: Vec<u32>,
    /// Ids gone from the list since the last update.
    gone: Vec<u64>,
    /// Scratch: ids that left a position this update.
    left: Vec<u64>,
    /// Which list `prev` is: one more after a scan that found it changed (`visit` then names
    /// every position that differs), two more after a resync (which names none, so nothing
    /// can catch up from it).
    version: u64,
}

impl CasterScan {
    /// The owner dropped its state: the next scan starts over.
    fn forget(&mut self) {
        self.synced = false;
    }

    /// Compares `casters` with the last update's list and leaves `prev` equal to `casters`.
    /// `extra` names unchanged casters that must be visited anyway (refreshed dynamic casters).
    ///
    /// Returns `true` when the owner may update from `visit` and `gone` alone, `false` when it
    /// must compare every caster: the first update, after [`Self::forget`], or with duplicate ids
    /// in this update or the last.
    fn scan(&mut self, casters: &[ShadowCaster], extra: impl Fn(&ShadowCaster) -> bool) -> bool {
        self.visit.clear();
        self.gone.clear();
        if !self.synced {
            self.resync(casters);
            return false;
        }
        self.left.clear();
        let old_len = self.prev.len();
        let mut changed = casters.len() != old_len;
        for (i, caster) in casters.iter().enumerate() {
            if i < old_len {
                if same_bits(caster, &self.prev[i]) {
                    if self.unstable[i] || extra(caster) {
                        self.visit.push(i as u32);
                    }
                    continue;
                }
                changed = true;
                let old = self.prev[i].id;
                if old != caster.id {
                    self.leave(old);
                    self.enter(caster.id);
                }
                self.prev[i] = *caster;
                self.pose_self[i] = caster.pose.displacement_bound(&caster.pose);
            } else {
                self.enter(caster.id);
                self.prev.push(*caster);
                self.pose_self.push(caster.pose.displacement_bound(&caster.pose));
                self.unstable.push(false);
            }
            self.visit.push(i as u32);
        }
        for i in casters.len()..old_len {
            let id = self.prev[i].id;
            self.leave(id);
        }
        self.prev.truncate(casters.len());
        self.pose_self.truncate(casters.len());
        self.unstable.truncate(casters.len());
        for &id in &self.left {
            if self.counts.get(&id) == Some(&0) {
                self.counts.remove(&id);
                self.gone.push(id);
            }
        }
        self.version += u64::from(changed);
        let incremental = self.duplicates == 0 && !self.mixed;
        self.mixed = self.duplicates > 0;
        incremental
    }

    fn resync(&mut self, casters: &[ShadowCaster]) {
        self.prev.clear();
        self.prev.extend_from_slice(casters);
        self.pose_self.clear();
        self.pose_self
            .extend(casters.iter().map(|c| c.pose.displacement_bound(&c.pose)));
        self.unstable.clear();
        self.unstable.resize(casters.len(), false);
        self.counts.clear();
        self.duplicates = 0;
        for caster in casters {
            self.enter(caster.id);
        }
        self.mixed = self.duplicates > 0;
        self.synced = true;
        self.version += 2;
    }

    fn enter(&mut self, id: u64) {
        let count = self.counts.entry(id).or_insert(0);
        *count += 1;
        if *count == 2 {
            self.duplicates += 1;
        }
    }

    fn leave(&mut self, id: u64) {
        self.left.push(id);
        if let Some(count) = self.counts.get_mut(&id) {
            if *count == 2 {
                self.duplicates -= 1;
            }
            *count = count.saturating_sub(1);
        }
    }

    /// Records which positions the owner found unstable this update: `all` after a pass over
    /// every caster, else only the visited positions are re-decided (an unvisited position is
    /// stable, or it would have been visited).
    fn mark_unstable<'a>(&mut self, all: bool, unstable: impl IntoIterator<Item = &'a [u32]>) {
        if all {
            self.unstable.fill(false);
        } else {
            for &i in &self.visit {
                self.unstable[i as usize] = false;
            }
        }
        for list in unstable {
            for &i in list {
                if let Some(flag) = self.unstable.get_mut(i as usize) {
                    *flag = true;
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
struct CascadePages {
    light_vp: [[f32; 4]; 4],
    has_vp: bool,
    /// The matrix of the last update, bit for bit (`light_vp` is the one the pages were drawn
    /// with, which a matrix within `matrices_match` of it does not replace).
    last_vp: [[f32; 4]; 4],
    /// Per caster: the footprint and pose its pages were last drawn with (not last frame's:
    /// a caster creeping by less than the tolerance per frame must still redraw once the
    /// creep adds up).
    casters: KeyMap<u64, (Aabb2, CasterPose)>,
    /// This frame's footprints while they are compared with `casters`; swapped with it
    /// afterwards, so neither map is reallocated per frame.
    seen: KeyMap<u64, (Aabb2, CasterPose)>,
    /// Page ids of moved, new and gone static casters. Cleared and refilled each steady frame.
    page_ids: Vec<(u32, u32)>,
    /// Page ids of dynamic casters in a refreshed cascade. Cleared and refilled each steady frame.
    dynamic_ids: Vec<(u32, u32)>,
    /// `casters` is out of date: the cascade was redrawn since it was built, and what its pages
    /// hold is `drawn` at `light_vp`. A redraw decides nothing from `casters`, so a camera that
    /// re-snaps the cascade every frame never pays to build it.
    deferred: bool,
    /// The caster list of the last redraw, by position.
    drawn: Vec<ShadowCaster>,
    /// The [`CasterScan::version`] of `drawn`.
    drawn_version: u64,
}

impl CascadePages {
    fn new() -> Self {
        Self {
            light_vp: [[0.0; 4]; 4],
            has_vp: false,
            last_vp: [[0.0; 4]; 4],
            casters: KeyMap::default(),
            seen: KeyMap::default(),
            deferred: false,
            drawn: Vec::new(),
            drawn_version: 0,
            page_ids: Vec::new(),
            dynamic_ids: Vec::new(),
        }
    }
}

/// What one cascade's update needs besides its own state.
struct CascadeInputs<'a, F> {
    casters: &'a [ShadowCaster],
    /// [`CasterScan::visit`] and [`CasterScan::gone`], read when `incremental`.
    visit: &'a [u32],
    gone: &'a [u64],
    /// [`CasterScan::pose_self`], by caster index.
    pose_self: &'a [f32],
    /// [`CasterScan::version`].
    version: u64,
    /// The cascades may update from `visit` and `gone` ([`CasterScan::scan`]).
    incremental: bool,
    dynamic_mask: u8,
    pages_per_axis: u32,
    footprint: &'a F,
}

/// What one cascade's update decided (besides the dirty pages it appended).
#[derive(Default)]
struct CascadeOutcome {
    full_redraw: bool,
    events: [u32; 3],
    dynamic_pages: u32,
    /// Casters whose footprint it read.
    visited: u32,
    /// It compared or stored every caster in the cascade.
    full_pass: bool,
    /// Caster indices whose stored reference does not settle ([`CasterScan::unstable`]).
    unstable: Vec<u32>,
}

/// A reference drawn with footprint `uv` and the caster's own pose compares as kept against
/// the same caster: the full loop's keep test with both sides equal.
fn settles(uv: Aabb2, pose_self: f32, tolerance_m: f32) -> bool {
    uv.nearly_same(uv) && pose_self <= tolerance_m
}

/// What a page-cache update decided for one frame.
#[derive(Clone, Debug, Default)]
pub struct PageUpdate {
    /// Pages whose casters moved or whose cascade matrix changed.
    pub dirty: Vec<PageId>,
    /// Per cascade: redraw the whole map (matrix change, first frame, or every page dirty).
    pub cascade_full_redraw: Vec<bool>,
    /// Per cascade, in kept cascades: casters that moved, appeared and disappeared.
    pub caster_events: Vec<[u32; 3]>,
    /// Pages dirtied by dynamic casters in the dynamic mask's cascades (counted per cascade
    /// before the merge with other dirty pages; a fully redrawn cascade adds none).
    pub dynamic_pages: u32,
    /// Caster and cascade pairs whose footprint the update read, to compare or to store
    /// (ADR-3079): 0 on a held scene and on a cascade being redrawn, the changed and refreshed
    /// dynamic casters' cascades otherwise, and every caster of a cascade that took a full pass
    /// (twice in the first update it holds after a redraw: once to build, once to compare).
    pub casters_visited: u32,
    /// Cascades that compared every caster: the first update a cascade holds after a redraw, a
    /// matrix within tolerance that differs in some bit, or a caster list with duplicate ids. A
    /// redraw is not one: it decides nothing from the casters ([`Self::cascade_full_redraw`]).
    pub full_passes: u32,
}

/// Sun-shadow page cache. One grid per cascade.
#[derive(Clone, Debug)]
pub struct SunPageCache {
    pages_per_axis: u32,
    cascades: Vec<CascadePages>,
    /// [`Self::set_parallel`].
    parallel: bool,
    /// The last update ran on tasks ([`Self::ran_on_tasks`]).
    on_tasks: bool,
    /// What changed in the caster list since the last update (ADR-3079).
    scan: CasterScan,
}

impl Default for SunPageCache {
    fn default() -> Self {
        Self::new(1, SUN_PAGES_PER_AXIS)
    }
}

impl SunPageCache {
    /// `cascades` matrices will be supplied to [`Self::update`]. `pages_per_axis` is at least 1.
    #[must_use]
    pub fn new(cascades: u32, pages_per_axis: u32) -> Self {
        let n = cascades.max(1) as usize;
        Self {
            pages_per_axis: pages_per_axis.max(1),
            cascades: (0..n).map(|_| CascadePages::new()).collect(),
            parallel: false,
            on_tasks: false,
            scan: CasterScan::default(),
        }
    }

    /// Pages along one edge.
    #[must_use]
    pub fn pages_per_axis(&self) -> u32 {
        self.pages_per_axis
    }

    /// Marks pages dirty. `light_vps[i]` is cascade `i`.
    ///
    /// A caster that did not move leaves its pages clean. A caster that moved
    /// dirties the pages covered by the union of its old and new footprints.
    /// A changed cascade matrix dirties every page of that cascade (CSM redraw).
    #[cfg(test)]
    pub fn update(&mut self, casters: &[ShadowCaster], light_vps: &[[[f32; 4]; 4]]) -> PageUpdate {
        self.update_with_footprints(casters, light_vps, 0, |cascade, index| {
            project_aabb_to_shadow_uv(casters[index].aabb, light_vps[cascade])
        })
    }

    /// [`Self::update`] with the footprints supplied by the caller:
    /// `footprint(cascade, caster_index)` must equal
    /// `project_aabb_to_shadow_uv(casters[caster_index].aabb, light_vps[cascade])` (the
    /// frame already projects every caster for its per-page caster lists).
    ///
    /// `dynamic_mask` (bit `i` = cascade `i`): a [`ShadowCaster::dynamic`] caster dirties the
    /// pages its footprint covers in these cascades on every update, moved or not (ADR-2028).
    ///
    /// The cost follows what changed (ADR-3079): a caster bit-for-bit equal to the one at the
    /// same position last update, in a cascade whose matrix is bit-for-bit last update's, is
    /// neither looked up nor projected, and `footprint` is called only for the casters that are.
    /// A redrawn cascade reads no footprint at all; it builds its references from
    /// `project_aabb_to_shadow_uv` in the first update it holds, hence the equality above.
    #[tracing::instrument(skip_all, fields(casters, casters_visited, full_passes))]
    pub(crate) fn update_with_footprints(
        &mut self,
        casters: &[ShadowCaster],
        light_vps: &[[[f32; 4]; 4]],
        dynamic_mask: u8,
        footprint: impl Fn(usize, usize) -> ([f32; 2], [f32; 2]) + Sync,
    ) -> PageUpdate {
        if self.cascades.len() != light_vps.len() {
            self.cascades = (0..light_vps.len()).map(|_| CascadePages::new()).collect();
            self.scan.forget();
        }
        let incremental = {
            let _span = tracing::info_span!("sun_pages_scan").entered();
            self.scan
                .scan(casters, |c| c.dynamic && c.cascades & dynamic_mask != 0)
        };
        let inputs = CascadeInputs {
            casters,
            visit: &self.scan.visit,
            gone: &self.scan.gone,
            pose_self: &self.scan.pose_self,
            version: self.scan.version,
            incremental,
            dynamic_mask,
            pages_per_axis: self.pages_per_axis,
            footprint: &footprint,
        };
        let mut dirty = Vec::new();
        let mut outcomes = Vec::with_capacity(light_vps.len());
        self.on_tasks = self.parallel && light_vps.len() > 1 && worth_a_task_per_cascade(casters.len());
        if self.on_tasks {
            // Each cascade touches only its own state, so each is a task. The frame's result
            // does not depend on the order: `dirty` is sorted below, the rest is per cascade.
            let inputs = &inputs;
            let cascades = self.cascades.iter_mut();
            let results = regia_core::tasks::scope(|scope| {
                for (index, (cascade, vp)) in cascades.zip(light_vps).enumerate() {
                    scope.spawn(async move {
                        let _span = tracing::info_span!("sun_pages_cascade", cascade = index).entered();
                        let mut dirty = Vec::new();
                        let outcome = cascade.update((index, vp), inputs, &mut dirty);
                        (dirty, outcome)
                    });
                }
            });
            // `tasks::scope` returns the results in spawn order: cascade order.
            for (cascade_dirty, outcome) in results {
                dirty.extend(cascade_dirty);
                outcomes.push(outcome);
            }
        } else {
            for (index, (cascade, vp)) in self.cascades.iter_mut().zip(light_vps).enumerate() {
                outcomes.push(cascade.update((index, vp), &inputs, &mut dirty));
            }
        }
        self.scan
            .mark_unstable(!incremental, outcomes.iter().map(|o| o.unstable.as_slice()));
        dirty.sort();
        dirty.dedup();
        let update = PageUpdate {
            dirty,
            cascade_full_redraw: outcomes.iter().map(|o| o.full_redraw).collect(),
            caster_events: outcomes.iter().map(|o| o.events).collect(),
            dynamic_pages: outcomes.iter().map(|o| o.dynamic_pages).sum(),
            casters_visited: outcomes.iter().map(|o| o.visited).sum(),
            full_passes: outcomes.iter().filter(|o| o.full_pass).count() as u32,
        };
        let span = tracing::Span::current();
        span.record("casters", casters.len());
        span.record("casters_visited", update.casters_visited);
        span.record("full_passes", update.full_passes);
        update
    }

    /// Whether [`Self::update_with_footprints`] updates each cascade on its own task
    /// ([`crate::FramePipelining::parallel_shadow_plan`]). The result is the same.
    pub(crate) fn set_parallel(&mut self, parallel: bool) {
        self.parallel = parallel;
    }

    /// See [`Self::set_parallel`].
    pub(crate) fn parallel(&self) -> bool {
        self.parallel
    }

    /// The last update ran on compute-pool tasks: the switch is on and there were enough casters
    /// and cascades.
    pub(crate) fn ran_on_tasks(&self) -> bool {
        self.on_tasks
    }
}

impl CascadePages {
    /// One cascade's share of [`SunPageCache::update_with_footprints`]: appends its dirty pages to
    /// `dirty` and returns what it decided.
    ///
    /// A changed matrix redraws the cascade, which reads no caster. Otherwise, with the matrix
    /// bit-for-bit last update's, an incremental scan and the references built, only the scan's
    /// visited positions and gone ids are compared; else every caster is (the full loop, which
    /// any position-independent order of the same casters decides the same way).
    fn update<F>(
        &mut self,
        (cascade_index, vp): (usize, &[[f32; 4]; 4]),
        inputs: &CascadeInputs<'_, F>,
        dirty: &mut Vec<PageId>,
    ) -> CascadeOutcome
    where
        F: Fn(usize, usize) -> ([f32; 2], [f32; 2]) + Sync,
    {
        let bit = 1u8 << cascade_index.min(7);
        let pages_per_axis = inputs.pages_per_axis;
        let mut out = CascadeOutcome::default();
        let matrix_changed = !self.has_vp || !matrices_match(self.light_vp, *vp);
        let same_vp = self.has_vp && same_matrix_bits(&self.last_vp, vp);
        self.last_vp = *vp;
        // Metres of caster travel that make `PAGE_MOVE_TOLERANCE_UV` in this cascade.
        let tolerance_m = PAGE_MOVE_TOLERANCE_UV / uv_per_metre(vp);
        if matrix_changed {
            self.has_vp = true;
            self.light_vp = *vp;
            self.remember_drawn(inputs);
            for y in 0..pages_per_axis {
                for x in 0..pages_per_axis {
                    dirty.push(PageId {
                        cascade: cascade_index as u32,
                        x,
                        y,
                    });
                }
            }
            out.full_redraw = true;
            return out;
        }
        let mut cascade_dirty = std::mem::take(&mut self.page_ids);
        let mut dynamic_dirty = std::mem::take(&mut self.dynamic_ids);
        #[cfg(test)]
        let cascade_cap = cascade_dirty.capacity();
        #[cfg(test)]
        let dynamic_cap = dynamic_dirty.capacity();
        cascade_dirty.clear();
        dynamic_dirty.clear();
        let mut pass = CascadePass {
            cascade_index,
            refresh_dynamic: inputs.dynamic_mask & bit != 0,
            tolerance_m,
            pages_per_axis,
            cascade_dirty: &mut cascade_dirty,
            dynamic_dirty: &mut dynamic_dirty,
            out: &mut out,
        };
        let full_pass = self.deferred || !inputs.incremental || !same_vp;
        if full_pass {
            self.build_deferred(bit, &mut pass);
            self.compare_all(bit, inputs, &mut pass);
        } else {
            self.compare_changed(bit, inputs, &mut pass);
        }
        out.full_pass = full_pass;
        dynamic_dirty.sort_unstable();
        dynamic_dirty.dedup();
        out.dynamic_pages = dynamic_dirty.len() as u32;
        cascade_dirty.append(&mut dynamic_dirty);
        cascade_dirty.sort_unstable();
        cascade_dirty.dedup();
        #[cfg(test)]
        {
            if cascade_dirty.capacity() > cascade_cap {
                note_page_list_growth();
            }
            if dynamic_dirty.capacity() > dynamic_cap {
                note_page_list_growth();
            }
        }
        let page_count = pages_per_axis * pages_per_axis;
        out.full_redraw = cascade_dirty.len() as u32 >= page_count && page_count > 0;
        for &(x, y) in &cascade_dirty {
            dirty.push(PageId {
                cascade: cascade_index as u32,
                x,
                y,
            });
        }
        self.page_ids = cascade_dirty;
        self.dynamic_ids = dynamic_dirty;
        out
    }

    /// A redraw: `drawn` becomes this update's caster list. A matrix snap is not a held-camera
    /// path, so any version change copies the whole list. Copying only `visit` is wrong when the
    /// version moved on an earlier frame: that frame's visit has already been cleared, and the
    /// next held frame would project the stale boxes.
    fn remember_drawn<F>(&mut self, inputs: &CascadeInputs<'_, F>) {
        self.deferred = true;
        if self.drawn_version == inputs.version {
            return;
        }
        self.drawn.clear();
        self.drawn.extend_from_slice(inputs.casters);
        self.drawn_version = inputs.version;
    }

    /// Builds the references a deferred redraw stands for: each caster of `drawn` in the cascade,
    /// with its footprint at the matrix it was drawn with.
    fn build_deferred(&mut self, bit: u8, pass: &mut CascadePass<'_>) {
        if !self.deferred {
            return;
        }
        self.deferred = false;
        self.casters.clear();
        for caster in self.drawn.iter().filter(|c| c.cascades & bit != 0) {
            let (min, max) = project_aabb_to_shadow_uv(caster.aabb, self.light_vp);
            self.casters.insert(caster.id, (Aabb2 { min, max }, caster.pose));
            pass.out.visited += 1;
        }
    }

    /// Decides one caster in the cascade against what its pages were drawn with, and returns
    /// the reference to keep for it (the full loop's per-caster rule).
    fn decide(
        &self,
        caster: &ShadowCaster,
        uv: Aabb2,
        pose_self: f32,
        pass: &mut CascadePass<'_>,
    ) -> ((Aabb2, CasterPose), bool) {
        let n = pass.pages_per_axis;
        if caster.dynamic && pass.refresh_dynamic {
            // Its geometry moves inside the bounds every frame: redraw what it covers
            // now and what it covered when its pages were last drawn.
            let covered = self.casters.get(&caster.id).map_or(uv, |&(old, _)| old.union(uv));
            cover_into(pass.dynamic_dirty, covered, n);
            return ((uv, caster.pose), settles(uv, pose_self, pass.tolerance_m));
        }
        if let Some(&(old, old_pose)) = self.casters.get(&caster.id) {
            if old.nearly_same(uv) && old_pose.displacement_bound(&caster.pose) <= pass.tolerance_m {
                // Still within tolerance of what its pages hold: keep that reference, which
                // the same caster next update compares as kept again.
                ((old, old_pose), true)
            } else {
                pass.out.events[0] += 1;
                cover_into(pass.cascade_dirty, old.union(uv), n);
                ((uv, caster.pose), settles(uv, pose_self, pass.tolerance_m))
            }
        } else {
            pass.out.events[1] += 1;
            cover_into(pass.cascade_dirty, uv, n);
            ((uv, caster.pose), settles(uv, pose_self, pass.tolerance_m))
        }
    }

    /// The full loop: every caster in the cascade is compared, and casters missing from the
    /// list dirty their old pages.
    fn compare_all<F>(&mut self, bit: u8, inputs: &CascadeInputs<'_, F>, pass: &mut CascadePass<'_>)
    where
        F: Fn(usize, usize) -> ([f32; 2], [f32; 2]) + Sync,
    {
        let mut seen = std::mem::take(&mut self.seen);
        seen.clear();
        for (index, caster) in inputs
            .casters
            .iter()
            .enumerate()
            .filter(|(_, c)| c.cascades & bit != 0)
        {
            let (min, max) = (inputs.footprint)(pass.cascade_index, index);
            pass.out.visited += 1;
            let (reference, settled) = self.decide(caster, Aabb2 { min, max }, inputs.pose_self[index], pass);
            seen.insert(caster.id, reference);
            if !settled {
                pass.out.unstable.push(index as u32);
            }
        }
        // Casters gone since the last frame dirty their old pages.
        for (id, (old, _)) in &self.casters {
            if !seen.contains_key(id) {
                pass.out.events[2] += 1;
                cover_into(pass.cascade_dirty, *old, pass.pages_per_axis);
            }
        }
        self.seen = std::mem::replace(&mut self.casters, seen);
    }

    /// The incremental pass (ADR-3079): only the scan's visited positions and gone ids. The
    /// casters it skips are the same as at the last update, at the same matrix, with a
    /// reference that settles, so the full loop would keep each of them as it is. Ids are
    /// unique here (the scan falls back to the full loop otherwise), so the cascade's map
    /// holds exactly the ids in the list whose mask has the cascade's bit.
    fn compare_changed<F>(&mut self, bit: u8, inputs: &CascadeInputs<'_, F>, pass: &mut CascadePass<'_>)
    where
        F: Fn(usize, usize) -> ([f32; 2], [f32; 2]) + Sync,
    {
        let n = pass.pages_per_axis;
        for &index in inputs.visit {
            let index = index as usize;
            let caster = &inputs.casters[index];
            if caster.cascades & bit == 0 {
                // Left this cascade (or never was in it): gone from it, if it was.
                if let Some((old, _)) = self.casters.remove(&caster.id) {
                    pass.out.events[2] += 1;
                    cover_into(pass.cascade_dirty, old, n);
                }
                continue;
            }
            let (min, max) = (inputs.footprint)(pass.cascade_index, index);
            pass.out.visited += 1;
            let (reference, settled) = self.decide(caster, Aabb2 { min, max }, inputs.pose_self[index], pass);
            self.casters.insert(caster.id, reference);
            if !settled {
                pass.out.unstable.push(index as u32);
            }
        }
        for id in inputs.gone {
            if let Some((old, _)) = self.casters.remove(id) {
                pass.out.events[2] += 1;
                cover_into(pass.cascade_dirty, old, n);
            }
        }
    }
}

/// One cascade's running decisions while its casters are compared.
struct CascadePass<'a> {
    cascade_index: usize,
    refresh_dynamic: bool,
    tolerance_m: f32,
    pages_per_axis: u32,
    cascade_dirty: &'a mut Vec<(u32, u32)>,
    dynamic_dirty: &'a mut Vec<(u32, u32)>,
    out: &'a mut CascadeOutcome,
}

/// Pixel rectangle of one page inside a square cascade of `resolution` texels.
///
/// The rectangle is the texels of the page's UV square `[page / n, (page + 1) / n)`, so
/// `pages_covering` (which dirties by UV) and this scissor name the same texels even when
/// `resolution` is not divisible by `pages_per_axis`.
#[must_use]
pub fn page_texel_rect(page_x: u32, page_y: u32, pages_per_axis: u32, resolution: u32) -> [u32; 4] {
    let n = u64::from(pages_per_axis.max(1));
    let res = u64::from(resolution.max(1));
    let axis = |page: u32| -> (u32, u32) {
        let p = u64::from(page.min(pages_per_axis.max(1) - 1));
        let start = (p * res / n) as u32;
        // A texel straddling a page boundary (its UV square cuts the texel) must lie in
        // both pages' rectangles: a sub-texel footprint on either side dirties only its
        // own page, and the scissor must redraw the straddling texel either way.
        let end = (((p + 1) * res).div_ceil(n) as u32)
            .max(start + 1)
            .min(res as u32);
        (start, end)
    };
    let (x0, x1) = axis(page_x);
    let (y0, y1) = axis(page_y);
    [x0, y0, x1 - x0, y1 - y0]
}

/// A local light's cached atlas footprint.
#[derive(Clone, Copy, Debug)]
pub struct AtlasLight {
    /// Stable light id.
    pub id: u64,
    /// World position.
    pub center: [f32; 3],
    /// Influence radius (point radius or spot range).
    pub radius: f32,
}

/// Tile invalidation for the local-light atlas.
#[derive(Clone, Debug, Default)]
pub struct AtlasTileCache {
    lights: Vec<(u64, [f32; 3], f32)>,
    /// Caster id → world AABB and pose its tiles were last drawn with.
    casters: KeyMap<u64, (Aabb3, CasterPose)>,
    /// [`Self::update`]'s next caster map and moved boxes, kept between frames so a steady
    /// frame allocates neither (ADR-385).
    seen: KeyMap<u64, (Aabb3, CasterPose)>,
    moved: Vec<Aabb3>,
    /// Bounds of this frame's refreshed dynamic casters (ADR-2028), kept like `moved`.
    dynamic: Vec<Aabb3>,
    /// What changed in the caster list since the last update (ADR-3079).
    scan: CasterScan,
    /// Casters the last update compared or stored ([`Self::casters_visited`]).
    visited: u32,
    /// The last update dirtied every light whole (the first frame with lights, or a light that
    /// moved or changed), not by the boxes in `moved` and `dynamic` ([`Self::face_dirty`]).
    whole: bool,
}

/// A reference drawn with the caster's own box and pose compares as kept against the same
/// caster (the atlas loop's keep test with both sides equal).
fn atlas_settles(aabb: Aabb3, pose_self: f32) -> bool {
    aabb_same(aabb, aabb) && pose_self <= ATLAS_MOVE_TOLERANCE_M
}

impl AtlasTileCache {
    /// An empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the tile indices that must be redrawn.
    ///
    /// Tile `i` corresponds to `lights[i]`. A caster dirties a tile only when its
    /// old or new bounds overlap that light's sphere.
    #[cfg(test)]
    pub fn update(&mut self, lights: &[AtlasLight], casters: &[ShadowCaster]) -> Vec<u32> {
        self.update_refreshing(lights, casters, false).0
    }

    /// [`Self::update`], and with `refresh_dynamic` every [`ShadowCaster::dynamic`] caster
    /// counts as moved, so the tiles its bounds reach are redrawn every frame (ADR-2028).
    /// Returns the dirty tiles and how many of them only a dynamic caster dirtied.
    ///
    /// Like the sun cache, the cost follows what changed (ADR-3079): a caster bit-for-bit equal
    /// to the one at the same position last update is not looked up.
    #[tracing::instrument(skip_all, fields(casters_visited))]
    pub fn update_refreshing(
        &mut self,
        lights: &[AtlasLight],
        casters: &[ShadowCaster],
        refresh_dynamic: bool,
    ) -> (Vec<u32>, u32) {
        self.visited = 0;
        self.whole = false;
        if lights.is_empty() {
            // No tile to dirty. The first frame with lights redraws every tile (the light
            // list changed), so the casters need not be tracked until then.
            self.lights.clear();
            self.casters.clear();
            self.scan.forget();
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
        let incremental = {
            let _span = tracing::info_span!("atlas_tiles_scan").entered();
            self.scan.scan(casters, |c| refresh_dynamic && c.dynamic)
        };
        // First frame with lights (the second test; the light list changed then too): every
        // tile is new.
        if light_moved
            || self.casters.is_empty() && !casters.is_empty() && self.lights.is_empty()
            || self.lights.is_empty() && !lights.is_empty()
        {
            self.lights = lights
                .iter()
                .map(|light| (light.id, light.center, light.radius))
                .collect();
            self.casters = casters.iter().map(|c| (c.id, (c.aabb, c.pose))).collect();
            self.visited = casters.len() as u32;
            let pose_self = &self.scan.pose_self;
            let unstable: Vec<u32> = (0..casters.len())
                .filter(|&i| !atlas_settles(casters[i].aabb, pose_self[i]))
                .map(|i| i as u32)
                .collect();
            self.scan.mark_unstable(true, [unstable.as_slice()]);
            tracing::Span::current().record("casters_visited", self.visited);
            self.whole = true;
            return ((0..lights.len() as u32).collect(), 0);
        }
        let mut moved = std::mem::take(&mut self.moved);
        moved.clear();
        let mut dynamic = std::mem::take(&mut self.dynamic);
        dynamic.clear();
        let mut unstable = Vec::new();
        if incremental {
            // Ids are unique here, so the map holds exactly the ids in the list.
            for &index in &self.scan.visit {
                let index = index as usize;
                let caster = &casters[index];
                self.visited += 1;
                let (reference, settled) = self.decide(
                    caster,
                    self.scan.pose_self[index],
                    refresh_dynamic,
                    &mut moved,
                    &mut dynamic,
                );
                self.casters.insert(caster.id, reference);
                if !settled {
                    unstable.push(index as u32);
                }
            }
            for id in &self.scan.gone {
                if let Some((old, _)) = self.casters.remove(id) {
                    moved.push(old);
                }
            }
        } else {
            let mut seen = std::mem::take(&mut self.seen);
            seen.clear();
            for (index, caster) in casters.iter().enumerate() {
                self.visited += 1;
                let (reference, settled) = self.decide(
                    caster,
                    self.scan.pose_self[index],
                    refresh_dynamic,
                    &mut moved,
                    &mut dynamic,
                );
                seen.insert(caster.id, reference);
                if !settled {
                    unstable.push(index as u32);
                }
            }
            for (id, (old, _)) in &self.casters {
                if !seen.contains_key(id) {
                    moved.push(*old);
                }
            }
            // The old map becomes next frame's scratch (emptied there, room kept).
            self.seen = std::mem::replace(&mut self.casters, seen);
        }
        self.scan.mark_unstable(!incremental, [unstable.as_slice()]);
        self.lights.clear();
        self.lights
            .extend(lights.iter().map(|light| (light.id, light.center, light.radius)));
        if light_moved {
            self.moved = moved;
            self.dynamic = dynamic;
            self.whole = true;
            return ((0..lights.len() as u32).collect(), 0);
        }
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
        self.moved = moved;
        self.dynamic = dynamic;
        tracing::Span::current().record("casters_visited", self.visited);
        (dirty, dynamic_only)
    }

    /// Whether one tile of dirty light `light` (an index into the last [`Self::update_refreshing`]'s
    /// lights) must be redrawn (ADR-7986). A light is several tiles (a point light's six cube
    /// faces) and the update names dirty lights: a face is dirty when the update dirtied every
    /// light whole, or when one of the boxes it moved or refreshed reaches the light's sphere and
    /// `face_sees(box, centre, radius)` (the face's own test). A face no moved box reaches holds
    /// what it held, so it is kept.
    #[must_use]
    pub(crate) fn face_dirty(&self, light: usize, face_sees: impl Fn(Aabb3, [f32; 3], f32) -> bool) -> bool {
        if self.whole {
            return true;
        }
        let Some(&(_, center, radius)) = self.lights.get(light) else {
            return true;
        };
        self.moved
            .iter()
            .chain(&self.dynamic)
            .any(|&aabb| sphere_hits_aabb(center, radius, aabb) && face_sees(aabb, center, radius))
    }

    /// Casters the last [`Self::update_refreshing`] compared or stored (ADR-3079): 0 on a held
    /// scene, every caster when the lights changed. The frame reads it from the update's
    /// `casters_visited` span field.
    #[cfg(test)]
    #[must_use]
    pub fn casters_visited(&self) -> u32 {
        self.visited
    }

    /// One caster against what its tiles were drawn with: pushes the boxes it dirties and
    /// returns the reference to keep and whether it settles.
    fn decide(
        &self,
        caster: &ShadowCaster,
        pose_self: f32,
        refresh_dynamic: bool,
        moved: &mut Vec<Aabb3>,
        dynamic: &mut Vec<Aabb3>,
    ) -> ((Aabb3, CasterPose), bool) {
        let fresh = ((caster.aabb, caster.pose), atlas_settles(caster.aabb, pose_self));
        if refresh_dynamic && caster.dynamic {
            // Moves inside its bounds every frame: its tiles always hold a stale pose.
            if let Some(&(old, _)) = self.casters.get(&caster.id) {
                dynamic.push(old);
            }
            dynamic.push(caster.aabb);
            return fresh;
        }
        // A static caster caches exactly like any other (ADR-5906, which superseded ADR-5704's
        // zero tolerance in the sun cascades; integ-v049 applies it here too, ADR-5993).
        match self.casters.get(&caster.id) {
            Some(&(old, old_pose))
                if aabb_same(old, caster.aabb)
                    && old_pose.displacement_bound(&caster.pose) <= ATLAS_MOVE_TOLERANCE_M =>
            {
                // Within tolerance of what its tiles hold: keep that reference.
                ((old, old_pose), true)
            }
            Some(&(old, _)) => {
                moved.push(old);
                moved.push(caster.aabb);
                fresh
            }
            None => {
                moved.push(caster.aabb);
                fresh
            }
        }
    }
}

/// Caster travel, world metres, below which its atlas tiles are kept (the box compare's
/// tolerance).
const ATLAS_MOVE_TOLERANCE_M: f32 = 1.0e-4;

fn aabb_same(a: Aabb3, b: Aabb3) -> bool {
    a.min
        .iter()
        .chain(a.max.iter())
        .zip(b.min.iter().chain(b.max.iter()))
        .all(|(p, q)| (p - q).abs() < 1.0e-4)
}

fn sphere_hits_aabb(center: [f32; 3], radius: f32, aabb: Aabb3) -> bool {
    let c = aabb.clamp_point(center);
    let d = Vec3::from_array(center) - Vec3::from_array(c);
    d.length_squared() <= radius * radius
}

/// Isolated-pixel flicker between two RGBA8 frames.
///
/// `0` is identical. A rigid shift of a solid shape stays low because neighbours
/// change together. Salt-and-pepper shimmer scores high.
#[must_use]
pub fn temporal_flicker(previous: &[u8], next: &[u8], width: u32, height: u32) -> f32 {
    if width == 0 || height == 0 || previous.len() != next.len() {
        return 1.0;
    }
    // Equal-length readbacks can still be shorter than the image; there is nothing to
    // compare then (the interior-pixel loop would index past the end).
    let bytes = (width as usize)
        .checked_mul(height as usize)
        .and_then(|t| t.checked_mul(4));
    if bytes.is_none_or(|need| previous.len() < need) {
        return 1.0;
    }
    let w = width as i32;
    let h = height as i32;
    let lum = |frame: &[u8], x: i32, y: i32| -> f32 {
        let i = ((y * w + x) * 4) as usize;
        let r = f32::from(frame[i]);
        let g = f32::from(frame[i + 1]);
        let b = f32::from(frame[i + 2]);
        0.2126 * r + 0.7152 * g + 0.0722 * b
    };
    let mut isolated = 0u32;
    let mut counted = 0u32;
    for y in 1..h - 1 {
        for x in 1..w - 1 {
            counted += 1;
            let delta = (lum(next, x, y) - lum(previous, x, y)).abs();
            if delta < 18.0 {
                continue;
            }
            let mut neighbour = 0.0;
            let mut n = 0.0;
            for dy in -1..=1 {
                for dx in -1..=1 {
                    if dx == 0 && dy == 0 {
                        continue;
                    }
                    neighbour += (lum(next, x + dx, y + dy) - lum(previous, x + dx, y + dy)).abs();
                    n += 1.0;
                }
            }
            if neighbour / n < 8.0 {
                isolated += 1;
            }
        }
    }
    if counted == 0 {
        return 0.0;
    }
    isolated as f32 / counted as f32
}

/// Mean isolated-flicker score across consecutive pairs. One frame scores `0`.
#[must_use]
pub fn sequence_flicker(frames: &[&[u8]], width: u32, height: u32) -> f32 {
    if frames.len() < 2 {
        return 0.0;
    }
    let mut sum = 0.0;
    for pair in frames.windows(2) {
        sum += temporal_flicker(pair[0], pair[1], width, height);
    }
    sum / (frames.len() - 1) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uv_vp() -> [[f32; 4]; 4] {
        // ndc = world / 2 on X and Z, so world [-2, 2] covers UV [0, 1].
        // Column-major: clip.x = 0.5 * world.x, clip.y = 0.5 * world.z.
        [
            [0.5, 0.0, 0.0, 0.0],
            [0.0, 0.0, 0.0, 0.0],
            [0.0, 0.5, 0.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ]
    }

    /// The update on the frame thread and on the compute pool decide the same pages, full
    /// redraws and events, frame after frame: casters still, moving a little and far, appearing,
    /// disappearing, and the cascade matrices changing.
    #[test]
    fn the_parallel_update_decides_the_same_as_the_serial_one() {
        let vps = |offset: f32| -> Vec<[[f32; 4]; 4]> {
            (0..3)
                .map(|cascade| {
                    let scale = 0.01 * (cascade + 1) as f32;
                    [
                        [scale, 0.0, 0.0, 0.0],
                        [0.0, scale, 0.0, 0.0],
                        [0.0, 0.0, scale, 0.0],
                        [offset, 0.0, 0.0, 1.0],
                    ]
                })
                .collect()
        };
        let mut serial = SunPageCache::new(3, 16);
        let mut parallel = SunPageCache::new(3, 16);
        parallel.set_parallel(true);
        let count = PARALLEL_MIN_CASTERS as u64 * 2;
        let mut parallel_events = 0;
        for frame in 0u64..12 {
            let casters: Vec<ShadowCaster> = (0..count)
                // Frame 6 on: a block of casters is gone; frame 9 on: others are new.
                .filter(|id| frame < 6 || !(50..90).contains(id))
                .map(|id| {
                    let id = if frame >= 9 && id < 20 { id + 10_000 } else { id };
                    let drift = if id % 7 == 0 { frame as f32 * 3.0 } else { 0.0 };
                    let mut caster = caster_at(id, (id % 97) as f32 - 48.0 + drift);
                    // Casters in different cascades, so the tasks do unequal work.
                    caster.cascades = 1 | (1 << (id % 3)) as u8;
                    caster
                })
                .collect();
            let matrices = vps(if frame == 4 { 0.25 } else { 0.0 });
            let one = serial.update(&casters, &matrices);
            let two = parallel.update(&casters, &matrices);
            assert!(
                parallel.ran_on_tasks(),
                "frame {frame} stayed on the frame thread"
            );
            assert!(!serial.ran_on_tasks());
            assert_eq!(one.dirty, two.dirty, "frame {frame}");
            assert_eq!(one.cascade_full_redraw, two.cascade_full_redraw, "frame {frame}");
            assert_eq!(one.caster_events, two.caster_events, "frame {frame}");
            parallel_events += two.caster_events.iter().flatten().sum::<u32>();
        }
        assert!(
            parallel_events > 0,
            "the sequence moved no caster, so it proves nothing"
        );
    }

    #[test]
    fn parallel_footprints_match_the_serial_projection() {
        let vps: Vec<[[f32; 4]; 4]> = (1..=3)
            .map(|k| {
                let scale = 0.01 * k as f32;
                [
                    [scale, 0.0, 0.0, 0.0],
                    [0.0, scale, 0.0, 0.0],
                    [0.0, 0.0, scale, 0.0],
                    [0.1 * k as f32, 0.0, 0.0, 1.0],
                ]
            })
            .collect();
        // Some casters unbounded, some outside a cascade's mask.
        let casters: Vec<(Option<Aabb3>, u8)> = (0..300u32)
            .map(|i| {
                let aabb = (i % 11 != 0).then(|| caster_at(u64::from(i), i as f32 * 0.25 - 30.0).aabb);
                (aabb, 1u8 << (i % 3))
            })
            .collect();
        let mut lists = vec![Vec::new(); vps.len()];
        project_footprints_parallel(&mut lists, &vps, &casters);
        for (k, (list, vp)) in lists.iter().zip(&vps).enumerate() {
            let expected: Vec<_> = casters
                .iter()
                .map(|&(aabb, mask)| {
                    aabb.filter(|_| mask & (1u8 << k) != 0)
                        .map(|aabb| project_aabb_to_shadow_uv(aabb, *vp))
                })
                .collect();
            assert_eq!(list, &expected, "cascade {k}");
            assert!(list.iter().any(Option::is_some), "cascade {k} projected nothing");
        }
    }

    #[test]
    fn a_small_caster_list_stays_on_the_frame_thread_and_decides_the_same() {
        let matrices = [[
            [0.01, 0.0, 0.0, 0.0],
            [0.0, 0.01, 0.0, 0.0],
            [0.0, 0.0, 0.01, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ]; 2];
        let casters: Vec<ShadowCaster> = (0..8).map(|id| caster_at(id, id as f32)).collect();
        let mut serial = SunPageCache::new(2, 8);
        let mut parallel = SunPageCache::new(2, 8);
        parallel.set_parallel(true);
        let one = serial.update(&casters, &matrices);
        let two = parallel.update(&casters, &matrices);
        assert!(!parallel.ran_on_tasks(), "a small list stays on the frame thread");
        assert_eq!(one.dirty, two.dirty);
    }

    fn caster_at(id: u64, x: f32) -> ShadowCaster {
        turned_caster_at(id, x, 0.0)
    }

    /// A 0.3 m caster at `x`, turned `yaw` radians about its centre: its box does not change.
    fn turned_caster_at(id: u64, x: f32, yaw: f32) -> ShadowCaster {
        let model = regia_core::glam::Mat4::from_translation(Vec3::new(x, 0.0, 0.0))
            * regia_core::glam::Mat4::from_rotation_y(yaw);
        ShadowCaster {
            is_static: false,
            id,
            aabb: Aabb3::from_center_half([x, 0.0, 0.0], [0.15, 0.15, 0.15]),
            cascades: u8::MAX,
            pose: CasterPose {
                model: model.to_cols_array_2d(),
                center: [0.0; 3],
                radius: 0.26,
            },
            dynamic: false,
        }
    }

    fn swaying_at(id: u64, x: f32) -> ShadowCaster {
        ShadowCaster {
            dynamic: true,
            ..caster_at(id, x)
        }
    }

    fn dynamic_update(
        cache: &mut SunPageCache,
        casters: &[ShadowCaster],
        vps: &[[[f32; 4]; 4]],
        mask: u8,
    ) -> PageUpdate {
        cache.update_with_footprints(casters, vps, mask, |cascade, index| {
            project_aabb_to_shadow_uv(casters[index].aabb, vps[cascade])
        })
    }

    /// ADR-2028: a swaying caster holds its bounds and pose, so without the dynamic class its
    /// pages keep the first pose forever; in a refreshed cascade they are redrawn every frame,
    /// and only its pages.
    #[test]
    fn a_dynamic_caster_redraws_its_pages_every_frame_in_refreshed_cascades() {
        let vp = uv_vp();
        let scene = [swaying_at(1, -1.5), caster_at(2, 1.5)];
        let mut cache = SunPageCache::new(2, 4);
        dynamic_update(&mut cache, &scene, &[vp, vp], 0b01);
        for _ in 0..3 {
            let update = dynamic_update(&mut cache, &scene, &[vp, vp], 0b01);
            assert!(!update.dirty.is_empty());
            assert!(
                update.dirty.iter().all(|page| page.cascade == 0 && page.x == 0),
                "{update:?}"
            );
            assert_eq!(update.dynamic_pages, update.dirty.len() as u32);
            assert_eq!(update.caster_events[0], [0, 0, 0], "a refresh is not a move");
        }
        // Without the class (mask 0) the same scene keeps every page once warm.
        let mut cache = SunPageCache::new(2, 4);
        dynamic_update(&mut cache, &scene, &[vp, vp], 0);
        let update = dynamic_update(&mut cache, &scene, &[vp, vp], 0);
        assert!(update.dirty.is_empty(), "{update:?}");
        assert_eq!(update.dynamic_pages, 0);
    }

    /// A dynamic caster that also moves dirties the union of where its pages were drawn and
    /// where it is; dropping the class afterwards keeps its last drawn pages.
    #[test]
    fn a_moving_dynamic_caster_covers_old_and_new_and_settles_when_switched_off() {
        let vp = uv_vp();
        let mut cache = SunPageCache::new(1, 4);
        dynamic_update(&mut cache, &[swaying_at(1, -1.5)], &[vp], 1);
        let moved = dynamic_update(&mut cache, &[swaying_at(1, -0.4)], &[vp], 1);
        assert!(moved.dirty.iter().any(|page| page.x == 0), "{moved:?}");
        assert!(moved.dirty.iter().any(|page| page.x == 1), "{moved:?}");
        let off = dynamic_update(&mut cache, &[swaying_at(1, -0.4)], &[vp], 0);
        assert!(
            off.dirty.is_empty(),
            "the class off: the held pose keeps its pages, {off:?}"
        );
    }

    /// Spec 88 § 2.3: outside the mask a dynamic caster is compared like any other caster. Held,
    /// cascade 1 keeps its pages while cascade 0 refreshes them; moved, it is a move event in
    /// cascade 1 (and not in the refreshed cascade 0) and dirties its old and new pages there.
    #[test]
    fn outside_the_mask_a_dynamic_caster_caches_and_moves_like_any_caster() {
        let vp = uv_vp();
        let mut cache = SunPageCache::new(2, 4);
        dynamic_update(&mut cache, &[swaying_at(1, -1.5)], &[vp, vp], 0b01);
        let held = dynamic_update(&mut cache, &[swaying_at(1, -1.5)], &[vp, vp], 0b01);
        assert!(held.dirty.iter().any(|page| page.cascade == 0), "{held:?}");
        assert!(held.dirty.iter().all(|page| page.cascade == 0), "{held:?}");
        let moved = dynamic_update(&mut cache, &[swaying_at(1, -0.4)], &[vp, vp], 0b01);
        assert_eq!(moved.caster_events[0], [0, 0, 0], "a refresh is not a move");
        assert_eq!(moved.caster_events[1], [1, 0, 0], "outside the mask it moved");
        let far: Vec<u32> = moved
            .dirty
            .iter()
            .filter(|page| page.cascade == 1)
            .map(|page| page.x)
            .collect();
        assert!(far.contains(&0) && far.contains(&1), "{moved:?}");
    }

    /// The dynamic atlas refresh dirties the tiles a swaying caster's box reaches every frame,
    /// counts them, and leaves other tiles alone.
    #[test]
    fn a_dynamic_caster_redraws_the_atlas_tiles_it_reaches() {
        let mut cache = AtlasTileCache::new();
        let near = AtlasLight {
            id: 1,
            center: [0.0, 1.0, 0.0],
            radius: 3.0,
        };
        let far = AtlasLight {
            id: 2,
            center: [20.0, 1.0, 0.0],
            radius: 3.0,
        };
        let casters = [swaying_at(1, 0.0), caster_at(2, 20.0)];
        cache.update_refreshing(&[near, far], &casters, true);
        for _ in 0..3 {
            assert_eq!(
                cache.update_refreshing(&[near, far], &casters, true),
                (vec![0], 1)
            );
        }
        assert_eq!(
            cache.update_refreshing(&[near, far], &casters, false),
            (vec![], 0)
        );
        assert!(cache.update(&[near, far], &casters).is_empty());
    }

    /// A caster turning about its centre keeps its footprint box but must redraw its pages
    /// (lane v013-shadow-patches: DO27's hulls kept the old pose's shadow).
    #[test]
    fn a_caster_turned_in_place_dirties_its_pages() {
        let mut cache = SunPageCache::new(1, 4);
        let vp = uv_vp();
        cache.update(&[caster_at(1, -1.5), caster_at(2, 1.5)], &[vp]);
        let turned = cache.update(&[turned_caster_at(1, -1.5, 0.5), caster_at(2, 1.5)], &[vp]);
        assert!(!turned.dirty.is_empty(), "the turn must dirty pages");
        assert!(
            turned.dirty.iter().all(|page| page.x == 0),
            "only its own pages: {turned:?}"
        );
        let held = cache.update(&[turned_caster_at(1, -1.5, 0.5), caster_at(2, 1.5)], &[vp]);
        assert!(held.dirty.is_empty(), "a held pose keeps every page");
    }

    /// A caster creeping by less than the tolerance per frame must redraw once the creep adds
    /// up: the cache compares with the pose its pages were drawn with, not last frame's.
    #[test]
    fn a_creeping_caster_redraws_once_the_creep_adds_up() {
        let mut cache = SunPageCache::new(1, 4);
        let vp = uv_vp();
        cache.update(&[caster_at(1, 0.0)], &[vp]);
        // 0.25 of the tolerance per frame (uv_vp: 0.25 UV per metre).
        let step = 0.25 * PAGE_MOVE_TOLERANCE_UV / 0.25;
        let mut redrawn = 0;
        for frame in 1..=40 {
            let update = cache.update(&[caster_at(1, step * frame as f32)], &[vp]);
            if !update.dirty.is_empty() {
                redrawn += 1;
                assert!(frame >= 4, "redrawn after {frame} frames, before the tolerance");
            }
        }
        assert!(
            redrawn >= 5,
            "a 10-tolerance creep must redraw, got {redrawn} redraws"
        );
    }

    /// The atlas cache sees a turn in place too.
    #[test]
    fn a_caster_turned_in_place_dirties_its_atlas_tiles() {
        let mut cache = AtlasTileCache::new();
        let light = AtlasLight {
            id: 1,
            center: [0.0, 2.0, 0.0],
            radius: 5.0,
        };
        cache.update(&[light], &[caster_at(1, 0.0)]);
        assert!(cache.update(&[light], &[caster_at(1, 0.0)]).is_empty());
        assert_eq!(cache.update(&[light], &[turned_caster_at(1, 0.0, 0.5)]), vec![0]);
        assert!(
            cache
                .update(&[light], &[turned_caster_at(1, 0.0, 0.5)])
                .is_empty()
        );
    }

    /// `specs/14` A1 in the local-light atlas since ADR-5906 (applied to the atlas by ADR-5993):
    /// [`ShadowCaster::is_static`] changes no cache behavior, so a `Static` caster's tiles absorb a
    /// turn far below `ATLAS_MOVE_TOLERANCE_M` exactly as a plain caster's do (ADR-5704's zero
    /// tolerance only made a marked caster cost more). Both hold their tiles on a bit-identical
    /// frame.
    #[test]
    fn spec14_a1_static_caster_caches_its_atlas_tiles_like_any_other() {
        let lights = [
            AtlasLight {
                id: 1,
                center: [0.0, 2.0, 0.0],
                radius: 3.0,
            },
            AtlasLight {
                id: 2,
                center: [20.0, 2.0, 0.0],
                radius: 3.0,
            },
        ];
        // 1e-7 rad about the centre: the box holds, the pose moves by about 4e-8 m.
        let creep = 1.0e-7;
        let marked = |yaw| ShadowCaster {
            is_static: true,
            ..turned_caster_at(1, 0.0, yaw)
        };
        let plain = |yaw| turned_caster_at(2, 20.0, yaw);
        assert!(
            marked(creep).pose.displacement_bound(&marked(0.0).pose) > 0.0,
            "the creep must change the pose bits"
        );
        assert!(
            plain(creep).pose.displacement_bound(&plain(0.0).pose) <= ATLAS_MOVE_TOLERANCE_M,
            "the creep must stay below the atlas move tolerance"
        );
        let mut cache = AtlasTileCache::new();
        cache.update(&lights, &[marked(0.0), plain(0.0)]);
        assert!(
            cache.update(&lights, &[marked(0.0), plain(0.0)]).is_empty(),
            "a bit-identical held frame redraws no tile"
        );
        assert!(
            cache.update(&lights, &[marked(creep), plain(creep)]).is_empty(),
            "neither light tile redraws on a below-tolerance turn, Static or not"
        );
        assert!(
            cache.update(&lights, &[marked(creep), plain(creep)]).is_empty(),
            "the new pose holds: nothing redraws"
        );
    }

    #[test]
    fn moving_caster_invalidates_only_its_pages() {
        let mut cache = SunPageCache::new(1, 4);
        assert_eq!(cache.pages_per_axis(), 4);
        let vp = uv_vp();
        let first = cache.update(&[caster_at(1, -1.5), caster_at(2, 1.5)], &[vp]);
        assert_eq!(
            first.dirty.len(),
            16,
            "first sight of a cascade redraws every page"
        );
        let still = cache.update(&[caster_at(1, -1.5), caster_at(2, 1.5)], &[vp]);
        assert!(still.dirty.is_empty(), "static casters keep every page");
        // -1.5 → UV x ≈ 0.125 (page 0). Move it to -0.4 → UV x ≈ 0.4 (page 1).
        let moved = cache.update(&[caster_at(1, -0.4), caster_at(2, 1.5)], &[vp]);
        assert!(!moved.dirty.is_empty());
        assert!(
            moved.dirty.iter().all(|page| page.x <= 1),
            "only the moved footprint"
        );
        assert!(
            moved.dirty.iter().all(|page| page.x != 3),
            "the stationary caster's page stays clean, got {moved:?}"
        );
    }

    /// A caster outside a cascade's mask neither dirties nor occupies that cascade's pages.
    #[test]
    fn masked_caster_leaves_other_cascades_clean() {
        let mut cache = SunPageCache::new(2, 4);
        let vp = uv_vp();
        let only_first = |id, x| ShadowCaster {
            cascades: 0b01,
            ..caster_at(id, x)
        };
        cache.update(&[only_first(1, -1.5)], &[vp, vp]);
        let moved = cache.update(&[only_first(1, -0.4)], &[vp, vp]);
        assert!(!moved.dirty.is_empty());
        assert!(moved.dirty.iter().all(|page| page.cascade == 0), "{moved:?}");
    }

    #[test]
    fn removed_caster_dirties_its_old_pages_only() {
        let mut cache = SunPageCache::new(1, 4);
        let vp = uv_vp();
        cache.update(&[caster_at(7, 1.5)], &[vp]);
        let update = cache.update(&[], &[vp]);
        assert!(update.dirty.iter().all(|page| page.x == 3));
        assert!(!update.dirty.is_empty());
    }

    #[test]
    fn changed_light_matrix_redraws_the_cascade() {
        let mut cache = SunPageCache::new(1, 4);
        let vp = uv_vp();
        cache.update(&[caster_at(1, 0.0)], &[vp]);
        let mut shifted = vp;
        shifted[3][0] = 0.25;
        let update = cache.update(&[caster_at(1, 0.0)], &[shifted]);
        assert_eq!(update.dirty.len(), 16);
        assert_eq!(update.cascade_full_redraw, vec![true]);
    }

    #[test]
    fn moving_caster_dirties_only_overlapping_atlas_tiles() {
        let mut cache = AtlasTileCache::new();
        let lights = [
            AtlasLight {
                id: 1,
                center: [0.0, 1.0, 0.0],
                radius: 2.0,
            },
            AtlasLight {
                id: 2,
                center: [20.0, 1.0, 0.0],
                radius: 2.0,
            },
        ];
        let caster = caster_at(3, 0.0);
        let first = cache.update(&lights, &[caster]);
        assert_eq!(first, vec![0, 1], "first frame allocates every tile");
        let quiet = cache.update(&lights, &[caster]);
        assert!(quiet.is_empty());
        let moved = cache.update(&lights, &[caster_at(3, 0.4)]);
        assert_eq!(moved, vec![0], "the far light does not see this caster");
    }

    #[test]
    fn flicker_metric_fails_on_shimmer_and_passes_a_shift() {
        let w = 32u32;
        let h = 32u32;
        let mut stable = vec![20u8; (w * h * 4) as usize];
        for y in 8..24 {
            for x in 8..24 {
                let i = ((y * w + x) * 4) as usize;
                stable[i] = 220;
                stable[i + 1] = 220;
                stable[i + 2] = 220;
                stable[i + 3] = 255;
            }
        }
        assert!(temporal_flicker(&stable, &stable, w, h) < 1.0e-5);
        let mut shifted = vec![20u8; stable.len()];
        for y in 0..h {
            for x in 0..w {
                let src_x = x.saturating_sub(2);
                let si = ((y * w + src_x) * 4) as usize;
                let di = ((y * w + x) * 4) as usize;
                shifted[di..di + 4].copy_from_slice(&stable[si..si + 4]);
            }
        }
        let shift_score = temporal_flicker(&stable, &shifted, w, h);
        // Salt pixels are four texels apart so each flicker is isolated from its neighbours.
        let mut shimmer = stable.clone();
        for y in (2..h).step_by(4) {
            for x in (2..w).step_by(4) {
                let i = ((y * w + x) * 4) as usize;
                shimmer[i] = 255 - shimmer[i];
                shimmer[i + 1] = 255 - shimmer[i + 1];
                shimmer[i + 2] = 255 - shimmer[i + 2];
            }
        }
        let shimmer_score = temporal_flicker(&stable, &shimmer, w, h);
        assert!(
            shimmer_score > shift_score * 4.0 && shimmer_score > 0.05,
            "shimmer {shimmer_score} should dwarf the rigid shift {shift_score}"
        );
    }

    /// The first steady frame may size the page-id lists. The next frame with the same dynamic
    /// casters and light matrices must report the same dirty pages and must not grow those lists.
    #[test]
    fn dynamic_page_lists_are_reused() {
        let vp = uv_vp();
        let scene = [
            swaying_at(1, -1.5),
            swaying_at(2, -0.5),
            swaying_at(3, 0.5),
            swaying_at(4, 1.5),
        ];
        let mut cache = SunPageCache::new(2, 8);
        let matrices = [vp, vp];
        let warm = dynamic_update(&mut cache, &scene, &matrices, 0b11);
        assert!(warm.cascade_full_redraw.iter().all(|full| *full));
        let before_steady = page_list_grows();
        let first = dynamic_update(&mut cache, &scene, &matrices, 0b11);
        assert!(
            !first.dirty.is_empty(),
            "dynamic casters must dirty pages, {first:?}"
        );
        assert!(first.dynamic_pages > 0, "{first:?}");
        let after_first = page_list_grows();
        assert!(
            after_first > before_steady,
            "the first steady frame should size the page lists ({before_steady} -> {after_first})"
        );
        let second = dynamic_update(&mut cache, &scene, &matrices, 0b11);
        assert_eq!(second.dirty, first.dirty, "steady frames disagree");
        assert_eq!(second.dynamic_pages, first.dynamic_pages);
        let grew = page_list_grows() - after_first;
        assert_eq!(
            grew, 0,
            "page-list buffers grew {grew} times on an unchanged steady frame"
        );
    }

    /// The texel rectangle of a page is the texels of its UV square
    /// `[page / n, (page + 1) / n)`, even when `resolution` is not divisible by `n`:
    /// the scissor must redraw exactly what `pages_covering` dirtied.
    #[test]
    fn page_texel_rect_matches_uv_pages_when_resolution_is_not_divisible() {
        let n = 8;
        for res in [20u32, 100, 512] {
            for t in 0..res {
                let page = (t as f32 / res as f32 * n as f32).floor() as u32;
                let [x, y, w, h] = page_texel_rect(page, page, n, res);
                assert!(
                    t >= x && t < x + w && t >= y && t < y + h,
                    "texel {t} of {res} must lie in page {page}'s rectangle [{x}, {})",
                    x + w
                );
            }
        }
        // The concrete stale-texel case: UV page 6 of 8 at resolution 20 is texels [15, 18)
        // (the page's UV square cuts texel 17, which the scissor must redraw).
        let [x, _, w, _] = page_texel_rect(6, 0, 8, 20);
        assert_eq!((x, w), (15, 3));
    }

    /// A zero-area footprint exactly on an interior page boundary used to dirty no page
    /// (`floor` and `ceil - 1` crossed), leaving that page stale until something else
    /// marked it. `uv_vp` maps world X ∈ [-2, 2] to U = x/4 + 1/2, so a caster with no
    /// X extent at x = -1.5 lands exactly on U = 0.125, the page 0/1 boundary.
    #[test]
    fn a_zero_area_footprint_on_an_interior_boundary_dirties_its_page() {
        let on_edge = Aabb3 {
            min: [-1.5, 0.0, -0.1],
            max: [-1.5, 1.0, 0.1],
        };
        let (min, max) = project_aabb_to_shadow_uv(on_edge, uv_vp());
        assert_eq!(min[0], max[0], "no X extent: a zero-area U coordinate");
        let pages = pages_covering(Aabb2 { min, max }, 8);
        assert!(
            pages.iter().any(|&(x, _)| x == 1),
            "the footprint on the page 0/1 boundary must dirty page column 1: {pages:?}"
        );
        // Off the boundary the same footprint still dirties exactly one page (z ∈
        // [0.1, 0.3] keeps V inside row 3).
        let off_edge = Aabb3 {
            min: [-1.4, 0.0, 0.1],
            max: [-1.4, 1.0, 0.3],
        };
        let (min, max) = project_aabb_to_shadow_uv(off_edge, uv_vp());
        let pages = pages_covering(Aabb2 { min, max }, 8);
        assert_eq!(pages, [(1, 3)], "one page away from a boundary");
    }

    /// Equal-length buffers shorter than the image used to index past the end.
    #[test]
    fn temporal_flicker_returns_when_buffers_are_shorter_than_the_image() {
        assert_eq!(temporal_flicker(&[0u8; 16], &[0u8; 16], 4, 4), 1.0);
        assert_eq!(sequence_flicker(&[&[0u8; 16][..], &[0u8; 16]], 4, 4), 1.0);
    }
}

#[cfg(test)]
mod incremental_tests;
