// SPDX-License-Identifier: MPL-2.0
//! Screen-space subsurface scattering in the frame (ADR-2049): the light moves as the profile says,
//! and nothing else does.
//!
//! The plate here has a perfectly hard light edge without a shadow map: its left half's normals face
//! the sun and its right half's face away, on one flat, depth-continuous surface. That makes the
//! frame comparable with the analytic [`regia_render::subsurface::edge_response`] pixel by pixel.

use regia_render::subsurface::edge_response;
use regia_render::{
    ALPHA_BLEND, DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene,
    LabTexture, LabVertex, MaterialShading, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset,
    SkinShading, SubsurfaceFactors, SubsurfaceProfile,
};
use regia_test_utils::serial_gpu::{SerialGpu, serial_device};

const WIDTH: u32 = 240;
const HEIGHT: u32 = 160;
/// Plate half-size in world units: far past the view.
const EXTENT: f32 = 1.0;
/// The camera's distance and field of view: pixels per millimeter on the plate.
const DISTANCE: f32 = 0.1;
const FOV_Y: f32 = 0.5;
/// How far the light's change between two columns may stray from the profile's, as a share of the
/// lit side. Measured on lavapipe: 0.0059 with the taps' noise, 0.022 with the noise taken out.
const STAIR_BOUND: f32 = 0.01;

fn px_per_mm() -> f32 {
    (HEIGHT as f32 * 0.5) / ((FOV_Y * 0.5).tan() * DISTANCE) / 1000.0
}

/// A quad on the plane `z`, between `x0` and `x1`, with the normal `normal_z` along z.
fn quad(x0: f32, x1: f32, z: f32, normal_z: f32, material: usize) -> LabMesh {
    let n = [0.0, 0.0, normal_z];
    let corner = |x: f32, y: f32| {
        let u = (x + EXTENT) / (2.0 * EXTENT);
        let v = (y + EXTENT) / (2.0 * EXTENT);
        LabVertex::from_pos_normal_uv([x, y, z], n, [u, v])
    };
    LabMesh {
        vertices: vec![
            corner(x0, -EXTENT),
            corner(x1, -EXTENT),
            corner(x1, EXTENT),
            corner(x0, EXTENT),
        ],
        indices: vec![0, 1, 2, 0, 2, 3],
        material,
        ..LabMesh::unskinned()
    }
}

/// No specular to speak of (an IOR of almost 1), so the frame is the diffuse light alone.
fn matte(subsurface: SubsurfaceFactors) -> LabMaterial {
    LabMaterial {
        base_color: [0.8, 0.6, 0.5, 1.0],
        roughness: 1.0,
        ior: 1.001,
        subsurface,
        ..LabMaterial::default()
    }
}

fn skin() -> SubsurfaceFactors {
    SubsurfaceFactors::new(SubsurfaceProfile::Skin)
}

/// The plate: `meshes` under a sun along -z, nothing else lighting it.
fn plate(materials: Vec<LabMaterial>, meshes: Vec<LabMesh>) -> LabScene {
    LabScene {
        width: WIDTH,
        height: HEIGHT,
        eye: [0.0, 0.0, DISTANCE],
        target: [0.0, 0.0, 0.0],
        fov_y: FOV_Y,
        near: 0.01,
        far: 10.0,
        depth_prepass: false,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 13.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(PostSettings {
            enabled: true,
            bloom: false,
            auto_exposure: false,
            taa: false,
            ..PostSettings::default()
        }),
        directional: DirectionalLightUniform::new([0.0, 0.0, -1.0], [1.0; 3], 20_000.0),
        local_lights: Vec::new(),
        materials,
        meshes,
    }
}

fn edge_plate(material: LabMaterial) -> LabScene {
    plate(
        vec![material],
        vec![quad(-EXTENT, 0.0, 0.0, 1.0, 0), quad(0.0, EXTENT, 0.0, -1.0, 0)],
    )
}

fn quality(scatter: bool) -> FrameQuality {
    FrameQuality {
        subsurface_scattering: scatter,
        specular_aa: false,
        ..FrameQuality::LAB
    }
}

/// The frame's linear HDR image.
fn hdr(
    device: &RenderDevice,
    renderer: &mut FrameRenderer,
    lab: &LabScene,
    quality: FrameQuality,
) -> Vec<[f32; 3]> {
    let scene = lab.as_frame_scene_with_quality(quality);
    renderer.request_hdr_capture();
    renderer.render_offscreen(device, &scene).expect("frame");
    renderer
        .take_hdr_capture(device)
        .expect("hdr readback")
        .expect("the post stack ran, so the HDR copy exists")
        .rgb
}

