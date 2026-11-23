// SPDX-License-Identifier: MPL-2.0
// Visibility-buffer instances in the shadow maps: per shadow job (a sun cascade, one sun page,
// or a local-light atlas tile) the casters are culled against the job's light-space side planes,
// and each cluster takes the LOD cut for the job's texel size. An orthographic cascade projects
// an object-space error `e` to `e · texels per metre` texels at any distance; a perspective
// tile divides by the distance to the light as the camera view does. The cut test and its
// crack-free argument are the camera's (`vis_cull.wgsl`). The selected clusters are drawn
// depth-only with one indirect draw per job (the vertex stage pulls triangles as
// `vis_raster.wgsl` does).
//
// Jobs run in order, and every slot comes from exclusive prefix sums (ADR-4778, the camera
// cull's scheme of ADR-2294): a count pass writes one value per element, `scan_instances` /
// `scan_work_items` scan blocks of 256 in workgroup memory (Hillis & Steele), a one-workgroup
// pass scans the block sums with a running carry (Blelloch), and a placement pass writes each
// element at its scanned offset. So the work items and each job's cluster ranges are in
// (instance, cluster) order on every run and adapter, and a capacity cut drops a stable tail in
// that order: with the atomic counters ADR-2294 left here, the shadow map past `max_clusters`
// kept whichever invocations won the scheduler, so the kept set — and the shaded frame —
// changed between runs (depth only cannot change a texel only while the kept set is the full
// set). The frame's cluster capacity fills at each job's turn: the job's single-sided clusters
// keep the room left, its double-sided ones the room after that. Clusters of double-sided
// instances (glTF `doubleSided`) fill the cluster list from its end, with their own range and
// DrawIndirect per job, so they draw with the cull-none pipeline (`vs_shadow_reversed`).

struct ShadowJob {
    view_proj: mat4x4<f32>,
    // Side planes of the job's region (left, right, bottom, top), inside >= 0.
    planes: array<vec4<f32>, 4>,
    // x = texels per metre (perspective: at unit distance), y = LOD threshold in texels,
    // z = 1 for a perspective job, w = smallest distance (perspective).
    lod: vec4<f32>,
    // xyz = light position (perspective jobs), w = reach slack of GPU cascade masks (metres).
    light: vec4<f32>,
    // xyz = camera eye snapped to a grid (GPU cascade masks), w = active cascade count.
    camera: vec4<f32>,
    // Far distance of each sun cascade.
    splits: vec4<f32>,
    // x = job index, y = sun cascade (255 = a local-light tile), z = instance count,
    // w = cluster capacity.
    info: vec4<u32>,
    // x = work-item capacity, y = job capacity of `jobs`, z = 1 while some sway moves (ADR-3916).
    caps: vec4<u32>,
    // x = first vec4 of the scan's per-element values, y = first vec4 of its block sums; the
    // work items' class words come before both (`vis_cull.wgsl` lays its scan out the same).
    scan: vec4<u32>,
}

