// SPDX-License-Identifier: MPL-2.0
//! Project Settings ▸ Icon (`docs/specs/19-editor.md` § 2, ADR-7475): pick a PNG and the editor
//! copies it, byte for byte, to `assets/branding/icon.png`, the file `regia package` already ships
//! as the game icon (Linux `.desktop` icon, Windows and Android launcher icons). No manifest key
//! and no content format change: the file location is the setting.
//!
//! Agent: `editor.project_icon { path? }` (absent `path` reads the current icon; `path` is a
//! request path, ADR-1816); `editor.project_icon_info` is the same read for read-only profiles.
//! Errors (`data.reason`): `icon_no_project`, `icon_source_missing`, `icon_not_png`,
//! `icon_write_failed`, `path_outside_project`. Choosing an icon while one exists asks first:
//! the overwrite has no undo.
//!
//! The Project Settings row reads only the icon's 24-byte header, and only when the file's size
//! or modified time changed since the last UI pass ([`icon_for_ui`]).

use std::path::{Path, PathBuf};

use regia_agent::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::viewport_agent::Handler;

/// The project-relative icon file `regia package` reads.
pub const ICON_REL: &str = "assets/branding/icon.png";

/// The eight-byte PNG signature.
const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// Why the icon was not set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IconError {
    /// The lowercase protocol reason.
    pub reason: &'static str,
    /// What to show.
    pub message: String,
}

impl IconError {
    fn new(reason: &'static str, message: impl Into<String>) -> Self {
        Self {
            reason,
            message: message.into(),
        }
    }

    /// The protocol error.
    #[must_use]
    pub fn rpc(&self) -> RpcError {
        let (code, regia) = match self.reason {
            "icon_no_project" | "icon_source_missing" => (NOT_FOUND, "F0003"),
            _ => (INVALID_PARAMS, "F0002"),
        };
        let mut error = RpcError::new(
            code,
            regia,
            self.message.clone(),
            "see book/agents/editor-menus.md",
        );
        if let Some(Value::Object(data)) = error.data.as_mut() {
            data.insert("reason".into(), Value::String(self.reason.to_string()));
        }
        error
    }
}

impl std::fmt::Display for IconError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.reason, self.message)
    }
}

/// The project's icon as it is on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IconInfo {
    /// The icon file (`<root>/assets/branding/icon.png`).
    pub path: PathBuf,
    /// Its size in bytes.
    pub bytes: u64,
    /// Width and height from the PNG header, when it has one.
    pub size: Option<(u32, u32)>,
}

impl IconInfo {
    /// The protocol object.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "icon": ICON_REL,
            "path": self.path.display().to_string(),
            "bytes": self.bytes,
            "width": self.size.map(|s| s.0),
            "height": self.size.map(|s| s.1),
        })
    }
}

/// Width and height of a PNG from its IHDR chunk; `None` when `bytes` is not a PNG.
#[must_use]
pub fn png_size(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 24 || bytes[..8] != PNG_SIGNATURE || &bytes[12..16] != b"IHDR" {
        return None;
    }
    let word = |at: usize| u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
    Some((word(16), word(20)))
}

/// The current icon of the project at `root`, when it has one. Reads the file's size and its
/// first 24 bytes (signature and IHDR), never the whole image.
#[must_use]
pub fn current_icon(root: &Path) -> Option<IconInfo> {
    use std::io::Read as _;
    let path = root.join(ICON_REL);
    let file = std::fs::File::open(&path).ok()?;
    let meta = file.metadata().ok().filter(std::fs::Metadata::is_file)?;
    let mut header = Vec::with_capacity(24);
    file.take(24).read_to_end(&mut header).ok()?;
    Some(IconInfo {
        size: png_size(&header),
        bytes: meta.len(),
        path,
    })
}

