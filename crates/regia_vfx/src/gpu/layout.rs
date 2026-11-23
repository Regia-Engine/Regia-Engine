// SPDX-License-Identifier: MPL-2.0
//! GPU-side data layouts (`#[repr(C)]` mirrors of the WGSL structs).
//!
//! Everything is `vec4`-shaped to keep WGSL struct alignment trivial (no hidden padding).
//! Each struct carries a static size assertion against the WGSL layout documented in
//! `shaders.rs`.

use bytemuck::{Pod, Zeroable};

use crate::curves::CURVE_SAMPLES;

/// Maximum per-frame emitter records (one per live emitter *instance*).
pub const MAX_EMITTERS: u32 = 256;
/// Maximum spawn jobs buffered per frame.
pub const MAX_SPAWN_JOBS: u32 = 65_536;
/// Maximum decals the GPU buffer can hold. Profiles pick a budget at or under this.
pub const MAX_DECALS: u32 = 512;
/// Trail vertices uploadable per frame.
pub const MAX_TRAIL_VERTICES: u32 = 65_536;

/// Per-frame uniforms (bind group 0 of every pass).
///
/// WGSL `Frame`:
/// ```text
/// view_proj: mat4x4<f32>       //  0
/// inv_view_proj: mat4x4<f32>   // 64
/// view: mat4x4<f32>            //128
/// cam_right: vec4<f32>         //192  xyz = camera right, w = delta time
/// cam_up: vec4<f32>            //208  xyz = camera up,    w = elapsed time
/// cam_pos: vec4<f32>           //224  xyz = camera pos,   w = near plane
/// viewport: vec4<f32>          //240  w, h, 1/w, 1/h
/// params: vec4<f32>            //256  far plane, CPU sort flag, unused, spawn count
/// sim: vec4<f32>               //272  pool capacity, sort_n, screen floor px, exposure
/// light0: vec4<f32>            //288  xyz toward the sun (world), w ambient
/// light1: vec4<f32>            //304  rgb sun colour, w pixels per metre at depth 1
/// decal: vec4<f32>             //320  decal flags, target mode (0 display bytes, 1 sRGB, 2 HDR), haze on, haze scale
/// fire_pos: vec4<f32>          //336  xyz strongest particle light, w radius (0 = none)
/// fire_color: vec4<f32>        //352  rgb colour × intensity, w = bit 0 spherical billboards, bits 1.. soft-pass mode
/// window: vec4<f32>            //368  update window: first slot, slot count, spawn base slot
/// sky_up: vec4<f32>            //384  rgb sky radiance from above (display units), w = 1 when set
/// sky_down: vec4<f32>          //400  rgb light from the ground below (display units)
/// shift: vec4<f32>             //416  xyz floating-origin shift of a rebase frame, else zero
/// events: vec4<u32>            //432  child-ring slots to update, event step, event rank budget, child-ring size
/// ```
#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct FrameGpu {
    /// Camera view-projection.
    pub view_proj: [[f32; 4]; 4],
    /// Inverse view-projection (clip → world for decals).
    pub inv_view_proj: [[f32; 4]; 4],
    /// World → view (for sort keys).
    pub view: [[f32; 4]; 4],
    /// Camera right vector + delta time.
    pub cam_right_dt: [f32; 4],
    /// Camera up vector + elapsed time.
    pub cam_up_time: [f32; 4],
    /// Camera position + near plane.
    pub cam_pos_near: [f32; 4],
    /// Viewport `(w, h, 1/w, 1/h)`.
    pub viewport: [f32; 4],
    /// `(far, cpu_sort_flag, unused, spawn_count)`; depth collision is per emitter (`EmitterGpu.kill.y`).
    pub params: [f32; 4],
    /// `(pool capacity, sort_n, screen floor px, exposure multiplier)`.
    pub sim: [f32; 4],
    /// World direction toward the sun (xyz) and scalar ambient (w).
    pub light0: [f32; 4],
    /// Sun linear RGB (xyz) and pixels-per-metre at view depth 1 (w).
    pub light1: [f32; 4],
    /// `(decal flags, target mode, post-decal copy, haze scale)`.
    ///
    /// Decal flags (a small integer stored as `f32`): bit 0 receiver texture bound, bit 1
    /// scene-colour copy bound. Target mode: 0 = `Rgba8Unorm` display bytes, 1 = an sRGB
    /// attachment (linear values), 2 = a float HDR target holding pre-exposure radiance
    /// (divide by `sim.w`). The third component is 1 when the bound scene copy holds the
    /// frame after decals, so heat-haze cards may sample it. The fourth scales every haze
    /// card's offset (host look, default 1).
    pub decal: [f32; 4],
    /// Strongest particle light: world position (xyz) and radius (w, 0 = none). Lit puffs
    /// take its light (smoke over a fire).
    pub fire_pos: [f32; 4],
    /// That light's linear colour × intensity (xyz); `w` packs the spherical-billboard flag
    /// (bit 0) and the soft-pass mode (bits 1..).
    pub fire_color: [f32; 4],
    /// The update window `(first pool slot, slot count, spawn base slot, unused)`: the
    /// ring-buffer slots spawned within the library's longest lifetime, the only ones that can
    /// be alive, and the pool slot of this frame's first spawn job (job `j` lands in
    /// `(base + j) % capacity`).
    pub window: [f32; 4],
    /// Sky radiance from above as a white surface facing up shows it (display units, the same
    /// mapping as the sun in `light1`), and `w` = 1 when the frame has a sky: lit puffs then
    /// take the volumetric sun-and-sky shading (ADR-1425). All zero keeps the scalar ambient.
    pub sky_up: [f32; 4],
    /// Light from the ground below, display units (`w` unused).
    pub sky_down: [f32; 4],
    /// The floating-origin shift (xyz) the `shift_pool` kernel subtracts from every live particle
    /// on a rebase frame (spec 78 § 12.2, ADR-2022); zero on every other frame (`w` unused).
    pub shift: [f32; 4],
    /// Event spawns: `(child-ring slots the update kernel covers, event step, event buffer
    /// capacity, child-ring size)`. The first is 0 while no live instance has events
    /// ([`super::VfxGpu::set_events_active`]), so such a frame updates exactly the main ring.
    pub events: [u32; 4],
}

