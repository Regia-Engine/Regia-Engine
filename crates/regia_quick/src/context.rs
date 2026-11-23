// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from nannou (https://github.com/nannou-org/nannou, commit bffa988),
// nannou/src/context.rs (the App system parameter). Copyright (c) 2017 MindBuffer, Copyright (c) 2019 nannou-org; dual-licensed
// MIT OR Apache-2.0, used here under the MIT licence (see docs/ported-sources.toml).
//
// Permission is hereby granted, free of charge, to any person obtaining a copy of this software
// and associated documentation files (the "Software"), to deal in the Software without restriction,
// including without limitation the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the Software is furnished to do so,
// subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all copies or substantial
// portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT
// LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN
// NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY,
// WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE
// SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
//! [`Quick`]: one system parameter with the conveniences a small game or sketch needs: time, keys,
//! mouse and touch in drawing coordinates, the window rectangle, [`Draw`], saving a frame, quitting
//! and loading bytes.
//!
//! `Quick` is an ordinary [`SystemParam`]. It works in any Regia Engine system (`fn system(q: Quick)`)
//! and behind a shared reference in the quick app's `model`/`update`/`view` functions
//! (`fn view(q: &Quick, model: &Model)`): every method takes `&self`, and changes (quit, run mode,
//! screenshots) are deferred through [`ParallelCommands`].
//!
//! Coordinates match [`Draw`]: logical units, origin at the window centre, +X right, +Y up.

use std::path::{Path, PathBuf};

use regia_app::prelude::*;
use regia_core::math::geom::Rect;
use regia_core::math::{Mat3, Quat, Vec2, Vec3};
use regia_ecs::ecs::system::{ParallelCommands, SystemParam};
use regia_input::{Keyboard, Mouse, Touches};
use regia_platform::{
    CursorGrab, MouseButton, PrimaryWindow, TouchPhase, Vfs, VfsPath, Window, WindowSettings,
};
use regia_render::{Camera, DirectionalLight, ScreenshotRequests};
use regia_scene::{GlobalTransform, Transform};

use crate::load::BytesRequest;
use crate::run_mode::{QuickLoop, RunMode};
use crate::{Draw, Draw3d};

/// The quick layer's system parameter. See the [module documentation](self).
///
/// # Example
///
/// ```
/// use regia_app::prelude::*;
/// use regia_quick::Quick;
///
/// fn spin(q: Quick) {
///     let draw = q.draw();
///     draw.rect().w_h(80.0, 80.0).rotate(q.time());
/// }
///
/// let mut app = App::new();
/// app.add_systems(Update, spin);
/// app.update(); // without a window or QuickPlugin the drawing goes nowhere, but it runs
/// ```
#[derive(SystemParam)]
pub struct Quick<'w, 's> {
    commands: ParallelCommands<'w, 's>,
    time: Option<Res<'w, Time>>,
    keyboard: Option<Res<'w, Keyboard>>,
    mouse: Option<Res<'w, Mouse>>,
    touches: Option<Res<'w, Touches>>,
    window: Query<'w, 's, &'static Window, With<PrimaryWindow>>,
    window_settings: Option<Res<'w, WindowSettings>>,
    draw: Option<Res<'w, Draw>>,
    draw_3d: Option<Res<'w, Draw3d>>,
    run_mode: Option<Res<'w, RunMode>>,
    loop_state: Option<Res<'w, QuickLoop>>,
    vfs: Option<Res<'w, Vfs>>,
}

/// One finger, in drawing coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QuickTouch {
    /// Stable finger id for the duration of the touch.
    pub id: u64,
    /// Current position (logical units, window centre origin, +Y up).
    pub position: Vec2,
    /// Where the finger went down, in the same coordinates.
    pub start: Vec2,
    /// Current phase.
    pub phase: TouchPhase,
}

