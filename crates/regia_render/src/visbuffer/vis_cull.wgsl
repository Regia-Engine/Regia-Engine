// SPDX-License-Identifier: MPL-2.0
// Visibility-buffer instance and cluster culling (`specs/27` § 4.2 stage 3).
//
// Instances are frustum-culled and expanded into 64-cluster work items. Every cluster of a
// visible instance is then tested in two phases (Haar & Aaltonen, "GPU-Driven Rendering
// Pipelines", SIGGRAPH 2015): phase 1 draws the selected clusters that were visible last
// frame; the Hi-Z pyramid (Greene, Kass & Miller, SIGGRAPH 1993) is built from that depth;
// phase 2 tests every selected cluster against it, draws the newly visible ones and records
// visibility for the next frame. LOD: a cluster is selected when its own simplification error
// projects to at most `threshold` pixels and its parent's to more (the cut of a batched
// multi-triangulation, Cignoni et al. 2005). Siblings share both spheres, so a group switches
// as a whole and the cut has no cracks. Back-facing clusters go by their normal cone
// (meshoptimizer's `meshopt_computeClusterBounds` test, sphere form), except those of a
// double-sided instance (glTF `doubleSided`), whose back faces are drawn. Each phase's list
// holds the back-face-culled clusters from its start and the double-sided ones from its end,
// so the raster draws each with its own cull mode (`vis_raster.wgsl`) without a second list.
//
// List order is deterministic (ADR-2294): no slot comes from an atomic counter. The work items
// (one per 64 clusters of a visible instance) and each phase's lists are laid out by exclusive
// prefix sums (Blelloch, "Prefix Sums and Their Applications", 1990): a pass writes one count per
// element, `scan_local` scans blocks of 256 in workgroup memory (Hillis & Steele, 1986),
// `scan_top_*` scans the block sums in one workgroup (and prepares the dispatch or the lists'
// counts in the same dispatch), and a placement pass writes every element at
// its block's offset plus its rank inside the block. A cluster's rank among its work item's 64
// lanes is a population count over the item's 2-bit class words, so each list is in (instance,
// cluster) order on every run and every adapter, and the raster's depth ties resolve the same.
//
// Masked (alpha-tested) instances (ADR-3915) take the instance scan's second channel, so their
// work items follow every opaque instance's, in instance order: each list then holds its opaque
// clusters first and the masked ones after them (the double-sided list likewise from its end), and
// the raster draws the masked part with the alpha-tested pipeline (`vis_raster_masked.wgsl`). A
// masked cluster never takes the software range. An instance's work items cover only the cluster
// range of the levels its distance can select (`instance_range`, the per-mesh level table).
//
// LOD hysteresis (ADR-4631): while the projected error stays within the band in `params.lod.x`
// of the threshold, each stored boundary comparison is kept. The first frame and a
// cleared history use the hard cut. Luebke, IEEE CG&A 21(3), 2001, on popping at a view-dependent cut.

struct CullParams {
    view_proj: mat4x4<f32>,
    planes: array<vec4<f32>, 6>,
    // xyz = eye, w = LOD threshold in pixels.
    eye: vec4<f32>,
    // x = width, y = height, z = Hi-Z mip count (0 = no pyramid), w = pixels per unit at unit distance.
    screen: vec4<f32>,
    // xyz = camera forward, w = near distance.
    forward: vec4<f32>,
    // x = instance count, y = cluster capacity per phase, z = 1 to cone-cull back faces,
    // w = largest cluster diameter for the software rasterizer in pixels (0 = off).
    counts: vec4<u32>,
    // The regions of `scan` (in vec4 elements): the class words are [0, x), the per-element
    // prefixes [x, y), the block sums from y. z = the entry of the masked lists' header in
    // `clusters` (two entries, one per phase: where the masked clusters start in each list),
    // w = 1 while some sway moves (ADR-3916).
    scan: vec4<u32>,
    // x = hysteresis band in pixels (f32 bits), y bit 0 = clear the stored LOD this frame,
    // z = instance slots in `lod_words` (not this frame's instance count), w unused (ADR-4631).
    lod: vec4<u32>,
}

struct Instance {
    model: mat4x4<f32>,
    // World bounding sphere.
    sphere: vec4<f32>,
    // x = first meshlet, y = meshlet count, z = first visibility bit, w = material slot.
    info: vec4<u32>,
    // x = a stretch upper bound (`gpu.rs` `max_stretch`), y = sway reach (metres, ADR-3916).
    scale: vec4<f32>,
    // x = shadow scale, y = shadow lift (world Y), z = shadow reach.
    shadow: vec4<f32>,
    // x = shadow flags (`vis_shadow.wgsl`), y bit 0 = double-sided, bit 1 = masked; z = the
    // mesh's first record in `levels`, w = its record count (0: every cluster is a candidate). y from
    // bit 8 = the sway record + 1 (the rasters' `sway_table`).
    flags: vec4<u32>,
}

// One run of clusters of one LOD level in a mesh's cluster list (ADR-3915): the bounds the
// instance cull uses to skip the run when no cluster in it can pass the LOD test at the
// instance's distance (`instance_range`). Object space.
struct LevelRun {
    // Cluster range [first, end) within the mesh.
    first: u32,
    end: u32,
    // The run's smallest `lod_error`, and the farthest any of its `lod` sphere centers sits from
    // the mesh's bounding-sphere center.
    min_error: f32,
    reach: f32,
    // The run's largest `parent_error` (ROOT when a cluster is a root), and the farthest any parent
    // sphere reaches from the mesh's center (center offset plus radius).
    max_parent_error: f32,
    parent_reach: f32,
    pad0: u32,
    pad1: u32,
}