fn row(image: &[[f32; 3]], y: u32) -> &[[f32; 3]] {
    &image[(y * WIDTH) as usize..((y + 1) * WIDTH) as usize]
}

/// The binary's one shared device, one test at a time: per-test devices on parallel threads hung
/// the NVIDIA driver (RTX 5090 GPU pre-gate, ADR-7483).
fn device() -> SerialGpu {
    serial_device().expect("headless device")
}

/// A hard light edge turns into the profile's response: red further than green, green further than
/// blue, both ways from the edge, and the frame without the blur shows the edge as it was.
#[test]
fn the_edge_follows_the_profile_at_every_pixel() {
    let device = device();
    let lab = edge_plate(matte(skin()));
    let mut renderer = FrameRenderer::new();
    let off = hdr(&device, &mut renderer, &lab, quality(false));
    let on = hdr(&device, &mut renderer, &lab, quality(true));
    let stats = renderer.subsurface_stats();
    assert_eq!(stats.reason, None, "{stats:?}");
    assert_eq!(stats.drawn, 2);
    assert!(stats.passes >= 1);

    let (off_row, on_row) = (row(&off, HEIGHT / 2), row(&on, HEIGHT / 2));
    let lit = off_row[0];
    assert!(lit.iter().all(|&c| c > 1.0), "{lit:?}");
    let dark = off_row[WIDTH as usize - 1];
    assert!(dark.iter().all(|&c| c == 0.0), "{dark:?}");

    // Per pixel the taps' noise shows; over a column it averages out to the profile.
    let reach = SubsurfaceProfile::Skin.reach_mm();
    let rows = 32..128u32;
    let (mut worst_pixel, mut worst_column) = (0.0f32, 0.0f32);
    for x in 0..WIDTH as usize {
        let inside_mm = (WIDTH as f32 * 0.5 - (x as f32 + 0.5)) / px_per_mm();
        let exact = edge_response(SubsurfaceProfile::Skin, inside_mm, reach);
        let mut mean = [0.0f32; 3];
        for y in rows.clone() {
            let value = row(&on, y)[x];
            for ch in 0..3 {
                worst_pixel = worst_pixel.max((value[ch] / lit[ch] - exact[ch]).abs());
                mean[ch] += value[ch] / lit[ch] / rows.len() as f32;
            }
        }
        for ch in 0..3 {
            worst_column = worst_column.max((mean[ch] - exact[ch]).abs());
        }
    }
    eprintln!("edge against the analytic response: worst pixel {worst_pixel}, worst column {worst_column}");
    assert!(worst_column < 0.03, "worst column {worst_column}");
    assert!(worst_pixel < 0.12, "worst pixel {worst_pixel}");

    // In the dark, three pixels past the edge: light has crossed it, red most.
    let past = on_row[WIDTH as usize / 2 + 3];
    assert!(past[0] / lit[0] > past[1] / lit[1], "{past:?}");
    assert!(past[1] / lit[1] > past[2] / lit[2], "{past:?}");
    assert!(past[0] / lit[0] > 0.08, "{past:?}");
}

/// A hard edge fades instead of climbing stairs. With the kernel's seventeen taps fixed on the pixel
/// grid, the light jumps by a tap's weight each time the edge crosses a tap and stays flat between,
/// which stays close to the profile at every pixel and passes the test above. So this one reads the
/// change from column to column against the profile's own: the taps' noise must spread each jump
/// over the span the tap stands for.
#[test]
fn a_hard_edge_does_not_climb_stairs() {
    let device = device();
    let lab = edge_plate(matte(skin()));
    let mut renderer = FrameRenderer::new();
    let off = hdr(&device, &mut renderer, &lab, quality(false));
    let on = hdr(&device, &mut renderer, &lab, quality(true));
    let lit = row(&off, HEIGHT / 2)[0];

    let reach = SubsurfaceProfile::Skin.reach_mm();
    let rows = 32..128u32;
    let column = |x: usize| {
        let mut mean = [0.0f32; 3];
        for y in rows.clone() {
            for ch in 0..3 {
                mean[ch] += row(&on, y)[x][ch] / lit[ch] / rows.len() as f32;
            }
        }
        mean
    };
    let exact = |x: usize| {
        let inside_mm = (WIDTH as f32 * 0.5 - (x as f32 + 0.5)) / px_per_mm();
        edge_response(SubsurfaceProfile::Skin, inside_mm, reach)
    };
    // The columns right at the edge hold the kernel's narrow center, where the light does change
    // fast and one pixel's reconstruction is the limit, so the stairs are read outside them.
    let (edge, mut worst) = (WIDTH as usize / 2, 0.0f32);
    for x in (1..WIDTH as usize - 1).filter(|x| x.abs_diff(edge) > 3) {
        let (now, next) = (column(x), column(x + 1));
        let (exact_now, exact_next) = (exact(x), exact(x + 1));
        for ch in 0..3 {
            worst = worst.max(((next[ch] - now[ch]) - (exact_next[ch] - exact_now[ch])).abs());
        }
    }
    eprintln!("change per column against the profile's: worst {worst}");
    assert!(worst < STAIR_BOUND, "worst {worst}");
}

