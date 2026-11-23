// SPDX-License-Identifier: MPL-2.0
//! Double-sided clusters in the software rasterizer (ADR-2689): a CPU reference of
//! `vis_software.wgsl`'s coverage, and the GPU frame against the hardware raster.
//!
//! The reference is the shader's integer edge test line for line (1/256-pixel positions, samples
//! at pixel centres, the top-left rule, the winding swap of a front face). What it proves:
//! a back face rasterizes only where its instance is double-sided; when it does, it covers the
//! pixels its front face covers; and a closed fan seen from behind still covers every pixel inside
//! it exactly once, so turning back faces on opens no crack and doubles no pixel.

use super::*;

/// One covered pixel of the reference: `(x, y, depth)`.
type Sample = (u32, u32, f32);

/// `vis_software.wgsl` `edge`: twice the signed area of `(a, b, p)` in 1/256 pixels squared.
fn edge(a: [i32; 2], b: [i32; 2], p: [i32; 2]) -> i32 {
    (b[0] - a[0]) * (p[1] - a[1]) - (b[1] - a[1]) * (p[0] - a[0])
}

/// `vis_software.wgsl` `edge_bias`: the top-left rule in the y-down grid.
fn edge_bias(a: [i32; 2], b: [i32; 2]) -> i32 {
    let d = [b[0] - a[0], b[1] - a[1]];
    if d[1] < 0 || (d[1] == 0 && d[0] > 0) {
        0
    } else {
        -1
    }
}

/// `vis_software.wgsl` `rasterize_triangle`: the samples one triangle covers on a
/// `width × height` grid. Corners are `[x, y]` in 1/256 pixels (y down) with a depth each;
/// `cull_back` is the round's cull mode for a single-sided instance (false for a double-sided one).
fn rasterize(corners: [([i32; 2], f32); 3], width: i32, height: i32, cull_back: bool) -> Vec<Sample> {
    let [(a, za), (b, zb), (c, zc)] = corners;
    let signed_area = edge(a, b, c);
    if signed_area == 0 {
        return Vec::new();
    }
    // Counter-clockwise in NDC (front) is negative in the y-down grid.
    let front = signed_area < 0;
    if !front && cull_back {
        return Vec::new();
    }
    let (p1, p2, z1, z2) = if front { (c, b, zc, zb) } else { (b, c, zb, zc) };
    let area = signed_area.unsigned_abs() as f32;
    let low = [a[0].min(p1[0]).min(p2[0]), a[1].min(p1[1]).min(p2[1])];
    let high = [a[0].max(p1[0]).max(p2[0]), a[1].max(p1[1]).max(p2[1])];
    let first = [((low[0] - 128) >> 8).max(0), ((low[1] - 128) >> 8).max(0)];
    let last = [
        ((high[0] - 128 + 255) >> 8).min(width - 1),
        ((high[1] - 128 + 255) >> 8).min(height - 1),
    ];
    let (bias0, bias1, bias2) = (edge_bias(p1, p2), edge_bias(p2, a), edge_bias(a, p1));
    let mut out = Vec::new();
    for y in first[1]..=last[1] {
        for x in first[0]..=last[0] {
            let sample = [x * 256 + 128, y * 256 + 128];
            let w0 = edge(p1, p2, sample);
            let w1 = edge(p2, a, sample);
            let w2 = edge(a, p1, sample);
            if w0 + bias0 < 0 || w1 + bias1 < 0 || w2 + bias2 < 0 {
                continue;
            }
            let z = (w0 as f32 * za + w1 as f32 * z1 + w2 as f32 * z2) / area;
            if z > 0.0 && z <= 1.0 {
                out.push((x as u32, y as u32, z));
            }
        }
    }
    out
}

fn pixels(samples: &[Sample]) -> Vec<(u32, u32)> {
    let mut p: Vec<(u32, u32)> = samples.iter().map(|&(x, y, _)| (x, y)).collect();
    p.sort_unstable();
    p
}

