// SPDX-License-Identifier: MPL-2.0
#import regia::clustering

// Builds froxel light lists. One thread per cluster.
// Exponential slices: Olsson, Billeter, Assarsson, HPG 2012.

struct LabFrame {
    view_proj: mat4x4<f32>,
    view: mat4x4<f32>,
    camera_pos: vec4<f32>,
    params: vec4<f32>,
    screen: vec4<f32>,
    extra: vec4<f32>,
}

struct GpuLight {
    position_kind: vec4<f32>,
    color_intensity: vec4<f32>,
    direction_radius: vec4<f32>,
    cone_shadow: vec4<f32>,
}

struct Aabb {
    lo: vec3<f32>,
    hi: vec3<f32>,
}

@group(0) @binding(0)
var<uniform> frame: LabFrame;

@group(0) @binding(1)
var<storage, read> lights: array<GpuLight>;

@group(0) @binding(2)
var<storage, read_write> counts: array<u32>;

@group(0) @binding(3)
var<storage, read_write> indices: array<u32>;

fn view_at(ndc_x: f32, ndc_y: f32, depth: f32, fov_y: f32, aspect: f32) -> vec3<f32> {
    let tan_y = tan(fov_y * 0.5);
    return vec3<f32>(ndc_x * aspect * tan_y * depth, ndc_y * tan_y * depth, -depth);
}

fn cluster_bounds(cell_x: u32, cell_y: u32, cell_z: u32) -> Aabb {
    let near = frame.params.x;
    let far = frame.params.y;
    let fov_y = frame.params.z;
    let aspect = frame.params.w;
    let x0 = f32(cell_x) / 16.0;
    let x1 = f32(cell_x + 1u) / 16.0;
    let y0 = f32(cell_y) / 9.0;
    let y1 = f32(cell_y + 1u) / 9.0;
    let z0 = regia::clustering::slice_depth(f32(cell_z), near, far);
    let z1 = regia::clustering::slice_depth(f32(cell_z + 1u), near, far);
    var lo = vec3<f32>(1.0e+30);
    var hi = vec3<f32>(-1.0e+30);
    for (var iz: u32 = 0u; iz < 2u; iz = iz + 1u) {
        let z = select(z0, z1, iz == 1u);
        for (var iy: u32 = 0u; iy < 2u; iy = iy + 1u) {
            let uy = select(y0, y1, iy == 1u);
            let ndc_y = 1.0 - uy * 2.0;
            for (var ix: u32 = 0u; ix < 2u; ix = ix + 1u) {
                let ux = select(x0, x1, ix == 1u);
                let ndc_x = ux * 2.0 - 1.0;
                let p = view_at(ndc_x, ndc_y, z, fov_y, aspect);
                lo = min(lo, p);
                hi = max(hi, p);
            }
        }
    }
    return Aabb(lo, hi);
}

fn sphere_hits_aabb(center: vec3<f32>, radius: f32, box: Aabb) -> bool {
    let c = clamp(center, box.lo, box.hi);
    return distance(c, center) <= radius;
}

// Finite spot cone against a froxel. A reject is disjoint. An accept may
// include froxels the true cone misses: `sqrt` in this compute shader
// miscompiled on lavapipe and rejected every cluster, so the half-angle is
// widened with `sin(θ) <= (sin²θ + 1) / 2` and the box uses the L1 corner
// radius. Both contain the tight cone/AABB test. The forward pass still
// applies the exact spot cone. Matches `cone_may_hit_aabb` in `light_budget.rs`.
// Wider than ~87° (cos ≤ 0.05) keeps the sphere result.
fn cone_may_hit_aabb(apex: vec3<f32>, dir_in: vec3<f32>, range: f32, cos_outer: f32, lo: vec3<f32>, hi: vec3<f32>) -> bool {
    let closest = clamp(apex, lo, hi);
    if (distance(closest, apex) > range) {
        return false;
    }
    if (cos_outer <= 0.05 || cos_outer != cos_outer) {
        return true;
    }
    let dir_len = length(dir_in);
    if (dir_len <= 1e-4) {
        return true;
    }
    let dir = dir_in / dir_len;
    let center = (lo + hi) * 0.5;
    let ext = (hi - lo) * 0.5;
    let rel = center - apex;
    let along = dot(rel, dir);
    // `ext` is non-negative when `lo <= hi`.
    let along_radius = ext.x * abs(dir.x) + ext.y * abs(dir.y) + ext.z * abs(dir.z);
    if (along + along_radius < 0.0) {
        return false;
    }
    let cos_safe = max(cos_outer, 1e-3);
    let sin2 = max(1.0 - cos_safe * cos_safe, 0.0);
    let tan_upper = ((sin2 + 1.0) * 0.5) / cos_safe;
    let reach = clamp(along + along_radius, 0.0, range);
    let perp2 = max(dot(rel, rel) - along * along, 0.0);
    let half_l1 = ext.x + ext.y + ext.z;
    let limit = reach * tan_upper + half_l1;
    return perp2 <= limit * limit;
}

@compute @workgroup_size(64)
fn cs_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let cluster = gid.x;
    let total = regia::clustering::cluster_count();
    if cluster >= total {
        return;
    }
    let cell_x = cluster % 16u;
    let cell_y = (cluster / 16u) % 9u;
    let cell_z = cluster / (16u * 9u);
    let box = cluster_bounds(cell_x, cell_y, cell_z);
    let light_count = u32(frame.screen.z);
    let max_per = u32(frame.screen.w);
    let base = cluster * max_per;
    var written = 0u;
    for (var i: u32 = 0u; i < light_count; i = i + 1u) {
        if written >= max_per {
            break;
        }
        let lp = (frame.view * vec4<f32>(lights[i].position_kind.xyz, 1.0)).xyz;
        let radius = max(lights[i].direction_radius.w, 0.05);
        let kind = u32(lights[i].position_kind.w + 0.5);
        var hit = sphere_hits_aabb(lp, radius, box);
        if (hit && kind == 2u) {
            let dir_v = (frame.view * vec4<f32>(lights[i].direction_radius.xyz, 0.0)).xyz;
            hit = cone_may_hit_aabb(lp, dir_v, radius, lights[i].cone_shadow.y, box.lo, box.hi);
        }
        if (hit) {
            indices[base + written] = i;
            written = written + 1u;
        }
    }
    counts[cluster] = written;
}
