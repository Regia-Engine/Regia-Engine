// SPDX-License-Identifier: MPL-2.0
//! The thin film in the forward pass (ADR-1977, `regia_render::thin_film`) end to end on the GPU:
//! the `THIN_FILM` variant shades a filmed material, everything else stays pixel for pixel what it
//! was, and the coverage and thickness maps read the channels the extension names.

use regia_render::thin_film::reflectance;
use regia_render::{
    ALPHA_BLEND, DirectionalLightUniform, FrameMesh, FrameQuality, FrameRenderer, FrameScene, LabMaterial,
    LabTexture, RenderDevice, ShadowCast, ThinFilmFactors, forward_permutation_report, uv_sphere,
};
use regia_test_utils::serial_gpu::{SerialGpu, serial_device};

const WIDTH: u32 = 320;
const HEIGHT: u32 = 160;

/// The binary's one shared device, one test at a time: per-test devices on parallel threads hung
/// the NVIDIA driver (RTX 5090 GPU pre-gate, ADR-7483).
fn device() -> SerialGpu {
    serial_device().expect("headless device")
}

const FILM: ThinFilmFactors = ThinFilmFactors {
    factor: 1.0,
    ior: 1.33,
    thickness_min: 100.0,
    thickness_max: 380.0,
};

fn dark_gloss() -> LabMaterial {
    LabMaterial {
        base_color: [0.02, 0.02, 0.03, 1.0],
        roughness: 0.25,
        ..LabMaterial::EMPTY
    }
}

fn filmed() -> LabMaterial {
    LabMaterial {
        thin_film: FILM,
        ..dark_gloss()
    }
}

/// Two spheres side by side, `left` and `right`, under a sun and the sky.
fn scene<'a>(
    verts: &'a [regia_render::LabVertex],
    idx: &'a [u32],
    left: &'a LabMaterial,
    right: &'a LabMaterial,
    quality: FrameQuality,
) -> FrameScene<'a> {
    scene_at(verts, idx, left, right, quality, 1.3)
}

/// [`scene`] with the right sphere at `right_x` (the diagnostics move it by one float step).
fn scene_at<'a>(
    verts: &'a [regia_render::LabVertex],
    idx: &'a [u32],
    left: &'a LabMaterial,
    right: &'a LabMaterial,
    quality: FrameQuality,
    right_x: f32,
) -> FrameScene<'a> {
    let mesh = |x: f32, material: &'a LabMaterial, key: u64| FrameMesh {
        is_static: false,
        mesh_key: 1,
        material_key: 100 + key,
        object_key: 200 + key,
        vertices: verts,
        indices: idx,
        material,
        model: regia_core::glam::Mat4::from_translation(regia_core::glam::Vec3::new(x, 0.0, 0.0))
            .to_cols_array_2d(),
        joints: &[],
        receiver_layer: 0,
        shadow: ShadowCast::All,
        uv1: &[],
        lod_dither: 0.0,
    };
    FrameScene {
        width: WIDTH,
        height: HEIGHT,
        eye: [0.0, 0.0, 6.5],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.62,
        near: 0.1,
        far: 100.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: true,
        exposure_ev: 13.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: None,
        fog: regia_render::FogSettings::default(),
        directional: DirectionalLightUniform::new([-0.35, -0.5, -0.8], [1.0, 0.98, 0.95], 20_000.0),
        local_lights: &[],
        quality,
        meshes: vec![mesh(-1.3, left, 0), mesh(right_x, right, 1)],
    }
}

fn frame(
    device: &RenderDevice,
    renderer: &mut FrameRenderer,
    left: &LabMaterial,
    right: &LabMaterial,
    quality: FrameQuality,
) -> Vec<u8> {
    let (verts, idx) = uv_sphere(1.0, 48, 24);
    renderer
        .render_offscreen(device, &scene(&verts, &idx, left, right, quality))
        .expect("frame")
        .pixels
}

/// Differing pixels in the left and the right half of the frame.
fn differing(a: &[u8], b: &[u8]) -> (usize, usize) {
    let (mut left, mut right) = (0, 0);
    for (i, (pa, pb)) in a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0).enumerate() {
        if pa != pb {
            if (i % WIDTH as usize) < WIDTH as usize / 2 {
                left += 1;
            } else {
                right += 1;
            }
        }
    }
    (left, right)
}

