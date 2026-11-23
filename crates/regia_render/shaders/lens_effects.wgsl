// SPDX-License-Identifier: MPL-2.0
// Runtime lens effects (ADR-1863, `regia_render::lens_effects`): ghost flare, lens dirt and
// lateral chromatic aberration over the scene-referred HDR image, before the post stack.
//
// Original code. The ghost model (two-bounce ghosts with a traced colour, position and size per
// ghost, each an image of the aperture) is from Hullin, Eisemann, Seidel and Lee, "Physically-Based
// Real-Time Lens Flare Rendering", ACM TOG 30(4), 2011, through the CPU reference
// `post_ref/lens_flare.rs`. Standing in for their per-light ghost meshes, the bright image is
// reduced to a small grid of cells (energy, centroid, spread) and each lit cell draws one aperture
// sprite per ghost: Regia's simplification, which keeps the aperture's edge and a bounded cost.
// Lateral chromatic aberration as a per-channel magnification about the optical centre is the
// textbook definition (Hecht, "Optics", § 6.3).
//
// Passes (one module, seven entry points):
// - `fs_bright`: the colour target (inside the viewport) to a quarter-size bright image: each
//   pixel keeps only its luminance above the threshold, then 4 x 4 pixels are averaged, so a
//   small light casts instead of diluting below the threshold.
// - `fs_down`: one mip of the bright image to the next (4-tap box).
// - `fs_cells`: the bright image to the grid of light cells.
// - `vs_sprite`, `fs_sprite`: the ghosts, at the bright image's size, one instance per ghost and
//   cell (a dark cell's instances collapse).
// - `fs_add`: flare plus dirt, added onto the colour target (no chromatic aberration).
// - `fs_replace`: the chromatic aberration of a copy of the colour target, plus flare and dirt.

const MAX_GHOSTS: u32 = 8u;
/// Largest radiance a bright texel keeps (half-float headroom).
const MAX_BRIGHT: f32 = 60000.0;
/// Fraction of the glare reaching the front element that its dirt scatters toward the sensor at
/// `dirt_intensity` 1 (`DIRT_SCATTER` in `lens_effects/gpu.rs`).
const DIRT_SCATTER: f32 = 0.02;
/// Most light the pass adds to a texel: twelve stops over display white, where every tonemap is
/// long since white. The sum with the scene stays a finite half float (the post stack turns a
/// non-finite texel black).
const MAX_LENS_LIGHT: f32 = 4096.0;
/// The grid of light cells (`CELLS_X` and `CELLS_Y` in `lens_effects/gpu.rs`).
const CELLS_X: u32 = 16u;
const CELLS_Y: u32 = 9u;
/// Taps per cell side when the grid reads the bright image.
const CELL_TAPS: u32 = 8u;
/// The bright image is this many times smaller than the viewport (`REDUCTION` in `gpu.rs`).
const REDUCTION: f32 = 4.0;
/// The iris: blades, rotation in radians, and how far the disc is drawn toward the polygon
/// (a slightly curved blade, 1 is straight).
const IRIS_BLADES: f32 = 6.0;
const IRIS_ROTATION: f32 = 0.35;
const IRIS_POLYGON: f32 = 0.85;
/// Area of that aperture at circumradius 1 (a disc is 3.14, the hexagon 2.60).
const IRIS_AREA: f32 = 2.68;
/// Edge softness of a ghost of a point light, a fraction of its radius.
const IRIS_EDGE: f32 = 0.05;
/// How much a cell's spatial spread adds to a ghost's radius: a broad source's ghost is the
/// aperture blurred by the source, so it grows and softens instead of drawing a lattice of sharp
/// sprites, one per cell.
const SPREAD_SCALE: f32 = 1.6;

struct LensParams {
    /// The view rectangle inside the colour target: x, y, w, h in pixels.
    viewport: vec4<f32>,
    /// threshold (in the target's pre-exposed units), flare intensity, dirt intensity, chromatic
    /// aberration.
    strengths: vec4<f32>,
    /// ghost count, the bright image's top mip, the viewport aspect (w / h), the dirt's glare mip.
    info: vec4<f32>,
    /// Per ghost: magnification at red, green, blue; footprint radius (fraction of the width).
    ghost_scale: array<vec4<f32>, 8>,
    /// Per ghost: traced energy at red, green, blue; unused.
    ghost_energy: array<vec4<f32>, 8>,
    /// Starburst intensity; the bright mip the spikes gather from; unused; unused.
    extra: vec4<f32>,
}