impl Quick<'_, '_> {
    /// Seconds of game time since start-up.
    pub fn time(&self) -> f32 {
        self.time.as_ref().map_or(0.0, |t| t.elapsed_secs())
    }

    /// Seconds of game time since the previous frame.
    pub fn delta(&self) -> f32 {
        self.time.as_ref().map_or(0.0, |t| t.delta_secs())
    }

    /// Frames run since start-up (the first frame is 0).
    pub fn frame(&self) -> u64 {
        self.time
            .as_ref()
            .map_or(0, |t| t.frame_count().saturating_sub(1))
    }

    /// Frames on which the quick app's `update`/`view` ran (equals [`Quick::frame`] + 1 in
    /// [`RunMode::Loop`]); 0 outside a quick app.
    pub fn runs(&self) -> u64 {
        self.loop_state.as_ref().map_or(0, |s| s.runs)
    }

    /// True while `key` is held. Keys are physical names: `"KeyW"`, `"Space"`, `"ArrowLeft"`,
    /// `"Enter"`, `"Digit1"`, `"ShiftLeft"`, ….
    pub fn key_down(&self, key: &str) -> bool {
        self.keyboard.as_ref().is_some_and(|k| k.pressed(key))
    }

    /// True on the frame `key` went down (auto-repeat excluded).
    pub fn key_pressed(&self, key: &str) -> bool {
        self.keyboard.as_ref().is_some_and(|k| k.just_pressed(key))
    }

    /// True on the frame `key` came back up.
    pub fn key_released(&self, key: &str) -> bool {
        self.keyboard.as_ref().is_some_and(|k| k.just_released(key))
    }

    /// True while `button` is held.
    pub fn mouse_down(&self, button: MouseButton) -> bool {
        self.mouse.as_ref().is_some_and(|m| m.pressed(button))
    }

    /// True on the frame `button` went down.
    pub fn mouse_pressed(&self, button: MouseButton) -> bool {
        self.mouse.as_ref().is_some_and(|m| m.just_pressed(button))
    }

    /// True on the frame `button` came back up.
    pub fn mouse_released(&self, button: MouseButton) -> bool {
        self.mouse.as_ref().is_some_and(|m| m.just_released(button))
    }

    /// The cursor in drawing coordinates (logical units, window centre origin, +Y up).
    pub fn mouse(&self) -> Vec2 {
        self.screen_to_world(self.mouse_screen())
    }

    /// The cursor in physical window pixels (top-left origin, +Y down), as the platform reports it.
    pub fn mouse_screen(&self) -> Vec2 {
        self.mouse.as_ref().map_or(Vec2::ZERO, |m| m.position())
    }

    /// Raw mouse motion this frame (device units; useful for mouse look).
    pub fn mouse_delta(&self) -> Vec2 {
        self.mouse.as_ref().map_or(Vec2::ZERO, |m| m.delta())
    }

    /// Wheel motion this frame, in lines.
    pub fn mouse_wheel(&self) -> Vec2 {
        self.mouse.as_ref().map_or(Vec2::ZERO, |m| m.wheel())
    }

    /// Fingers currently tracked, in drawing coordinates.
    pub fn touches(&self) -> Vec<QuickTouch> {
        let Some(touches) = self.touches.as_ref() else {
            return Vec::new();
        };
        touches
            .iter()
            .map(|t| QuickTouch {
                id: t.id,
                position: self.screen_to_world(t.current),
                start: self.screen_to_world(t.start),
                phase: t.phase,
            })
            .collect()
    }

    /// Window scale factor (physical pixels per logical unit).
    pub fn scale_factor(&self) -> f32 {
        self.window
            .iter()
            .next()
            .map_or(1.0, |w| w.scale_factor as f32)
            .max(f32::MIN_POSITIVE)
    }