/// Light neither appears nor vanishes: the sum over a wide row and over a uniform plate.
#[test]
fn the_blur_keeps_the_energy() {
    let device = device();
    let mut renderer = FrameRenderer::new();
    // A uniform plate stays where it was at every pixel.
    let lab = plate(vec![matte(skin())], vec![quad(-EXTENT, EXTENT, 0.0, 1.0, 0)]);
    let off = hdr(&device, &mut renderer, &lab, quality(false));
    let on = hdr(&device, &mut renderer, &lab, quality(true));
    let mut worst = 0.0f32;
    for (a, b) in off.iter().zip(&on) {
        for ch in 0..3 {
            worst = worst.max((a[ch] - b[ch]).abs() / a[ch].max(1e-3));
        }
    }
    eprintln!("uniform plate: worst relative change {worst}");
    assert!(worst < 2e-3, "worst {worst}");

    // The edge's light is only moved.
    let lab = edge_plate(matte(skin()));
    let off = hdr(&device, &mut renderer, &lab, quality(false));
    let on = hdr(&device, &mut renderer, &lab, quality(true));
    let sum = |row: &[[f32; 3]], ch: usize| row.iter().map(|c| f64::from(c[ch])).sum::<f64>();
    for ch in 0..3 {
        let (a, b) = (sum(row(&off, 80), ch), sum(row(&on, 80), ch));
        assert!((a - b).abs() / a < 0.01, "channel {ch}: {a} became {b}");
    }
}

/// The albedo goes on after the blur: texture detail stays sharp under even light.
#[test]
fn the_albedo_stays_sharp() {
    let device = device();
    let mut renderer = FrameRenderer::new();
    let size = 64u32;
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let c = if (x + y) % 2 == 0 { 230 } else { 40 };
            rgba.extend_from_slice(&[c, c, c, 255]);
        }
    }
    let material = LabMaterial {
        albedo: Some(LabTexture {
            width: size,
            height: size,
            rgba,
        }),
        ..matte(skin())
    };
    let lab = plate(vec![material], vec![quad(-EXTENT, EXTENT, 0.0, 1.0, 0)]);
    let off = hdr(&device, &mut renderer, &lab, quality(false));
    let on = hdr(&device, &mut renderer, &lab, quality(true));
    assert!(renderer.subsurface_stats().drawn > 0);
    let mut worst = 0.0f32;
    for (a, b) in off.iter().zip(&on) {
        for ch in 0..3 {
            worst = worst.max((a[ch] - b[ch]).abs() / a[ch].max(1.0));
        }
    }
    eprintln!("textured plate: worst relative change {worst}");
    assert!(worst < 0.01, "worst {worst}");
}

/// A tint of zero in a channel keeps that channel where it was, a whole tint moves it.
#[test]
fn a_black_tint_channel_does_not_scatter() {
    let device = device();
    let mut renderer = FrameRenderer::new();
    let lab = edge_plate(matte(SubsurfaceFactors {
        tint: [1.0, 0.0, 0.5],
        ..skin()
    }));
    let off = hdr(&device, &mut renderer, &lab, quality(false));
    let on = hdr(&device, &mut renderer, &lab, quality(true));
    let (off_row, on_row) = (row(&off, 80), row(&on, 80));
    let mid = WIDTH as usize / 2;
    let moved = |ch: usize| (on_row[mid + 2][ch] - off_row[mid + 2][ch]).abs();
    assert!(moved(0) > 1.0, "red scatters: {}", moved(0));
    assert!(moved(1) < 1e-2, "green stays: {}", moved(1));
    assert!(moved(2) > 0.01, "half the blue scatters: {}", moved(2));
    for x in 0..WIDTH as usize {
        assert!(
            (on_row[x][1] - off_row[x][1]).abs() / off_row[x][1].max(1.0) < 2e-3,
            "x {x}: {} against {}",
            on_row[x][1],
            off_row[x][1]
        );
    }
}

