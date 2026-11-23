// SPDX-License-Identifier: MPL-2.0
//! UI scale modes and window size tracking (`specs/18` § 3): how UI pixels map to physical pixels,
//! plus the platform safe area that roots with [`crate::SafeAreaPadding`] avoid.

use regia_app::prelude::*;
use serde::{Deserialize, Serialize};

/// How UI pixels scale to physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum ScaleMode {
    /// 1 UI px = 1 physical px.
    ConstantPixelSize,
    /// Scale so the UI fits a reference resolution. `match_width` is the blend between matching
    /// width (1.0) and height (0.0).
    ScaleWithScreen {
        /// Reference resolution (w, h) in UI px.
        reference: [f32; 2],
        /// 1.0 = match width, 0.0 = match height, 0.5 = arithmetic mean of the width and height ratios.
        match_width: f32,
    },
    /// 1 UI px = 1 device-independent pixel (physical px / OS scale factor).
    ConstantPhysicalSize,
    /// Pixel-art UIs (ADR-2866): the largest whole-number scale (at least 1) at which the
    /// reference resolution fits the window on both axes, so every UI pixel is an exact block of
    /// physical pixels. The layout area is the reference times that scale, centred; the rest of
    /// the window is letterbox ([`ScaleMode::letterbox`]).
    IntegerScale {
        /// Reference resolution (w, h) in UI px.
        reference: [f32; 2],
    },
}

impl Default for ScaleMode {
    fn default() -> Self {
        Self::ScaleWithScreen {
            reference: [1920.0, 1080.0],
            match_width: 0.5,
        }
    }
}

impl ScaleMode {
    /// The UI scale for a window of `size` physical pixels at OS `scale_factor`.
    pub fn scale_for(&self, size: [f32; 2], scale_factor: f64) -> f32 {
        match *self {
            Self::ConstantPixelSize => 1.0,
            // A missing or broken OS scale factor (0, negative, NaN, infinite) must not collapse
            // the UI: it counts as 1.
            Self::ConstantPhysicalSize if scale_factor.is_finite() && scale_factor > 0.0 => {
                scale_factor as f32
            }
            Self::ConstantPhysicalSize => 1.0,
            Self::IntegerScale { reference } => integer_scale(reference, size),
            Self::ScaleWithScreen {
                reference,
                match_width,
            } => {
                let (w, h) = (size[0].max(1.0), size[1].max(1.0));
                let sx = w / reference[0].max(1.0);
                let sy = h / reference[1].max(1.0);
                let t = match_width.clamp(0.0, 1.0);
                // Blend linear scale factors (arithmetic mean at 0.5).
                (t * sx + (1.0 - t) * sy).max(0.05)
            }
        }
    }
}

impl ScaleMode {
    /// The largest scale at which this mode's reference resolution fits inside `size` on both
    /// axes (`min(width ratio, height ratio)`); `None` for the modes without a reference.
    pub fn fit_scale(&self, size: [f32; 2]) -> Option<f32> {
        match *self {
            Self::ScaleWithScreen { reference, .. } => {
                let sx = size[0].max(1.0) / reference[0].max(1.0);
                let sy = size[1].max(1.0) / reference[1].max(1.0);
                Some(sx.min(sy).max(0.05))
            }
            Self::IntegerScale { reference } => Some(integer_scale(reference, size)),
            Self::ConstantPixelSize | Self::ConstantPhysicalSize => None,
        }
    }

    /// Insets (l, t, r, b, whole physical px) that centre an [`ScaleMode::IntegerScale`] layout of
    /// its reference times `scale` in a window of `size`; zero for the other modes and when the
    /// layout does not fit (a window smaller than the reference at scale 1).
    pub fn letterbox(&self, size: [f32; 2], scale: f32) -> [f32; 4] {
        match *self {
            Self::IntegerScale { reference } => {
                let free = |axis: usize| {
                    let r = if reference[axis].is_finite() {
                        reference[axis].max(1.0)
                    } else {
                        1.0
                    };
                    (size[axis] - (r * scale).round()).max(0.0)
                };
                let (fx, fy) = (free(0), free(1));
                let (l, t) = ((fx * 0.5).floor(), (fy * 0.5).floor());
                [l, t, fx - l, fy - t]
            }
            _ => [0.0; 4],
        }
    }

