// SPDX-License-Identifier: MPL-2.0
//! High-resolution screenshots of the main viewport (spec addendum § 3.8, ADR-7768,
//! `book/agents/viewport-tools.md`).
//!
//! **File ▸ Screenshot…** opens a dialog: the size as a multiplier of the perspective pane's size
//! (1x to 8x) or an explicit width and height (16 to 16384 pixels a side), editor helpers on or off
//! (grid, icons, outline and gizmo painted in, or the plain game look), and a file name. The
//! picture is rendered through its own renderer slot (so no pane's history is touched, and its
//! targets are freed afterwards) and written as a PNG into the project's `Saved/Screenshots`
//! folder, atomically (a temporary file renamed into place). A toast names the file and offers
//! **Open folder**. `editor.screenshot` does the same for agents.
//!
//! The capture is one render when both sides fit the device's texture limit
//! ([`device_limit`]), which on the supported desktop adapters covers the whole 16384 range.
//! [`plan_tiles`] gives the tile grid a size needs under a limit; a size that needs more than one
//! tile answers `screenshot_needs_tiles` (deferred in ADR-7768: rendering a tile needs an
//! off-axis projection window on the renderer's view, which the runtime view does not carry yet).

use std::path::{Path, PathBuf};

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::viewport::EditorCamera;
use crate::viewport_agent::Handler;

/// The largest side a screenshot may have, pixels.
pub const MAX_SIDE: u32 = 16384;
/// The smallest side a screenshot may have, pixels.
pub const MIN_SIDE: u32 = 16;
/// The largest multiplier of the pane size.
pub const MAX_MULTIPLIER: u32 = 8;
/// The renderer slot (a viewport instance id no panel uses) screenshots draw through.
pub const SCREENSHOT_INSTANCE: u64 = u64::MAX - 0x0053_484F;
/// The folder under the project root screenshots go to.
pub const SCREENSHOT_FOLDER: &str = "Saved/Screenshots";

/// How big a screenshot is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShotSize {
    /// A whole multiple (1 to [`MAX_MULTIPLIER`]) of the pane's size.
    Multiplier(u32),
    /// An explicit width and height in pixels.
    Exact([u32; 2]),
}

/// Why a screenshot was refused or failed. [`Self::code`] is the agent error reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShotError {
    /// The multiplier is outside 1..=8.
    BadMultiplier(u64),
    /// A side is outside 16..=16384 (or the multiplied pane size is).
    BadSize([u64; 2]),
    /// The file name is empty or has a path separator or a reserved character.
    BadName(String),
    /// No project is open (there is no `Saved/Screenshots` to write to).
    NoProject,
    /// The size needs more than one tile under the device limit (tiled rendering is deferred).
    NeedsTiles {
        /// The requested size.
        size: [u32; 2],
        /// The device's largest texture side.
        limit: u32,
        /// Tiles the size needs.
        tiles: u32,
    },
    /// The render produced no picture (no adapter).
    RenderFailed,
    /// Writing the file failed.
    Write(String),
}