struct Meshlet {
    bounds: vec4<f32>,
    cone: vec4<f32>,
    lod: vec4<f32>,
    parent: vec4<f32>,
    lod_error: f32,
    parent_error: f32,
    vertex_offset: u32,
    triangle_offset: u32,
    vertex_count: u32,
    triangle_count: u32,
    level: u32,
    pad: u32,
}

// `state` layout (u32): [0..4) phase-1 DrawIndirect, [4..8) phase-2 DrawIndirect,
// [8..11) cluster-cull dispatch, [11] work items, [12] clusters dropped at capacity,
// [16..20) phase-1 and [20..24) phase-2 DrawIndirect of the double-sided clusters,
// [24 + phase] clusters accepted or dropped in that phase (the capacity test),
// [26..29) software-raster dispatch, [29] software clusters listed, [30..32) the list range of
// the round `finalize` last prepared (`vis_software.wgsl`), [32..35) the dispatch of
// `scan_local` over the work items, [35] the element count of the running scan, [36..40) its
// totals, [40] the phase's first software slot, [41] the software slots left for it, [42 + phase]
// the single-sided clusters that phase kept in the software range, [44] the first masked work
// item (the opaque instances' item count), [45 + phase] the clusters that phase's back-face-culled
// list keeps (opaque and masked), [47 + phase] the same for its double-sided list, [52..68) the
// masked clusters' DrawIndirects: phase 1 back-face-culled, phase 1 double-sided, then phase 2's.
const VERTICES_PER_CLUSTER: u32 = 372u;
const WORK: u32 = 11u;
const DROPPED: u32 = 12u;
const DS_ARGS: u32 = 16u;
const TOTAL: u32 = 24u;
const SOFT_ARGS: u32 = 26u;
const SOFT_COUNT: u32 = 29u;
const SOFT_BASE: u32 = 30u;
const SOFT_END: u32 = 31u;
const SCAN_ARGS: u32 = 32u;
const SCAN_N: u32 = 35u;
const SCAN_TOTAL: u32 = 36u;
const SOFT_FROM: u32 = 40u;
const SOFT_ROOM: u32 = 41u;
// Two words, one per phase: the single-sided clusters that phase kept in the software range. They
// come first in its part of the range, the double-sided ones after (`finalize_phase`).
const SOFT_SINGLE_KEPT: u32 = 42u;
const MASK_ITEM: u32 = 44u;
const KEPT_SINGLE: u32 = 45u;
const KEPT_DOUBLE: u32 = 47u;
const MASK_ARGS: u32 = 52u;
// Elements per `scan_local` workgroup, and block sums per round of `scan_top`.
const SCAN_BLOCK: u32 = 256u;
// A lane's 2-bit class in its work item's class words: not drawn, the back-face-culled list,
// the double-sided list, the software range.
const CLASS_SINGLE: u32 = 1u;
const CLASS_DOUBLE: u32 = 2u;
const CLASS_SOFT: u32 = 3u;
// Software clusters per workgroup of `vis_software.wgsl`.
const CLUSTERS_PER_GROUP: u32 = 16u;
// `Instance.flags.y`: back faces drawn (no cone cull, the cull-none raster).
const DOUBLE_SIDED: u32 = 1u;
// `Instance.flags.y`: an alpha-tested material (ADR-3915).
const MASKED: u32 = 2u;
const ROOT: f32 = 3.0e38;
// Relative margin of the level-run bounds over the per-cluster test's own rounding.
const LEVEL_MARGIN: f32 = 1.0e-3;

@group(0) @binding(0) var<uniform> params: CullParams;
@group(0) @binding(1) var<storage, read> instances: array<Instance>;
@group(0) @binding(2) var<storage, read> meshlets: array<Meshlet>;
@group(0) @binding(3) var<storage, read_write> work: array<vec2<u32>>;
@group(0) @binding(4) var<storage, read_write> state: array<atomic<u32>>;
@group(0) @binding(5) var<storage, read_write> clusters: array<vec2<u32>>;
@group(0) @binding(6) var<storage, read_write> bits: array<atomic<u32>>;
@group(0) @binding(7) var hiz: texture_2d<f32>;
// Class words of every work item, then the scan's per-element values, then its block sums
// (`CullParams.scan`).
@group(0) @binding(8) var<storage, read_write> scan: array<vec4<u32>>;
// Every resident mesh's level runs (ADR-3915).
@group(0) @binding(9) var<storage, read> levels: array<LevelRun>;
// [0, cap) per-instance change flags, word `cap` the switch count.
// From `cap + 1`, one boundary word per visibility bit: own-above, parent-above, valid, selected.
// Keeping both comparisons preserves mixed-level cuts (ADR-4631).
@group(0) @binding(10) var<storage, read_write> lod_words: array<atomic<u32>>;

var<workgroup> scan_shared: array<vec4<u32>, 256>;
var<workgroup> scan_blocks: u32;
var<workgroup> item_classes: array<atomic<u32>, 4>;