    /// Physical pixels by which a layout that fills this mode's reference resolution overflows a
    /// window of `size` at `scale` (width, height); zero when it fits. A 16:9 reference blended
    /// at `match_width` 0.5 overflows a 16:10 panel horizontally: 1920×1080 on 1280×800 is about
    /// 71 px too wide.
    pub fn reference_overflow(&self, size: [f32; 2], scale: f32) -> [f32; 2] {
        match *self {
            Self::ScaleWithScreen { reference, .. } => [
                (reference[0] * scale - size[0]).max(0.0),
                (reference[1] * scale - size[1]).max(0.0),
            ],
            Self::IntegerScale { reference } => [
                (reference[0] * scale - size[0]).max(0.0),
                (reference[1] * scale - size[1]).max(0.0),
            ],
            Self::ConstantPixelSize | Self::ConstantPhysicalSize => [0.0, 0.0],
        }
    }
}

/// The floor of the fit scale of `reference` in `size`, at least 1 (ADR-2866).
fn integer_scale(reference: [f32; 2], size: [f32; 2]) -> f32 {
    let ratio = |axis: usize| {
        let r = if reference[axis].is_finite() {
            reference[axis].max(1.0)
        } else {
            1.0
        };
        let s = if size[axis].is_finite() {
            size[axis].max(1.0)
        } else {
            1.0
        };
        s / r
    };
    // A hair of slack so 1920 / 640 = 2.9999998 in f32 still floors to 3.
    (ratio(0).min(ratio(1)) + 1e-4).floor().max(1.0)
}

/// A piecewise-linear map from display DPI (96 × the OS scale factor) to UI scale, for
/// [`ScaleMode::ConstantPhysicalSize`] roots (ADR-2866). Points are `[dpi, scale]` in rising DPI
/// order; outside the first and last point the end values hold.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct DpiCurve {
    /// `[dpi, scale]` points, DPI strictly rising, scales above zero.
    pub points: Vec<[f32; 2]>,
}

impl DpiCurve {
    /// Checks the points: at least one, all finite, DPI strictly rising, scales above zero.
    pub fn validate(&self) -> Result<(), String> {
        if self.points.is_empty() {
            return Err("a DPI curve needs at least one [dpi, scale] point".to_owned());
        }
        for (i, p) in self.points.iter().enumerate() {
            if !p[0].is_finite() || !p[1].is_finite() || p[0] <= 0.0 || p[1] <= 0.0 {
                return Err(format!(
                    "point {i} {p:?}: dpi and scale must be finite and above zero"
                ));
            }
            if i > 0 && p[0] <= self.points[i - 1][0] {
                return Err(format!(
                    "point {i}: dpi {} does not rise above {}",
                    p[0],
                    self.points[i - 1][0]
                ));
            }
        }
        Ok(())
    }

    /// The UI scale at `dpi`; `None` when the curve is invalid ([`Self::validate`]).
    #[must_use]
    pub fn eval(&self, dpi: f32) -> Option<f32> {
        self.validate().ok()?;
        let first = self.points.first()?;
        let last = self.points.last()?;
        if !dpi.is_finite() || dpi <= first[0] {
            return Some(first[1]);
        }
        if dpi >= last[0] {
            return Some(last[1]);
        }
        self.points.windows(2).find_map(|w| {
            let (a, b) = (w[0], w[1]);
            (dpi >= a[0] && dpi <= b[0]).then(|| a[1] + (b[1] - a[1]) * (dpi - a[0]) / (b[0] - a[0]))
        })
    }
}

/// Project-wide UI scale settings (ADR-2866, ADR-8252). Read at startup from
/// [`UI_SETTINGS_PATH`], set by the game (which wins over the file), or set with the agent's
/// `editor.ui_scale_settings`.
#[derive(Resource, Debug, Clone, PartialEq, Default)]
pub struct UiScaleSettings {
    /// Optional DPI curve for [`ScaleMode::ConstantPhysicalSize`] roots; `None` uses the OS
    /// scale factor as is.
    pub dpi_curve: Option<DpiCurve>,
    /// Saved project scale rule (`.uisettings.ron` v2). Applied only to roots still on
    /// [`ScaleMode::default`]. A mode the game already set wins. `None` keeps the engine default.
    pub project_mode: Option<ScaleMode>,
}

/// The `.uisettings.ron` format version. v2 (ADR-8252) adds the optional `scale_mode` field.
pub const UI_SETTINGS_VERSION: u32 = 2;

/// Where the project's UI settings live. Under `assets/`, so a packaged game carries them on every
/// platform (the project manifest does not ship).
pub const UI_SETTINGS_PATH: &str = "assets://ui/project.uisettings.ron";

/// The project's UI settings file (`docs/formats/uisettings-ron.md`, ADR-2866). Every setting is
/// optional; an absent one keeps the engine default.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiSettingsFile {
    /// Format version ([`UI_SETTINGS_VERSION`]).
    pub version: u32,
    /// `[dpi, scale]` points of the DPI curve ([`DpiCurve`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dpi_curve: Option<Vec<[f32; 2]>>,
    /// Group layer budget in bytes ([`crate::UiLayerBudget`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer_budget_bytes: Option<u64>,
    /// Project scale rule (v2, ADR-8252). Absent keeps [`ScaleMode::default`] on each root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale_mode: Option<ScaleMode>,
}