impl ShotError {
    /// The stable agent error reason.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::BadMultiplier(_) => "screenshot_bad_multiplier",
            Self::BadSize(_) => "screenshot_bad_size",
            Self::BadName(_) => "screenshot_bad_name",
            Self::NoProject => "screenshot_no_project",
            Self::NeedsTiles { .. } => "screenshot_needs_tiles",
            Self::RenderFailed => "screenshot_render_failed",
            Self::Write(_) => "screenshot_write_failed",
        }
    }

    /// A sentence for the dialog and the toast.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::BadMultiplier(m) => format!("multiplier {m} is outside 1 to {MAX_MULTIPLIER}"),
            Self::BadSize([w, h]) => format!("{w}x{h} is outside {MIN_SIDE} to {MAX_SIDE} pixels a side"),
            Self::BadName(name) => format!("`{name}` is not a plain file name"),
            Self::NoProject => {
                "open a project first; screenshots go to its Saved/Screenshots folder".to_owned()
            }
            Self::NeedsTiles { size, limit, tiles } => format!(
                "{}x{} needs {tiles} tiles on this device (largest side {limit}); tiled capture is not available yet",
                size[0], size[1]
            ),
            Self::RenderFailed => "the viewport render produced no picture".to_owned(),
            Self::Write(err) => format!("cannot write the screenshot: {err}"),
        }
    }

    /// The agent error for this refusal.
    #[must_use]
    pub fn to_rpc(&self) -> RpcError {
        RpcError::new(
            INVALID_PARAMS,
            self.code(),
            self.message(),
            match self {
                Self::NoProject => "start the editor with --project <dir>",
                Self::NeedsTiles { .. } => "ask for a size within the device limit",
                _ => "see book/agents/viewport-tools.md",
            },
        )
    }
}

/// The pixel size `size` means for a pane of `base` pixels.
///
/// # Errors
///
/// [`ShotError::BadMultiplier`] or [`ShotError::BadSize`] outside the ranges.
pub fn resolve_size(size: ShotSize, base: [u32; 2]) -> Result<[u32; 2], ShotError> {
    let [width, height] = match size {
        ShotSize::Multiplier(m) => {
            if !(1..=MAX_MULTIPLIER).contains(&m) {
                return Err(ShotError::BadMultiplier(u64::from(m)));
            }
            [
                u64::from(base[0]) * u64::from(m),
                u64::from(base[1]) * u64::from(m),
            ]
        }
        ShotSize::Exact([w, h]) => [u64::from(w), u64::from(h)],
    };
    let side = u64::from(MIN_SIDE)..=u64::from(MAX_SIDE);
    if !side.contains(&width) || !side.contains(&height) {
        return Err(ShotError::BadSize([width, height]));
    }
    Ok([width as u32, height as u32])
}

/// A tile grid: `columns` x `rows` tiles of at most `tile` pixels; the last column and row are
/// cut to the picture's edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TilePlan {
    /// The picture size.
    pub size: [u32; 2],
    /// The largest tile.
    pub tile: [u32; 2],
    /// Tiles across.
    pub columns: u32,
    /// Tiles down.
    pub rows: u32,
}

impl TilePlan {
    /// Tiles in the grid.
    #[must_use]
    pub fn count(&self) -> u32 {
        self.columns * self.rows
    }

    /// The pixel rectangle `[x, y, width, height]` of tile (`column`, `row`).
    #[must_use]
    pub fn tile_rect(&self, column: u32, row: u32) -> [u32; 4] {
        let x = column * self.tile[0];
        let y = row * self.tile[1];
        [
            x,
            y,
            self.tile[0].min(self.size[0].saturating_sub(x)),
            self.tile[1].min(self.size[1].saturating_sub(y)),
        ]
    }
}

/// The fewest equal-width columns and equal-height rows of tiles no larger than `limit` that
/// cover `size`.
#[must_use]
pub fn plan_tiles(size: [u32; 2], limit: u32) -> TilePlan {
    let limit = limit.max(1);
    let columns = size[0].max(1).div_ceil(limit);
    let rows = size[1].max(1).div_ceil(limit);
    TilePlan {
        size,
        tile: [size[0].div_ceil(columns), size[1].div_ceil(rows)],
        columns,
        rows,
    }
}

/// The largest side one render may have on `device` (its 2D texture limit, at most [`MAX_SIDE`]).
#[must_use]
pub fn device_limit(device: &regia_render::RenderDevice) -> u32 {
    device.device.limits().max_texture_dimension_2d.min(MAX_SIDE)
}

