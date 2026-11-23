// SPDX-License-Identifier: MPL-2.0
//! The Inspector's full color picker (spec addendum § 7.12, ADR-7786): RGB as floats or 0-255
//! bytes, HSV, a perceptual mode (OKLCH), hex entry, saved swatches and recent colors (the user's
//! editor preferences `color.swatches` and `color.recent`), and an HDR intensity in EV for
//! `@ColorHdr` fields.
//!
//! Values are linear RGB(A), as components store them. The byte, HSV and hex views use the same
//! `pow(x, 1/2.2)` encode as the rest of the Inspector ([`super::linear_to_srgb_u8`]), so a color
//! reads the same in every view. OKLCH works on the linear values directly.
//!
//! Every control is a focusable widget: Tab reaches the swatch button (Enter or Space opens the
//! picker), the mode buttons, each channel (arrow keys step it; typing a number sets it), the hex
//! field (Enter applies), the swatch and recent buttons (Enter applies; Delete removes a focused
//! saved swatch) and **Save swatch**.

use super::picker_links::{PickerLinks, PickerRequest};

/// Most saved swatches kept.
pub const MAX_SWATCHES: usize = 32;
/// Most recent colors kept.
pub const MAX_RECENT: usize = 12;
/// The lowest and highest HDR intensity, EV.
pub const EV_RANGE: std::ops::RangeInclusive<f32> = -10.0..=20.0;

// ---------------------------------------------------------------------------------------------
// Color models

/// Linear channel to the Inspector's encoded `[0, 1]` value (`pow(x, 1/2.2)`, clamped).
#[must_use]
pub fn encode(linear: f32) -> f32 {
    linear.clamp(0.0, 1.0).powf(1.0 / 2.2)
}

/// Encoded `[0, 1]` value back to linear (`pow(x, 2.2)`).
#[must_use]
pub fn decode(encoded: f32) -> f32 {
    encoded.clamp(0.0, 1.0).powf(2.2)
}

/// Encoded RGB to HSV: hue in degrees `[0, 360)`, saturation and value in `[0, 1]`.
#[must_use]
pub fn rgb_to_hsv(rgb: [f32; 3]) -> [f32; 3] {
    let [r, g, b] = rgb;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;
    let hue = if delta <= f32::EPSILON {
        0.0
    } else if max == r {
        60.0 * ((g - b) / delta).rem_euclid(6.0)
    } else if max == g {
        60.0 * ((b - r) / delta + 2.0)
    } else {
        60.0 * ((r - g) / delta + 4.0)
    };
    let saturation = if max <= f32::EPSILON { 0.0 } else { delta / max };
    [hue.rem_euclid(360.0), saturation, max]
}

/// HSV (hue in degrees) to encoded RGB.
#[must_use]
pub fn hsv_to_rgb(hsv: [f32; 3]) -> [f32; 3] {
    let hue = hsv[0].rem_euclid(360.0) / 60.0;
    let saturation = hsv[1].clamp(0.0, 1.0);
    let value = hsv[2].clamp(0.0, 1.0);
    let chroma = value * saturation;
    let x = chroma * (1.0 - (hue.rem_euclid(2.0) - 1.0).abs());
    let (r, g, b) = match hue as u32 {
        0 => (chroma, x, 0.0),
        1 => (x, chroma, 0.0),
        2 => (0.0, chroma, x),
        3 => (0.0, x, chroma),
        4 => (x, 0.0, chroma),
        _ => (chroma, 0.0, x),
    };
    let m = value - chroma;
    [r + m, g + m, b + m]
}

/// Linear sRGB to OKLab `[L, a, b]` (the published matrices of the OKLab color space; see
/// `docs/provenance.md`).
#[must_use]
pub fn linear_to_oklab(rgb: [f32; 3]) -> [f32; 3] {
    let [r, g, b] = rgb;
    let l = 0.412_221_47 * r + 0.536_332_55 * g + 0.051_445_995 * b;
    let m = 0.211_903_5 * r + 0.680_699_5 * g + 0.107_396_96 * b;
    let s = 0.088_302_46 * r + 0.281_718_85 * g + 0.629_978_7 * b;
    let (l, m, s) = (l.cbrt(), m.cbrt(), s.cbrt());
    [
        0.210_454_26 * l + 0.793_617_8 * m - 0.004_072_047 * s,
        1.977_998_5 * l - 2.428_592_2 * m + 0.450_593_7 * s,
        0.025_904_037 * l + 0.782_771_77 * m - 0.808_675_77 * s,
    ]
}