const _: () = assert!(size_of::<FrameGpu>() == 448);

/// One per-instance emitter record (storage, stable slot for the instance's lifetime).
///
/// WGSL `Emitter` (17 × vec4 = 272 bytes):
/// ```text
/// gravity_drag: vec4<f32>   // world gravity xyz, drag
/// curl: vec4<f32>           // amount, frequency, speed, unused
/// vortex: vec4<f32>         // world axis xyz, strength
/// attractor: vec4<f32>      // world pos xyz, strength
/// kill: vec4<f32>           // kill_below y (=-1e30 off), depth mode (0/1/2), depth offset, softness
/// curve_ids: vec4<u32>      // size, color, alpha, rotation curve LUT ids
/// render: vec4<f32>         // mode (0 billboard,1 stretched,2 mesh), sprite tile, alpha scale, flip cols
/// flip: vec4<f32>           // flip rows, frames, fps (<=0 → over life), random start (0/1)
/// stretch: vec4<f32>        // velocity scale, min length, blend (0 additive,1 alpha), blend frames (0/1)
/// origin: vec4<f32>         // instance origin xyz (vortex axis point), heat haze px (0 = off)
/// quality: vec4<f32>        // emissive, lit (0/1), self-shadow, screen floor px (0 = profile)
/// wind: vec4<f32>           // coupling c × scene wind xyz, coupling c (wind_response × wind_drag)
/// events: vec4<u32>         // trigger mask (1 spawn, 2 death, 4 collision), vortex radial (f32 bits), unused
/// kill_volume: vec4<f32>    // world centre xyz, mode (0 none, 1/2 box in/out, 3/4 sphere in/out)
/// kill_axis_x..z: vec4<f32> // box: world axis / half extent (xyz); sphere: radius in kill_axis_x.x
/// ```
#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct EmitterGpu {
    /// World-space gravity (m/s²) and drag.
    pub gravity_drag: [f32; 4],
    /// Curl noise `(amount, frequency, speed, soft_layer)`; `w > 0` draws in the low-resolution soft layer.
    pub curl: [f32; 4],
    /// Vortex world axis + strength.
    pub vortex: [f32; 4],
    /// Attractor world position + strength.
    pub attractor: [f32; 4],
    /// Kill settings: `(kill_below, depth_mode, depth_offset, softness)`.
    pub kill: [f32; 4],
    /// Curve LUT ids `(size, color, alpha, rotation)`.
    pub curve_ids: [u32; 4],
    /// Render mode / sprite tile / alpha scale / flipbook columns.
    pub render: [f32; 4],
    /// Flipbook `(rows, frames, fps, random_start)`.
    pub flip: [f32; 4],
    /// Stretched billboard `(velocity_scale, min_length, blend, frame_blend)`.
    pub stretch: [f32; 4],
    /// Instance origin (a point on the vortex axis) + heat-haze offset magnitude in pixels in `w`
    /// (`0` = no haze; the value is the refraction amplitude, not a flag).
    pub origin: [f32; 4],
    /// `(emissive, lit, self_shadow, screen_floor_px)`.
    pub quality: [f32; 4],
    /// `(c × wind, c)`: the wind acceleration and the coupling rate `c = wind_response ×
    /// wind_drag` the update kernel integrates (all zero: no wind).
    pub wind: [f32; 4],
    /// Trigger mask of the emitter's event spawns in `x` ([`crate::events::TRIGGER_SPAWN`] and
    /// siblings; zero: the emitter fires nothing). `y`: the vortex's extra radial acceleration
    /// ([`crate::format::VortexDef::radial`]) as `f32` bits. `z` and `w` are unused.
    pub events: [u32; 4],
    /// Kill volume (v7): world centre (xyz) and mode (w, `crate::kill_volume::KILL_*`).
    pub kill_volume: [f32; 4],
    /// Kill volume rows ([`crate::kill_volume::ResolvedKillVolume::axes`]).
    pub kill_axes: [[f32; 4]; 3],
}

