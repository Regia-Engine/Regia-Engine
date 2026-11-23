// SPDX-License-Identifier: MPL-2.0
//! The editor's app identity on the desktop: its window and taskbar icon, and the id the
//! launcher entries match (ADR-20000).
//!
//! The icon set lives in `crates/regia_editor/assets/icon/` and is drawn from one SVG by
//! `cargo xtask editor-icons`. The window gets the 128-pixel PNG on Linux X11 and Windows, and
//! Windows' taskbar the 256-pixel one; on Wayland the compositor and the dock take the icon from
//! the desktop entry whose name equals [`APP_ID`] (`regia editor install-launcher` installs it),
//! and macOS takes it from the app bundle. The exe file's own icon on Windows comes from the
//! resource the build script embeds.

/// The Wayland app_id and the X11 `WM_CLASS` of the editor's main window. The Linux desktop
/// entry is `<APP_ID>.desktop` with `StartupWMClass=<APP_ID>`, so docks match the window to it.
/// It also matches window rules written as `regia\..*`.
pub const APP_ID: &str = "regia.editor";

/// The icon theme name of the editor (`Icon=` of the desktop entry, the hicolor file stem).
pub const ICON_NAME: &str = "regia-editor";

/// The 128-pixel icon PNG (the window icon).
pub const ICON_128_PNG: &[u8] = include_bytes!("../assets/icon/regia-editor-128.png");

/// The 256-pixel icon PNG (Windows' taskbar icon).
pub const ICON_256_PNG: &[u8] = include_bytes!("../assets/icon/regia-editor-256.png");

/// A decoded icon: straight-alpha RGBA rows, top to bottom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IconRgba {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// `width * height * 4` bytes.
    pub rgba: Vec<u8>,
}

/// Decodes an 8-bit RGBA PNG of the icon set. `None` for any other layout or a broken file
/// (the window then simply has no icon of its own).
#[must_use]
pub fn decode(png_bytes: &[u8]) -> Option<IconRgba> {
    let decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
    let mut reader = decoder.read_info().ok()?;
    let mut buffer = vec![0; reader.output_buffer_size()?];
    let frame = reader.next_frame(&mut buffer).ok()?;
    if frame.color_type != png::ColorType::Rgba || frame.bit_depth != png::BitDepth::Eight {
        return None;
    }
    buffer.truncate(frame.buffer_size());
    Some(IconRgba {
        width: frame.width,
        height: frame.height,
        rgba: buffer,
    })
}

fn winit_icon(png_bytes: &[u8]) -> Option<winit::window::Icon> {
    let icon = decode(png_bytes)?;
    winit::window::Icon::from_rgba(icon.rgba, icon.width, icon.height).ok()
}

/// The main window's icon (the 128-pixel image).
#[must_use]
pub fn window_icon() -> Option<winit::window::Icon> {
    winit_icon(ICON_128_PNG)
}

/// Windows' taskbar and Alt-Tab icon (the 256-pixel image).
#[must_use]
pub fn taskbar_icon() -> Option<winit::window::Icon> {
    winit_icon(ICON_256_PNG)
}

/// The icon of the editor's other OS windows (panels torn out of the main window). Decoded once:
/// the viewport builders that carry it are rebuilt every frame.
#[must_use]
pub fn egui_icon() -> Option<std::sync::Arc<egui::IconData>> {
    static ICON: std::sync::OnceLock<Option<std::sync::Arc<egui::IconData>>> = std::sync::OnceLock::new();
    ICON.get_or_init(|| {
        let icon = decode(ICON_128_PNG)?;
        Some(std::sync::Arc::new(egui::IconData {
            rgba: icon.rgba,
            width: icon.width,
            height: icon.height,
        }))
    })
    .clone()
}

/// `attributes` with the editor's app id, window icon and (on Windows) taskbar icon.
#[must_use]
pub fn with_app_identity(attributes: winit::window::WindowAttributes) -> winit::window::WindowAttributes {
    platform_identity(attributes.with_window_icon(window_icon()))
}