@compute @workgroup_size(1)
fn reset() {
    for (var phase = 0u; phase < 2u; phase = phase + 1u) {
        atomicStore(&state[phase * 4u], VERTICES_PER_CLUSTER);
        atomicStore(&state[phase * 4u + 1u], 0u);
        atomicStore(&state[phase * 4u + 2u], 0u);
        atomicStore(&state[phase * 4u + 3u], 0u);
        atomicStore(&state[DS_ARGS + phase * 4u], VERTICES_PER_CLUSTER);
        atomicStore(&state[DS_ARGS + phase * 4u + 1u], 0u);
        atomicStore(&state[DS_ARGS + phase * 4u + 2u], 0u);
        atomicStore(&state[DS_ARGS + phase * 4u + 3u], 0u);
        atomicStore(&state[TOTAL + phase], 0u);
        atomicStore(&state[SOFT_SINGLE_KEPT + phase], 0u);
        atomicStore(&state[KEPT_SINGLE + phase], 0u);
        atomicStore(&state[KEPT_DOUBLE + phase], 0u);
        for (var list = 0u; list < 2u; list = list + 1u) {
            let at = MASK_ARGS + phase * 8u + list * 4u;
            atomicStore(&state[at], VERTICES_PER_CLUSTER);
            atomicStore(&state[at + 1u], 0u);
            atomicStore(&state[at + 2u], 0u);
            atomicStore(&state[at + 3u], 0u);
        }
    }
    atomicStore(&state[MASK_ITEM], 0u);
    atomicStore(&state[8u], 0u);
    atomicStore(&state[9u], 1u);
    atomicStore(&state[10u], 1u);
    atomicStore(&state[WORK], 0u);
    atomicStore(&state[DROPPED], 0u);
    atomicStore(&state[SOFT_COUNT], 0u);
    atomicStore(&state[SOFT_END], 0u);
    // The first scan runs over the instances.
    atomicStore(&state[SCAN_N], params.counts.x);
}

fn frustum_inside(center: vec3<f32>, radius: f32) -> bool {
    for (var i: u32 = 0u; i < 6u; i = i + 1u) {
        let plane = params.planes[i];
        if (dot(plane.xyz, center) + plane.w < -radius) {
            return false;
        }
    }
    return true;
}

// The hysteresis band in pixels (f32 bits of `params.lod.x`). Zero is the hard cut.
// Luebke, IEEE CG&A 21(3), 2001: a view-dependent cut pops when the metric sits on the switch.
fn lod_band() -> f32 {
    return bitcast<f32>(params.lod.x);
}

// The cluster range [x, y) of instance `inst` that can pass the LOD test this frame (ADR-3915):
// the union of its mesh's level runs that are not ruled out at its distance. A run is out when even
// its smallest error, at the farthest its `lod` spheres can be, projects above the threshold (no
// cluster of it is fine enough), or when its largest parent error, at the nearest its parent
// spheres can be, projects at or below it (every cluster's parent is fine enough). Both bounds
// hold for every cluster of the run (`lod_selected` takes the distance to a sphere's center minus
// its radius, both within these reaches of the instance's center), with a margin over rounding, so
// the clusters the range drops would fail `lod_selected` anyway: the frame does not change.
// With boundary history and a positive band both comparisons move out by the band, so a held
// cluster remains a candidate. A cleared history or a zero band uses the original range.
fn instance_range(inst: Instance, index: u32) -> vec2<u32> {
    let runs = inst.flags.w;
    if (runs == 0u) {
        return vec2<u32>(0u, inst.info.y);
    }
    let d = distance(inst.sphere.xyz, params.eye.xyz);
    let s = inst.scale.x;
    let k = params.screen.w;
    let near = params.forward.w;
    let threshold = params.eye.w;
    var hi_t = threshold;
    var lo_t = threshold;
    let band = lod_band();
    if (band > 0.0 && (params.lod.y & 1u) == 0u) {
        hi_t = threshold + band;
        lo_t = max(threshold - band, 0.0);
    }
    var lo = inst.info.y;
    var hi = 0u;
    for (var i = 0u; i < runs; i = i + 1u) {
        let run = levels[inst.flags.z + i];
        let finest = run.min_error * s * k / max(d + run.reach * s, near);
        if (finest > hi_t * (1.0 + LEVEL_MARGIN)) {
            continue;
        }
        if (run.max_parent_error < ROOT) {
            let coarsest = run.max_parent_error * s * k / max(d - run.parent_reach * s, near);
            if (coarsest * (1.0 + LEVEL_MARGIN) < lo_t) {
                continue;
            }
        }
        lo = min(lo, run.first);
        hi = max(hi, min(run.end, inst.info.y));
    }
    if (hi <= lo) {
        return vec2<u32>(0u, 0u);
    }
    return vec2<u32>(lo, hi);
}

// Work items of instance `index`: one per 64 clusters of its range when its sphere is in the
// frustum, and the range's first cluster.
fn instance_chunks(index: u32) -> vec2<u32> {
    let inst = instances[index];
    if (!frustum_inside(inst.sphere.xyz, inst.sphere.w + sway_reach(inst))) {
        return vec2<u32>(0u, 0u);
    }
    let range = instance_range(inst, index);
    return vec2<u32>((range.y - range.x + 63u) / 64u, range.x);
}

// Writes every instance's work-item count for the scan (zero for a culled one): opaque instances
// in the first channel, masked ones in the second, so the masked work items follow the opaque ones.
@compute @workgroup_size(64)
fn cull_instances(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>) {
    let index = gid.x + gid.y * nwg.x * 64u;
    if (index >= params.counts.x) {
        return;
    }
    let chunks = instance_chunks(index).x;
    if ((instances[index].flags.y & MASKED) != 0u) {
        scan[params.scan.x + index] = vec4<u32>(0u, chunks, 0u, 0u);
    } else {
        scan[params.scan.x + index] = vec4<u32>(chunks, 0u, 0u, 0u);
    }
}

