// SPDX-License-Identifier: MPL-2.0
// TSR resolve (output resolution): accumulate jittered input samples into an
// output-resolution history.
//
// Sources (all public):
// * Jittered-sample reconstruction weighted by each sample's distance to the
//   output pixel, and the history blend scaled by how well this frame covers
//   the pixel: Yang, Liu & Salvi, "A Survey of Temporal Antialiasing
//   Techniques", JCGT 2020, § 4.3 (temporal upsampling).
// * History resampling with a 5-tap Catmull-Rom (bilinear taps with the
//   corner weights dropped): Jimenez, "Filmic SMAA", SIGGRAPH 2016 course.
// * Rectification: YCoCg (Malvar & Sullivan 2003) box clipping toward the box
//   centre (Karis 2014; Pedersen, "Temporal Reprojection Anti-Aliasing in INSIDE",
//   GDC 2016). The min/max box where reprojection is trusted; variance clipping
//   (Salvi, "An Excursion in Temporal Supersampling", GDC 2016) where an object
//   moves, with a gamma that narrows with motion.
// * Accumulation and filtering in a reversible tonemapped domain
//   c / (1 + luma) so one hot specular sample cannot dominate (Karis 2014):
//   the anti-flicker term on top of it damps a static pixel whose luma
//   oscillates frame to frame.
// * Thin-feature locks: a sub-pixel ridge in luma or in depth marks the output
//   pixel as locked; a locked pixel keeps its history through frames whose
//   jittered samples miss the feature, the way AMD FidelityFX Super
//   Resolution 2 describes its "locks" (GPUOpen FSR 2 documentation, 2022).
//   A missed feature is followed with the motion its locked history pixel
//   moved with last frame (it may differ from the background's, parallax).
// * Held pixels: the clip box's luma stretches toward the pixel's own
//   observed luma of the last frame, so detail the render resolution aliases
//   converges to its average instead of flickering between phases (variance of
//   the observed signal over time rather than over space: Salvi 2016 and the
//   temporal-moment boxes of Yang, Liu & Salvi, JCGT 2020, § 5.1).
//   The lock decays, and dies on disocclusion, a reactive pixel, or a
//   well-covered sample that contradicts it. It never bypasses the clip; it
//   stretches the box's luma range toward the locked luma.
// * Missed thin surfaces: where the prepare pass found (in the previous frame's depth) a
//   thin surface every sample of this frame missed, the pixel takes that surface's motion
//   and keeps its history unclipped with no weight for this frame's samples, which all
//   belong to what is behind it. The history pixel's stored motion (motion consistency)
//   decides whether this output pixel is the surface or merely beside it.
// * Reactive mask: per-pixel 0–1 from the host (VFX, decals, alpha) raises the
//   current-frame weight and drops locks (same GPUOpen documentation).
// * Young pixels (ADR-4934): a pixel that restarts with no sample of its own holds the nearest
//   sample's value as a placeholder of negligible weight, and until its accumulated weight
//   reaches YOUNG_WEIGHT it takes only a sample inside it, undamped by the anti-flicker term
//   (weighted accumulation, Yang, Liu & Salvi, JCGT 2020, § 4.3; the weight is the evidence).
// * Tracked locks (ADR-7694): a lock carries where in its pixel the thin feature lies, the feature's
//   motion moves that point each frame, and only the pixel the point lands in owns the lock and takes
//   the locked history pixel's colour unfiltered (point-sampled history for a feature narrower than a
//   pixel). A pixel that owns no thin feature leaves thin samples that lie in other pixels out of its
//   clip box, and a moving pixel whose own sample lands on its thin feature takes most of that sample.
//   At render scale 0.5 a 1 px member can go unsampled for 4–8 frames in a row (the jitter's x
//   alternates render-texel halves), and whole-pixel rounding ran it a pixel ahead of itself.

struct TsrFrame {
    render_size: vec4<f32>,
    output_size: vec4<f32>,
    jitter: vec4<f32>,
    tune: vec4<f32>,
    tune2: vec4<f32>,
    tune3: vec4<f32>,
    inv_jittered: mat4x4<f32>,
    cur_vp: mat4x4<f32>,
    prev_vp: mat4x4<f32>,
    velocity_prev_vp: mat4x4<f32>,
    prev_jitter: vec4<f32>,
}

@group(0) @binding(0) var current_tex: texture_2d<f32>;
@group(0) @binding(1) var motion_tex: texture_2d<f32>;
@group(0) @binding(2) var history_tex: texture_2d<f32>;
@group(0) @binding(3) var aux_tex: texture_2d<f32>;
@group(0) @binding(4) var reactive_tex: texture_2d<f32>;
@group(0) @binding(5) var samp: sampler;
@group(0) @binding(6) var<uniform> frame: TsrFrame;
// x = linear depth, y = dynamic (object motion relative to the camera, 0–1), zw = the
// texel's own motion, undilated (uv per frame).
@group(0) @binding(7) var depth_tex: texture_2d<f32>;
// xy = the motion (output px) each history pixel moved with last frame; zw = where in the pixel
// (output px from its centre) its locked thin feature lies (ADR-7694).
@group(0) @binding(8) var aux_motion_tex: texture_2d<f32>;