/// Linux: the same name sets the Wayland app_id and the X11 `WM_CLASS` (class and instance).
#[cfg(target_os = "linux")]
fn platform_identity(attributes: winit::window::WindowAttributes) -> winit::window::WindowAttributes {
    use winit::platform::wayland::WindowAttributesExtWayland as _;
    attributes.with_name(APP_ID, APP_ID)
}

/// Windows: the taskbar and Alt-Tab take the larger icon.
#[cfg(windows)]
fn platform_identity(attributes: winit::window::WindowAttributes) -> winit::window::WindowAttributes {
    use winit::platform::windows::WindowAttributesExtWindows as _;
    attributes.with_taskbar_icon(taskbar_icon())
}

/// macOS takes the icon and the name from the app bundle.
#[cfg(not(any(target_os = "linux", windows)))]
fn platform_identity(attributes: winit::window::WindowAttributes) -> winit::window::WindowAttributes {
    attributes
}

/// The project a launcher hands the editor as a bare path (`%F` of the Linux desktop entry, a
/// file dropped on the app): a folder is the project itself; a file opens the nearest folder
/// above it that holds a `Regia.toml`, else the file's own folder. `None` when the path does
/// not exist.
#[must_use]
pub fn project_from_open_arg(path: &std::path::Path) -> Option<std::path::PathBuf> {
    if path.is_dir() {
        return Some(path.to_path_buf());
    }
    if !path.is_file() {
        return None;
    }
    let parent = path.parent()?;
    parent
        .ancestors()
        .find(|dir| dir.join("Regia.toml").is_file() || dir.join("Ferrite.toml").is_file())
        .or(Some(parent))
        .map(std::path::Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_icons_decode_at_their_sizes() {
        let small = decode(ICON_128_PNG).expect("128 decodes");
        assert_eq!(
            (small.width, small.height, small.rgba.len()),
            (128, 128, 128 * 128 * 4)
        );
        let big = decode(ICON_256_PNG).expect("256 decodes");
        assert_eq!((big.width, big.height), (256, 256));
        assert!(window_icon().is_some() && taskbar_icon().is_some() && egui_icon().is_some());
    }

    #[test]
    fn icon_is_opaque_in_the_middle_and_clear_at_the_corner() {
        let icon = decode(ICON_128_PNG).expect("decodes");
        let alpha = |x: usize, y: usize| icon.rgba[(y * 128 + x) * 4 + 3];
        assert_eq!(alpha(0, 0), 0, "the tile's corner is rounded off");
        assert_eq!(alpha(64, 64), 255, "the tile is opaque");
    }

    #[test]
    fn decode_rejects_garbage() {
        assert!(decode(b"not a png").is_none());
    }

    #[test]
    fn app_id_matches_the_owner_window_rule_and_desktop_entry() {
        assert!(APP_ID.starts_with("regia."), "window rules match regia\\..*");
        assert!(
            APP_ID
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_'),
            "a valid desktop file id"
        );
    }

    #[test]
    fn open_args_resolve_to_a_project_folder() {
        let root = regia_test_utils::unique_temp_dir("editor-open-arg");
        let level_dir = root.join("assets/levels");
        std::fs::create_dir_all(&level_dir).expect("dirs");
        std::fs::write(root.join("Regia.toml"), "[project]\nname = \"t\"\n").expect("manifest");
        std::fs::write(level_dir.join("a.level.ron"), "()").expect("level");
        assert_eq!(project_from_open_arg(&root), Some(root.clone()));
        assert_eq!(
            project_from_open_arg(&root.join("Regia.toml")),
            Some(root.clone())
        );
        assert_eq!(
            project_from_open_arg(&level_dir.join("a.level.ron")),
            Some(root.clone())
        );
        assert_eq!(project_from_open_arg(&root.join("missing")), None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