const _: () = assert!(size_of::<EmitterGpu>() == 272);

/// One spawn job (4 × vec4).
///
/// WGSL `SpawnJob`:
/// ```text
/// pos_life: vec4<f32>    // xyz, lifetime
/// vel_size: vec4<f32>    // xyz, base size
/// rot_spin: vec4<f32>    // rotation, spin, emitter slot, flipbook phase (0..1)
/// color: vec4<f32>
/// ```
#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct SpawnJobGpu {
    /// Position + lifetime.
    pub pos_life: [f32; 4],
    /// Velocity + base size.
    pub vel_size: [f32; 4],
    /// Rotation, spin, emitter slot index, flipbook phase in `0..1`.
    pub rot_spin: [f32; 4],
    /// Initial linear RGBA.
    pub color: [f32; 4],
}

const _: () = assert!(size_of::<SpawnJobGpu>() == 64);

/// One fired event (WGSL `VfxEvent`, 32 bytes): where the parent was and what fired.
#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct EventGpu {
    /// Parent world position.
    pub position: [f32; 3],
    /// The event's seed ([`crate::events::event_seed`]).
    pub seed: u32,
    /// Parent velocity.
    pub velocity: [f32; 3],
    /// Trigger bit in the low 8 bits, parent emitter slot above.
    pub trigger_parent: u32,
}

const _: () = assert!(size_of::<EventGpu>() == 32);

/// One event link of an emitter slot (WGSL `EventLink`); the table holds
/// [`crate::format::MAX_EVENTS_PER_EMITTER`] per slot. All zero: no link.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Pod, Zeroable)]
pub struct EventLinkGpu {
    /// Trigger bit in the low 8 bits, children per firing above.
    pub trigger_count: u32,
    /// The child's emitter slot.
    pub child_slot: u32,
    /// Chance one firing spawns, `0..=1`.
    pub probability: f32,
    /// Fraction of the parent velocity each child inherits.
    pub inherit: f32,
}

const _: () = assert!(size_of::<EventLinkGpu>() == 16);