/// A plate nearer the camera lit on all of it does not bleed onto a dark plate behind it, and a dark
/// surface that does not scatter takes nothing from a scattering neighbor.
#[test]
fn light_does_not_cross_silhouettes_or_leave_the_material() {
    let device = device();
    let mut renderer = FrameRenderer::new();
    // In front, on the left: lit. Behind, on the right: dark (normal away), scattering too.
    let depth_step = plate(
        vec![matte(skin())],
        vec![
            quad(-EXTENT, 0.0, 0.03, 1.0, 0),
            quad(-EXTENT, EXTENT, 0.0, -1.0, 0),
        ],
    );
    let on = hdr(&device, &mut renderer, &depth_step, quality(true));
    let scene_row = row(&on, 80);
    let front_lit = scene_row[10][0];
    assert!(front_lit > 1.0, "{front_lit}");
    // The first pixels of the back plate sit a depth step away from the lit front plate.
    let edge_px = ((WIDTH as f32) * 0.5) as usize;
    let behind = scene_row[edge_px + 4][0];
    assert!(
        behind < 0.01 * front_lit,
        "bled over the silhouette: {behind} of {front_lit}"
    );

    // A dark plate without the factors beside a lit one with them.
    let neighbor = plate(
        vec![matte(skin()), matte(SubsurfaceFactors::OFF)],
        vec![quad(-EXTENT, 0.0, 0.0, 1.0, 0), quad(0.0, EXTENT, 0.0, -1.0, 1)],
    );
    let off = hdr(&device, &mut renderer, &neighbor, quality(false));
    let on = hdr(&device, &mut renderer, &neighbor, quality(true));
    let (off_row, on_row) = (row(&off, 80), row(&on, 80));
    for x in WIDTH as usize / 2 + 1..WIDTH as usize {
        assert_eq!(on_row[x], off_row[x], "x {x}");
    }
    // Its light stays on it: what would cross the edge returns to the pixel it left.
    for x in 0..WIDTH as usize / 2 - 1 {
        for ch in 0..3 {
            assert!(
                (on_row[x][ch] - off_row[x][ch]).abs() / off_row[x][ch] < 2e-3,
                "x {x} ch {ch}: {} against {}",
                on_row[x][ch],
                off_row[x][ch]
            );
        }
    }
}

/// A blended surface in front of the skin sees the finished skin: the reason the blur runs ahead of
/// the forward pass (ADR-2049).
#[test]
fn a_blended_surface_in_front_sees_the_scattered_light() {
    let device = device();
    let mut renderer = FrameRenderer::new();
    let pane = LabMaterial {
        base_color: [0.0, 0.0, 0.0, 0.5],
        alpha_mode: ALPHA_BLEND,
        roughness: 1.0,
        ior: 1.001,
        ..LabMaterial::default()
    };
    let bare = edge_plate(matte(skin()));
    let mut covered = edge_plate(matte(skin()));
    covered.materials.push(pane);
    covered.meshes.push(quad(-EXTENT, EXTENT, 0.02, 1.0, 1));
    let bare_on = hdr(&device, &mut renderer, &bare, quality(true));
    let covered_on = hdr(&device, &mut renderer, &covered, quality(true));
    let (bare_row, covered_row) = (row(&bare_on, 80), row(&covered_on, 80));
    // In the dark, where every bit of light is scattered: half of it comes through the pane.
    let mid = WIDTH as usize / 2;
    // Red carries the light this far; had the blur added after the pane, this would read 1.
    for x in [mid + 1, mid + 3, mid + 6, mid - 3] {
        let ratio = covered_row[x][0] / bare_row[x][0];
        assert!((ratio - 0.5).abs() < 0.02, "x {x}: pane passes {ratio}");
    }
}