fn map(width: u32, height: u32, texel: [u8; 4]) -> LabTexture {
    LabTexture {
        width,
        height,
        rgba: texel.repeat((width * height) as usize),
    }
}

/// The film moves the filmed sphere's pixels and no one else's, and the forward pass counts it.
#[test]
fn the_film_shades_its_material_and_touches_nothing_else() {
    let device = device();
    let plain = dark_gloss();
    let film = filmed();
    let mut renderer = FrameRenderer::new();
    let before = frame(&device, &mut renderer, &plain, &plain, FrameQuality::LAB);
    let stats = renderer.thin_film_stats();
    assert_eq!((stats.materials, stats.drawn), (0, 0), "{stats:?}");

    let mut renderer = FrameRenderer::new();
    let after = frame(&device, &mut renderer, &film, &plain, FrameQuality::LAB);
    let stats = renderer.thin_film_stats();
    assert_eq!(
        (stats.materials, stats.drawn, stats.fallback, stats.unsupported),
        (1, 1, 0, 0),
        "{stats:?}"
    );
    let (left, right) = differing(&before, &after);
    assert!(left > 1000, "the film changed {left} pixels of its sphere");
    assert_eq!(right, 0, "the plain sphere moved");
}

/// A material whose factor is 0 is the plain material bit for bit, whatever its other fields say.
#[test]
fn a_zero_factor_is_the_plain_material_and_composes_no_variant() {
    let device = device();
    let plain = dark_gloss();
    let off = LabMaterial {
        thin_film: ThinFilmFactors {
            factor: 0.0,
            ior: 2.0,
            thickness_min: 700.0,
            thickness_max: 900.0,
        },
        ..dark_gloss()
    };
    let mut renderer = FrameRenderer::new();
    let a = frame(&device, &mut renderer, &plain, &plain, FrameQuality::LAB);
    let mut renderer = FrameRenderer::new();
    let b = frame(&device, &mut renderer, &off, &off, FrameQuality::LAB);
    assert_eq!(a, b);
    let stats = renderer.thin_film_stats();
    assert_eq!((stats.materials, stats.drawn), (0, 0), "{stats:?}");
}

/// The variant is its own permutation: it is composed for a filmed draw, and it is declared.
#[test]
fn the_variant_is_a_declared_axis() {
    let device = device();
    let film = filmed();
    let mut renderer = FrameRenderer::new();
    frame(&device, &mut renderer, &film, &film, FrameQuality::LAB);
    let report = forward_permutation_report();
    assert!(report.undeclared.is_empty(), "{:?}", report.undeclared);
    assert!(
        report
            .permutations
            .iter()
            .any(|defines| defines.iter().any(|d| d == "THIN_FILM")),
        "{:?}",
        report.permutations
    );
}

/// A coverage map at zero is the plain material; a full map at full thickness is no map at all.
#[test]
fn the_map_reads_coverage_from_red_and_thickness_from_green() {
    let device = device();
    let plain = dark_gloss();
    let render = |left: &LabMaterial| {
        let mut renderer = FrameRenderer::new();
        frame(&device, &mut renderer, left, &plain, FrameQuality::LAB)
    };
    let base = render(&plain);
    let no_map = render(&filmed());
    let with = |texel: [u8; 4]| LabMaterial {
        thin_film_map: Some(map(4, 4, texel)),
        ..filmed()
    };
    // R 0: no coverage, whatever G and the other channels hold.
    assert_eq!(render(&with([0, 255, 255, 255])), base);
    // G 255: the maximum thickness; B and A are not read.
    assert_eq!(render(&with([255, 255, 0, 0])), no_map);
    // G 0: the minimum thickness, another colour.
    let thin = render(&with([255, 0, 0, 255]));
    assert!(differing(&thin, &no_map).0 > 1000);
    assert_eq!(
        thin,
        render(&LabMaterial {
            thin_film: ThinFilmFactors {
                thickness_max: FILM.thickness_min,
                ..FILM
            },
            ..dark_gloss()
        }),
        "a map at 0 draws thickness_min"
    );
}

