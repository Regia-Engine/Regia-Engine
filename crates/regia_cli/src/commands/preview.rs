// SPDX-License-Identifier: MPL-2.0
//! `regia preview <asset>` — draw a cooked asset in a window through the
//! ECS runtime path (mesh + camera + light entities).

use std::path::PathBuf;
use std::sync::Arc;

use clap::Args;
use regia::prelude::*;
use regia_asset::{AssetPlugin, AssetServer, AssetServerSettings};
use regia_asset_pipeline::{CookOptions, cook_project, host_platform};
use regia_render::{Camera, CpuMaterial, CpuMesh, DirectionalLight, LabVertex, MeshRenderer};
use regia_scene::{GlobalTransform, Transform};

/// Preview a cooked asset path (project-relative under `assets/`).
#[derive(Debug, Args)]
pub struct PreviewArgs {
    /// Project root.
    #[arg(long, default_value = ".")]
    pub project: PathBuf,
    /// Asset path (e.g. `models/DamagedHelmet.glb` or `assets/models/...`).
    pub asset: String,
    /// Frames before exit (0 = run until closed).
    #[arg(long, default_value_t = 0)]
    pub exit_after_frames: u64,
}

struct PreviewApp {
    project: PathBuf,
    asset_path: String,
    exit_after_frames: u64,
}

impl Plugin for PreviewApp {
    fn build(&self, app: &mut App) {
        app.add_plugins(AssetPlugin {
            settings: AssetServerSettings {
                project_root: self.project.clone(),
                platform: host_platform().to_string(),
                content_dir: None,
            },
        })
        .add_plugins(regia_scene::ScenePlugin)
        .insert_resource(PreviewState {
            asset_path: self.asset_path.clone(),
            exit_after_frames: self.exit_after_frames,
            ready: false,
        })
        .add_systems(Update, preview_frame);
    }
}

/// Confine the asset path to the project, then cook (incremental) and show the asset.
///
/// The asset argument is checked with [`regia_core::project_path`] before anything is cooked: a
/// path with `..`, a leading slash or backslash, a `:`, a Windows device segment, a symlink that
/// leaves the project, or a file that is not on disk under `assets/` returns an error and no cook,
/// window or device is started (ADR-4390).
pub fn run(args: &PreviewArgs) -> anyhow::Result<()> {
    let asset_path = validated_asset_arg(&args.project, &args.asset)?;
    cook_project(CookOptions::for_host(args.project.clone()))?;

    let exit = regia::run_with(
        PreviewApp {
            project: args.project.clone(),
            asset_path,
            exit_after_frames: args.exit_after_frames,
        },
        |app| {
            app.insert_resource(WindowSettings {
                window: Window {
                    title: format!("Regia Engine preview — {}", args.asset),
                    ..Default::default()
                },
            });
        },
    );
    match exit {
        AppExit::Success => Ok(()),
        AppExit::Error(code) => anyhow::bail!("preview exited with code {}", code.get()),
    }
}

/// Normalise CLI asset path to manifest form `assets/...` (one `/` between segments: the
/// manifest's path lookup is string-exact, so `assets//x.png` or `assets/./x.png` would miss
/// and poison its negative cache).
fn normalize_asset_arg(asset: &str) -> String {
    let s = asset.replace('\\', "/");
    let body = s.strip_prefix("assets/").unwrap_or(&s);
    let joined = body
        .split('/')
        .filter(|seg| !seg.is_empty() && *seg != ".")
        .collect::<Vec<_>>()
        .join("/");
    format!("assets/{joined}")
}

fn validated_asset_arg(project: &std::path::Path, asset: &str) -> anyhow::Result<String> {
    let normalized = asset.replace('\\', "/");
    regia_core::project_path::check_relative(&normalized)?;
    let asset_path = normalize_asset_arg(asset);
    regia_core::project_path::resolve_read(project, &asset_path)?;
    Ok(asset_path)
}

#[derive(Resource)]
struct PreviewState {
    asset_path: String,
    exit_after_frames: u64,
    ready: bool,
}

