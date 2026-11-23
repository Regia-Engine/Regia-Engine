// SPDX-License-Identifier: MPL-2.0
// regia_looks opal play of color (ADR-2587), shared by the standalone renderer and the viewer hook.
// No bindings. Each small domain is a grating with its own orientation (Bragg: lambda = 2 d n cos t,
// Sanders 1964). The hue swing with the view is widened past the physical value so it reads on a
// small stone. Fixed 3x3 cell loop, no texture read.

fn looks_opal_hash(x: u32) -> f32 {
    var h = x;
    h = h ^ (h >> 16u);
    h = h * 0x7feb352du;
    h = h ^ (h >> 15u);
    h = h * 0x846ca68bu;
    h = h ^ (h >> 16u);
    return f32(h & 0xffffu) / 65535.0;
}

fn looks_opal_spectral(lambda_nm: f32) -> vec3<f32> {
    let r = exp(-pow((lambda_nm - 605.0) / 45.0, 2.0)) + 0.25 * exp(-pow((lambda_nm - 430.0) / 25.0, 2.0));
    let g = exp(-pow((lambda_nm - 545.0) / 45.0, 2.0));
    let b = exp(-pow((lambda_nm - 455.0) / 35.0, 2.0));
    return vec3<f32>(r, g, b);
}

// A slow cloudy value in 0..1 for the body, in millimeters. Stable: position only.
fn looks_opal_cloud(p_mm: vec2<f32>) -> f32 {
    let a = sin(p_mm.x * 1.9 + 1.3 * sin(p_mm.y * 1.3 + 0.7));
    let b = sin(p_mm.y * 2.3 + 1.1 * sin(p_mm.x * 1.7 + 2.1));
    return 0.5 + 0.25 * (a + b);
}

// The play at a surface point. rgb: added color. a: how much of the body a lit domain covers.
// `footprint` is the pixel size in cell units (fwidth of position over the cell size, taken at the
// caller's top level): domains too small to resolve fade out instead of shimmering.
fn looks_opal_play(p_mm: vec3<f32>, n_obj: vec3<f32>, v_obj: vec3<f32>, cell_mm: f32, strength: f32, footprint: f32) -> vec4<f32> {
    // The domains sit under the surface: they slide against it as the stone turns. The radial term
    // follows the dome's arc, so domains keep their shape on the steep rim instead of streaking.
    let t = refract(-v_obj, n_obj, 1.0 / 1.45);
    let arc = p_mm.xy * (1.0 + 0.6 * (1.0 - clamp(n_obj.z, 0.0, 1.0)));
    let under = arc + t.xy * cell_mm * 1.5;
    var q = under / cell_mm;
    // Warp so domains are not polygons.
    q = q + 0.3 * vec2<f32>(sin(q.y * 1.7 + 1.3), sin(q.x * 1.3 + 4.1));
    let base = floor(q);
    var f1 = 1e9;
    var f2 = 1e9;
    var best_id = 0u;
    var best_off = vec2<f32>(0.0);
    for (var dy = -1; dy <= 1; dy = dy + 1) {
        for (var dx = -1; dx <= 1; dx = dx + 1) {
            let cell = base + vec2<f32>(f32(dx), f32(dy));
            let id = u32(i32(cell.x) + 4096) * 8191u + u32(i32(cell.y) + 4096);
            let off = cell + vec2<f32>(looks_opal_hash(id), looks_opal_hash(id + 77u)) - q;
            let d = length(off);
            if (d < f1) {
                f2 = f1;
                f1 = d;
                best_id = id;
                best_off = off;
            } else if (d < f2) {
                f2 = d;
            }
        }
    }
    // The domain's grating, bent a little across the domain so its hue rolls inside it.
    let ga = looks_opal_hash(best_id + 11u) * 6.2831853;
    let gt = 0.75 * sqrt(looks_opal_hash(best_id + 13u));
    let bend = -best_off * 0.28;
    let g = normalize(vec3<f32>(cos(ga) * gt + bend.x, sin(ga) * gt + bend.y, 1.0));
    let cos_t = max(dot(g, v_obj), 0.0);
    // Neighbors share a color zone: most of the spacing follows a slow field over the stone, so the
    // play forms patches of related hues rather than an even mix of every color.
    let zone = clamp(0.5 + 1.6 * (looks_opal_cloud(arc * 0.8 + vec2<f32>(3.1, 1.7)) - 0.5), 0.0, 1.0);
    let spacing = mix(150.0, 240.0, 0.7 * zone + 0.3 * looks_opal_hash(best_id + 17u));
    // Bragg, plus the widened swing with the view; folded back so it stays in the visible range.
    let swing_dir = vec2<f32>(cos(zone * 6.2831853), sin(zone * 6.2831853));
    let swing = 280.0 * dot(swing_dir, v_obj.xy - bend);
    let raw = 2.0 * spacing * 1.45 * cos_t + swing;
    let lambda = 420.0 + 260.0 * abs(fract(raw / 520.0) * 2.0 - 1.0);
    // A lit domain faces the viewer within its lobe, so a turn lights some and darkens others.
    let lobe = smoothstep(0.86, 0.985, cos_t);
    let soft = smoothstep(0.0, 0.3, f2 - f1);
    let weight = 0.25 + 0.75 * looks_opal_hash(best_id + 19u);
    let fade = 1.0 - smoothstep(0.55, 1.3, footprint);
    let flash = lobe * soft * weight * strength * fade;
    return vec4<f32>(looks_opal_spectral(lambda) * flash * 1.7, flash);
}