/// The channels' order on the map reaches the shader through the packer too.
#[test]
fn the_authored_maps_pack_into_the_same_frame() {
    let device = device();
    let plain = dark_gloss();
    let render = |left: &LabMaterial| {
        let mut renderer = FrameRenderer::new();
        frame(&device, &mut renderer, left, &plain, FrameQuality::LAB)
    };
    let packed =
        regia_render::pack_thin_film_map(Some(&map(2, 2, [255, 9, 9, 9])), Some(&map(2, 2, [9, 128, 9, 9])));
    let direct = render(&LabMaterial {
        thin_film_map: Some(map(2, 2, [255, 128, 0, 255])),
        ..filmed()
    });
    assert_eq!(
        render(&LabMaterial {
            thin_film_map: packed,
            ..filmed()
        }),
        direct
    );
}

/// A filmed draw leaves the GPU-driven batches and draws the same pixels the CPU path does.
#[test]
fn the_film_draws_alike_on_the_gpu_driven_path_and_beside_batched_draws() {
    let device = device();
    let plain = dark_gloss();
    let film = filmed();
    let mut renderer = FrameRenderer::new();
    let cpu = frame(&device, &mut renderer, &film, &plain, FrameQuality::LAB);
    let driven = FrameQuality {
        gpu_driven: true,
        ..FrameQuality::LAB
    };
    let mut renderer = FrameRenderer::new();
    let gpu = frame(&device, &mut renderer, &film, &plain, driven);
    assert_eq!(renderer.thin_film_stats().drawn, 1);
    let (left, right) = if device.capabilities.backend == wgpu::Backend::Metal {
        let (verts, idx) = uv_sphere(1.0, 48, 24);
        let mut empty = scene(&verts, &idx, &film, &plain, FrameQuality::LAB);
        empty.meshes.clear();
        let sky = FrameRenderer::new()
            .render_offscreen(&device, &empty)
            .expect("sky frame")
            .pixels;
        differing_off_the_rim(&cpu, &gpu, &sky)
    } else {
        differing(&cpu, &gpu)
    };
    assert!(
        left + right < 16,
        "{left} + {right} pixels differ; {}",
        describe_difference(&cpu, &gpu)
    );
}

/// Metal's tolerance for the comparison above (ADR-3373): the differing pixels of each half, not
/// counting a pixel within [`RIM_PX`] pixels of the silhouette (the sky frame `sky` shows there)
/// or one whose largest channel difference is at most [`RIM_LEVELS`]. wgpu builds Metal
/// libraries with fast math, so the direct and the GPU-driven vertex paths may round the normal
/// differently, and a glossy sphere's grazing rim magnifies that: on the M5 73 of 88 differing
/// pixels lay within 2 pixels of the rim (all 12 above 4 levels, up to 25), the other 15 within 4
/// levels, and a one-float-step move of the sphere on the direct path changed no pixel. A path
/// that shaded the sphere differently would differ across its face and still fails.
fn differing_off_the_rim(a: &[u8], b: &[u8], sky: &[u8]) -> (usize, usize) {
    let (pa, _) = a.as_chunks::<4>();
    let (pb, _) = b.as_chunks::<4>();
    let (ps, _) = sky.as_chunks::<4>();
    let (w, h) = (WIDTH as i64, HEIGHT as i64);
    let near_rim = |x: i64, y: i64| {
        (-RIM_PX..=RIM_PX).any(|dy| {
            (-RIM_PX..=RIM_PX).any(|dx| {
                let (nx, ny) = (x + dx, y + dy);
                (0..w).contains(&nx) && (0..h).contains(&ny) && {
                    let i = (ny * w + nx) as usize;
                    pa[i] == ps[i] || pb[i] == ps[i]
                }
            })
        })
    };
    let (mut left, mut right) = (0, 0);
    for (i, (qa, qb)) in pa.iter().zip(pb).enumerate() {
        let worst = qa.iter().zip(qb).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0);
        let (x, y) = ((i % WIDTH as usize) as i64, (i / WIDTH as usize) as i64);
        if worst <= RIM_LEVELS || near_rim(x, y) {
            continue;
        }
        if x < w / 2 {
            left += 1;
        } else {
            right += 1;
        }
    }
    (left, right)
}

