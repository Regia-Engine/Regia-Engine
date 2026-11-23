// SPDX-License-Identifier: MPL-2.0
//! HLOD proxy shadows (ADR-3253, spec 78 § 11.7) on the runtime path, lavapipe readback: a far
//! proxy hill cast with [`regia_render::hlod_proxy_shadow_cast`] (a `ShadowCast::Local` caster that
//! reaches only the sun cascades its distance picks) shadows the ground where the full-detail hill
//! did. The proxy is a coarse sphere (10 segments) standing in for a fine one (48); the bound is in
//! screen pixels at a fixed camera: every ground shadow pixel of either lies within
//! [`BOUND_PX`] of a ground shadow pixel of the other, and their shadow areas agree within
//! [`AREA_TOLERANCE`].
//!
//! No wall-clock and no golden: each shadow is the difference between the hill casting and the same
//! hill marked `NotShadowCaster`, ambient light as in `not_shadow_caster` and no GI probes (their
//! bounce tints the shadow with the caster's red), 8 levels of darkening count as shadow.
//! Only the ground counts: the caster has its own red material, and a pixel that shows the caster in
//! any of the four frames is left out, because a faceted sphere shades and self-shadows its own
//! facets unlike a smooth one, and that is not where its shadow lands. The penumbra is soft (the
//! mean darkening over the frame is ~2 luma levels against the 8-level cutoff), so a lone pixel that
//! crosses the cutoff is a speck, not an edge: at most [`MAX_SPECKS`] isolated pixels may lie
//! outside [`BOUND_PX`], and no pixel with a same-mask 8-neighbour may.

use regia_app::prelude::*;
use regia_render::{
    DirectionalLightUniform, LabMaterial, LabMesh, LabScene, LabVertex, MeshRenderer, MeshShadowCast,
    NotShadowCaster, RenderDevice, extract_frame, hlod_proxy_shadow_cast, spawn_lab_scene,
};

const W: u32 = 256;
/// The caster: a sphere standing on the floor, meters.
const SPHERE_RADIUS: f32 = 0.45;
const H: u32 = 160;
/// Most screen pixels between a shadow pixel of one hill and the nearest of the other: the
/// 10-segment proxy's silhouette lies within `SPHERE_RADIUS * (1 - cos 18°)` = 2 cm of the fine
/// one's, under a pixel here, plus a shadow texel of edge rounding on each side.
const BOUND_PX: i32 = 4;
/// Largest relative difference of the two shadow areas.
const AREA_TOLERANCE: f32 = 0.25;
/// Most isolated single pixels allowed outside [`BOUND_PX`], over both directions: the masks
/// threshold a soft penumbra (a ~2-level mean darkening against the 8-level cutoff), and a lone
/// pixel crossing the cutoff has no same-mask neighbour (measured on lavapipe: the one pixel
/// outside the bound sat 5 px from the nearest shadow pixel of either mask, with all 8
/// neighbours in neither mask).
const MAX_SPECKS: usize = 2;

fn vertex(position: [f32; 3], normal: [f32; 3]) -> LabVertex {
    LabVertex {
        position,
        normal,
        uv: [0.0; 2],
        tangent: [1.0, 0.0, 0.0, 1.0],
        color: [1.0; 4],
        joints: [0; 4],
        weights: [0.0; 4],
    }
}

/// The lit floor of `tests/not_shadow_caster.rs` (a scene that is known to cast) with a sphere of
/// `sides` segments standing on it: 48 for the full-detail caster, 10 for its proxy.
fn scene(sides: u32) -> LabScene {
    let h = 2.5;
    let ground = [[-h, 0.0, -h], [h, 0.0, -h], [h, 0.0, h], [-h, 0.0, h]]
        .into_iter()
        .map(|p| vertex(p, [0.0, 1.0, 0.0]))
        .collect();
    let (sphere, sphere_indices) = regia_render::uv_sphere(SPHERE_RADIUS, sides, (sides / 2).max(3));
    let sphere = sphere
        .into_iter()
        .map(|mut v| {
            v.position[1] += SPHERE_RADIUS;
            v
        })
        .collect();
    LabScene {
        width: W,
        height: H,
        eye: [1.6, 2.2, 3.2],
        target: [0.0, 0.3, 0.0],
        fov_y: 45.0_f32.to_radians(),
        near: 0.1,
        far: 30.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 14.8,
        ambient_lux: 10_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.35, -1.0, 0.25], [1.0, 0.98, 0.94], 100_000.0),
        local_lights: vec![],
        materials: vec![
            LabMaterial {
                base_color: [0.75, 0.75, 0.78, 1.0],
                roughness: 0.6,
                ..LabMaterial::default()
            },
            // The caster: red, so its own pixels can be told from the grey ground.
            LabMaterial {
                base_color: [0.8, 0.15, 0.1, 1.0],
                roughness: 0.6,
                ..LabMaterial::default()
            },
        ],
        meshes: vec![
            LabMesh {
                vertices: ground,
                indices: vec![0, 2, 1, 0, 3, 2],
                material: 0,
                ..LabMesh::unskinned()
            },
            LabMesh {
                vertices: sphere,
                indices: sphere_indices,
                material: 1,
                ..LabMesh::unskinned()
            },
        ],
    }
}