/// Renders the main viewport's perspective view at `size` pixels, with or without the editor
/// helpers.
///
/// # Errors
///
/// [`ShotError::NeedsTiles`] past the device limit; [`ShotError::RenderFailed`] without a
/// picture.
pub fn render(
    device: &regia_render::RenderDevice,
    world: &mut World,
    size: [u32; 2],
    helpers: bool,
) -> Result<egui::ColorImage, ShotError> {
    let _span =
        tracing::info_span!(target: "regia_editor", "screenshot_render", width = size[0], height = size[1])
            .entered();
    let limit = device_limit(device);
    let plan = plan_tiles(size, limit);
    if plan.count() > 1 {
        return Err(ShotError::NeedsTiles {
            size,
            limit,
            tiles: plan.count(),
        });
    }
    let camera = world.get_resource_or_insert_with(EditorCamera::default).clone();
    let mut layout = world
        .get_resource::<crate::views::ViewportLayout>()
        .cloned()
        .unwrap_or_default();
    layout.quad = false;
    if !helpers {
        layout.game_view = true;
    }
    let instance = crate::viewport_instance::ViewportInstance {
        id: SCREENSHOT_INSTANCE,
        camera: camera.clone(),
        layout,
        settings: world
            .get_resource::<crate::viewport::ViewportSettings>()
            .cloned()
            .unwrap_or_default(),
        link: None,
    };
    let request = crate::render::PaneRequest {
        pane: 0,
        kind: crate::views::ViewKind::Perspective,
        camera,
        width: size[0],
        height: size[1],
        ids: helpers,
        warm_frames: 0,
        debug_view: regia_render::DebugViewMode::Lit,
        reference: None,
    };
    let frame = if helpers {
        crate::render::capture_instance_pane(device, world, &instance, &request)
    } else {
        crate::render::render_instance_pane(device, world, &instance, &request)
    };
    // A large capture's targets are freed at once rather than kept for the next one.
    crate::render::release_instance_renderers(SCREENSHOT_INSTANCE);
    let image = frame.ok_or(ShotError::RenderFailed)?.image;
    if image.size != [size[0] as usize, size[1] as usize] {
        return Err(ShotError::RenderFailed);
    }
    Ok(image)
}

/// The PNG bytes of `image` (8-bit RGBA).
///
/// # Errors
///
/// The encoder's message.
pub fn png_bytes(image: &egui::ColorImage) -> Result<Vec<u8>, String> {
    let mut raw = Vec::with_capacity(image.pixels.len() * 4);
    for pixel in &image.pixels {
        raw.extend_from_slice(&[pixel.r(), pixel.g(), pixel.b(), pixel.a()]);
    }
    let mut bytes = Vec::new();
    let mut encoder = png::Encoder::new(&mut bytes, image.width() as u32, image.height() as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|err| err.to_string())?;
    writer.write_image_data(&raw).map_err(|err| err.to_string())?;
    drop(writer);
    Ok(bytes)
}

/// Writes `bytes` to `path` atomically: a temporary file next to it, then a rename.
///
/// # Errors
///
/// The IO error of the write or the rename.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temp = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
    std::fs::write(&temp, bytes)?;
    std::fs::rename(&temp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temp);
    })
}

/// The screenshot folder of a project.
#[must_use]
pub fn screenshots_dir(root: &Path) -> PathBuf {
    root.join(SCREENSHOT_FOLDER)
}

/// The first free `screenshot-NNNN.png` in `dir`.
#[must_use]
pub fn next_name(dir: &Path) -> String {
    (1u32..)
        .map(|n| format!("screenshot-{n:04}.png"))
        .find(|name| !dir.join(name).exists())
        .unwrap_or_else(|| "screenshot.png".to_owned())
}

/// A user file name made safe: `.png` added when missing; empty names and names with a path
/// separator, `..` or a reserved character are refused.
///
/// # Errors
///
/// [`ShotError::BadName`].
pub fn file_name(name: &str) -> Result<String, ShotError> {
    let trimmed = name.trim();
    let bad = trimmed.is_empty()
        || trimmed.starts_with('.')
        || trimmed.contains("..")
        || trimmed
            .chars()
            .any(|c| matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control());
    if bad {
        return Err(ShotError::BadName(name.to_owned()));
    }
    Ok(if trimmed.to_ascii_lowercase().ends_with(".png") {
        trimmed.to_owned()
    } else {
        format!("{trimmed}.png")
    })
}

