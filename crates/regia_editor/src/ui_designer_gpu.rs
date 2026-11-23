// SPDX-License-Identifier: MPL-2.0
//! The UI designer's themed preview (ADR-2715, `docs/specs/81-ui-designer.md` § 4.2): the same
//! scratch document the schematic preview lays out (samples applied, repeats expanded) is drawn by
//! the runtime UI renderer with the project's theme into an offscreen texture and read back, so
//! the designer shows real pixels: theme colors, borders, radii, text, images. Only the public
//! `regia_ui` render-out API is used (`begin_render_out`, `SharedUiRenderer`, `OffscreenTarget`);
//! the live editor UI is never touched.
//!
//! The device is the editor's `RenderDevice` when the world has one, else a headless device made
//! once and kept in [`DesignerGpu`] (lavapipe on a machine without a GPU).

use std::path::{Path, PathBuf};

use regia_app::prelude::*;
use regia_render::RenderDevice;
use regia_ui::authoring::UiLibrary;
use regia_ui::{
    ActiveTheme, Localization, OffscreenTarget, SharedUiRenderer, ThemeResolver, UiFile, UiOutput, UiThemes,
};

use crate::ui_designer::DesignerError;
use crate::ui_designer_preview::{PreviewSettings, expanded_document, preview_app};

/// Frames the scratch app runs before it draws (spawn, theme resolve, layout).
pub const THEMED_PREVIEW_FRAMES: usize = 3;

/// A headless GPU device the designer made for itself (an editor world without a `RenderDevice`).
#[derive(Resource, Clone)]
pub struct DesignerGpu(pub RenderDevice);

/// Where the themed preview's themes come from.
#[derive(Debug, Clone, Default)]
pub struct ThemeChoice {
    /// The themes to start from (the editor's [`UiThemes`]); `None`: the built-ins.
    pub themes: Option<UiThemes>,
    /// The selection to start from (the editor's [`ActiveTheme`]); `None`: the default.
    pub active: Option<ActiveTheme>,
    /// `.theme.ron` files loaded over them (the project's `assets/ui/themes`).
    pub files: Vec<PathBuf>,
    /// The theme to show; `None` keeps the active one.
    pub theme: Option<String>,
    /// The project whose source pictures the document's `image` and `mask` asset paths name
    /// (ADR-3148); `None`: they draw the runtime fallback.
    pub project: Option<PathBuf>,
}

/// A rendered themed preview.
#[derive(Debug, Clone, PartialEq)]
pub struct ThemedFrame {
    /// Width (px).
    pub width: u32,
    /// Height (px).
    pub height: u32,
    /// Tightly packed sRGB RGBA8, row by row from the top.
    pub rgba: Vec<u8>,
    /// The theme it was drawn with.
    pub theme: String,
    /// Theme files that did not load (`path: message`); the preview still draws.
    pub theme_errors: Vec<String>,
}

impl ThemedFrame {
    /// Pixels that differ from the top-left pixel (the backdrop): 0 means nothing was drawn.
    pub fn drawn_pixels(&self) -> usize {
        let Some(first) = self.rgba.first_chunk::<4>() else {
            return 0;
        };
        let (pixels, _) = self.rgba.as_chunks::<4>();
        pixels.iter().filter(|p| *p != first).count()
    }

    /// The frame as an egui image (for the panel and PNG encoding).
    pub fn to_color_image(&self) -> egui::ColorImage {
        egui::ColorImage::from_rgba_unmultiplied([self.width as usize, self.height as usize], &self.rgba)
    }
}

/// `<project>/assets/ui/themes/*.theme.ron`, sorted by name.
pub fn project_theme_files(project_root: &Path) -> Vec<PathBuf> {
    let dir = project_root.join("assets").join("ui").join("themes");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.to_str().is_some_and(|s| s.ends_with(".theme.ron")))
        .collect();
    files.sort();
    files
}

/// The theme names the themed preview can draw with (the designer's theme picker, ADR-7677): the
/// editor's themes (or the built-ins) plus the project's `.theme.ron` files, loaded the way
/// [`themed_preview`] loads them, sorted. Files that do not load are left out.
#[must_use]
pub fn theme_names(choice: &ThemeChoice) -> Vec<String> {
    let mut world = World::new();
    world.insert_resource(choice.themes.clone().unwrap_or_default());
    let mut pending: Vec<&PathBuf> = choice.files.iter().collect();
    for _ in 0..2 {
        pending.retain(|path| regia_ui::theme_agent::load_theme_file(&mut world, path).is_err());
        if pending.is_empty() {
            break;
        }
    }
    let mut names: Vec<String> = world
        .resource::<UiThemes>()
        .set
        .names()
        .map(str::to_owned)
        .collect();
    names.sort();
    names
}

