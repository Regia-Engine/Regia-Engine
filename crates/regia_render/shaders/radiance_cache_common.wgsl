// SPDX-License-Identifier: MPL-2.0
// The world-space radiance cache: layout and addressing (`radiance_cache.rs`, `specs/79` § 12).
//
// This file has no bindings. A host shader declares, under these names:
//   rc: RcUniform (uniform)
//   rc_cells: array<u32> (storage; read in the gather, read_write in the update)
// and then appends this text.
//
// Nested camera-centered grids (clipmap levels), addressed toroidally like the probes: a cell
// keeps its slot while the grid scrolls, so a scroll rewrites only the cells that enter. Each
// cell holds the radiance arriving at its center as an ambient cube rebuilt from the probes'
// irradiance (`radiance_cache_update.wgsl`; RGB9E5, no exposure), and a tag: its world cell, bit 31
// set when the probes covered it.
// A slot whose tag names another cell has not been rewritten since the scroll and reads as absent.

struct RcUniform {
    // Per level: xyz = world cell of the lowest corner.
    bases: array<vec4<i32>, 4>,
    // Per level: cell spacing (m).
    spacing: vec4<f32>,
    // xyz = cells per axis, w = levels.
    dims: vec4<u32>,
    // x = cells this frame's update writes; y = 1: the gather counts its work (ADR-2605).
    counts: vec4<u32>,
    view_to_world: mat4x4<f32>,
    // x = pre-exposure, y = ray length (m), z = strength, w = miss cap (0: a miss adds nothing).
    gather: vec4<f32>,
    // x = how far the difference may take a pixel down, as a share of the probe light it holds;
    // y = rays a pixel marches this frame (1, 2, 4 or 8), z = steps a ray (2..8), w = 1: a ray that
    // reaches the sky stops there (ADR-2605).
    limits: vec4<f32>,
}

const RC_CELL_WORDS: u32 = 7u;
const RC_TAG_WORD: u32 = 6u;
// `rc_level_at`: no level holds the point.
const RC_NO_LEVEL: u32 = 99u;

fn rc_count() -> u32 {
    return rc.dims.x * rc.dims.y * rc.dims.z;
}

fn rc_slot(level: u32, cell: vec3<i32>) -> u32 {
    let d = vec3<i32>(rc.dims.xyz);
    let s = ((cell % d) + d) % d;
    return level * rc_count() + u32(s.x + d.x * (s.y + d.y * s.z));
}

fn rc_tag(cell: vec3<i32>) -> u32 {
    let c = vec3<u32>(cell & vec3<i32>(1023));
    return c.x | (c.y << 10u) | (c.z << 20u) | (1u << 31u);
}

fn rc_unpack_rgb9e5(v: u32) -> vec3<f32> {
    let s = exp2(f32(i32(v >> 27u) - 24));
    return vec3<f32>(f32(v & 511u), f32((v >> 9u) & 511u), f32((v >> 18u) & 511u)) * s;
}

// The finest level whose cells all around `p` lie inside its grid.
fn rc_level_at(p: vec3<f32>) -> u32 {
    let hi = vec3<f32>(rc.dims.xyz) - vec3<f32>(1.0);
    for (var l = 0u; l < rc.dims.w; l = l + 1u) {
        let g = p / rc.spacing[l] - vec3<f32>(rc.bases[l].xyz);
        if (all(g >= vec3<f32>(0.0)) && all(g < hi)) {
            return l;
        }
    }
    return RC_NO_LEVEL;
}