@group(0) @binding(0) var source: texture_2d<f32>;
@group(0) @binding(1) var bright: texture_2d<f32>;
@group(0) @binding(2) var flare: texture_2d<f32>;
@group(0) @binding(3) var dirt: texture_2d<f32>;
@group(0) @binding(4) var linear_clamp: sampler;
@group(0) @binding(5) var<uniform> params: LensParams;
/// The light cells, read per sprite in the vertex stage: energy, and in the second texture the
/// centroid (viewport uv) and the spread (width units).
@group(1) @binding(0) var cell_energy: texture_2d<f32>;
@group(1) @binding(1) var cell_place: texture_2d<f32>;

struct FullOut {
    @builtin(position) pos: vec4<f32>,
}

@vertex
fn vs_main(@builtin(vertex_index) i: u32) -> FullOut {
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    return FullOut(vec4<f32>(p * 2.0 - 1.0, 0.0, 1.0));
}

/// `true` unless a component is NaN or infinite (a bit test: the compiler may assume finite
/// maths and fold `x != x`).
fn finite3(v: vec3<f32>) -> bool {
    return all((bitcast<vec3<u32>>(v) << vec3<u32>(1u)) < vec3<u32>(0xff000000u));
}

fn luminance(c: vec3<f32>) -> f32 {
    return dot(c, vec3<f32>(0.2126, 0.7152, 0.0722));
}

/// Average of four bilinear taps one source texel off `uv` diagonally (a 4 x 4 box at a 2x or
/// 4x reduction).
fn box4(t: texture_2d<f32>, uv: vec2<f32>, texel: vec2<f32>) -> vec3<f32> {
    let a = textureSampleLevel(t, linear_clamp, uv + vec2<f32>(-texel.x, -texel.y), 0.0).rgb;
    let b = textureSampleLevel(t, linear_clamp, uv + vec2<f32>(texel.x, -texel.y), 0.0).rgb;
    let c = textureSampleLevel(t, linear_clamp, uv + vec2<f32>(-texel.x, texel.y), 0.0).rgb;
    let d = textureSampleLevel(t, linear_clamp, uv + vec2<f32>(texel.x, texel.y), 0.0).rgb;
    return 0.25 * (a + b + c + d);
}

/// Texture coordinate of the colour target for a viewport uv (clamped inside the viewport).
fn source_uv(uv: vec2<f32>) -> vec2<f32> {
    let size = vec2<f32>(textureDimensions(source, 0));
    let vp = params.viewport;
    let px = vp.xy + clamp(uv, vec2<f32>(0.0), vec2<f32>(1.0)) * vp.zw;
    return px / size;
}

/// One pixel of the source: finite, clamped, and only the part above the threshold.
fn bright_pixel(at: vec2<i32>) -> vec3<f32> {
    var c = textureLoad(source, at, 0).rgb;
    if !finite3(c) {
        c = vec3<f32>(0.0);
    }
    c = min(max(c, vec3<f32>(0.0)), vec3<f32>(MAX_BRIGHT));
    let lum = luminance(c);
    let threshold = params.strengths.x;
    return c * max(lum - threshold, 0.0) / max(lum, 1.0e-4);
}

@fragment
fn fs_bright(in: FullOut) -> @location(0) vec4<f32> {
    // Mip 0's texel `k` covers the viewport's pixels 4k..4k+3 (the last row and column clamp:
    // the image is rounded up to whole texels).
    let first = vec2<i32>(in.pos.xy) * i32(REDUCTION) + vec2<i32>(params.viewport.xy);
    let last = vec2<i32>(params.viewport.xy + params.viewport.zw) - vec2<i32>(1);
    var sum = vec3<f32>(0.0);
    for (var y = 0; y < i32(REDUCTION); y++) {
        for (var x = 0; x < i32(REDUCTION); x++) {
            sum += bright_pixel(min(first + vec2<i32>(x, y), last));
        }
    }
    return vec4<f32>(sum / (REDUCTION * REDUCTION), 1.0);
}