/// What a screenshot wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shot {
    /// The PNG file.
    pub path: PathBuf,
    /// Its size, pixels.
    pub size: [u32; 2],
    /// Tiles rendered.
    pub tiles: u32,
}

/// Renders and writes one screenshot into the project's screenshot folder (`name`: a file name,
/// or the next free `screenshot-NNNN.png`).
///
/// # Errors
///
/// Any [`ShotError`].
pub fn take(
    device: &regia_render::RenderDevice,
    world: &mut World,
    size: [u32; 2],
    helpers: bool,
    name: Option<&str>,
) -> Result<Shot, ShotError> {
    let root = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone())
        .ok_or(ShotError::NoProject)?;
    let dir = screenshots_dir(&root);
    let name = match name {
        Some(name) => file_name(name)?,
        None => next_name(&dir),
    };
    let image = render(device, world, size, helpers)?;
    let bytes = png_bytes(&image).map_err(ShotError::Write)?;
    std::fs::create_dir_all(&dir).map_err(|err| ShotError::Write(err.to_string()))?;
    let path = dir.join(name);
    write_atomic(&path, &bytes).map_err(|err| ShotError::Write(err.to_string()))?;
    Ok(Shot { path, size, tiles: 1 })
}

/// Opens the OS file manager at `folder` (Linux `xdg-open`, Windows `explorer`, macOS `open`).
fn open_folder(folder: &Path) {
    #[cfg(target_os = "windows")]
    let program = "explorer";
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let program = "xdg-open";
    if std::process::Command::new(program).arg(folder).spawn().is_err() {
        tracing::warn!(target: "regia_editor", "cannot open a file manager at {}", folder.display());
    }
}

/// The pane size multipliers apply to: the main viewport's perspective pane as last drawn, else
/// 1280x720.
#[must_use]
pub fn pane_size(world: &World) -> [u32; 2] {
    world
        .get_resource::<ScreenshotDialog>()
        .and_then(|d| d.pane)
        .unwrap_or([1280, 720])
}

/// The Screenshot dialog's state (File ▸ Screenshot…).
#[derive(Resource, Debug, Clone, PartialEq)]
pub struct ScreenshotDialog {
    /// The window shows.
    pub open: bool,
    /// Multiplier mode when true, explicit size when false.
    pub by_multiplier: bool,
    /// The multiplier, 1..=8.
    pub multiplier: u32,
    /// The explicit size.
    pub exact: [u32; 2],
    /// Editor helpers in the picture.
    pub helpers: bool,
    /// The file name (empty: the next free `screenshot-NNNN.png`).
    pub name: String,
    /// The perspective pane's size in pixels when last drawn.
    pub pane: Option<[u32; 2]>,
    /// The last result line, and the folder the last screenshot went to.
    pub toast: Option<(String, Option<PathBuf>)>,
}

impl Default for ScreenshotDialog {
    fn default() -> Self {
        Self {
            open: false,
            by_multiplier: true,
            multiplier: 2,
            exact: [3840, 2160],
            helpers: false,
            name: String::new(),
            pane: None,
            toast: None,
        }
    }
}

/// Opens the dialog (File ▸ Screenshot…, the `file.screenshot` command).
pub fn open_dialog(world: &mut World) {
    world.get_resource_or_insert_with(ScreenshotDialog::default).open = true;
}

/// Notes the main viewport's perspective pane size, in pixels (multipliers apply to it).
pub fn note_pane_size(world: &mut World, size: [u32; 2]) {
    let mut dialog = world.get_resource_or_insert_with(ScreenshotDialog::default);
    if dialog.pane != Some(size) {
        dialog.pane = Some(size);
    }
}