struct Instance {
    model: mat4x4<f32>,
    sphere: vec4<f32>,
    info: vec4<u32>,
    // x = largest axis scale, y = sway reach (metres; ADR-3916).
    scale: vec4<f32>,
    // x = shadow scale, y = shadow lift (world Y), z = shadow reach.
    shadow: vec4<f32>,
    // x = shadow flags (see the constants below), y bit 0 = double-sided.
    flags: vec4<u32>,
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

// Instance shadow flags: bits 0-7 the sun cascades it is drawn into (when not GPU_MASK),
// CASTS = drawn into shadow maps at all, GPU_MASK = pick the cascades here from the reach and
// the cascade cap in bits 12-15 (`ShadowCast::Local` of a persistent instance set).
const MASK_BITS: u32 = 0xffu;
const CASTS: u32 = 0x100u;
const GPU_MASK: u32 = 0x200u;
const VERTICES_PER_CLUSTER: u32 = 372u;
const ROOT: f32 = 3.0e38;
const NO_CASCADE: u32 = 255u;
// `Instance.flags.y`: back faces drawn.
const DOUBLE_SIDED: u32 = 1u;

// `jobs` words: [0] work items admitted, [1] single-sided clusters admitted (from the list's
// start), [2] clusters dropped at capacity, [3] work items dropped at capacity, [4] double-sided
// clusters admitted (from the list's end), [5] spare; [8..12) the running scan's totals; job `j`'s
// record at 16 + 12j: DrawIndirect (4 words), work start, work count, single-sided cluster base,
// double-sided cluster base (counted from the end), the double-sided DrawIndirect (4 words); job
// `j`'s dispatch arguments at 16 + 12 · job capacity + 8j (`args_base` below): the cluster passes'
// dispatch (3 words) and the work-item scan's (3 words).
const HEADER: u32 = 16u;
const RECORD: u32 = 12u;
// Where `scan_block_sums` leaves the scan's totals (one vec4).
const TOTAL: u32 = 8u;
// Elements per scan workgroup.
const SCAN_BLOCK: u32 = 256u;
// A lane's class in its work item's class words: not selected, single-sided, double-sided.
const CLASS_SINGLE: u32 = 1u;
const CLASS_DOUBLE: u32 = 2u;

@group(0) @binding(0) var<uniform> job: ShadowJob;
@group(0) @binding(1) var<storage, read> instances: array<Instance>;
@group(0) @binding(2) var<storage, read> meshlets: array<Meshlet>;
@group(0) @binding(3) var<storage, read_write> work: array<vec2<u32>>;
@group(0) @binding(4) var<storage, read_write> jobs: array<atomic<u32>>;
@group(0) @binding(5) var<storage, read_write> clusters: array<vec2<u32>>;
// The work items' class words at [0, work-item capacity), then the scan's per-element values
// from `job.scan.x`, then its block sums from `job.scan.y`.
@group(0) @binding(6) var<storage, read_write> scan: array<vec4<u32>>;

var<workgroup> scan_shared: array<vec4<u32>, 256>;
var<workgroup> scan_blocks: u32;
var<workgroup> item_classes: array<atomic<u32>, 4>;

fn record() -> u32 {
    return HEADER + job.info.x * RECORD;
}

fn args_base() -> u32 {
    return HEADER + job.caps.y * RECORD + job.info.x * 8u;
}

@compute @workgroup_size(1)
fn shadow_reset() {
    for (var i = 0u; i < 6u; i = i + 1u) {
        atomicStore(&jobs[i], 0u);
    }
}

// The DrawIndirect words of both of the job's draws; the counts are set once the clusters are
// scanned (`scan_top_clusters`), the work range by `scan_top_instances`.
@compute @workgroup_size(1)
fn job_begin() {
    let r = record();
    atomicStore(&jobs[r], VERTICES_PER_CLUSTER);
    atomicStore(&jobs[r + 1u], 0u);
    atomicStore(&jobs[r + 2u], 0u);
    atomicStore(&jobs[r + 3u], 0u);
    atomicStore(&jobs[r + 6u], 0u);
    atomicStore(&jobs[r + 7u], 0u);
    atomicStore(&jobs[r + 8u], VERTICES_PER_CLUSTER);
    atomicStore(&jobs[r + 9u], 0u);
    atomicStore(&jobs[r + 10u], 0u);
    atomicStore(&jobs[r + 11u], 0u);
}

// The model the shadow passes draw with: `shadow_scale` about the model origin, then the lift.
fn shadow_point(inst: Instance, world: vec3<f32>) -> vec3<f32> {
    let origin = inst.model[3].xyz;
    return origin + (world - origin) * inst.shadow.x + vec3<f32>(0.0, inst.shadow.y, 0.0);
}

fn shadow_sphere(inst: Instance, s: vec4<f32>) -> vec4<f32> {
    let world = (inst.model * vec4<f32>(s.xyz, 1.0)).xyz;
    return vec4<f32>(shadow_point(inst, world), s.w * inst.scale.x * inst.shadow.x);
}

// How far a swaying instance's vertices can leave its rest bounds, while some sway moves (ADR-3916).
// The LOD test keeps the rest spheres, so a calm frame and a windy one take the same cut.
fn sway_reach(inst: Instance) -> f32 {
    return inst.scale.y * f32(job.caps.z);
}

fn inside(s: vec4<f32>) -> bool {
    for (var i = 0u; i < 4u; i = i + 1u) {
        let plane = job.planes[i];
        if (dot(plane.xyz, s.xyz) + plane.w < -s.w) {
            return false;
        }
    }
    return true;
}

// The sun cascades a short caster at `distance` from the eye can shadow: cascade `i`'s receivers
// lie between its near split (less the 15 % blend band into it) and its far split; at most
// `cap` cascades, starting from the one that holds the caster (`frame.rs` `cascade_mask`).
fn gpu_cascade_mask(distance: f32, radius: f32, reach: f32, cap: u32) -> u32 {
    let count = min(u32(job.camera.w), 4u);
    let lower = distance - radius - reach;
    let upper = distance + radius + reach;
    var first = 99u;
    var last = 0u;
    var home = 99u;
    for (var i = 0u; i < count; i = i + 1u) {
        var near = 0.0;
        if (i > 0u) {
            near = job.splits[i - 1u];
        }
        var prev_near = 0.0;
        if (i > 1u) {
            prev_near = job.splits[i - 2u];
        }
        var lo = 0.0;
        if (i > 0u) {
            lo = near - 0.15 * (near - prev_near);
        }
        let hi = job.splits[i];
        if (upper >= lo && lower <= hi) {
            if (first == 99u) {
                first = i;
            }
            last = i;
        }
        if (home == 99u && distance <= hi) {
            home = i;
        }
    }
    if (first == 99u || cap == 0u) {
        return 0u;
    }
    if (home == 99u) {
        home = last;
    }
    home = clamp(home, first, last);
    let start = max(min(home, last - min(last, cap - 1u)), first);
    let end = min(start + cap - 1u, last);
    var mask = 0u;
    for (var i = start; i <= end; i = i + 1u) {
        mask = mask | (1u << i);
    }
    return mask;
}

// Work items of instance `index` for this job: one per 64 clusters when the caster is kept
// (shared by the count and the placement pass, so both agree on the set).
fn instance_chunks(index: u32) -> u32 {
    let inst = instances[index];
    let flags = inst.flags.x;
    if ((flags & CASTS) == 0u) {
        return 0u;
    }
    // `sphere` is already in world space.
    let s = vec4<f32>(shadow_point(inst, inst.sphere.xyz), inst.sphere.w * inst.shadow.x + sway_reach(inst));
    let cascade = job.info.y;
    if (cascade != NO_CASCADE) {
        var mask = flags & MASK_BITS;
        if ((flags & GPU_MASK) != 0u) {
            let distance = distance(s.xyz, job.camera.xyz);
            mask = gpu_cascade_mask(distance, s.w, inst.shadow.z + job.light.w, (flags >> 12u) & 0xfu);
        }
        if ((mask & (1u << cascade)) == 0u) {
            return 0u;
        }
    }
    if (!inside(s)) {
        return 0u;
    }
    return (inst.info.y + 63u) / 64u;
}

// The count pass: every instance's work-item count for the scan (zero for a culled one).
@compute @workgroup_size(64)
fn job_instances(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>) {
    let index = gid.x + gid.y * nwg.x * 64u;
    if (index >= job.info.z) {
        return;
    }
    scan[job.scan.x + index] = vec4<u32>(instance_chunks(index), 0u, 0u, 0u);
}

// The exclusive prefix of element `i` of the last scan: its block's offset plus its rank there.
fn scan_offset(i: u32) -> vec4<u32> {
    return scan[job.scan.x + i] + scan[job.scan.y + i / SCAN_BLOCK];
}

// First level over `n` elements in place, block by block, into exclusive in-block prefixes,
// writing each block's sum (Blelloch's first pass; `vis_cull.wgsl` scans the same way).
fn scan_local_n(wg: vec3<u32>, nwg: vec3<u32>, li: u32, n: u32) {
    let block = wg.x + wg.y * nwg.x;
    let i = block * SCAN_BLOCK + li;
    var value = vec4<u32>(0u);
    if (i < n) {
        value = scan[job.scan.x + i];
    }
    let inclusive = workgroup_inclusive_scan(li, value);
    if (i < n) {
        scan[job.scan.x + i] = inclusive - value;
    }
    if (li == SCAN_BLOCK - 1u && block * SCAN_BLOCK < n) {
        scan[job.scan.y + block] = inclusive;
    }
}

@compute @workgroup_size(256)
fn scan_instances(
    @builtin(workgroup_id) wg: vec3<u32>,
    @builtin(num_workgroups) nwg: vec3<u32>,
    @builtin(local_invocation_index) li: u32,
) {
    scan_local_n(wg, nwg, li, job.info.z);
}

@compute @workgroup_size(256)
fn scan_work_items(
    @builtin(workgroup_id) wg: vec3<u32>,
    @builtin(num_workgroups) nwg: vec3<u32>,
    @builtin(local_invocation_index) li: u32,
) {
    scan_local_n(wg, nwg, li, atomicLoad(&jobs[record() + 5u]));
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

// Second level, one workgroup: the block sums become exclusive block offsets, 256 per round
// with a running carry, and the totals go to `jobs[TOTAL..]`. Any count is correct; the cost is
// ceil(n / 65536) rounds of eight barrier steps. Every lane of the workgroup must call it.
fn scan_block_sums(li: u32, n: u32) {
    if (li == 0u) {
        scan_blocks = (n + SCAN_BLOCK - 1u) / SCAN_BLOCK;
    }
    let blocks = workgroupUniformLoad(&scan_blocks);
    var carry = vec4<u32>(0u);
    for (var start = 0u; start < blocks; start = start + SCAN_BLOCK) {
        let b = start + li;
        var value = vec4<u32>(0u);
        if (b < blocks) {
            value = scan[job.scan.y + b];
        }
        let inclusive = workgroup_inclusive_scan(li, value);
        if (b < blocks) {
            scan[job.scan.y + b] = carry + inclusive - value;
        }
        carry = carry + scan_shared[SCAN_BLOCK - 1u];
        workgroupBarrier();
    }
    if (li == 0u) {
        atomicStore(&jobs[TOTAL], carry.x);
        atomicStore(&jobs[TOTAL + 1u], carry.y);
        atomicStore(&jobs[TOTAL + 2u], carry.z);
        atomicStore(&jobs[TOTAL + 3u], carry.w);
    }
}

// The instance scan's second level, then (lane 0) the job's work range and the cluster passes'
// dispatches. The frame's work-item capacity fills in job order: this job keeps the prefix of
// its work items the room left admits, and the tail counts as dropped.
@compute @workgroup_size(256)
fn scan_top_instances(@builtin(local_invocation_index) li: u32) {
    scan_block_sums(li, job.info.z);
    if (li == 0u) {
        let total = atomicLoad(&jobs[TOTAL]);
        let start = atomicLoad(&jobs[0]);
        let room = job.caps.x - min(start, job.caps.x);
        let kept = min(total, room);
        let r = record();
        atomicStore(&jobs[r + 4u], start);
        atomicStore(&jobs[r + 5u], kept);
        atomicStore(&jobs[0], start + kept);
        if (total > kept) {
            atomicAdd(&jobs[3], total - kept);
        }
        // One workgroup per admitted work item (`dispatch_workgroups_indirect` caps a
        // dimension at 65535), and the scan over the admitted work items; both dispatches read
        // their arguments from the `args` buffer (never bound as storage), written here into
        // this job's slot of the `jobs` buffer and copied over once per frame.
        let a = args_base();
        atomicStore(&jobs[a], min(kept, 65535u));
        atomicStore(&jobs[a + 1u], max((kept + 65534u) / 65535u, 1u));
        atomicStore(&jobs[a + 2u], 1u);
        let blocks = (kept + SCAN_BLOCK - 1u) / SCAN_BLOCK;
        atomicStore(&jobs[a + 4u], min(blocks, 65535u));
        atomicStore(&jobs[a + 5u], max((blocks + 65534u) / 65535u, 1u));
        atomicStore(&jobs[a + 6u], 1u);
    }
}

// Writes each kept instance's work items at its scanned offset, in instance order; the work
// items past the frame's capacity were counted as dropped by `scan_top_instances`.
@compute @workgroup_size(64)
fn place_instances(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>) {
    let index = gid.x + gid.y * nwg.x * 64u;
    if (index >= job.info.z) {
        return;
    }
    let chunks = instance_chunks(index);
    let base = scan_offset(index).x;
    let r = record();
    let kept = atomicLoad(&jobs[r + 5u]);
    let start = atomicLoad(&jobs[r + 4u]);
    for (var c = 0u; c < chunks; c = c + 1u) {
        if (base + c < kept) {
            work[start + base + c] = vec2<u32>(index, c * 64u);
        }
    }
}

fn projected_error(s: vec4<f32>, error: f32) -> f32 {
    if (job.lod.z < 0.5) {
        return error * job.lod.x;
    }
    let d = distance(s.xyz, job.light.xyz) - s.w;
    return error * job.lod.x / max(d, job.lod.w);
}

// The class of lane `li` of this work item's cluster: not selected, or the side of its list
// (the LOD cut and region test are the old `job_clusters` body, unchanged).
fn cluster_class(wg: vec3<u32>, nwg: vec3<u32>, li: u32) -> u32 {
    let r = record();
    let item_index = wg.x + wg.y * nwg.x;
    if (item_index >= atomicLoad(&jobs[r + 5u])) {
        return 0u;
    }
    let item = work[atomicLoad(&jobs[r + 4u]) + item_index];
    let local = item.y + li;
    let inst = instances[item.x];
    if (local >= inst.info.y) {
        return 0u;
    }
    let index = inst.info.x + local;
    let m = meshlets[index];
    let scale = inst.scale.x * inst.shadow.x;
    if (projected_error(shadow_sphere(inst, m.lod), m.lod_error * scale) > job.lod.y) {
        return 0u;
    }
    if (m.parent_error < ROOT
        && projected_error(shadow_sphere(inst, m.parent), m.parent_error * scale) <= job.lod.y) {
        return 0u;
    }
    let bounds = shadow_sphere(inst, m.bounds);
    if (!inside(vec4<f32>(bounds.xyz, bounds.w + sway_reach(inst)))) {
        return 0u;
    }
    if ((inst.flags.y & DOUBLE_SIDED) != 0u) {
        return CLASS_DOUBLE;
    }
    return CLASS_SINGLE;
}

// Lanes of each class in one class word (16 lanes): x single-sided, y double-sided.
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
    if (li == 0u && item_index < atomicLoad(&jobs[record() + 5u])) {
        let words = vec4<u32>(
            atomicLoad(&item_classes[0]),
            atomicLoad(&item_classes[1]),
            atomicLoad(&item_classes[2]),
            atomicLoad(&item_classes[3]),
        );
        scan[item_index] = words;
        var counts = vec4<u32>(0u);
        for (var k = 0u; k < 4u; k = k + 1u) {
            counts = counts + class_counts(words[k]);
        }
        scan[job.scan.x + item_index] = counts;
    }
}

// Clears the class words of this workgroup's item. Every lane of the workgroup must call it.
fn clear_classes(li: u32) {
    if (li < 4u) {
        atomicStore(&item_classes[li], 0u);
    }
    workgroupBarrier();
}

// The record pass: one workgroup per admitted work item writes its 64 lanes' classes.
@compute @workgroup_size(64)
fn job_clusters(
    @builtin(workgroup_id) wg: vec3<u32>,
    @builtin(num_workgroups) nwg: vec3<u32>,
    @builtin(local_invocation_index) li: u32,
) {
    clear_classes(li);
    record_classes(wg.x + wg.y * nwg.x, li, cluster_class(wg, nwg, li));
}

// The cluster scan's second level, then (lane 0) what this job keeps. The frame's cluster
// capacity fills at each job's turn, singles first: this job's single-sided clusters keep the
// room left, its double-sided ones the room after that, each in (instance, cluster) order, and
// the rest count as dropped.
@compute @workgroup_size(256)
fn scan_top_clusters(@builtin(local_invocation_index) li: u32) {
    scan_block_sums(li, atomicLoad(&jobs[record() + 5u]));
    if (li == 0u) {
        let single = atomicLoad(&jobs[TOTAL]);
        let double = atomicLoad(&jobs[TOTAL + 1u]);
        let s_base = atomicLoad(&jobs[1]);
        let d_base = atomicLoad(&jobs[4]);
        let room = job.info.w - min(s_base + d_base, job.info.w);
        let single_kept = min(single, room);
        let double_kept = min(double, room - single_kept);
        let r = record();
        atomicStore(&jobs[r + 1u], single_kept);
        atomicStore(&jobs[r + 9u], double_kept);
        atomicStore(&jobs[r + 6u], s_base);
        atomicStore(&jobs[r + 7u], d_base);
        atomicStore(&jobs[1], s_base + single_kept);
        atomicStore(&jobs[4], d_base + double_kept);
        let dropped = (single - single_kept) + (double - double_kept);
        if (dropped > 0u) {
            atomicAdd(&jobs[2], dropped);
        }
    }
}

// Writes lane `li` of a work item into its cluster list at its scanned slot: single-sided from
// the list's start, double-sided from its end (the reversed draw counts its range backwards).
// The kept counts `scan_top_clusters` set drop a stable tail.
@compute @workgroup_size(64)
fn place_clusters(
    @builtin(workgroup_id) wg: vec3<u32>,
    @builtin(num_workgroups) nwg: vec3<u32>,
    @builtin(local_invocation_index) li: u32,
) {
    let r = record();
    let item_index = wg.x + wg.y * nwg.x;
    if (item_index >= atomicLoad(&jobs[r + 5u])) {
        return;
    }
    let words = scan[item_index];
    let kind = (words[li / 16u] >> (2u * (li % 16u))) & 3u;
    if (kind == 0u) {
        return;
    }
    let item = work[atomicLoad(&jobs[r + 4u]) + item_index];
    let entry = vec2<u32>(item.x, instances[item.x].info.x + item.y + li);
    let at = scan_offset(item_index);
    let below = classes_below(words, li);
    if (kind == CLASS_DOUBLE) {
        let d = at.y + below.y;
        if (d < atomicLoad(&jobs[r + 9u])) {
            clusters[job.info.w - 1u - (atomicLoad(&jobs[r + 7u]) + d)] = entry;
        }
        return;
    }
    let h = at.x + below.x;
    if (h < atomicLoad(&jobs[r + 1u])) {
        clusters[atomicLoad(&jobs[r + 6u]) + h] = entry;
    }
}
