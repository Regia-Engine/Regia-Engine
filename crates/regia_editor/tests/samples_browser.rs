// SPDX-License-Identifier: MPL-2.0
//! The samples browser's integration checks (`book/agents/samples.md`) and the project thumbnail
//! the editor saves when a project closes (`recent_meta::thumbnail_source`).

use regia_ecs::prelude::*;
use regia_editor::recent_meta;
use regia_editor::samples_browser::{self, Roots};
use regia_editor::shell::EditorShell;

/// Serialises device creation and teardown across this binary's test threads (concurrent Vulkan
/// device churn can live-lock some drivers; ADR-1438).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn project(label: &str) -> (std::path::PathBuf, World) {
    let parent = regia_test_utils::unique_temp_dir(label);
    let mut world = World::new();
    let (created, _) = samples_browser::create_and_open(
        &mut world,
        &Roots::default(),
        "template/blank",
        &parent,
        Some("thumb_game"),
        true,
    )
    .expect("the template is copied and opened");
    (created.root, world)
}

#[test]
fn closing_a_project_writes_its_thumbnail() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = match regia_render::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe or use WARP): {err}"),
    };
    let (root, mut world) = project("thumb-close");
    let root = root.canonicalize().expect("root");
    assert_eq!(
        recent_meta::thumbnail_source(&root),
        None,
        "no picture before the close"
    );
    let mut shell = EditorShell::new(Some(device), Some(root.clone()));
    // The exit every close path ends in (window close, Save and exit, quit.request).
    shell.exit_cleanup_with(&mut world);
    let file = recent_meta::thumbnail_source(&root).expect("the close saved the project's picture");
    assert!(
        file.ends_with(recent_meta::VIEWPORT_THUMBNAIL),
        "{}",
        file.display()
    );
    let picture = image::open(&file).expect("a PNG").to_rgba8();
    assert!(picture.width() <= recent_meta::THUMBNAIL_PX && picture.height() <= recent_meta::THUMBNAIL_PX);
    assert!(
        picture.width() > 16 && picture.height() > 16,
        "{}x{}",
        picture.width(),
        picture.height()
    );
    assert!(
        picture
            .pixels()
            .any(|p| p.0[..3] != picture.get_pixel(0, 0).0[..3]),
        "a picture of a scene, not one flat color"
    );
}

#[test]
fn a_close_without_a_render_device_saves_no_thumbnail_and_does_not_fail() {
    let (root, mut world) = project("thumb-nodevice");
    let root = root.canonicalize().expect("root");
    let mut shell = EditorShell::new(None, Some(root.clone()));
    assert!(shell.save_project_thumbnail(&mut world).is_err());
    shell.exit_cleanup_with(&mut world);
    assert_eq!(recent_meta::thumbnail_source(&root), None);
}