/// The device the themed preview draws with: the world's, else one made once and kept. A headless
/// editor without an adapter (`--no-gpu`) gets `no_gpu` with its reason and no device (ADR-5180).
pub fn preview_device(world: &mut World) -> Result<RenderDevice, DesignerError> {
    if let Some(device) = world.get_resource::<RenderDevice>() {
        return Ok(device.clone());
    }
    if let Some(gpu) = world.get_resource::<DesignerGpu>() {
        return Ok(gpu.0.clone());
    }
    if let Some(err) = crate::headless::no_adapter_error(world) {
        return Err(DesignerError::new("no_gpu", err.message));
    }
    let device = crate::render::headless_device()
        .map_err(|e| DesignerError::new("no_gpu", format!("no GPU for the themed preview: {e}")))?;
    world.insert_resource(DesignerGpu(device.clone()));
    Ok(device)
}

/// Draws the document at `settings` with the runtime UI renderer and the chosen theme, offscreen,
/// and reads the pixels back.
pub fn themed_preview(
    file: &UiFile,
    settings: &PreviewSettings,
    localization: Option<Localization>,
    choice: &ThemeChoice,
    device: &RenderDevice,
) -> Result<ThemedFrame, DesignerError> {
    let _span = tracing::info_span!("ui_designer_themed_preview").entered();
    settings.validate()?;
    let mut document = expanded_document(file, settings);
    // Pictures named by asset path come from the project's source files (ADR-3148).
    let mut cache = crate::ui_designer_images::PictureCache::default();
    let uploads = match choice.project.as_deref() {
        Some(root) => crate::ui_designer_images::substitute_pictures(&mut document, Some((root, &mut cache))),
        None => {
            crate::ui_designer_images::substitute_pictures_without_project(&mut document);
            Vec::new()
        }
    };
    let mut app = preview_app(settings, localization)?;
    let world = app.world_mut();
    if let Some(shared) = world.get_resource::<SharedUiRenderer>() {
        regia_ui::ui_image_assets::queue_builtin_pictures(shared);
        let mut renderer = shared.lock();
        for (id, width, height, rgba) in uploads {
            renderer.queue_image_rgba(id, width, height, rgba);
        }
    }

    // Themes: the editor's, then the project files (a child before its parent fails once and
    // loads on the second round).
    if let Some(themes) = &choice.themes {
        world.insert_resource(themes.clone());
    }
    let mut theme_errors = Vec::new();
    let mut pending: Vec<&PathBuf> = choice.files.iter().collect();
    for round in 0..2 {
        let mut failed = Vec::new();
        for path in pending {
            if let Err(issue) = regia_ui::theme_agent::load_theme_file(world, path) {
                if round == 1 {
                    theme_errors.push(format!("{}: {}", path.display(), issue.message));
                }
                failed.push(path);
            }
        }
        pending = failed;
        if pending.is_empty() {
            break;
        }
    }
    let mut active = choice.active.clone().unwrap_or_default();
    // The caller's theme, else the document's view-state pick (ADR-7677), else the active one.
    if let Some(name) = choice.theme.as_ref().or(settings.theme.as_ref()) {
        if world.resource::<UiThemes>().set.get(name).is_none() {
            let known: Vec<&str> = world.resource::<UiThemes>().set.names().collect();
            return Err(DesignerError::new(
                "unknown_theme",
                format!("no theme {name:?}; loaded: {}", known.join(", ")),
            ));
        }
        name.clone_into(&mut active.name);
    }
    // A still frame: no cross-fade from the previous theme.
    active.reduced_motion = true;
    let theme = active.name.clone();
    world.insert_resource(active);

    const NAME: &str = "__ui_designer_themed";
    world
        .resource_mut::<UiLibrary>()
        .templates
        .insert(NAME.to_owned(), document);
    regia_ui::authoring::spawn_ui(world, NAME).map_err(|e| DesignerError::new("parse", e.to_string()))?;
    for _ in 0..THEMED_PREVIEW_FRAMES {
        app.update();
    }

    let width = settings.size[0].round().max(1.0) as u32;
    let height = settings.size[1].round().max(1.0) as u32;
    let world = app.world_mut();
    let [l, t, r, b] = settings.safe_area;
    world.get_resource_or_insert_with(UiOutput::default).safe_area = Some(regia_platform::SafeArea {
        left: l as u32,
        top: t as u32,
        right: r as u32,
        bottom: b as u32,
    });
    let spec = regia_render::RenderOutSpec::new(width, height, regia_render::HudLayout::Auto);
    regia_ui::begin_render_out(world, &spec);
    let backdrop = world
        .get_resource::<ThemeResolver>()
        .and_then(|r| r.resolved_theme())
        .and_then(|t| t.color("color.bg"))
        .map_or([0.0, 0.0, 0.0, 1.0], |c| c.0.map(f64::from));
    let shared = world.resource::<SharedUiRenderer>().clone();
    let target = OffscreenTarget::new(device, width, height);
    target.clear(device, backdrop);
    target.draw_ui(device, &mut shared.lock());
    let rgba = target
        .read(device)
        .map_err(|e| DesignerError::new("render_failed", format!("offscreen readback failed: {e}")))?;
    Ok(ThemedFrame {
        width,
        height,
        rgba,
        theme,
        theme_errors,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_ui::{UiMigrators, parse_ui_ron};

    const DOC: &str = r#"(version: 6, root: (
        style: (direction: column, size: (Percent(100), Percent(100))),
        children: [
            (name: "title", text: (spans: ["Inventory"]), style: (size: (300, 40))),
            (name: "buy", widget: button, text: (spans: ["Buy"]), style: (size: (160, 48))),
            (name: "feed", widget: list(0, 32.0), style: (size: (300, 160)), clip: true,
                repeat: (source: "Vm.items", key: "id"),
                children: [(name: "row", text: (spans: ["-"]), bindings: [(source: ".name")])]),
        ],
    ))"#;

    /// On a machine with a Vulkan device (lavapipe counts) the themed preview has the requested
    /// size and draws something over the backdrop. Without one it reports `no_gpu` and passes.
    #[test]
    fn themed_preview_has_the_requested_size_and_content() {
        let device = match crate::render::headless_device() {
            Ok(device) => device,
            Err(e) => {
                eprintln!("skipped: no GPU device ({e})");
                return;
            }
        };
        let file = parse_ui_ron(DOC, "t.ui.ron", &UiMigrators::default()).expect("parse");
        let settings = PreviewSettings {
            size: [640.0, 360.0],
            safe_area: [0.0; 4],
            ..PreviewSettings::default()
        };
        let frame = themed_preview(&file, &settings, None, &ThemeChoice::default(), &device).expect("render");
        assert_eq!((frame.width, frame.height), (640, 360));
        assert_eq!(frame.rgba.len(), 640 * 360 * 4);
        let drawn = frame.drawn_pixels();
        assert!(drawn > 500, "only {drawn} pixels differ from the backdrop");
        assert!(drawn < 640 * 360, "the backdrop shows somewhere");
        assert!(frame.theme_errors.is_empty(), "{:?}", frame.theme_errors);

        // Another size gives another texture; an empty document draws only the backdrop.
        let small = PreviewSettings {
            size: [200.0, 120.0],
            safe_area: [0.0; 4],
            ..PreviewSettings::default()
        };
        let empty =
            parse_ui_ron("(version: 6, root: ())", "e.ui.ron", &UiMigrators::default()).expect("parse");
        let blank = themed_preview(&empty, &small, None, &ThemeChoice::default(), &device).expect("render");
        assert_eq!((blank.width, blank.height), (200, 120));
        assert_eq!(blank.drawn_pixels(), 0, "nothing to draw over the backdrop");
    }

    #[test]
    fn an_unknown_theme_is_an_error() {
        let device = match crate::render::headless_device() {
            Ok(device) => device,
            Err(e) => {
                eprintln!("skipped: no GPU device ({e})");
                return;
            }
        };
        let file = parse_ui_ron(DOC, "t.ui.ron", &UiMigrators::default()).expect("parse");
        let choice = ThemeChoice {
            theme: Some("no-such-theme".to_owned()),
            ..ThemeChoice::default()
        };
        let err =
            themed_preview(&file, &PreviewSettings::default(), None, &choice, &device).expect_err("unknown");
        assert_eq!(err.code, "unknown_theme");
    }

    #[test]
    fn project_theme_files_are_the_theme_ron_files_sorted() {
        let root = regia_test_utils::unique_temp_dir("designer-themes");
        let dir = root.join("assets").join("ui").join("themes");
        std::fs::create_dir_all(&dir).expect("dir");
        for name in ["b.theme.ron", "a.theme.ron", "notes.txt"] {
            std::fs::write(dir.join(name), "").expect("write");
        }
        let files = project_theme_files(&root);
        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap_or_default().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["a.theme.ron", "b.theme.ron"]);
        assert!(project_theme_files(&root.join("missing")).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }
}
