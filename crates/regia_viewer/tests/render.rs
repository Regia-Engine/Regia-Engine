// SPDX-License-Identifier: MPL-2.0
//! End-to-end tests of the viewer's forward pass on whatever adapter the environment exposes (the lavapipe
//! driver on the farm). The device runs under WebGL2's limits, so a binding WebGL2 cannot hold fails here.
//!
//! Each test uses a fresh device. No test reads a clock.

#![allow(clippy::unwrap_used)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use glam::{Affine3A, Vec3};
use regia_viewer::bake::{BakeOptions, EnvSource, bake};
use regia_viewer::gpu::{headless_device, sample_count_for};
use regia_viewer::material::ShaderFamily;
use regia_viewer::standin::{ball, stud, stud_visible_bounds};
use regia_viewer::{
    AnodizeFilm, AnodizedParams, BakedEnv, CabochonParams, ContactShadow, FrostedParams, GemParams,
    MetalParams, OpalParams, ShaderHooks, Still, ViewerCamera, ViewerConfig, ViewerError, ViewerMaterial,
    ViewerRenderer, ViewerStyle, capture_still, capture_still_async,
};

const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

/// A device, or `None` with a note on a machine with no adapter at all. Any other failure fails the
/// test, and `REGIA_VIEWER_REQUIRE_ADAPTER=1` (the farm) makes a missing adapter fail too, so a broken
/// driver setup can never pass as a skip.
fn device() -> Option<(wgpu::Adapter, wgpu::Device, wgpu::Queue)> {
    match headless_device() {
        Ok(found) => {
            eprintln!(
                "adapter: {} ({:?})",
                found.0.get_info().name,
                found.0.get_info().backend
            );
            Some(found)
        }
        Err(ViewerError::NoAdapter(e)) => {
            assert!(
                std::env::var_os("REGIA_VIEWER_REQUIRE_ADAPTER").is_none(),
                "REGIA_VIEWER_REQUIRE_ADAPTER is set and there is no adapter: {e}"
            );
            eprintln!("skipped: no adapter: {e}");
            None
        }
        Err(e) => panic!("the adapter refused a device: {e}"),
    }
}

fn renderer_with(hooks: ShaderHooks) -> Option<ViewerRenderer> {
    let (adapter, device, queue) = device()?;
    let mut config = ViewerConfig::new(FORMAT);
    config.sample_count = sample_count_for(&adapter, FORMAT);
    config.hooks = hooks;
    Some(ViewerRenderer::new(device, queue, config).unwrap())
}

fn renderer() -> Option<ViewerRenderer> {
    renderer_with(ShaderHooks::default())
}

fn add_stud(viewer: &mut ViewerRenderer) -> Vec<regia_viewer::MaterialHandle> {
    let mut materials = Vec::new();
    for part in stud() {
        let mesh = viewer.upload_mesh(&part.mesh).unwrap();
        let material = viewer.create_material(&part.material);
        viewer.add_draw(mesh, material, &part.transforms).unwrap();
        materials.push(material);
    }
    viewer
        .set_camera(&ViewerCamera::front(36.0, 100.0, 36.0))
        .unwrap();
    materials
}

/// A fresh directory for a test, under the system temp directory (process id + counter).
fn temp_dir(tag: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("regia-viewer-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Where evidence images go: `$JEWELRY_OUT/evidence/regia/` on the farm, a temp directory elsewhere.
/// Never the repo.
fn evidence_dir() -> PathBuf {
    match std::env::var_os("JEWELRY_OUT") {
        Some(out) => {
            let dir = PathBuf::from(out).join("evidence/regia");
            match std::fs::create_dir_all(&dir) {
                Ok(()) => dir,
                Err(_) => temp_dir("evidence"),
            }
        }
        None => temp_dir("evidence"),
    }
}

fn write_png(path: &std::path::Path, still: &Still) {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, still.width, still.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&still.rgba).unwrap();
        writer.finish().unwrap();
    }
    // Tmp + rename: a parallel reader never sees half a file.
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, &bytes).unwrap();
    std::fs::rename(&tmp, path).unwrap();
}

fn pixel(still: &Still, x: u32, y: u32) -> [u8; 4] {
    let at = ((y * still.width + x) * 4) as usize;
    [
        still.rgba[at],
        still.rgba[at + 1],
        still.rgba[at + 2],
        still.rgba[at + 3],
    ]
}

/// The screen-space position of a world point.
fn project(viewer: &ViewerRenderer, world: Vec3, width: u32, height: u32) -> (u32, u32) {
    let m = viewer.camera().view_proj(width as f32 / height as f32);
    let clip = m * world.extend(1.0);
    let ndc = clip.truncate() / clip.w;
    (
        ((ndc.x * 0.5 + 0.5) * width as f32) as u32,
        ((0.5 - ndc.y * 0.5) * height as f32) as u32,
    )
}

fn coverage(still: &Still, clear: [u8; 4]) -> usize {
    let (pixels, _) = still.rgba.as_chunks::<4>();
    pixels.iter().filter(|p| **p != clear).count()
}

