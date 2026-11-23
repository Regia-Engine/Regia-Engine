// SPDX-License-Identifier: MPL-2.0
// The product viewer's shared shader text: bindings, the surface a hook receives, and the environment
// helpers. A family hook (family_*.wgsl) defines `fn shade_surface(s: Surface) -> vec3<f32>` and returns
// linear radiance before exposure. entry.wgsl adds the entry points, exposure, tone mapping and output.
//
// WebGL2 is a target: uniform buffers and textures only, no storage buffers, no compute, no loops over
// scene data. Keep hooks cheap: they run per pixel on a phone.

struct FrameBlock {
    view_proj: mat4x4<f32>,
    // xyz: eye position in mm. w: exposure multiplier.
    eye: vec4<f32>,
    // Diffuse environment as second-order spherical harmonics, rgb per coefficient, already convolved
    // with the cosine lobe and divided by pi: a white diffuse surface reflects env_diffuse(n).
    sh: array<vec4<f32>, 9>,
    // x: last mip of env_cube. y: 1 when the target stores encoded sRGB without hardware conversion.
    // z: contact shadow opacity. w: reserved.
    env: vec4<f32>,
    // The contact shadow plane: xyz: center in mm. w: softness in mm.
    shadow_center: vec4<f32>,
    // xy: half extents of the piece's footprint in mm (world x and y). zw: reserved.
    shadow_extent: vec4<f32>,
    // Three fixed studio lights for glints: xyz the unit direction toward the light, w unused.
    key_dir: array<vec4<f32>, 3>,
    // Their linear radiance: rgb, w unused.
    key_col: array<vec4<f32>, 3>,
}

struct MaterialBlock {
    base: vec4<f32>,
    p0: vec4<f32>,
    p1: vec4<f32>,
    p2: vec4<f32>,
}

// Group 0: per frame.
@group(0) @binding(0) var<uniform> frame: FrameBlock;
@group(0) @binding(1) var env_cube: texture_cube<f32>;
@group(0) @binding(2) var env_sampler: sampler;
@group(0) @binding(3) var dfg_lut: texture_2d<f32>;
@group(0) @binding(4) var lut_atlas: texture_2d<f32>;
@group(0) @binding(5) var clamp_sampler: sampler;

// Group 1: per material, one block behind a dynamic offset. A material swap writes bytes here.
@group(1) @binding(0) var<uniform> material: MaterialBlock;

// Group 2: textures a look may use, default 1x1 white (ViewerRenderer::set_look_texture).
@group(2) @binding(0) var look_tex0: texture_2d<f32>;
@group(2) @binding(1) var look_tex1: texture_2d<f32>;
@group(2) @binding(2) var look_sampler: sampler;

// What a hook gets for one pixel.
struct Surface {
    // World position in mm.
    world_pos: vec3<f32>,
    // Unit shading normal, facing the viewer. A cut stone's mesh carries one normal per facet.
    normal: vec3<f32>,
    // Unit direction from the surface to the eye.
    view: vec3<f32>,
    // dot(normal, view), at least 0.001.
    n_dot_v: f32,
    uv: vec2<f32>,
    // Position in the mesh's own space in mm (stable under the piece's transform).
    local_pos: vec3<f32>,
    // Unit normal in the mesh's own space.
    local_normal: vec3<f32>,
    // The mesh's facet number (0 on smooth parts). The same over a whole facet.
    facet: f32,
    // The instance's unit axes in world space, as columns: `s.basis * d` turns a direction from the mesh's
    // space into the world and `s.basis[2]` is a stone's axis. The entry shader builds `s.basis`
    // from normalized matrix columns, so it is orthonormal and `transpose(s.basis) * d` is the
    // exact way back; a non-orthonormal basis would need its true inverse instead.
    basis: mat3x3<f32>,
}

