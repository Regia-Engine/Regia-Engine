// SPDX-License-Identifier: MPL-2.0
//! WGSL sources for the particle, decal and trail pipelines.
//!
//! Layouts mirror [`super::layout`]. Provenance for each non-trivial algorithm is collected in
//! `docs/provenance.md`:
//!
//! - Curl turbulence: Bridson & Hsu, "Curl-Noise for Procedural Fluid Flow" (2007), on top of an
//!   in-house PCG-hashed lattice value noise (O'Neill, PCG report, 2014).
//! - Bitonic sort: Batcher's sorting networks (1968).
//! - Soft particles: Lorach, "Soft Particles" (NVIDIA DX10 whitepaper, 2007).
//! - Projected decals: in-house derivation (depth reconstruction + box projection + grazing fade).

/// Shared declarations available to every module: the frame uniform, PCG hashing, value noise,
/// curl noise and depth reconstruction. Modules must declare their own group-1 bindings.
pub const COMMON: &str = r#"
struct Frame {
    view_proj: mat4x4<f32>,
    inv_view_proj: mat4x4<f32>,
    view: mat4x4<f32>,
    cam_right_dt: vec4<f32>,   // xyz right, w delta time
    cam_up_time: vec4<f32>,    // xyz up,    w elapsed time
    cam_pos_near: vec4<f32>,   // xyz pos,   w near plane
    viewport: vec4<f32>,       // w, h, 1/w, 1/h
    params: vec4<f32>,         // far, sort_enabled, depth_collision, spawn_count
    sim: vec4<f32>,            // capacity, sort_n, screen floor px, exposure
    light0: vec4<f32>,         // xyz toward the sun (world), w ambient
    light1: vec4<f32>,         // rgb sun colour, w pixels per metre at depth 1
    decal: vec4<f32>,          // decal flags, target mode (0 bytes, 1 sRGB, 2 HDR), haze on (0/1), haze scale
    fire_pos: vec4<f32>,       // xyz strongest particle light (world), w radius (0 = none)
    fire_color: vec4<f32>,     // rgb linear colour × intensity, w bit 0 spherical billboards, bits 1.. soft-pass mode
    window: vec4<f32>,         // update window: first pool slot, slot count; z ring slot of spawn job 0
    sky_up: vec4<f32>,         // rgb sky radiance from above (display units), w 1 when set (ADR-1425)
    sky_down: vec4<f32>,       // rgb light from the ground below (display units)
    shift: vec4<f32>,          // xyz floating-origin shift of a rebase frame, else zero
    events: vec4<u32>,         // child-ring slots to update, event step, event capacity, child-ring size
};

@group(0) @binding(0) var<uniform> frame: Frame;

// `layout::MAX_EMITTERS`. Event ranks index the link and child-init tables with this bound.
const MAX_EMITTERS: u32 = 256u;

// PCG-style 32-bit hash (O'Neill 2014); integer-exact, so results are portable.
fn hash_u32(seed: u32) -> u32 {
    let state = seed * 747796405u + 2891336453u;
    let word = ((state >> ((state >> 28u) + 4u)) ^ state) * 277803737u;
    return (word >> 22u) ^ word;
}

// `events::child_unit`: a hash channel as a float in [0, 1). Shared by the event prepare and
// child-emit kernels and the CPU mirror (`events.rs`).
fn child_unit(seed: u32, channel: u32) -> f32 {
    return f32(hash_u32(seed ^ (channel * 2654435769u)) & 0xFFFFFFu) / 16777216.0;
}

// How many events one pool slot can fire in one update: a spawn (age 0), then a death or a
// collision, or a collision plus its kill death. Event ranks are `slot_ordinal * 3 + n`, so the
// staged set is a pure function of the slot order, never of thread scheduling (the vfx1
// determinism audit, finding 1). `frame.events.z` counts kept *events*, not ranks:
// `event_prepare` compacts the first `frame.events.z` fired events in slot order, so a frame
// keeps however many events actually fired, up to the budget, wherever their slots sit in the
// window (fixrev-22-k, Muse review of fix-vfx1-determinism finding 1).
const EVENTS_PER_SLOT: u32 = 3u;
// `layout::STATS_EVENT_HIGHEST`: one past the highest slot ordinal that staged an event this
// frame (`fire_event` tracks it with an order-independent atomicMax; `event_prepare` scans the
// fired-slot mask only up to it, clears what it scanned, and resets the slot to 0).
const STATS_EVENT_HIGHEST: u32 = 2u;
// `format::MAX_EVENTS_PER_EMITTER`.
const LINKS_PER_EMITTER: u32 = 4u;
// Reservation sentinel in `event_reservations`: the link was rolled back (did not fit the ring).
const NO_RESERVATION: u32 = 0xFFFFFFFFu;

// The slots the update covers: the main ring's window, then the child ring behind it (slots
// `capacity..capacity + child ring`), which only event spawns write. No live instance with
// events: `frame.events.x` is 0 and this is exactly the window. Used by `update` and
// `shift_pool`; `event_prepare` bounds its scan by the mask's capacity instead, since staged
// ordinals belong to the window of the frame that fired them.
fn pool_slots() -> u32 {
    return min(u32(frame.window.y), u32(frame.sim.x)) + frame.events.x;
}

fn pool_slot(k: u32) -> u32 {
    let capacity = u32(frame.sim.x);
    let window = min(u32(frame.window.y), capacity);
    if (k < window) { return (u32(frame.window.x) + k) % capacity; }
    return capacity + (k - window);
}

// Words of `event_mask` per parity buffer: one bit for every pool slot (main ring + child
// ring), the largest region `pool_slots` can cover. Shared by `fire_event` (SIM_BINDINGS) and
// `event_prepare` (EVENT_PREPARE); the mask buffer itself is declared in each module.
fn event_mask_words() -> u32 {
    return (u32(frame.sim.x) + frame.events.w + 31u) / 32u;
}

// Trailing-zero count of a nonzero u32: naga has no bit-scan builtin, so a binary search over
// the halves (naga's WGSL lacks `firstTrailingZero`).
fn ctz32(bits: u32) -> u32 {
    var b = bits;
    var n = 0u;
    if ((b & 0xffffu) == 0u) { n = n + 16u; b = b >> 16u; }
    if ((b & 0xffu) == 0u) { n = n + 8u; b = b >> 8u; }
    if ((b & 0xfu) == 0u) { n = n + 4u; b = b >> 4u; }
    if ((b & 0x3u) == 0u) { n = n + 2u; b = b >> 2u; }
    if ((b & 0x1u) == 0u) { n = n + 1u; }
    return n;
}

fn hash3_to_unit(seed: u32) -> vec3<f32> {
    let a = f32(hash_u32(seed) & 0xFFFFu) / 65535.0 * 2.0 - 1.0;
    let b = f32(hash_u32(seed ^ 0x9E3779B9u) & 0xFFFFu) / 65535.0 * 2.0 - 1.0;
    let c = f32(hash_u32(seed ^ 0x85EBCA6Bu) & 0xFFFFu) / 65535.0 * 2.0 - 1.0;
    return normalize(vec3<f32>(a, b, c) + vec3<f32>(1e-5));
}

fn corner_hash(ii: vec3<u32>, off: vec3<u32>, channel: u32) -> f32 {
    let h = hash_u32((ii.x + off.x) * 197u ^ (ii.y + off.y) * 60493u ^ (ii.z + off.z) * 1759u ^ channel);
    return f32(h & 0xFFFFFFu) / 16777215.0;
}

// Trilinear lattice value noise in [0, 1].
fn value_noise(p: vec3<f32>, channel: u32) -> f32 {
    let ip = floor(p);
    let f = p - ip;
    let w = f * f * (3.0 - 2.0 * f);
    // Bit-cast the signed lattice index: a plain f32 -> u32 conversion saturates every negative
    // cell to 0, which would make the field constant below the origin.
    let ii = bitcast<vec3<u32>>(vec3<i32>(ip));
    let c000 = corner_hash(ii, vec3<u32>(0u, 0u, 0u), channel);
    let c100 = corner_hash(ii, vec3<u32>(1u, 0u, 0u), channel);
    let c010 = corner_hash(ii, vec3<u32>(0u, 1u, 0u), channel);
    let c110 = corner_hash(ii, vec3<u32>(1u, 1u, 0u), channel);
    let c001 = corner_hash(ii, vec3<u32>(0u, 0u, 1u), channel);
    let c101 = corner_hash(ii, vec3<u32>(1u, 0u, 1u), channel);
    let c011 = corner_hash(ii, vec3<u32>(0u, 1u, 1u), channel);
    let c111 = corner_hash(ii, vec3<u32>(1u, 1u, 1u), channel);
    let x00 = mix(c000, c100, w.x);
    let x10 = mix(c010, c110, w.x);
    let x01 = mix(c001, c101, w.x);
    let x11 = mix(c011, c111, w.x);
    return mix(mix(x00, x10, w.y), mix(x01, x11, w.y), w.z);
}

fn potential(p: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(value_noise(p, 1u), value_noise(p, 2u), value_noise(p, 3u));
}

// Curl of a 3-channel potential field via central differences (Bridson & Hsu 2007).
fn curl_noise(p: vec3<f32>, scale: f32) -> vec3<f32> {
    let e = 0.75;
    let dx = (potential(p + vec3<f32>(e, 0.0, 0.0)) - potential(p - vec3<f32>(e, 0.0, 0.0))) / (2.0 * e);
    let dy = (potential(p + vec3<f32>(0.0, e, 0.0)) - potential(p - vec3<f32>(0.0, e, 0.0))) / (2.0 * e);
    let dz = (potential(p + vec3<f32>(0.0, 0.0, e)) - potential(p - vec3<f32>(0.0, 0.0, e))) / (2.0 * e);
    return vec3<f32>(dy.z - dz.y, dz.x - dx.z, dx.y - dy.x) * scale;
}

// Reconstructs the world position behind a pixel from its post-projection depth ([0,1], RH
// 0..w clip). Returns vec4(world.xyz, view-space depth).
fn world_from_depth(uv: vec2<f32>, depth: f32) -> vec4<f32> {
    let ndc = vec3<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, depth);
    let world = frame.inv_view_proj * vec4<f32>(ndc, 1.0);
    let w = max(world.w, 1e-6);
    let p = world.xyz / w;
    let view_p = frame.view * vec4<f32>(p, 1.0);
    return vec4<f32>(p, -view_p.z);
}
"#;

/// Curve-LUT sampling functions. The including module must have declared `curve_scalars`
/// (storage, read, `array<f32>`) and `curve_colors` (storage, read, `array<vec4<f32>>`).
pub const LUT_FNS: &str = r#"
fn lut_scalar(id: u32, t: f32) -> f32 {
    let n = 8u;
    let x = clamp(t, 0.0, 1.0) * f32(n - 1u);
    let i = min(u32(x), n - 2u);
    let f = x - f32(i);
    let a = curve_scalars[id * n + i];
    let b = curve_scalars[id * n + i + 1u];
    return a + (b - a) * f;
}

fn lut_color(id: u32, t: f32) -> vec4<f32> {
    let n = 8u;
    let x = clamp(t, 0.0, 1.0) * f32(n - 1u);
    let i = min(u32(x), n - 2u);
    let f = x - f32(i);
    let a = curve_colors[id * n + i];
    let b = curve_colors[id * n + i + 1u];
    return vec4<f32>(a.xyz + (b.xyz - a.xyz) * f, a.w + (b.w - a.w) * f);
}
"#;

/// Simulation compute: ring-buffer emission and the update kernel (forces, curves, kills,
/// gather into blend/mode draw lists).
pub const SIM_BINDINGS: &str = r#"
struct Emitter {
    gravity_drag: vec4<f32>,
    curl: vec4<f32>,
    vortex: vec4<f32>,
    attractor: vec4<f32>,
    kill: vec4<f32>,
    curve_ids: vec4<u32>,
    render: vec4<f32>,
    flip: vec4<f32>,
    stretch: vec4<f32>,
    origin: vec4<f32>,
    quality: vec4<f32>,
    wind: vec4<f32>,
    events: vec4<u32>,
    kill_volume: vec4<f32>,
    kill_axis_x: vec4<f32>,
    kill_axis_y: vec4<f32>,
    kill_axis_z: vec4<f32>,
};

struct SpawnJob {
    pos_life: vec4<f32>,
    vel_size: vec4<f32>,
    rot_spin: vec4<f32>,
    color: vec4<f32>,
};

// A fired event: where the parent was, what it was doing, the seed of its children.
struct VfxEvent {
    pos: vec3<f32>,
    seed: u32,
    vel: vec3<f32>,
    trigger_parent: u32,
};

@group(1) @binding(0) var<storage, read>       emitters: array<Emitter>;
@group(1) @binding(1) var<storage, read_write> pos_age: array<vec4<f32>>;
@group(1) @binding(2) var<storage, read_write> vel_life: array<vec4<f32>>;
@group(1) @binding(3) var<storage, read_write> attrib: array<vec4<f32>>;
@group(1) @binding(4) var<storage, read_write> color0: array<vec4<f32>>;
@group(1) @binding(5) var<storage, read>       spawn_jobs: array<SpawnJob>;
@group(1) @binding(6) var<storage, read_write> counters: array<atomic<u32>>;
@group(1) @binding(7) var<storage, read_write> stats: array<atomic<u32>>;
@group(1) @binding(8) var<storage, read_write> lists: array<vec2<u32>>;
@group(1) @binding(9) var<storage, read>       curve_scalars: array<f32>;
@group(1) @binding(10) var<storage, read>      curve_colors: array<vec4<f32>>;
@group(1) @binding(11) var depth_tex: texture_depth_2d;
@group(1) @binding(12) var depth_sampler: sampler;
@group(1) @binding(13) var<storage, read_write> events: array<VfxEvent>;
// The epoch (simulated step) each staged `events` rank was written at: `event_prepare` skips
// ranks whose epoch is not this frame's step, so stale slots are never replayed and no buffer
// has to be cleared per frame. Sized `3 * (particle capacity + child capacity)` like `events`
// itself: every fired event is staged (at most `EVENTS_PER_SLOT` ranks per pool slot) and the
// over-budget drop happens in `event_prepare`, by count, not here by position.
@group(1) @binding(14) var<storage, read_write> event_epoch: array<u32>;
// One bit per pool slot, double-buffered by the event step's parity: `update` sets the bit of
// every slot that fired an event, and `event_prepare` scans only the set bits (never the empty
// ranks between them) and clears every word it consumed, in both parities. Sized
// `2 * ceil((particle capacity + child capacity) / 32)`.
@group(1) @binding(15) var<storage, read_write> event_mask: array<atomic<u32>>;

// Draw-list regions: additive [0, sort_n), alpha [sort_n, 2*sort_n), mesh [2*sort_n, 3*sort_n).
fn list_base(list: u32) -> u32 {
    let sort_n = u32(frame.sim.y);
    if (list == 1u) { return sort_n; }
    if (list == 2u) { return sort_n * 2u; }
    return 0u;
}

// The additive list is never sorted, and a profile that does not sort draws its alpha and mesh
// lists unsorted too, so the update does not append with an atomic counter (thread-scheduling
// order): it writes each slot's `(key, list)` at its slot ordinal in a fourth region,
// `[3*sort_n, 4*sort_n)`, and `GATHER` compacts that region into the three lists in slot-ordinal
// order (ADR-5991). The sorts then run on the alpha and mesh lists as before.
const GATHER_NO_LIST: u32 = 3u;

