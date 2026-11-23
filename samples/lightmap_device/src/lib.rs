// SPDX-License-Identifier: MIT-0
//! `lightmap_device` (ADR-2792): the lightmap baker's quad-and-box reference scene (a 4 m floor and a
//! 1 m box, `regia bake-lightmaps quad-box`) drawn as lightmap receivers, so a phone can measure what a
//! lightmap costs: the atlas's GPU bytes, the app's memory with it installed or not, and the frame cost
//! with `render.set_lightmaps` on or off.
//!
//! The sample installs nothing by itself. A driver puts `.flmap` files in the sample's data directory
//! (`<app files>/lm` on Android, `./lm` or `$LIGHTMAP_DEVICE_DIR` on a desktop) and writes one line to
//! `command.txt` there: `install <file>` or `clear`. The sample reads the line once, runs it and renames
//! the file to `command.done`, then answers in `status.txt`. Switching the lightmap on and off, and
//! reading the numbers, go through the agent protocol (`render.set_lightmaps`, `render.get_lightmaps`).
//! The meshes below must match the baker's scene vertex for vertex: a receiver's unwrap indexes them.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use regia::prelude::*;
use regia_core::math::{Mat3, Quat, Vec3};
use regia_render::lightmap::{InstalledLightmap, LightmapReceiver};
use regia_render::{Camera, CpuMaterial, CpuMesh, DirectionalLight, LabVertex, MeshRenderer};
use regia_scene::Transform;

/// Half the floor's side, metres (the baker's `QuadBoxScene::FLOOR_HALF`).
const FLOOR_HALF: f32 = 2.0;
/// The box's minimum corner (the baker's `QuadBoxScene::BOX_MIN`).
const BOX_MIN: [f32; 3] = [0.0, 0.0, -0.5];
/// The box's maximum corner (the baker's `QuadBoxScene::BOX_MAX`).
const BOX_MAX: [f32; 3] = [1.0, 1.0, 0.5];
/// Frames between two looks at `command.txt`.
const POLL_FRAMES: u32 = 15;

/// The sample's plugin.
pub struct LightmapDevice;

/// Where the driver's files live and the frame counter that paces the polling.
#[derive(Resource, Debug)]
struct Driver {
    dir: PathBuf,
    frames: u32,
}

impl Plugin for LightmapDevice {
    fn build(&self, app: &mut App) {
        let dir = data_dir(app);
        install(app, dir);
    }
}

/// The plugin's work with the driver's directory chosen.
fn install(app: &mut App, dir: PathBuf) {
    if let Err(err) = std::fs::create_dir_all(&dir) {
        tracing::warn!(target: "lightmap_device", "cannot create {}: {err}", dir.display());
    }
    stage_shader_overrides(&dir.join("shaders"));
    // `regia::DefaultPlugins` has no transform propagation; without it every
    // `GlobalTransform` stays the identity and the camera looks from inside the box at the
    // floor's edge, so the receivers cover almost no pixels (ADR-3145).
    if !app
        .world()
        .contains_resource::<regia_scene::propagate::PropagationCache>()
    {
        app.add_plugins(regia_scene::ScenePlugin);
    }
    // Extraction can render with fallback settings, but the live profile protocol needs the
    // resource itself. Preserve a host's preset while making standalone device runs switchable.
    app.init_resource::<regia_render::RenderSettings>()
        .insert_resource(Driver { dir, frames: 0 })
        .insert_resource(InstalledLightmap::default())
        .add_systems(Startup, spawn_scene)
        .add_systems(Update, poll_command);
}

/// Stages every `*.wgsl` file in `dir` as an edit of the engine shader of the same name
/// (`regia_render::shader_reload`), before the device and the boot pipeline warm exist, so a
/// device bisect of a driver's shader compiler (ADR-1749's method, ADR-3145) needs no rebuild:
/// push an edited shader, relaunch, read the log. Returns how many files were staged; a file
/// the composer rejects is logged and skipped. No directory: nothing happens.
pub fn stage_shader_overrides(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "wgsl"))
        .collect();
    files.sort();
    let mut staged = 0;
    for path in files {
        match regia_render::shader_reload::stage_shader_file(&path) {
            Ok(swap) => {
                staged += 1;
                tracing::info!(target: "lightmap_device", "shader override {} staged ({} entries validated)", swap.file, swap.validated);
            }
            Err(err) => {
                tracing::warn!(target: "lightmap_device", "shader override {} refused: {err}", path.display())
            }
        }
    }
    staged
}

#[cfg(target_os = "android")]
fn data_dir(app: &App) -> PathBuf {
    app.world()
        .get_resource::<regia::platform::AndroidAppHandle>()
        .and_then(|h| h.0.internal_data_path())
        .map_or_else(|| PathBuf::from("lm"), |p| p.join("lm"))
}