@fragment
fn fs_down(in: FullOut) -> @location(0) vec4<f32> {
    // `source` is the previous mip (one level, as its own view).
    let src_size = vec2<f32>(textureDimensions(source, 0));
    let dst_size = max(floor(src_size * 0.5), vec2<f32>(1.0));
    let uv = in.pos.xy / dst_size;
    return vec4<f32>(box4(source, uv, 1.0 / src_size), 1.0);
}

/// 1 inside the frame, fading to 0 over the outer 5 % (a light leaving the frame takes its
/// ghost with it smoothly), 0 outside.
fn frame_window(uv: vec2<f32>) -> f32 {
    let edge = min(uv, vec2<f32>(1.0) - uv);
    let w = smoothstep(vec2<f32>(0.0), vec2<f32>(0.05), edge);
    return select(0.0, w.x * w.y, all(edge >= vec2<f32>(0.0)));
}

struct CellOut {
    @location(0) energy: vec4<f32>,
    @location(1) place: vec4<f32>,
}

/// One light cell: the cell's share of the frame's bright energy (the sum over the cells is the
/// frame's mean bright radiance), its luminance-weighted centroid in viewport uv and the RMS
/// distance of its light from that centroid in width units. The frame window fades a light
/// leaving the frame, and its ghosts with it.
@fragment
fn fs_cells(in: FullOut) -> CellOut {
    let grid = vec2<f32>(f32(CELLS_X), f32(CELLS_Y));
    let aspect = params.info.z;
    let cell = floor(in.pos.xy);
    let lod = clamp(
        log2(f32(textureDimensions(bright, 0).x) / (grid.x * f32(CELL_TAPS))),
        0.0,
        params.info.y,
    );
    var energy = vec3<f32>(0.0);
    var weight = 0.0;
    var mean = vec2<f32>(0.0);
    var square = 0.0;
    for (var j = 0u; j < CELL_TAPS; j++) {
        for (var i = 0u; i < CELL_TAPS; i++) {
            let uv = (cell + (vec2<f32>(f32(i), f32(j)) + 0.5) / f32(CELL_TAPS)) / grid;
            let c = textureSampleLevel(bright, linear_clamp, uv, lod).rgb * frame_window(uv);
            let l = luminance(c);
            let w = vec2<f32>(uv.x, uv.y / aspect);
            energy += c;
            weight += l;
            mean += l * uv;
            square += l * dot(w, w);
        }
    }
    let taps = f32(CELL_TAPS * CELL_TAPS);
    let cell_energy = vec4<f32>(energy / (taps * grid.x * grid.y), 1.0);
    if weight <= 0.0 {
        return CellOut(cell_energy, vec4<f32>(0.5, 0.5, 0.0, 0.0));
    }
    let centroid = mean / weight;
    let w = vec2<f32>(centroid.x, centroid.y / aspect);
    let spread = sqrt(max(square / weight - dot(w, w), 0.0));
    return CellOut(cell_energy, vec4<f32>(centroid, spread, 0.0));
}

struct SpriteOut {
    @builtin(position) pos: vec4<f32>,
    /// Position in the ghost's own coordinates: the green image of the aperture is centred on 0
    /// with circumradius 1.
    @location(0) local: vec2<f32>,
    /// Where the red and blue images are centred, in the same coordinates.
    @location(1) red_at: vec2<f32>,
    @location(2) blue_at: vec2<f32>,
    /// The ghost's radiance.
    @location(3) colour: vec3<f32>,
    /// Edge softness, a fraction of the radius.
    @location(4) edge: f32,
}