/// A frame that does not scatter is the frame without the feature: same bytes, nothing from the pool,
/// and the report says why. This is the cost guarantee of the default path.
#[test]
fn a_frame_that_does_not_scatter_is_the_frame_without_the_feature() {
    let device = device();
    let with = edge_plate(matte(skin()));
    let without = edge_plate(matte(SubsurfaceFactors::OFF));
    let render = |lab: &LabScene, quality: FrameQuality| {
        let mut renderer = FrameRenderer::new();
        let scene = lab.as_frame_scene_with_quality(quality);
        let pixels = renderer.render_offscreen(&device, &scene).expect("frame").pixels;
        (
            pixels,
            renderer.subsurface_stats(),
            renderer.graph_report().graph.transients.created,
        )
    };
    let (plain, plain_stats, plain_pool) = render(&without, quality(false));
    assert_eq!(plain_stats.reason, Some("profile_off"));
    // The profile does not ask: same bytes, however the material is set.
    let (asked_not, stats, pool) = render(&with, quality(false));
    assert_eq!(asked_not, plain);
    assert_eq!(pool, plain_pool, "the pool");
    assert_eq!(stats.reason, Some("profile_off"));
    assert_eq!(stats.drawn, 0);
    // The profile asks, no material has it: same bytes.
    let (no_material, stats, pool) = render(&without, quality(true));
    assert_eq!(no_material, plain);
    assert_eq!(pool, plain_pool, "the pool");
    assert_eq!(stats.reason, Some("no_material"));
    // The frame cannot scatter: the material draws without it.
    let mut no_post = edge_plate(matte(skin()));
    no_post.post = None;
    let mut no_post_plain = edge_plate(matte(SubsurfaceFactors::OFF));
    no_post_plain.post = None;
    let (blocked, stats, _) = render(&no_post, quality(true));
    assert_eq!(stats.reason, Some("no_hdr_target"));
    assert_eq!(stats.fallback, 2);
    assert_eq!(blocked, render(&no_post_plain, quality(false)).0);
    let msaa = FrameQuality {
        msaa_samples: 4,
        ..quality(true)
    };
    let (blocked, stats, _) = render(&with, msaa);
    assert_eq!(stats.reason, Some("msaa"));
    assert_eq!(
        blocked,
        render(
            &without,
            FrameQuality {
                msaa_samples: 4,
                ..quality(false)
            }
        )
        .0
    );
}

/// A skin material keeps its pre-integrated look when the frame cannot scatter, and swaps it for the
/// blur when it can (never both, or the same light would scatter twice).
#[test]
fn skin_shading_swaps_its_falloff_for_the_blur() {
    let device = device();
    let lab = edge_plate(matte(skin()));
    let material_key = lab.as_frame_scene().meshes[0].material_key;
    let shading = MaterialShading::skin(SkinShading {
        curvature_radius_mm: 3.0,
        transmission: 0.0,
        ..SkinShading::default()
    });
    let mut renderer = FrameRenderer::new();
    renderer.set_material_shading([(material_key, shading)]);
    let preintegrated = hdr(&device, &mut renderer, &lab, quality(false));
    let scattered = hdr(&device, &mut renderer, &lab, quality(true));
    assert_eq!(renderer.subsurface_stats().drawn, 2);
    let mut plain = FrameRenderer::new();
    let plain_scattered = hdr(&device, &mut plain, &lab, quality(true));
    // Under the blur the skin model's falloff is gone: the same as a standard material's edge, to
    // what the second specular lobe adds at this IOR.
    let (a, b) = (row(&scattered, 80), row(&plain_scattered, 80));
    let mid = WIDTH as usize / 2;
    for x in [mid + 2, mid + 8, mid + 20] {
        for ch in 0..3 {
            assert!(
                (a[x][ch] - b[x][ch]).abs() / b[x][ch].max(1e-2) < 0.01,
                "x {x} ch {ch}: {} against {}",
                a[x][ch],
                b[x][ch]
            );
        }
    }
    // Without it the falloff is what the frame shows: its own values, not the blur's.
    let p = row(&preintegrated, 80);
    assert_ne!(p[mid + 2], a[mid + 2]);
}

/// A material this path cannot scatter draws as before and is counted.
#[test]
fn an_unsupported_material_draws_as_before_and_is_counted() {
    let device = device();
    let lab = edge_plate(LabMaterial {
        alpha_mode: ALPHA_BLEND,
        ..matte(skin())
    });
    let mut plain_lab = lab.clone();
    plain_lab.materials[0].subsurface = SubsurfaceFactors::OFF;
    let mut renderer = FrameRenderer::new();
    let on = hdr(&device, &mut renderer, &lab, quality(true));
    let stats = renderer.subsurface_stats();
    assert_eq!(stats.unsupported, 2);
    assert_eq!(stats.drawn, 0);
    assert_eq!(stats.reason, Some("no_material"));
    let mut plain = FrameRenderer::new();
    assert_eq!(on, hdr(&device, &mut plain, &plain_lab, quality(false)));
}