fn gather_base() -> u32 {
    return u32(frame.sim.y) * 3u;
}
"#;

/// Emit + update entry points (to be concatenated after [`SIM_BINDINGS`] + [`LUT_FNS`]).
pub const SIM_ENTRY: &str = r#"
// Event triggers (`events::TRIGGER_*`) and stats words (`layout::STATS_*`).
const TRIGGER_SPAWN: u32 = 1u;
const TRIGGER_DEATH: u32 = 2u;
const TRIGGER_COLLISION: u32 = 4u;
const STATS_EVENT_FIRED: u32 = 4u;
const STATS_EVENT_OVERFLOW: u32 = 5u;

// A particle's identity for its event seeds, the same on every run: a main-ring slot comes from
// the host's cursor. A child-ring slot depends on thread order, so a child carries its identity
// in the low bits of its lifetime instead (`events::particle_identity`).
fn particle_identity(i: u32, life: f32) -> u32 {
    if (i < u32(frame.sim.x)) { return i; }
    return (bitcast<u32>(life) & 0x3FFu) | 0x100000u;
}

// `events::event_seed`.
fn event_seed(identity: u32, trigger: u32) -> u32 {
    return hash_u32((identity * 2654435761u) ^ (frame.events.y * 40503u) ^ (trigger * 2246822519u));
}

// Stages the event at the rank `slot_ordinal * EVENTS_PER_SLOT + n` and stamps its epoch;
// `event_prepare` next frame compacts the first `frame.events.z` fired events in slot order
// (the fired-slot mask above bounds its scan), so which events survive an over-capacity frame
// is a function of how many fired and where the firing slots sit, never of thread scheduling.
// Every fired event is staged: the buffer is sized `EVENTS_PER_SLOT` ranks per pool slot, and
// the over-budget drop is counted in `event_prepare`, by count.
fn fire_event(trigger: u32, slot: u32, i: u32, k: u32, n: u32, life: f32, p: vec3<f32>, v: vec3<f32>) -> u32 {
    atomicAdd(&stats[STATS_EVENT_FIRED], 1u);
    let rank = k * EVENTS_PER_SLOT + n;
    events[rank] = VfxEvent(p, event_seed(particle_identity(i, life), trigger), v, trigger | (slot << 8u));
    event_epoch[rank] = frame.events.y;
    let words = event_mask_words();
    atomicOr(&event_mask[(frame.events.y & 1u) * words + k / 32u], 1u << (k % 32u));
    atomicMax(&stats[STATS_EVENT_HIGHEST], k + 1u);
    return n + 1u;
}

// Ring-buffer pool: job `j` lands in slot `(window.z + j) % capacity`, where `window.z` is the
// host's ring cursor, so the slot of every particle is the same on every run and every GPU
// (an atomic slot claim hands slots out in thread-scheduling order). Overwriting a still-alive
// particle counts toward the `dropped` stat (hard budget enforcement on the GPU). A frame with
// more jobs than slots lands only its last `capacity` jobs; the rest count as dropped.
@compute @workgroup_size(64)
fn emit(@builtin(global_invocation_id) gid: vec3<u32>) {
    let job = gid.x;
    let count = u32(frame.params.w);
    if (job >= count) { return; }
    let capacity = u32(frame.sim.x);
    atomicAdd(&stats[0u], 1u);
    if (count > capacity && job < count - capacity) {
        atomicAdd(&stats[1u], 1u);
        return;
    }
    let slot = (u32(frame.window.z) + job) % capacity;
    if (pos_age[slot].w < vel_life[slot].w) { atomicAdd(&stats[1u], 1u); }
    let j = spawn_jobs[job];
    pos_age[slot] = vec4<f32>(j.pos_life.xyz, 0.0);
    vel_life[slot] = vec4<f32>(j.vel_size.xyz, j.pos_life.w);
    // Slot in the integer part, flipbook phase in the fraction (phase is written in 0..1).
    attrib[slot] = vec4<f32>(j.vel_size.w, j.rot_spin.x, j.rot_spin.y, j.rot_spin.z + clamp(j.rot_spin.w, 0.0, 0.999));
    color0[slot] = j.color;
}

// A floating-origin rebase (spec 78 § 12.2): every slot of the update window, the only ones that
// can be alive, moves by -shift. Dispatched before `emit` on a rebase frame only, so this frame's
// spawns (already in the new space) are not moved twice.
@compute @workgroup_size(64)
fn shift_pool(@builtin(global_invocation_id) gid: vec3<u32>) {
    let k = gid.x;
    if (k >= pool_slots()) { return; }
    let i = pool_slot(k);
    let pa = pos_age[i];
    pos_age[i] = vec4<f32>(pa.xyz - frame.shift.xyz, pa.w);
}

// Kill volume (`.vfx.ron` v7, `kill_volume::ResolvedKillVolume::kills`, operation for operation):
// mode 0 none, 1 box inside, 2 box outside, 3 sphere inside, 4 sphere outside. A box point is
// inside when every |dot(d, axis row)| <= 1 (rows are axis / half extent); a sphere point when
// dot(d, d) <= r * r. The boundary is inside. Plain values, no function-scope struct or array.
fn in_kill_volume(kv: vec4<f32>, ax: vec4<f32>, ay: vec4<f32>, az: vec4<f32>, p: vec3<f32>) -> bool {
    let mode = u32(kv.w);
    if (mode == 0u) { return false; }
    let d = p - kv.xyz;
    var inside = false;
    if (mode <= 2u) {
        inside = max(abs(dot(d, ax.xyz)), max(abs(dot(d, ay.xyz)), abs(dot(d, az.xyz)))) <= 1.0;
    } else {
        inside = dot(d, d) <= ax.x * ax.x;
    }
    if (mode == 2u || mode == 4u) { return !inside; }
    return inside;
}

@compute @workgroup_size(64)
fn update(@builtin(global_invocation_id) gid: vec3<u32>) {
    // Only the ring window that can hold live particles (the spawns of the last max-lifetime
    // seconds); every older slot is dead. The whole pool when the host gives no window.
    let k = gid.x;
    if (k >= pool_slots()) { return; }
    let i = pool_slot(k);
    // "No list" until the gather at the end of this kernel draws the slot (every early return
    // below leaves it so).
    lists[gather_base() + k] = vec2<u32>(0u, GATHER_NO_LIST);
    let vl = vel_life[i];
    if (vl.w <= 0.0) { return; }
    var pa = pos_age[i];
    if (pa.w >= vl.w) { return; }
    let dt = frame.cam_right_dt.w;
    let at = attrib[i];
    let slot = u32(at.w);
    // A frame that does not advance (a second render of one game frame) fires nothing.
    let triggers = select(0u, emitters[slot].events.x, dt > 0.0);
    // This slot's staged-event counter: rank = k * EVENTS_PER_SLOT + event_n, handed to
    // fire_event and threaded through the calls below.
    var event_n = 0u;
    if ((triggers & TRIGGER_SPAWN) != 0u && pa.w <= 0.0) {
        event_n = fire_event(TRIGGER_SPAWN, slot, i, k, event_n, vl.w, pa.xyz, vl.xyz);
    }
    pa.w = pa.w + dt;
    if (pa.w >= vl.w) {
        if ((triggers & TRIGGER_DEATH) != 0u) {
            event_n = fire_event(TRIGGER_DEATH, slot, i, k, event_n, vl.w, pa.xyz, vl.xyz);
        }
        pos_age[i] = pa;
        return;
    }
    let e = emitters[slot];

    // Implicit Euler of dv/dt = g - drag v + c (wind - v), with c × wind in `wind.xyz` and c in
    // `wind.w`: exact at the terminal velocity for any dt, and unchanged where c is 0.
    var v = vl.xyz + (e.gravity_drag.xyz + e.wind.xyz) * dt;
    v = v / (1.0 + (max(e.gravity_drag.w, 0.0) + e.wind.w) * dt);

    // `events.y`: VortexDef::radial (f32 bits), added to the built-in outward 0.25 x strength.
    let vortex_radial = bitcast<f32>(e.events.y);
    if (abs(e.vortex.w) > 0.0 || abs(vortex_radial) > 0.0) {
        let rel = pa.xyz - e.origin.xyz;
        let axis = normalize(e.vortex.xyz + vec3<f32>(1e-6));
        let tangential = cross(axis, rel);
        let radial = rel - axis * dot(rel, axis);
        let rt = length(tangential) + 1e-5;
        let rr = length(radial) + 1e-5;
        v = v + (tangential / rt * e.vortex.w + radial / rr * (e.vortex.w * 0.25 + vortex_radial)) * dt;
    }
    if (abs(e.attractor.w) > 0.0) {
        let d = e.attractor.xyz - pa.xyz;
        let dist2 = dot(d, d) + 1.0;
        // d * inversesqrt(dist2) (not normalize(d)): zero at the centre, never NaN.
        v = v + d * inverseSqrt(dist2) * (e.attractor.w / dist2) * dt;
    }
    if (e.curl.x > 0.0) {
        v = v + curl_noise(pa.xyz * e.curl.y, e.curl.z) * e.curl.x * dt;
    }

    // WGSL forbids assigning to swizzles, so rebuild the whole vec4.
    pa = vec4<f32>(pa.xyz + v * dt, pa.w);
    vel_life[i] = vec4<f32>(v, vl.w);

    if (pa.y < e.kill.x || in_kill_volume(e.kill_volume, e.kill_axis_x, e.kill_axis_y, e.kill_axis_z, pa.xyz)) {
        if ((triggers & TRIGGER_DEATH) != 0u) {
            event_n = fire_event(TRIGGER_DEATH, slot, i, k, event_n, vl.w, pa.xyz, v);
        }
        pos_age[i] = vec4<f32>(pa.xyz, vl.w);
        return;
    }
    // depth-buffer collision: e.kill.y: 0 none, 1 slide, 2 kill
    if (e.kill.y >= 1.0) {
        let clip = frame.view_proj * vec4<f32>(pa.xyz, 1.0);
        let ndc = clip.xyz / max(clip.w, 1e-6);
        let uv = vec2<f32>(ndc.x * 0.5 + 0.5, 1.0 - (ndc.y * 0.5 + 0.5));
        if (uv.x >= 0.0 && uv.x <= 1.0 && uv.y >= 0.0 && uv.y <= 1.0 && ndc.z > 0.0 && ndc.z < 1.0) {
            let scene = textureSampleLevel(depth_tex, depth_sampler, uv, 0i);
            let surface = world_from_depth(uv, scene);
            let particle_d = -(frame.view * vec4<f32>(pa.xyz, 1.0)).z;
            // Only a particle that has just gone through the visible surface collides: one far
            // behind it is hidden by nearer geometry (a ridge, a wall) and flies on. The band
            // covers this frame's travel.
            let band = e.kill.z + 2.0 + 2.0 * length(v) * dt;
            if (particle_d > surface.w + e.kill.z && particle_d < surface.w + band) {
                if ((triggers & TRIGGER_COLLISION) != 0u) {
                    event_n = fire_event(TRIGGER_COLLISION, slot, i, k, event_n, vl.w, pa.xyz, v);
                }
                if (e.kill.y >= 1.5) {
                    if ((triggers & TRIGGER_DEATH) != 0u) {
                        event_n = fire_event(TRIGGER_DEATH, slot, i, k, event_n, vl.w, pa.xyz, v);
                    }
                    pos_age[i] = vec4<f32>(pa.xyz, vl.w);
                    return;
                }
                pa = vec4<f32>(surface.xyz + vec3<f32>(0.0, e.kill.z, 0.0), pa.w);
                vel_life[i] = vec4<f32>(v * 0.3, vl.w);
            }
        }
    }
    pos_age[i] = pa;

    // gather: mesh particles → list 2, alpha → list 1 (sorted), additive → list 0
    let mode = u32(e.render.x);
    var list = 0u;
    if (mode == 2u) {
        list = 2u;
    } else if (u32(e.stretch.z) == 1u) {
        list = 1u;
    }
    // Alpha and mesh keys: 20-bit inverted 2 cm depth bins, 12-bit slot. Ascending sort is
    // back-to-front (both lists blend without depth writes). Equal keys (slots 4096 apart in
    // one bin) fall back to the full slot in the payload (`pair_greater`), so the order
    // cannot flicker. The additive list is not sorted.
    // Must match `quality::alpha_sort_key` (SORT_DEPTH_SCALE = 50, 12 index bits).
    let view_z = frame.view * vec4<f32>(pa.xyz, 1.0);
    let depth = max(-view_z.z, 0.0);
    var key = bitcast<u32>(depth);
    if (list != 0u) {
        let bin = min(u32(depth * 50.0), 0xFFFFFu);
        key = ((0xFFFFFu - bin) << 12u) | (i & 0xFFFu);
    }
    // No alpha sort (mobile): the alpha key is the emitter's CPU back-to-front rank
    // (`Emitter.events.z`, 0 = farthest), sorted by an 8-bit radix pass (ADR-7210) after the
    // slot-ordinal gather (ADR-5991), so particles keep their gather order inside an emitter.
    if (list == 1u && frame.params.y < 0.5) {
        key = min(e.events.z, 255u);
    }
    // Slot-ordinal order, compacted by `GATHER` (which also writes the list counters).
    lists[gather_base() + k] = vec2<u32>(key, list);
    atomicAdd(&counters[3u], 1u);
}
"#;

/// The ordered list gather (ADR-5991), concatenated after [`COMMON`], dispatched after `update`
/// and before the alpha and mesh sorts. The update writes each pool slot's `(key, list)` at its slot ordinal `k` in the
/// fourth list region; three kernels compact it into the additive, alpha and mesh lists in
/// slot-ordinal order, so the draw order is a function of the pool, never of thread scheduling:
///
/// 1. `gather_count`: each 256-slot block counts its entries per list (workgroup atomics: the
///    count does not depend on their order).
/// 2. `gather_scan`: one workgroup turns the block counts into exclusive offsets (each thread
///    sums a run of blocks, then a Hillis–Steele scan over the 256 runs) and stores the list
///    totals in the counters `prepare_indirect` reads.
/// 3. `gather_write`: each block ranks its entries per list with a workgroup prefix sum of
///    packed 10-bit flags and writes `(key, slot)` at the list base + block offset + rank.
pub const GATHER: &str = r#"
@group(1) @binding(0) var<storage, read_write> counters: array<atomic<u32>>;
@group(1) @binding(1) var<storage, read_write> lists: array<vec2<u32>>;
// One vec4 per 256-slot block: the per-list counts (`gather_count`), then the per-list exclusive
// offsets (`gather_scan`).
@group(1) @binding(2) var<storage, read_write> blocks: array<vec4<u32>>;

const GATHER_BLOCK: u32 = 256u;
const GATHER_NO_LIST: u32 = 3u;

fn gather_blocks() -> u32 {
    return (pool_slots() + GATHER_BLOCK - 1u) / GATHER_BLOCK;
}

// The list slot ordinal `k` was gathered into (`GATHER_NO_LIST`: none, or past the pool).
fn gather_list(k: u32) -> u32 {
    if (k >= pool_slots()) { return GATHER_NO_LIST; }
    return min(lists[u32(frame.sim.y) * 3u + k].y, GATHER_NO_LIST);
}

var<workgroup> block_count: array<atomic<u32>, 3>;