/// Pixels from the silhouette that [`differing_off_the_rim`] does not count (Metal only).
const RIM_PX: i64 = 2;
/// Largest channel difference [`differing_off_the_rim`] does not count (Metal only).
const RIM_LEVELS: u8 = 4;

/// Diagnostics for the GPU-driven comparison on a new backend (ADR-3373): how far the plain sphere's
/// pixels move when the direct path itself moves the sphere by one float step (0.12 µm), against
/// what the GPU-driven path changes. A difference of the same size and spread is the frame's own
/// sensitivity to rounding in the vertex transform: a pixel on one of the sphere's interior triangle
/// edges changes which triangle shades its pixel quad, and so its screen-space derivatives. With
/// `REGIA_EVIDENCE_DIR` set, the frames are written there as PPM images.
/// `cargo test -p regia_render --test thin_film -- --ignored --nocapture gpu_driven_diagnostics`.
#[test]
#[ignore = "diagnostics: prints the GPU-driven and one-float-step differences, asserts nothing"]
fn gpu_driven_diagnostics() {
    let device = device();
    let plain = dark_gloss();
    let film = filmed();
    let (verts, idx) = uv_sphere(1.0, 48, 24);
    let mut direct = FrameRenderer::new();
    let render = |renderer: &mut FrameRenderer, quality: FrameQuality, x: f32| {
        renderer
            .render_offscreen(&device, &scene_at(&verts, &idx, &film, &plain, quality, x))
            .expect("frame")
            .pixels
    };
    let cpu = render(&mut direct, FrameQuality::LAB, 1.3);
    let again = render(&mut direct, FrameQuality::LAB, 1.3);
    let stepped = render(&mut direct, FrameQuality::LAB, 1.3f32.next_up());
    let driven = FrameQuality {
        gpu_driven: true,
        ..FrameQuality::LAB
    };
    let gpu = render(&mut FrameRenderer::new(), driven, 1.3);
    let info = device.adapter.get_info();
    println!("adapter {} ({:?})", info.name, info.backend);
    for (label, other) in [
        ("direct again", &again),
        ("direct, sphere one float step right", &stepped),
        ("GPU-driven", &gpu),
    ] {
        let (left, right) = differing(&cpu, other);
        println!(
            "{label}: {left} + {right} pixels differ; {}",
            describe_difference(&cpu, other)
        );
    }
    if let Ok(dir) = regia_core::brand::env_var("REGIA_EVIDENCE_DIR") {
        let dir = std::path::Path::new(&dir);
        let _ = std::fs::create_dir_all(dir);
        for (name, pixels) in [("direct", &cpu), ("stepped", &stepped), ("gpu_driven", &gpu)] {
            let mut ppm = format!("P6 {WIDTH} {HEIGHT} 255\n").into_bytes();
            for px in pixels.as_chunks::<4>().0 {
                ppm.extend_from_slice(&px[..3]);
            }
            let _ = std::fs::write(dir.join(format!("thin_film_{name}.ppm")), ppm);
        }
    }
}

/// The largest channel difference between `a` and `b`, how many differing pixels touch a pixel
/// of the background (the sky: a silhouette pixel), and the differing pixels' bounding box.
fn describe_difference(a: &[u8], b: &[u8]) -> String {
    let (pa, _) = a.as_chunks::<4>();
    let (pb, _) = b.as_chunks::<4>();
    let w = WIDTH as usize;
    let h = HEIGHT as usize;
    // The sky is where both frames match the frame's corner pixel.
    let sky = |i: usize| pa[i] == pa[0] && pb[i] == pb[0];
    let (mut worst, mut edge, mut count) = (0u8, 0usize, 0usize);
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0, 0);
    for (i, (qa, qb)) in pa.iter().zip(pb).enumerate() {
        if qa == qb {
            continue;
        }
        count += 1;
        let d = qa.iter().zip(qb).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0);
        worst = worst.max(d);
        let (x, y) = (i % w, i / w);
        (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
        let near_sky = [(-1i64, 0i64), (1, 0), (0, -1), (0, 1)].iter().any(|(dx, dy)| {
            let (nx, ny) = (x as i64 + dx, y as i64 + dy);
            nx >= 0 && ny >= 0 && (nx as usize) < w && (ny as usize) < h && sky(ny as usize * w + nx as usize)
        });
        edge += usize::from(near_sky);
    }
    format!(
        "largest channel difference {worst}, {edge} of {count} beside the sky, box x {x0}..={x1} y {y0}..={y1}"
    )
}