/// A triangle that faces the camera (counter-clockwise in NDC: negative area in the y-down grid).
const FRONT: [([i32; 2], f32); 3] = [
    ([2 * 256 + 37, 2 * 256 + 11], 0.5),
    ([3 * 256 + 200, 13 * 256 + 140], 0.6),
    ([12 * 256 + 91, 9 * 256 + 3], 0.4),
];

fn reversed(t: [([i32; 2], f32); 3]) -> [([i32; 2], f32); 3] {
    [t[0], t[2], t[1]]
}

#[test]
fn reference_front_face_is_front_facing() {
    let [(a, _), (b, _), (c, _)] = FRONT;
    assert!(
        edge(a, b, c) < 0,
        "the fixture's front face winds counter-clockwise in NDC"
    );
}

#[test]
fn a_back_face_rasterizes_only_on_a_double_sided_instance() {
    let back = reversed(FRONT);
    let front_px = pixels(&rasterize(FRONT, 16, 16, true));
    assert!(front_px.len() > 20, "{} pixels", front_px.len());
    // A single-sided instance: the back face is culled, the front face drawn.
    assert!(rasterize(back, 16, 16, true).is_empty());
    // A double-sided instance: the back face covers exactly the front face's pixels.
    let back_px = pixels(&rasterize(back, 16, 16, false));
    assert_eq!(back_px, front_px);
    // And the front face is unchanged by the instance's flag.
    assert_eq!(pixels(&rasterize(FRONT, 16, 16, false)), front_px);
}

/// A closed fan around an off-grid centre, seen from the front and from behind (every triangle
/// reversed): each pixel inside covers exactly once both ways, and the same pixels.
#[test]
fn back_faces_keep_the_fill_rule_watertight() {
    let centre = [10 * 256 + 37, 9 * 256 + 91];
    let n = 11;
    let rim: Vec<[i32; 2]> = (0..n)
        .map(|i| {
            let angle = std::f32::consts::TAU * i as f32 / n as f32 + 0.3;
            // Snapped to the 1/256 grid like `screen_vertex`.
            [
                centre[0] + (angle.cos() * 7.3 * 256.0).round() as i32,
                centre[1] + (angle.sin() * 6.1 * 256.0).round() as i32,
            ]
        })
        .collect();
    let fan: Vec<[([i32; 2], f32); 3]> = (0..n)
        .map(|i| {
            let (p, q) = (rim[i], rim[(i + 1) % n]);
            let t = [(centre, 0.5), (p, 0.5), (q, 0.5)];
            // Face the camera whichever way the rim runs.
            if edge(t[0].0, t[1].0, t[2].0) < 0 {
                t
            } else {
                reversed(t)
            }
        })
        .collect();
    let count = |triangles: &[[([i32; 2], f32); 3]], cull_back: bool| {
        let mut hits = std::collections::BTreeMap::<(u32, u32), u32>::new();
        for &t in triangles {
            for (x, y, _) in rasterize(t, 24, 24, cull_back) {
                *hits.entry((x, y)).or_default() += 1;
            }
        }
        hits
    };
    let front = count(&fan, true);
    assert!(front.len() > 100, "{} pixels", front.len());
    assert!(front.values().all(|&k| k == 1), "the front fan overlaps itself");
    let behind: Vec<_> = fan.iter().copied().map(reversed).collect();
    assert!(
        count(&behind, true).is_empty(),
        "single-sided: nothing from behind"
    );
    let back = count(&behind, false);
    assert!(back.values().all(|&k| k == 1), "the back fan doubles a pixel");
    assert_eq!(
        back.keys().collect::<Vec<_>>(),
        front.keys().collect::<Vec<_>>(),
        "the back fan covers other pixels than the front"
    );
}