@compute @workgroup_size(256)
fn gather_count(
    @builtin(global_invocation_id) gid: vec3<u32>,
    @builtin(local_invocation_id) lid: vec3<u32>,
    @builtin(workgroup_id) wid: vec3<u32>,
) {
    let list = gather_list(gid.x);
    if (list < GATHER_NO_LIST) { atomicAdd(&block_count[list], 1u); }
    workgroupBarrier();
    if (lid.x == 0u && wid.x < gather_blocks()) {
        blocks[wid.x] = vec4<u32>(
            atomicLoad(&block_count[0]),
            atomicLoad(&block_count[1]),
            atomicLoad(&block_count[2]),
            0u,
        );
    }
}

var<workgroup> scan_runs: array<vec4<u32>, 256>;

@compute @workgroup_size(256)
fn gather_scan(@builtin(local_invocation_id) lid: vec3<u32>) {
    let t = lid.x;
    let n = gather_blocks();
    let per = (n + GATHER_BLOCK - 1u) / GATHER_BLOCK;
    let first = min(t * per, n);
    let last = min(first + per, n);
    var run = vec4<u32>(0u);
    for (var b = first; b < last; b = b + 1u) {
        run = run + blocks[b];
    }
    scan_runs[t] = run;
    workgroupBarrier();
    for (var d = 1u; d < GATHER_BLOCK; d = d << 1u) {
        var add = vec4<u32>(0u);
        if (t >= d) { add = scan_runs[t - d]; }
        workgroupBarrier();
        scan_runs[t] = scan_runs[t] + add;
        workgroupBarrier();
    }
    var offset = scan_runs[t] - run;
    for (var b = first; b < last; b = b + 1u) {
        let count = blocks[b];
        blocks[b] = offset;
        offset = offset + count;
    }
    if (t == GATHER_BLOCK - 1u) {
        let total = scan_runs[t];
        atomicStore(&counters[0u], total.x);
        atomicStore(&counters[1u], total.y);
        atomicStore(&counters[2u], total.z);
    }
}

var<workgroup> rank_scan: array<u32, 256>;

@compute @workgroup_size(256)
fn gather_write(
    @builtin(global_invocation_id) gid: vec3<u32>,
    @builtin(local_invocation_id) lid: vec3<u32>,
    @builtin(workgroup_id) wid: vec3<u32>,
) {
    let k = gid.x;
    let t = lid.x;
    let list = gather_list(k);
    // Three 10-bit counters in one word: a block holds at most 256 entries of one list.
    var flag = 0u;
    if (list < GATHER_NO_LIST) { flag = 1u << (10u * list); }
    rank_scan[t] = flag;
    workgroupBarrier();
    for (var d = 1u; d < GATHER_BLOCK; d = d << 1u) {
        var add = 0u;
        if (t >= d) { add = rank_scan[t - d]; }
        workgroupBarrier();
        rank_scan[t] = rank_scan[t] + add;
        workgroupBarrier();
    }
    if (list < GATHER_NO_LIST && wid.x < gather_blocks()) {
        let rank = ((rank_scan[t] - flag) >> (10u * list)) & 0x3FFu;
        let offsets = blocks[wid.x];
        let key = lists[u32(frame.sim.y) * 3u + k].x;
        lists[u32(frame.sim.y) * list + offsets[list] + rank] = vec2<u32>(key, pool_slot(k));
    }
}
"#;

/// Event prepare, one thread: compacts this frame's staged events (slot-ordinal order — the
/// keep-set is deterministic, never thread-scheduling order) into the buffer front, and walks
/// them in order reserving child-ring budget per link. A link that does not fit is rolled back
/// (its reservation stays `NO_RESERVATION` and it consumes no budget), so a later smaller link
/// can still spawn. Group 0 is the frame uniform (from [`COMMON`]).
pub const EVENT_PREPARE: &str = r#"
struct VfxEvent {
    pos: vec3<f32>,
    seed: u32,
    vel: vec3<f32>,
    trigger_parent: u32,
};

struct EventLink {
    trigger_count: u32,
    child_slot: u32,
    probability: f32,
    inherit: f32,
};

@group(1) @binding(0) var<storage, read_write> stats: array<atomic<u32>>;
@group(1) @binding(1) var<storage, read_write> args: array<u32>;
@group(1) @binding(2) var<storage, read_write> events: array<VfxEvent>;
@group(1) @binding(3) var<storage, read>       links: array<EventLink>;
// One vec4 per event slot: the child-ring cursor each link of the compacted event reserved
// (`NO_RESERVATION`: rolled back), written here and read by `child_emit`.
@group(1) @binding(4) var<storage, read_write> event_reservations: array<vec4<u32>>;
@group(1) @binding(5) var<storage, read_write> event_epoch: array<u32>;
// The fired-slot mask `fire_event` sets (one bit per pool slot, double-buffered by the event
// step's parity): this kernel scans the set bits in slot order instead of every rank of the
// window and clears each word it consumes, so a frame pays a scan proportional to the slots
// that fired since the last prepare (plus `highest / 32` word reads), not to the window.
@group(1) @binding(6) var<storage, read_write> event_mask: array<u32>;

// `layout::STATS_*` (this module's copies; COMMON carries the ones `fire_event` uses too).
const STATS_EVENT_CONSUME: u32 = 3u;
const STATS_EVENT_OVERFLOW: u32 = 5u;
const STATS_CHILD_CURSOR: u32 = 6u;
const STATS_CHILD_DROPPED: u32 = 8u;
const STATS_CHILD_FRAME: u32 = 9u;
// `layout::STATS_EVENT_SCAN`: fired-slot mask bits visited (a scan-cost diagnostic).
const STATS_EVENT_SCAN: u32 = 10u;
// Marks an event rank consumed (or never written): never a real step (steps start at 1).
const INVALID_EPOCH: u32 = 0xFFFFFFFFu;

@compute @workgroup_size(1)
fn event_prepare() {
    let event_cap = frame.events.z;
    let ring = frame.events.w;
    // This frame consumes last frame's staging: the current step's (a held frame re-runs with
    // the same step) and the previous step's (a normal frame stepped since the events fired).
    let step = frame.events.y;
    let prev = step - 1u;
    // Never stepped: the zero-initialized epoch buffer would read as staged garbage.
    if (step == 0u) {
        atomicStore(&stats[STATS_EVENT_CONSUME], 0u);
        atomicStore(&stats[STATS_CHILD_FRAME], 0u);
        atomicStore(&stats[STATS_CHILD_CURSOR], 0u);
        atomicStore(&stats[STATS_EVENT_HIGHEST], 0u);
        args[0u] = 0u;
        args[1u] = 1u;
        args[2u] = 1u;
        return;
    }
    // Fired events are staged at `slot_ordinal * EVENTS_PER_SLOT + n` and marked in the mask.
    // `highest` is one past the highest ordinal `fire_event` staged since the last prepare,
    // clamped to the mask's capacity (main ring + child ring) — never to this frame's
    // `pool_slots()`: the ordinals were taken against the window of the frame that fired them,
    // and a window that has shrunk since (its old spawns past `life + 0.25 s`) must not hide
    // the child-ring events staged at `old window + ring index` (rev-kimi-19 finding 2). Both
    // parity buffers are scanned (this step's — a held re-render staged ahead of this prepare
    // — and the previous step's). Each scanned word is cleared in both parities once its bits
    // are consumed: every staged rank of those slots has been compacted or counted lost by
    // then, so the next scan visits only the slots that fire after this prepare, never the
    // whole window prefix again (rev-kimi-19 finding 1). The epoch check on each rank still
    // keeps exactly one generation live.
    let words = event_mask_words();
    let highest = min(atomicLoad(&stats[STATS_EVENT_HIGHEST]), u32(frame.sim.x) + frame.events.w);
    atomicStore(&stats[STATS_EVENT_HIGHEST], 0u);
    let parity_cur = (step & 1u) * words;
    let parity_prev = (prev & 1u) * words;
    let word_count = (highest + 31u) / 32u;
    var out = 0u;
    var used = 0u;
    var dropped = 0u;
    var lost = 0u;
    var visited = 0u;
    for (var w = 0u; w < word_count; w++) {
        // The bits of this word below `highest` (all of them but in the last word); a bit at
        // or past it was not staged since the last prepare and is left alone.
        let below = highest - w * 32u;
        let scanned = select(0xFFFFFFFFu, (1u << below) - 1u, below < 32u);
        var bits = (event_mask[parity_prev + w] | event_mask[parity_cur + w]) & scanned;
        if (bits == 0u) { continue; }
        event_mask[parity_prev + w] = event_mask[parity_prev + w] & ~scanned;
        event_mask[parity_cur + w] = event_mask[parity_cur + w] & ~scanned;
        while (bits != 0u) {
            let b = ctz32(bits);
            bits = bits & (bits - 1u);
            let k = w * 32u + b;
            visited += 1u;
            // Ranks of every generation are scanned in slot order; the epoch on each rank
            // keeps exactly one generation live, so a slot fires its events once. Every bit
            // left in `bits` is below `highest` (`scanned`), so no `k < highest` test is needed.
            for (var j = 0u; j < EVENTS_PER_SLOT; j++) {
                let rank = k * EVENTS_PER_SLOT + j;
                let epoch = event_epoch[rank];
                // Epoch 0 is the never-written value. Steps start at 1, so a real stamp is
                // never 0. Matching `prev` when it is 0 keeps every cleared rank on the first
                // advancing frame (a zero payload is parent 0, trigger 0, and a trigger-0 link
                // then spawns). Clearing the mask does not cover this: the cleared ranks sit
                // beside a rank that fired, under the same mask bit. Stale ranks (older than
                // one held frame, or already compacted) never replay.
                if (epoch == 0u || (epoch != step && epoch != prev)) { continue; }
                // Past the budget the event is counted and lost, as the fire-time overflow
                // did before: it reserves no child-ring budget and is not replayed.
                if (out >= event_cap) {
                    lost += 1u;
                    event_epoch[rank] = INVALID_EPOCH;
                    continue;
                }
                let ev = events[rank];
                // Scalars avoid both a Function-local ArrayStride in SPIR-V and an FXC
                // dynamically indexed vector write that can force the event loop to unroll.
                var reserve0 = NO_RESERVATION;
                var reserve1 = NO_RESERVATION;
                var reserve2 = NO_RESERVATION;
                var reserve3 = NO_RESERVATION;
                let trigger = ev.trigger_parent & 0xFFu;
                let parent = ev.trigger_parent >> 8u;
                // The link table is one record per emitter. A parent outside it is not a
                // staged event; indexing `links` with it reads off the end of the buffer.
                if (parent >= MAX_EMITTERS) {
                    event_epoch[rank] = INVALID_EPOCH;
                    continue;
                }
                for (var l = 0u; l < LINKS_PER_EMITTER; l++) {
                    let link = links[parent * LINKS_PER_EMITTER + l];
                    if ((link.trigger_count & 0xFFu) != trigger) { continue; }
                    if (link.child_slot >= MAX_EMITTERS) {
                        dropped += link.trigger_count >> 8u;
                        continue;
                    }
                    // `events::link_fires`: all or nothing per firing, from the event's own
                    // hash.
                    let link_seed = hash_u32(ev.seed ^ ((l + 1u) * 2246822519u));
                    if (child_unit(link_seed, 12u) >= link.probability) { continue; }
                    let count = link.trigger_count >> 8u;
                    if (count == 0u) { continue; }
                    // One ring per frame, first fit in deterministic event/link order; a
                    // link that does not fit is rolled back instead of consuming the
                    // budget it asked for.
                    if (used + count > ring) {
                        dropped += count;
                        continue;
                    }
                    switch l {
                        case 0u: { reserve0 = used; }
                        case 1u: { reserve1 = used; }
                        case 2u: { reserve2 = used; }
                        default: { reserve3 = used; }
                    }
                    used += count;
                }
                // Compact in place: single thread, and `out <= rank` always. Invalidate
                // the staging rank so a later held frame with the same step cannot
                // consume it again.
                events[out] = ev;
                event_reservations[out] = vec4<u32>(reserve0, reserve1, reserve2, reserve3);
                event_epoch[rank] = INVALID_EPOCH;
                out++;
            }
        }
    }
    if (lost > 0u) { atomicAdd(&stats[STATS_EVENT_OVERFLOW], lost); }
    if (dropped > 0u) { atomicAdd(&stats[STATS_CHILD_DROPPED], dropped); }
    if (visited > 0u) { atomicAdd(&stats[STATS_EVENT_SCAN], visited); }
    atomicStore(&stats[STATS_EVENT_CONSUME], out);
    // The ring cursor persists across frames (a live child is overwritten only when the ring
    // wraps), so this frame's children land at `base + reservation`: reserve the frame's
    // total and publish the base for `child_emit`.
    let base = atomicAdd(&stats[STATS_CHILD_CURSOR], used);
    atomicStore(&stats[STATS_CHILD_FRAME], base);
    args[0u] = (out + 63u) / 64u;
    args[1u] = 1u;
    args[2u] = 1u;
}
"#;

/// Child emit: expands last frame's events into child particles at the ring slots
/// `event_prepare` reserved, in event order — the child layout is a pure function of the events
/// and the link table, not of thread scheduling. `events.rs` mirrors the sampling on the CPU,
/// hash for hash and bit for bit (`child_unit`, `sample_child`, and the `det_sin_cos` /
/// `det_cbrt` twins below). Group 0 is the frame uniform.
pub const EVENT_EMIT: &str = r#"
struct VfxEvent {
    pos: vec3<f32>,
    seed: u32,
    vel: vec3<f32>,
    trigger_parent: u32,
};

struct EventLink {
    trigger_count: u32,
    child_slot: u32,
    probability: f32,
    inherit: f32,
};

struct ChildInit {
    shape: vec4<f32>,
    shape2: vec4<f32>,
    velocity: vec4<f32>,
    velocity2: vec4<f32>,
    inherited: vec4<f32>,
    life_size: vec4<f32>,
    rot_spin: vec4<f32>,
    color: vec4<f32>,
    right: vec4<f32>,
    up: vec4<f32>,
    forward: vec4<f32>,
};

@group(1) @binding(0) var<storage, read>       events: array<VfxEvent>;
@group(1) @binding(1) var<storage, read>       links: array<EventLink>;
@group(1) @binding(2) var<storage, read>       child_inits: array<ChildInit>;
@group(1) @binding(3) var<storage, read_write> pos_age: array<vec4<f32>>;
@group(1) @binding(4) var<storage, read_write> vel_life: array<vec4<f32>>;
@group(1) @binding(5) var<storage, read_write> attrib: array<vec4<f32>>;
@group(1) @binding(6) var<storage, read_write> color0: array<vec4<f32>>;
@group(1) @binding(7) var<storage, read_write> stats: array<atomic<u32>>;
@group(1) @binding(8) var<storage, read>       event_reservations: array<vec4<u32>>;

const TAU: f32 = 6.283185307;
// `layout::STATS_*`.
const STATS_EVENT_CONSUME: u32 = 3u;
const STATS_CHILD_SPAWNED: u32 = 7u;
const STATS_CHILD_DROPPED: u32 = 8u;
// This frame's child-ring base, published by `event_prepare` (slot 9; the persistent cursor
// itself lives in slot 6).
const STATS_CHILD_FRAME_BASE: u32 = 9u;