/// Copies the PNG `source` to `<root>/assets/branding/icon.png` unchanged (written to a
/// temporary file and renamed, so a reader never sees half an icon).
///
/// # Errors
/// `icon_source_missing`, `icon_not_png` (the bytes do not start with a PNG header) or
/// `icon_write_failed`.
pub fn set_project_icon(root: &Path, source: &Path) -> Result<IconInfo, IconError> {
    let bytes = std::fs::read(source).map_err(|e| {
        IconError::new(
            "icon_source_missing",
            format!("cannot read {}: {e}", source.display()),
        )
    })?;
    let size = png_size(&bytes).ok_or_else(|| {
        IconError::new(
            "icon_not_png",
            format!(
                "{} is not a PNG image (the package icon must be a PNG)",
                source.display()
            ),
        )
    })?;
    let path = root.join(ICON_REL);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| {
            IconError::new(
                "icon_write_failed",
                format!("cannot create {}: {e}", dir.display()),
            )
        })?;
    }
    crate::atomic_write::write_atomic(&path, &bytes).map_err(|e| {
        IconError::new(
            "icon_write_failed",
            format!("cannot write {}: {e}", path.display()),
        )
    })?;
    Ok(IconInfo {
        path,
        bytes: bytes.len() as u64,
        size: Some(size),
    })
}

/// The icon file's identity the row's cache keys on: size and modified time.
type IconStamp = (u64, Option<std::time::SystemTime>);

/// The Project Settings row's last read of the icon.
#[derive(Resource, Debug, Clone, Default)]
pub struct IconRowCache {
    key: Option<(PathBuf, IconStamp)>,
    info: Option<IconInfo>,
}

/// The current icon as the Project Settings row shows it (drawn every UI pass while the panel is
/// open): one `metadata` call per pass, and a header read only when the file's size or modified
/// time changed.
#[must_use]
pub fn icon_for_ui(world: &mut World, root: &Path) -> Option<IconInfo> {
    let path = root.join(ICON_REL);
    let Some(stamp) = std::fs::metadata(&path)
        .ok()
        .filter(std::fs::Metadata::is_file)
        .map(|meta| (meta.len(), meta.modified().ok()))
    else {
        world.insert_resource(IconRowCache::default());
        return None;
    };
    let key = Some((path, stamp));
    let mut cache = world.get_resource_or_insert_with(IconRowCache::default);
    if cache.key != key {
        cache.info = current_icon(root);
        cache.key = key;
    }
    cache.info.clone()
}

/// The status line the picker shows after a choice.
#[derive(Resource, Debug, Clone, Default)]
pub struct IconPickerStatus(pub String);

/// The pending icon overwrite the confirm window asks about (rev-claude-63 finding G: choosing an
/// icon while one exists must not overwrite it silently).
#[derive(Resource, Debug, Clone, Default)]
pub struct IconPickConfirm {
    /// Whether the window is open.
    pub open: bool,
    /// The chosen PNG (applied when the user confirms).
    pub source: PathBuf,
}

/// Applies the picked PNG, or opens the confirm window when an icon exists (never overwrites one
/// silently); the outcome lands in [`IconPickerStatus`] (or the window).
fn apply_or_confirm(world: &mut World, root: &Path, source: PathBuf) {
    if current_icon(root).is_some() {
        world.insert_resource(IconPickConfirm { open: true, source });
    } else {
        let line = match set_project_icon(root, &source) {
            Ok(info) => format!("icon set from {} ({} bytes)", source.display(), info.bytes),
            Err(error) => error.to_string(),
        };
        world.insert_resource(IconPickerStatus(line));
    }
}

/// The confirm window [`apply_or_confirm`] opens over an existing icon.
fn icon_pick_confirm_window(ui: &egui::Ui, world: &mut World, root: &Path) {
    let Some(mut confirm) = world.get_resource::<IconPickConfirm>().cloned() else {
        return;
    };
    if !confirm.open {
        return;
    }
    let mut open = true;
    egui::Window::new("Replace the project icon?")
        .id(egui::Id::new("icon-pick-confirm"))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .show(ui.ctx(), |ui| {
            ui.label(format!(
                "{ICON_REL} already exists. Replacing it overwrites the current icon; there is no undo."
            ));
            ui.horizontal(|ui| {
                if crate::agent_parity::action_button(ui, &crate::agent_parity::ICON_PICK_REPLACE).clicked() {
                    let line = match set_project_icon(root, &confirm.source) {
                        Ok(info) => format!(
                            "icon set from {} ({} bytes)",
                            confirm.source.display(),
                            info.bytes
                        ),
                        Err(error) => error.to_string(),
                    };
                    world.insert_resource(IconPickerStatus(line));
                    confirm.open = false;
                }
                if crate::agent_parity::action_button(ui, &crate::agent_parity::ICON_PICK_KEEP).clicked() {
                    confirm.open = false;
                }
            });
        });
    confirm.open = confirm.open && open;
    world.insert_resource(confirm);
}