/// The shader reads the flag the instance writes, and the cull no longer keeps double-sided
/// clusters out of the software range.
#[test]
fn the_shaders_route_and_keep_double_sided_clusters() {
    let software = include_str!("../vis_software.wgsl");
    let flag = crate::visbuffer::gpu::VIS_DOUBLE_SIDED;
    assert!(software.contains(&format!("const DOUBLE_SIDED: u32 = {flag}u;")));
    assert!(software.contains("let cull_back = params.mode.y == 1u && (inst.flags.y & DOUBLE_SIDED) == 0u;"));
    let cull = include_str!("../vis_cull.wgsl");
    let route = cull
        .split("fn takes_software(")
        .nth(1)
        .and_then(|body| body.split("\n}\n").next())
        .expect("takes_software");
    assert!(
        !route.contains("DOUBLE_SIDED"),
        "the route still refuses double-sided clusters"
    );
}

/// Small double-sided fins seen from behind, among the fixture's spheres.
fn fins_scene<'a>(
    f: &'a Fixture,
    post: &'a PostSettings,
    fin: &'a (Vec<LabVertex>, Vec<u32>),
    material: &'a LabMaterial,
) -> FrameScene<'a> {
    let mut s = scene(f, post, true, false);
    s.quality.gi_probes = false;
    for i in 0..24u64 {
        let (col, row) = ((i % 8) as f32, (i / 8) as f32);
        s.meshes.push(FrameMesh {
            is_static: false,
            mesh_key: 7,
            material_key: 1007,
            object_key: 700 + i,
            vertices: &fin.0,
            indices: &fin.1,
            material,
            model: translate(-2.8 + 0.8 * col, 0.9 + 0.7 * row, -4.0 - 0.6 * row, 0.6),
            joints: &[],
            receiver_layer: 0,
            shadow: ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        });
    }
    s
}

/// The GPU side: with the software raster on, the small double-sided fins rasterize in compute
/// (as many software clusters as the single-sided fins give), and the frame matches the hardware
/// raster's, which draws them through the cull-none pipeline.
#[test]
fn software_raster_draws_double_sided_clusters_like_the_hardware() {
    let device = crate::test_device();
    let f = fixture(2);
    let settings = PostSettings {
        exposure_ev: 15.0,
        ..post()
    };
    let fin = fin_facing_away();
    let materials = [false, true].map(|double_sided| LabMaterial {
        base_color: [0.55, 0.35, 0.25, 1.0],
        double_sided,
        ..LabMaterial::default()
    });
    let route = VisBufferSettings {
        software_raster_max_px: 24,
        ..software_on()
    };
    let single = fins_scene(&f, &settings, &fin, &materials[0]);
    let double = fins_scene(&f, &settings, &fin, &materials[1]);
    let (_, single_on) = software_frame(&device, &single, route);
    let (hardware, off) = software_frame(&device, &double, VisBufferSettings::default());
    let (software, on) = software_frame(&device, &double, route);
    eprintln!("single-sided {single_on:?}\nhardware {off:?}\nsoftware {on:?}");
    assert_eq!(off.software_clusters, 0);
    // Single-sided clusters this small take the software range whichever way they face (their
    // back-facing triangles are culled there). Double-sided ones now route the same way; before
    // ADR-2689 the 24 fins stayed in hardware and the double-sided count was 24 lower.
    assert!(single_on.software_clusters >= 24, "{single_on:?}");
    assert_eq!(
        on.software_clusters, single_on.software_clusters,
        "the double-sided fins did not take the software range: {on:?} vs single-sided {single_on:?}"
    );
    assert_eq!(on.dropped_clusters, 0);
    // The fins show (the double-sided frame is not the single-sided one) ...
    let (single_px, _) = software_frame(&device, &single, VisBufferSettings::default());
    let shown = single_px
        .as_chunks::<4>()
        .0
        .iter()
        .zip(hardware.as_chunks::<4>().0)
        .filter(|(a, b)| a.iter().zip(b.iter()).any(|(x, y)| x.abs_diff(*y) > 8))
        .count();
    assert!(shown >= 100, "the fins do not show from behind: {shown} pixels");
    // ... and the compute raster draws them as the hardware does.
    let (worst, close) = diff(&hardware, &software);
    eprintln!("software vs hardware: worst {worst}, close {close}");
    assert!(
        close >= close_floor(&device, 0.985),
        "{close} of the pixels close (worst {worst})"
    );
}