// Deterministic sin/cos twin of `regia_core::math::det_sin_cos_f32`: the identical fixed
// sequence of plain f32 ops, so the GPU and the CPU mirror produce the same bits (WGSL `sin`
// and `cos` are not portable). Any edit must edit the Rust twin in the same commit.
const DET_FRAC_PI_2: f32 = 1.5707963267948966;
fn det_sin_cos(radians: f32) -> vec2<f32> {
    let q = radians / DET_FRAC_PI_2;
    let k = floor(q + 0.5);
    let x = (q - k) * DET_FRAC_PI_2;
    let x2 = x * x;
    var sin = 0.0;
    var cos = 0.0;
    for (var n = 7; n >= 0; n--) {
        let m = f32(2u * u32(n));
        cos = 1.0 - cos * x2 / ((m + 1.0) * (m + 2.0));
        sin = 1.0 - sin * x2 / ((m + 2.0) * (m + 3.0));
    }
    sin = x * sin;
    // `u32(k) % 4` equals `rem_euclid(4)` for every integral k (4 divides 2^32).
    let k4 = u32(i32(k)) % 4u;
    if (k4 == 1u) { return vec2<f32>(cos, -sin); }
    if (k4 == 2u) { return vec2<f32>(-sin, -cos); }
    if (k4 == 3u) { return vec2<f32>(-cos, sin); }
    return vec2<f32>(sin, cos);
}

// Deterministic cube-root twin of `regia_core::math::det_cbrt_f32` (fixed-count Newton in f32):
// the filled-sphere child radius the CPU mirror reproduces bit for bit. No WGSL `pow`.
// `y + y`, not `2.0 * y`: both are exact, but the mul-add shape of `2.0 * y + ...` is what a
// driver contracts into an fma (observed on lavapipe as a 1-ulp drift after the 48 steps,
// failing `gpu::determinism_tests::filled_sphere_child_matches_the_cpu_mirror_within_driver_ulp`),
// and WGSL's no-contraction rule is a driver courtesy, not a guarantee.
fn det_cbrt(u: f32) -> f32 {
    if (u <= 0.0) { return 0.0; }
    var y = 1.0;
    for (var i = 0; i < 48; i++) {
        y = (y + y + u / (y * y)) / 3.0;
    }
    return y;
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    return a + (b - a) * t;
}

// `events::sample_local`: the spawn offset in the instance frame.
fn sample_local(init: ChildInit, s: u32) -> vec3<f32> {
    let kind = u32(init.shape.x);
    if (kind == 1u) {
        let z = 2.0 * child_unit(s, 0u) - 1.0;
        let phi = TAU * child_unit(s, 1u);
        let flat_r = sqrt(max(1.0 - z * z, 0.0));
        let sc = det_sin_cos(phi);
        let dir = vec3<f32>(flat_r * sc.y, flat_r * sc.x, z);
        var r = lerp(init.shape.z, init.shape.w, child_unit(s, 2u));
        if (init.shape.y < 0.5) {
            r = r * det_cbrt(child_unit(s, 3u));
        }
        return dir * r;
    }
    if (kind == 2u) {
        let h = init.shape2.xyz;
        let p = vec3<f32>(
            (2.0 * child_unit(s, 0u) - 1.0) * h.x,
            (2.0 * child_unit(s, 1u) - 1.0) * h.y,
            (2.0 * child_unit(s, 2u) - 1.0) * h.z,
        );
        if (init.shape.y < 0.5) {
            return p;
        }
        let face = min(u32(child_unit(s, 3u) * 6.0), 5u);
        if (face == 0u) { return vec3<f32>(h.x, p.y, p.z); }
        if (face == 1u) { return vec3<f32>(-h.x, p.y, p.z); }
        if (face == 2u) { return vec3<f32>(p.x, h.y, p.z); }
        if (face == 3u) { return vec3<f32>(p.x, -h.y, p.z); }
        if (face == 4u) { return vec3<f32>(p.x, p.y, h.z); }
        return vec3<f32>(p.x, p.y, -h.z);
    }
    if (kind == 3u) {
        let l = lerp(init.shape.z, init.shape.w, child_unit(s, 0u));
        let r = l * init.shape2.w * sqrt(child_unit(s, 1u));
        let phi = TAU * child_unit(s, 2u);
        let sc = det_sin_cos(phi);
        return vec3<f32>(r * sc.y, r * sc.x, l);
    }
    return vec3<f32>(0.0);
}

// `events::sample_velocity`: the init module's velocity, before scaling.
fn sample_velocity(init: ChildInit, s: u32, local: vec3<f32>) -> vec3<f32> {
    let mode = u32(init.velocity.x);
    let speed = lerp(init.velocity.z, init.velocity.w, child_unit(s, 4u));
    if (mode == 1u) {
        var d = init.velocity2.xyz;
        let cos_spread = init.velocity.y;
        if (cos_spread < 1.0 && dot(d, d) > 1e-10) {
            let z = lerp(cos_spread, 1.0, child_unit(s, 5u));
            let phi = TAU * child_unit(s, 6u);
            let r = sqrt(max(1.0 - z * z, 0.0));
            let sc = det_sin_cos(phi);
            let basis = select(vec3<f32>(0.0, 0.0, 1.0), vec3<f32>(0.0, 1.0, 0.0), abs(d.z) > 0.999);
            let n = normalize(d);
            let side = normalize(cross(n, basis));
            let up = cross(side, n);
            d = side * (r * sc.y) + up * (r * sc.x) + n * z;
        }
        return d * speed;
    }
    if (mode == 2u) {
        let flat_dir = init.right.xyz * local.x + init.up.xyz * local.y;
        var d = flat_dir;
        if (dot(flat_dir, flat_dir) > 1e-10) {
            d = normalize(flat_dir);
        } else {
            let phi = TAU * child_unit(s, 5u);
            let sc = det_sin_cos(phi);
            d = init.right.xyz * sc.y + init.up.xyz * sc.x;
        }
        let rise = lerp(init.velocity2.x, init.velocity2.y, child_unit(s, 6u));
        return d * speed + init.forward.xyz * rise;
    }
    // Radial keeps its v1 meaning: the unrotated local offset (ADR-1398, `sim::sample_velocity`).
    if (dot(local, local) > 1e-10) {
        return normalize(local) * speed;
    }
    return init.forward.xyz * speed;
}

fn spawn_child(ev: VfxEvent, link: EventLink, seed: u32, slot: u32) {
    let init = child_inits[link.child_slot];
    let local = sample_local(init, seed);
    let scale = init.right.w;
    let speed_scale = init.up.w;
    let offset = init.right.xyz * local.x + init.up.xyz * local.y + init.forward.xyz * local.z;
    // The event was fired last frame: on a rebase frame its position is still in the old space.
    let position = ev.pos - frame.shift.xyz + offset * scale;
    let velocity = (sample_velocity(init, seed, local) + init.inherited.xyz) * speed_scale + ev.vel * link.inherit;
    let life = max(lerp(init.life_size.x, init.life_size.y, child_unit(seed, 7u)), 0.01);
    // `events::stamp_identity`: the low bits of the lifetime carry the child's identity.
    let id = hash_u32(seed ^ 0x51ED270Bu) & 0x3FFu;
    let stamped = bitcast<f32>((bitcast<u32>(life) & 0xFFFFFC00u) | id);
    let size = max(lerp(init.life_size.z, init.life_size.w, child_unit(seed, 8u)), 0.001);
    let rotation = lerp(init.rot_spin.x, init.rot_spin.y, child_unit(seed, 9u));
    let spin = lerp(init.rot_spin.z, init.rot_spin.w, child_unit(seed, 10u));
    let phase = init.inherited.w * clamp(child_unit(seed, 11u), 0.0, 0.999);
    if (pos_age[slot].w < vel_life[slot].w) {
        atomicAdd(&stats[STATS_CHILD_DROPPED], 1u);
    }
    pos_age[slot] = vec4<f32>(position, 0.0);
    vel_life[slot] = vec4<f32>(velocity, stamped);
    attrib[slot] = vec4<f32>(size, rotation, spin, f32(link.child_slot) + phase);
    color0[slot] = init.color;
}

@compute @workgroup_size(64)
fn child_emit(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= atomicLoad(&stats[STATS_EVENT_CONSUME])) { return; }
    let ev = events[gid.x];
    // The budget `event_prepare` reserved for this event, in deterministic event/link order,
    // on top of the ring cursor base it published: no atomics decide what spawns or where it
    // lands.
    let reserve = event_reservations[gid.x];
    let base = atomicLoad(&stats[STATS_CHILD_FRAME_BASE]);
    let parent = ev.trigger_parent >> 8u;
    if (parent >= MAX_EMITTERS) { return; }
    let capacity = u32(frame.sim.x);
    let ring = frame.events.w;
    for (var l = 0u; l < LINKS_PER_EMITTER; l = l + 1u) {
        let first = reserve[l];
        if (first == NO_RESERVATION) { continue; }
        let link = links[parent * LINKS_PER_EMITTER + l];
        // `child_inits` is one record per emitter. A slot outside it is not spawned.
        if (link.child_slot >= MAX_EMITTERS) {
            atomicAdd(&stats[STATS_CHILD_DROPPED], link.trigger_count >> 8u);
            continue;
        }
        let count = link.trigger_count >> 8u;
        atomicAdd(&stats[STATS_CHILD_SPAWNED], count);
        for (var c = 0u; c < count; c = c + 1u) {
            let seed = hash_u32(hash_u32(ev.seed ^ ((l + 1u) * 2246822519u)) ^ ((c + 1u) * 3266489917u));
            spawn_child(ev, link, seed, capacity + (base + first + c) % ring);
        }
    }
}
"#;

/// Sort module: sentinel padding for the alpha list, then Batcher bitonic merge steps driven by a
/// dynamic-offset uniform (`k_j_base_n` = `(k, j, region_base, n)`).
pub const SORT: &str = r#"
@group(0) @binding(0) var<storage, read_write> lists: array<vec2<u32>>;
@group(0) @binding(1) var<storage, read_write> counters: array<atomic<u32>>;

struct SortParams {
    k_j_base_n: vec4<u32>,
};
@group(1) @binding(0) var<uniform> p: SortParams;

// The sort order: by key, then by payload (the pool slot, unique per live particle). A total
// order, so the result does not depend on the order the update kernel gathered the list in
// (atomic appends), and equal keys cannot swap from frame to frame.
fn pair_greater(a: vec2<u32>, b: vec2<u32>) -> bool {
    return a.x > b.x || (a.x == b.x && a.y > b.y);
}

// Pads one list region past its live count. The pad block is `(0, counter index, region
// base, region size)`: the alpha list (counter 1) or the mesh list (counter 2).
@compute @workgroup_size(64)
fn pad_alpha(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    let base = p.k_j_base_n.z;
    if (i >= p.k_j_base_n.w) { return; }
    let count = atomicLoad(&counters[p.k_j_base_n.y]);
    if (i >= count) {
        // Above every live pair, even a key of 0xFFFFFFFF (a particle within 2 cm of the eye).
        lists[base + i] = vec2<u32>(0xFFFFFFFFu, 0xFFFFFFFFu);
    }
}

// Shared-memory bitonic stages (the same network as `sort_step`, fewer dispatches): each
// workgroup loads a 512-key tile and runs every compare-exchange whose distance j < 512 inside
// it. `k == 0`: the whole sort of each tile (k = 2..512); `k >= 1024`: the j = 256..1 tail of
// stage k after its global j >= 512 steps.
const SORT_TILE: u32 = 512u;
var<workgroup> tile: array<vec2<u32>, 512>;

fn tile_exchange(t: u32, k: u32, j: u32, block: u32) {
    let i = 2u * j * (t / j) + (t % j);
    let partner = i + j;
    let ascending = ((block + i) & k) == 0u;
    let a = tile[i];
    let b = tile[partner];
    if (pair_greater(a, b) == ascending) {
        tile[i] = b;
        tile[partner] = a;
    }
}

@compute @workgroup_size(256)
fn sort_local(@builtin(local_invocation_id) lid: vec3<u32>, @builtin(workgroup_id) wid: vec3<u32>) {
    let base = p.k_j_base_n.z;
    let stage = p.k_j_base_n.x;
    let block = wid.x * SORT_TILE;
    let t = lid.x;
    tile[t] = lists[base + block + t];
    tile[t + 256u] = lists[base + block + t + 256u];
    workgroupBarrier();
    if (stage == 0u) {
        for (var k = 2u; k <= SORT_TILE; k = k << 1u) {
            for (var j = k >> 1u; j > 0u; j = j >> 1u) {
                tile_exchange(t, k, j, block);
                workgroupBarrier();
            }
        }
    } else {
        for (var j = SORT_TILE >> 1u; j > 0u; j = j >> 1u) {
            tile_exchange(t, stage, j, block);
            workgroupBarrier();
        }
    }
    lists[base + block + t] = tile[t];
    lists[base + block + t + 256u] = tile[t + 256u];
}

@compute @workgroup_size(64)
fn sort_step(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    let k = p.k_j_base_n.x;
    let j = p.k_j_base_n.y;
    let base = p.k_j_base_n.z;
    let n = p.k_j_base_n.w;
    if (i >= n) { return; }
    let partner = i ^ j;
    if (partner <= i || partner >= n) { return; }
    let ascending = (i & k) == 0u;
    let a = lists[base + i];
    let b = lists[base + partner];
    var lo = a;
    var hi = b;
    if (pair_greater(a, b)) { lo = b; hi = a; }
    if (ascending) {
        lists[base + i] = lo;
        lists[base + partner] = hi;
    } else {
        lists[base + i] = hi;
        lists[base + partner] = lo;
    }
}
"#;

/// Indirect-argument preparation: instance counts from the atomic list counters.
pub const PREPARE: &str = r#"
@group(0) @binding(0) var<storage, read_write> counters: array<atomic<u32>>;
@group(0) @binding(1) var<storage, read_write> indirect: array<vec4<u32>>;

@compute @workgroup_size(1)
fn prepare_indirect() {
    indirect[0u] = vec4<u32>(6u, atomicLoad(&counters[0u]), 0u, 0u);
    indirect[1u] = vec4<u32>(6u, atomicLoad(&counters[1u]), 0u, 0u);
    indirect[2u] = vec4<u32>(36u, atomicLoad(&counters[2u]), 0u, 0u);
}
"#;

/// Bindings shared by the billboard and mesh-particle render modules.
pub const PARTICLE_RENDER_BINDINGS: &str = r#"
struct Emitter {
    gravity_drag: vec4<f32>,
    curl: vec4<f32>,
    vortex: vec4<f32>,
    attractor: vec4<f32>,
    kill: vec4<f32>,
    curve_ids: vec4<u32>,
    render: vec4<f32>,
    flip: vec4<f32>,
    stretch: vec4<f32>,
    origin: vec4<f32>,
    quality: vec4<f32>,
    wind: vec4<f32>,
    events: vec4<u32>,
    kill_volume: vec4<f32>,
    kill_axis_x: vec4<f32>,
    kill_axis_y: vec4<f32>,
    kill_axis_z: vec4<f32>,
};

@group(1) @binding(0) var<storage, read> pos_age: array<vec4<f32>>;
@group(1) @binding(1) var<storage, read> vel_life: array<vec4<f32>>;
@group(1) @binding(2) var<storage, read> attrib: array<vec4<f32>>;
@group(1) @binding(3) var<storage, read> color0: array<vec4<f32>>;
@group(1) @binding(4) var<storage, read> emitters: array<Emitter>;
@group(1) @binding(5) var<storage, read> lists: array<vec2<u32>>;
@group(1) @binding(6) var<storage, read> curve_scalars: array<f32>;
@group(1) @binding(7) var<storage, read> curve_colors: array<vec4<f32>>;
@group(1) @binding(8) var atlas: texture_2d<f32>;
@group(1) @binding(9) var atlas_sampler: sampler;
@group(1) @binding(10) var depth_tex: texture_depth_2d;
@group(1) @binding(11) var depth_sampler: sampler;
// Scene colour after decals, before particles (heat haze); a dummy when haze is off.
@group(1) @binding(12) var scene_tex: texture_2d<f32>;
// The 256 px fireball sheet (`atlas::FIREBALL_*`), mipmapped, 4× anisotropic.
@group(1) @binding(13) var fireball_tex: texture_2d<f32>;
@group(1) @binding(14) var fireball_sampler: sampler;