/// The same frame twice is the same frame, on two renderers.
#[test]
fn the_scattered_frame_is_deterministic() {
    let device = device();
    let lab = edge_plate(matte(skin()));
    let mut a = FrameRenderer::new();
    let mut b = FrameRenderer::new();
    let scene = lab.as_frame_scene_with_quality(quality(true));
    let first = a.render_offscreen(&device, &scene).expect("frame").pixels;
    assert_eq!(first, b.render_offscreen(&device, &scene).expect("frame").pixels);
    assert_eq!(first, a.render_offscreen(&device, &scene).expect("frame").pixels);
}

/// The desktop stacks over a scattering draw, a double-sided one and a non-scattering one beside them:
/// the depth prepass, the GPU-driven batches, the temporal resolve and SSAO all accept the pass.
#[test]
fn it_runs_under_every_desktop_preset_with_the_prepass() {
    let device = device();
    for preset in [
        RenderProfilePreset::DesktopLow,
        RenderProfilePreset::DesktopHigh,
        RenderProfilePreset::DesktopUltra,
    ] {
        let profile = RenderProfile {
            subsurface_scattering: true,
            ..RenderProfile::from_preset(preset)
        };
        let frame_quality = FrameQuality::from_profile(&profile);
        let mut lab = plate(
            vec![
                matte(skin()),
                LabMaterial {
                    double_sided: true,
                    ..matte(SubsurfaceFactors::new(SubsurfaceProfile::Foliage))
                },
                matte(SubsurfaceFactors::OFF),
            ],
            vec![
                quad(-EXTENT, -0.01, 0.0, 1.0, 0),
                quad(-0.01, 0.01, 0.0, 1.0, 1),
                quad(0.01, EXTENT, 0.0, 1.0, 2),
            ],
        );
        lab.depth_prepass = true;
        let mut renderer = FrameRenderer::new();
        let scene = lab.as_frame_scene_with_quality(frame_quality);
        renderer.render_offscreen(&device, &scene).expect("frame");
        let stats = renderer.subsurface_stats();
        assert_eq!(stats.reason, None, "{preset:?}: {stats:?}");
        assert_eq!(stats.drawn, 2, "{preset:?}");
        assert_eq!(stats.fallback, 0, "{preset:?}");
    }
}

/// The blur reads the same surface with or without a depth prepass, and the graph lists the nodes
/// ahead of the forward pass.
#[test]
fn the_prepass_changes_nothing_and_the_graph_lists_the_nodes() {
    let device = device();
    let plain = edge_plate(matte(skin()));
    let mut prepass = edge_plate(matte(skin()));
    prepass.depth_prepass = true;
    let mut renderer = FrameRenderer::new();
    let without = hdr(&device, &mut renderer, &plain, quality(true));
    let with = hdr(&device, &mut renderer, &prepass, quality(true));
    let mut worst = 0.0f32;
    for (a, b) in without.iter().zip(&with) {
        for ch in 0..3 {
            worst = worst.max((a[ch] - b[ch]).abs() / a[ch].max(1.0));
        }
    }
    assert!(worst < 2e-3, "worst {worst}");
    let report = renderer.graph_report();
    let names: Vec<&str> = report
        .steps
        .iter()
        .filter_map(|step| match step {
            regia_render::FrameStep::Builtin { name, .. } => Some(*name),
            regia_render::FrameStep::Slot(_) => None,
        })
        .collect();
    let at = |name: &str| names.iter().position(|n| *n == name).expect(name);
    assert!(at("subsurface_draws") < at("subsurface_blur"));
    assert!(at("subsurface_blur") < at("forward"));
}