#[cfg(not(target_os = "android"))]
fn data_dir(_app: &App) -> PathBuf {
    regia_core::brand::env_var_os("LIGHTMAP_DEVICE_DIR").map_or_else(|| PathBuf::from("lm"), PathBuf::from)
}

/// The floor: the baker's quad, winding as the baker has it.
pub fn floor_mesh() -> CpuMesh {
    let h = FLOOR_HALF;
    let n = [0.0, 1.0, 0.0];
    let uv = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
    let p = [[-h, 0.0, -h], [h, 0.0, -h], [h, 0.0, h], [-h, 0.0, h]];
    CpuMesh {
        vertices: p
            .iter()
            .zip(uv)
            .map(|(&p, uv)| LabVertex::from_pos_normal_uv(p, n, uv))
            .collect(),
        indices: vec![0, 2, 1, 0, 3, 2],
    }
}

/// The box: six faces of four vertices, in the baker's face order.
pub fn box_mesh() -> CpuMesh {
    let (min, max) = (Vec3::from_array(BOX_MIN), Vec3::from_array(BOX_MAX));
    let centre = (min + max) * 0.5;
    let half = (max - min) * 0.5;
    let faces = [
        (Vec3::X, Vec3::Y, Vec3::Z),
        (-Vec3::X, Vec3::Z, Vec3::Y),
        (Vec3::Y, Vec3::Z, Vec3::X),
        (-Vec3::Y, Vec3::X, Vec3::Z),
        (Vec3::Z, Vec3::X, Vec3::Y),
        (-Vec3::Z, Vec3::Y, Vec3::X),
    ];
    let mut mesh = CpuMesh {
        vertices: Vec::new(),
        indices: Vec::new(),
    };
    for (n, u, v) in faces {
        let (u, v, c) = (u * half, v * half, centre + n * half);
        let base = mesh.vertices.len() as u32;
        for (su, sv) in [(-1.0f32, -1.0f32), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
            let uv = [(su + 1.0) * 0.5, (sv + 1.0) * 0.5];
            mesh.vertices.push(LabVertex::from_pos_normal_uv(
                (c + u * su + v * sv).to_array(),
                n.to_array(),
                uv,
            ));
        }
        mesh.indices
            .extend([base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    mesh
}

fn material(base_color: [f32; 4]) -> Arc<CpuMaterial> {
    Arc::new(CpuMaterial {
        base_color,
        roughness: 0.9,
        ..CpuMaterial::default()
    })
}

fn spawn_scene(mut commands: Commands) {
    commands.spawn((
        DirectionalLight {
            direction: [-0.4, -0.6, -0.5],
            color: [1.0, 1.0, 1.0],
            illuminance: 20_000.0,
            cast_shadows: false,
            static_shadow: false,
        },
        Transform::default(),
    ));
    let eye = Vec3::new(3.0, 4.0, 5.0);
    let forward = (Vec3::new(0.5, 0.0, 0.0) - eye).normalize();
    let right = forward.cross(Vec3::Y).normalize();
    let up = right.cross(forward);
    let rotation = Quat::from_mat3(&Mat3::from_cols(right, up, -forward));
    commands.spawn((
        Transform {
            translation: eye,
            rotation,
            ..Transform::default()
        },
        Camera {
            fov_y: 50.0_f32.to_radians(),
            near: 0.1,
            far: 100.0,
            exposure_ev: Some(12.0),
            ..Camera::default()
        },
    ));
    commands.spawn((
        Transform::default(),
        MeshRenderer::new(Arc::new(floor_mesh()), material([0.5, 0.5, 0.5, 1.0])),
        LightmapReceiver {
            entry: "instance_0".into(),
        },
    ));
    commands.spawn((
        Transform::default(),
        MeshRenderer::new(Arc::new(box_mesh()), material([0.8, 0.15, 0.1, 1.0])),
        LightmapReceiver {
            entry: "instance_1".into(),
        },
    ));
}

/// What one `command.txt` line asks for.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    /// Install the named `.flmap` from the data directory.
    Install(String),
    /// Remove the installed lightmap.
    Clear,
}

/// Parses a command line. A file name has no separators, so a command cannot leave the data directory.
pub fn parse_command(line: &str) -> Option<Command> {
    let mut words = line.split_whitespace();
    match (words.next()?, words.next(), words.next()) {
        ("clear", None, None) => Some(Command::Clear),
        ("install", Some(file), None)
            if file.ends_with(".flmap") && !file.contains(['/', '\\']) && !file.starts_with('.') =>
        {
            Some(Command::Install(file.to_owned()))
        }
        _ => None,
    }
}

fn poll_command(mut driver: ResMut<Driver>, mut installed: ResMut<InstalledLightmap>) {
    driver.frames = driver.frames.wrapping_add(1);
    if driver.frames == 1 {
        tracing::info!(target: "lightmap_device", "polling {}", driver.dir.display());
    }
    if !driver.frames.is_multiple_of(POLL_FRAMES) {
        return;
    }
    let command_path = driver.dir.join("command.txt");
    let Ok(line) = std::fs::read_to_string(&command_path) else {
        return;
    };
    tracing::info!(target: "lightmap_device", "command {}", line.trim());
    let status = match parse_command(&line) {
        Some(Command::Clear) => {
            installed.clear();
            "ok clear".to_owned()
        }
        Some(Command::Install(file)) => match installed.load(&driver.dir.join(&file), file.as_str()) {
            Ok(()) => format!("ok install {file}"),
            Err(err) => format!("error install {file} {} {err}", err.code()),
        },
        None => format!("error unknown_command {}", line.trim()),
    };
    write_status(&driver.dir, &status);
    let _ = std::fs::rename(&command_path, driver.dir.join("command.done"));
}

fn write_status(dir: &Path, status: &str) {
    let tmp = dir.join("status.tmp");
    if std::fs::write(&tmp, status).is_ok() {
        let _ = std::fs::rename(&tmp, dir.join("status.txt"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Live `render.set_profile` needs the settings resource even before the first frame.
    #[test]
    fn profile_switch_settings_exist_and_preserve_host_configuration() {
        let dir = regia_test_utils::unique_temp_dir("lightmap-device-profile");
        let mut app = App::new();
        install(&mut app, dir.clone());
        assert!(app.world().contains_resource::<regia_render::RenderSettings>());
        app.world_mut()
            .resource_mut::<regia_render::RenderSettings>()
            .apply_preset(regia_render::RenderProfilePreset::MobileLow);
        install(&mut app, dir.clone());
        assert_eq!(
            app.world().resource::<regia_render::RenderSettings>().preset(),
            Some(regia_render::RenderProfilePreset::MobileLow)
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn commands_parse_and_stay_in_the_directory() {
        assert_eq!(parse_command("clear\n"), Some(Command::Clear));
        assert_eq!(
            parse_command("install a.flmap"),
            Some(Command::Install("a.flmap".into()))
        );
        for bad in [
            "",
            "install",
            "install a.txt",
            "install ../a.flmap",
            "install /a.flmap",
            "install .a.flmap",
            "clear now",
        ] {
            assert_eq!(parse_command(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_meshes_have_the_bakers_vertex_counts() {
        assert_eq!(floor_mesh().vertices.len(), 4);
        assert_eq!(floor_mesh().indices.len(), 6);
        assert_eq!(box_mesh().vertices.len(), 24);
        assert_eq!(box_mesh().indices.len(), 36);
    }

    /// The override hook stages each `.wgsl` file of the directory, skips other files, logs and
    /// skips a file the composer refuses, and does nothing without the directory (ADR-3145).
    #[test]
    fn shader_overrides_stage_wgsl_files_and_skip_the_rest() {
        let dir = regia_test_utils::unique_temp_dir("lightmap-device-shaders");
        assert_eq!(stage_shader_overrides(&dir.join("missing")), 0);
        let engine = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../crates/regia_render/shaders");
        let text =
            std::fs::read_to_string(engine.join("xr_eye_mask.wgsl")).expect("engine shader in the checkout");
        std::fs::write(dir.join("xr_eye_mask.wgsl"), text).expect("write override");
        std::fs::write(dir.join("notes.txt"), "not a shader").expect("write note");
        assert_eq!(
            stage_shader_overrides(&dir),
            1,
            "the valid shader is staged, the note skipped"
        );
        std::fs::write(dir.join("xr_eye_mask.wgsl"), "fn broken(").expect("write broken override");
        assert_eq!(stage_shader_overrides(&dir), 0, "a refused file is not counted");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The camera ends up where `spawn_scene` puts it, looking at the floor: the plugin brings
    /// transform propagation (ADR-3145; before, the camera stayed at the origin).
    #[test]
    fn the_camera_is_placed_and_looks_at_the_floor() {
        let dir = regia_test_utils::unique_temp_dir("lightmap-device-camera");
        let mut app = App::new();
        app.insert_resource(Driver {
            dir: dir.clone(),
            frames: 0,
        });
        LightmapDevice.build(&mut app);
        app.update();
        app.update();
        let world = app.world_mut();
        let mut cameras = world.query::<(&regia_render::Camera, &regia_scene::GlobalTransform)>();
        let (_, global) = cameras.single(world).expect("one camera");
        let eye = Vec3::from(global.0.translation);
        assert!((eye - Vec3::new(3.0, 4.0, 5.0)).length() < 1e-4, "eye {eye:?}");
        let forward = -Vec3::from(global.0.matrix3.z_axis);
        assert!(forward.y < -0.5, "looks down at the floor: {forward:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