fn particle_of(list: u32, instance: u32) -> u32 {
    return lists[list + instance].y;
}
"#;

/// Billboard and stretched-billboard rendering. `list_base` is a pipeline override constant.
pub const BILLBOARD: &str = r#"
override list_base: u32 = 0u;
// 1: this pipeline draws only heat-haze particles (before every other list); 0: it skips them.
override haze_pass: u32 = 0u;

struct VsOut {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec4<f32>,
    @location(1) tile_scale: vec4<f32>, // xy tile origin uv, zw tile scale
    @location(2) local_uv: vec2<f32>,
    @location(3) soft_depth: vec4<f32>, // softness (m), view depth, sphere radius (m; 0 = flat), rotation (rad)
    @location(4) tile_b: vec4<f32>,     // second flipbook frame
    @location(5) shade: vec4<f32>,      // blend, lit, emissive (haze px on a haze card), self-shadow
    @location(6) world: vec3<f32>,
    @location(7) @interpolate(flat) sheet: u32, // 1: the fireball sheet, 0: the 64 px atlas
};

// Black-body chromaticity in linear sRGB, normalised to its largest channel, for 900–3500 K
// (Planck's law integrated against the CIE 1931 2° matching functions in the analytic form of
// Wyman, Sloan & Shirley, JCGT 2013, then XYZ → linear sRGB, IEC 61966-2-1). Piecewise
// linear between the computed stops; `quality::blackbody_rgb` is the same table.
fn blackbody_rgb(t: f32) -> vec3<f32> {
    if (t < 1100.0) {
        return mix(vec3<f32>(1.0, 0.0089, 0.0), vec3<f32>(1.0, 0.0503, 0.0), clamp((t - 900.0) / 200.0, 0.0, 1.0));
    }
    if (t < 1300.0) {
        return mix(vec3<f32>(1.0, 0.0503, 0.0), vec3<f32>(1.0, 0.0968, 0.0), (t - 1100.0) / 200.0);
    }
    if (t < 1500.0) {
        return mix(vec3<f32>(1.0, 0.0968, 0.0), vec3<f32>(1.0, 0.1452, 0.0), (t - 1300.0) / 200.0);
    }
    if (t < 1800.0) {
        return mix(vec3<f32>(1.0, 0.1452, 0.0), vec3<f32>(1.0, 0.2184, 0.0), (t - 1500.0) / 300.0);
    }
    if (t < 2200.0) {
        return mix(vec3<f32>(1.0, 0.2184, 0.0), vec3<f32>(1.0, 0.3131, 0.0281), (t - 1800.0) / 400.0);
    }
    if (t < 2700.0) {
        return mix(vec3<f32>(1.0, 0.3131, 0.0281), vec3<f32>(1.0, 0.4231, 0.0996), (t - 2200.0) / 500.0);
    }
    return mix(vec3<f32>(1.0, 0.4231, 0.0996), vec3<f32>(1.0, 0.5768, 0.2605), clamp((t - 2700.0) / 800.0, 0.0, 1.0));
}

// Optical depth of a spherical billboard per radius (`quality::SPHERE_DENSITY`).
const SPHERE_DENSITY: f32 = 3.0;

// A particle light reaching this far (metres) is a flash or fireball with a sphere source of
// `radius / FLASH_RADIUS_PER_SOURCE` (`runtime::FLASH_MIN_RADIUS_M`, `FLASH_RADIUS_PER_SOURCE`).
const FLASH_MIN_RADIUS_M: f32 = 400.0;
const FLASH_RADIUS_PER_SOURCE: f32 = 80.0;

// Squared sphere-source radius of the frame's fire light of influence `radius` (0: a point).
fn fire_source_sq(radius: f32) -> f32 {
    let s = select(0.0, radius / FLASH_RADIUS_PER_SOURCE, radius >= FLASH_MIN_RADIUS_M);
    return s * s;
}

// Black-body fire on HDR frames: brightness = gain × heat^power (the sheet's luminance is the
// heat). Gain 1.7 keeps a tongue (heat 0.6) where heat^1.4 had it; the core gets 1.5×.
const FIRE_HEAT_POWER: f32 = 2.4;
const FIRE_CORE_GAIN: f32 = 1.7;
// Sheet heat (luminance) from soot to flame: the cool rim and soot folds (ramp temperature
// below ~0.1) are soot, a tongue (0.38 and up) is flame.
const SOOT_HEAT: vec2<f32> = vec2<f32>(0.1, 0.32);
// Lit puffs' forward scattering: Henyey–Greenstein asymmetry (smoke aerosols scatter
// forward, g about 0.6) and the share of the fringe's light that scattering carries.
const SMOKE_HG_G: f32 = 0.6;
// Chords of extinction toward the sun the neighbours of a dense puff add at self-shadow 1
// (`quality::NEIGHBOUR_DEPTH`).
const NEIGHBOUR_DEPTH: f32 = 6.0;
// How much of the light's chroma a bright aerosol adapts away: `CHROMA_ADAPT × albedo²`
// (`quality::CHROMA_ADAPT`).
const CHROMA_ADAPT: f32 = 1.5;
const FORWARD_SCATTER: f32 = 0.3;
// How strongly a lit puff's sprite density bends its normal (0 = the smooth sphere).
const BILLOW_NORMAL: f32 = 0.35;
// Soot albedo (carbon black; aged organic smoke is grey, not brown: ADR-1444).
const SOOT_ALBEDO: vec3<f32> = vec3<f32>(0.04, 0.04, 0.04);

const ATLAS_COLS: u32 = 8u;
const ATLAS_ROWS: u32 = 4u;
// Tile indices from here address the fireball sheet (`atlas::FIREBALL_ANIM_TILE`).
const FIREBALL_TILE0: u32 = 32u;
const FIREBALL_FRAMES: u32 = 8u;

fn tile_rect(tile: u32) -> vec4<f32> {
    if (tile >= FIREBALL_TILE0) {
        let f = f32(min(tile - FIREBALL_TILE0, FIREBALL_FRAMES - 1u));
        return vec4<f32>(f / f32(FIREBALL_FRAMES), 0.0, 1.0 / f32(FIREBALL_FRAMES), 1.0);
    }
    let col = f32(tile % ATLAS_COLS);
    let row = f32(min(tile / ATLAS_COLS, ATLAS_ROWS - 1u));
    return vec4<f32>(col / f32(ATLAS_COLS), row / f32(ATLAS_ROWS), 1.0 / f32(ATLAS_COLS), 1.0 / f32(ATLAS_ROWS));
}

// (-1,-1),(1,-1),(-1,1), (-1,1),(1,-1),(1,1). Selects, not a function-local array (ADR-026).
// An 18-vertex octagon that drops the empty corners was tried for the 100k smoke overdraw:
// lavapipe drew it ~20 % slower (more, thinner triangles), so the quad stays and the empty
// texels are discarded in the fragment shader instead.
fn billboard_corner(i: u32) -> vec2<f32> {
    let x = select(-1.0, 1.0, i == 1u || i == 4u || i == 5u);
    let y = select(-1.0, 1.0, i == 2u || i == 3u || i == 5u);
    return vec2<f32>(x, y);
}

@vertex
fn vs_billboard(
    @builtin(vertex_index) vi: u32,
    @builtin(instance_index) ii: u32,
) -> VsOut {
    let entry = lists[list_base + ii];
    let p = entry.y;
    let pa = pos_age[p];
    let vl = vel_life[p];
    let at = attrib[p];
    let c0 = color0[p];
    let e = emitters[u32(at.w)];
    let t = clamp(pa.w / max(vl.w, 1e-5), 0.0, 1.0);
    // Heat-haze cards draw only in the haze pipeline; everything else skips it. A skipped
    // particle collapses to a point outside the clip volume.
    let is_haze = e.origin.w > 0.0;
    if (is_haze != (haze_pass == 1u)) {
        // A constructor, not a function-local `var` of the struct (ADR-026).
        let z4 = vec4<f32>(0.0);
        return VsOut(vec4<f32>(0.0, 0.0, -2.0, 1.0), z4, z4, vec2<f32>(0.0), z4, z4, z4, vec3<f32>(0.0), 0u);
    }
    // Low-resolution soft particles (`VfxProfile::soft_particle_res`): in mode 1 this full-res
    // pass skips the soft emitters (dust, smoke, billows; drawn into the low-res layer), in
    // mode 2 the low-res pass draws only them. Mode 0 draws everything.
    let soft_mode = u32(frame.fire_color.w) >> 1u;
    let is_soft = e.curl.w > 0.5;
    if ((soft_mode == 1u && is_soft) || (soft_mode == 2u && !is_soft)) {
        let z4 = vec4<f32>(0.0);
        return VsOut(vec4<f32>(0.0, 0.0, -2.0, 1.0), z4, z4, vec2<f32>(0.0), z4, z4, z4, vec3<f32>(0.0), 0u);
    }

    var size = at.x * lut_scalar(e.curve_ids.x, t);
    let alpha = c0.a * lut_scalar(e.curve_ids.z, t) * max(e.render.z, 0.0);
    let rgb = c0.rgb * lut_color(e.curve_ids.y, t).rgb;
    let rot = at.y + at.z * pa.w + lut_scalar(e.curve_ids.w, t);

    // Screen-size floor. `quality::apply_screen_floor`, cap 16×. Emitter floor wins over the profile.
    let centre_view = frame.view * vec4<f32>(pa.xyz, 1.0);
    let centre_depth = max(-centre_view.z, 1e-3);
    let floor_px = select(frame.sim.z, e.quality.w, e.quality.w > 0.0);
    let px = size * frame.light1.w / centre_depth;
    if (floor_px > 0.0 && px > 1e-6 && px < floor_px) {
        size = size * min(floor_px / px, 16.0);
    }

    let corner = billboard_corner(vi % 6u);
    // Spark glyphs are long along u. A stretched quad is long along corner.y, so swap
    // the axes and the ribbon follows velocity instead of sitting across it.
    var uv_corner = corner;
    var world = pa.xyz;
    if (u32(e.render.x) == 1u) {
        uv_corner = vec2<f32>(corner.y, corner.x);
        // stretched billboard: long axis along velocity, width camera-facing
        let speed = length(vl.xyz);
        let dir = select(vec3<f32>(0.0, 1.0, 0.0), vl.xyz / max(speed, 1e-5), speed > 1e-3);
        let to_cam = frame.cam_pos_near.xyz - pa.xyz;
        let view_depth = max(length(to_cam), 1e-3);
        let side = normalize(cross(dir, to_cam) + vec3<f32>(1e-5));
        // Quad length = velocity_scale * speed (total), clamped to a fraction of the
        // view depth: a streak longer than its distance to the camera would cross
        // the near plane and smear the quad across the whole frame (fresh, fast
        // particles near the camera). Half-length must stay in front of the camera.
        // A negative minimum is a multiple of the particle's current size: a stretched puff
        // is round at rest and streaks along its velocity when fast.
        let min_len = select(e.stretch.y, -e.stretch.y * size, e.stretch.y < 0.0);
        let along = min(max(e.stretch.x * speed, min_len), view_depth * 0.9);
        world = world + dir * (along * 0.5 * corner.y) + side * (size * 0.5 * corner.x);
    } else {
        let cs = cos(rot);
        let sn = sin(rot);
        let q = vec2<f32>(corner.x * cs - corner.y * sn, corner.x * sn + corner.y * cs);
        world = world + (frame.cam_right_dt.xyz * q.x + frame.cam_up_time.xyz * q.y) * size * 0.5;
    }
    // Spherical billboards: the quad moves to the sphere's front, scaled about the eye so it
    // covers exactly the same pixels, and the depth test then admits every pixel the fade
    // keeps (a quad at the centre depth would be cut hard where the ground passes it).
    var sphere_r = 0.0;
    if ((u32(frame.fire_color.w) & 1u) == 1u && u32(e.render.x) != 1u) {
        sphere_r = size * 0.5;
        let pull = clamp(sphere_r, 0.0, max(centre_depth - 2.0 * frame.cam_pos_near.w, 0.0));
        let k = (centre_depth - pull) / centre_depth;
        world = frame.cam_pos_near.xyz + (world - frame.cam_pos_near.xyz) * k;
    }
    let position = frame.view_proj * vec4<f32>(world, 1.0);

    // Flipbook cursor matches `quality::flipbook_cursor`. Phase is the fractional emitter id
    // and is used only when the emitter asked for a random start (`flip.w`).
    let base = u32(e.render.y);
    let frames = max(u32(e.flip.y), 1u);
    var frame_i = 0u;
    var frame_n = 0u;
    var blend = 0.0;
    if (frames > 1u) {
        let n = f32(frames);
        let phase = select(0.0, fract(at.w), e.flip.w > 0.5);
        let cursor = select(t * n, max(pa.w, 0.0) * e.flip.z, e.flip.z > 0.0) + phase * n;
        let wrapped = cursor - floor(cursor / n) * n;
        frame_i = min(u32(wrapped), frames - 1u);
        let fr = clamp(wrapped - f32(frame_i), 0.0, 0.999);
        frame_n = (frame_i + 1u) % frames;
        if (e.stretch.w > 0.5) {
            blend = fr;
        } else {
            frame_n = frame_i;
        }
    }
    let view_p = frame.view * vec4<f32>(world, 1.0);
    // Atlas UVs are the 8×4 grid (`tile % 8`, `tile / 8`), not the flipbook's own
    // cols×rows: several sheets share one texture. `tile_rect` matches `atlas::TILES_X`.
    return VsOut(
        position,
        vec4<f32>(rgb, alpha),
        tile_rect(base + frame_i),
        uv_corner * 0.5 + vec2<f32>(0.5),
        // Camera-facing quads are spheres of half their size; stretched streaks stay flat.
        // w: the sprite's rotation, so lit puffs turn their sprite-space normal to the screen.
        vec4<f32>(e.kill.w, select(-view_p.z, centre_depth, sphere_r > 0.0), sphere_r, select(rot, 0.0, u32(e.render.x) == 1u)),
        tile_rect(base + frame_n),
        vec4<f32>(blend, e.quality.y, select(e.quality.x, e.origin.w, is_haze), e.quality.z),
        world,
        select(0u, 1u, base >= FIREBALL_TILE0),
    );
}

// Refraction card (heat haze): bends the scene copy behind the card by rising value noise.
// The copy holds the frame after decals and before particles, so fire and smoke drawn
// next stay on top. A screen-space offset of the background, not a physical refraction
// (the usual real-time approximation; see `docs/provenance.md`).
fn haze(in: VsOut, mask: f32) -> vec4<f32> {
    let px = in.position.xy;
    let t = frame.cam_up_time.w;
    let q = vec3<f32>(in.local_uv * 3.0 + vec2<f32>(37.0, 11.0 + t * 1.7), t * 0.6 + 5.0);
    let n = vec2<f32>(value_noise(q, 7u), value_noise(q + vec3<f32>(19.0, 7.0, 3.0), 8u)) - vec2<f32>(0.5);
    let offset = n * 2.0 * in.shade.z * frame.decal.w * mask;
    let uv = clamp((px + offset) * frame.viewport.zw, vec2<f32>(0.0), vec2<f32>(1.0));
    let bent = textureSampleLevel(scene_tex, atlas_sampler, uv, 0.0).rgb;
    return vec4<f32>(bent * mask, mask);
}