/// The `.material.ron` block (v6; the lane wrote it as v5 before ADR-2106 renumbered it behind
/// the weather response) reaches the runtime material, beside a v7 file's transmission roughness;
/// v5 and v4 files have none.
#[test]
fn the_material_asset_carries_the_factors() {
    let foliage = SubsurfaceFactors {
        profile: SubsurfaceProfile::Foliage,
        radius_mm: 4.0,
        tint: [0.2, 0.9, 0.3],
    };
    let v6 = br#"(
        format_version: 6,
        shader: standard,
        base: (),
        subsurface: (profile: foliage, radius: 4.0, tint: (0.2, 0.9, 0.3)),
    )"#;
    let asset = regia_asset::MaterialAssetRon::from_ron(v6).expect("v6");
    assert_eq!(LabMaterial::from_authored(&asset).subsurface, foliage);
    let v7 = br#"(
        format_version: 7,
        shader: standard,
        base: (),
        transmission: (factor: 0.5),
        subsurface: (profile: foliage, radius: 4.0, tint: (0.2, 0.9, 0.3)),
        transmission_roughness: 0.35,
    )"#;
    let asset = regia_asset::MaterialAssetRon::from_ron(v7).expect("v7");
    assert_eq!(asset.transmission_roughness, Some(0.35));
    assert_eq!(LabMaterial::from_authored(&asset).subsurface, foliage);
    for older in [
        &br#"(format_version: 5, shader: standard, base: ())"#[..],
        &br#"(format_version: 4, shader: standard, base: ())"#[..],
    ] {
        let asset = regia_asset::MaterialAssetRon::from_ron(older).expect("older");
        assert_eq!(
            LabMaterial::from_authored(&asset).subsurface,
            SubsurfaceFactors::OFF
        );
    }
}

/// A leaf card: an alpha-tested plate scatters on its covered pixels, its holes stay what is behind
/// them, and no light lands in them.
#[test]
fn an_alpha_tested_leaf_scatters_only_where_it_is_covered() {
    let device = device();
    let size = 8u32;
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    for _y in 0..size {
        for x in 0..size {
            // Covered on the left half of the texture, a hole on the right.
            let alpha = if x < size / 2 { 255 } else { 0 };
            rgba.extend_from_slice(&[200, 220, 120, alpha]);
        }
    }
    let leaf = LabMaterial {
        alpha_mode: regia_render::ALPHA_MASK,
        alpha_cutoff: 0.5,
        albedo: Some(LabTexture {
            width: size,
            height: size,
            rgba,
        }),
        ..matte(SubsurfaceFactors::new(SubsurfaceProfile::Foliage))
    };
    // The plate spans the view exactly in u: its texture's edge falls on the view's center.
    let half_view = (FOV_Y * 0.5).tan() * DISTANCE * WIDTH as f32 / HEIGHT as f32;
    let mut card = quad(-half_view, half_view, 0.0, 1.0, 0);
    for vertex in &mut card.vertices {
        vertex.uv[0] = (vertex.position[0] + half_view) / (2.0 * half_view);
    }
    let lab = plate(vec![leaf], vec![card]);
    let mut renderer = FrameRenderer::new();
    let off = hdr(&device, &mut renderer, &lab, quality(false));
    let on = hdr(&device, &mut renderer, &lab, quality(true));
    assert_eq!(renderer.subsurface_stats().drawn, 1);
    let (off_row, on_row) = (row(&off, 80), row(&on, 80));
    let mid = WIDTH as usize / 2;
    // The covered side is even light: it stays. The holes are the clear color in both frames.
    for x in 2..mid - 2 {
        for ch in 0..3 {
            assert!(
                (on_row[x][ch] - off_row[x][ch]).abs() / off_row[x][ch] < 3e-3,
                "x {x} ch {ch}: {} against {}",
                on_row[x][ch],
                off_row[x][ch]
            );
        }
    }
    for x in mid + 2..WIDTH as usize {
        assert_eq!(on_row[x], off_row[x], "hole at x {x}");
    }
}