// The exclusive prefix of element `i` of the last scan: its block's offset plus its rank there.
fn scan_offset(i: u32) -> vec4<u32> {
    return scan[params.scan.x + i] + scan[params.scan.y + i / SCAN_BLOCK];
}

// Writes each visible instance's work items at its scanned offset, in instance order: the opaque
// instances' first, the masked ones' after them.
@compute @workgroup_size(64)
fn place_instances(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>) {
    let index = gid.x + gid.y * nwg.x * 64u;
    if (index >= params.counts.x) {
        return;
    }
    let chunks = instance_chunks(index);
    let at = scan_offset(index);
    var base = at.x;
    if ((instances[index].flags.y & MASKED) != 0u) {
        base = atomicLoad(&state[MASK_ITEM]) + at.y;
    }
    for (var c = 0u; c < chunks.x; c = c + 1u) {
        work[base + c] = vec2<u32>(index, chunks.y + c * 64u);
    }
}

// Inclusive sum of `value` over lanes 0..=li of the workgroup (Hillis & Steele); every lane of
// the workgroup must call it. Leaves the inclusive sums in `scan_shared`.
fn workgroup_inclusive_scan(li: u32, value: vec4<u32>) -> vec4<u32> {
    scan_shared[li] = value;
    workgroupBarrier();
    var sum = value;
    for (var stride = 1u; stride < SCAN_BLOCK; stride = stride * 2u) {
        if (li >= stride) {
            sum = sum + scan_shared[li - stride];
        }
        workgroupBarrier();
        scan_shared[li] = sum;
        workgroupBarrier();
    }
    return sum;
}

// First level: scans `state[SCAN_N]` elements in place, block by block, into exclusive
// in-block prefixes, and writes each block's sum.
@compute @workgroup_size(256)
fn scan_local(
    @builtin(workgroup_id) wg: vec3<u32>,
    @builtin(num_workgroups) nwg: vec3<u32>,
    @builtin(local_invocation_index) li: u32,
) {
    let block = wg.x + wg.y * nwg.x;
    let i = block * SCAN_BLOCK + li;
    let n = atomicLoad(&state[SCAN_N]);
    var value = vec4<u32>(0u);
    if (i < n) {
        value = scan[params.scan.x + i];
    }
    let inclusive = workgroup_inclusive_scan(li, value);
    if (i < n) {
        scan[params.scan.x + i] = inclusive - value;
    }
    if (li == SCAN_BLOCK - 1u && block * SCAN_BLOCK < n) {
        scan[params.scan.y + block] = inclusive;
    }
}

// Second level, one workgroup: the block sums become exclusive block offsets, 256 per round with
// a running carry, and the totals go to `state[SCAN_TOTAL..]`. Any count is correct; the cost
// is ceil(n / 65536) rounds of eight barrier steps: 64 rounds at the instance cap
// (`MAX_INSTANCES`, 2^22, which the host asserts) and one round for the work items of up to
// 4 million candidate clusters. Every lane of the workgroup must call it; lane 0 then sees the
// totals it stored.
fn scan_block_sums(li: u32) {
    if (li == 0u) {
        scan_blocks = (atomicLoad(&state[SCAN_N]) + SCAN_BLOCK - 1u) / SCAN_BLOCK;
    }
    let blocks = workgroupUniformLoad(&scan_blocks);
    var carry = vec4<u32>(0u);
    for (var start = 0u; start < blocks; start = start + SCAN_BLOCK) {
        let b = start + li;
        var value = vec4<u32>(0u);
        if (b < blocks) {
            value = scan[params.scan.y + b];
        }
        let inclusive = workgroup_inclusive_scan(li, value);
        if (b < blocks) {
            scan[params.scan.y + b] = carry + inclusive - value;
        }
        carry = carry + scan_shared[SCAN_BLOCK - 1u];
        workgroupBarrier();
    }
    if (li == 0u) {
        atomicStore(&state[SCAN_TOTAL], carry.x);
        atomicStore(&state[SCAN_TOTAL + 1u], carry.y);
        atomicStore(&state[SCAN_TOTAL + 2u], carry.z);
        atomicStore(&state[SCAN_TOTAL + 3u], carry.w);
    }
}

// The instance scan's second level, then (lane 0) the work-item count, one cull workgroup per
// work item (`dispatch_workgroups_indirect` caps each dimension at 65535) and the dispatch of
// the work items' scan.
@compute @workgroup_size(256)
fn scan_top_instances(@builtin(local_invocation_index) li: u32) {
    scan_block_sums(li);
    if (li == 0u) {
        prepare_dispatch();
    }
}

// A phase's second level, then (lane 0) what each list keeps.
@compute @workgroup_size(256)
fn scan_top_phase1(@builtin(local_invocation_index) li: u32) {
    scan_block_sums(li);
    // `finalize_phase` reads the block offsets other lanes just wrote.
    storageBarrier();
    if (li == 0u) {
        finalize_phase(0u);
    }
}

@compute @workgroup_size(256)
fn scan_top_phase2(@builtin(local_invocation_index) li: u32) {
    scan_block_sums(li);
    storageBarrier();
    if (li == 0u) {
        finalize_phase(1u);
    }
}

fn prepare_dispatch() {
    let opaque = atomicLoad(&state[SCAN_TOTAL]);
    let n = opaque + atomicLoad(&state[SCAN_TOTAL + 1u]);
    atomicStore(&state[MASK_ITEM], opaque);
    atomicStore(&state[WORK], n);
    atomicStore(&state[8u], min(n, 65535u));
    atomicStore(&state[9u], max((n + 65534u) / 65535u, 1u));
    atomicStore(&state[10u], 1u);
    let blocks = (n + SCAN_BLOCK - 1u) / SCAN_BLOCK;
    atomicStore(&state[SCAN_N], n);
    atomicStore(&state[SCAN_ARGS], min(blocks, 65535u));
    atomicStore(&state[SCAN_ARGS + 1u], max((blocks + 65534u) / 65535u, 1u));
    atomicStore(&state[SCAN_ARGS + 2u], 1u);
}