/// How the hill casts.
#[derive(Clone, Copy)]
enum Cast {
    /// The default (`ShadowCast::All`): the full-detail hill.
    All,
    /// As an HLOD proxy with shadows on: far cascades only.
    Proxy,
    /// `NotShadowCaster`: the reference without a shadow.
    Off,
}

fn render(device: &RenderDevice, sides: u32, cast: Cast) -> Vec<u8> {
    let scene = scene(sides);
    let mut app = App::new();
    app.init_resource::<regia_render::ExtractedFrame>();
    spawn_lab_scene(app.world_mut(), &scene);
    let hill = {
        let mut query = app.world_mut().query::<(Entity, &MeshRenderer)>();
        query
            .iter(app.world())
            .find(|(_, r)| r.mesh.vertices.len() > 4)
            .map(|(e, _)| e)
            .expect("the hill entity")
    };
    match cast {
        Cast::All => {}
        Cast::Proxy => {
            let radius = SPHERE_RADIUS;
            app.world_mut()
                .entity_mut(hill)
                .insert(MeshShadowCast(hlod_proxy_shadow_cast(radius, 2)));
        }
        Cast::Off => {
            app.world_mut().entity_mut(hill).insert(NotShadowCaster);
        }
    }
    app.add_systems(Render, extract_frame);
    app.update();
    let mut extracted = app.world().resource::<regia_render::ExtractedFrame>().clone();
    // The shadow maps alone (ADR-5960 (4)). The default profile (Desktop High) traces GI probes,
    // and since ADR-5716 they see the flat ground quad: the red caster then bounces red light into
    // its own shadow, `caster_pixel` takes those ground pixels for the caster, and both masks lose
    // them (lavapipe at the gi-fix-g-land merge: full 394 -> 149 px, exactly the 245 px the caster
    // mask gained). The probes' bounce is the same in the casting and the reference frame and is
    // not what this test measures.
    extracted.quality.gi_probes = false;
    let frame = extracted.frame_scene(0, W, H);
    regia_render::FrameRenderer::new()
        .render_offscreen(device, &frame)
        .expect("render")
        .pixels
}

fn luma(pixels: &[u8], i: usize) -> f32 {
    let p = &pixels[i * 4..i * 4 + 3];
    (f32::from(p[0]) + f32::from(p[1]) + f32::from(p[2])) / 3.0
}

/// Whether pixel `i` shows the red caster (the ground is grey).
fn caster_pixel(pixels: &[u8], i: usize) -> bool {
    let p = &pixels[i * 4..i * 4 + 3];
    i32::from(p[0]) > i32::from(p[1]) + 12
}

/// Ground pixels the cast darkens by more than 8 levels against the same frame without the shadow;
/// `caster` marks the pixels left out (the caster in any frame).
fn shadow_mask(cast: &[u8], reference: &[u8], caster: &[bool]) -> Vec<bool> {
    (0..(W * H) as usize)
        .map(|i| !caster[i] && luma(cast, i) + 8.0 < luma(reference, i))
        .collect()
}

/// Pixels of `a` farther than `BOUND_PX` from every pixel of `b`, as (x, y), for the log.
fn outside_at(a: &[bool], b: &[bool]) -> Vec<(i32, i32)> {
    let at =
        |x: i32, y: i32| x >= 0 && y >= 0 && x < W as i32 && y < H as i32 && b[(y * W as i32 + x) as usize];
    let mut out = Vec::new();
    for y in 0..H as i32 {
        for x in 0..W as i32 {
            if a[(y * W as i32 + x) as usize]
                && !(-BOUND_PX..=BOUND_PX).any(|dy| (-BOUND_PX..=BOUND_PX).any(|dx| at(x + dx, y + dy)))
            {
                out.push((x, y));
            }
        }
    }
    out
}