/// Draws the Screenshot dialog and the result toast (the shell calls it every frame through
/// `crate::menus::windows`). Every control is a focusable widget: Tab walks the dialog, Enter on
/// **Take screenshot** captures, Esc closes.
pub fn window(ctx: &egui::Context, world: &mut World) {
    let Some(mut dialog) = world.get_resource::<ScreenshotDialog>().cloned() else {
        return;
    };
    if dialog.open {
        let mut take_now = false;
        let mut open = true;
        egui::Window::new("Screenshot")
            .default_pos(crate::tool_windows::default_pos(ctx, "Screenshot"))
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                let base = dialog.pane.unwrap_or([1280, 720]);
                ui.horizontal(|ui| {
                    let mut by = dialog.by_multiplier;
                    crate::agent_parity::action_toggle(
                        ui,
                        &crate::agent_parity::SCREENSHOT_BY_MULTIPLIER,
                        &mut by,
                    );
                    dialog.by_multiplier = by;
                });
                if dialog.by_multiplier {
                    ui.horizontal(|ui| {
                        ui.label("Multiplier");
                        ui.add(egui::Slider::new(&mut dialog.multiplier, 1..=MAX_MULTIPLIER).suffix("x"));
                    });
                } else {
                    ui.horizontal(|ui| {
                        ui.label("Width");
                        ui.add(egui::DragValue::new(&mut dialog.exact[0]).range(MIN_SIDE..=MAX_SIDE));
                        ui.label("Height");
                        ui.add(egui::DragValue::new(&mut dialog.exact[1]).range(MIN_SIDE..=MAX_SIDE));
                    });
                }
                let size = if dialog.by_multiplier {
                    ShotSize::Multiplier(dialog.multiplier)
                } else {
                    ShotSize::Exact(dialog.exact)
                };
                match resolve_size(size, base) {
                    Ok([w, h]) => ui.label(format!("Output: {w} x {h} pixels")),
                    Err(err) => ui.colored_label(ui.visuals().error_fg_color, err.message()),
                };
                crate::agent_parity::action_toggle(
                    ui,
                    &crate::agent_parity::SCREENSHOT_HELPERS,
                    &mut dialog.helpers,
                );
                ui.horizontal(|ui| {
                    ui.label("File name");
                    let field =
                        ui.add(egui::TextEdit::singleline(&mut dialog.name).hint_text("screenshot-NNNN.png"));
                    crate::a11y_audit::spoken_only(field, egui::WidgetType::TextEdit, "Screenshot file name");
                });
                ui.label(format!("Saved to the project's {SCREENSHOT_FOLDER} folder"));
                if crate::agent_parity::action_button(ui, &crate::agent_parity::SCREENSHOT_TAKE).clicked() {
                    take_now = true;
                }
            });
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            open = false;
        }
        dialog.open = open && !take_now;
        world.insert_resource(dialog.clone());
        if take_now {
            let base = dialog.pane.unwrap_or([1280, 720]);
            let size = if dialog.by_multiplier {
                ShotSize::Multiplier(dialog.multiplier)
            } else {
                ShotSize::Exact(dialog.exact)
            };
            let name = (!dialog.name.trim().is_empty()).then(|| dialog.name.clone());
            let result = resolve_size(size, base).and_then(|size| {
                let device = crate::agent_ext::viewport_device(world).map_err(|_| ShotError::RenderFailed)?;
                take(&device, world, size, dialog.helpers, name.as_deref())
            });
            let toast = match result {
                Ok(shot) => (
                    format!(
                        "Saved {} ({} x {})",
                        shot.path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                        shot.size[0],
                        shot.size[1]
                    ),
                    shot.path.parent().map(Path::to_path_buf),
                ),
                Err(err) => (format!("Screenshot failed: {}", err.message()), None),
            };
            world.get_resource_or_insert_with(ScreenshotDialog::default).toast = Some(toast);
        }
    }
    let Some((text, folder)) = world
        .get_resource::<ScreenshotDialog>()
        .and_then(|d| d.toast.clone())
    else {
        return;
    };
    let mut dismiss = false;
    egui::Area::new(egui::Id::new("regia_editor::screenshot-toast"))
        .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-16.0, -40.0))
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(text);
                    if let Some(folder) = folder.as_ref()
                        && crate::agent_parity::action_button(
                            ui,
                            &crate::agent_parity::SCREENSHOT_OPEN_FOLDER,
                        )
                        .clicked()
                    {
                        open_folder(folder);
                    }
                    if crate::agent_parity::action_button(ui, &crate::agent_parity::SCREENSHOT_DISMISS)
                        .clicked()
                    {
                        dismiss = true;
                    }
                });
            });
        });
    if dismiss && let Some(mut dialog) = world.get_resource_mut::<ScreenshotDialog>() {
        dialog.toast = None;
    }
}