/// MSAA and double-sided targets get their own variants.
#[test]
fn msaa_and_double_sided_draws_take_the_film() {
    let device = device();
    let plain = dark_gloss();
    let film = LabMaterial {
        double_sided: true,
        ..filmed()
    };
    let msaa = FrameQuality {
        msaa_samples: 4,
        ..FrameQuality::LAB
    };
    let mut renderer = FrameRenderer::new();
    let a = frame(&device, &mut renderer, &plain, &plain, msaa);
    let b = frame(&device, &mut renderer, &film, &plain, msaa);
    assert_eq!(renderer.thin_film_stats().drawn, 1);
    assert!(differing(&a, &b).0 > 1000);
}

/// A blended film (a soap bubble's alpha) draws through the blend variant.
#[test]
fn a_blended_material_takes_the_blend_variant() {
    let device = device();
    let plain = dark_gloss();
    let blended = |thin_film| LabMaterial {
        alpha_mode: ALPHA_BLEND,
        base_color: [0.02, 0.02, 0.03, 0.5],
        thin_film,
        ..dark_gloss()
    };
    let mut renderer = FrameRenderer::new();
    let a = frame(
        &device,
        &mut renderer,
        &blended(ThinFilmFactors::OFF),
        &plain,
        FrameQuality::LAB,
    );
    let mut renderer = FrameRenderer::new();
    let b = frame(&device, &mut renderer, &blended(FILM), &plain, FrameQuality::LAB);
    assert_eq!(renderer.thin_film_stats().drawn, 1);
    let (left, right) = differing(&a, &b);
    assert!(left > 1000, "{left}");
    assert_eq!(right, 0);
}

/// What the variant cannot shade draws the base Fresnel and is counted.
#[test]
fn a_transmissive_film_is_counted_and_draws_without_the_film() {
    let device = device();
    let plain = dark_gloss();
    let glass = |thin_film| LabMaterial {
        transmission: 0.9,
        thin_film,
        ..dark_gloss()
    };
    let mut renderer = FrameRenderer::new();
    let a = frame(
        &device,
        &mut renderer,
        &glass(ThinFilmFactors::OFF),
        &plain,
        FrameQuality::LAB,
    );
    let mut renderer = FrameRenderer::new();
    let b = frame(&device, &mut renderer, &glass(FILM), &plain, FrameQuality::LAB);
    assert_eq!(a, b);
    let stats = renderer.thin_film_stats();
    assert_eq!((stats.drawn, stats.unsupported), (0, 1), "{stats:?}");
    assert_eq!(
        (stats.transmissive, stats.masked, stats.lightmapped),
        (0, 0, 0),
        "{stats:?}"
    );
}

/// A parameter edit under one material key rewrites the block, with no new binding or pipeline.
#[test]
fn a_parameter_edit_updates_in_place() {
    let device = device();
    let plain = dark_gloss();
    let mut renderer = FrameRenderer::new();
    let thick = filmed();
    let first = frame(&device, &mut renderer, &thick, &plain, FrameQuality::LAB);
    let thin = LabMaterial {
        thin_film: ThinFilmFactors {
            thickness_max: 220.0,
            ..FILM
        },
        ..dark_gloss()
    };
    let second = frame(&device, &mut renderer, &thin, &plain, FrameQuality::LAB);
    assert!(differing(&first, &second).0 > 1000);
    assert_eq!(renderer.thin_film_stats().materials, 1);
    let mut fresh = FrameRenderer::new();
    assert_eq!(
        second,
        frame(&device, &mut fresh, &thin, &plain, FrameQuality::LAB)
    );
}