/// A saved [`ScaleMode`] the project file and the agent may store. References must be finite and
/// above zero. `match_width` must be in 0..=1.
pub fn validate_scale_mode(mode: &ScaleMode) -> Result<(), String> {
    let reference_ok = |reference: [f32; 2]| {
        reference[0].is_finite() && reference[1].is_finite() && reference[0] > 0.0 && reference[1] > 0.0
    };
    match *mode {
        ScaleMode::ConstantPixelSize | ScaleMode::ConstantPhysicalSize => Ok(()),
        ScaleMode::ScaleWithScreen {
            reference,
            match_width,
        } => {
            if !reference_ok(reference) {
                return Err("scale_mode reference must be finite and above zero".to_owned());
            }
            if !match_width.is_finite() || !(0.0..=1.0).contains(&match_width) {
                return Err("scale_mode match_width must be from 0 to 1".to_owned());
            }
            Ok(())
        }
        ScaleMode::IntegerScale { reference } => {
            if !reference_ok(reference) {
                return Err("scale_mode reference must be finite and above zero".to_owned());
            }
            Ok(())
        }
    }
}

/// Parses a `.uisettings.ron` document. Version 1 and 2 both load; the returned file is stamped
/// at [`UI_SETTINGS_VERSION`] so callers see the current document. A newer version, an unknown
/// field, an invalid DPI curve or an invalid scale rule is an error. The on-disk bytes of a v1
/// file are unchanged until [`migrate_ui_settings_ron`].
pub fn parse_ui_settings_ron(source: &str, path: &str) -> Result<UiSettingsFile, crate::UiError> {
    let options = ron::Options::default().with_default_extension(ron::extensions::Extensions::IMPLICIT_SOME);
    let mut file: UiSettingsFile = options.from_str(source).map_err(|source| crate::UiError::Ron {
        path: path.to_owned(),
        source,
    })?;
    if file.version == 0 || file.version > UI_SETTINGS_VERSION {
        return Err(crate::UiError::UnsupportedVersion {
            path: path.to_owned(),
            found: file.version,
            supported: UI_SETTINGS_VERSION,
        });
    }
    if let Some(points) = &file.dpi_curve {
        let curve = DpiCurve {
            points: points.clone(),
        };
        curve.validate().map_err(|reason| crate::UiError::Invalid {
            path: path.to_owned(),
            reason: format!("dpi_curve: {reason}"),
        })?;
    }
    if let Some(mode) = &file.scale_mode {
        validate_scale_mode(mode).map_err(|reason| crate::UiError::Invalid {
            path: path.to_owned(),
            reason,
        })?;
    }
    file.version = UI_SETTINGS_VERSION;
    Ok(file)
}

/// The schema hash input of `.uisettings.ron` (`cargo xtask check-formats`).
#[must_use]
pub fn ui_settings_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[regia_core::schema::serde_field_names::<UiSettingsFile>()])
}

/// Migration steps registered for `.uisettings.ron`: v1 → v2 rewrites the version number
/// (ADR-8252). `scale_mode` is optional, so a v1 file means the same at v2.
#[must_use]
pub fn ui_settings_migration_sources() -> Vec<u32> {
    vec![1]
}

/// Rewrites a `.uisettings.ron` document to [`UI_SETTINGS_VERSION`]. v1 → v2 changes only the
/// root `version` number and keeps every other byte. A file already at the current version is
/// returned unchanged. A newer version, or a file with no version, is an error.
pub fn migrate_ui_settings_ron(source: &str) -> Result<String, String> {
    let found = regia_core::version::peek_ron_version(source).unwrap_or(0);
    if found == UI_SETTINGS_VERSION {
        return Ok(source.to_owned());
    }
    if found == 1 {
        return regia_core::version::rewrite_ron_version(source, "version", UI_SETTINGS_VERSION)
            .ok_or_else(|| "uisettings: no version field".to_owned());
    }
    if found > UI_SETTINGS_VERSION {
        return Err(format!(
            "uisettings version {found} is newer than {UI_SETTINGS_VERSION}"
        ));
    }
    Err(format!("no migrator from uisettings v{found}"))
}