// After a phase's scan: how many clusters each list keeps. The software range takes the phase's
// software clusters up to its room, the single-sided ones first, each side in list order; the
// ones past it draw in hardware with their instance's cull mode (ADR-2689): a double-sided cluster
// keeps its back faces in the double-sided list. The two hardware lists share `cap`: the
// back-face-culled list keeps first and the double-sided list the room left; the rest are counted
// as dropped.
fn finalize_phase(phase: u32) {
    let cap = params.counts.y;
    let one_sided = atomicLoad(&state[SCAN_TOTAL]);
    let two_sided = atomicLoad(&state[SCAN_TOTAL + 1u]);
    let soft = atomicLoad(&state[SCAN_TOTAL + 2u]);
    let soft_double = atomicLoad(&state[SCAN_TOTAL + 3u]);
    let soft_single = soft - soft_double;
    let soft_from = atomicLoad(&state[SOFT_COUNT]);
    let room = software_capacity() - min(soft_from, software_capacity());
    let soft_kept = min(soft, room);
    let soft_single_kept = min(soft_single, room);
    let soft_double_kept = soft_kept - soft_single_kept;
    let hardware_single = one_sided + soft_single - soft_single_kept;
    let hardware_double = two_sided + soft_double - soft_double_kept;
    let total = hardware_single + hardware_double;
    let single_kept = min(hardware_single, cap);
    let double_kept = min(hardware_double, cap - single_kept);
    // The masked work items follow the opaque ones and have no software lanes, so the clusters of
    // each list before the first masked item, plus the software clusters past the room (all from
    // opaque items), are the list's opaque part (ADR-3915).
    let masked_item = atomicLoad(&state[MASK_ITEM]);
    var before = vec4<u32>(one_sided, two_sided, 0u, 0u);
    if (masked_item < atomicLoad(&state[WORK])) {
        before = scan_offset(masked_item);
    }
    let opaque_single = min(before.x + soft_single - soft_single_kept, single_kept);
    let opaque_double = min(before.y + soft_double - soft_double_kept, double_kept);
    atomicStore(&state[KEPT_SINGLE + phase], single_kept);
    atomicStore(&state[KEPT_DOUBLE + phase], double_kept);
    atomicStore(&state[phase * 4u + 1u], opaque_single);
    atomicStore(&state[DS_ARGS + phase * 4u + 1u], opaque_double);
    atomicStore(&state[MASK_ARGS + phase * 8u + 1u], single_kept - opaque_single);
    atomicStore(&state[MASK_ARGS + phase * 8u + 5u], double_kept - opaque_double);
    clusters[params.scan.z + phase] = vec2<u32>(opaque_single, opaque_double);
    atomicStore(&state[SOFT_SINGLE_KEPT + phase], soft_single_kept);
    atomicStore(&state[TOTAL + phase], min(total, cap));
    if (total > cap) {
        atomicAdd(&state[DROPPED], total - cap);
    }
    atomicStore(&state[SOFT_FROM], soft_from);
    atomicStore(&state[SOFT_ROOM], room);
    // The next software round: what this phase added to the list.
    let listed = soft_from + soft_kept;
    atomicStore(&state[SOFT_COUNT], listed);
    let base = atomicLoad(&state[SOFT_END]);
    atomicStore(&state[SOFT_BASE], base);
    atomicStore(&state[SOFT_END], listed);
    let groups = (listed - base + CLUSTERS_PER_GROUP - 1u) / CLUSTERS_PER_GROUP;
    atomicStore(&state[SOFT_ARGS], min(groups, 65535u));
    atomicStore(&state[SOFT_ARGS + 1u], max((groups + 65534u) / 65535u, 1u));
    atomicStore(&state[SOFT_ARGS + 2u], 1u);
}

// Clusters the software range holds: `capacity + k` must fit the ID's 24-bit slot.
fn software_capacity() -> u32 {
    let cap = params.counts.y;
    return min(cap, 0xffffffu - min(cap, 0xffffffu));
}

// A cluster small on screen rasterizes in compute: it is entirely in front of the near plane
// (nothing to clip), and its bounding sphere spans at most `counts.w` pixels at its nearest depth.
// A double-sided instance's clusters go too: `vis_software.wgsl` keeps their back faces
// (ADR-2689). Off the view axis a sphere's image stretches along the radius by 1 + tan²θ (θ the
// angle off the axis), so the span counts that too: `vis_software.wgsl` relies on the bound.
fn takes_software(inst: Instance, s: vec4<f32>) -> bool {
    if (params.counts.w == 0u) {
        return false;
    }
    let clip = params.view_proj * vec4<f32>(s.xyz, 1.0);
    let nearest = clip.w - s.w;
    if (nearest <= params.forward.w) {
        return false;
    }
    // `nearest > 0` above, so `clip.w > 0`: pixels from the screen's centre, over the focal length.
    let off_axis = 0.5 * params.screen.xy * clip.xy / (clip.w * params.screen.w);
    let stretch = 1.0 + dot(off_axis, off_axis);
    return 2.0 * s.w * params.screen.w * stretch / nearest <= f32(params.counts.w);
}

