// SPDX-License-Identifier: MPL-2.0
//! What the second set of gameplay data panels shares (ADR-2031, `book/agents/editor-data-panels.md`):
//! the Tables panel ([`crate::tables_panel`]), the Abilities inspector ([`crate::abilities_panel`]), the
//! Montage timeline ([`crate::montage_panel`]) and the Ragdoll editor ([`crate::ragdoll_panel`]).
//!
//! Every panel keeps its model in an edit-world resource. Its buttons and its `editor.*` agent methods call
//! the same functions on that model, so an agent can do everything a person can, and a test can drive a
//! panel headless and read what it would draw.

use std::path::{Component, Path, PathBuf};

use regia_agent::protocol::{self, RpcError};
use serde_json::{Value, json};

/// A failed panel operation: a stable lowercase code and a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanelError {
    /// Stable code (`tables_panel_bad_path`, ...), also the agent's `data.reason`.
    pub code: String,
    /// What went wrong.
    pub message: String,
}

impl PanelError {
    /// An error with `code` and `message`.
    #[must_use]
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_owned(),
            message: message.into(),
        }
    }

    /// As the agent reports it: `invalid_params`, or `not_found` for the `*_not_found` codes, with
    /// `data.reason` and `data.hint`.
    #[must_use]
    pub fn rpc(&self, hint: &str) -> RpcError {
        let (code, regia) = if self.code.ends_with("_not_found") {
            (protocol::NOT_FOUND, "F0003")
        } else {
            (protocol::INVALID_PARAMS, "F0002")
        };
        RpcError {
            code,
            message: self.message.clone(),
            data: Some(json!({ "code": regia, "reason": self.code, "hint": hint })),
        }
    }

    /// The error as `{ code, message }`.
    #[must_use]
    pub fn json(&self) -> Value {
        json!({ "code": self.code, "message": self.message })
    }

    /// Draws the error line: the sentence, with the stable code in its tooltip (bug 20, ADR-8586).
    pub fn draw(&self, ui: &mut egui::Ui) {
        ui.colored_label(egui::Color32::from_rgb(230, 90, 90), sentence(&self.message))
            .on_hover_text(format!("Agent error code: {}", self.code));
    }
}

/// `message` as a sentence for a person: first letter upper case, a closing period (ADR-8586).
#[must_use]
pub fn sentence(message: &str) -> String {
    let message = message.trim();
    let mut chars = message.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    let mut out: String = first.to_uppercase().chain(chars).collect();
    if !out.ends_with(['.', '!', '?', ':']) {
        out.push('.');
    }
    out
}

/// `root/relative` when `relative` is a non-empty relative path with plain components only (no `..`,
/// not absolute) ending in one of `suffixes`, and it passes the shared rule of
/// `regia_core::project_path` (ADR-7388): no drive letter or `:`, no `\`-separated `..`, no device
/// name, and no symlink inside the project leading out.
///
/// # Errors
/// `<prefix>_bad_path` otherwise.
pub fn confined(root: &Path, relative: &str, suffixes: &[&str], prefix: &str) -> Result<PathBuf, PanelError> {
    let plain = !relative.is_empty()
        && Path::new(relative)
            .components()
            .all(|c| matches!(c, Component::Normal(_)));
    let lower = relative.to_ascii_lowercase();
    let bad = |why: String| {
        PanelError::new(
            &format!("{prefix}_bad_path"),
            format!(
                "`{relative}` must be a relative {} path inside the project (no `..`, not absolute){why}",
                suffixes.join(" or ")
            ),
        )
    };
    if !plain || !suffixes.iter().any(|s| lower.ends_with(s)) {
        return Err(bad(String::new()));
    }
    use regia_core::project_path::{ProjectPathError, check_relative, resolve_write};
    match resolve_write(root, relative) {
        Ok(path) => Ok(path),
        // A root that does not exist yet has no symlink under it to lead out through.
        Err(ProjectPathError::Io(_)) => {
            Ok(root.join(check_relative(relative).map_err(|e| bad(format!(": {e}")))?))
        }
        Err(e) => Err(bad(format!(": {e}"))),
    }
}

/// Writes `text` atomically: a temp file beside `path`, then a rename.
///
/// # Errors
/// `<prefix>_io_failed` when a step fails.
pub fn write_atomic(path: &Path, text: &str, prefix: &str) -> Result<(), PanelError> {
    let io = |e: std::io::Error| {
        PanelError::new(&format!("{prefix}_io_failed"), format!("{}: {e}", path.display()))
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(io)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".{}.tmp", std::process::id()));
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, text).map_err(io)?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        io(e)
    })
}

/// A protocol error as a [`PanelError`]: its `data.reason` (else its `data.error_code`, else `fallback`) and
/// its message.
#[must_use]
pub fn from_rpc(error: &RpcError, fallback: &str) -> PanelError {
    let reason = error
        .data
        .as_ref()
        .and_then(|d| d["reason"].as_str().or_else(|| d["error_code"].as_str()))
        .unwrap_or(fallback);
    PanelError::new(reason, error.message.clone())
}