// Accumulated sample weight below which a pixel is young: it restarted with no sample of its
// own, and takes only a sample inside it until it has one (ADR-4934).
const YOUNG_WEIGHT: f32 = 0.3;
// Weight of a restart's placeholder value when no sample of its layer lies inside the pixel.
const PLACEHOLDER_WEIGHT: f32 = 0.01;

struct VertexOut {
    @builtin(position) position: vec4<f32>,
}

@vertex
fn vs_main(@builtin(vertex_index) id: u32) -> VertexOut {
    let uv = vec2<f32>(f32((id << 1u) & 2u), f32(id & 2u));
    return VertexOut(vec4<f32>(uv * 2.0 - vec2<f32>(1.0), 0.0, 1.0));
}

struct ResolveOut {
    // Linear HDR history (rgb); a = the tonemapped luma a sample inside the pixel last saw (ADR-4859).
    @location(0) color: vec4<f32>,
    // x = lock strength, y = lock luma (tonemapped), z = previous luma, w = accumulated sample weight.
    @location(1) aux: vec4<f32>,
    // xy = motion this pixel's content moved with (output px), for lock following; zw = the
    // locked feature's offset from the pixel centre (output px, ADR-7694).
    @location(2) motion: vec4<f32>,
}

fn luma(c: vec3<f32>) -> f32 {
    return dot(c, vec3<f32>(0.25, 0.5, 0.25));
}

fn ycocg(rgb: vec3<f32>) -> vec3<f32> {
    let y = dot(rgb, vec3<f32>(0.25, 0.5, 0.25));
    return vec3<f32>(y, (rgb.r - rgb.b) * 0.5, rgb.g - y);
}

fn rgb_from_ycocg(c: vec3<f32>) -> vec3<f32> {
    let tmp = c.x - c.z;
    return vec3<f32>(tmp + c.y, c.x + c.z, tmp - c.y);
}

// Linear HDR → tonemapped [0, 1) domain (exposure applied).
fn to_t(c: vec3<f32>) -> vec3<f32> {
    let e = max(finite_or_zero(c), vec3<f32>(0.0)) * frame.tune.x;
    return e / (1.0 + luma(e));
}

// Zero for a NaN or infinite channel, by the exponent bits: `max(x, 0)` (SPIR-V FMax) is
// undefined for NaN and RADV/ACO keeps it, so a NaN reached the history (v0.1.4).
fn finite_or_zero(c: vec3<f32>) -> vec3<f32> {
    let bad = (bitcast<vec3<u32>>(c) << vec3<u32>(1u)) >= vec3<u32>(0xff000000u);
    return select(c, vec3<f32>(0.0), bad);
}

fn from_t(t: vec3<f32>) -> vec3<f32> {
    let l = min(luma(max(t, vec3<f32>(0.0))), 0.999);
    return max(t, vec3<f32>(0.0)) / (1.0 - l) / max(frame.tune.x, 1.0e-12);
}

// Clips `h` toward the box centre so it lands on the box (Pedersen 2016).
//
// The box gets a small minimum half-extent on every axis. A neutral (grey)
// neighbourhood has ~zero chroma extent; without the floor, half-float chroma
// noise in the history scales `d` by a huge factor and drags its *luma* to the
// box centre, half-way between the dark and bright samples (edges went 50 %).
fn clip_to_box(h: vec3<f32>, lo: vec3<f32>, hi: vec3<f32>) -> vec3<f32> {
    let centre = 0.5 * (lo + hi);
    let floor_extent = 0.002 + 0.01 * max(abs(hi.x), abs(lo.x));
    let extent = max(0.5 * (hi - lo), vec3<f32>(floor_extent));
    let d = h - centre;
    let units = abs(d / extent);
    let m = max(units.x, max(units.y, units.z));
    if (m > 1.0) {
        return centre + d / m;
    }
    return h;
}

// A tonemapped luma stored last frame, at this frame's pre-exposure (`jitter.z` = new / old).
// Tonemapped, not linear: an exposed HDR luma overflows the half-float history's alpha.
fn rescale_t(t: f32, history_scale: f32) -> f32 {
    let bad = (bitcast<u32>(t) << 1u) >= 0xff000000u;
    let l = clamp(select(t, 0.0, bad), 0.0, 0.999);
    let x = l / (1.0 - l) * history_scale;
    return x / (1.0 + x);
}

