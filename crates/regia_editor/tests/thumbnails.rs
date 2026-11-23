// SPDX-License-Identifier: MPL-2.0
//! Batch thumbnails (Tincal engine request F8): N jobs give N pictures of the right sizes, equal
//! jobs give equal pixels, a debug mode changes the picture, and no pipeline is built after
//! the warm-up.

use std::sync::Arc;

use regia_core::math::Vec3;
use regia_ecs::prelude::*;
use regia_editor::mesh_source::{LoadedModel, ModelPart};
use regia_editor::thumbnails::{ThumbJob, ThumbView, ThumbnailRenderer};
use regia_editor::views::ViewKind;
use regia_render::{CpuMesh, DebugViewMode, LabMaterial, MeshRenderer};
use regia_scene::prelude::Transform;

fn device() -> regia_render::RenderDevice {
    match regia_render::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe or use WARP): {err}"),
    }
}

fn cube_mesh() -> Arc<CpuMesh> {
    let (vertices, indices) = regia_render::box_mesh([0.5, 0.5, 0.5]);
    Arc::new(CpuMesh { vertices, indices })
}

fn material() -> Arc<LabMaterial> {
    Arc::new(LabMaterial {
        base_color: [0.7, 0.3, 0.2, 1.0],
        ..LabMaterial::default()
    })
}

fn cube_model() -> Arc<LoadedModel> {
    Arc::new(LoadedModel {
        parts: vec![ModelPart {
            mesh: cube_mesh(),
            material: material(),
            material_name: "cube".into(),
        }],
        collision_parts: Vec::new(),
        aabb_min: Vec3::splat(-0.5),
        aabb_max: Vec3::splat(0.5),
    })
}

fn distinct_colors(image: &egui::ColorImage) -> usize {
    let mut seen: Vec<egui::Color32> = Vec::new();
    for pixel in &image.pixels {
        if !seen.contains(pixel) {
            seen.push(*pixel);
        }
    }
    seen.len()
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn n_jobs_give_n_pictures_of_the_right_sizes_in_order() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let mut renderer = ThumbnailRenderer::new(&device);
    let model = cube_model();
    let sizes = [32, 64, 32, 48, 64];
    let jobs: Vec<ThumbJob> = sizes
        .iter()
        .map(|&s| ThumbJob::model(Arc::clone(&model), s))
        .collect();
    let images = renderer.render(&jobs);
    assert_eq!(images.len(), sizes.len());
    for (image, size) in images.iter().zip(sizes) {
        assert_eq!(image.size, [size as usize; 2]);
        assert!(distinct_colors(image) > 8, "a lit cube on the sky has shading");
    }
    let stats = renderer.stats();
    assert_eq!((stats.jobs, stats.rendered, stats.failed), (5, 5, 0));
    assert_eq!(stats.sizes, 3, "jobs run grouped by size");
}

#[test]
fn identical_jobs_are_bit_identical_even_with_other_jobs_between() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let mut renderer = ThumbnailRenderer::new(&device);
    let model = cube_model();
    let job = ThumbJob::model(Arc::clone(&model), 64).viewed((0.9, 0.2));
    let other = ThumbJob::model(Arc::clone(&model), 96).viewed(ViewKind::Top);
    let images = renderer.render(&[job.clone(), other, job.clone()]);
    assert_eq!(images[0].pixels, images[2].pixels);
    assert_eq!(images[1].size, [96, 96]);
    // A later call with the same job matches too.
    let again = renderer.render(&[job]);
    assert_eq!(images[0].pixels, again[0].pixels);
}

#[test]
fn views_and_debug_modes_change_pixels() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let mut renderer = ThumbnailRenderer::new(&device);
    let model = cube_model();
    let base = ThumbJob::model(Arc::clone(&model), 64);
    let jobs = [
        base.clone(),
        base.clone().debug(DebugViewMode::Wireframe),
        base.clone().viewed(ViewKind::Front),
        base.clone().viewed((2.0, 0.1)),
    ];
    let images = renderer.render(&jobs);
    for other in &images[1..] {
        assert_ne!(images[0].pixels, other.pixels);
    }
    // The wireframe only adds line pixels over the lit picture.
    let changed = images[0]
        .pixels
        .iter()
        .zip(&images[1].pixels)
        .filter(|(lit, wire)| lit != wire)
        .count();
    assert!(changed > 20, "wireframe lines are drawn ({changed} px changed)");
    assert!(
        changed < images[0].pixels.len() / 2,
        "the lit picture stays visible"
    );
}