// ---------------------------------------------------------------------------
// Agent protocol
// ---------------------------------------------------------------------------

/// `editor.screenshot`.
fn screenshot_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    // `close: true` is the window's close button and the toast's Dismiss (ADR-8583): no shot.
    if params.get("close").and_then(Value::as_bool) == Some(true) {
        let others = params
            .as_object()
            .is_some_and(|object| object.keys().any(|key| key != "close"));
        if others {
            return Err(RpcError::invalid_params("`close` takes no other parameters"));
        }
        let closed = world
            .get_resource_mut::<ScreenshotDialog>()
            .is_some_and(|mut dialog| {
                let was = dialog.open || dialog.toast.is_some();
                dialog.open = false;
                dialog.toast = None;
                was
            });
        return Ok(json!({ "closed": closed }));
    }
    // `size: [width, height]` is the same as `width` and `height`.
    let pair = params.get("size").and_then(Value::as_array).cloned();
    let (width, height) = match pair.as_deref() {
        Some([w, h]) if params.get("width").is_none() && params.get("height").is_none() => (Some(w), Some(h)),
        Some(_) => {
            return Err(RpcError::invalid_params(
                "`size` is [width, height], without `width` or `height`",
            ));
        }
        None => (params.get("width"), params.get("height")),
    };
    let size = match (params.get("multiplier"), width, height) {
        (Some(m), None, None) => {
            let m = m
                .as_u64()
                .ok_or_else(|| RpcError::invalid_params("`multiplier` is a whole number 1 to 8"))?;
            ShotSize::Multiplier(u32::try_from(m).map_err(|_| ShotError::BadMultiplier(m).to_rpc())?)
        }
        (None, Some(w), Some(h)) => {
            let side = |v: &Value| v.as_u64().map(|v| u32::try_from(v).unwrap_or(u32::MAX));
            match (side(w), side(h)) {
                (Some(w), Some(h)) => ShotSize::Exact([w, h]),
                _ => return Err(RpcError::invalid_params("`width` and `height` are whole numbers")),
            }
        }
        (None, None, None) => ShotSize::Multiplier(1),
        _ => {
            return Err(RpcError::invalid_params(
                "pass `multiplier`, or `width` and `height`, not both",
            ));
        }
    };
    if let Some(base) = params.get("pane") {
        let pair: Option<Vec<u32>> = base.as_array().map(|a| {
            a.iter()
                .filter_map(Value::as_u64)
                .filter_map(|v| u32::try_from(v).ok())
                .collect()
        });
        match pair.as_deref() {
            Some([w, h]) => note_pane_size(world, [*w, *h]),
            _ => return Err(RpcError::invalid_params("`pane` is [width, height] in pixels")),
        }
    }
    let helpers = params.get("helpers").and_then(Value::as_bool).unwrap_or(false);
    let name = params.get("name").and_then(Value::as_str);
    let size = resolve_size(size, pane_size(world)).map_err(|err| err.to_rpc())?;
    let device = crate::agent_ext::viewport_device(world)?;
    let shot = take(&device, world, size, helpers, name).map_err(|err| err.to_rpc())?;
    let relative = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone())
        .and_then(|root| {
            shot.path
                .strip_prefix(root)
                .ok()
                .map(|p| p.to_string_lossy().replace('\\', "/"))
        })
        .unwrap_or_else(|| shot.path.to_string_lossy().into_owned());
    Ok(json!({
        "path": relative,
        "width": shot.size[0],
        "height": shot.size[1],
        "tiles": shot.tiles,
        "helpers": helpers,
    }))
}