fn history_catmull_rom(uv: vec2<f32>) -> vec3<f32> {
    let size = frame.output_size.xy;
    let p = uv * size;
    let tc1 = floor(p - vec2<f32>(0.5)) + vec2<f32>(0.5);
    let f = p - tc1;
    let w0 = f * (-0.5 + f * (1.0 - 0.5 * f));
    let w1 = 1.0 + f * f * (-2.5 + 1.5 * f);
    let w2 = f * (0.5 + f * (2.0 - 1.5 * f));
    let w3 = f * f * (-0.5 + 0.5 * f);
    let w12 = w1 + w2;
    let off12 = w2 / w12;
    let inv = frame.output_size.zw;
    let t0 = (tc1 - vec2<f32>(1.0)) * inv;
    let t3 = (tc1 + vec2<f32>(2.0)) * inv;
    let t12 = (tc1 + off12) * inv;
    var sum = vec3<f32>(0.0);
    var wsum = 0.0;
    var w = w12.x * w0.y;
    sum += textureSampleLevel(history_tex, samp, vec2<f32>(t12.x, t0.y), 0.0).rgb * w;
    wsum += w;
    w = w0.x * w12.y;
    sum += textureSampleLevel(history_tex, samp, vec2<f32>(t0.x, t12.y), 0.0).rgb * w;
    wsum += w;
    w = w12.x * w12.y;
    sum += textureSampleLevel(history_tex, samp, t12, 0.0).rgb * w;
    wsum += w;
    w = w3.x * w12.y;
    sum += textureSampleLevel(history_tex, samp, vec2<f32>(t3.x, t12.y), 0.0).rgb * w;
    wsum += w;
    w = w12.x * w3.y;
    sum += textureSampleLevel(history_tex, samp, vec2<f32>(t12.x, t3.y), 0.0).rgb * w;
    wsum += w;
    return max(finite_or_zero(sum / max(wsum, 1.0e-5)), vec3<f32>(0.0));
}

// Thin luma ridge through the centre along one direction: both neighbours on the same side,
// by more than the threshold relative to the local brightness.
fn luma_ridge(lc: f32, a: f32, b: f32, threshold: f32) -> bool {
    let da = lc - a;
    let db = lc - b;
    let contrast = min(abs(da), abs(db));
    return da * db > 0.0 && contrast > threshold * max(max(lc, max(a, b)), 0.02);
}

