// SPDX-License-Identifier: MPL-2.0
// TSR prepare pass (render resolution).
//
// For every input texel: dilated, jitter-free motion, a disocclusion factor and
// a thin-geometry flag, plus this frame's linear depth for the next frame.
//
// * Depth dilation: motion is taken from the nearest surface in the 3×3
//   neighbourhood, so a thin or silhouette edge carries its own motion instead
//   of the background's (Karis, "High Quality Temporal Supersampling",
//   SIGGRAPH 2014).
// * Camera motion is reprojected from depth for every texel (sky included);
//   object motion comes from the velocity target and is moved onto the
//   jitter-free convention by the camera-only difference of the two previous
//   matrices (the velocity pass may have used a jittered one).
// * Disocclusion: the expected previous view depth of the surface is compared
//   with the previous frame's depth at the reprojected position (2×2
//   footprint). A surface that was hidden last frame finds a nearer depth there
//   (Yang, Liu & Salvi, JCGT 2020, § 5.1 "geometry-based rejection").
// * Dynamic: how far the surface moved relative to the camera-only reprojection.
//   The resolve trusts history (loose min/max box) where this is 0 and narrows
//   to the variance box where an object moves.
// * Missed thin surfaces (ADR-2638): a 1 px member can fall between every sample of a
//   frame, and then no spatial dilation can see it. Under camera translation the
//   previous frame's depth still can: a point at inverse depth t along this texel's ray
//   reprojects along a straight (epipolar) line in the previous frame, so walking that
//   line one input texel at a time finds a surface the ray passes through in front of
//   what it hit now. For a static scene that can only be a surface the samples missed
//   (screen-space ray marching against a depth buffer: McGuire & Mara, "Efficient GPU
//   Screen-Space Ray Tracing", JCGT 2014; temporal reuse of depth: Yang, Liu & Salvi,
//   JCGT 2020, § 5). The walk accepts only a surface one texel thin there (a large
//   surface that moved away is a disocclusion) that was not moving. A hit carries the
//   thin surface's motion, marks the texel (`motion.w` = THIN_MISSED) and is remembered
//   in this frame's depth (`depth.x` nearer, `depth.y` = −age) for THIN_MEMORY_FRAMES
//   frames, so a member missed twice in a row is still found.

struct TsrFrame {
    // x, y = render size, z, w = 1 / render size.
    render_size: vec4<f32>,
    // x, y = output size, z, w = 1 / output size.
    output_size: vec4<f32>,
    // xy = sample offset of every input texel in input pixels (jitter), zw unused.
    jitter: vec4<f32>,
    // x = exposure, y = sharpness, z = 1 when history is valid, w = max accumulated frames.
    tune: vec4<f32>,
    // x = static variance gamma, y = moving variance gamma, z = ridge threshold, w = depth tolerance.
    tune2: vec4<f32>,
    // x = 1 when a reactive mask is bound, y = lock decay, zw unused.
    tune3: vec4<f32>,
    // NDC of the jittered raster → eye-relative world.
    inv_jittered: mat4x4<f32>,
    // Eye-relative world → this frame's jitter-free clip.
    cur_vp: mat4x4<f32>,
    // Eye-relative world → previous frame's jitter-free clip.
    prev_vp: mat4x4<f32>,
    // Eye-relative world → the previous clip the velocity pass used.
    velocity_prev_vp: mat4x4<f32>,
    // xy = last frame's sample offset in input pixels (where the previous depth was sampled).
    prev_jitter: vec4<f32>,
}

@group(0) @binding(0) var depth_tex: texture_depth_2d;
@group(0) @binding(1) var velocity_tex: texture_2d<f32>;
@group(0) @binding(2) var prev_depth_tex: texture_2d<f32>;
@group(0) @binding(3) var<uniform> frame: TsrFrame;

struct VertexOut {
    @builtin(position) position: vec4<f32>,
}

