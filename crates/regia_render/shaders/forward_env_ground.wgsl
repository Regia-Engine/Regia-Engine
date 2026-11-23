// SPDX-License-Identifier: MPL-2.0
#define_import_path regia::forward::env_ground

// The image light's ground plane (ADR-2000; `env_ground.rs` is the CPU mirror, its doc the design):
// what share of a surface's view a horizontal plane or disc fills, in the diffuse term and in the
// reflection. `g` is the frame's `env_ground`: height, radius (0 = unbounded), centre x, z.

// The form factor of a spherical cap with its horizon: Lagarde & de Rousiers 2014, § 4.7.2.
fn env_cap_form_factor(cos_theta: f32, sin_sigma_sq: f32) -> f32 {
    let s2 = clamp(sin_sigma_sq, 1e-6, 1.0);
    if (cos_theta * cos_theta > s2) {
        return s2 * clamp(cos_theta, 0.0, 1.0);
    }
    let sin_theta = max(sqrt(max(1.0 - cos_theta * cos_theta, 0.0)), 1e-5);
    let x = sqrt(max(1.0 / s2 - 1.0, 0.0));
    let y = clamp(-x * cos_theta / sin_theta, -1.0, 1.0);
    let w = sin_theta * sqrt(max(1.0 - y * y, 0.0));
    let e = (cos_theta * acos(y) - x * w) * s2 + atan2(w, x);
    return clamp(e / 3.14159265, 0.0, 1.0);
}

// x = the plane's share of the cosine-weighted hemisphere around `n`, y = the share of the image's
// lower-hemisphere light it hides.
fn env_ground_cover(p: vec3<f32>, n: vec3<f32>, g: vec4<f32>) -> vec2<f32> {
    var h = p.y - g.x;
    if (h < -0.01) {
        return vec2<f32>(0.0);
    }
    h = max(h, 1e-3);
    let lower = 0.5 * (1.0 - n.y);
    var cos_theta = -n.y;
    var sin_sigma_sq = 1.0;
    if (g.y > 0.0) {
        // The cap's two ends are the disc's near and far edges seen from the point.
        let to_centre = g.zw - p.xz;
        let offset = length(to_centre);
        let near_edge = atan((g.y - offset) / h);
        let far_edge = atan((g.y + offset) / h);
        let half_angle = 0.5 * (near_edge + far_edge);
        let tilt = 0.5 * (far_edge - near_edge);
        let u = select(vec2<f32>(0.0), to_centre / max(offset, 1e-6), offset > 1e-6);
        let axis = vec3<f32>(sin(tilt) * u.x, -cos(tilt), sin(tilt) * u.y);
        cos_theta = dot(n, axis);
        // Past the edge the disc is a thin ellipse wider than its in-plane cap: its projected
        // area sets the cap instead (exact straight above the centre).
        let s = sin(half_angle);
        let distance_sq = offset * offset + h * h;
        let projected = g.y * g.y * h / sqrt(distance_sq);
        sin_sigma_sq = max(s * s, projected / (projected + distance_sq));
    }
    let f = env_cap_form_factor(cos_theta, sin_sigma_sq);
    return vec2<f32>(f, clamp(f / max(lower, 1e-4), 0.0, 1.0));
}

// The share of a reflection of direction `r` from `p` that lands on the plane.
fn env_ground_reflection(p: vec3<f32>, r: vec3<f32>, perceptual: f32, g: vec4<f32>) -> f32 {
    let h = p.y - g.x;
    if (h < -0.01) {
        return 0.0;
    }
    let spread = 0.01 + perceptual * perceptual;
    let below = smoothstep(-spread, spread, -r.y);
    if (g.y <= 0.0) {
        return below;
    }
    let t = max(h, 0.0) / -min(r.y, -0.5 * spread);
    let hit = p.xz + r.xz * t - g.zw;
    let edge = 0.02 + perceptual;
    return below * (1.0 - smoothstep(g.y * (1.0 - edge), g.y * (1.0 + edge), length(hit)));
}