/// OKLab `[L, a, b]` back to linear sRGB (unclamped: a color outside sRGB comes back with a
/// channel below 0 or above 1).
#[must_use]
pub fn oklab_to_linear(lab: [f32; 3]) -> [f32; 3] {
    let [lightness, a, b] = lab;
    let l = lightness + 0.396_337_78 * a + 0.215_803_76 * b;
    let m = lightness - 0.105_561_346 * a - 0.063_854_17 * b;
    let s = lightness - 0.089_484_18 * a - 1.291_485_5 * b;
    let (l, m, s) = (l * l * l, m * m * m, s * s * s);
    [
        4.076_741_7 * l - 3.307_711_6 * m + 0.230_969_94 * s,
        -1.268_438 * l + 2.609_757_4 * m - 0.341_319_38 * s,
        -0.004_196_086_3 * l - 0.703_418_6 * m + 1.707_614_7 * s,
    ]
}

/// Linear sRGB to OKLCH `[L, C, h]` (hue in degrees `[0, 360)`).
#[must_use]
pub fn linear_to_oklch(rgb: [f32; 3]) -> [f32; 3] {
    let [lightness, a, b] = linear_to_oklab(rgb);
    let chroma = a.hypot(b);
    let hue = if chroma < 1e-6 {
        0.0
    } else {
        b.atan2(a).to_degrees().rem_euclid(360.0)
    };
    [lightness, chroma, hue]
}

/// OKLCH `[L, C, h]` to linear sRGB. A color outside sRGB is brought inside by lowering its
/// chroma at the same lightness and hue (a bisection over chroma), so the hue the user picked
/// stays.
#[must_use]
pub fn oklch_to_linear(lch: [f32; 3]) -> [f32; 3] {
    let lightness = lch[0].clamp(0.0, 1.0);
    let hue = lch[2].to_radians();
    let at = |chroma: f32| oklab_to_linear([lightness, chroma * hue.cos(), chroma * hue.sin()]);
    let inside = |rgb: [f32; 3]| rgb.iter().all(|c| (-1e-4..=1.0 + 1e-4).contains(c));
    let chroma = lch[1].max(0.0);
    let full = at(chroma);
    if inside(full) {
        return full.map(|c| c.clamp(0.0, 1.0));
    }
    let (mut low, mut high) = (0.0_f32, chroma);
    for _ in 0..24 {
        let mid = 0.5 * (low + high);
        if inside(at(mid)) {
            low = mid;
        } else {
            high = mid;
        }
    }
    at(low).map(|c| c.clamp(0.0, 1.0))
}

/// Parses `#rgb`, `#rrggbb` or `#rrggbbaa` (the `#` is optional, case does not matter) into a
/// linear RGBA color. `None` for anything else.
#[must_use]
pub fn parse_hex(text: &str) -> Option<[f32; 4]> {
    let digits = text.trim().trim_start_matches('#');
    if !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(digits.get(i..i + 2)?, 16).ok();
    let bytes = match digits.len() {
        3 => {
            let nibble = |i: usize| u8::from_str_radix(digits.get(i..=i)?, 16).ok().map(|n| n * 17);
            [nibble(0)?, nibble(1)?, nibble(2)?, 255]
        }
        6 => [byte(0)?, byte(2)?, byte(4)?, 255],
        8 => [byte(0)?, byte(2)?, byte(4)?, byte(6)?],
        _ => return None,
    };
    Some([
        decode(f32::from(bytes[0]) / 255.0),
        decode(f32::from(bytes[1]) / 255.0),
        decode(f32::from(bytes[2]) / 255.0),
        f32::from(bytes[3]) / 255.0,
    ])
}

/// A linear RGBA color as `#rrggbb` (or `#rrggbbaa` with `alpha`). HDR channels clamp to 1.
#[must_use]
pub fn to_hex(rgba: [f32; 4], alpha: bool) -> String {
    let byte = |c: f32| (encode(c) * 255.0).round() as u8;
    let a = (rgba[3].clamp(0.0, 1.0) * 255.0).round() as u8;
    if alpha {
        format!(
            "#{:02x}{:02x}{:02x}{a:02x}",
            byte(rgba[0]),
            byte(rgba[1]),
            byte(rgba[2])
        )
    } else {
        format!("#{:02x}{:02x}{:02x}", byte(rgba[0]), byte(rgba[1]), byte(rgba[2]))
    }
}