/// One aperture sprite: instance `ghost * cells + cell`, six vertices. A dark cell's sprite
/// collapses outside the target.
@vertex
fn vs_sprite(@builtin(vertex_index) vertex: u32, @builtin(instance_index) instance: u32) -> SpriteOut {
    let cells = CELLS_X * CELLS_Y;
    let cell = instance % cells;
    let at = vec2<i32>(i32(cell % CELLS_X), i32(cell / CELLS_X));
    let energy = textureLoad(cell_energy, at, 0).rgb;
    if max(energy.x, max(energy.y, energy.z)) <= 0.0 {
        let nowhere = vec2<f32>(0.0);
        return SpriteOut(vec4<f32>(2.0, 2.0, 0.0, 1.0), nowhere, nowhere, nowhere, vec3<f32>(0.0), 1.0);
    }
    let ghost = instance / cells;
    let scale = params.ghost_scale[ghost];
    let aspect = params.info.z;
    let place = textureLoad(cell_place, at, 0);
    let light = place.xy * 2.0 - 1.0;
    // The radius in width units: the aperture's, a broad source's spread (magnified with the
    // ghost), and no less than a texel and a half of the target the sprite draws into.
    let spread = SPREAD_SCALE * place.z * abs(scale.y);
    let texel = REDUCTION / max(params.viewport.z, 1.0);
    let radius = max(sqrt(scale.w * scale.w + spread * spread), 1.5 * texel);
    // Centred frame units per unit of the ghost's own coordinates.
    let unit = vec2<f32>(2.0 * radius, 2.0 * radius * aspect);
    let green = scale.y * light;
    let red_at = (scale.x - scale.y) * light / unit;
    let blue_at = (scale.z - scale.y) * light / unit;
    // Two triangles over the three channels' images.
    let corner = vec2<f32>(f32(((vertex + 2u) / 3u) % 2u), f32(((vertex + 1u) / 3u) % 2u)) * 2.0 - 1.0;
    let local = corner * (1.05 + max(length(red_at), length(blue_at)));
    let p = green + local * unit;
    let ghost_energy = params.ghost_energy[ghost].xyz;
    let colour = energy * ghost_energy * params.strengths.y / (IRIS_AREA * radius * radius * aspect);
    let edge = clamp(max(IRIS_EDGE, spread / radius), 2.0 * texel / radius, 1.0);
    return SpriteOut(vec4<f32>(p.x, -p.y, 0.0, 1.0), local, red_at, blue_at, colour, edge);
}

/// The aperture at `q` (circumradius 1): the blades' polygon, softly edged, a little brighter at
/// the rim as a real ghost is.
fn aperture(q: vec2<f32>, edge: f32) -> f32 {
    let len = length(q);
    if len < 1.0e-4 {
        return 0.85;
    }
    let sector = 6.2831853 / IRIS_BLADES;
    let angle = atan2(q.y, q.x) + IRIS_ROTATION;
    let off_edge = angle - sector * floor(angle / sector) - 0.5 * sector;
    let polygon = len * cos(off_edge) / cos(0.5 * sector);
    let s = mix(len, polygon, IRIS_POLYGON);
    let rim = 0.85 + 0.35 * s * s * s * s;
    return rim * (1.0 - smoothstep(1.0 - edge, 1.0, s));
}

@fragment
fn fs_sprite(in: SpriteOut) -> @location(0) vec4<f32> {
    let mask = vec3<f32>(
        aperture(in.local - in.red_at, in.edge),
        aperture(in.local, in.edge),
        aperture(in.local - in.blue_at, in.edge),
    );
    return vec4<f32>(in.colour * mask, 0.0);
}

/// A coarse mip of the bright image at `uv` through a cubic B-spline, in four bilinear taps
/// (Sigg and Hadwiger, GPU Gems 2, ch. 20): its kernel is round, where a plain bilinear read of a
/// coarse mip draws the texel's square.
fn glare_at(uv: vec2<f32>, mip: u32) -> vec3<f32> {
    let size = vec2<f32>(textureDimensions(bright, mip));
    let at = uv * size - 0.5;
    let base = floor(at);
    let f = at - base;
    let f2 = f * f;
    let f3 = f2 * f;
    let w0 = (1.0 - 3.0 * f + 3.0 * f2 - f3) / 6.0;
    let w1 = (4.0 - 6.0 * f2 + 3.0 * f3) / 6.0;
    let w3 = f3 / 6.0;
    let w2 = 1.0 - w0 - w1 - w3;
    let g0 = w0 + w1;
    let g1 = w2 + w3;
    let p0 = (base + w1 / g0 - 0.5) / size;
    let p1 = (base + w3 / g1 + 1.5) / size;
    let lod = f32(mip);
    let a = textureSampleLevel(bright, linear_clamp, vec2<f32>(p0.x, p0.y), lod).rgb;
    let b = textureSampleLevel(bright, linear_clamp, vec2<f32>(p1.x, p0.y), lod).rgb;
    let c = textureSampleLevel(bright, linear_clamp, vec2<f32>(p0.x, p1.y), lod).rgb;
    let d = textureSampleLevel(bright, linear_clamp, vec2<f32>(p1.x, p1.y), lod).rgb;
    return g0.y * (g0.x * a + g1.x * b) + g1.y * (g0.x * c + g1.x * d);
}

