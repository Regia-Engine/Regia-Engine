// SPDX-License-Identifier: MPL-2.0
#import regia::utils

// Local exposure, step 1: the bilateral grid of the exposed HDR image's log luminance
// (Chen, Paris and Durand, "Real-time edge-aware image processing with the bilateral grid",
// SIGGRAPH 2007; Paris and Durand, "A fast approximation of the bilateral filter using a
// signal processing approach", ECCV 2006). One grid cell per `cell` × `cell` pixel block and
// per 1 EV luminance bin holds the homogeneous pair (mean log luminance · weight, weight);
// `cs_blur` smooths it with a [1 2 1]³ kernel into the 3D texture the exposure pass slices
// (`post.wgsl` mode 0: trilinear lookup at the pixel's own position and luminance, then
// Durand and Dorsey's base / detail split).

const BINS: u32 = 16u;
// Log2 of the exposed luminance the bins span: 2^-10 .. 2^6 (1 EV per bin).
const LOG_MIN: f32 = -10.0;
const LOG_RANGE: f32 = 16.0;
// Fixed-point scale of the per-bin sums (workgroup atomics are integer).
const FIXED: f32 = 4096.0;

struct LeParams {
    // x = exposure multiplier the exposure pass applies, y = cell edge (pixels),
    // zw = image size (pixels).
    a: vec4<f32>,
    // The exposure pass's white-balance gains (rgb).
    wb: vec4<f32>,
    // Grid cells along x, y and bins.
    dims: vec4<u32>,
}

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var<uniform> le: LeParams;
@group(0) @binding(2) var<storage, read_write> grid: array<vec2<f32>>;
@group(0) @binding(3) var grid_out: texture_storage_3d<rgba16float, write>;

var<workgroup> bin_sum: array<atomic<u32>, 16>;
var<workgroup> bin_count: array<atomic<u32>, 16>;

fn grid_index(x: u32, y: u32, z: u32) -> u32 {
    return x + le.dims.x * (y + le.dims.y * z);
}

// One workgroup per pixel block: every pixel's normalised log luminance lands in its bin.
@compute @workgroup_size(8, 8, 1)
fn cs_build(
    @builtin(workgroup_id) wg: vec3<u32>,
    @builtin(local_invocation_id) lid: vec3<u32>,
    @builtin(local_invocation_index) li: u32,
) {
    if (li < BINS) {
        atomicStore(&bin_sum[li], 0u);
        atomicStore(&bin_count[li], 0u);
    }
    workgroupBarrier();
    let cell = u32(le.a.y);
    let size = vec2<u32>(le.a.zw);
    let origin = wg.xy * cell;
    for (var y = lid.y; y < cell; y = y + 8u) {
        for (var x = lid.x; x < cell; x = x + 8u) {
            let px = origin + vec2<u32>(x, y);
            if (px.x >= size.x || px.y >= size.y) {
                continue;
            }
            let c = regia::utils::finite_or_zero(textureLoad(src, vec2<i32>(px), 0).rgb) * le.a.x * le.wb.rgb;
            let t = clamp((log2(max(regia::utils::luminance(c), 1e-8)) - LOG_MIN) / LOG_RANGE, 0.0, 1.0);
            let bin = min(u32(t * f32(BINS)), BINS - 1u);
            atomicAdd(&bin_sum[bin], u32(t * FIXED + 0.5));
            atomicAdd(&bin_count[bin], 1u);
        }
    }
    workgroupBarrier();
    if (li < BINS) {
        let n = atomicLoad(&bin_count[li]);
        let s = atomicLoad(&bin_sum[li]);
        let w = f32(n) / f32(max(cell * cell, 1u));
        let mean = select(0.0, f32(s) / (FIXED * f32(max(n, 1u))), n > 0u);
        grid[grid_index(wg.x, wg.y, li)] = vec2<f32>(mean * w, w);
    }
}

// [1 2 1]³ blur of the homogeneous grid (cells outside the grid add nothing).
@compute @workgroup_size(4, 4, 4)
fn cs_blur(@builtin(global_invocation_id) id: vec3<u32>) {
    let d = le.dims.xyz;
    if (id.x >= d.x || id.y >= d.y || id.z >= d.z) {
        return;
    }
    var acc = vec2<f32>(0.0);
    for (var dz: i32 = -1; dz <= 1; dz = dz + 1) {
        for (var dy: i32 = -1; dy <= 1; dy = dy + 1) {
            for (var dx: i32 = -1; dx <= 1; dx = dx + 1) {
                let p = vec3<i32>(id) + vec3<i32>(dx, dy, dz);
                if (any(p < vec3<i32>(0)) || any(p >= vec3<i32>(d))) {
                    continue;
                }
                let k = f32((2 - abs(dx)) * (2 - abs(dy)) * (2 - abs(dz)));
                acc += grid[grid_index(u32(p.x), u32(p.y), u32(p.z))] * k;
            }
        }
    }
    textureStore(grid_out, id, vec4<f32>(acc / 64.0, 0.0, 0.0));
}