/// The Project Settings icon row: the current icon and Choose icon….
pub fn icon_picker_ui(ui: &mut egui::Ui, world: &mut World, root: &Path) {
    ui.separator();
    ui.strong("Icon");
    ui.horizontal(|ui| {
        match icon_for_ui(world, root) {
            Some(info) => {
                let size = info
                    .size
                    .map_or_else(|| "not a PNG".to_string(), |(w, h)| format!("{w}×{h}"));
                ui.label(format!("{ICON_REL} · {size} · {} bytes", info.bytes));
            }
            None => {
                ui.weak(format!("no icon ({ICON_REL}); packages use the default"));
            }
        }
        if crate::agent_parity::action_button(ui, &crate::agent_parity::PROJECT_ICON_CHOOSE).clicked()
            && let Some(source) = crate::file_dialog::FileDialog::new()
                .add_filter("PNG image", &["png"])
                .pick_file()
        {
            apply_or_confirm(world, root, source);
        }
    });
    if let Some(status) = world
        .get_resource::<IconPickerStatus>()
        .filter(|s| !s.0.is_empty())
    {
        ui.weak(status.0.clone());
    }
    icon_pick_confirm_window(ui, world, root);
}

/// The read half of `editor.project_icon`: the project's icon as it is on disk.
fn project_icon_info(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let root = crate::mesh_source::project_root(world)
        .ok_or_else(|| IconError::new("icon_no_project", "open a project first").rpc())?;
    Ok(match current_icon(&root) {
        Some(info) => {
            let mut out = info.to_json();
            out["set"] = json!(true);
            out
        }
        None => json!({ "icon": ICON_REL, "path": null, "set": false }),
    })
}

/// `editor.project_icon`.
fn project_icon(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let root = crate::mesh_source::project_root(world)
        .ok_or_else(|| IconError::new("icon_no_project", "open a project first").rpc())?;
    let source = match params.get("path") {
        None | Some(Value::Null) => None,
        // A request path (ADR-1816): the project, the temp dir, the working directory or an
        // `output_roots` entry, without `..` or a symlink leading out.
        Some(Value::String(path)) => Some(crate::agent_ext::allowed_request_path(world, "path", path)?),
        Some(_) => return Err(RpcError::wrong_type("path", "string")),
    };
    match source {
        None => Ok(match current_icon(&root) {
            Some(info) => {
                let mut out = info.to_json();
                out["set"] = json!(false);
                out
            }
            None => json!({ "icon": ICON_REL, "path": null, "set": false }),
        }),
        Some(source) => {
            let info = set_project_icon(&root, &source).map_err(|e| e.rpc())?;
            let mut out = info.to_json();
            out["set"] = json!(true);
            out["source"] = json!(source.display().to_string());
            Ok(out)
        }
    }
}