// The class of a selected cluster: the software range when it takes the software rasterizer
// (either cull mode: the rasterizer reads it from the instance), else the hardware list of its
// cull mode.
fn class_of(double_sided: bool, software: bool) -> u32 {
    if (software) {
        return CLASS_SOFT;
    }
    if (double_sided) {
        return CLASS_DOUBLE;
    }
    return CLASS_SINGLE;
}

// Lanes of each class in one class word (16 lanes): x back-face-culled, y double-sided,
// z software. The scan's w (`record_classes`) is the software lanes of double-sided items.
fn class_counts(word: u32) -> vec4<u32> {
    let lo = word & 0x55555555u;
    let hi = (word >> 1u) & 0x55555555u;
    return vec4<u32>(countOneBits(lo & ~hi), countOneBits(hi & ~lo), countOneBits(lo & hi), 0u);
}

// Lanes of each class among lanes 0..li of a work item.
fn classes_below(words: vec4<u32>, li: u32) -> vec4<u32> {
    var n = vec4<u32>(0u);
    for (var k = 0u; k < 4u; k = k + 1u) {
        let lane0 = k * 16u;
        if (lane0 >= li) {
            break;
        }
        var word = words[k];
        if (li < lane0 + 16u) {
            word = word & ((1u << (2u * (li - lane0))) - 1u);
        }
        n = n + class_counts(word);
    }
    return n;
}

// Records lane `li`'s class in the work item's class words, and (lane 0) the words and the
// item's per-class counts for the scan. Every lane of the workgroup must call it.
fn record_classes(item_index: u32, li: u32, kind: u32) {
    if (kind != 0u) {
        atomicOr(&item_classes[li / 16u], kind << (2u * (li % 16u)));
    }
    workgroupBarrier();
    if (li == 0u && item_index < atomicLoad(&state[WORK])) {
        let words = vec4<u32>(
            atomicLoad(&item_classes[0]),
            atomicLoad(&item_classes[1]),
            atomicLoad(&item_classes[2]),
            atomicLoad(&item_classes[3]),
        );
        scan[item_index] = words;
        var counts =
            class_counts(words.x) + class_counts(words.y) + class_counts(words.z) + class_counts(words.w);
        // A work item's lanes are one instance's, so its software lanes are all of one side.
        if ((instances[work[item_index].x].flags.y & DOUBLE_SIDED) != 0u) {
            counts.w = counts.z;
        }
        scan[params.scan.x + item_index] = counts;
    }
}

// Clears the class words of this workgroup's item. Every lane of the workgroup must call it.
fn clear_classes(li: u32) {
    if (li < 4u) {
        atomicStore(&item_classes[li], 0u);
    }
    workgroupBarrier();
}

// Writes lane `li` of a work item into `phase`'s lists at its scanned slot: back-face-culled
// clusters from the start (count in that phase's DrawIndirect), double-sided ones from the end
// (count in its double-sided DrawIndirect), software clusters to the third range of the buffer,
// shared by both phases, after the earlier phase's. Software clusters past their side's room join
// their cull mode's list in list order. `finalize_phase` set the counts each list keeps.
fn place(phase: u32, wg: vec3<u32>, nwg: vec3<u32>, li: u32) {
    let item_index = wg.x + wg.y * nwg.x;
    if (item_index >= atomicLoad(&state[WORK])) {
        return;
    }
    let words = scan[item_index];
    let kind = (words[li / 16u] >> (2u * (li % 16u))) & 3u;
    if (kind == 0u) {
        return;
    }
    let item = work[item_index];
    let entry = vec2<u32>(item.x, instances[item.x].info.x + item.y + li);
    let at = scan_offset(item_index);
    let below = classes_below(words, li);
    let cap = params.counts.y;
    let double_sided = (instances[item.x].flags.y & DOUBLE_SIDED) != 0u;
    // This item's side of the software range: where it starts, its room, and this lane's place
    // among that side's software clusters (those of earlier items and those below this lane).
    let room = atomicLoad(&state[SOFT_ROOM]);
    let single_kept = atomicLoad(&state[SOFT_SINGLE_KEPT + phase]);
    let side_first = select(0u, single_kept, double_sided);
    let side_room = select(room, room - single_kept, double_sided);
    let side_index = select(at.z - at.w, at.w, double_sided) + below.z;
    if (kind == CLASS_SOFT && side_index < side_room) {
        clusters[2u * cap + atomicLoad(&state[SOFT_FROM]) + side_first + side_index] = entry;
        return;
    }
    // Software clusters past the room before this lane join its list ahead of it.
    let past_room = side_index - min(side_index, side_room);
    if (double_sided) {
        let d = at.y + below.y + past_room;
        if (d < atomicLoad(&state[KEPT_DOUBLE + phase])) {
            clusters[phase * cap + cap - 1u - d] = entry;
        }
        return;
    }
    let h = at.x + below.x + past_room;
    if (h < atomicLoad(&state[KEPT_SINGLE + phase])) {
        clusters[phase * cap + h] = entry;
    }
}

fn world_sphere(inst: Instance, s: vec4<f32>) -> vec4<f32> {
    return vec4<f32>((inst.model * vec4<f32>(s.xyz, 1.0)).xyz, s.w * inst.scale.x);
}

// How far a swaying instance's vertices can leave its rest bounds, while some sway moves (ADR-3916).
// In calm air it is zero, so the lists are the lists of the same material without sway.
fn sway_reach(inst: Instance) -> f32 {
    return inst.scale.y * f32(params.scan.w);
}