/// Splits an HDR color into a base color (brightest channel at most 1) and an intensity in EV
/// (`rgb = base * 2^ev`). A color whose brightest channel is at most 1 has intensity 0.
#[must_use]
pub fn split_intensity(rgb: [f32; 3]) -> ([f32; 3], f32) {
    let peak = rgb[0].max(rgb[1]).max(rgb[2]);
    if !peak.is_finite() || peak <= 1.0 {
        return (rgb, 0.0);
    }
    (rgb.map(|c| c / peak), peak.log2())
}

/// [`split_intensity`] reversed: `base * 2^ev`.
#[must_use]
pub fn with_intensity(base: [f32; 3], ev: f32) -> [f32; 3] {
    let scale = ev.clamp(*EV_RANGE.start(), *EV_RANGE.end()).exp2();
    base.map(|c| c * scale)
}

/// Parses a comma-separated hex list (the preference text of `color.swatches` and
/// `color.recent`); entries that do not parse are skipped.
#[must_use]
pub fn parse_hex_list(text: &str) -> Vec<[f32; 4]> {
    text.split(',').filter_map(parse_hex).collect()
}

/// A color list as the preference text: `#rrggbbaa` entries joined by commas.
#[must_use]
pub fn hex_list(colors: &[[f32; 4]]) -> String {
    colors
        .iter()
        .map(|c| to_hex(*c, true))
        .collect::<Vec<_>>()
        .join(",")
}

/// `color` first in `list`, without an earlier equal entry, at most `max` entries.
pub fn push_front_unique(list: &mut Vec<[f32; 4]>, color: [f32; 4], max: usize) {
    let key = to_hex(color, true);
    list.retain(|c| to_hex(*c, true) != key);
    list.insert(0, color);
    list.truncate(max);
}

// ---------------------------------------------------------------------------------------------
// The picker

/// How the channel row reads the color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorMode {
    /// Linear RGB floats (what the component stores).
    #[default]
    Linear,
    /// Encoded 0-255 bytes.
    Bytes,
    /// Hue, saturation, value.
    Hsv,
    /// Perceptual lightness, chroma, hue (OKLCH).
    Oklch,
}

impl ColorMode {
    /// Every mode, in button order.
    pub const ALL: [Self; 4] = [Self::Linear, Self::Bytes, Self::Hsv, Self::Oklch];

    /// The button text.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Linear => "RGB",
            Self::Bytes => "0-255",
            Self::Hsv => "HSV",
            Self::Oklch => "OKLCH",
        }
    }
}

/// The picker's per-field state, kept in egui's memory.
#[derive(Debug, Clone, Default)]
struct PickerState {
    open: bool,
    mode: ColorMode,
    hex: String,
    /// The color the hex text was last filled from (refilled when the value moves elsewhere).
    hex_of: Option<[f32; 4]>,
    /// The last HSV / OKLCH reading and the color it was read from: a grey keeps its hue.
    hsv: Option<([f32; 4], [f32; 3])>,
    lch: Option<([f32; 4], [f32; 3])>,
}

/// What [`color_picker`] did this frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PickerOutcome {
    /// The color changed.
    pub changed: bool,
    /// The edit ended (a drag released, a typed value or hex applied, a swatch picked): the
    /// color joins the recent list.
    pub committed: bool,
}

/// The fill of a swatch button: the color itself (data, not a theme color).
fn fill(rgba: [f32; 4]) -> egui::Color32 {
    let byte = |c: f32| (encode(c) * 255.0).round() as u8;
    egui::Color32::from_rgb(byte(rgba[0]), byte(rgba[1]), byte(rgba[2]))
}

/// A focusable button filled with `rgba`, named `name` for assistive tech.
fn swatch_button(ui: &mut egui::Ui, rgba: [f32; 4], name: &str) -> egui::Response {
    let edge = ui.spacing().interact_size.y;
    let response = ui.add(
        egui::Button::new("")
            .fill(fill(rgba))
            .min_size(egui::vec2(edge * 1.5, edge)),
    );
    crate::a11y_audit::name(response, egui::WidgetType::Button, name).on_hover_text(name)
}