/// The methods of this module.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.project_icon",
                tool: "project_icon",
                description: "Project Settings ▸ Icon: copy a PNG unchanged to assets/branding/icon.png, the icon \
                    regia package ships (no manifest key). Without `path`, reads the current icon \
                    (editor.project_icon_info is the same read for read-only profiles). Errors: \
                    icon_no_project, icon_source_missing, icon_not_png, icon_write_failed, \
                    path_outside_project.",
                params: &[ParamSchema {
                    name: "path",
                    kind: "string",
                    required: false,
                    description: "The PNG to use: project-relative, or absolute under the project, the temp \
                        directory, the working directory or an output_roots entry (ADR-1816); absent reads \
                        the current icon.",
                }],
                result: "{ icon, path, bytes, width, height, set, source? }",
                mutating: true,
                permission: Permission::Assets,
            },
            project_icon as Handler,
        ),
        (
            MethodSchema {
                name: "editor.project_icon_info",
                tool: "project_icon_info",
                description: "Reads the project's package icon (assets/branding/icon.png): its \
                    path, size in bytes and PNG dimensions. `set` is false when the project has \
                    no icon (packages then use the default). The read-only half of \
                    editor.project_icon, so a read-only agent profile can see the icon.",
                params: &[],
                result: "{ icon, path, bytes, width, height, set }",
                mutating: false,
                permission: Permission::Read,
            },
            project_icon_info as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Calls a method of this module through the registered extension, as dispatch does.
    fn call(world: &mut World, method: &str, params: Value) -> Result<Value, RpcError> {
        let mut extension = world
            .get_resource::<regia_agent::extension::AgentExtension>()
            .cloned()
            .unwrap_or_default();
        for (schema, handler) in methods() {
            extension.register(schema, handler);
        }
        world.insert_resource(extension);
        let extension = world.resource::<regia_agent::extension::AgentExtension>().clone();
        let (_, handler) = extension
            .method(method)
            .unwrap_or_else(|| panic!("{method} is not registered"));
        handler(world, &params)
    }

    fn world_with_project(root: &Path) -> World {
        let mut world = World::new();
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.to_path_buf()),
        });
        world
    }

    /// A minimal headless egui driver: draws `draw` per frame and clicks a drawn label, the same
    /// shape-walk the editor's other UI tests use.
    struct Ui {
        ctx: egui::Context,
        time: f64,
        texts: Vec<(String, egui::Rect)>,
    }

    impl Ui {
        fn new() -> Self {
            Self {
                ctx: egui::Context::default(),
                time: 0.0,
                texts: Vec::new(),
            }
        }

        fn frame(&mut self, events: Vec<egui::Event>, draw: &mut dyn FnMut(&mut egui::Ui)) {
            fn walk(shape: &egui::Shape, out: &mut Vec<(String, egui::Rect)>) {
                match shape {
                    egui::Shape::Text(text) => out.push((
                        text.galley.text().to_owned(),
                        egui::Rect::from_min_size(text.pos, text.galley.size()),
                    )),
                    egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| walk(s, out)),
                    _ => {}
                }
            }
            self.time += 0.05;
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1200.0, 900.0),
                )),
                events,
                time: Some(self.time),
                ..Default::default()
            };
            let mut output = self.ctx.run_ui(raw, |ui| {
                egui::CentralPanel::default().show(ui, |ui| draw(ui));
            });
            output.textures_delta.clear();
            self.texts.clear();
            for clipped in &output.shapes {
                walk(&clipped.shape, &mut self.texts);
            }
        }

        fn settle(&mut self, draw: &mut dyn FnMut(&mut egui::Ui)) {
            self.frame(vec![], draw);
            self.frame(vec![], draw);
        }

        /// Clicks the widget drawn with `label`.
        fn click(&mut self, label: &str, draw: &mut dyn FnMut(&mut egui::Ui)) {
            self.settle(draw);
            let at = self
                .texts
                .iter()
                .find(|(text, _)| text == label)
                .map(|(_, rect)| rect.center())
                .unwrap_or_else(|| {
                    panic!(
                        "`{label}` is drawn; drawn:\n{}",
                        self.texts
                            .iter()
                            .map(|(t, _)| t.as_str())
                            .collect::<Vec<_>>()
                            .join("\n")
                    )
                });
            let button = |pressed| egui::Event::PointerButton {
                pos: at,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            };
            self.frame(vec![egui::Event::PointerMoved(at)], draw);
            self.frame(vec![button(true)], draw);
            self.frame(vec![button(false)], draw);
            self.frame(vec![], draw);
        }
    }

    /// rev-claude-63 finding G: the picker must not overwrite an existing icon silently. With one
    /// set, choosing a file opens a confirm: "Replace icon" overwrites, "Keep current" changes
    /// nothing.
    #[test]
    fn replacing_an_existing_icon_asks_first() {
        let root = regia_test_utils::unique_temp_dir("project_icon_confirm");
        std::fs::create_dir_all(root.join("assets/branding")).expect("branding");
        std::fs::write(root.join(ICON_REL), png_of(16)).expect("existing icon");
        let picked = root.join("picked.png");
        std::fs::write(&picked, png_of(64)).expect("picked");
        let existing = std::fs::read(root.join(ICON_REL)).expect("existing");

        // Keep current: the icon is untouched and the window closes.
        let mut world = world_with_project(&root);
        world.insert_resource(IconPickConfirm {
            open: true,
            source: picked.clone(),
        });
        let mut ui = Ui::new();
        let mut draw = |ui: &mut egui::Ui| icon_picker_ui(ui, &mut world, &root);
        ui.settle(&mut draw);
        ui.click("Keep current", &mut draw);
        assert_eq!(
            std::fs::read(root.join(ICON_REL)).expect("kept"),
            existing,
            "Keep current changes nothing"
        );
        assert!(!world.resource::<IconPickConfirm>().open, "the window closed");

        // Replace icon: the picked PNG is copied, byte for byte.
        let mut world = world_with_project(&root);
        world.insert_resource(IconPickConfirm {
            open: true,
            source: picked.clone(),
        });
        let mut ui = Ui::new();
        let mut draw = |ui: &mut egui::Ui| icon_picker_ui(ui, &mut world, &root);
        ui.settle(&mut draw);
        ui.click("Replace icon", &mut draw);
        assert_eq!(
            std::fs::read(root.join(ICON_REL)).expect("replaced"),
            std::fs::read(&picked).expect("picked"),
            "Replace icon copies the picked PNG"
        );
        assert!(!world.resource::<IconPickConfirm>().open, "the window closed");
        std::fs::remove_dir_all(&root).ok();
    }

    /// Review rev63-followups-verify (finding G): the step the file dialog feeds. A PNG chosen
    /// while an icon exists opens the confirm and writes nothing; with no icon there is nothing to
    /// overwrite, so it applies at once.
    #[test]
    fn choosing_a_png_over_an_existing_icon_opens_the_confirm_and_writes_nothing() {
        let root = regia_test_utils::unique_temp_dir("project_icon_pick");
        std::fs::create_dir_all(root.join("assets/branding")).expect("branding");
        std::fs::write(root.join(ICON_REL), png_of(16)).expect("existing icon");
        let picked = root.join("picked.png");
        std::fs::write(&picked, png_of(64)).expect("picked");
        let mut world = world_with_project(&root);

        apply_or_confirm(&mut world, &root, picked.clone());
        assert_eq!(
            std::fs::read(root.join(ICON_REL)).expect("icon"),
            png_of(16),
            "the existing icon is not overwritten before the user confirms"
        );
        let confirm = world.resource::<IconPickConfirm>();
        assert!(confirm.open, "the confirm window opens");
        assert_eq!(confirm.source, picked);

        std::fs::remove_file(root.join(ICON_REL)).expect("remove icon");
        let mut world = world_with_project(&root);
        apply_or_confirm(&mut world, &root, picked.clone());
        assert_eq!(
            std::fs::read(root.join(ICON_REL)).expect("icon set"),
            png_of(64),
            "with no icon the choice applies at once"
        );
        assert!(
            world.get_resource::<IconPickConfirm>().is_none(),
            "no confirm without an icon"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// rev-claude-63 finding G: reading the current icon must not need the Assets permission, so
    /// a read-only agent profile can see it: `editor.project_icon_info` is the read-only half of
    /// `editor.project_icon`.
    #[test]
    fn project_icon_info_reads_the_icon_and_is_the_read_only_method() {
        let root = regia_test_utils::unique_temp_dir("project_icon_info");
        std::fs::create_dir_all(&root).expect("root");
        let mut world = world_with_project(&root);
        let empty = call(&mut world, "editor.project_icon_info", json!({})).expect("info");
        assert_eq!(empty["set"], json!(false));
        std::fs::create_dir_all(root.join("assets/branding")).expect("branding");
        std::fs::write(root.join(ICON_REL), png_of(48)).expect("icon");
        let info = call(&mut world, "editor.project_icon_info", json!({})).expect("info");
        assert_eq!(info["set"], json!(true));
        assert_eq!(info["width"], json!(48));
        assert_eq!(info["icon"], json!(ICON_REL));
        let (schema, _) = methods()
            .into_iter()
            .find(|(schema, _)| schema.name == "editor.project_icon_info")
            .expect("registered");
        assert!(!schema.mutating, "reading the icon changes nothing");
        assert_eq!(schema.permission, Permission::Read);
        std::fs::remove_dir_all(&root).ok();
    }

    fn tiny_png() -> Vec<u8> {
        let mut bytes = PNG_SIGNATURE.to_vec();
        bytes.extend_from_slice(&13u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&64u32.to_be_bytes());
        bytes.extend_from_slice(&32u32.to_be_bytes());
        bytes.extend_from_slice(&[8, 6, 0, 0, 0, 1, 2, 3, 4]);
        bytes
    }

    fn png_of(width: u32) -> Vec<u8> {
        let mut bytes = tiny_png();
        bytes[16..20].copy_from_slice(&width.to_be_bytes());
        bytes
    }

    fn set_mtime(path: &Path, time: std::time::SystemTime) {
        std::fs::File::options()
            .write(true)
            .open(path)
            .and_then(|file| file.set_modified(time))
            .expect("set mtime");
    }

    /// Review w8 finding 6: the Project Settings row runs every UI pass (twice per frame) while
    /// the panel is open. It reads the icon file once and again only when its size or modified
    /// time changes.
    #[test]
    fn the_settings_row_reads_the_icon_once_until_it_changes() {
        let root = regia_test_utils::unique_temp_dir("project_icon_cache");
        let icon = root.join(ICON_REL);
        std::fs::create_dir_all(icon.parent().expect("dir")).expect("dir");
        let stamp = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        std::fs::write(&icon, png_of(16)).expect("icon");
        set_mtime(&icon, stamp);
        let mut world = World::new();
        assert_eq!(
            icon_for_ui(&mut world, &root).and_then(|i| i.size),
            Some((16, 32))
        );
        // Same length, same modified time: the row does not read the file again.
        std::fs::write(&icon, png_of(48)).expect("rewrite");
        set_mtime(&icon, stamp);
        assert_eq!(
            icon_for_ui(&mut world, &root).and_then(|i| i.size),
            Some((16, 32)),
            "an unchanged file is not read again"
        );
        set_mtime(&icon, stamp + std::time::Duration::from_secs(5));
        assert_eq!(
            icon_for_ui(&mut world, &root).and_then(|i| i.size),
            Some((48, 32)),
            "a changed file is read again"
        );
        std::fs::remove_file(&icon).expect("remove");
        assert_eq!(icon_for_ui(&mut world, &root), None, "a removed icon is gone");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_icon_is_copied_byte_for_byte_and_non_png_is_refused() {
        let root = regia_test_utils::unique_temp_dir("project_icon");
        std::fs::create_dir_all(&root).expect("root");
        let source = root.join("pick.png");
        let png = tiny_png();
        std::fs::write(&source, &png).expect("png");
        let info = set_project_icon(&root, &source).expect("set");
        assert_eq!(info.size, Some((64, 32)));
        assert_eq!(std::fs::read(root.join(ICON_REL)).expect("icon"), png);
        assert_eq!(current_icon(&root).map(|i| i.bytes), Some(png.len() as u64));

        let text = root.join("note.png");
        std::fs::write(&text, b"not an image").expect("text");
        let err = set_project_icon(&root, &text).expect_err("not png");
        assert_eq!(err.reason, "icon_not_png");
        assert_eq!(
            std::fs::read(root.join(ICON_REL)).expect("kept"),
            png,
            "a refusal keeps the icon"
        );
        let err = set_project_icon(&root, &root.join("missing.png")).expect_err("missing");
        assert_eq!(err.reason, "icon_source_missing");
        std::fs::remove_dir_all(&root).ok();
    }
}