#[test]
fn the_stand_in_piece_renders_with_a_stable_hash_and_is_not_blank() {
    let Some(mut first) = renderer() else { return };
    add_stud(&mut first);
    let a = capture_still(&mut first, 512, 512).unwrap();
    let b = capture_still(&mut first, 512, 512).unwrap();
    assert_eq!(a.hash(), b.hash(), "the same renderer drew two different frames");

    // A new device and renderer from the same inputs.
    let Some(mut second) = renderer() else { return };
    add_stud(&mut second);
    let c = capture_still(&mut second, 512, 512).unwrap();
    assert_eq!(a.hash(), c.hash(), "a second renderer drew a different frame");

    // Not blank, and a lit piece: real contrast inside the silhouette.
    let clear = clear_of(&a);
    let covered = coverage(&a, clear);
    assert!(
        covered > 20_000,
        "only {covered} pixels differ from the clear color"
    );
    let (pixels, _) = a.rgba.as_chunks::<4>();
    let luma: Vec<u32> = pixels
        .iter()
        .filter(|p| **p != clear)
        .map(|p| u32::from(p[0]) + u32::from(p[1]) + u32::from(p[2]))
        .collect();
    let (min, max) = (luma.iter().min().unwrap(), luma.iter().max().unwrap());
    assert!(max - min > 300, "flat image: luma {min}..{max}");

    let path = evidence_dir().join("viewer-standin-512.png");
    write_png(&path, &a);
    println!("standin hash {:016x} {}", a.hash(), path.display());
    if let Some(dir) = std::env::var_os("REGIA_VIEWER_HASH_FILE") {
        std::fs::write(dir, format!("{:016x}\n", a.hash())).unwrap();
    }
}

/// The clear color as the target stored it: the frame's corner, where no test's piece reaches.
fn clear_of(still: &Still) -> [u8; 4] {
    pixel(still, 0, 0)
}

fn every_kind() -> Vec<ViewerMaterial> {
    let metal = |base_color, roughness| {
        ViewerMaterial::Metal(MetalParams {
            base_color,
            roughness,
        })
    };
    vec![
        metal([1.0, 0.77, 0.34], 0.1),
        metal([0.91, 0.92, 0.92], 0.3),
        ViewerMaterial::Anodized(AnodizedParams {
            roughness: 0.2,
            film: AnodizeFilm::Color([0.1, 0.5, 0.7]),
        }),
        ViewerMaterial::Gem(GemParams {
            tint: [0.2, 0.3, 0.9],
            roughness: 0.02,
            ior: 1.77,
            dispersion: 0.2,
            sparkle: 0.5,
            seed: 3.0,
        }),
        ViewerMaterial::Opal(OpalParams {
            body: [0.9, 0.9, 0.85],
            roughness: 0.1,
            fire: 0.8,
            fire_scale_mm: 0.4,
            seed: 1.0,
        }),
        ViewerMaterial::Cabochon(CabochonParams {
            body: [0.2, 0.6, 0.4],
            roughness: 0.05,
            ior: 1.55,
            translucency: 0.4,
        }),
        ViewerMaterial::Frosted(FrostedParams {
            tint: [0.8, 0.9, 1.0],
            roughness: 0.6,
            ior: 1.5,
        }),
    ]
}