/// Whether the pixel has no same-mask 8-neighbour: a lone threshold speck, not an edge pixel.
fn isolated(mask: &[bool], x: i32, y: i32) -> bool {
    (-1..=1).all(|dy| {
        (-1..=1).all(|dx| {
            (dx == 0 && dy == 0)
                || x + dx < 0
                || y + dy < 0
                || x + dx >= W as i32
                || y + dy >= H as i32
                || !mask[((y + dy) * W as i32 + (x + dx)) as usize]
        })
    })
}

/// ASCII dump of the masks in `(x, y)`'s neighbourhood: `F` full only, `P` proxy only, `B` both,
/// `.` neither. For reading a stray pixel's shape (a threshold speck or a real edge offset).
fn dump_at(full: &[bool], proxy: &[bool], x: i32, y: i32) -> String {
    let mut out = String::new();
    for dy in -6..=6 {
        for dx in -6..=6 {
            let (px, py) = (x + dx, y + dy);
            let i = (py * W as i32 + px) as usize;
            let c = if px < 0 || py < 0 || px >= W as i32 || py >= H as i32 {
                ' '
            } else {
                match (full[i], proxy[i]) {
                    (true, true) => 'B',
                    (true, false) => 'F',
                    (false, true) => 'P',
                    (false, false) => '.',
                }
            };
            out.push(c);
        }
        out.push('\n');
    }
    out
}

#[test]
fn a_far_proxy_hill_shadows_where_the_full_hill_did() {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => {
            eprintln!("skip hlod_proxy_shadow: {err}");
            return;
        }
    };
    let (full_cast, full_off) = (render(&device, 48, Cast::All), render(&device, 48, Cast::Off));
    let mean = |p: &[u8]| (0..(W * H) as usize).map(|i| luma(p, i)).sum::<f32>() / (W * H) as f32;
    eprintln!(
        "hlod proxy shadow: mean luma full hill casting {:.2}, not casting {:.2}",
        mean(&full_cast),
        mean(&full_off)
    );
    let (proxy_cast, proxy_off) = (render(&device, 10, Cast::Proxy), render(&device, 10, Cast::Off));
    let caster: Vec<bool> = (0..(W * H) as usize)
        .map(|i| {
            [&full_cast, &full_off, &proxy_cast, &proxy_off]
                .iter()
                .any(|p| caster_pixel(p, i))
        })
        .collect();
    let caster_px = caster.iter().filter(|c| **c).count();
    assert!(caster_px > 100, "the caster is on screen ({caster_px} px)");
    let full = shadow_mask(&full_cast, &full_off, &caster);
    let proxy = shadow_mask(&proxy_cast, &proxy_off, &caster);
    let (full_px, proxy_px) = (
        full.iter().filter(|s| **s).count(),
        proxy.iter().filter(|s| **s).count(),
    );
    let stray_at = outside_at(&proxy, &full);
    let missed_at = outside_at(&full, &proxy);
    let (stray, missed) = (stray_at.len(), missed_at.len());
    let hard = |at: &[(i32, i32)], mask: &[bool]| at.iter().filter(|(x, y)| !isolated(mask, *x, *y)).count();
    let hard_stray = hard(&stray_at, &proxy);
    let hard_missed = hard(&missed_at, &full);
    eprintln!(
        "hlod proxy shadow: caster {caster_px} px left out; ground shadow full {full_px} px, proxy {proxy_px} px, \
         proxy outside the bound {stray} {:?}, full outside the bound {missed} {:?}",
        stray_at, missed_at
    );
    for (x, y) in missed_at.iter().chain(stray_at.iter()).take(4) {
        eprintln!("neighbourhood of ({x}, {y}):\n{}", dump_at(&full, &proxy, *x, *y));
    }
    assert!(
        full_px > 100,
        "the full hill casts a visible shadow ({full_px} px)"
    );
    assert!(proxy_px > 100, "the proxy casts a visible shadow ({proxy_px} px)");
    assert_eq!(
        hard_stray + hard_missed,
        0,
        "every shadow pixel with a same-mask neighbour is within {BOUND_PX} px of the other mask"
    );
    assert!(
        stray + missed <= MAX_SPECKS,
        "at most {MAX_SPECKS} isolated threshold specks outside the {BOUND_PX} px bound, got {stray} + {missed}"
    );
    let area = (proxy_px as f32 - full_px as f32).abs() / full_px as f32;
    assert!(area <= AREA_TOLERANCE, "shadow areas differ by {area:.3}");
}