/// The swatch button that opens and closes the picker of field `id`; returns whether the picker
/// is open (draw it with [`picker_panel`]).
pub fn picker_toggle(ui: &mut egui::Ui, id: egui::Id, rgba: [f32; 4], name: &str) -> bool {
    let state_id = id.with("color_picker_state");
    let mut state = ui
        .data(|d| d.get_temp::<PickerState>(state_id))
        .unwrap_or_default();
    let toggle = swatch_button(ui, rgba, &format!("{name} color picker"));
    if toggle.clicked() {
        state.open = !state.open;
    }
    let open = state.open;
    ui.data_mut(|d| d.insert_temp(state_id, state));
    open
}

/// The picker of field `id` when [`picker_toggle`] opened it (nothing otherwise).
///
/// `rgba` is linear; `alpha` shows the alpha channel; `hdr` adds the intensity control and lets
/// float channels pass 1. `links` supplies the saved swatches and recent colors and takes the
/// save and remove requests (none outside the Inspector).
pub fn picker_panel(
    ui: &mut egui::Ui,
    id: egui::Id,
    rgba: &mut [f32; 4],
    alpha: bool,
    hdr: bool,
    name: &str,
    mut links: Option<&mut PickerLinks>,
) -> PickerOutcome {
    let state_id = id.with("color_picker_state");
    let mut state = ui
        .data(|d| d.get_temp::<PickerState>(state_id))
        .unwrap_or_default();
    let mut outcome = PickerOutcome::default();
    if state.open {
        picker_body(
            ui,
            &mut state,
            rgba,
            FieldKind { alpha, hdr, name },
            &mut links,
            &mut outcome,
        );
    }
    ui.data_mut(|d| d.insert_temp(state_id, state));
    outcome
}

/// [`picker_toggle`] and [`picker_panel`] one under the other.
pub fn color_picker(
    ui: &mut egui::Ui,
    id: egui::Id,
    rgba: &mut [f32; 4],
    alpha: bool,
    hdr: bool,
    name: &str,
    links: Option<&mut PickerLinks>,
) -> PickerOutcome {
    picker_toggle(ui, id, *rgba, name);
    picker_panel(ui, id, rgba, alpha, hdr, name, links)
}

/// What kind of color field a picker edits.
#[derive(Clone, Copy)]
struct FieldKind<'a> {
    alpha: bool,
    hdr: bool,
    name: &'a str,
}

