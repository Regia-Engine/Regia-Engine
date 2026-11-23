// SPDX-License-Identifier: MPL-2.0
// regia_looks step-cut pavilion (ADR-2587), shared by the standalone renderer and the viewer hooks.
// Pure functions: no bindings. The stone is in normalized coordinates (x and y by the half extents,
// z by the half width), so a princess outline is the square |x|, |y| <= 1 at the girdle.
//
// A four-sided pyramid in place of the round cut's cone, its sides broken into chevron tiers, each
// with its own tilt. Exit directions fan out by facet, so whole facets light up and go dark as the
// view turns. Fixed math, no data-dependent loop: three plane tests per channel at most.

const LOOKS_STEP_GIRDLE: f32 = 0.024;
const LOOKS_STEP_PAV_TAN: f32 = 0.8613;
const LOOKS_STEP_CROWN_Z: f32 = 0.347;
const LOOKS_STEP_TABLE: f32 = 0.53;

struct LooksStepExit {
    // Exit direction in normalized stone space, not yet unit length in object space.
    dir: vec3<f32>,
    // 1 for a clean exit, less for total internal reflection and leaks.
    weight: f32,
    // The pavilion facets the ray met, stable across frames.
    facet: u32,
};

// Distance along `d` to the pyramid side the ray is heading for; a large value when it heads for
// none. The vector holds the four sides +x, -x, +y, -y.
fn looks_step_dist(p: vec3<f32>, d: vec3<f32>) -> f32 {
    let oz = p.z + LOOKS_STEP_GIRDLE + LOOKS_STEP_PAV_TAN;
    let sp = vec4<f32>(p.x, -p.x, p.y, -p.y);
    let sd = vec4<f32>(d.x, -d.x, d.y, -d.y);
    let num = max(vec4<f32>(oz) - LOOKS_STEP_PAV_TAN * sp, vec4<f32>(0.0));
    let den = LOOKS_STEP_PAV_TAN * sd - vec4<f32>(d.z);
    let t = select(vec4<f32>(1e6), num / max(den, vec4<f32>(1e-4)), den > vec4<f32>(1e-4));
    return min(min(t.x, t.y), min(t.z, t.w));
}

// The chevron tier under a point on the pavilion: V-shaped bands, 0 at the culet to 3 at the girdle.
fn looks_step_tier(h: vec3<f32>) -> f32 {
    let r = max(abs(h.x), abs(h.y));
    let l = min(abs(h.x), abs(h.y));
    return clamp(floor((r - 0.45 * l) * 4.0 / 0.8), 0.0, 3.0);
}

// The pavilion facet's outward normal at a point: a side, a tier, a left or right half of the V.
fn looks_step_normal(h: vec3<f32>) -> vec3<f32> {
    let x_side = abs(h.x) >= abs(h.y);
    let u = select(vec2<f32>(0.0, select(-1.0, 1.0, h.y >= 0.0)), vec2<f32>(select(-1.0, 1.0, h.x >= 0.0), 0.0), x_side);
    let w = vec2<f32>(-u.y, u.x);
    let tier = looks_step_tier(h);
    // Steeper toward the girdle, the V opening wider tier by tier.
    let angle = 0.66 + 0.075 * tier;
    let chevron = 0.2 + 0.11 * tier;
    let side = select(-1.0, 1.0, dot(h.xy, w) >= 0.0);
    let dir = u * cos(chevron) + w * side * sin(chevron);
    return vec3<f32>(dir * sin(angle), -cos(angle));
}

// A stable id of the pavilion facet at a point.
fn looks_step_facet(h: vec3<f32>) -> u32 {
    let x_side = abs(h.x) >= abs(h.y);
    let along = select(h.y, h.x, x_side);
    let across = select(h.x, h.y, x_side);
    let side = select(0u, 1u, x_side) * 2u + select(0u, 1u, along >= 0.0);
    return side * 8u + u32(looks_step_tier(h)) * 2u + select(0u, 1u, across >= 0.0);
}

// The crown facet a ray leaves through: the table, then two step tiers leaning toward their side.
fn looks_step_crown(e: vec3<f32>) -> vec3<f32> {
    let r = max(abs(e.x), abs(e.y));
    let x_side = abs(e.x) >= abs(e.y);
    let u = select(vec2<f32>(0.0, select(-1.0, 1.0, e.y >= 0.0)), vec2<f32>(select(-1.0, 1.0, e.x >= 0.0), 0.0), x_side);
    let angle = select(0.62, 0.38, r < 0.78);
    let tier = vec3<f32>(u * sin(angle), cos(angle));
    return select(tier, vec3<f32>(0.0, 0.0, 1.0), r < LOOKS_STEP_TABLE);
}

// One channel of the internal path: in through the crown, up to three pavilion bounces (each only
// when the ray reaches the pavilion before the crown), out through the crown facet above.
fn looks_step_exit(p0: vec3<f32>, d0: vec3<f32>, eta: f32) -> LooksStepExit {
    let h1 = p0 + d0 * clamp(looks_step_dist(p0, d0), 0.0, 4.0);
    let d1 = reflect(d0, looks_step_normal(h1));
    let t2 = looks_step_dist(h1, d1);
    let up1 = (LOOKS_STEP_CROWN_Z - h1.z) / max(d1.z, 1e-3);
    let second = d1.z <= 0.0 || t2 < up1;
    let h2c = h1 + d1 * clamp(t2, 0.0, 4.0);
    let h2 = select(h1, h2c, second);
    let d2 = select(d1, reflect(d1, looks_step_normal(h2c)), second);
    let t3 = looks_step_dist(h2, d2);
    let up2 = (LOOKS_STEP_CROWN_Z - h2.z) / max(d2.z, 1e-3);
    let third = second && (d2.z <= 0.0 || t3 < up2);
    let h3c = h2 + d2 * clamp(t3, 0.0, 4.0);
    let h3 = select(h2, h3c, third);
    let d3 = select(d2, reflect(d2, looks_step_normal(h3c)), third);
    // Leaving upward through the crown.
    let t_up = (LOOKS_STEP_CROWN_Z - h3.z) / max(d3.z, 1e-3);
    let e_pt = h3 + d3 * clamp(t_up, 0.0, 4.0);
    let e_up = refract(d3, -looks_step_crown(e_pt), eta);
    let tir = dot(e_up, e_up) < 1e-6;
    // Total internal reflection at the crown: the light wanders and leaves dimmer, steeper.
    let e_tir = normalize(vec3<f32>(-d3.x, -d3.y, abs(d3.z) + 0.6));
    // Still running down after the last bounce: it leaks out of the pavilion.
    let leak = d3.z <= 0.0;
    // Built with the constructor, not a `var` (naga gives every struct an explicit layout, which
    // VUID-StandaloneSpirv-None-10684 forbids in Function storage; ADR-7587).
    return LooksStepExit(
        select(select(e_up, e_tir, tir), d3, leak),
        select(select(1.0, 0.6, tir), 0.3, leak),
        // Both ends of the path: panes that share a last facet still differ by the first one they met.
        looks_step_facet(h1) * 37u + looks_step_facet(h3),
    );
}