/// How a child emitter's particles start, for one instance (WGSL `ChildInit`, 11 × vec4): the
/// emitter's init module resolved to world space with the instance's scale and intensity.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Pod, Zeroable)]
pub struct ChildInitGpu {
    /// Shape kind (0 point, 1 sphere, 2 box, 3 cone), surface (0/1), radius or length min, max.
    pub shape: [f32; 4],
    /// Box half extents (xyz); tangent of the cone's half angle (w).
    pub shape2: [f32; 4],
    /// Velocity mode (0 radial, 1 directional, 2 planar), cosine of the spread (1: none), speed
    /// min, max.
    pub velocity: [f32; 4],
    /// Directional: the world direction (xyz). Planar: lift min, max (xy).
    pub velocity2: [f32; 4],
    /// The instance velocity the init module inherits (xyz); `w` 1 draws a random flipbook phase.
    pub inherited: [f32; 4],
    /// Lifetime min, max; size min, max (scaled).
    pub life_size: [f32; 4],
    /// Rotation min, max; spin min, max.
    pub rot_spin: [f32; 4],
    /// Initial linear RGBA (alpha scaled by the instance's intensity).
    pub color: [f32; 4],
    /// Instance right (xyz) and spatial scale (w).
    pub right: [f32; 4],
    /// Instance up (xyz) and the speed scale `sqrt(scale)` (w).
    pub up: [f32; 4],
    /// Instance forward (xyz); `w` unused.
    pub forward: [f32; 4],
}

const _: () = assert!(size_of::<ChildInitGpu>() == 176);

/// One decal record.
///
/// WGSL `Decal`: 10 × vec4 = 160 bytes.
/// `right`, `up`, `forward` carry the half extents in their `w` components.
/// A mask of `0` accepts every receiver layer (`u32::MAX` is uploaded as `0`).
#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct DecalGpu {
    /// Decal local right, w = half width.
    pub right: [f32; 4],
    /// Decal local up, w = half height.
    pub up: [f32; 4],
    /// Projection direction, w = half depth.
    pub forward: [f32; 4],
    /// Centre + lifetime fade.
    pub center_fade: [f32; 4],
    /// Linear albedo RGB + opacity.
    pub albedo_opacity: [f32; 4],
    /// Tangent normal xy, roughness, normal strength.
    pub normal_rough: [f32; 4],
    /// Linear emissive RGB + angle-fade start.
    pub emissive_angle0: [f32; 4],
    /// Angle-fade end, distance fade start/end, atlas layer (`< 0` = solid).
    pub fade_atlas: [f32; 4],
    /// Receiver mask, channel bits, edge softness, blend (`0` modulate, `1` replace).
    pub mask_channels: [f32; 4],
    /// Priority (CPU sort only) and padding.
    pub sort_pad: [f32; 4],
}

const _: () = assert!(size_of::<DecalGpu>() == 160);

/// One screen quad for a visible decal: the union of the 16 px tiles it touches.
///
/// WGSL `TileQuad` (32 bytes). `rect` is `(x0, y0, x1, y1)` in pixels with `x1`/`y1`
/// exclusive. `slot` indexes the decal storage buffer (not the pending-list index).
///
/// The shader padding is three `u32`s. A `vec3<u32>` would align to 16 and stretch
/// the stride to 48, so every instance after the first would shade the wrong rect.
#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct DecalTileGpu {
    /// Pixel rectangle `(x0, y0, x1, y1)`, exclusive max.
    pub rect: [f32; 4],
    /// Index into the uploaded decal buffer.
    pub slot: u32,
    /// Padding to 16-byte alignment.
    pub _pad: [u32; 3],
}

const _: () = assert!(size_of::<DecalTileGpu>() == 32);

/// One trail (ribbon) vertex, CPU-built.
///
/// WGSL `TrailVertex`: position (xyz) + side (+1/−1), uv `(t, side)`, color.
///
/// The two former padding slots carry optional shading flags; zero keeps the plain ribbon
/// (flat across with soft long edges, premultiplied over), bit for bit.
#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct TrailVertex {
    /// World position.
    pub position: [f32; 3],
    /// Additive share 0..1: the part of the vertex alpha that emits without dimming what is
    /// behind (1 = pure emission, for optically thin glowing gas). 0 = premultiplied over.
    pub _pad: f32,
    /// `(t along trail 0..1, side −1..1)`.
    pub uv: [f32; 2],
    /// `[soft, tile]`: `soft` above 0.5 shades the cross-section as a Gaussian of the side
    /// (`exp(-4.5 side^2)`) with no flat core and no outline; 0 is the plain soft-edged ribbon.
    /// `tile` above 0 takes the cross-section from sprite-atlas tile `tile - 1` instead, with
    /// `uv.x` running along the tile (wrapping) and the side across it (`.vfx.ron` v7 ribbons).
    pub _pad2: [f32; 2],
    /// Premultiplied linear RGBA.
    pub color: [f32; 4],
}

