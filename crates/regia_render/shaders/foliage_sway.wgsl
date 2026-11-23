// SPDX-License-Identifier: MPL-2.0
#define_import_path regia::foliage_sway

// The foliage sway (`specs/88` § 2.2, ADR-2028), the one definition every path shares: the forward,
// depth, shadow and velocity hosts call `offset` through `FOLIAGE_SWAY_VERTEX`, and the visibility
// path's rasters, resolve and velocity pass call it through `cluster_offset` (ADR-3916). Mirrored on
// the CPU by `FoliageSway::offset`; keep the two in step.

// One swaying material of the visibility path (`visbuffer/gpu/sway.rs`): graph slot 15, graph slot 14,
// and graph slot 0 (time, previous time, wind x, wind z) as the forward passes read them.
struct SwayRecord {
    sway: vec4<f32>,
    limit: vec4<f32>,
    clock: vec4<f32>,
}

// `frame`: xyz = the main camera's eye, w = pixels per unit at unit distance over the LOD threshold.
// One record per distinct sway. `SWAY_TABLE_RECORDS` in `visbuffer/sway.rs`.
struct SwayTable {
    frame: vec4<f32>,
    records: array<SwayRecord, 256>,
}

// The natural style's noise (ADR-2446); mirrored by `crate::wind_natural`.
fn fsw_pcg(v: u32) -> u32 {
    let state = v * 747796405u + 2891336453u;
    let word = ((state >> ((state >> 28u) + 4u)) ^ state) * 277803737u;
    return (word >> 22u) ^ word;
}

fn fsw_unit(h: u32) -> f32 {
    return f32(h >> 8u) / 16777216.0;
}

fn fsw_lattice(ix: i32, iy: i32, seed: u32) -> f32 {
    return fsw_unit(fsw_pcg((bitcast<u32>(ix) * 0x9E3779B1u) ^ fsw_pcg(bitcast<u32>(iy) ^ seed)));
}

fn fsw_vnoise(p: vec2<f32>, seed: u32) -> f32 {
    let f = floor(p);
    let t = p - f;
    let s = t * t * (3.0 - 2.0 * t);
    let ix = i32(f.x);
    let iy = i32(f.y);
    let a = fsw_lattice(ix, iy, seed);
    let b = fsw_lattice(ix + 1, iy, seed);
    let c = fsw_lattice(ix, iy + 1, seed);
    let d = fsw_lattice(ix + 1, iy + 1, seed);
    let top = a + (b - a) * s.x;
    let bottom = c + (d - c) * s.x;
    return top + (bottom - top) * s.y;
}