// Mirror-sharp to fully rough environment light along `dir`. `roughness` is perceptual.
fn env_radiance(dir: vec3<f32>, roughness: f32) -> vec3<f32> {
    let lod = sqrt(clamp(roughness, 0.0, 1.0)) * frame.env.x;
    return textureSampleLevel(env_cube, env_sampler, dir, lod).rgb;
}

// The light a white diffuse surface with this normal reflects.
fn env_diffuse(n: vec3<f32>) -> vec3<f32> {
    let x = n.x;
    let y = n.y;
    let z = n.z;
    var c = frame.sh[0].rgb * 0.282095;
    c += (frame.sh[1].rgb * y + frame.sh[2].rgb * z + frame.sh[3].rgb * x) * 0.488603;
    c += (frame.sh[4].rgb * x * y + frame.sh[5].rgb * y * z + frame.sh[7].rgb * x * z) * 1.092548;
    c += frame.sh[6].rgb * (3.0 * z * z - 1.0) * 0.315392;
    c += frame.sh[8].rgb * (x * x - y * y) * 0.546274;
    return max(c, vec3<f32>(0.0));
}

// The split-sum scale and bias of a GGX lobe: F0 * x + y.
fn dfg(n_dot_v: f32, roughness: f32) -> vec2<f32> {
    // Texel centers of the 32 x 32 table.
    let uv = vec2<f32>(n_dot_v, roughness) * (31.0 / 32.0) + vec2<f32>(0.5 / 32.0);
    return textureSampleLevel(dfg_lut, clamp_sampler, uv, 0.0).rg;
}

// A row of the small lookup atlas at `coord`, 0..1 along the row.
fn lut_sample(row: f32, coord: f32) -> vec3<f32> {
    // 64 rows: LUT_ROWS in renderer.rs.
    let uv = vec2<f32>(clamp(coord, 0.0, 1.0), (row + 0.5) / 64.0);
    return textureSampleLevel(lut_atlas, clamp_sampler, uv, 0.0).rgb;
}

// Widens a roughness where the normal changes fast across a pixel, so highlights on small curved parts
// stay stable while dragging.
fn antialiased_roughness(n: vec3<f32>, roughness: f32) -> f32 {
    let du = dpdx(n);
    let dv = dpdy(n);
    let variance = 0.25 * (dot(du, du) + dot(dv, dv));
    let kernel = min(2.0 * variance, 0.18);
    let a = roughness * roughness;
    let a2 = clamp(a * a + kernel, 0.0, 1.0);
    return sqrt(sqrt(a2));
}

// Environment reflection of a conductor-like lobe with the reflectance `f0`, with the energy a single
// scattering lobe loses put back.
fn env_specular(s: Surface, f0: vec3<f32>, roughness: f32) -> vec3<f32> {
    let r = reflect(-s.view, s.normal);
    let ab = dfg(s.n_dot_v, roughness);
    let single = f0 * ab.x + vec3<f32>(ab.y);
    let ess = max(ab.x + ab.y, 0.001);
    let multi = vec3<f32>(1.0) + f0 * (1.0 / ess - 1.0);
    return env_radiance(r, roughness) * single * multi;
}

// Schlick's Fresnel for a dielectric with this index, viewer in air.
fn fresnel_dielectric(cos_theta: f32, eta: f32) -> f32 {
    let f = (eta - 1.0) / (eta + 1.0);
    let f0 = f * f;
    let m = 1.0 - clamp(cos_theta, 0.0, 1.0);
    let m2 = m * m;
    return f0 + (1.0 - f0) * m2 * m2 * m;
}

// A stable pseudo-random number 0..1 from a number (PCG hash): no time in it, the same on every GPU.
fn hash11(x: f32) -> f32 {
    let state = bitcast<u32>(x) * 747796405u + 2891336453u;
    let word = ((state >> ((state >> 28u) + 4u)) ^ state) * 277803737u;
    return f32((word >> 22u) ^ word) / 4294967295.0;
}