@vertex
fn vs_main(@builtin(vertex_index) id: u32) -> VertexOut {
    let uv = vec2<f32>(f32((id << 1u) & 2u), f32(id & 2u));
    return VertexOut(vec4<f32>(uv * 2.0 - vec2<f32>(1.0), 0.0, 1.0));
}

struct PrepareOut {
    // xy = motion (previous uv − current uv), z = disocclusion, w = thin geometry.
    @location(0) motion: vec4<f32>,
    // x = linear view depth of this texel (the next frame's previous depth), or a
    // remembered missed thin surface's; y = dynamic: object motion relative to the camera,
    // 0–1 (1 at ≥ 1 output px), or −age of the remembered surface; zw = this texel's own
    // motion, undilated (the resolve labels a restarting pixel's content with it).
    @location(1) depth: vec4<f32>,
}

// Value of `motion.w` on a texel whose ray passes through a thin surface it missed.
const THIN_MISSED: f32 = 0.25;
// Frames a missed thin surface stays remembered. At 0.5 scale a 1 px member is missed on
// half the frames, so three in a row is one frame in eight; a thin surface that was really
// removed lingers at most this long.
const THIN_MEMORY_FRAMES: f32 = 3.0;
// Input texels of relative parallax the walk covers.
const THIN_WALK_TEXELS: i32 = 8;

fn uv_of(clip: vec4<f32>) -> vec2<f32> {
    let ndc = clip.xy / max(abs(clip.w), 1.0e-6);
    return vec2<f32>(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5);
}

fn load_depth(p: vec2<i32>) -> f32 {
    let size = vec2<i32>(textureDimensions(depth_tex));
    return textureLoad(depth_tex, clamp(p, vec2<i32>(0), size - vec2<i32>(1)), 0);
}

fn world_at(p: vec2<i32>, depth: f32) -> vec3<f32> {
    let uv = (vec2<f32>(p) + vec2<f32>(0.5)) * frame.render_size.zw;
    let ndc = vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, depth, 1.0);
    let world = frame.inv_jittered * ndc;
    return world.xyz / world.w;
}

fn view_depth(p: vec2<i32>) -> f32 {
    let world = world_at(p, load_depth(p));
    return (frame.cur_vp * vec4<f32>(world, 1.0)).w;
}

fn prev_depth(q: vec2<i32>) -> f32 {
    let size = vec2<i32>(textureDimensions(prev_depth_tex));
    return textureLoad(prev_depth_tex, clamp(q, vec2<i32>(0), size - vec2<i32>(1)), 0).r;
}

// Walks this texel's ray through the previous frame's depth (see the header). `world` is the
// point the ray hit now, eye-relative. Returns the inverse-depth factor t of a missed thin
// surface (the ray's point is `world / t`) in x and its remembered age in y, or x = 0.
fn find_missed_thin(world: vec3<f32>) -> vec2<f32> {
    // clip(t) = (A + t B) / t, with t = 1 at what the ray hit now.
    let a = frame.prev_vp * vec4<f32>(world, 0.0);
    let b = frame.prev_vp * vec4<f32>(0.0, 0.0, 0.0, 1.0);
    let at_1 = uv_of(a + b);
    let at_2 = uv_of(a + 2.0 * b);
    let texels_per_t = length((at_2 - at_1) * frame.render_size.xy);
    // No parallax (a still or rotating camera): every depth reprojects alike.
    if (texels_per_t < 1.0e-3 || a.w + b.w <= 0.0) {
        return vec2<f32>(0.0);
    }
    let dt = 1.0 / texels_per_t;
    let own = a.w + b.w;
    let psize = vec2<i32>(textureDimensions(prev_depth_tex));
    for (var k: i32 = 1; k <= THIN_WALK_TEXELS; k = k + 1) {
        let t = 1.0 + f32(k) * dt;
        let uv = uv_of(a + t * b);
        let q = vec2<i32>(floor(uv * frame.render_size.xy - frame.prev_jitter.xy));
        if (any(q < vec2<i32>(0)) || any(q >= psize)) {
            continue;
        }
        let s = textureLoad(prev_depth_tex, q, 0);
        // The surface there, on this ray: previous view depth z = (A.w + t' B.w) / t'.
        let t_surface = a.w / max(s.r - b.w, 1.0e-6);
        if (s.r >= 0.9 * own || abs(t_surface - t) > 0.5 * dt + 0.01 * t) {
            continue;
        }
        let age = max(-s.g, 0.0) + 1.0;
        let thin = s.r < 0.9 * min(prev_depth(q + vec2<i32>(-1, 0)), prev_depth(q + vec2<i32>(1, 0)))
            || s.r < 0.9 * min(prev_depth(q + vec2<i32>(0, -1)), prev_depth(q + vec2<i32>(0, 1)));
        if (s.g < 0.25 && thin && age <= THIN_MEMORY_FRAMES) {
            return vec2<f32>(t_surface, age);
        }
        return vec2<f32>(0.0);
    }
    return vec2<f32>(0.0);
}