const _: () = assert!(size_of::<TrailVertex>() == 48);

impl TrailVertex {
    /// Vertex buffer layout matching the WGSL `TrailVertex`.
    pub fn layout() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: size_of::<TrailVertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &[
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x4,
                    offset: 0,
                    shader_location: 0,
                },
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x4,
                    offset: 16,
                    shader_location: 1,
                },
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x4,
                    offset: 32,
                    shader_location: 2,
                },
            ],
        }
    }
}

/// Counters cleared each frame: list lengths + alive count.
pub const COUNTER_COUNT: u32 = 4;
/// Counter indices: additive list length.
pub const COUNTER_ADDITIVE: u32 = 0;
/// Counter indices: alpha list length.
pub const COUNTER_ALPHA: u32 = 1;
/// Counter indices: mesh list length.
pub const COUNTER_MESH: u32 = 2;
/// Counter indices: live particles.
pub const COUNTER_ALIVE: u32 = 3;

/// Persistent stats: spawn jobs run, dropped-particle total, then the event words.
pub const STATS_COUNT: u32 = 11;
/// Stats indices: spawn jobs the emit kernel has run (wrapping). Slots come from the host's
/// ring cursor (`FrameGpu::window`), not from this counter.
pub const STATS_CURSOR: u32 = 0;
/// Stats indices: total particles dropped by the ring (budget pressure).
pub const STATS_DROPPED: u32 = 1;
/// Stats indices: one past the highest slot ordinal that staged an event this frame
/// (`fire_event` tracks it with an order-independent atomicMax; `event_prepare` scans the
/// fired-slot mask only up to it, clamped to the mask's capacity, then resets the slot to 0).
/// The name predates the rank-staging removal (fixrev-22-k); the slot is no longer an append
/// count.
pub const STATS_EVENT_APPEND: u32 = 2;
/// Stats indices: events the child-emit dispatch consumes this frame.
pub const STATS_EVENT_CONSUME: u32 = 3;
/// Stats indices: events fired since the context was created, overflow included (wrapping).
pub const STATS_EVENT_FIRED: u32 = 4;
/// Stats indices: events lost to a full event buffer (wrapping).
pub const STATS_EVENT_OVERFLOW: u32 = 5;
/// Stats indices: the child ring's cursor.
pub const STATS_CHILD_CURSOR: u32 = 6;
/// Stats indices: child particles spawned (wrapping).
pub const STATS_CHILD_SPAWNED: u32 = 7;
/// Stats indices: child particles lost: a live child overwritten, or past one ring per frame.
pub const STATS_CHILD_DROPPED: u32 = 8;
/// Stats indices: children claimed this frame (reset by the event prepare kernel).
pub const STATS_CHILD_FRAME: u32 = 9;
/// Stats indices: fired-slot mask bits the event prepare kernel has visited since the context
/// was created (wrapping): a diagnostic of its scan cost, which is proportional to the slots
/// that fired since the last prepare, not to the window (fixrev5-vfx-events-vt). Crate-private:
/// read by the determinism tests, not part of `VfxPoolStats`.
pub(crate) const STATS_EVENT_SCAN: u32 = 10;
const _: () = assert!(STATS_EVENT_SCAN < STATS_COUNT);

/// Curve LUT stride (scalars per curve / colour samples per curve).
pub const LUT: u32 = CURVE_SAMPLES;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pod_sizes_match_wgsl_layouts() {
        // 384 before the two sky rows of ADR-1425, 432 before the event row of ADR-2642.
        assert_eq!(size_of::<FrameGpu>(), 448);
        assert_eq!(size_of::<EmitterGpu>(), 272);
        assert_eq!(size_of::<EventGpu>(), 32);
        assert_eq!(size_of::<EventLinkGpu>(), 16);
        assert_eq!(size_of::<ChildInitGpu>(), 176);
        assert_eq!(size_of::<SpawnJobGpu>(), 64);
        assert_eq!(size_of::<DecalGpu>(), 160);
        assert_eq!(size_of::<TrailVertex>(), 48);
    }
}