/// `Startup`: applies [`UI_SETTINGS_PATH`] when the project has one. A setting the game already
/// changed from its default stays, so code wins over the file. A bad file logs the warning
/// `ui_settings_invalid` and changes nothing: a typo never blocks the game from starting.
pub fn load_ui_settings(world: &mut World) {
    let Some(vfs) = world.get_resource::<regia_platform::Vfs>().cloned() else {
        return;
    };
    let Ok(path) = regia_platform::VfsPath::parse(UI_SETTINGS_PATH) else {
        return;
    };
    let Ok(bytes) = vfs.read_sync(&path) else {
        return;
    };
    let file = match parse_ui_settings_ron(&String::from_utf8_lossy(&bytes), UI_SETTINGS_PATH) {
        Ok(file) => file,
        Err(e) => {
            tracing::warn!(target: "regia_ui", code = "ui_settings_invalid", "{e}");
            return;
        }
    };
    if let Some(points) = file.dpi_curve {
        let mut settings = world.get_resource_or_insert_with(UiScaleSettings::default);
        if settings.dpi_curve.is_none() {
            settings.dpi_curve = Some(DpiCurve { points });
        }
    }
    if let Some(max_bytes) = file.layer_budget_bytes {
        let mut budget = world.get_resource_or_insert_with(crate::group_layer::UiLayerBudget::default);
        if *budget == crate::group_layer::UiLayerBudget::default() {
            budget.max_bytes = max_bytes;
        }
    }
    if let Some(mode) = file.scale_mode {
        let mut settings = world.get_resource_or_insert_with(UiScaleSettings::default);
        if settings.project_mode.is_none() {
            settings.project_mode = Some(mode);
        }
    }
}

/// `PostUpdate`, before layout: a root still on [`ScaleMode::default`] takes
/// [`UiScaleSettings::project_mode`]. A mode the game set (anything other than the engine default)
/// stays. Runs for roots that already exist and for roots spawned after the file loaded.
pub fn apply_project_scale_mode(
    settings: Option<Res<UiScaleSettings>>,
    mut roots: Query<&mut crate::UiRoot>,
) {
    let Some(mode) = settings.as_ref().and_then(|s| s.project_mode) else {
        return;
    };
    for mut root in &mut roots {
        if root.scale_mode == ScaleMode::default() {
            root.scale_mode = mode;
        }
    }
}

impl UiScaleSettings {
    /// `scale` (from [`ScaleMode::scale_for`]) after the DPI curve, for a root of `mode` at OS
    /// `scale_factor`.
    #[must_use]
    pub fn adjust(&self, mode: &ScaleMode, scale_factor: f64, scale: f32) -> f32 {
        match (mode, &self.dpi_curve) {
            (ScaleMode::ConstantPhysicalSize, Some(curve)) => {
                let factor = if scale_factor.is_finite() && scale_factor > 0.0 {
                    scale_factor as f32
                } else {
                    1.0
                };
                curve.eval(96.0 * factor).unwrap_or(scale)
            }
            _ => scale,
        }
    }
}

impl ScaleMode {
    /// The handheld layout reference: the Steam Deck's 1280×800 panel. A UI authored at this
    /// reference renders at scale 1.0 on the Deck and grows on larger outputs (docked, desktop).
    pub const HANDHELD: ScaleMode = ScaleMode::ScaleWithScreen {
        reference: [1280.0, 800.0],
        match_width: 0.5,
    };
}

/// Smallest legible text on a 7" 1280×800 panel (about 216 ppi), physical px. Valve's Steam Deck
/// Verified checklist asks for text that is readable on the handheld screen without zooming; the
/// engine's floor is 12 px (an em of about 1.4 mm on that panel).
pub const HANDHELD_MIN_TEXT_PX: f32 = 12.0;

/// Device-class rules the UI applies on top of the author's layout: a floor on text size and an
/// extra title-safe margin. Defaults come from the environment ([`Self::detect`]); a game can
/// overwrite the resource (a "UI size" setting would raise `min_text_px`).
#[derive(Resource, Debug, Clone, Copy, PartialEq)]
pub struct UiDevicePolicy {
    /// Smallest rendered font size, physical px (0 = no floor).
    pub min_text_px: f32,
    /// Extra margin on each side for roots with [`crate::SafeAreaPadding`], as a fraction of the
    /// window size (TV overscan; 0 on the Deck's own panel).
    pub title_safe: f32,
    /// [`ScaleMode::ScaleWithScreen`] roots never scale past the size at which their reference
    /// resolution fits the window ([`ScaleMode::fit_scale`]), so a UI authored for 16:9 is not
    /// clipped at the sides of a 16:10 panel (ADR-526). The blend still decides when it is the
    /// smaller scale. On for handhelds.
    pub fit_reference: bool,
}

impl Default for UiDevicePolicy {
    fn default() -> Self {
        Self::detect()
    }
}

impl UiDevicePolicy {
    /// No floor, no margin.
    pub const DESKTOP: Self = Self {
        min_text_px: 0.0,
        title_safe: 0.0,
        fit_reference: false,
    };
    /// The Steam Deck's own panel: text floor, no margin (the panel has no cutouts or bezel
    /// overlap), and reference layouts fitted to the 16:10 panel.
    pub const HANDHELD: Self = Self {
        min_text_px: HANDHELD_MIN_TEXT_PX,
        title_safe: 0.0,
        fit_reference: true,
    };