    /// The window size in logical units. Before the window exists (for example in a quick app's
    /// `model`, which runs while the app is built) this is the size the window was asked for.
    pub fn window_size(&self) -> Vec2 {
        if let Some(window) = self.window.iter().next() {
            let (w, h) = window.physical_size;
            if w > 0 && h > 0 {
                let scale = window.scale_factor.max(f64::MIN_POSITIVE);
                return Vec2::new((f64::from(w) / scale) as f32, (f64::from(h) / scale) as f32);
            }
            let (w, h) = window.logical_size;
            return Vec2::new(w as f32, h as f32);
        }
        let (w, h) = self
            .window_settings
            .as_ref()
            .map_or_else(|| Window::default().logical_size, |s| s.window.logical_size);
        Vec2::new(w as f32, h as f32)
    }

    /// The window as a rectangle in drawing coordinates: centred on the origin, [`Quick::window_size`]
    /// wide and high.
    pub fn window_rect(&self) -> Rect {
        let size = self.window_size();
        Rect::from_w_h(size.x, size.y)
    }

    /// Converts physical window pixels (top-left origin, +Y down) to drawing coordinates.
    pub fn screen_to_world(&self, screen: Vec2) -> Vec2 {
        let size = self.window_size();
        let logical = screen / self.scale_factor();
        Vec2::new(logical.x - size.x * 0.5, size.y * 0.5 - logical.y)
    }

    /// Converts drawing coordinates to physical window pixels (top-left origin, +Y down).
    pub fn world_to_screen(&self, world: Vec2) -> Vec2 {
        let size = self.window_size();
        Vec2::new(world.x + size.x * 0.5, size.y * 0.5 - world.y) * self.scale_factor()
    }

    /// The frame's 2D drawing handle (shown over the frame, cleared for the next one).
    ///
    /// Without [`QuickPlugin`](crate::QuickPlugin) the returned handle is detached: drawing into it
    /// is allowed and shows nothing.
    pub fn draw(&self) -> Draw {
        self.draw.as_ref().map_or_else(Draw::default, |d| (**d).clone())
    }

    /// The frame's lit 3D primitive drawing handle (cube, sphere, cylinder, plane, grid, lines).
    /// Detached, like [`Quick::draw`], without [`QuickPlugin`](crate::QuickPlugin).
    pub fn draw_3d(&self) -> Draw3d {
        self.draw_3d
            .as_ref()
            .map_or_else(Draw3d::default, |d| (**d).clone())
    }

    /// Places the 3D camera at `eye` looking at `target` (+Y up), for [`Quick::draw_3d`].
    ///
    /// The first call spawns a perspective [`Camera`] marked [`QuickCamera`] and, when the world has
    /// no [`DirectionalLight`], a sun marked [`QuickSun`]; later calls move the same camera. Applied at
    /// the end of the system, so call it every frame the camera moves.
    pub fn camera_3d(&self, eye: Vec3, target: Vec3) {
        let transform = Transform {
            translation: eye,
            rotation: look_rotation(eye, target),
            scale: Vec3::ONE,
        };
        self.commands.command_scope(|mut commands| {
            commands.queue(move |world: &mut World| place_camera(world, transform));
        });
    }

    /// Locks and hides the cursor for mouse look (`true`), or frees and shows it (`false`). Read the
    /// motion with [`Quick::mouse_delta`].
    pub fn grab_cursor(&self, grab: bool) {
        self.commands.command_scope(|mut commands| {
            commands.queue(move |world: &mut World| {
                let mut windows = world.query_filtered::<&mut Window, With<PrimaryWindow>>();
                for mut window in windows.iter_mut(world) {
                    window.cursor_grab = if grab {
                        CursorGrab::Locked
                    } else {
                        CursorGrab::None
                    };
                    window.cursor_visible = !grab;
                }
            });
        });
    }

    /// Runs `f` with [`Commands`] (applied at the end of the system), for anything outside the
    /// curated surface: spawning entities, inserting resources.
    pub fn commands<R>(&self, f: impl FnOnce(Commands) -> R) -> R {
        self.commands.command_scope(f)
    }