#[test]
fn no_pipeline_is_built_after_the_warm_up() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let mut renderer = ThumbnailRenderer::new(&device);
    let model = cube_model();
    let lit = ThumbJob::model(Arc::clone(&model), 64);
    let wire = lit.clone().debug(DebugViewMode::Wireframe);
    renderer.render(&[lit.clone(), wire.clone()]);
    assert!(
        renderer.stats().counters.pipelines > 0,
        "the warm-up compiles the pipelines"
    );

    // Another model, other sizes, other views and the wireframe again: all warm.
    let other = Arc::new(LoadedModel {
        parts: vec![ModelPart {
            mesh: cube_mesh(),
            material: material(),
            material_name: "second".into(),
        }],
        collision_parts: Vec::new(),
        aabb_min: Vec3::splat(-0.5),
        aabb_max: Vec3::splat(0.5),
    });
    let jobs = [
        ThumbJob::model(Arc::clone(&other), 32).viewed(ViewKind::Side),
        ThumbJob::model(Arc::clone(&other), 128).debug(DebugViewMode::Wireframe),
        ThumbJob::model(other, 48).viewed((1.2, 0.5)),
        lit,
        wire,
    ];
    renderer.render(&jobs);
    assert_eq!(
        renderer.stats().counters.pipelines,
        0,
        "a warm renderer builds no pipeline for new models, sizes, views or debug modes"
    );
}

#[test]
fn entity_jobs_render_their_subtree_and_need_a_world() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let mut app = regia_app::App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    regia_editor::mesh_source::register_level_types(app.world_mut());
    let world = app.world_mut();
    let mesh = cube_mesh();
    let material = material();
    let parent = world
        .spawn(Transform::from_translation(Vec3::new(5.0, 0.0, 0.0)))
        .id();
    world.spawn((
        Transform::IDENTITY,
        MeshRenderer::new(Arc::clone(&mesh), Arc::clone(&material)),
        ChildOf(parent),
    ));
    let far_cube = world
        .spawn((
            Transform::from_translation(Vec3::new(-40.0, 0.0, 0.0)),
            MeshRenderer::new(mesh, material),
        ))
        .id();

    let mut renderer = ThumbnailRenderer::new(&device);
    let jobs = [
        ThumbJob::entity(parent, 64),
        ThumbJob::entity(far_cube, 64),
        ThumbJob::entity(parent, 64),
    ];
    let images = renderer.render_in(world, &jobs);
    assert_eq!(images.len(), 3);
    assert!(distinct_colors(&images[0]) > 8);
    assert_eq!(images[0].pixels, images[2].pixels);
    // Each subject is framed on its own bounds: the far cube is visible too.
    assert!(distinct_colors(&images[1]) > 8);
    assert_eq!(renderer.stats().failed, 0);

    // Without a world an entity job is a blank picture and counts as failed.
    let blank = renderer.render(&jobs[..1]);
    assert!(blank[0].pixels.iter().all(|p| *p == egui::Color32::TRANSPARENT));
    assert_eq!(renderer.stats().failed, 1);

    // An entity with nothing to draw fails alone.
    let empty = world.spawn(Transform::IDENTITY).id();
    let mixed = renderer.render_in(
        world,
        &[ThumbJob::entity(empty, 32), ThumbJob::entity(parent, 32)],
    );
    assert_eq!(mixed[0].size, [32, 32]);
    assert_eq!(renderer.stats().failed, 1);
    assert_eq!(renderer.stats().rendered, 1);
}

#[test]
fn view_angles() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(ThumbView::Kind(ViewKind::Front).angles(), (0.0, 0.0));
    assert_eq!(ThumbView::from((1.0, 2.0)).angles(), (1.0, 2.0));
    let (yaw, pitch) = ThumbView::from(ViewKind::Perspective).angles();
    assert!(yaw != 0.0 && pitch != 0.0);
}