/// The agent method of the screenshot (`book/agents/viewport-tools.md`).
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.screenshot",
            tool: "editor_screenshot",
            description: "File ▸ Screenshot… (ADR-7768): render the main viewport's perspective view at a multiple of the pane size or an explicit size (up to 16384 a side) and write a PNG to the project's Saved/Screenshots folder, atomically.",
            params: &[
                ParamSchema {
                    name: "multiplier",
                    kind: "number",
                    required: false,
                    description: "1 to 8 times the pane size (default 1 when no width/height).",
                },
                ParamSchema {
                    name: "width",
                    kind: "number",
                    required: false,
                    description: "Explicit width, 16 to 16384 (with height; not with multiplier).",
                },
                ParamSchema {
                    name: "height",
                    kind: "number",
                    required: false,
                    description: "Explicit height, 16 to 16384.",
                },
                ParamSchema {
                    name: "size",
                    kind: "array",
                    required: false,
                    description: "[width, height]: the same as width and height.",
                },
                ParamSchema {
                    name: "pane",
                    kind: "array",
                    required: false,
                    description: "[width, height] the multiplier applies to (default: the perspective pane as last drawn, else 1280x720).",
                },
                ParamSchema {
                    name: "helpers",
                    kind: "boolean",
                    required: false,
                    description: "Paint the editor helpers (grid, icons, outline, gizmo) in; default false (the game look).",
                },
                ParamSchema {
                    name: "name",
                    kind: "string",
                    required: false,
                    description: "File name in Saved/Screenshots (.png added); default the next free screenshot-NNNN.png.",
                },
                ParamSchema {
                    name: "close",
                    kind: "boolean",
                    required: false,
                    description: "true closes the Screenshot window and dismisses the result toast instead of taking a shot (alone; `closed` says whether either showed).",
                },
            ],
            result: "{ path (project-relative), width, height, tiles, helpers }, or { closed } with close",
            mutating: true,
            permission: Permission::Assets,
        },
        screenshot_method as Handler,
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_resolve_from_the_multiplier_or_exactly_and_refuse_outside_the_ranges() {
        assert_eq!(resolve_size(ShotSize::Multiplier(1), [640, 360]), Ok([640, 360]));
        assert_eq!(
            resolve_size(ShotSize::Multiplier(8), [640, 360]),
            Ok([5120, 2880])
        );
        assert_eq!(
            resolve_size(ShotSize::Exact([16384, 16]), [1, 1]),
            Ok([16384, 16])
        );
        assert_eq!(
            resolve_size(ShotSize::Multiplier(0), [640, 360]),
            Err(ShotError::BadMultiplier(0))
        );
        assert_eq!(
            resolve_size(ShotSize::Multiplier(9), [640, 360]).map_err(|e| e.code()),
            Err("screenshot_bad_multiplier")
        );
        assert_eq!(
            resolve_size(ShotSize::Multiplier(8), [4000, 360]),
            Err(ShotError::BadSize([32000, 2880]))
        );
        assert_eq!(
            resolve_size(ShotSize::Exact([15, 100]), [1, 1]).map_err(|e| e.code()),
            Err("screenshot_bad_size")
        );
    }

    #[test]
    fn the_tile_plan_covers_the_picture_with_the_fewest_tiles() {
        let one = plan_tiles([2048, 1024], 2048);
        assert_eq!((one.columns, one.rows, one.count()), (1, 1, 1));
        assert_eq!(one.tile_rect(0, 0), [0, 0, 2048, 1024]);
        // A 2x capture of a pane has the same tile count as the 1x one while both fit.
        assert_eq!(
            plan_tiles([1280, 720], 8192).count(),
            plan_tiles([2560, 1440], 8192).count()
        );
        let grid = plan_tiles([16384, 9000], 8192);
        assert_eq!((grid.columns, grid.rows), (2, 2));
        assert_eq!(grid.tile, [8192, 4500]);
        assert_eq!(grid.tile_rect(1, 1), [8192, 4500, 8192, 4500]);
        let uneven = plan_tiles([10000, 100], 4096);
        assert_eq!(uneven.columns, 3);
        assert_eq!(uneven.tile[0], 3334);
        let last = uneven.tile_rect(2, 0);
        assert_eq!(last[0] + last[2], 10000, "the last tile ends at the edge");
    }

    #[test]
    fn file_names_are_plain_and_get_png() {
        assert_eq!(file_name("hero shot"), Ok("hero shot.png".to_owned()));
        assert_eq!(file_name("a.PNG"), Ok("a.PNG".to_owned()));
        for bad in ["", "  ", "../x", "a/b", "a\\b", ".hidden", "c:x"] {
            assert_eq!(
                file_name(bad).map_err(|e| e.code()),
                Err("screenshot_bad_name"),
                "{bad}"
            );
        }
    }

    #[test]
    fn writes_are_atomic_and_names_count_up() {
        let dir = regia_test_utils::unique_temp_dir("screenshot-write");
        assert_eq!(next_name(&dir), "screenshot-0001.png");
        let image = egui::ColorImage::filled([4, 3], egui::Color32::from_rgb(10, 20, 30));
        let bytes = png_bytes(&image).expect("png");
        let path = dir.join(next_name(&dir));
        write_atomic(&path, &bytes).expect("write");
        assert_eq!(std::fs::read(&path).expect("read"), bytes);
        assert_eq!(next_name(&dir), "screenshot-0002.png");
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .expect("dir")
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "no temporary file is left");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Lane v06-editor-undo-agent-routes (ADR-8583, smoke bug 13): the window an agent opened with
    /// `editor.command { run: "file.screenshot" }` closes with `editor.screenshot { close: true }`
    /// (the window's close button and Esc; it also dismisses the result toast), and no shot is
    /// taken.
    #[test]
    fn agent_can_close_the_screenshot_window() {
        let dispatch = |world: &mut World, method: &str, params: Value| {
            let (_, handler) = crate::agent_ext::all_methods()
                .into_iter()
                .find(|(m, _)| m.name == method)
                .expect("registered");
            handler(world, &params)
        };
        let mut world = World::new();
        dispatch(&mut world, "editor.command", json!({ "run": "file.screenshot" })).expect("open");
        assert!(world.resource::<ScreenshotDialog>().open, "the window opened");
        world.resource_mut::<ScreenshotDialog>().toast = Some(("Saved a.png (1 x 1)".into(), None));
        let out = dispatch(&mut world, "editor.screenshot", json!({ "close": true })).expect("close");
        assert_eq!(out["closed"], true, "{out}");
        let dialog = world.resource::<ScreenshotDialog>();
        assert!(!dialog.open, "the window closed");
        assert!(dialog.toast.is_none(), "the toast went too");
        let out = dispatch(&mut world, "editor.screenshot", json!({ "close": true })).expect("again");
        assert_eq!(out["closed"], false, "nothing was open");
        let err = dispatch(
            &mut world,
            "editor.screenshot",
            json!({ "close": true, "multiplier": 2 }),
        )
        .expect_err("close takes nothing else");
        assert_eq!(err.code, regia_agent::protocol::INVALID_PARAMS);
    }
}