/// A required string parameter.
///
/// # Errors
/// `invalid_params` when it is missing or not a string.
pub fn str_param<'a>(params: &'a Value, key: &str) -> Result<&'a str, RpcError> {
    params
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params(format!("`{key}` must be a string")))
}

/// An optional finite number parameter.
///
/// # Errors
/// `invalid_params` when present and not a finite number.
pub fn opt_f32(params: &Value, key: &str) -> Result<Option<f32>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .map(|f| f as f32)
            .filter(|f| f.is_finite())
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{key}` must be a finite number"))),
    }
}

/// An optional boolean parameter.
///
/// # Errors
/// `invalid_params` when present and not a boolean.
pub fn opt_bool(params: &Value, key: &str) -> Result<Option<bool>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(RpcError::invalid_params(format!("`{key}` must be a boolean"))),
    }
}

/// The panel header: a title and a dim detail line.
pub fn header(ui: &mut egui::Ui, title: &str, detail: &str) {
    ui.horizontal(|ui| {
        ui.strong(title);
        ui.weak(detail);
    });
    ui.separator();
}

#[cfg(test)]
pub(crate) mod test_ui {
    //! Headless drawing for the panel smoke tests.

    use regia_ecs::prelude::World;

    /// Draws `panel` for `frames` frames on a headless egui context; returns the last frame's meshes.
    pub fn draw_frames(
        world: &mut World,
        frames: usize,
        panel: fn(&mut egui::Ui, &mut World),
    ) -> Vec<egui::ClippedPrimitive> {
        let ctx = egui::Context::default();
        let mut shapes = Vec::new();
        for _ in 0..frames {
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(900.0, 700.0),
                )),
                ..Default::default()
            };
            let mut output = ctx.run_ui(raw, |ui| panel(ui, world));
            output.textures_delta.clear();
            shapes = ctx.tessellate(output.shapes, output.pixels_per_point);
        }
        shapes
    }

    /// Whether any vertex of `shapes` has exactly `color`.
    pub fn has_color(shapes: &[egui::ClippedPrimitive], color: egui::Color32) -> bool {
        shapes.iter().any(|p| match &p.primitive {
            egui::epaint::Primitive::Mesh(mesh) => mesh.vertices.iter().any(|v| v.color == color),
            egui::epaint::Primitive::Callback(_) => false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confined_paths_refuse_escapes_and_wrong_suffixes() {
        let root = Path::new("/p");
        assert_eq!(
            confined(root, "assets/a.curves.ron", &[".curves.ron"], "x").expect("ok"),
            Path::new("/p/assets/a.curves.ron")
        );
        for bad in [
            "",
            "../a.curves.ron",
            "/a.curves.ron",
            "a/../../b.curves.ron",
            "a.txt",
        ] {
            let e = confined(root, bad, &[".curves.ron"], "x").expect_err(bad);
            assert_eq!(e.code, "x_bad_path", "{bad}");
        }
    }

    /// ADR-7388 (audit rc1-sec-paths-z): Windows forms are refused on every host, and a symlink in
    /// the project that leads out of it is not followed.
    #[test]
    fn confined_paths_refuse_windows_forms_and_symlinks_out() {
        let base = regia_test_utils::unique_temp_dir("data-panels-confined");
        let root = base.join("project");
        std::fs::create_dir_all(root.join("assets")).expect("root");
        assert_eq!(
            confined(&root, "assets/a.curves.ron", &[".curves.ron"], "x").expect("ok"),
            root.join("assets/a.curves.ron")
        );
        for bad in [
            "a\\..\\..\\b.curves.ron",
            "C:b.curves.ron",
            "C:\\b.curves.ron",
            "\\\\server\\share\\b.curves.ron",
            "nul.curves.ron",
        ] {
            let e = confined(&root, bad, &[".curves.ron"], "x").expect_err(bad);
            assert_eq!(e.code, "x_bad_path", "{bad}");
        }
        #[cfg(unix)]
        {
            std::fs::create_dir_all(base.join("outside")).expect("outside");
            std::os::unix::fs::symlink(base.join("outside"), root.join("assets/linked")).expect("link");
            let e = confined(&root, "assets/linked/b.curves.ron", &[".curves.ron"], "x").expect_err("link");
            assert_eq!(e.code, "x_bad_path");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn errors_map_not_found_codes_to_not_found() {
        let e = PanelError::new("tables_panel_row_not_found", "no row").rpc("hint");
        assert_eq!(e.code, protocol::NOT_FOUND);
        let e = PanelError::new("tables_panel_bad_value", "bad").rpc("hint");
        assert_eq!(e.code, protocol::INVALID_PARAMS);
        assert_eq!(e.data.expect("data")["reason"], "tables_panel_bad_value");
    }

    #[test]
    fn atomic_writes_leave_no_temp_file() {
        let dir = regia_test_utils::unique_temp_dir("data-panels-write");
        let path = dir.join("a/b.txt");
        write_atomic(&path, "one", "x").expect("write");
        write_atomic(&path, "two", "x").expect("rewrite");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "two");
        let names: Vec<_> = std::fs::read_dir(dir.join("a")).expect("dir").collect();
        assert_eq!(names.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
