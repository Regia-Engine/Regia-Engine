// SPDX-License-Identifier: MPL-2.0
//! Project Settings → XR (spec 58 § 6): edits `config/xr.ron` (backend, tracking space, render
//! scale, mirror, comfort), and the XR status in the play toolbar. The `Simulator` backend or the
//! page's toggle needs no headset: play runs the scripted runtime (`crate::xr_simulator`).
//! `editor.xr_settings` is the same page for an agent; both read the file, so neither holds a copy
//! the other can miss.

use std::path::Path;

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use regia_xr::comfort::DominantHand;
use regia_xr::config::{MAX_RETRY_FRAMES, RENDER_SCALE_RANGE};
use regia_xr::{Foveation, Mirror, TrackingSpace, XR_CONFIG_PATH, XrBackendKind, XrConfig, XrState};
use serde_json::{Value, json};

use crate::viewport_agent::Handler;

/// The XR page's state.
#[derive(Debug, Default)]
pub struct XrSettings {
    config: Option<XrConfig>,
    dirty: bool,
    /// The status line for the last action.
    pub toast: String,
    /// The simulator toggle, shown here and kept in [`crate::xr_simulator::XrSimulator`].
    pub simulator: bool,
}

/// Reads `<root>/config/xr.ron`; an absent file is the default configuration.
pub(crate) fn read_config(root: &Path) -> Result<XrConfig, String> {
    let path = root.join(XR_CONFIG_PATH);
    match std::fs::read_to_string(&path) {
        Ok(text) => XrConfig::from_ron(&text).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(XrConfig::default()),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

/// Validates `config` and writes it through a temporary file, so a reader never sees half of it.
fn write_config(root: &Path, config: &XrConfig) -> Result<(), String> {
    config.validate().map_err(|e| e.to_string())?;
    let text = config.to_ron().map_err(|e| e.to_string())?;
    let path = root.join(XR_CONFIG_PATH);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let temporary = path.with_extension("ron.tmp");
    std::fs::write(&temporary, text).map_err(|e| format!("cannot write {}: {e}", temporary.display()))?;
    std::fs::rename(&temporary, &path).map_err(|e| format!("cannot replace {}: {e}", path.display()))
}

impl XrSettings {
    /// Loads the project's file.
    ///
    /// # Errors
    ///
    /// When it cannot be read or parsed; the message names the field.
    pub fn load(&mut self, root: &Path) -> Result<(), String> {
        self.config = Some(read_config(root)?);
        self.dirty = false;
        Ok(())
    }

    /// The loaded configuration.
    #[must_use]
    pub fn config(&self) -> Option<&XrConfig> {
        self.config.as_ref()
    }

    /// Whether unsaved edits exist.
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Changes the loaded configuration.
    pub fn edit(&mut self, change: impl FnOnce(&mut XrConfig)) {
        if let Some(config) = self.config.as_mut() {
            change(config);
            self.dirty = true;
        }
    }

    /// Writes the file.
    ///
    /// # Errors
    ///
    /// When a value is out of range or the file cannot be written.
    pub fn save(&mut self, root: &Path) -> Result<(), String> {
        let Some(config) = self.config.as_ref() else {
            return Err("nothing loaded".to_string());
        };
        write_config(root, config)?;
        self.dirty = false;
        self.toast = format!("saved {XR_CONFIG_PATH}");
        Ok(())
    }
}

fn combo<T: Copy + PartialEq>(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut T,
    all: &[T],
    name: fn(T) -> &'static str,
) -> bool {
    let mut changed = false;
    egui::ComboBox::from_label(label)
        .selected_text(name(*value))
        .show_ui(ui, |ui| {
            for option in all {
                changed |= ui.selectable_value(value, *option, name(*option)).changed();
            }
        });
    changed
}

/// The XR section of Project Settings.
#[tracing::instrument(target = "regia_editor", skip_all, name = "xr_settings_section")]
pub fn xr_settings_section(ui: &mut egui::Ui, state: &mut XrSettings, root: &Path) {
    if state.config.is_none()
        && let Err(err) = state.load(root)
    {
        ui.weak(err);
        return;
    }
    let Some(mut config) = state.config.clone() else {
        return;
    };
    let mut changed = false;
    changed |= combo(
        ui,
        "Backend",
        &mut config.backend,
        &XrBackendKind::ALL,
        XrBackendKind::name,
    );
    ui.checkbox(&mut state.simulator, "Simulator in play");
    if state.simulator || config.backend == XrBackendKind::Simulator {
        ui.weak("Play runs the scripted runtime: no headset needed.");
    }
    changed |= ui
        .checkbox(
            &mut config.required,
            "Required (no session: the game refuses to start)",
        )
        .changed();
    changed |= combo(
        ui,
        "Tracking space",
        &mut config.tracking_space,
        &TrackingSpace::ALL,
        TrackingSpace::name,
    );
    let (lo, hi) = RENDER_SCALE_RANGE;
    changed |= ui
        .add(egui::Slider::new(&mut config.render_scale, lo..=hi).text("Render scale"))
        .changed();
    changed |= combo(
        ui,
        "Foveation",
        &mut config.foveation,
        &Foveation::ALL,
        Foveation::name,
    );
    changed |= combo(
        ui,
        "Mirror window",
        &mut config.mirror,
        &Mirror::ALL,
        Mirror::name,
    );
    let comfort = &mut config.comfort;
    changed |= ui
        .add(egui::Slider::new(&mut comfort.snap_turn_deg, 0.0..=180.0).text("Snap turn (0 = smooth)"))
        .changed();
    changed |= ui
        .add(egui::Slider::new(&mut comfort.vignette_strength, 0.0..=1.0).text("Vignette"))
        .changed();
    changed |= ui
        .add(egui::Slider::new(&mut comfort.eye_height_m, 0.5..=2.5).text("Eye height (m)"))
        .changed();
    changed |= ui.checkbox(&mut comfort.seated, "Seated").changed();
    changed |= combo(
        ui,
        "Dominant hand",
        &mut comfort.dominant_hand,
        &[DominantHand::Left, DominantHand::Right],
        |hand| match hand {
            DominantHand::Left => "left",
            DominantHand::Right => "right",
        },
    );
    changed |= ui
        .add(egui::Slider::new(&mut config.retry_frames, 1..=MAX_RETRY_FRAMES).text("Retry (fixed steps)"))
        .changed();
    if changed {
        state.edit(|edited| *edited = config);
    }
    ui.horizontal(|ui| {
        if ui
            .add_enabled(state.dirty, egui::Button::new("Save xr.ron"))
            .clicked()
            && let Err(err) = state.save(root)
        {
            state.toast = err;
        }
        if ui.button("Reload xr.ron").clicked()
            && let Err(err) = state.load(root)
        {
            state.toast = err;
        }
    });
    if !state.toast.is_empty() {
        ui.weak(&state.toast);
    }
}

/// The play toolbar's XR line (backend, app state, runtime name), `None` when the world has no XR.
#[must_use]
pub fn xr_status_line(world: &World) -> Option<String> {
    let state = world.get_resource::<XrState>()?;
    let runtime = state.runtime.as_deref().unwrap_or("no runtime");
    Some(format!(
        "XR {} · {} · {runtime}",
        state.backend_kind.name(),
        state.app.name()
    ))
}

fn invalid(key: &str, reason: &str) -> RpcError {
    let mut e = RpcError::new(
        INVALID_PARAMS,
        "F0002",
        format!("{key}: {reason}"),
        "editor.xr_settings lists the current values",
    );
    if let Some(Value::Object(data)) = e.data.as_mut() {
        data.insert("reason".into(), json!("xr_invalid_value"));
    }
    e
}

/// Applies one `set` entry.
fn apply(config: &mut XrConfig, key: &str, value: &Value) -> Result<(), RpcError> {
    let number = || {
        value
            .as_f64()
            .map(|n| n as f32)
            .ok_or_else(|| invalid(key, "a number"))
    };
    let flag = || value.as_bool().ok_or_else(|| invalid(key, "true or false"));
    let text = || value.as_str().ok_or_else(|| invalid(key, "a string"));
    match key {
        "backend" => {
            config.backend = XrBackendKind::from_name(text()?)
                .ok_or_else(|| invalid(key, "auto, openxr, simulator or off"))?;
        }
        "required" => config.required = flag()?,
        "tracking_space" => {
            config.tracking_space = TrackingSpace::from_name(text()?)
                .ok_or_else(|| invalid(key, "local, stage or local_floor"))?;
        }
        "render_scale" => config.render_scale = number()?,
        "foveation" => {
            config.foveation =
                Foveation::from_name(text()?).ok_or_else(|| invalid(key, "off, low, medium or high"))?;
        }
        "mirror" => {
            config.mirror =
                Mirror::from_name(text()?).ok_or_else(|| invalid(key, "off, left, right or both"))?;
        }
        "snap_turn_deg" => config.comfort.snap_turn_deg = number()?,
        "vignette_strength" => config.comfort.vignette_strength = number()?,
        "eye_height_m" => config.comfort.eye_height_m = number()?,
        "seated" => config.comfort.seated = flag()?,
        // The Dominant hand combo and the Retry slider (ADR-7747).
        "dominant_hand" => {
            config.comfort.dominant_hand = match text()? {
                "left" => DominantHand::Left,
                "right" => DominantHand::Right,
                _ => return Err(invalid(key, "left or right")),
            };
        }
        "retry_frames" => {
            config.retry_frames = value
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .filter(|n| (1..=MAX_RETRY_FRAMES).contains(n))
                .ok_or_else(|| {
                    invalid(
                        key,
                        &format!("a whole number of fixed steps, 1..={MAX_RETRY_FRAMES}"),
                    )
                })?;
        }
        other => {
            let mut e = invalid(other, "not an XR setting");
            if let Some(Value::Object(data)) = e.data.as_mut() {
                data.insert("reason".into(), json!("xr_unknown_option"));
            }
            return Err(e);
        }
    }
    Ok(())
}

fn config_json(config: &XrConfig) -> Value {
    json!({
        "format_version": config.format_version,
        "backend": config.backend.name(),
        "required": config.required,
        "tracking_space": config.tracking_space.name(),
        "render_scale": config.render_scale,
        "foveation": config.foveation.name(),
        "mirror": config.mirror.name(),
        "snap_turn_deg": config.comfort.snap_turn_deg,
        "vignette_strength": config.comfort.vignette_strength,
        "eye_height_m": config.comfort.eye_height_m,
        "seated": config.comfort.seated,
        "dominant_hand": match config.comfort.dominant_hand {
            DominantHand::Left => "left",
            DominantHand::Right => "right",
        },
        "retry_frames": config.retry_frames,
    })
}

/// `editor.xr_settings { set? }`: the project's `config/xr.ron`; `set` writes the named values.
fn editor_xr_settings(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let root =
        crate::mesh_source::project_root(world).ok_or_else(|| invalid("project", "no project open"))?;
    let mut config = read_config(&root).map_err(|e| invalid("config/xr.ron", &e))?;
    let set = params.get("set");
    if let Some(set) = set {
        let entries = set
            .as_object()
            .ok_or_else(|| invalid("set", "an object of setting names to values"))?;
        for (key, value) in entries {
            apply(&mut config, key, value)?;
        }
        write_config(&root, &config).map_err(|e| invalid("set", &e))?;
    }
    Ok(json!({
        "path": XR_CONFIG_PATH,
        "written": set.is_some(),
        "config": config_json(&config),
        "status": world
            .get_resource::<crate::pie::PieStatus>()
            .and_then(|status| status.xr.clone())
            .or_else(|| xr_status_line(world)),
    }))
}

/// The XR page's methods.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.xr_settings",
            tool: "xr_settings",
            description: "Read the project's config/xr.ron (absent: the defaults) and the XR status line; `set` changes the named values and writes the file. Errors are F0002 with data.reason xr_invalid_value or xr_unknown_option; nothing is written then.",
            params: &[ParamSchema {
                name: "set",
                kind: "object",
                required: false,
                description: "Setting names to values: backend, required, tracking_space, render_scale, foveation, mirror, snap_turn_deg, vignette_strength, eye_height_m, seated, dominant_hand (left | right), retry_frames (fixed steps, 1..=7200).",
            }],
            result: "{ path, written, config: { format_version, backend, required, tracking_space, render_scale, foveation, mirror, snap_turn_deg, vignette_strength, eye_height_m, seated, dominant_hand, retry_frames }, status: string | null }",
            mutating: true,
            permission: Permission::EditScene,
        },
        editor_xr_settings as Handler,
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draw(state: &mut XrSettings, root: &Path, events: Vec<egui::Event>) {
        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            events,
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 900.0),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(raw, |ui| xr_settings_section(ui, state, root));
        output.textures_delta.clear();
    }

    #[test]
    fn the_page_draws_headless_on_an_absent_file_and_saves_an_edit_that_round_trips() {
        let root = regia_test_utils::unique_temp_dir("xr_settings_page");
        let mut state = XrSettings::default();
        draw(&mut state, &root, Vec::new());
        assert_eq!(
            state.config(),
            Some(&XrConfig::default()),
            "an absent file is the default"
        );
        assert!(!state.is_dirty());
        state.edit(|config| {
            config.backend = XrBackendKind::Simulator;
            config.mirror = Mirror::Both;
            config.comfort.vignette_strength = 0.25;
        });
        assert!(state.is_dirty());
        draw(&mut state, &root, Vec::new());
        state.save(&root).expect("save");
        assert!(!state.is_dirty());
        assert_eq!(state.toast, "saved config/xr.ron");
        assert!(
            !root.join("config/xr.ron.tmp").exists(),
            "no temporary file stays"
        );
        let mut again = XrSettings::default();
        again.load(&root).expect("load");
        let config = again.config().expect("config");
        assert_eq!(config.backend, XrBackendKind::Simulator);
        assert_eq!(config.mirror, Mirror::Both);
        assert_eq!(config.comfort.vignette_strength, 0.25);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_broken_file_is_reported_and_an_out_of_range_edit_is_not_written() {
        let root = regia_test_utils::unique_temp_dir("xr_settings_bad");
        std::fs::create_dir_all(root.join("config")).expect("config dir");
        std::fs::write(root.join(XR_CONFIG_PATH), "(format_version: 1, surprise: true)").expect("write");
        let mut state = XrSettings::default();
        let err = state.load(&root).expect_err("unknown field");
        assert!(err.contains("surprise"), "{err}");
        draw(&mut state, &root, Vec::new());
        assert!(
            state.config().is_none(),
            "the page stays empty, the file stays as it was"
        );
        std::fs::remove_file(root.join(XR_CONFIG_PATH)).expect("remove");
        state.load(&root).expect("defaults");
        state.edit(|config| config.render_scale = 9.0);
        assert!(state.save(&root).is_err());
        assert!(!root.join(XR_CONFIG_PATH).exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_status_line_names_backend_state_and_runtime() {
        let mut world = World::new();
        assert_eq!(xr_status_line(&world), None);
        world.insert_resource(XrState {
            backend_kind: XrBackendKind::Simulator,
            ..XrState::default()
        });
        assert_eq!(
            xr_status_line(&world).as_deref(),
            Some("XR simulator · Off · no runtime")
        );
    }

    #[test]
    fn set_entries_validate_by_name() {
        let mut config = XrConfig::default();
        apply(&mut config, "backend", &json!("simulator")).expect("backend");
        assert_eq!(config.backend, XrBackendKind::Simulator);
        apply(&mut config, "seated", &json!(true)).expect("seated");
        assert!(config.comfort.seated);
        apply(&mut config, "mirror", &json!("right")).expect("mirror");
        let reason = |e: RpcError| e.data.and_then(|d| d.get("reason").cloned());
        let e = apply(&mut config, "nope", &json!(1)).expect_err("unknown");
        assert_eq!(reason(e), Some(json!("xr_unknown_option")));
        let e = apply(&mut config, "mirror", &json!("sideways")).expect_err("value");
        assert_eq!(reason(e), Some(json!("xr_invalid_value")));
        let e = apply(&mut config, "render_scale", &json!("big")).expect_err("type");
        assert_eq!(reason(e), Some(json!("xr_invalid_value")));
    }
}