    /// The policy for the running machine: [`Self::HANDHELD`] under Steam's `SteamDeck=1`
    /// environment (also set in Game Mode), [`Self::DESKTOP`] elsewhere.
    pub fn detect() -> Self {
        Self::from_env(|k| regia_core::brand::env_var(k).ok())
    }

    /// [`Self::detect`] over an injectable environment.
    pub fn from_env(get: impl Fn(&str) -> Option<String>) -> Self {
        if get("SteamDeck").is_some_and(|v| v == "1") {
            Self::HANDHELD
        } else {
            Self::DESKTOP
        }
    }

    /// The scale of a root with `mode` in a window of `size` at OS `scale_factor`, under this
    /// policy: [`ScaleMode::scale_for`], capped at [`ScaleMode::fit_scale`] when
    /// [`Self::fit_reference`] is on.
    pub fn scale_for(&self, mode: &ScaleMode, size: [f32; 2], scale_factor: f64) -> f32 {
        let scale = mode.scale_for(size, scale_factor);
        match mode.fit_scale(size) {
            Some(fit) if self.fit_reference => scale.min(fit),
            _ => scale,
        }
    }

    /// Insets (l, t, r, b) in physical px the title-safe margin takes from a window of `size`.
    pub fn title_safe_insets(&self, size: [f32; 2]) -> [f32; 4] {
        let f = if self.title_safe.is_nan() {
            0.0
        } else {
            self.title_safe.clamp(0.0, 0.25)
        };
        [size[0] * f, size[1] * f, size[0] * f, size[1] * f]
    }
}

/// `PreUpdate`: copies the policy's text floor into the text system. A change bumps the
/// localization generation (like [`apply_ui_accessibility`]), which every text cache key and the
/// layout dirty check follow, so text re-measures at the new floor.
pub fn apply_ui_device_policy(
    policy: Res<UiDevicePolicy>,
    mut text: ResMut<crate::text::TextSystem>,
    mut loc: ResMut<crate::localization::Localization>,
) {
    if text.min_font_px != policy.min_text_px {
        text.min_font_px = policy.min_text_px;
        loc.generation.0 += 1;
    }
}

/// The player's accessibility choices as the UI sees them. The framework copies
/// `GameSettings.accessibility` into it; tween and theme code read the flags (ADR-341).
#[derive(Resource, Debug, Clone, Copy, PartialEq)]
pub struct UiAccessibility {
    /// Multiplier on all text sizes (`1.0` = as authored); see [`Self::effective_text_scale`].
    pub text_scale: f32,
    /// Animations jump to their end value instead of playing.
    pub reduced_motion: bool,
    /// Themes should pick their high-contrast colors.
    pub high_contrast: bool,
}

impl Default for UiAccessibility {
    fn default() -> Self {
        Self {
            text_scale: 1.0,
            reduced_motion: false,
            high_contrast: false,
        }
    }
}

impl UiAccessibility {
    /// Smallest accepted text scale.
    pub const MIN_TEXT_SCALE: f32 = 0.5;
    /// Largest accepted text scale.
    pub const MAX_TEXT_SCALE: f32 = 3.0;

    /// `text_scale` limited to [`Self::MIN_TEXT_SCALE`]..=[`Self::MAX_TEXT_SCALE`]; a non-finite
    /// value counts as `1.0`.
    pub fn effective_text_scale(&self) -> f32 {
        if self.text_scale.is_finite() {
            self.text_scale.clamp(Self::MIN_TEXT_SCALE, Self::MAX_TEXT_SCALE)
        } else {
            1.0
        }
    }
}

/// `PreUpdate`: applies [`UiAccessibility::text_scale`] to the text system, times the OS text size
/// ([`regia_platform::SystemTextScale`]: Android's font size setting, 1 elsewhere) and limited to
/// [`UiAccessibility::MIN_TEXT_SCALE`]..=[`UiAccessibility::MAX_TEXT_SCALE`] (ADR-4932). A change
/// bumps the localization generation, which every text cache key and the layout dirty check already
/// follow, so all text re-measures and re-lays out on the next layout pass.
pub fn apply_ui_accessibility(
    access: Res<UiAccessibility>,
    system: Option<Res<regia_platform::SystemTextScale>>,
    mut text: ResMut<crate::text::TextSystem>,
    mut loc: ResMut<crate::localization::Localization>,
) {
    let os = system
        .map(|s| s.0)
        .filter(|s| s.is_finite() && *s > 0.0)
        .unwrap_or(1.0);
    let wanted = (access.effective_text_scale() * os)
        .clamp(UiAccessibility::MIN_TEXT_SCALE, UiAccessibility::MAX_TEXT_SCALE);
    if text.text_scale != wanted {
        text.text_scale = wanted;
        loc.generation.0 += 1;
    }
}