@fragment
fn fs_billboard(in: VsOut) -> @location(0) vec4<f32> {
    let on_sheet = in.sheet == 1u;
    // Half a texel of the tile in use stops bleeding into the neighbour.
    let inset = select(0.5 / 64.0, 0.5 / 256.0, on_sheet);
    // local_uv.y grows up the screen; atlas rows grow down the image, so the tile's top
    // row (v = −1 in `atlas.rs`, where flame tips and tongues are drawn) maps to the top.
    let local = clamp(vec2<f32>(in.local_uv.x, 1.0 - in.local_uv.y), vec2<f32>(inset), vec2<f32>(1.0 - inset));
    let uv = in.tile_scale.xy + local * in.tile_scale.zw;
    let uv_b = in.tile_b.xy + local * in.tile_b.zw;
    // Gradients in uniform control flow, then explicit-gradient samples in the branch.
    let gx = dpdx(uv);
    let gy = dpdy(uv);
    var tex_a: vec4<f32>;
    var tex_b: vec4<f32>;
    if (on_sheet) {
        // +0.5 mip bias: a far fireball card takes a smaller level (cheaper, no shimmer).
        let k = 1.41421356;
        tex_a = textureSampleGrad(fireball_tex, fireball_sampler, uv, gx * k, gy * k);
        tex_b = textureSampleGrad(fireball_tex, fireball_sampler, uv_b, gx * k, gy * k);
    } else {
        // The 64 px atlas has one level: an explicit level 0 is the same texel as an implicit
        // sample, at the full-rate sample instruction (explicit gradients are slower on RDNA).
        tex_a = textureSampleLevel(atlas, atlas_sampler, uv, 0.0);
        tex_b = textureSampleLevel(atlas, atlas_sampler, uv_b, 0.0);
    }
    let tex = mix(tex_a, tex_b, clamp(in.shade.x, 0.0, 1.0));
    var alpha = in.color.a * tex.a;
    // Empty texels (a sprite's clear corners and fringe, about a third of a puff quad)
    // contribute exactly nothing in both blend modes: skip the depth fetch, the lighting
    // and the blend. Haze cards bend the scene everywhere their mask is not zero.
    if (haze_pass == 0u && alpha <= 0.0) {
        discard;
    }
    if (haze_pass == 1u) {
        // Haze needs a bound post-decal scene copy (`frame.decal.z`).
        if (frame.decal.z < 0.5) {
            discard;
        }
        return haze(in, clamp(alpha, 0.0, 1.0));
    }
    let spherical = in.soft_depth.z > 0.0;
    if (spherical) {
        // Spherical billboard (Umenhoffer, Szirmay-Kalos & Szijártó, "Spherical Billboards
        // and their Application to Rendering Explosions", GI 2006): the puff is a sphere of
        // half the quad's size; its opacity is the share of this pixel's chord that lies in
        // front of the scene, so ground and walls cut it gradually over its own depth. The
        // flat soft fade still applies, its distance at least the puff's radius, so the
        // contact softens with the puff's size. Matches `quality::spherical_billboard_fade`.
        let screen_uv = in.position.xy * frame.viewport.zw;
        let scene = textureSampleLevel(depth_tex, depth_sampler, screen_uv, 0i);
        let surface = world_from_depth(screen_uv, scene);
        let q = in.local_uv * 2.0 - vec2<f32>(1.0);
        let half_chord = in.soft_depth.z * sqrt(max(0.0, 1.0 - dot(q, q)));
        let front = max(in.soft_depth.y - half_chord, frame.cam_pos_near.w);
        let back = in.soft_depth.y + half_chord;
        // Near the rim the chord vanishes: fade over the authored softness or a quarter radius.
        let span = max(back - front, max(in.soft_depth.x, 0.25 * in.soft_depth.z));
        // Dense medium (their 1 − e^(−τd)): opacity saturates within a fraction of the radius,
        // so only the last part of the chord before a surface thins the puff, normalised so a
        // free chord is fully opaque.
        let tau = SPHERE_DENSITY / max(in.soft_depth.z, 1e-4);
        let seen = clamp(surface.w - front, 0.0, span);
        let xs = (1.0 - exp(-tau * seen)) / max(1.0 - exp(-tau * span), 1e-4);
        let soft = max(in.soft_depth.x, in.soft_depth.z);
        let xf = clamp((surface.w - in.soft_depth.y) / soft, 0.0, 1.0);
        let x = max(xs, xf * xf * (3.0 - 2.0 * xf));
        alpha = alpha * x;
    } else if (in.soft_depth.x > 0.0) {
        let screen_uv = in.position.xy * frame.viewport.zw;
        let scene = textureSampleLevel(depth_tex, depth_sampler, screen_uv, 0i);
        let surface = world_from_depth(screen_uv, scene);
        // Hermite smoothstep of Lorach's fade. Matches `quality::soft_particle_fade`.
        let x = clamp((surface.w - in.soft_depth.y) / max(in.soft_depth.x, 1e-4), 0.0, 1.0);
        alpha = alpha * (x * x * (3.0 - 2.0 * x));
    }
    if (in.shade.y > 0.5 && !on_sheet) {
        // Noise-eroded rim (ADR-1444): a lit puff's thin edge is broken up by two octaves of
        // value noise seeded by the particle's own rotation, so overlapping puffs merge into
        // one ragged mass instead of reading as separate round sprites. The dense core
        // (alpha near 1) is untouched; the erosion fades in below alpha 0.6.
        let eq = (in.local_uv * 2.0 - vec2<f32>(1.0)) * 4.0 + vec2<f32>(in.soft_depth.w * 7.3, in.soft_depth.w * 3.1);
        let en = 0.65 * value_noise(vec3<f32>(eq, 1.7), 11u) + 0.35 * value_noise(vec3<f32>(eq * 2.3, 4.1), 12u);
        let rim = 1.0 - smoothstep(0.0, 0.6, clamp(alpha / max(in.color.a, 1e-3), 0.0, 1.0));
        alpha = alpha * mix(1.0, smoothstep(0.15, 0.75, en + 0.2), rim * 0.8);
    }
    var rgb = in.color.rgb;
    if (in.shade.y > 0.5) {
        // Spherical billboard normal (Harris, GDC 2002) plus a top-lit hemispheric term, bent
        // by the sprite's own density: its gradient (central differences two texels apart,
        // per unit of the sphere's radius) tilts the normal away from the thicker side, so
        // each billow inside a puff is lit on its sun side and shaded on the other (a rolling
        // surface, not one smooth ball). The sprite's rotation (`soft_depth.w`) turns the
        // sprite-space normal to the screen, so every puff is lit from the same side.
        let p = in.local_uv * 2.0 - vec2<f32>(1.0);
        let nz = sqrt(max(0.0, 1.0 - dot(p, p)));
        let h = 2.0 / 64.0;
        let ax0 = textureSampleLevel(atlas, atlas_sampler, in.tile_scale.xy + clamp(local - vec2<f32>(h, 0.0), vec2<f32>(inset), vec2<f32>(1.0 - inset)) * in.tile_scale.zw, 0.0).a;
        let ax1 = textureSampleLevel(atlas, atlas_sampler, in.tile_scale.xy + clamp(local + vec2<f32>(h, 0.0), vec2<f32>(inset), vec2<f32>(1.0 - inset)) * in.tile_scale.zw, 0.0).a;
        let ay0 = textureSampleLevel(atlas, atlas_sampler, in.tile_scale.xy + clamp(local - vec2<f32>(0.0, h), vec2<f32>(inset), vec2<f32>(1.0 - inset)) * in.tile_scale.zw, 0.0).a;
        let ay1 = textureSampleLevel(atlas, atlas_sampler, in.tile_scale.xy + clamp(local + vec2<f32>(0.0, h), vec2<f32>(inset), vec2<f32>(1.0 - inset)) * in.tile_scale.zw, 0.0).a;
        // Atlas rows grow down while local_uv.y grows up; p = 2 local_uv − 1.
        let grad = vec2<f32>(ax1 - ax0, ay0 - ay1) / (2.0 * h) * 0.5;
        let bent = p - clamp(grad * BILLOW_NORMAL, vec2<f32>(-1.0), vec2<f32>(1.0));
        let cs = cos(in.soft_depth.w);
        let sn = sin(in.soft_depth.w);
        let n = normalize(vec3<f32>(bent.x * cs - bent.y * sn, bent.x * sn + bent.y * cs, max(nz, 0.05)));
        let sun_view = (frame.view * vec4<f32>(frame.light0.xyz, 0.0)).xyz;
        let ndotl = max(dot(n, normalize(sun_view + vec3<f32>(1e-6))), 0.0);
        let upw = clamp(n.y * 0.5 + 0.5, 0.0, 1.0);
        let shadow = clamp(in.shade.w, 0.0, 1.0);
        let hemi = (1.0 - shadow) + shadow * upw;
        let amb = clamp(frame.light0.w, 0.0, 1.0);
        let lambert = amb + (1.0 - amb) * ndotl;
        var light = frame.light1.rgb * lambert * hemi;
        var sun_fwd = frame.light1.rgb;
        if (frame.sky_up.w > 0.5) {
            // Under a sky (ADR-1425, `quality::puff_shade_volumetric`): the puff is a sphere of
            // participating medium. This pixel's opacity gives its extinction over the view
            // chord (2 n_z radii); the sun reaches the point under the pixel through the chord
            // toward the sun, so the sun side keeps a bright rind and the core goes dark. The
            // transmission blends single scattering toward the two-stream diffuse
            // 1 / (1 + 3/4 (1 − g) τ) (Bohren 1987) by the albedo. The sky lights it from above
            // and the ground from below by the world-up facing of the normal.
            let a_px = clamp(alpha, 0.0, 0.98);
            let sigma = -log(1.0 - a_px) / max(2.0 * nz, 0.1);
            var sp = vec2<f32>(bent.x * cs - bent.y * sn, bent.x * sn + bent.y * cs);
            let sp2 = dot(sp, sp);
            if (sp2 > 0.98) {
                sp = sp * (0.99 / sqrt(sp2));
            }
            let l = normalize(sun_view + vec3<f32>(1e-6));
            let b = dot(sp, l.xy);
            let chord = -b + sqrt(max(b * b - (dot(sp, sp) - 1.0), 0.0));
            // The puffs around this one add `NEIGHBOUR_DEPTH × shadow²` chords of extinction
            // toward the sun (ADR-1454), so a dense cap's core is dark, not a sunlit sphere.
            let tau = sigma * chord * (0.5 + NEIGHBOUR_DEPTH * shadow * shadow);
            let single = exp(-tau);
            let albedo = clamp(dot(in.color.rgb, vec3<f32>(0.2126, 0.7152, 0.0722)), 0.0, 1.0);
            // Diffusion regime of an absorbing medium: exp(-kappa tau) over the two-stream
            // conservative transport (`quality::diffuse_transmission`).
            let kappa = sqrt(max(3.0 * (1.0 - albedo) * (1.0 - SMOKE_HG_G * albedo), 0.0));
            let diffuse = exp(-kappa * tau) / (1.0 + 0.75 * (1.0 - SMOKE_HG_G) * tau);
            let up_view = (frame.view * vec4<f32>(0.0, 1.0, 0.0, 0.0)).xyz;
            let facing = clamp(dot(n, up_view) * 0.5 + 0.5, 0.0, 1.0);
            let inner = 1.0 - 0.5 * a_px * shadow;
            // Bright aerosol (snow powder, chalk-white dust) is seen as white-grey under a low
            // orange sun: the sun's and the ground's chroma is partly adapted away by albedo
            // (`quality::adapt_chroma`, ADR-1454). Soot (albedo about 0.1) keeps the full hue.
            let adapt = clamp(CHROMA_ADAPT * albedo * albedo, 0.0, 1.0);
            let sun_p = mix(frame.light1.rgb, vec3<f32>(dot(frame.light1.rgb, vec3<f32>(0.2126, 0.7152, 0.0722))), adapt);
            let down_p = mix(frame.sky_down.rgb, vec3<f32>(dot(frame.sky_down.rgb, vec3<f32>(0.2126, 0.7152, 0.0722))), adapt);
            light = sun_p * mix(single, diffuse, albedo)
                + mix(down_p, frame.sky_up.rgb, facing) * inner;
            sun_fwd = sun_p;
        }
        // Forward scattering: smoke scatters mostly forward (Henyey & Greenstein 1941 phase,
        // 4π-normalised so isotropic is 1), so looking toward the sun the thin fringe of each
        // billow glows (a backlit column's bright rim) while its thick body stays dark.
        let to_eye = normalize(in.world - frame.cam_pos_near.xyz);
        let cos_t = dot(to_eye, normalize(frame.light0.xyz + vec3<f32>(1e-6)));
        let hg_d = 1.0 + SMOKE_HG_G * SMOKE_HG_G - 2.0 * SMOKE_HG_G * cos_t;
        let phase = (1.0 - SMOKE_HG_G * SMOKE_HG_G) / (hg_d * sqrt(hg_d));
        // The sun reaches a backlit puff through the puffs behind it (ADR-1454: the glow of a
        // thin fringe survives, the whole body of a dense cap does not turn the sun's orange).
        let sun_through = exp(-NEIGHBOUR_DEPTH * clamp(in.shade.w, 0.0, 1.0) * clamp(in.shade.w, 0.0, 1.0)
            * 0.5 * -log(1.0 - clamp(alpha, 0.0, 0.98)));
        light = light + sun_fwd * (FORWARD_SCATTER * phase * (1.0 - clamp(tex.a, 0.0, 1.0)) * sun_through);
        // The strongest particle light (the fire) lights the smoke around it: wrapped
        // Lambert on the same sphere normal, windowed inverse-square falloff, so the
        // underside of a column glows orange over the fire.
        if (frame.fire_pos.w > 0.0) {
            let to_fire = frame.fire_pos.xyz - in.world;
            let dist2 = dot(to_fire, to_fire);
            let ratio = sqrt(dist2) / frame.fire_pos.w;
            let window = clamp(1.0 - ratio * ratio * ratio * ratio, 0.0, 1.0);
            // A flash or fireball light is a sphere source of the ball's radius (ADR-1498,
            // `crash::fire_light_falloff`): a puff under the ball is lit at most `I / s²`, so the
            // dust skirt beneath it does not glow as a white band. Punctual lights are unchanged.
            let falloff = window * window / (dist2 + max(fire_source_sq(frame.fire_pos.w), 1.0));
            let fire_view = (frame.view * vec4<f32>(to_fire / max(sqrt(dist2), 1e-4), 0.0)).xyz;
            let wrap = clamp((dot(n, fire_view) + 0.5) / 1.5, 0.0, 1.0);
            light = light + frame.fire_color.rgb * falloff * wrap;
        }
        rgb = rgb * light * max(in.shade.z, 0.0);
    } else {
        rgb = rgb * max(in.shade.z, 0.0);
    }
    // Floor matches `quality::EXPOSURE_FLOOR`. 1e-4 is about EV 13 and disagrees with
    // the post multiply at daylight EV 15.
    rgb = rgb / max(frame.sim.w, 1e-8);
    // Atlas rgb is white on every identity tile. Flame tiles darken the fringe and
    // leave the tongue at 1, on all three channels, so the particle tint keeps its hue.
    if (on_sheet && frame.decal.y > 1.5) {
        // Fire on an HDR frame emits as a black body: the sheet's brightness is the heat,
        // mapped to 1300–3000 K, and the colour is that temperature's chromaticity. The
        // tonemap then keeps cool flame orange-red and only the hot core goes yellow-white
        // (a flat sheet tint went beige through AgX). Visible radiance climbs steeply with
        // temperature (Wien: about e^(−hc/λkT)), so the brightness is a steep power of the
        // heat and the core reads far brighter than the tongues.
        let heat = clamp(dot(tex.rgb, vec3<f32>(0.2126, 0.7152, 0.0722)), 0.0, 1.0);
        let body = blackbody_rgb(mix(1300.0, 3000.0, heat));
        let body_luma = dot(body, vec3<f32>(0.2126, 0.7152, 0.0722));
        let emission = rgb * body * (FIRE_CORE_GAIN * pow(heat, FIRE_HEAT_POWER) / max(body_luma, 0.25));
        // Below the flame threshold the sheet is soot (the sheet's cool rim and folds): a
        // dark, non-emissive smoke lobe, so the fireball keeps dark lobes around its hot core
        // instead of glowing evenly. Albedo about 0.04 (carbon black), lit by the sun as a
        // half-shadowed puff and by the fire inside it (the frame's strongest particle light,
        // unwrapped: the lobes sit around the flame), so they glow dull orange near the core.
        // Display-referred, then into the pre-exposure target like `rgb`.
        let amb = clamp(frame.light0.w, 0.0, 1.0);
        var soot_light = max(frame.light1.rgb, vec3<f32>(0.0)) * (amb + (1.0 - amb) * 0.5);
        if (frame.sky_up.w > 0.5) {
            // Under a sky (ADR-1425): the lobe is a sphere lit from the sun's side (wrapped, so
            // the terminator is soft and the far side keeps a little multiply scattered light),
            // its thin fringe glows with forward scattering toward the sun, and the sky lights
            // the top and the ground the bottom, so a soot lobe has a sunlit rind and a dark
            // core instead of one flat brown.
            let ps = in.local_uv * 2.0 - vec2<f32>(1.0);
            let nzs = sqrt(max(0.0, 1.0 - dot(ps, ps)));
            let rc = cos(in.soft_depth.w);
            let rs = sin(in.soft_depth.w);
            let ns = normalize(vec3<f32>(ps.x * rc - ps.y * rs, ps.x * rs + ps.y * rc, max(nzs, 0.05)));
            let ls = normalize((frame.view * vec4<f32>(frame.light0.xyz, 0.0)).xyz + vec3<f32>(1e-6));
            let wrap = clamp((dot(ns, ls) + 0.3) / 1.3, 0.0, 1.0);
            let up_s = (frame.view * vec4<f32>(0.0, 1.0, 0.0, 0.0)).xyz;
            let facing_s = clamp(dot(ns, up_s) * 0.5 + 0.5, 0.0, 1.0);
            let to_eye_s = normalize(in.world - frame.cam_pos_near.xyz);
            let cos_s = dot(to_eye_s, normalize(frame.light0.xyz + vec3<f32>(1e-6)));
            let hg_s = 1.0 + SMOKE_HG_G * SMOKE_HG_G - 2.0 * SMOKE_HG_G * cos_s;
            let phase_s = (1.0 - SMOKE_HG_G * SMOKE_HG_G) / (hg_s * sqrt(hg_s));
            let fringe = FORWARD_SCATTER * phase_s * (1.0 - clamp(tex.a, 0.0, 1.0));
            soot_light = max(frame.light1.rgb, vec3<f32>(0.0)) * (0.1 + 0.9 * wrap * wrap + fringe)
                + mix(frame.sky_down.rgb, frame.sky_up.rgb, facing_s);
        }
        if (frame.fire_pos.w > 0.0) {
            let to_fire = frame.fire_pos.xyz - in.world;
            let dist2 = dot(to_fire, to_fire);
            let ratio = sqrt(dist2) / frame.fire_pos.w;
            let window = clamp(1.0 - ratio * ratio * ratio * ratio, 0.0, 1.0);
            soot_light = soot_light + frame.fire_color.rgb * 0.15 * window * window / (dist2 + max(fire_source_sq(frame.fire_pos.w), 1.0));
        }
        let soot = SOOT_ALBEDO * soot_light / max(frame.sim.w, 1e-8);
        let flame = smoothstep(SOOT_HEAT.x, SOOT_HEAT.y, heat);
        rgb = mix(soot, emission, flame);
        // Soot is optically thick: the same sheet density hides more of what is behind it
        // (1 − (1 − a)², still zero where the sheet is clear), so the lobes read dark, not as
        // a grey veil.
        alpha = mix(1.0 - (1.0 - alpha) * (1.0 - alpha), alpha, flame);
    } else {
        rgb = rgb * tex.rgb;
    }
    // Premultiplied source. Alpha blend replaces the backdrop with this colour;
    // additive adds it. The weight is the sprite alpha. Widening coverage without
    // the same colour, or repainting the pixel afterward, hides a pink edge.
    return vec4<f32>(rgb * alpha, alpha);
}
"#;