#[test]
fn switching_materials_builds_no_pipeline_and_creates_nothing() {
    let Some(mut viewer) = renderer() else { return };
    let materials = add_stud(&mut viewer);
    let stone = materials[3];
    let target_size = 128;
    let lut = viewer.add_lut(&[[0.8, 0.2, 0.1], [0.1, 0.6, 0.9]]).unwrap();

    let before_first_frame = viewer.stats();
    assert_eq!(viewer.pipeline_count(), ShaderFamily::ALL.len() * 2 + 1);
    assert_eq!(
        before_first_frame.pipelines_created,
        viewer.pipeline_count() as u64
    );
    let device = viewer.device().clone();
    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("swap target"),
        size: wgpu::Extent3d {
            width: target_size,
            height: target_size,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    viewer.render(&target).unwrap();
    let settled = viewer.stats();

    let mut kinds = every_kind();
    kinds.push(ViewerMaterial::Anodized(AnodizedParams {
        roughness: 0.2,
        film: AnodizeFilm::Lut(lut),
    }));
    let mut families_seen = std::collections::BTreeSet::new();
    let mut writes = settled.material_writes;
    for round in 0..50 {
        for kind in &kinds {
            viewer.set_material(stone, kind).unwrap();
            viewer.set_material(materials[0], kind).unwrap();
            families_seen.insert(kind.family());
            writes += 2;
            if round < 3 {
                viewer.render(&target).unwrap();
            }
        }
    }
    viewer.render(&target).unwrap();
    device.poll(wgpu::PollType::wait_indefinitely()).unwrap();

    assert_eq!(
        families_seen.len(),
        ShaderFamily::ALL.len(),
        "the test must cross every family"
    );
    let stats = viewer.stats();
    assert_eq!(
        stats.pipelines_created, settled.pipelines_created,
        "a swap built a pipeline"
    );
    assert_eq!(
        stats.buffers_created, settled.buffers_created,
        "a swap created a buffer"
    );
    assert_eq!(
        stats.bind_groups_created, settled.bind_groups_created,
        "a swap created a bind group"
    );
    assert_eq!(stats.material_writes, writes, "a swap is one block write");
    assert_eq!(viewer.pipeline_count(), ShaderFamily::ALL.len() * 2 + 1);
}

#[test]
fn a_material_switch_changes_the_picture_and_coming_back_restores_it() {
    let Some(mut viewer) = renderer() else { return };
    let materials = add_stud(&mut viewer);
    let first = capture_still(&mut viewer, 256, 256).unwrap();
    let gold = stud()[0].material;
    viewer
        .set_material(
            materials[0],
            &ViewerMaterial::Metal(MetalParams {
                base_color: [0.91, 0.92, 0.92],
                roughness: 0.14,
            }),
        )
        .unwrap();
    let second = capture_still(&mut viewer, 256, 256).unwrap();
    assert_ne!(first.hash(), second.hash());
    viewer.set_material(materials[0], &gold).unwrap();
    let third = capture_still(&mut viewer, 256, 256).unwrap();
    assert_eq!(first.hash(), third.hash());
}

#[test]
fn nothing_is_drawn_while_nothing_changes() {
    let Some(mut viewer) = renderer() else { return };
    let materials = add_stud(&mut viewer);
    let device = viewer.device().clone();
    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("lazy target"),
        size: wgpu::Extent3d {
            width: 64,
            height: 64,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    assert!(viewer.needs_redraw());
    assert!(viewer.render_if_needed(&target).unwrap());
    let drawn = viewer.stats().frames_drawn;
    for _ in 0..100 {
        assert!(!viewer.render_if_needed(&target).unwrap());
    }
    assert_eq!(viewer.stats().frames_drawn, drawn, "an idle viewer drew");

    // The same values are no change.
    let camera = *viewer.camera();
    viewer.set_camera(&camera).unwrap();
    let style = *viewer.style();
    viewer.set_style(&style);
    let gold = stud()[0].material;
    viewer.set_material(materials[0], &gold).unwrap();
    assert!(
        !viewer.needs_redraw(),
        "setting the same values asked for a frame"
    );

    // A real change asks for exactly one frame.
    let moved = ViewerCamera {
        eye: [1.0, 0.0, 36.0],
        ..camera
    };
    viewer.set_camera(&moved).unwrap();
    assert!(viewer.needs_redraw());
    assert!(viewer.render_if_needed(&target).unwrap());
    assert!(!viewer.render_if_needed(&target).unwrap());

    let bright = ViewerStyle {
        exposure: 1.5,
        ..style
    };
    viewer.set_style(&bright);
    assert!(viewer.render_if_needed(&target).unwrap());
    assert!(!viewer.render_if_needed(&target).unwrap());

    viewer.request_redraw();
    assert!(viewer.render_if_needed(&target).unwrap());
    assert_eq!(viewer.stats().frames_drawn, drawn + 3);
}

/// Correctness audit (corr-web-viewer): after the target changes size (a resized canvas), the lazy path used to
/// skip the frame because nothing in the scene changed, leaving the new target undrawn.
#[test]
fn corr_a_resized_target_is_drawn_by_the_lazy_path() {
    let Some(mut viewer) = renderer() else { return };
    add_stud(&mut viewer);
    let target = |viewer: &ViewerRenderer, size: u32| {
        viewer.device().create_texture(&wgpu::TextureDescriptor {
            label: Some("lazy target"),
            size: wgpu::Extent3d {
                width: size,
                height: size,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        })
    };
    let small = target(&viewer, 64);
    assert!(viewer.render_if_needed(&small).unwrap());
    assert!(!viewer.render_if_needed(&small).unwrap());
    let large = target(&viewer, 96);
    assert!(
        viewer.render_if_needed(&large).unwrap(),
        "a target of a new size is drawn"
    );
    assert!(!viewer.render_if_needed(&large).unwrap());
}

/// White reflectance under a uniform world must return that world at every roughness (a furnace test): the
/// split-sum table and the energy put back by `env_specular` agree.
#[test]
fn a_white_metal_in_a_uniform_world_is_that_world_at_every_roughness() {
    let Some(mut viewer) = renderer() else { return };
    viewer.set_environment(&BakedEnv::uniform([0.5, 0.5, 0.5]));
    let mesh = viewer.upload_mesh(&ball(5.0, 64)).unwrap();
    let material = viewer.create_material(&ViewerMaterial::default());
    viewer.add_draw(mesh, material, &[Affine3A::IDENTITY]).unwrap();
    viewer
        .set_camera(&ViewerCamera::front(36.0, 100.0, 36.0))
        .unwrap();
    let mut seen = Vec::new();
    for roughness in [0.0, 0.2, 0.5, 0.8, 1.0] {
        viewer
            .set_material(
                material,
                &ViewerMaterial::Metal(MetalParams {
                    base_color: [1.0; 3],
                    roughness,
                }),
            )
            .unwrap();
        let still = capture_still(&mut viewer, 128, 128).unwrap();
        let (x, y) = project(&viewer, Vec3::new(0.0, 0.0, 5.0), 128, 128);
        let center = pixel(&still, x, y);
        // Mid-way toward the rim the table's other column is in use.
        let (x2, y2) = project(&viewer, Vec3::new(3.5, 0.0, 3.5), 128, 128);
        let side = pixel(&still, x2, y2);
        seen.push((roughness, center, side));
    }
    // PBR Neutral keeps a value below its knee, minus the small toe offset; 0.5 stays 0.46 linear = 180.
    for (roughness, center, side) in &seen {
        for value in [center[0], side[0]] {
            assert!(
                (172..=190).contains(&value),
                "roughness {roughness}: center {center:?}, side {side:?}"
            );
        }
    }
}

/// The world is its own direction as a color, so a mirror ball proves the cube's faces and the reflection.
struct DirectionWorld;
impl EnvSource for DirectionWorld {
    fn radiance(&self, d: Vec3) -> Vec3 {
        Vec3::new(d.x.max(0.0), d.y.max(0.0), d.z.max(0.0))
    }
}

#[test]
fn the_environment_cube_is_read_in_the_direction_the_baker_wrote() {
    let Some(mut viewer) = renderer() else { return };
    let env = bake(
        &DirectionWorld,
        BakeOptions {
            face_size: 32,
            mip_count: 3,
            samples: 16,
        },
    )
    .unwrap();
    viewer.set_environment(&env);
    let mesh = viewer.upload_mesh(&ball(5.0, 64)).unwrap();
    let material = viewer.create_material(&ViewerMaterial::Metal(MetalParams {
        base_color: [1.0; 3],
        roughness: 0.0,
    }));
    viewer.add_draw(mesh, material, &[Affine3A::IDENTITY]).unwrap();
    viewer
        .set_camera(&ViewerCamera::front(36.0, 100.0, 36.0))
        .unwrap();
    let still = capture_still(&mut viewer, 256, 256).unwrap();
    let r = 5.0 * std::f32::consts::FRAC_1_SQRT_2;

    // The ball's center faces the eye: it reflects +Z, so blue dominates.
    let (x, y) = project(&viewer, Vec3::new(0.0, 0.0, 5.0), 256, 256);
    let c = pixel(&still, x, y);
    assert!(c[2] > c[0] + 40 && c[2] > c[1] + 40, "center {c:?}");

    // A normal 45 degrees right reflects +X: red dominates.
    let (x, y) = project(&viewer, Vec3::new(r, 0.0, r), 256, 256);
    let c = pixel(&still, x, y);
    assert!(c[0] > c[1] + 40 && c[0] > c[2] + 20, "right {c:?}");

    // A normal 45 degrees up reflects +Y: green dominates, and the image's top is up.
    let (x, y) = project(&viewer, Vec3::new(0.0, r, r), 256, 256);
    assert!(y < 128);
    let c = pixel(&still, x, y);
    assert!(c[1] > c[0] + 40 && c[1] > c[2] + 20, "up {c:?}");

    // A normal 45 degrees left reflects -X: the world is black there.
    let (x, y) = project(&viewer, Vec3::new(-r, 0.0, r), 256, 256);
    let c = pixel(&still, x, y);
    assert!(c[0] < 60, "left {c:?}");
}

/// A hook that shows the shading normal: the pass and the mirrored winding can be read off the picture.
const NORMAL_HOOK: &str =
    "fn shade_surface(s: Surface) -> vec3<f32> { return s.normal * 0.4 + vec3<f32>(0.5); }";

#[test]
fn a_mirrored_part_keeps_its_outside_facing_out() {
    let hooks = ShaderHooks::default().with_hook(ShaderFamily::Metal, NORMAL_HOOK);
    let Some(mut viewer) = renderer_with(hooks) else {
        return;
    };
    let mesh = viewer.upload_mesh(&ball(5.0, 64)).unwrap();
    let material = viewer.create_material(&ViewerMaterial::default());
    let draw = viewer.add_draw(mesh, material, &[Affine3A::IDENTITY]).unwrap();
    viewer
        .set_camera(&ViewerCamera::front(36.0, 100.0, 36.0))
        .unwrap();
    let plain = capture_still(&mut viewer, 128, 128).unwrap();

    viewer
        .set_draw_transforms(draw, &[Affine3A::from_scale(Vec3::new(-1.0, 1.0, 1.0))])
        .unwrap();
    let mirrored = capture_still(&mut viewer, 128, 128).unwrap();
    let stats = viewer.stats();
    assert_eq!(stats.pipelines_created, viewer.pipeline_count() as u64);

    // A sphere mirrored in X is the same sphere: with the winding or normals wrong the inside would show.
    let mut worst = 0;
    for (a, b) in plain.rgba.iter().zip(&mirrored.rgba) {
        worst = worst.max(a.abs_diff(*b));
    }
    assert!(worst <= 24, "worst channel difference {worst}");
    let total: u64 = plain
        .rgba
        .iter()
        .zip(&mirrored.rgba)
        .map(|(a, b)| u64::from(a.abs_diff(*b)))
        .sum();
    let mean = total as f64 / plain.rgba.len() as f64;
    assert!(mean < 1.0, "mean channel difference {mean}");
}

#[test]
fn instances_of_one_draw_share_one_draw_call_and_keep_the_mirrored_ones_apart() {
    let Some(mut viewer) = renderer() else { return };
    let mesh = viewer.upload_mesh(&ball(1.0, 16)).unwrap();
    let material = viewer.create_material(&ViewerMaterial::default());
    let transforms: Vec<Affine3A> = (0..200)
        .map(|i| {
            let x = (i % 20) as f32 - 10.0;
            let y = (i / 20) as f32 - 5.0;
            Affine3A::from_translation(Vec3::new(x, y, 0.0))
        })
        .collect();
    viewer.add_draw(mesh, material, &transforms).unwrap();
    viewer
        .set_camera(&ViewerCamera::front(80.0, 100.0, 36.0))
        .unwrap();
    let before = viewer.stats();
    capture_still(&mut viewer, 64, 64).unwrap();
    let after = viewer.stats();
    assert_eq!(
        after.draw_calls - before.draw_calls,
        1,
        "200 stones are one draw call"
    );
    assert_eq!(after.instances_drawn - before.instances_drawn, 200);
    assert!(
        after.buffers_created > before.buffers_created,
        "the instance buffer grew past its first size"
    );

    // Half of a second draw's instances are mirrored: that draw becomes a plain and a mirrored batch.
    let draw_list: Vec<Affine3A> = transforms
        .iter()
        .map(|t| {
            if t.translation.x < 0.0 {
                *t
            } else {
                *t * Affine3A::from_scale(Vec3::new(-1.0, 1.0, 1.0))
            }
        })
        .collect();
    let mirrored_draw = viewer.add_draw(mesh, material, &draw_list).unwrap();
    let before = viewer.stats();
    capture_still(&mut viewer, 64, 64).unwrap();
    let after = viewer.stats();
    assert_eq!(
        after.draw_calls - before.draw_calls,
        3,
        "first draw, then its plain and mirrored parts"
    );
    viewer.remove_draw(mirrored_draw).unwrap();
    let before = viewer.stats();
    capture_still(&mut viewer, 64, 64).unwrap();
    assert_eq!(viewer.stats().draw_calls - before.draw_calls, 1);
}

#[test]
fn the_contact_shadow_darkens_around_the_piece_and_leaves_the_piece_alone() {
    let Some(mut viewer) = renderer() else { return };
    add_stud(&mut viewer);
    let plain = capture_still(&mut viewer, 256, 256).unwrap();
    let shadow = ContactShadow::under(&stud_visible_bounds(), 0.6);
    viewer.set_style(&ViewerStyle {
        contact_shadow: Some(shadow),
        ..ViewerStyle::default()
    });
    let shaded = capture_still(&mut viewer, 256, 256).unwrap();
    assert_eq!(viewer.stats().pipelines_created, viewer.pipeline_count() as u64);

    let clear = clear_of(&plain);
    let mut darkened = 0;
    let mut changed_on_piece = 0;
    for (a, b) in plain
        .rgba
        .as_chunks::<4>()
        .0
        .iter()
        .zip(shaded.rgba.as_chunks::<4>().0)
    {
        if *a == clear {
            if b[0] < a[0] {
                darkened += 1;
            }
            assert!(b[0] <= a[0], "a shadow never brightens");
        } else if a != b {
            changed_on_piece += 1;
        }
    }
    assert!(darkened > 2000, "only {darkened} pixels darkened");
    // Edge pixels mix piece and background under MSAA; the piece's interior stays exactly the same.
    let covered = coverage(&plain, clear);
    assert!(
        changed_on_piece * 5 < covered,
        "{changed_on_piece} of {covered} piece pixels changed"
    );
    // Far from the piece the frame still shows the clear color.
    assert_eq!(pixel(&shaded, 2, 2), clear);
}

#[test]
fn an_anodize_lookup_follows_the_view_angle() {
    let Some(mut viewer) = renderer() else { return };
    viewer.set_environment(&BakedEnv::uniform([1.0, 1.0, 1.0]));
    // Red when looking along the normal is 0, blue at n dot v = 1: table runs from red (grazing) to blue.
    let lut = viewer.add_lut(&[[0.9, 0.05, 0.05], [0.05, 0.05, 0.9]]).unwrap();
    let mesh = viewer.upload_mesh(&ball(5.0, 64)).unwrap();
    let material = viewer.create_material(&ViewerMaterial::Anodized(AnodizedParams {
        roughness: 0.15,
        film: AnodizeFilm::Lut(lut),
    }));
    viewer.add_draw(mesh, material, &[Affine3A::IDENTITY]).unwrap();
    viewer
        .set_camera(&ViewerCamera::front(36.0, 100.0, 36.0))
        .unwrap();
    let still = capture_still(&mut viewer, 256, 256).unwrap();
    let (x, y) = project(&viewer, Vec3::new(0.0, 0.0, 5.0), 256, 256);
    let center = pixel(&still, x, y);
    assert!(center[2] > center[0] + 60, "center {center:?}");
    let (x, y) = project(&viewer, Vec3::new(4.8, 0.0, 1.4), 256, 256);
    let rim = pixel(&still, x, y);
    assert!(rim[0] > rim[2], "rim {rim:?}");
}

#[test]
fn the_lookup_atlas_refuses_past_its_size_and_bad_tables() {
    let Some(mut viewer) = renderer() else { return };
    assert!(matches!(viewer.add_lut(&[[0.0; 3]]), Err(ViewerError::BadLut(1))));
    for _ in 0..regia_viewer::LUT_ROWS {
        viewer.add_lut(&[[0.0; 3], [1.0; 3]]).unwrap();
    }
    assert!(matches!(
        viewer.add_lut(&[[0.0; 3], [1.0; 3]]),
        Err(ViewerError::LutFull(_))
    ));
}

#[test]
fn a_custom_block_reaches_its_family_hook() {
    // The hook paints the block's `p2`, a field no built-in material fills.
    let hook = "fn shade_surface(s: Surface) -> vec3<f32> { return material.p2.rgb; }";
    let hooks = ShaderHooks::default().with_hook(ShaderFamily::Opal, hook);
    let Some(mut viewer) = renderer_with(hooks) else {
        return;
    };
    let mesh = viewer.upload_mesh(&ball(5.0, 32)).unwrap();
    let block = regia_viewer::MaterialBlock {
        base: [0.0; 4],
        p0: [0.0; 4],
        p1: [0.0; 4],
        p2: [0.1, 0.5, 0.2, 0.0],
    };
    let material = viewer.create_material(&ViewerMaterial::Custom {
        family: ShaderFamily::Opal,
        block,
    });
    viewer.add_draw(mesh, material, &[Affine3A::IDENTITY]).unwrap();
    viewer
        .set_camera(&ViewerCamera::front(36.0, 100.0, 36.0))
        .unwrap();
    let still = capture_still(&mut viewer, 64, 64).unwrap();
    let c = pixel(&still, 32, 32);
    assert!(c[1] > c[2] && c[2] > c[0], "center {c:?}");
}

#[test]
fn the_surface_basis_turns_with_the_instance_and_the_key_lights_are_data() {
    // Paints the instance's +Z axis in the world.
    let hook = "fn shade_surface(s: Surface) -> vec3<f32> { return s.basis[2] * 0.4 + vec3<f32>(0.5); }";
    let hooks = ShaderHooks::default().with_hook(ShaderFamily::Metal, hook);
    let Some(mut viewer) = renderer_with(hooks) else {
        return;
    };
    let mesh = viewer.upload_mesh(&ball(5.0, 32)).unwrap();
    let material = viewer.create_material(&ViewerMaterial::default());
    let draw = viewer.add_draw(mesh, material, &[Affine3A::IDENTITY]).unwrap();
    viewer
        .set_camera(&ViewerCamera::front(36.0, 100.0, 36.0))
        .unwrap();
    let front = pixel(&capture_still(&mut viewer, 64, 64).unwrap(), 32, 32);
    assert!(
        front[2] > front[0] + 40 && front[2] > front[1] + 40,
        "axis +Z: {front:?}"
    );

    // A quarter turn about +Y sends the instance's +Z to world +X, at any scale.
    let turned =
        Affine3A::from_rotation_y(std::f32::consts::FRAC_PI_2) * Affine3A::from_scale(Vec3::splat(0.5));
    viewer.set_draw_transforms(draw, &[turned]).unwrap();
    let side = pixel(&capture_still(&mut viewer, 64, 64).unwrap(), 32, 32);
    assert!(
        side[0] > side[1] + 40 && side[0] > side[2] + 40,
        "axis +X: {side:?}"
    );

    // A key light change asks for a frame and reaches a hook through the frame block.
    let mut lights = regia_viewer::DEFAULT_KEY_LIGHTS;
    lights[0].radiance = [0.0, 3.0, 0.0];
    let style = ViewerStyle {
        key_lights: lights,
        ..ViewerStyle::default()
    };
    let hook = "fn shade_surface(s: Surface) -> vec3<f32> { return frame.key_col[0].rgb * 0.2; }";
    let hooks = ShaderHooks::default().with_hook(ShaderFamily::Metal, hook);
    let Some(mut viewer) = renderer_with(hooks) else {
        return;
    };
    let mesh = viewer.upload_mesh(&ball(5.0, 32)).unwrap();
    let material = viewer.create_material(&ViewerMaterial::default());
    viewer.add_draw(mesh, material, &[Affine3A::IDENTITY]).unwrap();
    viewer
        .set_camera(&ViewerCamera::front(36.0, 100.0, 36.0))
        .unwrap();
    viewer.set_style(&style);
    let c = pixel(&capture_still(&mut viewer, 64, 64).unwrap(), 32, 32);
    assert!(c[1] > c[0] + 40 && c[1] > c[2] + 40, "green key: {c:?}");
}

#[test]
fn a_non_finite_style_is_stored_finite() {
    let Some(mut viewer) = renderer() else { return };
    let mut key_lights = ViewerStyle::default().key_lights;
    key_lights[0].radiance = [f32::INFINITY, 1.0, 1.0];
    let style = ViewerStyle {
        exposure: f32::NAN,
        clear_color: [f32::NAN, 0.5, 0.5, f32::INFINITY],
        contact_shadow: Some(ContactShadow {
            center_mm: [f32::NAN, 0.0, 0.0],
            half_extent_mm: [f32::INFINITY, 2.0],
            softness_mm: f32::NAN,
            opacity: f32::NAN,
        }),
        key_lights,
    };
    viewer.set_style(&style);
    let stored = viewer.style();
    assert_eq!(stored.exposure, 1.0, "non-finite exposure");
    assert!(
        stored.clear_color.iter().all(|c| c.is_finite()),
        "{:?}",
        stored.clear_color
    );
    assert_eq!(stored.clear_color[0], ViewerStyle::default().clear_color[0]);
    let shadow = stored.contact_shadow.expect("shadow kept");
    assert!(shadow.center_mm.iter().all(|c| c.is_finite()), "{shadow:?}");
    assert!(shadow.half_extent_mm.iter().all(|c| c.is_finite()), "{shadow:?}");
    assert!(
        shadow.softness_mm.is_finite() && shadow.opacity.is_finite(),
        "{shadow:?}"
    );
    assert!(
        stored.key_lights[0].radiance.iter().all(|c| c.is_finite()),
        "{:?}",
        stored.key_lights[0]
    );
    assert_eq!(stored.key_lights[0].radiance[0], 0.0);
}

#[test]
fn a_bad_look_texture_is_an_error() {
    let Some(mut viewer) = renderer() else { return };
    let overflow = viewer.set_look_texture(0, u32::MAX, u32::MAX, &[]);
    assert!(matches!(overflow, Err(ViewerError::Config(_))), "{overflow:?}");
    let limit = viewer
        .device()
        .limits()
        .max_texture_dimension_2d
        .saturating_add(1);
    let rgba = vec![255u8; limit as usize * 4];
    let oversized = viewer.set_look_texture(1, limit, 1, &rgba);
    assert!(matches!(oversized, Err(ViewerError::Config(_))), "{oversized:?}");
}

#[test]
fn a_still_does_not_use_up_the_hosts_lazy_frame() {
    let Some(mut viewer) = renderer() else { return };
    add_stud(&mut viewer);
    assert!(viewer.needs_redraw());
    capture_still(&mut viewer, 16, 16).unwrap();
    assert!(viewer.needs_redraw(), "the window still has to show the change");
}

#[test]
fn every_family_draws_without_a_device_error() {
    let Some(mut viewer) = renderer() else { return };
    let mesh = viewer
        .upload_mesh(&regia_viewer::standin::round_cut(8.0, 16))
        .unwrap();
    let material = viewer.create_material(&ViewerMaterial::default());
    viewer.add_draw(mesh, material, &[Affine3A::IDENTITY]).unwrap();
    viewer
        .set_camera(&ViewerCamera::front(36.0, 100.0, 36.0))
        .unwrap();
    let mut hashes = std::collections::BTreeSet::new();
    let kinds = every_kind();
    for kind in &kinds {
        viewer.set_material(material, kind).unwrap();
        let still = capture_still(&mut viewer, 128, 128).unwrap();
        assert!(coverage(&still, clear_of(&still)) > 500, "{kind:?} drew nothing");
        hashes.insert(still.hash());
    }
    assert_eq!(hashes.len(), kinds.len(), "each material looks different");
}

#[test]
fn a_target_without_hardware_srgb_gets_the_same_picture() {
    let Some((_, device, queue)) = device() else {
        return;
    };
    let mut plain = ViewerConfig::new(wgpu::TextureFormat::Rgba8Unorm);
    plain.sample_count = 1;
    let mut srgb = ViewerConfig::new(FORMAT);
    srgb.sample_count = 1;
    let mut a = ViewerRenderer::new(device.clone(), queue.clone(), plain).unwrap();
    let mut b = ViewerRenderer::new(device, queue, srgb).unwrap();
    add_stud(&mut a);
    add_stud(&mut b);
    let sa = capture_still(&mut a, 128, 128).unwrap();
    let sb = capture_still(&mut b, 128, 128).unwrap();
    for (x, y) in sa.rgba.iter().zip(&sb.rgba) {
        assert!(x.abs_diff(*y) <= 2, "{x} vs {y}");
    }
}

/// Correctness audit (corr-web-viewer): a browser's WebGPU canvas offers only non-sRGB formats, so the web player
/// draws in encode mode there. The contact shadow's blend used to scale the encoded backdrop, a much darker shadow
/// than an sRGB target (the stills, WebGL2, native) shows.
#[test]
fn corr_the_contact_shadow_matches_on_a_target_without_hardware_srgb() {
    let Some((_, device, queue)) = device() else {
        return;
    };
    let mut plain = ViewerConfig::new(wgpu::TextureFormat::Rgba8Unorm);
    plain.sample_count = 1;
    let mut srgb = ViewerConfig::new(FORMAT);
    srgb.sample_count = 1;
    let mut a = ViewerRenderer::new(device.clone(), queue.clone(), plain).unwrap();
    let mut b = ViewerRenderer::new(device, queue, srgb).unwrap();
    for viewer in [&mut a, &mut b] {
        add_stud(viewer);
        let style = ViewerStyle {
            contact_shadow: Some(ContactShadow::under(&stud_visible_bounds(), 0.3)),
            ..*viewer.style()
        };
        viewer.set_style(&style);
    }
    let sa = capture_still(&mut a, 128, 128).unwrap();
    let sb = capture_still(&mut b, 128, 128).unwrap();
    let clear = clear_of(&sb);
    let shadowed = sb
        .rgba
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|p| u16::from(p[0]) + 8 < u16::from(clear[0]))
        .count();
    assert!(shadowed > 0, "the shadow darkens part of the backdrop");
    let worst = sa
        .rgba
        .iter()
        .zip(&sb.rgba)
        .map(|(x, y)| x.abs_diff(*y))
        .max()
        .unwrap_or(0);
    assert!(worst <= 3, "encode mode differs by {worst} from the sRGB target");
}

#[test]
fn a_bgra_target_reads_back_as_rgba() {
    let Some((_, device, queue)) = device() else {
        return;
    };
    let mut a = ViewerConfig::new(wgpu::TextureFormat::Bgra8UnormSrgb);
    a.sample_count = 1;
    let mut b = ViewerConfig::new(FORMAT);
    b.sample_count = 1;
    let mut bgra = ViewerRenderer::new(device.clone(), queue.clone(), a).unwrap();
    let mut rgba = ViewerRenderer::new(device, queue, b).unwrap();
    add_stud(&mut bgra);
    add_stud(&mut rgba);
    let x = capture_still(&mut bgra, 64, 64).unwrap();
    let y = capture_still(&mut rgba, 64, 64).unwrap();
    for (a, b) in x.rgba.iter().zip(&y.rgba) {
        assert!(a.abs_diff(*b) <= 1);
    }
}

#[test]
fn the_async_capture_gives_the_same_still() {
    let Some(mut viewer) = renderer() else { return };
    add_stud(&mut viewer);
    let blocking = capture_still(&mut viewer, 64, 64).unwrap();
    let device = viewer.device().clone();
    let mut future = std::pin::pin!(capture_still_async(&mut viewer, 64, 64));
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    let still = loop {
        if let std::task::Poll::Ready(result) = future.as_mut().poll(&mut context) {
            break result.unwrap();
        }
        device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
    };
    assert_eq!(still.hash(), blocking.hash());
}

#[test]
fn a_part_swap_a_resize_and_a_new_environment_build_no_pipeline() {
    let Some(mut viewer) = renderer() else { return };
    let small = viewer.upload_mesh(&ball(2.0, 16)).unwrap();
    let large = viewer.upload_mesh(&ball(6.0, 32)).unwrap();
    let material = viewer.create_material(&ViewerMaterial::default());
    let draw = viewer.add_draw(small, material, &[Affine3A::IDENTITY]).unwrap();
    viewer
        .set_camera(&ViewerCamera::front(36.0, 100.0, 36.0))
        .unwrap();
    let a = capture_still(&mut viewer, 128, 128).unwrap();
    viewer.set_draw_mesh(draw, large).unwrap();
    let b = capture_still(&mut viewer, 128, 128).unwrap();
    let clear = clear_of(&a);
    assert!(coverage(&b, clear) > coverage(&a, clear));
    let other_size = capture_still(&mut viewer, 200, 100).unwrap();
    assert_eq!((other_size.width, other_size.height), (200, 100));
    viewer.set_environment(&BakedEnv::uniform([0.2, 0.4, 0.8]));
    let c = capture_still(&mut viewer, 128, 128).unwrap();
    assert_ne!(b.hash(), c.hash());
    assert_eq!(viewer.stats().pipelines_created, viewer.pipeline_count() as u64);
}

#[test]
fn a_still_past_the_device_limit_is_refused_without_a_panic() {
    let Some(mut viewer) = renderer() else { return };
    let too_big = viewer.device().limits().max_texture_dimension_2d + 1;
    for (width, height) in [(too_big, 8), (8, too_big)] {
        assert!(
            matches!(
                capture_still(&mut viewer, width, height),
                Err(ViewerError::Still(_))
            ),
            "{width}x{height}"
        );
    }
    // The refusal left the renderer usable.
    assert!(capture_still(&mut viewer, 8, 8).is_ok());
}

#[test]
fn stale_handles_and_a_wrong_target_are_refused_without_a_panic() {
    let Some(mut viewer) = renderer() else { return };
    let mesh = viewer.upload_mesh(&ball(2.0, 16)).unwrap();
    let material = viewer.create_material(&ViewerMaterial::default());
    let draw = viewer.add_draw(mesh, material, &[Affine3A::IDENTITY]).unwrap();

    assert!(matches!(viewer.remove_mesh(mesh), Err(ViewerError::InUse(_))));
    assert!(matches!(
        viewer.remove_material(material),
        Err(ViewerError::InUse(_))
    ));
    viewer.remove_draw(draw).unwrap();
    assert!(matches!(
        viewer.remove_draw(draw),
        Err(ViewerError::StaleHandle("draw"))
    ));
    viewer.remove_mesh(mesh).unwrap();
    assert!(matches!(
        viewer.remove_mesh(mesh),
        Err(ViewerError::StaleHandle("mesh"))
    ));
    assert!(matches!(
        viewer.add_draw(mesh, material, &[Affine3A::IDENTITY]),
        Err(ViewerError::StaleHandle("mesh"))
    ));

    // A reused slot does not answer to the old handle.
    let new_mesh = viewer.upload_mesh(&ball(3.0, 16)).unwrap();
    assert!(viewer.remove_mesh(mesh).is_err());
    assert!(matches!(
        viewer.add_draw(new_mesh, material, &[]),
        Err(ViewerError::Config(_))
    ));
    let not_finite = Affine3A::from_translation(Vec3::new(f32::NAN, 0.0, 0.0));
    assert!(matches!(
        viewer.add_draw(new_mesh, material, &[not_finite]),
        Err(ViewerError::Config(_))
    ));

    let camera = *viewer.camera();
    let bad = ViewerCamera {
        eye: camera.target,
        ..camera
    };
    assert!(matches!(viewer.set_camera(&bad), Err(ViewerError::BadCamera(_))));
    let nan = ViewerCamera {
        fov_deg: f32::NAN,
        ..camera
    };
    assert!(matches!(viewer.set_camera(&nan), Err(ViewerError::BadCamera(_))));
    let parallel = ViewerCamera {
        up: [0.0, 0.0, 1.0],
        ..camera
    };
    assert!(matches!(
        viewer.set_camera(&parallel),
        Err(ViewerError::BadCamera(_))
    ));

    assert!(viewer.set_look_texture(2, 1, 1, &[0; 4]).is_err());
    assert!(viewer.set_look_texture(0, 2, 2, &[0; 4]).is_err());
    viewer.set_look_texture(0, 1, 1, &[10, 20, 30, 255]).unwrap();

    let device = viewer.device().clone();
    let wrong = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("wrong"),
        size: wgpu::Extent3d {
            width: 4,
            height: 4,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Bgra8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    assert!(matches!(
        viewer.render(&wrong),
        Err(ViewerError::TargetFormat { .. })
    ));
    assert!(capture_still(&mut viewer, 0, 4).is_err());
    assert!(
        ViewerRenderer::new(
            device,
            viewer.queue().clone(),
            ViewerConfig {
                sample_count: 2,
                ..ViewerConfig::new(FORMAT)
            }
        )
        .is_err()
    );
}

#[test]
fn the_device_really_runs_under_webgl2_limits() {
    let Some((_, device, _)) = device() else { return };
    let limits = device.limits();
    assert_eq!(limits.max_storage_buffers_per_shader_stage, 0);
    assert_eq!(limits.max_storage_textures_per_shader_stage, 0);
    assert_eq!(limits.max_compute_invocations_per_workgroup, 0);
    assert!(limits.max_uniform_buffer_binding_size <= 16 << 10);
    assert!(limits.max_bind_groups <= 4);
}

/// The stand-in tilted to the edge of the default view with the contact shadow, for a person to look at.
#[test]
fn writes_a_tilted_still_with_the_shadow_for_review() {
    let Some(mut viewer) = renderer() else { return };
    add_stud(&mut viewer);
    let base = *viewer.camera();
    let tilt = 30f32.to_radians();
    let eye = Vec3::new(0.0, tilt.sin(), tilt.cos()) * 36.0;
    viewer
        .set_camera(&ViewerCamera {
            eye: eye.to_array(),
            ..base
        })
        .unwrap();
    viewer.set_style(&ViewerStyle {
        contact_shadow: Some(ContactShadow::under(&stud_visible_bounds(), 0.45)),
        ..ViewerStyle::default()
    });
    let still = capture_still(&mut viewer, 512, 512).unwrap();
    let path = evidence_dir().join("viewer-standin-tilt-shadow.png");
    write_png(&path, &still);
    println!("tilted {}", path.display());
}

/// A still of every material on a faceted stone and on a ball, for a person to look at. Not a golden.
#[test]
fn writes_a_contact_sheet_for_review() {
    let Some(mut viewer) = renderer() else { return };
    let cut = viewer
        .upload_mesh(&regia_viewer::standin::round_cut(9.0, 16))
        .unwrap();
    let round = viewer.upload_mesh(&ball(4.5, 64)).unwrap();
    let material = viewer.create_material(&ViewerMaterial::default());
    let draw = viewer.add_draw(cut, material, &[Affine3A::IDENTITY]).unwrap();
    viewer
        .set_camera(&ViewerCamera::front(36.0, 100.0, 36.0))
        .unwrap();
    let kinds = every_kind();
    let tile = 192u32;
    let columns = kinds.len() as u32;
    let mut sheet = vec![255u8; (tile * columns * tile * 2 * 4) as usize];
    for (row, mesh) in [cut, round].into_iter().enumerate() {
        viewer.set_draw_mesh(draw, mesh).unwrap();
        for (column, kind) in kinds.iter().enumerate() {
            viewer.set_material(material, kind).unwrap();
            let still = capture_still(&mut viewer, tile, tile).unwrap();
            for y in 0..tile as usize {
                let from = y * tile as usize * 4;
                let to = ((row * tile as usize + y) * (tile * columns) as usize + column * tile as usize) * 4;
                sheet[to..to + tile as usize * 4]
                    .copy_from_slice(&still.rgba[from..from + tile as usize * 4]);
            }
        }
    }
    let still = Still {
        width: tile * columns,
        height: tile * 2,
        rgba: sheet,
    };
    let path = evidence_dir().join("viewer-materials-sheet.png");
    write_png(&path, &still);
    println!("sheet {}", path.display());
}