/// The size (physical px) of the primary window, as seen by the UI. Updated from window events.
#[derive(Resource, Debug, Clone, Copy, PartialEq)]
pub struct UiScales {
    /// Primary window size in physical px.
    pub window_size: [f32; 2],
    /// OS scale factor.
    pub scale_factor: f64,
}

impl Default for UiScales {
    /// No window yet (size 0) at an OS scale of 1: a factor of 0 would collapse every
    /// physical-size root, and a render-out would restore that 0 afterwards.
    fn default() -> Self {
        Self {
            window_size: [0.0, 0.0],
            scale_factor: 1.0,
        }
    }
}

impl UiScales {
    /// Per-root resolved scale from the current window size and [`ScaleMode`].
    pub fn scale_of(&self, mode: &ScaleMode) -> f32 {
        mode.scale_for(self.window_size, self.scale_factor)
    }

    /// [`Self::scale_of`] under a device policy ([`UiDevicePolicy::scale_for`]); without a policy
    /// it is [`Self::scale_of`].
    pub fn scale_with_policy(&self, mode: &ScaleMode, policy: Option<&UiDevicePolicy>) -> f32 {
        match policy {
            Some(policy) => policy.scale_for(mode, self.window_size, self.scale_factor),
            None => self.scale_of(mode),
        }
    }
}

/// Fills [`UiScales`] with the primary window's current size (best effort at startup).
pub fn init_window_size(world: &mut World) {
    let primary = world
        .query_filtered::<Entity, With<regia_platform::PrimaryWindow>>()
        .iter(world)
        .next();
    let mut scales = UiScales {
        window_size: [1280.0, 720.0],
        scale_factor: 1.0,
    };
    // Prefer the wgpu surface config when it already exists — that is the size the overlay draws into.
    if let Some(surfaces) = world.get_resource::<regia_render::WindowSurfaces>()
        && let Some((w, h)) = surfaces.primary_size()
        && w > 0
        && h > 0
    {
        scales.window_size = [w as f32, h as f32];
    } else if let Some(handles) = world.get_resource::<regia_platform::WindowHandles>()
        && let Some(os) = startup_window(primary, handles).and_then(|e| handles.get(e))
    {
        let s = os.inner_size();
        if s.width > 0 && s.height > 0 {
            scales.window_size = [s.width as f32, s.height as f32];
        }
        scales.scale_factor = os.scale_factor();
    } else if let Some(settings) = world.get_resource::<regia_platform::WindowSettings>() {
        let (w, h) = settings.window.logical_size;
        scales.window_size = [
            (w * settings.window.scale_factor).max(1.0) as f32,
            (h * settings.window.scale_factor).max(1.0) as f32,
        ];
        scales.scale_factor = settings.window.scale_factor;
    }
    if let Some(handles) = world.get_resource::<regia_platform::WindowHandles>()
        && let Some(os) = startup_window(primary, handles).and_then(|e| handles.get(e))
    {
        scales.scale_factor = os.scale_factor();
    }
    world.insert_resource(scales);
}

/// The window the startup scale is read from: the primary window, else the lowest entity.
fn startup_window(primary: Option<Entity>, handles: &regia_platform::WindowHandles) -> Option<Entity> {
    pick_startup_window(primary, handles.iter().map(|(e, _)| e))
}

/// `primary` when it is among `windows`, else the lowest entity (never hash order).
fn pick_startup_window(primary: Option<Entity>, windows: impl Iterator<Item = Entity>) -> Option<Entity> {
    let all: Vec<Entity> = windows.collect();
    primary
        .filter(|p| all.contains(p))
        .or_else(|| all.into_iter().min_by_key(|e| e.to_bits()))
}

/// `PreUpdate`: follow resize / scale-factor messages, then reconcile with the live surface config.
pub fn track_window_size(
    mut scales: ResMut<UiScales>,
    mut resized: MessageReader<regia_platform::WindowResized>,
    mut sf: MessageReader<regia_platform::WindowScaleFactorChanged>,
    surfaces: Option<Res<regia_render::WindowSurfaces>>,
) {
    let _span = tracing::debug_span!("track_window_size").entered();
    for e in resized.read() {
        scales.window_size = [e.width as f32, e.height as f32];
    }
    for e in sf.read() {
        scales.scale_factor = e.scale_factor;
    }
    // Surface config wins: layout must match the texture the overlay actually samples into.
    if let Some(surfaces) = surfaces.as_ref()
        && let Some((w, h)) = surfaces.primary_size()
        && w > 0
        && h > 0
    {
        let size = [w as f32, h as f32];
        if scales.window_size != size {
            scales.window_size = size;
        }
    }
}