// `s` (object space) as the visibility tests see it: under the model and grown by the sway's reach.
// The LOD test keeps the rest spheres (`world_sphere`), so wind does not move the cut.
fn cull_sphere(inst: Instance, s: vec4<f32>) -> vec4<f32> {
    let world = world_sphere(inst, s);
    return vec4<f32>(world.xyz, world.w + sway_reach(inst));
}

fn projected_error(s: vec4<f32>, error: f32) -> f32 {
    let d = distance(s.xyz, params.eye.xyz) - s.w;
    return error * params.screen.w / max(d, params.forward.w);
}

// Hold each boundary comparison independently. Linked children/parents use the same metric,
// so mixed-level cuts stay consistent (ADR-4631).
fn lod_above(error: f32, previous: bool, valid: bool) -> bool {
    if (valid && lod_band() > 0.0 && abs(error - params.eye.w) <= lod_band()) {
        return previous;
    }
    return error > params.eye.w;
}

fn lod_decision(inst: Instance, m: Meshlet, prev: u32) -> u32 {
    let valid = (prev & 4u) != 0u;
    let own = lod_above(projected_error(world_sphere(inst, m.lod), m.lod_error * inst.scale.x),
        (prev & 1u) != 0u, valid);
    let parent = m.parent_error >= ROOT || lod_above(
        projected_error(world_sphere(inst, m.parent), m.parent_error * inst.scale.x),
        (prev & 2u) != 0u, valid);
    return 4u | u32(own) | (u32(parent) << 1u) | (u32(!own && parent) << 3u);
}

// Decisions are prepared before visibility/cone tests, including clusters pruned by level runs.
fn lod_selected(index: u32, inst: Instance, local: u32) -> bool {
    let offset = params.lod.z + 1u + inst.info.z + local;
    return (atomicLoad(&lod_words[offset]) & 8u) != 0u;
}

fn ndc_of(world: vec3<f32>) -> vec4<f32> {
    let clip = params.view_proj * vec4<f32>(world, 1.0);
    return vec4<f32>(clip.xyz / clip.w, clip.w);
}

// A sphere is occluded when its nearest depth is behind the farthest depth of every Hi-Z texel
// its screen footprint touches. The pyramid is power-of-two sized with far depth outside the
// screen (`vis_hiz.wgsl`), so each mip texel covers exactly 2^mip pixels and odd edges are kept.
fn hiz_occluded(center: vec3<f32>, radius: f32) -> bool {
    if (params.screen.z < 0.5) {
        return false;
    }
    let c = ndc_of(center);
    // A sphere that reaches the camera plane projects from behind the eye: never culled.
    if (c.w - radius <= params.forward.w) {
        return false;
    }
    let nearer = ndc_of(center - params.forward.xyz * radius);
    let nearest_z = min(c.z, nearer.z);
    let size = vec2<f32>(max(params.screen.x, 1.0), max(params.screen.y, 1.0));
    // Footprint radius in pixels, taken at the sphere's nearest depth. Off the view axis the
    // sphere's image stretches along the radius by 1 + tan²θ (θ the angle off the axis, as in
    // `takes_software`); without it a wide lens samples too few texels at the screen's edges and
    // culls a cluster whose rim is still visible. `c.w - radius > 0` above.
    let off_axis = 0.5 * params.screen.xy * c.xy / params.screen.w;
    let stretch = 1.0 + dot(off_axis, off_axis);
    let px = radius * params.screen.w * stretch / (c.w - radius);
    let p = vec2<f32>(c.x * 0.5 + 0.5, 0.5 - c.y * 0.5) * size;
    if (p.x + px < 0.0 || p.y + px < 0.0 || p.x - px > size.x || p.y - px > size.y) {
        return false;
    }
    let mips = u32(params.screen.z);
    // The footprint (2 px across) fits in one texel at this mip, so it touches at most 2×2.
    let mip = min(u32(ceil(log2(max(px * 2.0, 1.0)))), mips - 1u);
    let texel = f32(1u << mip);
    let last = vec2<i32>(textureDimensions(hiz, i32(mip))) - vec2<i32>(1);
    let lo = clamp(vec2<i32>(floor(max(p - vec2<f32>(px), vec2<f32>(0.0)) / texel)), vec2<i32>(0), last);
    let hi = clamp(vec2<i32>(floor((p + vec2<f32>(px)) / texel)), vec2<i32>(0), last);
    var farthest = 0.0;
    for (var y = lo.y; y <= hi.y; y = y + 1) {
        for (var x = lo.x; x <= hi.x; x = x + 1) {
            farthest = max(farthest, textureLoad(hiz, vec2<i32>(x, y), i32(mip)).r);
        }
    }
    return nearest_z > farthest;
}

// x = instance, y = meshlet, z = cluster index within the instance, w = 1 when valid.
fn candidate(wg: vec3<u32>, nwg: vec3<u32>, li: u32) -> vec4<u32> {
    let item_index = wg.x + wg.y * nwg.x;
    if (item_index >= atomicLoad(&state[WORK])) {
        return vec4<u32>(0u);
    }
    let item = work[item_index];
    let local = item.y + li;
    let info = instances[item.x].info;
    if (local >= info.y) {
        return vec4<u32>(0u);
    }
    return vec4<u32>(item.x, info.x + local, local, 1u);
}