/// The sphere's centre sees the sun's reflection direction nowhere near: the film's reflected sky
/// there runs with [`reflectance`] at normal incidence, so the warm and the cool thickness differ
/// in the hue the formula says they do.
#[test]
fn the_film_hue_follows_the_formula_at_the_sphere_centre() {
    let device = device();
    let plain = dark_gloss();
    let centre = |pixels: &[u8]| -> [f32; 3] {
        // The left sphere's centre on screen: x = -1.3 at 6.5 from the eye, vertical half-field
        // tan(0.31), aspect 2.
        let ndc = -1.3 / (6.5 * 0.31f32.tan() * 2.0);
        let x = ((0.5 + 0.5 * ndc) * WIDTH as f32) as usize - 2;
        let y = HEIGHT as usize / 2 - 2;
        let mut sum = [0.0f32; 3];
        for dy in 0..4 {
            for dx in 0..4 {
                let i = ((y + dy) * WIDTH as usize + x + dx) * 4;
                for k in 0..3 {
                    sum[k] += f32::from(pixels[i + k]);
                }
            }
        }
        sum
    };
    // Thicknesses whose formula colours lean to opposite ends.
    let pick = |nm: f32| {
        let material = LabMaterial {
            thin_film: ThinFilmFactors {
                thickness_max: nm,
                ..FILM
            },
            ..dark_gloss()
        };
        let mut renderer = FrameRenderer::new();
        centre(&frame(
            &device,
            &mut renderer,
            &material,
            &plain,
            FrameQuality::LAB,
        ))
    };
    let (red_nm, blue_nm) = (
        (100..700)
            .step_by(10)
            .map(|nm| nm as f32)
            .max_by(|a, b| {
                let lean = |nm: f32| {
                    let r = reflectance(1.0, FILM.ior, nm, [0.04; 3]);
                    r[0] / r[2].max(1e-3)
                };
                lean(*a).total_cmp(&lean(*b))
            })
            .expect("range"),
        (100..700)
            .step_by(10)
            .map(|nm| nm as f32)
            .max_by(|a, b| {
                let lean = |nm: f32| {
                    let r = reflectance(1.0, FILM.ior, nm, [0.04; 3]);
                    r[2] / r[0].max(1e-3)
                };
                lean(*a).total_cmp(&lean(*b))
            })
            .expect("range"),
    );
    let warm = pick(red_nm);
    let cool = pick(blue_nm);
    assert!(
        warm[0] / warm[2].max(1.0) > cool[0] / cool[2].max(1.0),
        "{red_nm} nm {warm:?} against {blue_nm} nm {cool:?}"
    );
}

/// On a Mobile lite tier an opaque film draws through the lite shader as its base Fresnel and is
/// counted; a blended film keeps its variant, since the blend pass has no lite path.
#[test]
fn a_lite_tier_draws_an_opaque_film_as_its_base_and_keeps_a_blended_one() {
    use regia_render::forward_lite::ForwardLite;
    let device = device();
    let plain = dark_gloss();
    let lite = |left: &LabMaterial| {
        let mut renderer = FrameRenderer::new();
        renderer.set_forward_lite(Some(ForwardLite::Low));
        let pixels = frame(&device, &mut renderer, left, &plain, FrameQuality::LAB);
        (pixels, renderer.thin_film_stats())
    };
    let (base, _) = lite(&plain);
    let (opaque, stats) = lite(&filmed());
    assert_eq!(base, opaque, "the lite tier drew the opaque film");
    assert_eq!(
        (stats.materials, stats.drawn, stats.unsupported),
        (0, 0, 1),
        "{stats:?}"
    );
    let blended = |thin_film| LabMaterial {
        alpha_mode: ALPHA_BLEND,
        base_color: [0.02, 0.02, 0.03, 0.5],
        thin_film,
        ..dark_gloss()
    };
    let (a, _) = lite(&blended(ThinFilmFactors::OFF));
    let (b, stats) = lite(&blended(FILM));
    assert_eq!(stats.drawn, 1, "{stats:?}");
    let (left, right) = differing(&a, &b);
    assert!(left > 1000, "{left}");
    assert_eq!(right, 0);
}