// The world offset of a vertex at `world` (rest pose) with unit `normal` and vertex colour red `red`,
// on an object whose origin is `origin`, at clock `time` under `wind`. `sway` and `limit` are graph
// slots 15 and 14.
fn offset(
    sway: vec4<f32>,
    limit: vec4<f32>,
    wind: vec2<f32>,
    time: f32,
    world: vec3<f32>,
    normal: vec3<f32>,
    red: f32,
    origin: vec3<f32>,
) -> vec3<f32> {
    let cap = max(limit.x, 0.0);
    let raw = length(wind);
    let speed = min(raw, cap);
    if (speed <= 1.0e-4 || sway.y <= 1.0e-4 || time - time != 0.0) {
        return vec3<f32>(0.0);
    }
    let dir = wind / raw;
    if (limit.y > 0.5) {
        let o = origin.xz;
        // The material clock wraps at 3600 s (ADR-1238): evaluate the field on one lap, as
        // `FoliageSway::offset` does, so the wrap is seamless.
        let t = fract(time / 3600.0) * 3600.0;
        let u = dot(o, dir);
        let v = -o.x * dir.y + o.y * dir.x;
        let big = fsw_vnoise(vec2<f32>((u - 3.2 * t) / 46.0, v / 30.0), 0x51ed270bu);
        let small = fsw_vnoise(vec2<f32>((u - 2.1 * t) / 14.0 + 17.3, (v + 0.8 * t) / 11.0 + 5.1), 0x9c3a7d15u);
        let intensity = smoothstep(0.40, 0.74, 0.65 * big + 0.35 * small);
        let pa = fsw_vnoise(vec2<f32>(o.x / 6.0 + 31.0, o.y / 6.0), 0x2f6b1a93u);
        let pb = fsw_vnoise(vec2<f32>(o.x / 2.3, o.y / 2.3 + 9.0), 0x7be144c7u);
        let nphase = 6.283185307179586 * 2.0 * (0.6 * pa + 0.4 * pb);
        let hf = clamp(sqrt(0.4 / max(sway.y, 0.02)), 0.6, 1.25);
        let nw = 6.283185307179586 * 0.55 * hf * t / max(sway.w, 0.1);
        let ngust = intensity * (0.55 + 0.45 * sin(nw + nphase));
        let namp = min(max(sway.x, 0.0) * speed * ngust, 0.5 * sway.y);
        let nh = clamp((world.y - origin.y) / sway.y, 0.0, 1.0);
        let nbend = namp * nh * nh;
        let ndrop = namp * namp * nh * nh * nh / (2.0 * sway.y);
        let seed = bitcast<u32>(sway.x) ^ (bitcast<u32>(sway.y) * 0x9E3779B1u);
        let band = 2.6 + 2.0 * fsw_unit(fsw_pcg(seed ^ 0xf10a77e5u));
        let ndetail = dot(world, vec3<f32>(1.7, 0.9, 2.3));
        let nflutter = max(sway.z, 0.0) * clamp(red, 0.0, 1.0) * (speed / (speed + 5.0)) * intensity
            * sin(6.283185307179586 * 2.0 * band * t / max(sway.w, 0.1) + ndetail);
        return vec3<f32>(dir.x * nbend, -ndrop, dir.y * nbend) + normal * nflutter;
    }
    let lap = fract(time / max(sway.w, 0.1));
    let w = 6.283185307179586 * lap;
    let phase = dot(origin.xz, vec2<f32>(0.37, 0.61));
    let gust = 0.7 + 0.2 * sin(w + phase) + 0.1 * sin(3.0 * w + 1.7 * phase);
    let amp = min(max(sway.x, 0.0) * speed * gust, 0.5 * sway.y);
    let h = clamp((world.y - origin.y) / sway.y, 0.0, 1.0);
    let bend = amp * h * h;
    let drop = amp * amp * h * h * h / (2.0 * sway.y);
    let detail = dot(world, vec3<f32>(1.7, 0.9, 2.3));
    let flutter = max(sway.z, 0.0) * clamp(red, 0.0, 1.0) * (speed / (speed + 5.0))
        * sin(7.0 * w + detail);
    return vec3<f32>(dir.x * bend, -drop, dir.y * bend) + normal * flutter;
}

// How much of the sway an instance keeps at `distance` from the eye when its first voxel far level
// can be selected from `switch_distance` on: all of it up to 70 % of that distance, none from 95 %.
// So the voxel levels, which never sway, meet a plant at rest (`sway_fade` in `visbuffer/sway.rs`).
fn fade(distance: f32, switch_distance: f32) -> f32 {
    if (switch_distance <= 0.0) {
        return 0.0;
    }
    return clamp((0.95 * switch_distance - distance) / (0.25 * switch_distance), 0.0, 1.0);
}

// The offset of a visibility-path vertex (ADR-3916): `model_normal` is the normal under the instance's
// model, `center` the instance's world sphere centre, `voxel` its `scale` word (z = the first voxel
// level's world error, w = the distance that level's sphere adds; z = 0: no voxel levels).
fn cluster_offset(
    record: SwayRecord,
    frame: vec4<f32>,
    center: vec3<f32>,
    voxel: vec4<f32>,
    time: f32,
    world: vec3<f32>,
    model_normal: vec3<f32>,
    red: f32,
    origin: vec3<f32>,
) -> vec3<f32> {
    let moved = offset(
        record.sway,
        record.limit,
        record.clock.zw,
        time,
        world,
        model_normal * inverseSqrt(max(dot(model_normal, model_normal), 1e-12)),
        red,
        origin,
    );
    if (voxel.z <= 0.0) {
        return moved;
    }
    return moved * fade(distance(center, frame.xyz), voxel.z * frame.w + voxel.w);
}