/// A skinned draw takes the pass as a rigid one does: same joints, same blur.
#[test]
fn a_skinned_draw_scatters_at_its_posed_position() {
    let device = device();
    // Bound to joint 0, which moves the left half into place from far off to the side.
    let shifted = |mesh: &LabMesh, offset: f32| {
        let mut vertices = mesh.vertices.clone();
        for vertex in &mut vertices {
            vertex.position[0] += offset;
            vertex.joints = [0; 4];
            vertex.weights = [1.0, 0.0, 0.0, 0.0];
        }
        vertices
    };
    let left = quad(-EXTENT, 0.0, 0.0, 1.0, 0);
    let right = quad(0.0, EXTENT, 0.0, -1.0, 0);
    let rigid = plate(vec![matte(skin())], vec![left.clone(), right.clone()]);
    let skinned = plate(
        vec![matte(skin())],
        vec![
            LabMesh {
                vertices: shifted(&left, 5.0),
                joints: vec![regia_render::JointMatrix {
                    m: [[1.0, 0.0, 0.0, -5.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0]],
                }],
                ..left.clone()
            },
            right,
        ],
    );
    let mut renderer = FrameRenderer::new();
    let a = hdr(&device, &mut renderer, &rigid, quality(true));
    let stats = renderer.subsurface_stats();
    assert_eq!((stats.drawn, stats.fallback), (2, 0), "{stats:?}");
    let mut renderer = FrameRenderer::new();
    let b = hdr(&device, &mut renderer, &skinned, quality(true));
    let stats = renderer.subsurface_stats();
    assert_eq!((stats.drawn, stats.fallback), (2, 0), "{stats:?}");
    let (a_row, b_row) = (row(&a, 80), row(&b, 80));
    for x in 0..WIDTH as usize {
        for ch in 0..3 {
            assert!(
                (a_row[x][ch] - b_row[x][ch]).abs() / a_row[x][ch].max(1.0) < 5e-3,
                "x {x} ch {ch}: rigid {} skinned {}",
                a_row[x][ch],
                b_row[x][ch]
            );
        }
    }
}

/// Two scattering materials of very different reach side by side, under even light: nothing moves.
/// The light they trade must not take either one's reach as its weight.
#[test]
fn a_seam_between_two_reaches_keeps_the_light() {
    let device = device();
    let wide = matte(SubsurfaceFactors {
        radius_mm: 8.0,
        ..skin()
    });
    let narrow = matte(SubsurfaceFactors {
        radius_mm: 1.0,
        ..SubsurfaceFactors::new(SubsurfaceProfile::Foliage)
    });
    let lab = plate(
        vec![wide, narrow],
        vec![quad(-EXTENT, 0.0, 0.0, 1.0, 0), quad(0.0, EXTENT, 0.0, 1.0, 1)],
    );
    let mut renderer = FrameRenderer::new();
    let off = hdr(&device, &mut renderer, &lab, quality(false));
    let on = hdr(&device, &mut renderer, &lab, quality(true));
    assert_eq!(renderer.subsurface_stats().drawn, 2);
    let mut worst = 0.0f32;
    for (a, b) in off.iter().zip(&on) {
        for ch in 0..3 {
            worst = worst.max((a[ch] - b[ch]).abs() / a[ch].max(1e-3));
        }
    }
    eprintln!("seam between two reaches: worst relative change {worst}");
    assert!(worst < 2e-3, "worst {worst}");
}

/// A frame whose scattering draws are all out of view says so and draws what a frame without them
/// would.
#[test]
fn a_culled_draw_says_so() {
    let device = device();
    // Small enough for the frame's culling, unlike the plate: its bound does not hold the camera.
    let (vertices, indices) = regia_render::uv_sphere(0.02, 16, 8);
    let mut lab = plate(
        vec![matte(skin())],
        vec![LabMesh {
            vertices,
            indices,
            material: 0,
            ..LabMesh::unskinned()
        }],
    );
    let mut renderer = FrameRenderer::new();
    let _ = hdr(&device, &mut renderer, &lab, quality(true));
    let stats = renderer.subsurface_stats();
    assert_eq!((stats.reason, stats.drawn), (None, 1), "{stats:?}");
    assert!(
        stats.covered_permille > 0 && stats.covered_permille < 1000,
        "{stats:?}"
    );
    // The camera turned away.
    lab.target = [0.0, 0.0, 2.0 * DISTANCE];
    let pixels = {
        let scene = lab.as_frame_scene_with_quality(quality(true));
        renderer.render_offscreen(&device, &scene).expect("frame").pixels
    };
    let stats = renderer.subsurface_stats();
    assert_eq!(stats.reason, Some("no_visible_draw"), "{stats:?}");
    assert_eq!((stats.drawn, stats.covered_permille), (0, 0), "{stats:?}");
    let mut plain = FrameRenderer::new();
    let mut plain_lab = lab.clone();
    plain_lab.materials[0].subsurface = SubsurfaceFactors::OFF;
    let plain_scene = plain_lab.as_frame_scene_with_quality(quality(false));
    assert_eq!(
        pixels,
        plain
            .render_offscreen(&device, &plain_scene)
            .expect("frame")
            .pixels
    );
}