/// Past the software range's room a double-sided cluster draws through the cull-none raster, not
/// the back-face-culled list of the other overflow (ADR-2689): far, tiny, single-sided boxes fill
/// the range, and the fins, seen from close enough to measure, still show from behind.
#[test]
fn software_overflow_keeps_the_back_faces_of_double_sided_clusters() {
    let device = crate::test_device();
    let f = fixture(2);
    let settings = PostSettings {
        exposure_ev: 15.0,
        ..post()
    };
    let fin = fin_facing_away();
    let materials = [false, true].map(|double_sided| LabMaterial {
        base_color: [0.55, 0.35, 0.25, 1.0],
        double_sided,
        ..LabMaterial::default()
    });
    let cube = crate::box_mesh([0.004, 0.004, 0.004]);
    let filler = LabMaterial::default();
    let with_boxes = |material| {
        let mut s = fins_scene(&f, &settings, &fin, material);
        // The fins go last: the path lists instances in scene order, so before ADR-2689 the
        // boxes took the range and the fins overflowed into the back-face-culled list.
        let fins = s.meshes.split_off(s.meshes.len() - 24);
        // 40 × 40 single-cluster boxes on the ground far behind the fins: more than the range
        // holds even after the cull (34 × 34 left 959 of them, short of the 1024 slots).
        for i in 0..40u64 * 40 {
            let (x, z) = ((i % 40) as f32, (i / 40) as f32);
            s.meshes.push(FrameMesh {
                is_static: false,
                mesh_key: 8,
                material_key: 1008,
                object_key: 2000 + i,
                vertices: &cube.0,
                indices: &cube.1,
                material: &filler,
                model: translate(-1.7 + 0.085 * x, 0.2, -14.0 + 0.085 * z, 1.0),
                joints: &[],
                receiver_layer: 0,
                shadow: ShadowCast::Off,
                uv1: &[],
                lod_dither: 0.0,
            });
        }
        s.meshes.extend(fins);
        s
    };
    let single = with_boxes(&materials[0]);
    let double = with_boxes(&materials[1]);
    let hardware = VisBufferSettings::default();
    let (single_px, _) = software_frame(&device, &single, hardware);
    let (double_px, _) = software_frame(&device, &double, hardware);
    let small = VisBufferSettings {
        max_clusters: 1024,
        software_raster_max_px: 24,
        ..software_on()
    };
    let (overflow_px, on) = software_frame(&device, &double, small);
    eprintln!("overflow {on:?}");
    assert_eq!(
        on.software_clusters, 1024,
        "the boxes fill the software range: {on:?}"
    );
    assert_eq!(on.dropped_clusters, 0, "{on:?}");
    // The pixels the fins' back faces change, and which frame the overflow frame is nearer there.
    let pixel_distance =
        |a: &[u8; 4], b: &[u8; 4]| -> u32 { a.iter().zip(b).map(|(x, y)| u32::from(x.abs_diff(*y))).sum() };
    let (mut shown, mut kept) = (0u32, 0u32);
    for ((s, d), o) in single_px
        .as_chunks::<4>()
        .0
        .iter()
        .zip(double_px.as_chunks::<4>().0)
        .zip(overflow_px.as_chunks::<4>().0)
    {
        if s.iter().zip(d).any(|(x, y)| x.abs_diff(*y) > 8) {
            shown += 1;
            kept += u32::from(pixel_distance(o, d) < pixel_distance(o, s));
        }
    }
    eprintln!("the fins show on {shown} pixels, {kept} of them in the overflow frame");
    assert!(shown >= 100, "the fins do not show from behind: {shown} pixels");
    assert!(
        f64::from(kept) >= 0.9 * f64::from(shown),
        "the overflow lost the fins' back faces: {kept} of {shown} pixels"
    );
}