fn passes(index: u32, inst: Instance, m: Meshlet, local: u32) -> bool {
    if (!lod_selected(index, inst, local)) {
        return false;
    }
    let s = cull_sphere(inst, m.bounds);
    if (!frustum_inside(s.xyz, s.w)) {
        return false;
    }
    // A swaying cluster's triangles leave the rest pose's normal cone: the raster culls their backs.
    if (params.counts.z == 1u
        && (inst.flags.y & DOUBLE_SIDED) == 0u
        && m.cone.w < 1.0
        && sway_reach(inst) <= 0.0) {
        let axis = normalize((inst.model * vec4<f32>(m.cone.xyz, 0.0)).xyz);
        let to_center = s.xyz - params.eye.xyz;
        if (dot(to_center, axis) >= m.cone.w * length(to_center) + s.w) {
            return false;
        }
    }
    return true;
}

// Phase 1: a selected cluster that was visible last frame.
fn phase1_class(wg: vec3<u32>, nwg: vec3<u32>, li: u32) -> u32 {
    let c = candidate(wg, nwg, li);
    if (c.w == 0u) {
        return 0u;
    }
    let inst = instances[c.x];
    let m = meshlets[c.y];
    if (!passes(c.x, inst, m, c.z)) {
        return 0u;
    }
    let bit = inst.info.z + c.z;
    if ((atomicLoad(&bits[bit >> 5u]) & (1u << (bit & 31u))) == 0u) {
        return 0u;
    }
    return class_of(
        (inst.flags.y & DOUBLE_SIDED) != 0u,
        (inst.flags.y & MASKED) == 0u && takes_software(inst, cull_sphere(inst, m.bounds)),
    );
}

// Phase 2: records every selected cluster's visibility against the Hi-Z pyramid for the next
// frame, and lists the visible ones phase 1 did not draw. Each cluster owns its bit, so the
// bits end the same whatever order the lanes run in.
fn phase2_class(wg: vec3<u32>, nwg: vec3<u32>, li: u32) -> u32 {
    let c = candidate(wg, nwg, li);
    if (c.w == 0u) {
        return 0u;
    }
    let inst = instances[c.x];
    let m = meshlets[c.y];
    if (!passes(c.x, inst, m, c.z)) {
        return 0u;
    }
    let bit = inst.info.z + c.z;
    let mask = 1u << (bit & 31u);
    let s = cull_sphere(inst, m.bounds);
    if (hiz_occluded(s.xyz, s.w)) {
        atomicAnd(&bits[bit >> 5u], ~mask);
        return 0u;
    }
    if ((atomicOr(&bits[bit >> 5u], mask) & mask) != 0u) {
        // Drawn in phase 1.
        return 0u;
    }
    return class_of((inst.flags.y & DOUBLE_SIDED) != 0u, (inst.flags.y & MASKED) == 0u && takes_software(inst, s));
}

@compute @workgroup_size(64)
fn cull_phase1(
    @builtin(workgroup_id) wg: vec3<u32>,
    @builtin(num_workgroups) nwg: vec3<u32>,
    @builtin(local_invocation_index) li: u32,
) {
    clear_classes(li);
    record_classes(wg.x + wg.y * nwg.x, li, phase1_class(wg, nwg, li));
}

@compute @workgroup_size(64)
fn cull_phase2(
    @builtin(workgroup_id) wg: vec3<u32>,
    @builtin(num_workgroups) nwg: vec3<u32>,
    @builtin(local_invocation_index) li: u32,
) {
    clear_classes(li);
    record_classes(wg.x + wg.y * nwg.x, li, phase2_class(wg, nwg, li));
}

@compute @workgroup_size(64)
fn place_phase1(
    @builtin(workgroup_id) wg: vec3<u32>,
    @builtin(num_workgroups) nwg: vec3<u32>,
    @builtin(local_invocation_index) li: u32,
) {
    place(0u, wg, nwg, li);
}

@compute @workgroup_size(64)
fn place_phase2(
    @builtin(workgroup_id) wg: vec3<u32>,
    @builtin(num_workgroups) nwg: vec3<u32>,
    @builtin(local_invocation_index) li: u32,
) {
    place(1u, wg, nwg, li);
}

// One workgroup per instance. Each lane advances distinct cluster boundaries before the cull.
// In-place history is safe: later phases read only the prepared selection bit.
@compute @workgroup_size(64)
fn lod_prepare(@builtin(workgroup_id) wg: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>,
    @builtin(local_invocation_index) lane: u32) {
    let index = wg.x + wg.y * nwg.x;
    let cap = params.lod.z;
    let in_bounds = index < params.counts.x && index < cap;
    if (lane == 0u && in_bounds) {
        atomicStore(&lod_words[index], 0u);
        if (index == 0u) {
            atomicStore(&lod_words[cap], 0u);
        }
    }
    storageBarrier();
    // FXC requires every invocation to reach the barrier before any bounds return.
    if (!in_bounds) {
        return;
    }
    let inst = instances[index];
    for (var local = lane; local < inst.info.y; local += 64u) {
        let offset = cap + 1u + inst.info.z + local;
        var prev = atomicLoad(&lod_words[offset]);
        if ((params.lod.y & 1u) != 0u) {
            prev = 0u;
        }
        let chosen = lod_decision(inst, meshlets[inst.info.x + local], prev);
        atomicStore(&lod_words[offset], chosen);
        if ((prev & 4u) != 0u && ((prev ^ chosen) & 8u) != 0u) {
            atomicOr(&lod_words[index], 1u);
        }
    }
}

// Count changed instance cuts, rather than changed meshlets. Missing history is not a switch.
@compute @workgroup_size(64)
fn lod_commit(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>) {
    let index = gid.x + gid.y * nwg.x * 64u;
    let cap = params.lod.z;
    if (index >= params.counts.x || index >= cap) {
        return;
    }
    if (atomicLoad(&lod_words[index]) != 0u) {
        atomicAdd(&lod_words[cap], 1u);
    }
}