// The motion of the surface at texel `p` itself, `depth` its raster depth.
fn own_motion(p: vec2<i32>, depth: f32) -> vec2<f32> {
    let world = world_at(p, depth);
    let cur_clip = frame.cur_vp * vec4<f32>(world, 1.0);
    let camera = uv_of(frame.prev_vp * vec4<f32>(world, 1.0));
    let vsize = vec2<i32>(textureDimensions(velocity_tex));
    let velocity = textureLoad(velocity_tex, clamp(p, vec2<i32>(0), vsize - vec2<i32>(1)), 0);
    if (velocity.a > 0.5 && velocity.z > 0.0 && abs(velocity.z - cur_clip.w) <= 0.01 * cur_clip.w + 1.0e-3) {
        return velocity.xy + (camera - uv_of(frame.velocity_prev_vp * vec4<f32>(world, 1.0)));
    }
    return camera - uv_of(cur_clip);
}

@fragment
fn fs_main(in: VertexOut) -> PrepareOut {
    let px = vec2<i32>(in.position.xy);
    var nearest = px;
    var nearest_depth = 2.0;
    for (var y: i32 = -1; y <= 1; y = y + 1) {
        for (var x: i32 = -1; x <= 1; x = x + 1) {
            let q = px + vec2<i32>(x, y);
            let d = load_depth(q);
            if (d < nearest_depth) {
                nearest_depth = d;
                nearest = q;
            }
        }
    }
    let size = vec2<i32>(frame.render_size.xy);
    nearest = clamp(nearest, vec2<i32>(0), size - vec2<i32>(1));
    let world = world_at(nearest, nearest_depth);
    let cur_clip = frame.cur_vp * vec4<f32>(world, 1.0);
    let prev_clip = frame.prev_vp * vec4<f32>(world, 1.0);
    let cur_uv = uv_of(cur_clip);
    let prev_uv_camera = uv_of(prev_clip);
    var motion = prev_uv_camera - cur_uv;
    var expected = prev_clip.w;
    var dynamic = 0.0;

    let vsize = vec2<i32>(textureDimensions(velocity_tex));
    let velocity = textureLoad(velocity_tex, clamp(nearest, vec2<i32>(0), vsize - vec2<i32>(1)), 0);
    // The velocity pass has no depth test: accept its motion only when the
    // surface that wrote it is the visible one (its view depth in `z`). A
    // writer with no depth (`z` = 0, the terrain pass) is static: camera motion.
    let accepted = velocity.a > 0.5 && velocity.z > 0.0
        && abs(velocity.z - cur_clip.w) <= 0.01 * cur_clip.w + 1.0e-3;
    if (accepted) {
        // Object motion. The camera-only difference between the jitter-free
        // previous matrix and the one the velocity pass used is the same for a
        // static and a moving surface, so it converts the convention exactly.
        let used = uv_of(frame.velocity_prev_vp * vec4<f32>(world, 1.0));
        motion = velocity.xy + (prev_uv_camera - used);
        dynamic = clamp(length((motion - (prev_uv_camera - cur_uv)) * frame.output_size.xy), 0.0, 1.0);
        // A moving surface keeps (about) its own depth.
        expected = cur_clip.w;
    }

    // Disocclusion against the previous frame's linear depth.
    let prev_uv = cur_uv + motion;
    var disocclusion = 0.0;
    if (any(prev_uv < vec2<f32>(0.0)) || any(prev_uv > vec2<f32>(1.0))) {
        disocclusion = 1.0;
    } else if (frame.tune.z > 0.5) {
        let p = prev_uv * frame.render_size.xy - vec2<f32>(0.5);
        let base = vec2<i32>(floor(p));
        let psize = vec2<i32>(textureDimensions(prev_depth_tex));
        var best = 1.0e+9;
        for (var y: i32 = 0; y <= 1; y = y + 1) {
            for (var x: i32 = 0; x <= 1; x = x + 1) {
                let q = clamp(base + vec2<i32>(x, y), vec2<i32>(0), psize - vec2<i32>(1));
                let prev = textureLoad(prev_depth_tex, q, 0).r;
                // Signed: only a previous surface *nearer* than this one hid it.
                // A farther one is a thin feature the last jitter missed.
                best = min(best, (expected - prev) / max(expected, 1.0e-4));
            }
        }
        // Disoccluded only when all four previous depths are nearer than this
        // surface: something in front covered it last frame.
        disocclusion = smoothstep(frame.tune2.w, frame.tune2.w * 2.0, best);
    }

    // Thin geometry: a texel clearly nearer than both neighbours on one axis
    // (a 1-px bar, a wire, a lattice member).
    let wc = view_depth(px);
    let wl = view_depth(px + vec2<i32>(-1, 0));
    let wr = view_depth(px + vec2<i32>(1, 0));
    let wu = view_depth(px + vec2<i32>(0, -1));
    let wd = view_depth(px + vec2<i32>(0, 1));
    let thin_x = wc < 0.9 * min(wl, wr);
    let thin_y = wc < 0.9 * min(wu, wd);
    let thin = select(0.0, 1.0, thin_x || thin_y);

    // A thin surface this frame's samples missed: only where nothing nearer already gives
    // this texel its motion (the spatial dilation) and its own surface is static.
    if (thin < 0.5 && dynamic < 0.25 && frame.tune.z > 0.5 && cur_clip.w >= 0.9 * wc) {
        let world_p = world_at(px, load_depth(px));
        let missed = find_missed_thin(world_p);
        if (missed.x > 0.0) {
            let a = frame.prev_vp * vec4<f32>(world_p, 0.0);
            let b = frame.prev_vp * vec4<f32>(0.0, 0.0, 0.0, 1.0);
            let cur_a = frame.cur_vp * vec4<f32>(world_p, 0.0);
            let cur_b = frame.cur_vp * vec4<f32>(0.0, 0.0, 0.0, 1.0);
            let missed_motion = uv_of(a + missed.x * b) - uv_of(frame.cur_vp * vec4<f32>(world_p, 1.0));
            let remembered = (cur_a.w + missed.x * cur_b.w) / missed.x;
            return PrepareOut(
                vec4<f32>(missed_motion, 0.0, THIN_MISSED),
                vec4<f32>(remembered, -missed.y, missed_motion),
            );
        }
    }

    var own = motion;
    if (any(nearest != px)) {
        own = own_motion(px, load_depth(px));
    }
    return PrepareOut(vec4<f32>(motion, disocclusion, thin), vec4<f32>(wc, dynamic, own));
}