@fragment
fn fs_main(in: VertexOut) -> ResolveOut {
    let out_px = in.position.xy;
    let uv = out_px * frame.output_size.zw;
    let rsize = vec2<i32>(frame.render_size.xy);
    // Continuous input position of this output pixel's centre, and the input
    // texel whose (jittered) sample lies nearest to it.
    let p_in = uv * frame.render_size.xy;
    let jitter = frame.jitter.xy;
    let centre = vec2<i32>(floor(p_in - jitter));
    let ratio = frame.output_size.xy * frame.render_size.zw;

    var smooth_sum = vec3<f32>(0.0);
    var smooth_w = 0.0;
    var sharp_sum = vec3<f32>(0.0);
    var sharp_w = 0.0;
    var coverage = 0.0;
    var m1 = vec3<f32>(0.0);
    var m2 = vec3<f32>(0.0);
    var lo = vec3<f32>(1.0e+6);
    var hi = vec3<f32>(-1.0e+6);
    // The box without the thin samples that lie outside this pixel (ADR-7694): used by a pixel that
    // owns no thin feature, so a member's sample does not keep a stale copy of it beside the member.
    var lo_own = vec3<f32>(1.0e+6);
    var hi_own = vec3<f32>(-1.0e+6);
    // 3×3 lumas by row (vectors, not a function-local array: ADR-026).
    var luma_row0 = vec3<f32>(0.0);
    var luma_row1 = vec3<f32>(0.0);
    var luma_row2 = vec3<f32>(0.0);
    var nearest_t = vec3<f32>(0.0);
    // Motion of the nearest texel that missed a thin surface (prepare's `THIN_MISSED`).
    var missed_motion = vec2<f32>(0.0);
    var missed_d2 = 1.0e+6;
    for (var y: i32 = -1; y <= 1; y = y + 1) {
        for (var x: i32 = -1; x <= 1; x = x + 1) {
            let q = clamp(centre + vec2<i32>(x, y), vec2<i32>(0), rsize - vec2<i32>(1));
            let t = to_t(textureLoad(current_tex, q, 0).rgb);
            let d_in = vec2<f32>(centre + vec2<i32>(x, y)) + vec2<f32>(0.5) + jitter - p_in;
            let d_out = d_in * ratio;
            let ws = exp(-2.0 * dot(d_in, d_in));
            let wp = exp(-frame.tune3.w * dot(d_out, d_out));
            smooth_sum += t * ws;
            smooth_w += ws;
            sharp_sum += t * wp;
            sharp_w += wp;
            coverage = max(coverage, wp);
            let c = ycocg(t);
            m1 += c;
            m2 += c * c;
            lo = min(lo, c);
            hi = max(hi, c);
            if (y < 0) {
                luma_row0[x + 1] = c.x;
            } else if (y == 0) {
                luma_row1[x + 1] = c.x;
            } else {
                luma_row2[x + 1] = c.x;
            }
            if (x == 0 && y == 0) {
                nearest_t = t;
            }
            let qm = textureLoad(motion_tex, q, 0);
            if (!(qm.w > 0.5 && any(abs(d_out) > vec2<f32>(0.5)))) {
                lo_own = min(lo_own, c);
                hi_own = max(hi_own, c);
            }
            if (qm.w > 0.0 && qm.w < 0.5 && dot(d_in, d_in) < missed_d2) {
                missed_d2 = dot(d_in, d_in);
                missed_motion = qm.xy;
            }
        }
    }
    let smooth_t = smooth_sum / max(smooth_w, 1.0e-6);
    var current_t = smooth_t;
    if (sharp_w > 1.0e-4) {
        current_t = sharp_sum / sharp_w;
    }
    let mean = m1 / 9.0;
    let sigma = sqrt(max(m2 / 9.0 - mean * mean, vec3<f32>(0.0)));

    let mq = clamp(centre, vec2<i32>(0), rsize - vec2<i32>(1));
    let motion_s = textureLoad(motion_tex, mq, 0);
    let depth_thin = motion_s.w;
    // Negative: the age of a remembered thin surface, not motion (prepare).
    let dynamic = max(textureLoad(depth_tex, mq, 0).y, 0.0);
    // A static pixel beside a missed thin surface may be that surface: the history decides. It
    // is the surface only if what reprojects here with the surface's motion was locked onto a
    // thin feature and moved with that motion last frame; a wall pixel beside the member keeps
    // its own motion, or it would keep the member's blur and grow with every frame.
    //
    // Where the surface is now is found to a fraction of a pixel (ADR-7694): every locked history
    // pixel carries where in it the feature lies, so the surface's motion moves that point, and
    // the pixel the point lands in is the surface. Taking the history pixel the reprojection
    // lands in instead rounds the feature to a whole pixel each frame a sample does not see it;
    // under a motion of a fraction of a pixel per frame (an orbit) the rounding ran the member
    // up to a pixel ahead and left its true pixel to restart as wall.
    var missed = false;
    var claim_px = vec2<i32>(0);
    var claim_off = vec2<f32>(0.0);
    if (missed_d2 < 1.0e+6 && depth_thin <= 0.5 && dynamic < 0.25 && frame.tune.z > 0.5) {
        let size = vec2<i32>(frame.output_size.xy);
        let m_px = missed_motion * frame.output_size.xy;
        let base_c = vec2<i32>(floor(out_px + m_px));
        var best_lock = 0.1;
        for (var cy: i32 = -1; cy <= 1; cy = cy + 1) {
            for (var cx: i32 = -1; cx <= 1; cx = cx + 1) {
                let cq = base_c + vec2<i32>(cx, cy);
                if (any(cq < vec2<i32>(0)) || any(cq >= size)) {
                    continue;
                }
                let l = textureLoad(aux_tex, cq, 0).x;
                let m_c = textureLoad(aux_motion_tex, cq, 0);
                let here = vec2<f32>(cq) + vec2<f32>(0.5) + m_c.zw - m_px - out_px;
                if (l > best_lock && length(m_c.xy - m_px) < 0.3 + 0.05 * length(m_px)
                    && all(abs(here) <= vec2<f32>(0.5))) {
                    best_lock = l;
                    missed = true;
                    claim_px = cq;
                    claim_off = here;
                }
            }
        }
    }
    let motion = select(motion_s.xy, missed_motion, missed);
    let disocclusion = select(motion_s.z, 0.0, missed);
    let motion_px = length(motion * frame.output_size.xy);

    var reactive = 0.0;
    if (frame.tune3.x > 0.5) {
        let rs = vec2<i32>(textureDimensions(reactive_tex));
        reactive = clamp(textureLoad(reactive_tex, clamp(centre, vec2<i32>(0), rs - vec2<i32>(1)), 0).r, 0.0, 1.0);
    }

    // Thin luma ridge at the nearest sample (horizontal, vertical, diagonals).
    let lc = luma_row1.y;
    let threshold = frame.tune2.z;
    let ridge = luma_ridge(lc, luma_row0.x, luma_row2.z, threshold)
        || luma_ridge(lc, luma_row0.y, luma_row2.y, threshold)
        || luma_ridge(lc, luma_row0.z, luma_row2.x, threshold)
        || luma_ridge(lc, luma_row1.x, luma_row1.z, threshold);
    // The feature can only be locked onto this output pixel if the sample that
    // saw it lies on the pixel.
    let nearest_d = (vec2<f32>(centre) + vec2<f32>(0.5) + jitter - p_in) * ratio;
    let on_pixel = all(abs(nearest_d) <= vec2<f32>(0.5));

    var history_uv = uv + motion;
    var used_motion = motion * frame.output_size.xy;
    let in_bounds = all(history_uv >= vec2<f32>(0.0)) && all(history_uv <= vec2<f32>(1.0));
    let osize = vec2<i32>(frame.output_size.xy);

    // Lock following. When this frame's samples miss a thin feature, depth
    // dilation cannot see it and the pixel carries the background's motion
    // (parallax), so the reprojection lands beside the feature's history. Look
    // for a locked history pixel near the reprojected position whose own last
    // motion (constant-velocity assumption) brings it onto this output pixel.
    var follow = false;
    var follow_px = vec2<i32>(0);
    var follow_aux = vec4<f32>(0.0);
    var follow_off = vec2<f32>(0.0);
    if (frame.tune.z > 0.5 && !missed) {
        let base_h = vec2<i32>(floor(history_uv * frame.output_size.xy));
        let own = textureLoad(aux_tex, clamp(base_h, vec2<i32>(0), osize - vec2<i32>(1)), 0).x;
        // Any lock above the ownership floor (0.1, as a claim and an owned lock) is followed
        // (ADR-7876). At 0.5 a member's lock decays over its unsampled frames and a covered wall
        // sample cuts it to a quarter, so a 0.3 floor dropped the member in the rows where the point
        // had already moved to the next pixel: neither pixel kept it, and the bar's contrast came down
        // to how many rows still held the member (bar 90 read 0.50 of native, the bar itself).
        var best = max(own, 0.1);
        for (var y: i32 = -1; y <= 1; y = y + 1) {
            // ±2 px across: under parallax the wall's reprojection lands up to two pixels from the
            // member's history (1.9 px/frame in the chart's orbit, ADR-7694).
            for (var x: i32 = -2; x <= 2; x = x + 1) {
                let hq = base_h + vec2<i32>(x, y);
                if (any(hq < vec2<i32>(0)) || any(hq >= osize)) {
                    continue;
                }
                let a = textureLoad(aux_tex, hq, 0);
                if (a.x <= best) {
                    continue;
                }
                let hm = textureLoad(aux_motion_tex, hq, 0);
                let m = hm.xy;
                let lands = vec2<f32>(hq) + vec2<f32>(0.5) + hm.zw - m - out_px;
                if (all(abs(lands) < vec2<f32>(0.5))) {
                    best = a.x;
                    follow = true;
                    follow_px = hq;
                    follow_aux = a;
                    follow_off = lands;
                    used_motion = m;
                }
            }
        }
    }

    // Motion consistency. Every history pixel stores the motion its content
    // moved with. Reprojecting with a clearly different motion means the
    // history there belongs to another layer: a dilated background pixel that
    // fetched a thin foreground feature, or the ground behind a moving object
    // that fetched the object (nearly coplanar, so the depth test cannot see it).
    // Such a pixel gets no lock, the tight box and a short accumulation
    // (velocity-difference rejection: Yang, Liu & Salvi, JCGT 2020, § 5.2).
    // Strongly inconsistent history is dropped like a disocclusion.
    let aux_q = clamp(vec2<i32>(history_uv * frame.output_size.xy), vec2<i32>(0), osize - vec2<i32>(1));
    var inconsistent = 0.0;
    // A claimed surface (`missed`) was matched on its own history pixel's motion already.
    if (!follow && !missed && frame.tune.z > 0.5) {
        let m_hist = textureLoad(aux_motion_tex, aux_q, 0).xy;
        let diff = length(m_hist - used_motion);
        // Full at ~0.8 px of disagreement (plus 5 % of the motion for smooth
        // camera acceleration): a 1 px parallax step must not keep the history.
        inconsistent = clamp((diff - (0.3 + 0.05 * length(used_motion))) * 2.0, 0.0, 1.0);
    }
    let valid = frame.tune.z > 0.5 && ((in_bounds && disocclusion < 0.5 && inconsistent < 0.75) || follow);

    if (!valid) {
        // No history: a fresh accumulation. It starts from this frame's
        // samples with their kernel weight (the same weighting a converged
        // pixel has), so a reset region matches its converged neighbours within
        // a few frames; only a pixel with no near sample uses the wide smooth
        // reconstruction, at a low weight.
        var lock = 0.0;
        if (on_pixel && (ridge || depth_thin > 0.5) && reactive < 0.5) {
            lock = 1.0;
        }
        // One layer only (ADR-2638): a pixel with no history cannot average across a silhouette.
        // A layer is what moves alike (the texels' own, undilated motion): the layer of the
        // sample inside this output pixel, else of the farthest sample in reach, because what
        // restarts beside a silhouette is mostly what it just uncovered. A sample of another
        // layer would drag the object over the ground it left (smear) or make a thin member one
        // render texel wide for a frame. The history pixel is labelled with the layer's motion,
        // not the dilated one: labelled with the silhouette's motion it would pass the motion
        // consistency test next frame and hold the bleed, unclipped, in a halo around the object.
        var layer_motion = vec2<f32>(0.0);
        var layer_d2 = 1.0e+6;
        var far = -1.0;
        var far_motion = vec2<f32>(0.0);
        for (var y: i32 = -1; y <= 1; y = y + 1) {
            for (var x: i32 = -1; x <= 1; x = x + 1) {
                let q = clamp(centre + vec2<i32>(x, y), vec2<i32>(0), rsize - vec2<i32>(1));
                let s = textureLoad(depth_tex, q, 0);
                let own = s.zw * frame.output_size.xy;
                if (s.x > far) {
                    far = s.x;
                    far_motion = own;
                }
                let d = (vec2<f32>(centre + vec2<i32>(x, y)) + vec2<f32>(0.5) + jitter - p_in) * ratio;
                if (all(abs(d) <= vec2<f32>(0.5)) && dot(d, d) < layer_d2) {
                    layer_d2 = dot(d, d);
                    layer_motion = own;
                }
            }
        }
        if (layer_d2 >= 1.0e+6) {
            layer_motion = far_motion;
        }
        let layer_tolerance = 0.3 + 0.05 * length(layer_motion);
        var layer_sharp = vec3<f32>(0.0);
        var layer_sharp_w = 0.0;
        var layer_smooth = vec3<f32>(0.0);
        var layer_smooth_w = 0.0;
        for (var y: i32 = -1; y <= 1; y = y + 1) {
            for (var x: i32 = -1; x <= 1; x = x + 1) {
                let q = clamp(centre + vec2<i32>(x, y), vec2<i32>(0), rsize - vec2<i32>(1));
                let own = textureLoad(depth_tex, q, 0).zw * frame.output_size.xy;
                if (length(own - layer_motion) > layer_tolerance) {
                    continue;
                }
                let t = to_t(textureLoad(current_tex, q, 0).rgb);
                let d_in = vec2<f32>(centre + vec2<i32>(x, y)) + vec2<f32>(0.5) + jitter - p_in;
                let d_out = d_in * ratio;
                let ws = exp(-2.0 * dot(d_in, d_in));
                let wp = exp(-frame.tune3.w * dot(d_out, d_out));
                layer_smooth += t * ws;
                layer_smooth_w += ws;
                layer_sharp += t * wp;
                layer_sharp_w += wp;
            }
        }
        var start = layer_smooth / max(layer_smooth_w, 1.0e-6);
        var start_weight = 0.05;
        if (layer_sharp_w > 0.05) {
            start = layer_sharp / layer_sharp_w;
            // With no sample of its own the pixel only borrows the nearest one, which beside an
            // edge is the other side's (ADR-4934): a placeholder, not evidence. At its full kernel
            // weight the uncovered ground behind a moving object kept the neighboring stripe's
            // color for 3–8 frames at 0.5 (ghosting 0.028).
            start_weight = select(PLACEHOLDER_WEIGHT, min(layer_sharp_w, 1.0), layer_d2 < 1.0e+6);
        }
        let start_own = select(luma(start), lc, on_pixel);
        return ResolveOut(
            vec4<f32>(from_t(start), start_own),
            vec4<f32>(lock, lc, lc, start_weight),
            vec4<f32>(
                layer_motion,
                select(vec2<f32>(0.0), clamp(nearest_d, vec2<f32>(-0.5), vec2<f32>(0.5)), lock > 0.0),
            ),
        );
    }

    let hist_q = select(aux_q, claim_px, missed);
    var aux = textureLoad(aux_tex, hist_q, 0);
    // Where the locked feature lies in this pixel now (ADR-7694): the claim's or the follow's
    // point, else the history pixel's carried by the motion used.
    var lock_off = select(claim_off, follow_off, follow);
    if (!missed && !follow) {
        let hist_off = textureLoad(aux_motion_tex, hist_q, 0).zw;
        lock_off = vec2<f32>(hist_q) + vec2<f32>(0.5) + hist_off - used_motion - out_px;
    }
    // The history was written at the previous frame's pre-exposure (jitter.z = new / old, 0 = 1).
    let history_scale = select(1.0, frame.jitter.z, frame.jitter.z > 0.0);
    var history_t: vec3<f32>;
    if (follow) {
        aux = follow_aux;
        history_t = to_t(textureLoad(history_tex, follow_px, 0).rgb * history_scale);
    } else if (missed) {
        // A claimed thin feature moves with its tracked point, unfiltered (ADR-7694).
        history_t = to_t(textureLoad(history_tex, claim_px, 0).rgb * history_scale);
    } else if (aux.x * frame.tune3.y > 0.1 && all(abs(lock_off) <= vec2<f32>(0.5))) {
        // A pixel that owns a lock (its tracked point lands here) takes the lock's history pixel
        // unfiltered: resampling a 1 px feature at a fractional offset mixes it with the wall.
        history_t = to_t(textureLoad(history_tex, hist_q, 0).rgb * history_scale);
    } else {
        history_t = to_t(history_catmull_rom(history_uv) * history_scale);
    }
    var lock = aux.x * frame.tune3.y;
    // A lock is inherited only by the pixel its tracked point lands in (ADR-7694).
    if (!missed && !follow && any(abs(lock_off) > vec2<f32>(0.5))) {
        lock = 0.0;
    }
    let owns_thin = lock > 0.1 || missed || (on_pixel && (ridge || depth_thin > 0.5)) || hi_own.x < lo_own.x;
    let lo_b = select(lo_own, lo, owns_thin);
    let hi_b = select(hi_own, hi, owns_thin);
    let lock_luma = aux.y;
    let prev_luma = aux.z;
    // Accumulated sample weight (Σ coverage), not a frame count.
    var weight = aux.w;
    lock = lock * (1.0 - inconsistent);
    weight = mix(weight, min(weight, 0.25), inconsistent);

    let h = ycocg(history_t);

    // Rectification. Where the reprojection is trusted (depth-validated camera
    // motion of a static surface) the box is the neighbourhood min/max: a thin or
    // corner sample is 1 of 9 and a variance box would clip it away. A moving
    // object, a reactive pixel or a near-disocclusion narrows it to the variance
    // box, whose gamma narrows again with motion (Salvi 2016).
    let tight = clamp(max(max(max(dynamic, reactive), disocclusion * 2.0), inconsistent), 0.0, 1.0);
    let gamma = mix(frame.tune2.x, frame.tune2.y, clamp(motion_px * 0.5, 0.0, 1.0));
    var box_lo = mix(lo_b, max(lo_b, mean - gamma * sigma), tight);
    var box_hi = mix(hi_b, min(hi_b, mean + gamma * sigma), tight);

    // Temporal range of a held pixel (ADR-2638). Detail above the render resolution's
    // Nyquist limit (1 px bars at 0.5) aliases: all 3×3 samples of a frame can sit on one
    // phase, the box collapses to it, and clipping to it makes the pixel follow the phase
    // frame to frame (a held chart flickered by 11 levels). A still, trusted pixel stretches
    // the box's luma toward the luma it observed last frame (aux.z, a short average of its
    // own observations), so the accumulation averages the phases, and with the jitter
    // resolves the detail, instead of following them. A moving, dynamic, disoccluded or
    // inconsistent pixel (tight) keeps the box.
    let held = (1.0 - clamp(motion_px * 2.0, 0.0, 1.0)) * (1.0 - tight);
    box_lo.x = min(box_lo.x, mix(box_lo.x, prev_luma, held));
    box_hi.x = max(box_hi.x, mix(box_hi.x, prev_luma, held));

    // Uncovered pixels (ADR-4859). Every texel of a frame shares one jitter, so detail at the
    // render resolution's Nyquist limit (1 px bars at 0.5) puts all 3×3 samples on one phase
    // while the camera moves too: the box collapses to that phase, and an output pixel that no
    // sample of this frame lands in is clipped to its neighbour's colour (the band was lost under
    // a 2 px/frame pan). Such a pixel has no evidence of its own this frame. Each history pixel
    // carries the luma a sample inside its footprint last saw (`own`, the history's alpha), and
    // where the pixel is uncovered and its reprojection trusted (not tight) the box's luma
    // stretches toward that observation: what this frame did not see is not clipped away, but a
    // history value the pixel never observed (a reprojected edge a pixel off, Catmull-Rom
    // ringing) still is. The observation is bounded by the four history texels the fetch reads.
    // Covered means the nearest sample lies within ½ output px on both axes (at most one sample
    // lands in a pixel when upscaling); the kernel's radial weight would count a sample in the next
    // column but on this row as half covered and clip the pixel to that column's colour.
    let nearest_far = max(abs(nearest_d.x), abs(nearest_d.y));
    let uncovered = smoothstep(0.45, 0.55, nearest_far);
    let keep = uncovered * (1.0 - tight);
    let own_q = select(hist_q, follow_px, follow);
    let own_prev_t = rescale_t(textureLoad(history_tex, own_q, 0).a, history_scale);
    if (keep > 0.0) {
        let hp = history_uv * frame.output_size.xy - vec2<f32>(0.5);
        let h0 = vec2<i32>(floor(hp));
        var foot_lo = 1.0e+6;
        var foot_hi = -1.0e+6;
        for (var fy: i32 = 0; fy <= 1; fy = fy + 1) {
            for (var fx: i32 = 0; fx <= 1; fx = fx + 1) {
                let fq = clamp(h0 + vec2<i32>(fx, fy), vec2<i32>(0), osize - vec2<i32>(1));
                let fl = ycocg(to_t(textureLoad(history_tex, fq, 0).rgb * history_scale)).x;
                foot_lo = min(foot_lo, fl);
                foot_hi = max(foot_hi, fl);
            }
        }
        let kept = clamp(own_prev_t, foot_lo, foot_hi);
        box_lo.x = min(box_lo.x, mix(box_lo.x, kept, keep));
        box_hi.x = max(box_hi.x, mix(box_hi.x, kept, keep));
    }

    // Locks. A well-covered sample that disagrees with the lock (and is no
    // ridge) ends it; a reactive pixel or a disocclusion drops it.
    let cur_l = ycocg(current_t).x;
    if (coverage > exp(-frame.tune3.w * 0.25) && !ridge && depth_thin < 0.5
        && abs(cur_l - lock_luma) > 0.5 * max(max(cur_l, lock_luma), 0.02)) {
        lock = lock * 0.25;
    }
    lock = clamp(lock * (1.0 - reactive) * (1.0 - disocclusion), 0.0, 1.0);
    // A lock does not bypass rectification: it stretches the box's luma range
    // toward the locked feature, so a thin feature this frame's samples missed
    // keeps its history while any other stale value is still clipped.
    box_lo.x = min(box_lo.x, mix(box_lo.x, lock_luma, lock));
    box_hi.x = max(box_hi.x, mix(box_hi.x, lock_luma, lock));
    // A missed thin surface: nothing of this frame describes it, so nothing clips it.
    let rect = select(clip_to_box(h, box_lo, box_hi), h, missed);
    // A claimed pixel has no sample of its feature this frame (a luma ridge there is a valley or another
    // member, ADR-7694); the tracked offset and the sample blend follow a depth-thin sample on the pixel.
    let seen = on_pixel && (ridge || depth_thin > 0.5) && reactive < 0.5 && !missed;
    let seen_thin = on_pixel && depth_thin > 0.5 && reactive < 0.5 && !missed;
    if (seen) {
        lock = 1.0;
    }
    let new_lock_luma = select(lock_luma, lc, seen);
    lock_off = clamp(select(lock_off, nearest_d, seen_thin), vec2<f32>(-0.5), vec2<f32>(0.5));

    // Accumulation: the history's weight is capped so it keeps adapting.
    let max_weight = frame.tune.w * 0.6;
    // Proper weighted accumulation: the history is Σ wᵢ·cᵢ / Σ wᵢ over every
    // sample it has seen, so this frame adds its kernel weights (not just the
    // nearest sample's) and the converged image is the exp(−k·d²) kernel itself.
    // A young pixel (ADR-4934) takes only a sample inside it, so its first own sample replaces
    // the placeholder instead of the off-pixel samples around it slowly pulling it over.
    let young = aux.w < YOUNG_WEIGHT;
    let frame_weight = select(select(min(sharp_w, 1.0), 0.0, young && !on_pixel), 0.0, missed);
    let n = min(weight, max_weight) + frame_weight;
    var alpha = frame_weight / max(n, 1.0e-3);

    // Anti-flicker: a still, unclipped pixel whose luma jumps against the
    // history takes less of the new frame.
    let clip_amount = length(rect - h);
    let still = 1.0 - clamp(motion_px * 2.0, 0.0, 1.0);
    let rect_t = rgb_from_ycocg(rect);
    let flicker = abs(cur_l - prev_luma) / max(cur_l + prev_luma, 1.0e-3);
    // Not for a young pixel: its history is a placeholder, and its jump to its own sample is not
    // flicker.
    if (clip_amount < 1.0e-3 && !young) {
        alpha = alpha * (1.0 - 0.5 * still * clamp(flicker * 2.0, 0.0, 1.0));
    }
    // A moving pixel that sees its thin feature inside it takes most of that sample (ADR-7694): its
    // history was carried through unsampled frames, and the sample is the feature's own evidence.
    if (seen_thin && motion_px > 0.25) {
        alpha = max(alpha, 0.8);
    }
    alpha = mix(alpha, 1.0, reactive);
    alpha = clamp(alpha, 0.0, 1.0);

    let result_t = mix(rect_t, current_t, alpha);
    var aux_out = vec4<f32>(lock, new_lock_luma, select(mix(prev_luma, cur_l, 0.5), prev_luma, missed), n);
    if (frame.tune3.z > 4.5) {
        // Debug view 5: this frame's reconstruction only (tonemapped luma).
        aux_out.z = ycocg(current_t).x;
    } else if (frame.tune3.z > 3.5) {
        // Debug view 4: motion error against a 2 px/frame pan, ×10 (chart harness).
        aux_out.z = 0.5 + 10.0 * (motion.x * frame.output_size.x - 2.0);
    } else if (frame.tune3.z > 2.5) {
        // Debug view 3: disocclusion in aux.z (the anti-flicker luma is lost).
        aux_out.z = disocclusion;
    }
    // The pixel's own observation: this frame's nearest sample where it lies in the pixel, else
    // the one carried with the history (tonemapped).
    let own_t = select(own_prev_t, lc, on_pixel);
    return ResolveOut(vec4<f32>(finite_or_zero(from_t(result_t)), own_t), aux_out, vec4<f32>(used_motion, lock_off));
}