/// Resolves the usable size and padding (physical px) for a root: window size minus the safe area
/// when the root entity has [`crate::SafeAreaPadding`] (`specs/18` § 3). Returns
/// `(window_size, padding_ltrb)`.
pub fn resolve_window_size(world: &World, root_entity: Entity, scales: &UiScales) -> ([f32; 2], [f32; 4]) {
    // Camera-target and world roots lay out at their target's size, with no safe area (ADR-2406).
    if let Some(size) = crate::world_target::root_target_size(world, root_entity) {
        return (size, [0.0; 4]);
    }
    let size = scales.window_size;
    let mut padding = [0.0f32; 4]; // l, t, r, b
    if world.get::<crate::SafeAreaPadding>(root_entity).is_some() {
        // A render-out insets by its own safe area (the window's does not describe the output).
        let safe = crate::output::forced_safe_area(world)
            .or_else(|| world.get_resource::<regia_platform::SafeArea>().copied());
        if let Some(safe) = safe {
            padding = [
                safe.left as f32,
                safe.top as f32,
                safe.right as f32,
                safe.bottom as f32,
            ];
            // A device policy (the Deck's title-safe margin) can only widen the inset.
            if let Some(policy) = world.get_resource::<UiDevicePolicy>() {
                let extra = policy.title_safe_insets(size);
                for (p, e) in padding.iter_mut().zip(extra) {
                    *p = p.max(e);
                }
            }
        }
    }
    (size, padding)
}

#[cfg(test)]
mod tests {
    #[test]
    fn startup_window_prefers_primary_then_lowest_entity() {
        let (a, b) = (Entity::from_bits(1), Entity::from_bits(2));
        assert_eq!(super::pick_startup_window(Some(b), [a, b].into_iter()), Some(b));
        assert_eq!(super::pick_startup_window(Some(b), [b, a].into_iter()), Some(b));
        assert_eq!(super::pick_startup_window(None, [b, a].into_iter()), Some(a));
        assert_eq!(
            super::pick_startup_window(Some(Entity::from_bits(9)), [b, a].into_iter()),
            Some(a)
        );
        assert_eq!(super::pick_startup_window(None, std::iter::empty()), None);
    }

    use super::*;

    /// From the TEST Pixel 9 Pro (2026-11-09, ADR-4932): Android's font size setting changed no
    /// text. The OS multiplier now scales the player's own text scale, as Android's sp units do.
    #[test]
    fn the_os_text_size_multiplies_the_players_text_scale() {
        use bevy_ecs::system::RunSystemOnce;
        let run = |player: f32, os: Option<f32>| {
            let mut world = World::new();
            world.insert_resource(UiAccessibility {
                text_scale: player,
                ..Default::default()
            });
            world.init_resource::<crate::text::TextSystem>();
            world.init_resource::<crate::localization::Localization>();
            if let Some(os) = os {
                world.insert_resource(regia_platform::SystemTextScale(os));
            }
            world.run_system_once(apply_ui_accessibility).expect("runs");
            world.resource::<crate::text::TextSystem>().text_scale
        };
        assert_eq!(run(1.0, None), 1.0);
        assert_eq!(run(1.0, Some(1.3)), 1.3);
        assert_eq!(run(1.5, Some(1.3)), 1.5 * 1.3);
        // The product stays in the accepted range, and a broken OS value counts as 1.
        assert_eq!(run(2.0, Some(2.0)), UiAccessibility::MAX_TEXT_SCALE);
        assert_eq!(run(0.5, Some(0.5)), UiAccessibility::MIN_TEXT_SCALE);
        assert_eq!(run(1.2, Some(f32::NAN)), 1.2);
        assert_eq!(run(1.2, Some(0.0)), 1.2);
    }

    #[test]
    fn text_scale_is_clamped_and_sanitized() {
        let at = |text_scale| UiAccessibility {
            text_scale,
            ..UiAccessibility::default()
        };
        assert_eq!(at(1.5).effective_text_scale(), 1.5);
        assert_eq!(at(0.0).effective_text_scale(), UiAccessibility::MIN_TEXT_SCALE);
        assert_eq!(at(40.0).effective_text_scale(), UiAccessibility::MAX_TEXT_SCALE);
        assert_eq!(at(f32::NAN).effective_text_scale(), 1.0);
        assert_eq!(at(f32::INFINITY).effective_text_scale(), 1.0);
    }

    #[test]
    fn constant_modes() {
        let s = ScaleMode::ConstantPixelSize.scale_for([3840.0, 2160.0], 2.0);
        assert_eq!(s, 1.0);
        let s = ScaleMode::ConstantPhysicalSize.scale_for([3840.0, 2160.0], 2.0);
        assert_eq!(s, 2.0);
    }