    /// The quick app's current run mode ([`RunMode::Loop`] outside a quick app).
    pub fn run_mode(&self) -> RunMode {
        self.run_mode.as_deref().copied().unwrap_or_default()
    }

    /// Changes the run mode from the next frame. Setting [`RunMode::LoopNTimes`] again restarts its
    /// budget.
    pub fn set_run_mode(&self, mode: RunMode) {
        self.commands.command_scope(|mut commands| {
            commands.queue(move |world: &mut World| {
                world.insert_resource(mode);
                if let Some(mut state) = world.get_resource_mut::<QuickLoop>() {
                    state.restart();
                }
            });
        });
    }

    /// Asks the app to exit at the end of this frame.
    pub fn quit(&self) {
        self.commands.command_scope(|mut commands| {
            commands.queue(|world: &mut World| {
                world.write_message(AppExit::Success);
            });
        });
    }

    /// Saves the next presented frame of the window as a PNG at `path` (a host path, created with
    /// its parent directories). Errors are logged; nothing happens without a renderer.
    pub fn save_frame(&self, path: impl AsRef<Path>) {
        let path = path.as_ref().to_path_buf();
        self.commands.command_scope(|mut commands| {
            commands.queue(move |world: &mut World| {
                let Some(mut requests) = world.get_resource_mut::<ScreenshotRequests>() else {
                    tracing::warn!(target: "regia_quick", "save_frame: no renderer; {} not written", path.display());
                    return;
                };
                requests.request(
                    None,
                    Box::new(move |captured| match captured {
                        Ok(image) => {
                            if let Err(err) = write_png(&path, image.width, image.height, &image.rgba) {
                                tracing::error!(target: "regia_quick", "save_frame {}: {err}", path.display());
                            }
                        }
                        Err(err) => {
                            tracing::error!(target: "regia_quick", "save_frame {}: {err}", path.display());
                        }
                    }),
                );
            });
        });
    }

    /// Starts reading a file through the [`Vfs`] on the IO task pool (`"assets://levels/a.ron"`,
    /// `"user://save.bin"`, or a bare path, which means `assets://`). Poll the request in later
    /// frames; the same call works on desktop and on Android (APK assets).
    pub fn load_bytes(&self, path: &str) -> BytesRequest {
        let Some(vfs) = self.vfs.as_deref() else {
            return BytesRequest::failed(path, "no Vfs (add PlatformPlugin or insert a Vfs)".into());
        };
        BytesRequest::start(vfs, path)
    }

    /// The parsed form of a `load_bytes` path (`assets://` when no mount is given).
    pub fn vfs_path(path: &str) -> Result<VfsPath, String> {
        crate::load::parse_path(path)
    }
}

/// The camera [`Quick::camera_3d`] spawned and moves.
#[derive(Component, Debug, Clone, Copy, Default)]
pub struct QuickCamera;

/// The sun [`Quick::camera_3d`] spawned because the world had no directional light.
#[derive(Component, Debug, Clone, Copy, Default)]
pub struct QuickSun;

/// The rotation that points an entity's −Z axis from `eye` at `target`, +Y up (identity if they meet).
fn look_rotation(eye: Vec3, target: Vec3) -> Quat {
    let forward = (target - eye).normalize_or_zero();
    if forward == Vec3::ZERO {
        return Quat::IDENTITY;
    }
    let up = if forward.y.abs() > 0.999 { Vec3::Z } else { Vec3::Y };
    let right = forward.cross(up).normalize();
    let up = right.cross(forward);
    Quat::from_mat3(&Mat3::from_cols(right, up, -forward))
}