/// Loads the cooked mesh, then spawns the preview entities once: the mesh at
/// the origin, an EV-pinned camera at the classic preview position, and a
/// 100 klx key light (the runtime path renders them every frame).
fn preview_frame(
    mut state: ResMut<PreviewState>,
    mut server: ResMut<AssetServer>,
    mut commands: Commands,
    time: Res<Time>,
    mut app_exit: MessageWriter<AppExit>,
) {
    if !state.ready {
        let mesh = server
            .load_mesh_path(&state.asset_path)
            .ok()
            .and_then(|handle| server.meshes().get(&handle).cloned());
        if let Some(mesh) = mesh {
            state.ready = true;
            spawn_preview_entities(&mut commands, mesh);
        }
    }
    if state.exit_after_frames > 0 && time.frame_count() >= state.exit_after_frames {
        app_exit.write(AppExit::Success);
    }
}

/// Spawns the preview stage: mesh, camera (EV-pinned so screenshots are
/// deterministic) and key light, in world poses.
fn spawn_preview_entities(commands: &mut Commands, mesh: regia_asset::CpuMesh) {
    use regia_core::math::{Quat, Vec3};
    let cpu = Arc::new(CpuMesh {
        vertices: mesh
            .positions
            .iter()
            .zip(mesh.normals.iter())
            .zip(mesh.uvs.iter())
            .enumerate()
            .map(|(index, ((position, normal), uv))| LabVertex {
                position: *position,
                normal: *normal,
                uv: *uv,
                tangent: mesh.tangents.get(index).copied().unwrap_or([1.0, 0.0, 0.0, 1.0]),
                color: [1.0; 4],
                joints: [0; 4],
                weights: [0.0; 4],
            })
            .collect(),
        indices: mesh.indices.clone(),
    });
    let transform = Transform::default();
    commands.spawn((
        regia_scene::components::Name::new("preview_mesh"),
        transform,
        GlobalTransform(transform.compute_affine()),
        MeshRenderer::new(cpu, Arc::new(CpuMaterial::default())),
    ));
    let eye = Vec3::new(0.0, 0.2, 2.5);
    let camera_transform = Transform {
        translation: eye,
        rotation: Quat::from_rotation_arc(-Vec3::Z, -eye.normalize_or_zero()),
        scale: Vec3::ONE,
    };
    commands.spawn((
        regia_scene::components::Name::new("preview_camera"),
        camera_transform,
        GlobalTransform(camera_transform.compute_affine()),
        Camera {
            exposure_ev: Some(14.5),
            ..Camera::default()
        },
    ));
    let dir = Vec3::new(-0.577, -0.577, -0.577).normalize_or_zero();
    let light_transform = Transform {
        translation: Vec3::ZERO,
        rotation: Quat::from_rotation_arc(-Vec3::Z, dir),
        scale: Vec3::ONE,
    };
    commands.spawn((
        regia_scene::components::Name::new("preview_light"),
        light_transform,
        GlobalTransform(light_transform.compute_affine()),
        DirectionalLight {
            direction: [-0.577, -0.577, -0.577],
            color: [1.0, 1.0, 1.0],
            illuminance: 100_000.0,
            cast_shadows: false,
            static_shadow: false,
        },
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// corr-low cli (unverified): `assets//x.png` passed unnormalized and the manifest's
    /// string-exact path lookup missed (and cached the miss).
    #[test]
    fn the_asset_arg_is_normalized_before_the_manifest_lookup() {
        let dir = regia_test_utils::unique_temp_dir("cli-preview-arg");
        std::fs::create_dir_all(dir.join("assets")).expect("assets");
        std::fs::write(dir.join("assets/x.png"), b"x").expect("file");
        assert_eq!(
            validated_asset_arg(&dir, "assets//x.png").expect("double slash"),
            "assets/x.png"
        );
        assert_eq!(
            validated_asset_arg(&dir, "assets/./x.png").expect("dot segment"),
            "assets/x.png"
        );
        assert_eq!(
            validated_asset_arg(&dir, "x.png").expect("bare name"),
            "assets/x.png"
        );
        assert!(validated_asset_arg(&dir, "assets/../x.png").is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