/// Unit-cube triangle list shared by mesh particles and decals.
/// A `switch`, not a function-local array: naga 30 would emit `ArrayStride` on a
/// Function variable, which SPIR-V 1.6 rejects (ADR-026).
pub const CUBE_VERT: &str = r#"
fn cube_vert(i: u32) -> vec3<f32> {
    // Same 36-vertex list as before. Switch: no function-local array (ADR-026).
    switch i {
        case 0u: { return vec3<f32>(-1.0, -1.0, -1.0); }
        case 1u: { return vec3<f32>(1.0, -1.0, -1.0); }
        case 2u: { return vec3<f32>(-1.0, 1.0, -1.0); }
        case 3u: { return vec3<f32>(-1.0, 1.0, -1.0); }
        case 4u: { return vec3<f32>(1.0, -1.0, -1.0); }
        case 5u: { return vec3<f32>(1.0, 1.0, -1.0); }
        case 6u: { return vec3<f32>(-1.0, -1.0, 1.0); }
        case 7u: { return vec3<f32>(-1.0, 1.0, 1.0); }
        case 8u: { return vec3<f32>(1.0, -1.0, 1.0); }
        case 9u: { return vec3<f32>(-1.0, 1.0, 1.0); }
        case 10u: { return vec3<f32>(1.0, 1.0, 1.0); }
        case 11u: { return vec3<f32>(1.0, -1.0, 1.0); }
        case 12u: { return vec3<f32>(-1.0, -1.0, -1.0); }
        case 13u: { return vec3<f32>(-1.0, 1.0, -1.0); }
        case 14u: { return vec3<f32>(-1.0, -1.0, 1.0); }
        case 15u: { return vec3<f32>(-1.0, 1.0, -1.0); }
        case 16u: { return vec3<f32>(-1.0, 1.0, 1.0); }
        case 17u: { return vec3<f32>(-1.0, -1.0, 1.0); }
        case 18u: { return vec3<f32>(1.0, -1.0, -1.0); }
        case 19u: { return vec3<f32>(1.0, -1.0, 1.0); }
        case 20u: { return vec3<f32>(1.0, 1.0, -1.0); }
        case 21u: { return vec3<f32>(1.0, 1.0, -1.0); }
        case 22u: { return vec3<f32>(1.0, -1.0, 1.0); }
        case 23u: { return vec3<f32>(1.0, 1.0, 1.0); }
        case 24u: { return vec3<f32>(-1.0, -1.0, -1.0); }
        case 25u: { return vec3<f32>(1.0, -1.0, -1.0); }
        case 26u: { return vec3<f32>(1.0, -1.0, 1.0); }
        case 27u: { return vec3<f32>(-1.0, -1.0, -1.0); }
        case 28u: { return vec3<f32>(1.0, -1.0, 1.0); }
        case 29u: { return vec3<f32>(-1.0, -1.0, 1.0); }
        case 30u: { return vec3<f32>(-1.0, 1.0, -1.0); }
        case 31u: { return vec3<f32>(-1.0, 1.0, 1.0); }
        case 32u: { return vec3<f32>(1.0, 1.0, -1.0); }
        case 33u: { return vec3<f32>(-1.0, 1.0, -1.0); }
        case 34u: { return vec3<f32>(1.0, 1.0, -1.0); }
        default: { return vec3<f32>(1.0, 1.0, 1.0); }
    }
}
"#;

/// Mesh particles: a rotated unit cube per particle (unlit, tinted), or a registered mesh asset
/// (`.vfx.ron` v8), lit by the frame's sun and ambient when its emitter is `lit` (ADR-7261).
pub const MESH_PARTICLE: &str = r#"
struct VsOut {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec4<f32>,
    // World-space face normal of a mesh asset (zero for the cube).
    @location(1) normal: vec3<f32>,
    // 1: shade with the frame's sun and ambient (a `lit` mesh-asset emitter), 0: flat tint.
    @location(2) @interpolate(flat) lit: u32,
};

fn rotate(v: vec3<f32>, axis: vec3<f32>, angle: f32) -> vec3<f32> {
    let c = cos(angle);
    let s = sin(angle);
    return v * c + cross(axis, v) * s + axis * dot(axis, v) * (1.0 - c);
}

// A vertex every rasterizer clips (outside the clip volume on every axis).
const MESH_CULLED: vec4<f32> = vec4<f32>(2.0, 2.0, 2.0, 1.0);

// Head entries of the bucketed mesh-asset list (`mesh_draw::BUCKET_HEAD`): `(start, count)` of
// render-mesh slot k at entry k, the bucketed entries after them.
const MESH_BUCKET_HEAD: u32 = 65u;

// One mesh-particle vertex: list entry `index` (into `lists`), object-space vertex `local`
// (scaled by the particle size) with its face normal `normal` (zero for the cube). `want` is the
// render-mesh slot this draw serves (`Emitter.events.y`: 0 = the built-in cube, k = registered
// mesh k, `0xffffffff` = an unresolved mesh that draws nothing, `.vfx.ron` v8 `render.mesh`); a
// particle of another slot is culled. `lightable`: a lit emitter is shaded (mesh assets only).
fn mesh_particle_vertex(index: u32, local: vec3<f32>, normal: vec3<f32>, want: u32, lightable: bool) -> VsOut {
    let entry = lists[index];
    let p = entry.y;
    let pa = pos_age[p];
    let vl = vel_life[p];
    let at = attrib[p];
    let c0 = color0[p];
    let e = emitters[u32(at.w)];
    if (e.events.y != want) {
        return VsOut(MESH_CULLED, vec4<f32>(0.0), vec3<f32>(0.0), 0u);
    }
    let t = clamp(pa.w / max(vl.w, 1e-5), 0.0, 1.0);
    let size = at.x * lut_scalar(e.curve_ids.x, t);
    let alpha = c0.a * lut_scalar(e.curve_ids.z, t) * max(e.render.z, 0.0);
    let rgb = c0.rgb * lut_color(e.curve_ids.y, t).rgb * max(e.quality.x, 0.0) / max(frame.sim.w, 1e-8);
    let angle = at.y + at.z * pa.w + lut_scalar(e.curve_ids.w, t);
    let axis = hash3_to_unit(p * 2654435761u + 1u);
    let world = pa.xyz + rotate(local * size, axis, angle);
    // The size is uniform, so the rotated face normal stays the world normal.
    let lit = select(0u, 1u, lightable && e.quality.y > 0.5);
    return VsOut(frame.view_proj * vec4<f32>(world, 1.0), vec4<f32>(rgb, alpha), rotate(normal, axis, angle), lit);
}

// The built-in cube (edge = particle size), 36 vertices per instance, over the whole mesh list.
@vertex
fn vs_mesh(
    @builtin(vertex_index) vi: u32,
    @builtin(instance_index) ii: u32,
) -> VsOut {
    return mesh_particle_vertex(u32(frame.sim.y) * 2u + ii, cube_vert(vi % 36u) * 0.5, vec3<f32>(0.0), 0u, false);
}

// A registered mesh asset (`.vfx.ron` v8): indexed, one draw per mesh over only that mesh's
// instances. `lists` is bound to the bucketed list here (`mesh_draw::MESH_BUCKET`): slot k's
// instances start at `lists[k].x` past the head. `local.w` carries the mesh's slot and `normal`
// the face normal (written into every vertex at upload).
@vertex
fn vs_mesh_asset(
    @location(0) local: vec4<f32>,
    @location(1) normal: vec4<f32>,
    @builtin(instance_index) ii: u32,
) -> VsOut {
    let slot = u32(local.w);
    return mesh_particle_vertex(MESH_BUCKET_HEAD + lists[slot].x + ii, local.xyz, normal.xyz, slot, true);
}

// Light reaching a lit mesh-asset face of world normal `n` (display-referred, like a lit
// sprite): Lambert from the sun over the frame's ambient floor; under a sky (ADR-1425) the sun
// plus the sky from above and the ground from below by the normal's world-up facing.
fn mesh_light(n: vec3<f32>) -> vec3<f32> {
    let sun_len = length(frame.light0.xyz);
    let sun = select(vec3<f32>(0.0, 1.0, 0.0), frame.light0.xyz / max(sun_len, 1e-6), sun_len > 1e-4);
    let ndl = max(dot(n, sun), 0.0);
    if (frame.sky_up.w > 0.5) {
        let facing = clamp(n.y * 0.5 + 0.5, 0.0, 1.0);
        return frame.light1.rgb * ndl + mix(frame.sky_down.rgb, frame.sky_up.rgb, facing);
    }
    let amb = clamp(frame.light0.w, 0.0, 1.0);
    return frame.light1.rgb * (amb + (1.0 - amb) * ndl);
}

@fragment
fn fs_mesh(in: VsOut, @builtin(front_facing) front: bool) -> @location(0) vec4<f32> {
    var rgb = in.color.rgb;
    if (in.lit == 1u) {
        // A lit mesh draws its front faces only (counter-clockwise, glTF's convention): mesh
        // particles blend without depth writes, so a shaded back face drawn later would paint
        // over the sunlit front.
        if (!front) {
            discard;
        }
        rgb = rgb * mesh_light(normalize(in.normal));
    }
    return vec4<f32>(rgb * in.color.a, in.color.a);
}
"#;

/// Projected decals: unit-cube volumes. The fragment reconstructs the surface from the depth
/// buffer, projects it into the decal frame, and fades at edges and grazing angles.
///
/// `frame.decal.x` is a bitfield (stored as a small integer): bit 0 = a receiver texture is bound
/// (layer in R, world normal in GBA), bit 1 = a scene-colour copy is bound. With both bits clear
/// the shader returns the M14 premultiplied tint. The material path modulates or replaces that
/// colour and optionally relights from the receiver normal.
pub const DECAL: &str = r#"
struct Decal {
    right: vec4<f32>,          // xyz basis, w half width
    up: vec4<f32>,             // xyz basis, w half height
    forward: vec4<f32>,        // xyz basis, w half depth
    center_fade: vec4<f32>,    // xyz centre, w lifetime fade
    albedo_opacity: vec4<f32>, // linear albedo, opacity
    normal_rough: vec4<f32>,   // normal xy, roughness, normal strength
    emissive_angle0: vec4<f32>,// emissive rgb, angle-fade start
    fade_atlas: vec4<f32>,     // angle end, dist start, dist end, atlas layer (<0 solid)
    mask_channels: vec4<f32>,  // mask (0 = all), channels, edge softness, blend
    sort_pad: vec4<f32>,
};

