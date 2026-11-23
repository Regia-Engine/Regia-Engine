// SPDX-License-Identifier: MIT-0
//! Portable player for XeSS agent captures (ADR-4656).
//!
//! Build with `--features regia/xess`, launch in an isolated working directory, and set
//! `REGIA_PIPELINE_CACHE_DIR` inside that directory. Logs, settings, saves and agent discovery
//! stay local to the run. Use the normal render/seeing/play agent methods; quit with quit.request.

use regia::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_platform::{DirMount, MountPoint, PresentMode, Vfs};
use regia_render::{Camera, CpuMaterial, CpuMesh, DirectionalLight, MeshRenderer};
use regia_scene::{GlobalTransform, Transform};
use std::{process::ExitCode, sync::Arc};

fn main() -> Result<ExitCode, Box<dyn std::error::Error>> {
    let root = std::env::current_dir()?;
    let profiler = regia::frame_profiler();
    let log = regia_core::log::init(regia_core::log::LogConfig {
        file_dir: Some(root.join("logs")),
        profiler: Some(profiler.clone()),
        ..Default::default()
    })?;
    let mut vfs = Vfs::new();
    for (point, name) in [(MountPoint::User, "user"), (MountPoint::Cache, "cache")] {
        vfs.mount(point, DirMount::writable(root.join(name)));
    }
    vfs.mount(MountPoint::Logs, DirMount::read_only(root.join("logs")));
    let mut app = App::new();
    app.insert_resource(regia::Profiler(profiler));
    app.insert_resource(vfs);
    app.insert_resource(regia_framework::settings::SettingsPath(None));
    app.insert_resource(regia_render::RenderSettings::from_profile(
        regia_render::RenderProfilePreset::Deck,
    ));
    app.insert_resource(regia_framework::save_game::SaveDirectory(root.join("saves")));
    app.insert_resource(regia::agent::AgentSettings {
        project_dir: Some(root),
    });
    app.insert_resource(regia_audio::AudioSettings {
        headless: true,
        ..Default::default()
    });
    app.insert_resource(WindowSettings {
        window: Window {
            title: "Regia XeSS capture".into(),
            logical_size: (960.0, 540.0),
            present_mode: PresentMode::Immediate,
            ..Default::default()
        },
    });
    app.add_plugins(regia::DefaultPlugins);
    app.add_systems(Startup, capture_scene);
    let exit = app.run();
    let code = match exit {
        AppExit::Success => 0,
        AppExit::Error(code) => code.get(),
    };
    regia_platform::finish_exit(i32::from(code), || log.flush());
    Ok(ExitCode::from(code))
}

// A static procedural view makes sequential native/XeSS captures comparable. The normal
// sim_bridge sample's authoritative mock advances on snapshot reads even while Time is paused.
fn capture_scene(mut commands: Commands) {
    let (vertices, indices) = sim_bridge::build_heightfield_mesh();
    let ground = Arc::new(CpuMesh { vertices, indices });
    let (vertices, indices) = sim_bridge::build_disc_mesh(0.85, 32);
    let disc = Arc::new(CpuMesh { vertices, indices });
    for (index, color) in [
        [0.65, 0.12, 0.08, 1.0],
        [0.08, 0.55, 0.18, 1.0],
        [0.08, 0.18, 0.7, 1.0],
    ]
    .into_iter()
    .enumerate()
    {
        let transform = Transform {
            translation: Vec3::new(index as f32 * 2.0 - 2.0, 0.8, 0.0),
            ..Default::default()
        };
        commands.spawn((
            transform,
            GlobalTransform(transform.compute_affine()),
            MeshRenderer::new(
                Arc::clone(&disc),
                Arc::new(CpuMaterial {
                    base_color: color,
                    roughness: 0.7,
                    ..Default::default()
                }),
            ),
        ));
    }
    let transform = Transform::default();
    commands.spawn((
        transform,
        GlobalTransform(transform.compute_affine()),
        MeshRenderer::new(
            ground,
            Arc::new(CpuMaterial {
                base_color: [0.4, 0.4, 0.4, 1.0],
                roughness: 0.9,
                ..Default::default()
            }),
        ),
    ));
    let direction = Vec3::new(-0.5, -1.0, -0.5).normalize();
    let transform = Transform {
        rotation: Quat::from_rotation_arc(-Vec3::Z, direction),
        ..Default::default()
    };
    commands.spawn((
        transform,
        GlobalTransform(transform.compute_affine()),
        DirectionalLight {
            direction: direction.to_array(),
            color: [1.0; 3],
            illuminance: 100_000.0,
            cast_shadows: true,
            static_shadow: false,
        },
    ));
    let eye = Vec3::new(4.0, 6.0, 10.0);
    let transform = Transform {
        translation: eye,
        rotation: Quat::from_mat4(
            &regia_core::glam::camera::rh::view::look_at_mat4(eye, Vec3::ZERO, Vec3::Y).inverse(),
        ),
        ..Default::default()
    };
    commands.spawn((
        transform,
        GlobalTransform(transform.compute_affine()),
        Camera::default(),
    ));
}