fn place_camera(world: &mut World, transform: Transform) {
    let global = GlobalTransform(transform.compute_affine());
    let existing = world
        .query_filtered::<Entity, With<QuickCamera>>()
        .iter(world)
        .next();
    if let Some(camera) = existing {
        world.entity_mut(camera).insert((transform, global));
        return;
    }
    world.spawn((
        Name::new("Quick Camera"),
        QuickCamera,
        Camera::default(),
        transform,
        global,
    ));
    if world.query::<&DirectionalLight>().iter(world).next().is_none() {
        let direction = Vec3::new(-0.4, -0.8, -0.45).normalize();
        let sun = Transform {
            translation: Vec3::ZERO,
            rotation: Quat::from_rotation_arc(-Vec3::Z, direction),
            scale: Vec3::ONE,
        };
        world.spawn((
            Name::new("Quick Sun"),
            QuickSun,
            sun,
            GlobalTransform(sun.compute_affine()),
            DirectionalLight {
                direction: direction.to_array(),
                color: [1.0, 1.0, 1.0],
                illuminance: 100_000.0,
                cast_shadows: true,
                static_shadow: false,
            },
        ));
    }
}

/// Writes tightly packed RGBA8 rows as a PNG, creating parent directories.
fn write_png(path: &PathBuf, width: u32, height: u32, rgba: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    // Write beside the target and rename, so a failed write never replaces a good file.
    // The name is unique per call, so a concurrent save or a user's `<name>.tmp` is never touched.
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut tmp_name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    tmp_name.push(format!(
        ".{}-{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let tmp = path.with_file_name(tmp_name);
    let result = write_png_file(&tmp, width, height, rgba)
        .and_then(|()| std::fs::rename(&tmp, path).map_err(|e| e.to_string()));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Encodes and flushes the PNG at `path`, surfacing every IO error (including the final flush).
fn write_png_file(path: &Path, width: u32, height: u32, rgba: &[u8]) -> Result<(), String> {
    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut buffered = std::io::BufWriter::new(file);
    let mut encoder = png::Encoder::new(&mut buffered, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(rgba).map_err(|e| e.to_string())?;
    writer.finish().map_err(|e| e.to_string())?;
    let file = buffered.into_inner().map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_ecs::ecs::system::SystemState;

    fn world_with_window(physical: (u32, u32), scale: f64) -> World {
        let mut world = World::new();
        world.spawn((
            Window {
                physical_size: physical,
                scale_factor: scale,
                ..Window::default()
            },
            PrimaryWindow,
        ));
        world
    }

    #[test]
    fn coordinates_are_centred_logical_and_y_up() {
        let mut world = world_with_window((1600, 1200), 2.0);
        let mut state = SystemState::<Quick>::new(&mut world);
        let q = state.get_mut(&mut world).expect("quick resolves");
        assert_eq!(q.window_size(), Vec2::new(800.0, 600.0));
        let rect = q.window_rect();
        assert_eq!((rect.left(), rect.top()), (-400.0, 300.0));
        // Top-left pixel → top-left corner; centre → origin.
        assert_eq!(q.screen_to_world(Vec2::new(0.0, 0.0)), Vec2::new(-400.0, 300.0));
        assert_eq!(q.screen_to_world(Vec2::new(800.0, 600.0)), Vec2::ZERO);
        let p = Vec2::new(123.0, -45.0);
        assert_eq!(q.screen_to_world(q.world_to_screen(p)), p);
    }

    #[test]
    fn before_the_window_exists_the_requested_size_is_used() {
        let mut world = World::new();
        let mut settings = WindowSettings::default();
        settings.window.logical_size = (640.0, 480.0);
        world.insert_resource(settings);
        let mut state = SystemState::<Quick>::new(&mut world);
        let q = state.get_mut(&mut world).expect("quick resolves");
        assert_eq!(q.window_size(), Vec2::new(640.0, 480.0));
        // Everything optional is absent: the conveniences degrade, nothing panics.
        assert_eq!(q.time(), 0.0);
        assert!(!q.key_down("Space"));
        assert!(q.touches().is_empty());
        assert_eq!(q.run_mode(), RunMode::Loop);
        assert!(q.load_bytes("missing.bin").poll().is_some_and(|r| r.is_err()));
    }

    #[test]
    fn camera_3d_spawns_once_then_moves_and_looks_at_the_target() {
        let mut app = App::new();
        app.add_systems(Update, |q: Quick| {
            q.camera_3d(Vec3::new(0.0, 0.0, 5.0 + q.frame() as f32), Vec3::ZERO)
        });
        app.update();
        app.update();
        let world = app.world_mut();
        let cams: Vec<Transform> = world
            .query_filtered::<&Transform, With<QuickCamera>>()
            .iter(world)
            .copied()
            .collect();
        assert_eq!(cams.len(), 1, "one camera, moved");
        assert_eq!(cams[0].translation, Vec3::new(0.0, 0.0, 6.0));
        let forward = cams[0].rotation * -Vec3::Z;
        assert!((forward - -Vec3::Z).length() < 1e-5, "{forward}");
        assert_eq!(world.query::<&QuickSun>().iter(world).count(), 1);
        let down = look_rotation(Vec3::Y, Vec3::ZERO) * -Vec3::Z;
        assert!(
            (down - -Vec3::Y).length() < 1e-5,
            "straight down is handled: {down}"
        );
    }

    #[test]
    fn quit_writes_app_exit() {
        let mut app = App::new();
        app.add_systems(Update, |q: Quick| q.quit());
        app.update();
        assert_eq!(app.should_exit(), Some(AppExit::Success));
    }

    #[test]
    fn failed_png_write_keeps_the_previous_file() {
        let dir = regia_test_utils::unique_temp_dir("quick-png-keep");
        let path = dir.join("frame.png");
        std::fs::write(&path, b"previous").expect("seed");
        // Wrong pixel count: the encoder rejects it; only the temp file is touched.
        assert!(write_png(&path, 2, 2, &[0; 4]).is_err());
        assert_eq!(std::fs::read(&path).expect("read"), b"previous");
        assert_eq!(dir_names(&dir), ["frame.png"], "temp file is cleaned up");
        let _ = std::fs::remove_dir_all(dir);
    }

    fn dir_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("read_dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn png_write_leaves_a_users_tmp_sibling_alone() {
        let dir = regia_test_utils::unique_temp_dir("quick-png-sibling");
        let path = dir.join("frame.png");
        std::fs::write(dir.join("frame.png.tmp"), b"mine").expect("seed");
        assert!(write_png(&path, 2, 2, &[0; 4]).is_err());
        write_png(&path, 1, 1, &[1, 2, 3, 255]).expect("write");
        assert_eq!(std::fs::read(dir.join("frame.png.tmp")).expect("read"), b"mine");
        assert_eq!(dir_names(&dir), ["frame.png", "frame.png.tmp"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn png_writer_round_trips_pixels() {
        let dir = regia_test_utils::unique_temp_dir("quick-png");
        let path = dir.join("nested").join("frame.png");
        let rgba = [255, 0, 0, 255, 0, 255, 0, 255];
        write_png(&path, 2, 1, &rgba).expect("write");
        assert_eq!(
            dir_names(&dir.join("nested")),
            ["frame.png"],
            "temp file is renamed away"
        );
        let decoder = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(&path).expect("open")));
        let mut reader = decoder.read_info().expect("png");
        assert_eq!((reader.info().width, reader.info().height), (2, 1));
        let mut buf = vec![0u8; reader.output_buffer_size().expect("png size")];
        let frame = reader.next_frame(&mut buf).expect("decode");
        assert_eq!(frame.color_type, png::ColorType::Rgba);
        assert_eq!(frame.bit_depth, png::BitDepth::Eight);
        assert_eq!(&buf[..frame.buffer_size()], &rgba);
        let short = write_png(&dir.join("short.png"), 2, 1, &rgba[..4]);
        assert!(short.is_err(), "a short pixel buffer is rejected");
        let _ = std::fs::remove_dir_all(dir);
    }
}