    #[test]
    fn handheld_reference_is_identity_on_the_deck() {
        let s = ScaleMode::HANDHELD.scale_for([1280.0, 800.0], 1.0);
        assert!((s - 1.0).abs() < 1e-5);
        // Docked on a 1080p TV the UI grows ~1.4x instead of staying Deck-sized.
        let docked = ScaleMode::HANDHELD.scale_for([1920.0, 1080.0], 1.0);
        assert!(docked > 1.3 && docked < 1.5, "{docked}");
    }

    #[test]
    fn policy_follows_the_steam_environment() {
        let deck = UiDevicePolicy::from_env(|k| (k == "SteamDeck").then(|| "1".to_owned()));
        assert_eq!(deck, UiDevicePolicy::HANDHELD);
        assert_eq!(deck.min_text_px, HANDHELD_MIN_TEXT_PX);
        assert_eq!(UiDevicePolicy::from_env(|_| None), UiDevicePolicy::DESKTOP);
        let tv = UiDevicePolicy {
            title_safe: 0.05,
            ..UiDevicePolicy::DESKTOP
        };
        assert_eq!(tv.title_safe_insets([1920.0, 1000.0]), [96.0, 50.0, 96.0, 50.0]);
        // An absurd margin is clamped so the UI never collapses.
        let wild = UiDevicePolicy {
            title_safe: 5.0,
            ..UiDevicePolicy::DESKTOP
        };
        assert_eq!(wild.title_safe_insets([100.0, 100.0]), [25.0; 4]);
    }

    #[test]
    fn a_16_9_reference_fits_the_16_10_panel_on_handhelds_only() {
        let authored = ScaleMode::default(); // 1920×1080, match_width 0.5
        let deck = [1280.0, 800.0];
        let blended = authored.scale_for(deck, 1.0);
        assert!((blended - 0.7037).abs() < 1e-3, "{blended}");
        let overflow = authored.reference_overflow(deck, blended);
        assert!(
            overflow[0] > 70.0 && overflow[0] < 72.0,
            "clipped at the sides: {overflow:?}"
        );
        assert_eq!(overflow[1], 0.0);

        let fit = authored.fit_scale(deck).expect("reference mode");
        assert!((fit - 1280.0 / 1920.0).abs() < 1e-5);
        let handheld = UiDevicePolicy::HANDHELD.scale_for(&authored, deck, 1.0);
        assert_eq!(handheld, fit);
        assert_eq!(authored.reference_overflow(deck, handheld), [0.0, 0.0]);
        // Desktops keep the author's blend; on a 16:9 window fit and blend agree anyway.
        assert_eq!(UiDevicePolicy::DESKTOP.scale_for(&authored, deck, 1.0), blended);
        let tv = [1920.0, 1080.0];
        assert_eq!(
            UiDevicePolicy::HANDHELD.scale_for(&authored, tv, 1.0),
            authored.scale_for(tv, 1.0)
        );
        // The Deck's own reference is identity on its panel, fitted or not.
        assert!((UiDevicePolicy::HANDHELD.scale_for(&ScaleMode::HANDHELD, deck, 1.0) - 1.0).abs() < 1e-5);
        // Modes without a reference are untouched.
        assert_eq!(ScaleMode::ConstantPixelSize.fit_scale(deck), None);
        assert_eq!(
            UiDevicePolicy::HANDHELD.scale_for(&ScaleMode::ConstantPhysicalSize, deck, 1.5),
            1.5
        );
        let scales = UiScales {
            window_size: deck,
            scale_factor: 1.0,
        };
        assert_eq!(scales.scale_with_policy(&authored, None), blended);
        assert_eq!(
            scales.scale_with_policy(&authored, Some(&UiDevicePolicy::HANDHELD)),
            handheld
        );
    }

    #[test]
    fn reference_modes() {
        let mode = ScaleMode::ScaleWithScreen {
            reference: [1920.0, 1080.0],
            match_width: 1.0,
        };
        assert!((mode.scale_for([960.0, 540.0], 1.0) - 0.5).abs() < 1e-4);
        let mode = ScaleMode::ScaleWithScreen {
            reference: [1920.0, 1080.0],
            match_width: 0.0,
        };
        assert!((mode.scale_for([960.0, 270.0], 1.0) - 0.25).abs() < 1e-4);
        // Blend at 0.5 averages both.
        let mode = ScaleMode::ScaleWithScreen {
            reference: [1920.0, 1080.0],
            match_width: 0.5,
        };
        assert!((mode.scale_for([960.0, 540.0], 1.0) - 0.5).abs() < 1e-4);
    }
}