/// Spikes per iris: one per blade (the diffraction pattern of a polygon aperture puts a spike
/// perpendicular to each edge; with an even blade count they pair up across the centre, so a
/// six-bladed iris shows six).
const STARBURST_SPIKES: u32 = 6u;
/// Gather taps along each spike, and the spike's length as a fraction of the view width.
const STARBURST_TAPS: u32 = 10u;
const STARBURST_LENGTH: f32 = 0.16;
/// Overall scale of the spikes against the source's own light (tuned so strength 1 reads as a
/// faint star on a bright lamp rather than a second light).
const STARBURST_SCALE: f32 = 0.5;

/// The diffraction starburst at the viewport uv: each bright pixel of the (blurred) bright image
/// is smeared along the six blade directions with a falling weight, so a pixel at distance `s`
/// along a spike from a source gathers that source. A ray gather (the sources are not listed),
/// `STARBURST_SPIKES * STARBURST_TAPS` bilinear reads of a coarse mip.
fn starburst_at(uv: vec2<f32>) -> vec3<f32> {
    let aspect = params.info.z;
    let mip = u32(params.extra.y);
    var sum = vec3<f32>(0.0);
    for (var k = 0u; k < STARBURST_SPIKES; k = k + 1u) {
        let angle = IRIS_ROTATION + f32(k) * (3.14159265 / 3.0);
        let dir = vec2<f32>(cos(angle), sin(angle) * aspect);
        for (var t = 0u; t < STARBURST_TAPS; t = t + 1u) {
            let f = f32(t) / f32(STARBURST_TAPS);
            let weight = (1.0 - f) * (1.0 - f);
            let at = uv - dir * (STARBURST_LENGTH * f);
            sum = sum + weight * textureSampleLevel(bright, linear_clamp, at, f32(mip)).rgb;
        }
    }
    return params.extra.x * STARBURST_SCALE * sum / f32(STARBURST_TAPS);
}

/// Flare plus starburst plus dirt at the viewport uv.
fn lens_light(uv: vec2<f32>) -> vec3<f32> {
    var ghosts = textureSampleLevel(flare, linear_clamp, uv, 0.0).rgb;
    if params.extra.x > 0.0 {
        ghosts = ghosts + starburst_at(uv);
    }
    if params.strengths.z <= 0.0 {
        return min(ghosts, vec3<f32>(MAX_LENS_LIGHT));
    }
    let glare = glare_at(uv, u32(params.info.w));
    let mask = textureSampleLevel(dirt, linear_clamp, uv, 0.0).r;
    return min(ghosts + params.strengths.z * DIRT_SCATTER * mask * (ghosts + glare), vec3<f32>(MAX_LENS_LIGHT));
}

@fragment
fn fs_add(in: FullOut) -> @location(0) vec4<f32> {
    let uv = (in.pos.xy - params.viewport.xy) / params.viewport.zw;
    return vec4<f32>(lens_light(uv), 0.0);
}

@fragment
fn fs_replace(in: FullOut) -> @location(0) vec4<f32> {
    let uv = (in.pos.xy - params.viewport.xy) / params.viewport.zw;
    // Green stays on its texel (bit-exact); red is magnified and blue minified about the
    // centre by the aberration fraction.
    let here = textureLoad(source, vec2<i32>(in.pos.xy), 0);
    let d = uv - 0.5;
    let ca = params.strengths.w;
    let red = textureSampleLevel(source, linear_clamp, source_uv(0.5 + d * (1.0 + ca)), 0.0).r;
    let blue = textureSampleLevel(source, linear_clamp, source_uv(0.5 + d * (1.0 - ca)), 0.0).b;
    return vec4<f32>(vec3<f32>(red, here.g, blue) + lens_light(uv), here.a);
}