@group(1) @binding(0) var<storage, read> decals: array<Decal>;
@group(1) @binding(1) var depth_tex: texture_depth_2d;
@group(1) @binding(2) var depth_sampler: sampler;
@group(1) @binding(3) var receiver_tex: texture_2d<f32>;
@group(1) @binding(4) var scene_tex: texture_2d<f32>;
@group(1) @binding(5) var decal_atlas: texture_2d_array<f32>;
@group(1) @binding(6) var decal_sampler: sampler;

struct TileQuad {
    rect: vec4<f32>, // x0, y0, x1, y1 pixels, exclusive max
    slot: u32,
    // Three scalars, not vec3: vec3<u32> aligns to 16 and would make the
    // array stride 48 while DecalTileGpu is 32 bytes. Instance 1+ would
    // then read the wrong rectangle.
    pad0: u32,
    pad1: u32,
    pad2: u32,
};
@group(1) @binding(7) var<storage, read> tile_quads: array<TileQuad>;

struct VsOut {
    @builtin(position) position: vec4<f32>,
    @interpolate(flat) @location(0) index: u32,
};

// Two triangles over a tile rect (x0, y0, x1, y1). y is top-down.
// A switch, not a function-local array (ADR-026).
fn tile_corner(i: u32, rect: vec4<f32>) -> vec2<f32> {
    switch i {
        case 0u: { return rect.xy; }
        case 1u: { return rect.zy; }
        case 2u: { return rect.xw; }
        case 3u: { return rect.xw; }
        case 4u: { return rect.zy; }
        default: { return rect.zw; }
    }
}

// IEC 61966-2-1. The Unorm golden target stores display bytes; an sRGB target
// already decodes on load and encodes on store.
fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    let lo = c / 12.92;
    let hi = pow((c + vec3<f32>(0.055)) / vec3<f32>(1.055), vec3<f32>(2.4));
    return select(hi, lo, c <= vec3<f32>(0.04045));
}

fn linear_to_srgb(c: vec3<f32>) -> vec3<f32> {
    let x = max(c, vec3<f32>(0.0));
    let lo = x * 12.92;
    let hi = vec3<f32>(1.055) * pow(x, vec3<f32>(1.0 / 2.4)) - vec3<f32>(0.055);
    return select(hi, lo, x <= vec3<f32>(0.0031308));
}

// Rec. 709 luma of one atlas texel. Dark texels are the recessed part of a scorch or a rut.
fn atlas_shade(layer: i32, uv: vec2<f32>) -> f32 {
    let texel = textureSampleLevel(decal_atlas, decal_sampler, uv, layer, 0.0);
    return dot(texel.rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
}

@vertex
fn vs_decal(
    @builtin(vertex_index) vi: u32,
    @builtin(instance_index) ii: u32,
) -> VsOut {
    let q = tile_quads[ii];
    let px = tile_corner(vi % 6u, q.rect);
    let ndc_x = px.x * frame.viewport.z * 2.0 - 1.0;
    let ndc_y = 1.0 - px.y * frame.viewport.w * 2.0;
    // z = 0 sits on the near plane so LessEqual passes against every receiver.
    return VsOut(vec4<f32>(ndc_x, ndc_y, 0.0, 1.0), q.slot);
}

@fragment
fn fs_decal(in: VsOut) -> @location(0) vec4<f32> {
    let d = decals[in.index];
    let uv = in.position.xy * frame.viewport.zw;
    let scene_depth = textureSampleLevel(depth_tex, depth_sampler, uv, 0);
    if (scene_depth >= 1.0) { discard; }
    let surface = world_from_depth(uv, scene_depth).xyz;
    let rel = surface - d.center_fade.xyz;
    let x = dot(rel, d.right.xyz) / max(d.right.w, 1e-5);
    let y = dot(rel, d.up.xyz) / max(d.up.w, 1e-5);
    let z = dot(rel, d.forward.xyz) / max(d.forward.w, 1e-5);
    if (abs(x) > 1.0 || abs(y) > 1.0 || abs(z) > 1.0) { discard; }

    let dx = dpdx(surface);
    let dy = dpdy(surface);
    let n_geom = normalize(cross(dx, dy) + vec3<f32>(1e-6));
    let flags = u32(frame.decal.x + 0.5);
    var n = n_geom;
    if ((flags & 1u) != 0u) {
        let rec = textureLoad(receiver_tex, vec2<i32>(in.position.xy), 0);
        let layer = u32(round(rec.r * 255.0));
        let mask = u32(d.mask_channels.x + 0.5);
        if (mask != 0u) {
            let bit = min(layer, 31u);
            if ((mask & (1u << bit)) == 0u) { discard; }
        }
        let enc = rec.gba * 2.0 - vec3<f32>(1.0);
        if (length(enc) > 0.5) {
            n = normalize(enc);
        }
    }

    let facing = abs(dot(n, d.forward.xyz));
    let a0 = d.emissive_angle0.w;
    let a1 = d.fade_atlas.x;
    var grazing = 1.0;
    if (a1 > a0) {
        grazing = smoothstep(a0, a1, facing);
    }
    let feather = (1.0 - smoothstep(0.7, 1.0, abs(x))) * (1.0 - smoothstep(0.7, 1.0, abs(y)));
    let edge = mix(1.0, feather, clamp(d.mask_channels.z, 0.0, 1.0));
    let dist = length(frame.cam_pos_near.xyz - d.center_fade.xyz);
    let d0 = d.fade_atlas.y;
    let d1 = d.fade_atlas.z;
    var dist_fade = 1.0;
    if (d1 > d0) {
        dist_fade = 1.0 - smoothstep(d0, d1, dist);
    }
    var tint = vec3<f32>(1.0);
    var coverage = 1.0;
    var auv = vec2<f32>(0.5);
    let atlas_i = i32(round(d.fade_atlas.w));
    if (atlas_i >= 0) {
        auv = clamp(vec2<f32>(x * 0.5 + 0.5, 0.5 - y * 0.5), vec2<f32>(0.0), vec2<f32>(1.0));
        // Rgba8UnormSrgb: rgb is linear, alpha stays the coverage (ADR-038).
        let texel = textureSampleLevel(decal_atlas, decal_sampler, auv, atlas_i, 0.0);
        tint = texel.rgb;
        coverage = texel.a;
    }
    let albedo = d.albedo_opacity.rgb * tint;
    let w = d.albedo_opacity.a * d.center_fade.w * grazing * edge * dist_fade * coverage;
    if (w <= 1e-4) { discard; }

    // Mode 2: the target is pre-exposure radiance (the runtime HDR pass). The shader
    // works in exposed linear light (scene × exposure) and divides its output back.
    let hdr_target = frame.decal.y > 1.5;
    let exposure = max(frame.sim.w, 1e-8);
    // No scene-colour copy: the M14 premultiplied tint (defaults reproduce it exactly).
    if ((flags & 2u) == 0u) {
        let tint_rgb = select(albedo, albedo / exposure, hdr_target);
        return vec4<f32>(tint_rgb * w, w);
    }

    let channels = u32(d.mask_channels.y + 0.5);
    // One-texel slope of (1 − luma) in decal UV. `auv.y` grows as local y shrinks,
    // so the v difference is applied along −up. The decal axes (not the receiver
    // tangent) keep a rut groove across the tyre.
    var n_base = n;
    if ((channels & 2u) != 0u && atlas_i >= 0 && atlas_i < 4) {
        let eps = 1.0 / 64.0;
        // Luma is the height: a dark centre sits below a bright rim (a bowl, a groove).
        let h = dot(tint, vec3<f32>(0.2126, 0.7152, 0.0722));
        let hx = atlas_shade(atlas_i, clamp(auv + vec2<f32>(eps, 0.0), vec2<f32>(0.0), vec2<f32>(1.0)));
        let hy = atlas_shade(atlas_i, clamp(auv + vec2<f32>(0.0, eps), vec2<f32>(0.0), vec2<f32>(1.0)));
        let du = (hx - h) / eps;
        let dv = (hy - h) / eps;
        let scale = clamp(d.normal_rough.w, 0.0, 1.0) * 0.95;
        n_base = normalize(n + d.right.xyz * (-du * scale) + d.up.xyz * (dv * scale));
    }
    var n_shade = n_base;
    let nxy = d.normal_rough.xy;
    if (dot(nxy, nxy) > 1e-6) {
        let nz = sqrt(max(0.0, 1.0 - dot(nxy, nxy)));
        let n2 = vec3<f32>(nxy, nz);
        // Whiteout (Barré-Brisebois & Hill, "Blending in Detail", 2012). Both normals
        // live in the receiver's tangent frame; n1 is that frame's base (0, 0, 1).
        // The frame follows the decal: x is the decal's right axis on the receiver, so an
        // authored tilt points the same way on flat and gently rolling ground (a world-up
        // cross product flipped it with the slope's azimuth).
        var t = d.right.xyz - n_base * dot(d.right.xyz, n_base);
        if (dot(t, t) < 1e-6) {
            t = d.up.xyz - n_base * dot(d.up.xyz, n_base);
        }
        t = normalize(t);
        let b = cross(n_base, t);
        let n1 = vec3<f32>(0.0, 0.0, 1.0);
        let n_ts = normalize(vec3<f32>(n1.xy + n2.xy, n1.z * n2.z));
        n_shade = normalize(t * n_ts.x + b * n_ts.y + n_base * n_ts.z);
    }
    // The frame's sun (`light0` toward it, `light1` its display-scaled colour).
    let sun_len = length(frame.light0.xyz);
    let sun = select(vec3<f32>(0.0, 1.0, 0.0), frame.light0.xyz / max(sun_len, 1e-6), sun_len > 1e-4);
    let ndl = clamp(dot(n_shade, sun), 0.0, 1.0);
    let ndl_geom = clamp(dot(n_geom, sun), 0.0, 1.0);
    let relight = ndl / max(ndl_geom, 0.15);
    let lit = textureLoad(scene_tex, vec2<i32>(in.position.xy), 0).rgb;
    // frame.decal.y > 0.5: the attachment is Rgba8UnormSrgb (or HDR), so `lit` is linear.
    let target_linear = frame.decal.y > 0.5;
    var scene_lin = lit;
    if (hdr_target) {
        scene_lin = lit * exposure;
    } else if (!target_linear) {
        scene_lin = srgb_to_linear(lit);
    }
    var modified = scene_lin;
    if ((channels & 1u) != 0u) {
        if (d.mask_channels.w < 0.5) {
            modified = scene_lin * albedo;
        } else {
            // Paint replaces the receiver's albedo under the receiver's own light. The
            // light is the scene luma over a reference ground albedo (0.45: the receiver's
            // real albedo is unknown here), so the paint prints at shade × paint / 0.45,
            // capped at 2× the ground: a white pad marking reads brighter than dust, a dark
            // wear mark darker, and a stripe in shade stays in shade. The old fixed light
            // floor (0.45 + shade) made paint self-lit: a glowing ribbon on a dim slope.
            let luma_w = vec3<f32>(0.2126, 0.7152, 0.0722);
            let shade = dot(scene_lin, luma_w);
            let paint_luma = max(dot(albedo, luma_w), 1e-4);
            let ratio = min(paint_luma / 0.45, 2.0);
            modified = albedo * (shade * ratio / paint_luma);
        }
    }
    if ((channels & 2u) != 0u) {
        modified = modified * mix(1.0, relight, clamp(d.normal_rough.w, 0.0, 1.0));
    }
    if ((channels & 4u) != 0u) {
        let view_dir = normalize(frame.cam_pos_near.xyz - surface);
        let refl = reflect(-sun, n_shade);
        let rough = clamp(d.normal_rough.z, 0.0, 1.0);
        let spec = pow(max(dot(refl, view_dir), 0.0), mix(48.0, 4.0, rough)) * (1.0 - rough) * 0.35;
        modified = modified + spec * max(frame.light1.rgb, vec3<f32>(0.0));
    }
    if ((channels & 8u) != 0u) {
        modified = modified + d.emissive_angle0.rgb;
    }
    var out_rgb = modified;
    if (hdr_target) {
        out_rgb = modified / exposure;
    } else if (!target_linear) {
        out_rgb = linear_to_srgb(modified);
    }
    return vec4<f32>(out_rgb * w, w);
}
"#;

/// CPU-built ribbon/trail strips.
pub const TRAIL: &str = r#"
@group(1) @binding(0) var trail_atlas: texture_2d<f32>;
@group(1) @binding(1) var trail_sampler: sampler;

struct TrailVertex {
    @location(0) position_size: vec4<f32>,
    @location(1) uv_pad: vec4<f32>,
    @location(2) color: vec4<f32>,
};

struct VsOut {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec4<f32>,
    // (t along trail, side -1..1, soft cross-section 0/1, additive share 0..1)
    @location(1) fade: vec4<f32>,
    // Ribbon texture (`.vfx.ron` v7): atlas tile + 1, 0 = none.
    @location(2) @interpolate(flat) tile: f32,
};

@vertex
fn vs_trail(in: TrailVertex) -> VsOut {
    return VsOut(
        frame.view_proj * vec4<f32>(in.position_size.xyz, 1.0),
        in.color,
        vec4<f32>(in.uv_pad.xyz, in.position_size.w),
        in.uv_pad.w,
    );
}

// The 8 x 4 sprite atlas (`atlas::TILES_X`, `atlas::TILES_Y`); u wraps inside the tile (a tiled
// ribbon), v runs across the strip.
fn trail_tile_alpha(tile: u32, u: f32, side: f32) -> f32 {
    let col = f32(tile % 8u);
    let row = f32(min(tile / 8u, 3u));
    let local = vec2<f32>(clamp(fract(u), 0.02, 0.98), clamp(side * 0.5 + 0.5, 0.02, 0.98));
    let uv = (vec2<f32>(col, row) + local) / vec2<f32>(8.0, 4.0);
    return textureSampleLevel(trail_atlas, trail_sampler, uv, 0.0).a;
}

@fragment
fn fs_trail(in: VsOut) -> @location(0) vec4<f32> {
    // fade.y = side across the ribbon: soften the long edges. (The along-trail fade is the
    // vertex colour's alpha — using t here as well would double-fade the tail to nothing.)
    let side = abs(in.fade.y);
    let flat_edge = 1.0 - smoothstep(0.6, 1.0, side);
    // Soft ribbons (the entry glow and its dust train) fall off as a Gaussian across, with no
    // flat core and no outline; the last tenth takes the 1 % left at the edge to zero.
    let soft_edge = exp(-4.5 * side * side) * (1.0 - smoothstep(0.9, 1.0, side));
    var edge = select(flat_edge, soft_edge, in.fade.z > 0.5);
    // A textured ribbon takes its cross-section from the sprite instead.
    if (in.tile > 0.5) {
        edge = trail_tile_alpha(u32(in.tile - 0.5), in.fade.x, in.fade.y);
    }
    let alpha = in.color.a * edge;
    // `sim.w` is the post exposure on an HDR target and 1 on a display target, the same
    // divide the billboards use, so a trail survives EV 15 (exposure ~2.5e-5).
    let rgb = in.color.rgb / max(frame.sim.w, 1e-8);
    // The additive share emits without dimming what is behind (optically thin glowing gas);
    // 0 (every other trail) is premultiplied over, exactly as before.
    return vec4<f32>(rgb * alpha, alpha * (1.0 - clamp(in.fade.w, 0.0, 1.0)));
}
"#;