fn picker_body(
    ui: &mut egui::Ui,
    state: &mut PickerState,
    rgba: &mut [f32; 4],
    field: FieldKind<'_>,
    links: &mut Option<&mut PickerLinks>,
    outcome: &mut PickerOutcome,
) {
    let FieldKind { alpha, hdr, name } = field;
    let set = |rgba: &mut [f32; 4], new: [f32; 4], outcome: &mut PickerOutcome| {
        if new.iter().all(|c| c.is_finite()) && new != *rgba {
            *rgba = new;
            outcome.changed = true;
        }
    };
    ui.vertical(|ui| {
        ui.horizontal(|ui| {
            for mode in ColorMode::ALL {
                let button = ui.selectable_label(state.mode == mode, mode.label());
                let button = crate::a11y_audit::name(
                    button,
                    egui::WidgetType::SelectableLabel,
                    &format!("{name} {} mode", mode.label()),
                );
                if button.clicked() {
                    state.mode = mode;
                }
            }
        });
        let (base, ev) = if hdr {
            split_intensity([rgba[0], rgba[1], rgba[2]])
        } else {
            ([rgba[0], rgba[1], rgba[2]], 0.0)
        };
        let rebuild = |base: [f32; 3], a: f32| {
            let rgb = if hdr { with_intensity(base, ev) } else { base };
            [rgb[0], rgb[1], rgb[2], a]
        };
        ui.horizontal(|ui| {
            let track = |ui: &mut egui::Ui,
                         value: &mut f32,
                         range: std::ops::RangeInclusive<f32>,
                         speed: f64,
                         label: &str,
                         outcome: &mut PickerOutcome|
             -> bool {
                let response = ui.add(
                    egui::DragValue::new(value)
                        .range(range)
                        .speed(speed)
                        .prefix(format!("{label} ")),
                );
                let response = crate::a11y_audit::name(
                    response,
                    egui::WidgetType::DragValue,
                    &format!("{name} {label}"),
                );
                if response.drag_stopped() || response.lost_focus() {
                    outcome.committed = true;
                }
                response.changed()
            };
            match state.mode {
                ColorMode::Linear => {
                    let top = if hdr { f32::MAX } else { 1.0 };
                    let mut channels = [rgba[0], rgba[1], rgba[2]];
                    let mut any = false;
                    for (channel, label) in channels.iter_mut().zip(["R", "G", "B"]) {
                        any |= track(ui, channel, 0.0..=top, 0.005, label, outcome);
                    }
                    if any {
                        set(rgba, [channels[0], channels[1], channels[2], rgba[3]], outcome);
                    }
                }
                ColorMode::Bytes => {
                    let mut bytes = base.map(|c| (encode(c) * 255.0).round());
                    let mut any = false;
                    for (channel, label) in bytes.iter_mut().zip(["R", "G", "B"]) {
                        any |= track(ui, channel, 0.0..=255.0, 1.0, label, outcome);
                    }
                    if any {
                        set(
                            rgba,
                            rebuild(bytes.map(|b| decode(b.round() / 255.0)), rgba[3]),
                            outcome,
                        );
                    }
                }
                ColorMode::Hsv => {
                    let encoded = base.map(encode);
                    let mut hsv = match state.hsv {
                        Some((of, hsv)) if of == *rgba => hsv,
                        _ => rgb_to_hsv(encoded),
                    };
                    let mut shown = [hsv[0], hsv[1] * 100.0, hsv[2] * 100.0];
                    let mut any = track(ui, &mut shown[0], 0.0..=360.0, 1.0, "H", outcome);
                    any |= track(ui, &mut shown[1], 0.0..=100.0, 0.5, "S", outcome);
                    any |= track(ui, &mut shown[2], 0.0..=100.0, 0.5, "V", outcome);
                    if any {
                        hsv = [shown[0], shown[1] / 100.0, shown[2] / 100.0];
                        let new = rebuild(hsv_to_rgb(hsv).map(decode), rgba[3]);
                        set(rgba, new, outcome);
                    }
                    state.hsv = Some((*rgba, hsv));
                }
                ColorMode::Oklch => {
                    let mut lch = match state.lch {
                        Some((of, lch)) if of == *rgba => lch,
                        _ => linear_to_oklch(base),
                    };
                    let mut shown = [lch[0] * 100.0, lch[1], lch[2]];
                    let mut any = track(ui, &mut shown[0], 0.0..=100.0, 0.5, "L", outcome);
                    any |= track(ui, &mut shown[1], 0.0..=0.4, 0.002, "C", outcome);
                    any |= track(ui, &mut shown[2], 0.0..=360.0, 1.0, "h", outcome);
                    if any {
                        lch = [shown[0] / 100.0, shown[1], shown[2]];
                        let new = rebuild(oklch_to_linear(lch), rgba[3]);
                        set(rgba, new, outcome);
                    }
                    state.lch = Some((*rgba, lch));
                }
            }
            if alpha {
                let mut a = rgba[3];
                if track(ui, &mut a, 0.0..=1.0, 0.005, "A", outcome) {
                    set(rgba, [rgba[0], rgba[1], rgba[2], a], outcome);
                }
            }
        });
        if hdr {
            ui.horizontal(|ui| {
                let (base, mut ev) = split_intensity([rgba[0], rgba[1], rgba[2]]);
                let response = ui.add(
                    egui::DragValue::new(&mut ev)
                        .range(EV_RANGE)
                        .speed(0.05)
                        .prefix("Intensity ")
                        .suffix(" EV"),
                );
                let response = crate::a11y_audit::name(
                    response,
                    egui::WidgetType::DragValue,
                    &format!("{name} intensity"),
                );
                if response.drag_stopped() || response.lost_focus() {
                    outcome.committed = true;
                }
                if response.changed() {
                    let rgb = with_intensity(base, ev);
                    set(rgba, [rgb[0], rgb[1], rgb[2], rgba[3]], outcome);
                }
                ui.weak(format!("x{:.3}", ev.exp2()));
            });
        }
        ui.horizontal(|ui| {
            if state.hex_of != Some(*rgba) {
                state.hex = to_hex(*rgba, alpha);
                state.hex_of = Some(*rgba);
            }
            let response = ui.add(egui::TextEdit::singleline(&mut state.hex).desired_width(90.0));
            let response =
                crate::a11y_audit::name(response, egui::WidgetType::TextEdit, &format!("{name} hex"));
            // Enter surrenders a single-line field's focus, as Tab away does: both apply.
            if response.lost_focus() {
                match parse_hex(&state.hex) {
                    Some(mut new) => {
                        if !alpha {
                            new[3] = rgba[3];
                        }
                        if hdr {
                            // A hex color is a base color: the intensity stays.
                            let rgb = with_intensity(
                                [new[0], new[1], new[2]],
                                split_intensity([rgba[0], rgba[1], rgba[2]]).1,
                            );
                            new = [rgb[0], rgb[1], rgb[2], new[3]];
                        }
                        set(rgba, new, outcome);
                        outcome.committed = true;
                        state.hex_of = None;
                    }
                    None => {
                        state.hex = to_hex(*rgba, alpha);
                        state.hex_of = Some(*rgba);
                    }
                }
            }
            ui.weak("hex");
        });
        let mut pick = None;
        let swatches = links.as_ref().map(|l| l.swatches.clone()).unwrap_or_default();
        ui.horizontal_wrapped(|ui| {
            ui.weak("Swatches");
            for (index, swatch) in swatches.iter().enumerate() {
                let response = swatch_button(ui, *swatch, &format!("Swatch {}", to_hex(*swatch, true)));
                if response.clicked() {
                    pick = Some(*swatch);
                }
                let delete = response.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Delete));
                let mut remove = delete;
                response.context_menu(|ui| {
                    if ui.button("Remove swatch").clicked() {
                        remove = true;
                        ui.close();
                    }
                });
                if remove && let Some(links) = links.as_mut() {
                    links.request(PickerRequest::RemoveSwatch(index));
                }
            }
            let save = ui.button("Save swatch");
            let save = crate::a11y_audit::name(save, egui::WidgetType::Button, "Save swatch");
            if save.clicked()
                && let Some(links) = links.as_mut()
            {
                links.request(PickerRequest::SaveSwatch(*rgba));
            }
        });
        let recent = links.as_ref().map(|l| l.recent.clone()).unwrap_or_default();
        if !recent.is_empty() {
            ui.horizontal_wrapped(|ui| {
                ui.weak("Recent");
                for color in &recent {
                    if swatch_button(ui, *color, &format!("Recent {}", to_hex(*color, true))).clicked() {
                        pick = Some(*color);
                    }
                }
            });
        }
        if let Some(mut picked) = pick {
            if !alpha {
                picked[3] = rgba[3];
            }
            set(rgba, picked, outcome);
            outcome.committed = true;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: [f32; 3], b: [f32; 3], tolerance: f32) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() <= tolerance)
    }

    #[test]
    fn oklab_reads_the_published_reference_values() {
        // White is L = 1 with no chroma; the primaries' published OKLab coordinates.
        assert!(close(linear_to_oklab([1.0, 1.0, 1.0]), [1.0, 0.0, 0.0], 1e-3));
        assert!(close(
            linear_to_oklab([1.0, 0.0, 0.0]),
            [0.6280, 0.2249, 0.1258],
            1e-3
        ));
        assert!(close(
            linear_to_oklab([0.0, 1.0, 0.0]),
            [0.8664, -0.2339, 0.1795],
            1e-3
        ));
        assert!(close(
            linear_to_oklab([0.0, 0.0, 1.0]),
            [0.4520, -0.0325, -0.3115],
            1e-3
        ));
    }

    #[test]
    fn oklch_round_trips_and_out_of_gamut_keeps_the_hue() {
        for rgb in [
            [0.2, 0.5, 0.9],
            [1.0, 0.0, 0.0],
            [0.05, 0.05, 0.05],
            [0.9, 0.8, 0.1],
        ] {
            let back = oklch_to_linear(linear_to_oklch(rgb));
            assert!(close(back, rgb, 2e-3), "{rgb:?} -> {back:?}");
        }
        // A chroma no sRGB color has at this lightness: the result is inside sRGB, same hue.
        let wanted = [0.7, 0.4, 145.0];
        let rgb = oklch_to_linear(wanted);
        assert!(rgb.iter().all(|c| (0.0..=1.0).contains(c)), "{rgb:?}");
        let got = linear_to_oklch(rgb);
        assert!((got[2] - wanted[2]).abs() < 1.0, "hue {got:?}");
        assert!((got[0] - wanted[0]).abs() < 0.01, "lightness {got:?}");
        assert!(got[1] < wanted[1]);
    }

    #[test]
    fn hsv_round_trips() {
        for rgb in [
            [1.0, 0.0, 0.0],
            [0.2, 0.6, 0.4],
            [0.5, 0.5, 0.5],
            [0.1, 0.2, 0.95],
        ] {
            assert!(close(hsv_to_rgb(rgb_to_hsv(rgb)), rgb, 1e-5), "{rgb:?}");
        }
        assert!(close(rgb_to_hsv([0.0, 0.0, 1.0]), [240.0, 1.0, 1.0], 1e-4));
    }

    #[test]
    fn hex_parses_short_long_and_alpha_and_refuses_junk() {
        let red = parse_hex("#ff0000").expect("red");
        assert!(close([red[0], red[1], red[2]], [1.0, 0.0, 0.0], 1e-6) && red[3] == 1.0);
        assert_eq!(parse_hex("F00"), parse_hex("#ff0000"));
        let half = parse_hex("#ffffff80").expect("alpha");
        assert!((half[3] - 128.0 / 255.0).abs() < 1e-6);
        for junk in ["", "#ff00", "#gg0000", "#ff0000ff00", "red", "#ff 000"] {
            assert_eq!(parse_hex(junk), None, "{junk:?}");
        }
        assert_eq!(to_hex([1.0, 0.0, 0.0, 1.0], false), "#ff0000");
        assert_eq!(to_hex([1.0, 0.0, 0.0, 0.0], true), "#ff000000");
        let grey = parse_hex("#7f7f7f").expect("grey");
        assert_eq!(to_hex(grey, false), "#7f7f7f", "hex round-trips through linear");
    }

    #[test]
    fn hdr_intensity_splits_and_rebuilds() {
        let (base, ev) = split_intensity([4.0, 2.0, 0.0]);
        assert!(close(base, [1.0, 0.5, 0.0], 1e-6));
        assert!((ev - 2.0).abs() < 1e-6);
        assert!(close(with_intensity(base, ev), [4.0, 2.0, 0.0], 1e-5));
        assert_eq!(split_intensity([0.5, 0.2, 0.1]), ([0.5, 0.2, 0.1], 0.0));
    }

    #[test]
    fn color_lists_round_trip_and_recent_moves_to_the_front() {
        let red = [1.0, 0.0, 0.0, 1.0];
        let blue = [0.0, 0.0, 1.0, 1.0];
        let mut list = vec![red, blue];
        assert_eq!(parse_hex_list(&hex_list(&list)), list);
        assert_eq!(parse_hex_list("#ff0000ff,junk,#0000ffff"), list);
        push_front_unique(&mut list, blue, 4);
        assert_eq!(list, vec![blue, red]);
        for _ in 0..5 {
            push_front_unique(&mut list, [0.5, 0.5, 0.5, 1.0], 2);
        }
        assert_eq!(list.len(), 2);
    }

    /// Runs one frame of a picker over `rgba` with `links`; returns what it did and the label of
    /// the focused widget.
    fn frame(
        ctx: &egui::Context,
        events: Vec<egui::Event>,
        rgba: &mut [f32; 4],
        links: &mut PickerLinks,
        hdr: bool,
    ) -> (PickerOutcome, Option<String>) {
        ctx.enable_accesskit();
        let mut outcome = PickerOutcome::default();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            events,
            ..Default::default()
        };
        let mut output = ctx.run_ui(raw, |ui| {
            let got = color_picker(ui, egui::Id::new("tint"), rgba, true, hdr, "Tint", Some(links));
            outcome.changed |= got.changed;
            outcome.committed |= got.committed;
        });
        output.textures_delta.clear();
        let focus = output.platform_output.accesskit_update.and_then(|update| {
            update
                .nodes
                .iter()
                .find(|(id, _)| *id == update.focus)
                .and_then(|(_, node)| node.label().map(str::to_owned))
        });
        (outcome, focus)
    }

    fn key(key: egui::Key, modifiers: egui::Modifiers) -> Vec<egui::Event> {
        [true, false]
            .into_iter()
            .map(|pressed| egui::Event::Key {
                key,
                physical_key: None,
                pressed,
                repeat: false,
                modifiers,
            })
            .collect()
    }

    /// Presses Tab until the widget called `label` has focus.
    fn tab_to(ctx: &egui::Context, rgba: &mut [f32; 4], links: &mut PickerLinks, hdr: bool, label: &str) {
        let mut seen = Vec::new();
        for _ in 0..40 {
            let (_, focus) = frame(ctx, key(egui::Key::Tab, egui::Modifiers::NONE), rgba, links, hdr);
            if focus.as_deref() == Some(label) {
                return;
            }
            seen.push(focus.unwrap_or_default());
        }
        panic!("Tab never reached {label:?}: {seen:?}");
    }

    /// The keyboard path: Tab to the swatch, Enter opens the picker, Tab to the hex field, type a
    /// color, Enter applies it; then Tab to **Save swatch** and press Enter.
    #[test]
    fn the_picker_is_operable_from_the_keyboard() {
        let ctx = egui::Context::default();
        let mut rgba = [0.2, 0.2, 0.2, 1.0];
        let mut links = PickerLinks::default();
        frame(&ctx, Vec::new(), &mut rgba, &mut links, false);
        tab_to(&ctx, &mut rgba, &mut links, false, "Tint color picker");
        frame(
            &ctx,
            key(egui::Key::Enter, egui::Modifiers::NONE),
            &mut rgba,
            &mut links,
            false,
        );
        frame(&ctx, Vec::new(), &mut rgba, &mut links, false);
        tab_to(&ctx, &mut rgba, &mut links, false, "Tint hex");
        frame(
            &ctx,
            key(egui::Key::A, egui::Modifiers::COMMAND),
            &mut rgba,
            &mut links,
            false,
        );
        frame(
            &ctx,
            vec![egui::Event::Text("#ff0000".to_owned())],
            &mut rgba,
            &mut links,
            false,
        );
        let (outcome, _) = frame(
            &ctx,
            key(egui::Key::Enter, egui::Modifiers::NONE),
            &mut rgba,
            &mut links,
            false,
        );
        assert!(outcome.changed && outcome.committed, "{outcome:?}");
        assert!(
            close([rgba[0], rgba[1], rgba[2]], [1.0, 0.0, 0.0], 1e-6),
            "{rgba:?}"
        );
        assert_eq!(rgba[3], 1.0);
        tab_to(&ctx, &mut rgba, &mut links, false, "Save swatch");
        frame(
            &ctx,
            key(egui::Key::Enter, egui::Modifiers::NONE),
            &mut rgba,
            &mut links,
            false,
        );
        assert_eq!(
            links.take_requests(),
            vec![PickerRequest::SaveSwatch([1.0, 0.0, 0.0, 1.0])]
        );
        // The OKLCH mode from the keyboard: its lightness field takes a typed value.
        tab_to(&ctx, &mut rgba, &mut links, false, "Tint OKLCH mode");
        frame(
            &ctx,
            key(egui::Key::Enter, egui::Modifiers::NONE),
            &mut rgba,
            &mut links,
            false,
        );
        tab_to(&ctx, &mut rgba, &mut links, false, "Tint L");
        let before = linear_to_oklch([rgba[0], rgba[1], rgba[2]]);
        let (outcome, _) = frame(
            &ctx,
            key(egui::Key::ArrowUp, egui::Modifiers::NONE),
            &mut rgba,
            &mut links,
            false,
        );
        let after = linear_to_oklch([rgba[0], rgba[1], rgba[2]]);
        assert!(outcome.changed, "arrow up steps the focused channel");
        assert!(after[0] > before[0], "{before:?} -> {after:?}");
    }

    /// A saved swatch applies with Enter; the HDR intensity keeps the base color.
    #[test]
    fn a_swatch_applies_and_hdr_intensity_scales() {
        let ctx = egui::Context::default();
        let mut rgba = [2.0, 1.0, 0.0, 1.0];
        let mut links = PickerLinks::default();
        links.swatches = vec![[0.0, 0.0, 1.0, 1.0]];
        frame(&ctx, Vec::new(), &mut rgba, &mut links, true);
        tab_to(&ctx, &mut rgba, &mut links, true, "Tint color picker");
        frame(
            &ctx,
            key(egui::Key::Enter, egui::Modifiers::NONE),
            &mut rgba,
            &mut links,
            true,
        );
        frame(&ctx, Vec::new(), &mut rgba, &mut links, true);
        tab_to(&ctx, &mut rgba, &mut links, true, "Swatch #0000ffff");
        let (outcome, _) = frame(
            &ctx,
            key(egui::Key::Enter, egui::Modifiers::NONE),
            &mut rgba,
            &mut links,
            true,
        );
        assert!(outcome.changed && outcome.committed, "{outcome:?}");
        assert_eq!(rgba, [0.0, 0.0, 1.0, 1.0]);
    }
}
