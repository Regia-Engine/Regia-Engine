// SPDX-License-Identifier: MPL-2.0
//! Bit-flag and layer-mask fields (UI parity addendum § 7.8, ADR-7785).
//!
//! An integer field with `@Flags("a, b, c")` draws one checkbox per named bit. An integer field
//! with `@LayerMask("physics")`, and the scene's `Layers` component, draw the bit-layer grid: one
//! toggle cell per layer, named from the project's collision layers (`config/physics.ron`, the
//! names the Project Settings matrix edits) or numbered for another domain. Pressing a cell and
//! dragging across others paints them all to the pressed cell's new state; each cell is also a
//! focusable toggle (Space or Enter flips it), so the grid works from the keyboard.

use std::any::Any;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

/// The bits of an integer value and its width, for the unsigned and signed integer types.
#[must_use]
pub fn read_bits(value: &dyn Any) -> Option<(u64, u32)> {
    macro_rules! unsigned {
        ($($ty:ty),*) => {$(
            if let Some(v) = value.downcast_ref::<$ty>() {
                return Some((u64::from(*v), <$ty>::BITS));
            }
        )*};
    }
    macro_rules! signed {
        ($($ty:ty => $unsigned:ty),*) => {$(
            if let Some(v) = value.downcast_ref::<$ty>() {
                return Some((u64::from(v.cast_unsigned()), <$ty>::BITS));
            }
        )*};
    }
    unsigned!(u8, u16, u32, u64);
    signed!(i8 => u8, i16 => u16, i32 => u32, i64 => u64);
    if let Some(v) = value.downcast_ref::<usize>() {
        return Some((*v as u64, usize::BITS));
    }
    if let Some(v) = value.downcast_ref::<isize>() {
        return Some((v.cast_unsigned() as u64, isize::BITS));
    }
    None
}

/// Writes `bits` (masked to the type's width) into an integer value; false for another type.
pub fn write_bits(value: &mut dyn Any, bits: u64) -> bool {
    macro_rules! unsigned {
        ($($ty:ty),*) => {$(
            if let Some(v) = value.downcast_mut::<$ty>() {
                *v = bits as $ty;
                return true;
            }
        )*};
    }
    macro_rules! signed {
        ($($ty:ty => $unsigned:ty),*) => {$(
            if let Some(v) = value.downcast_mut::<$ty>() {
                *v = (bits as $unsigned).cast_signed();
                return true;
            }
        )*};
    }
    unsigned!(u8, u16, u32, usize);
    signed!(i8 => u8, i16 => u16, i32 => u32, i64 => u64, isize => usize);
    if let Some(v) = value.downcast_mut::<u64>() {
        *v = bits;
        return true;
    }
    false
}

/// `mask` with `bit` set to `on` (bits at or past 64 leave it unchanged).
#[must_use]
pub fn set_bit(mask: u64, bit: u32, on: bool) -> u64 {
    if bit >= 64 {
        return mask;
    }
    if on { mask | (1 << bit) } else { mask & !(1 << bit) }
}

/// Whether `bit` is set in `mask`.
#[must_use]
pub fn bit_on(mask: u64, bit: u32) -> bool {
    bit < 64 && mask & (1 << bit) != 0
}

/// One drag-paint stroke over the grid: the state the first cell took, which every cell the
/// pointer then crosses takes too. Applying a stroke is [`paint_stroke`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stroke {
    on: bool,
}

/// `mask` after a paint stroke that pressed `first` and crossed `crossed`: the pressed cell
/// flips, and every crossed cell takes the pressed cell's new state.
#[must_use]
pub fn paint_stroke(mask: u64, first: u32, crossed: &[u32]) -> u64 {
    let on = !bit_on(mask, first);
    let mut out = set_bit(mask, first, on);
    for &bit in crossed {
        out = set_bit(out, bit, on);
    }
    out
}

/// The names of the layers of `domain`: the project's collision layers for `"physics"` (the
/// engine default without a project or a readable `config/physics.ron`), `Layer 0` … for any other
/// domain, `count` of them. Physics names are cached per file modification time.
#[must_use]
pub fn layer_names(domain: &str, root: Option<&Path>, count: u32) -> Vec<String> {
    if domain.eq_ignore_ascii_case("physics") {
        return physics_layer_names(root);
    }
    (0..count).map(|bit| format!("Layer {bit}")).collect()
}

/// Layer names per config path, with the file's modification time when read.
type NameCache = HashMap<PathBuf, (Option<SystemTime>, Vec<String>)>;

fn physics_layer_names(root: Option<&Path>) -> Vec<String> {
    let Some(root) = root else {
        return regia_physics::LayerFile::default().layers;
    };
    static CACHE: OnceLock<Mutex<NameCache>> = OnceLock::new();
    let path = crate::physics_matrix::config_path(root);
    let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(cache) = cache.lock()
        && let Some((when, names)) = cache.get(&path)
        && *when == modified
    {
        return names.clone();
    }
    let names = match crate::physics_matrix::load_matrix(root) {
        Ok((file, _)) => file.layers,
        Err(_) => regia_physics::LayerFile::default().layers,
    };
    if let Ok(mut cache) = cache.lock() {
        cache.insert(path, (modified, names.clone()));
    }
    names
}

/// One checkbox per named bit of `bits`; returns the new bits when one was flipped.
pub fn flags_ui(ui: &mut egui::Ui, bits: u64, names: &[(u32, &str)], field: &str) -> Option<u64> {
    let mut out = None;
    ui.horizontal_wrapped(|ui| {
        for &(bit, name) in names {
            let mut on = bit_on(bits, bit);
            let response = ui.checkbox(&mut on, name);
            let enabled = response.enabled();
            response.widget_info(|| {
                egui::WidgetInfo::selected(
                    egui::WidgetType::Checkbox,
                    enabled,
                    on,
                    format!("{field}: {name}"),
                )
            });
            if response.changed() {
                out = Some(set_bit(out.unwrap_or(bits), bit, on));
            }
        }
    });
    out
}

/// Cells per grid row.
pub const GRID_COLUMNS: usize = 8;

/// The width of a grid cell, in points (it scales with the UI scale like every point size).
const CELL_WIDTH: f32 = 56.0;

/// The bit-layer grid: a toggle cell per layer name (`names[i]` is bit `i`). A press flips the
/// cell and starts a paint stroke; cells the pointer crosses while the button is held take the
/// same state. Returns the new mask when it changed.
pub fn layer_grid(ui: &mut egui::Ui, id: egui::Id, bits: u64, names: &[String], field: &str) -> Option<u64> {
    let stroke_id = id.with("layer_paint");
    let mut mask = bits;
    let (primary_down, pointer) = ui.input(|i| (i.pointer.primary_down(), i.pointer.interact_pos()));
    let mut stroke: Option<Stroke> = ui.data(|d| d.get_temp(stroke_id));
    // The release that ends a stroke also reports a click on the cell under it: not a toggle.
    let ending = !primary_down && stroke.is_some();
    if ending {
        stroke = None;
        ui.data_mut(|d| d.remove::<Stroke>(stroke_id));
    }
    let visuals = ui.visuals().clone();
    egui::Grid::new(id.with("layer_grid"))
        .spacing(egui::vec2(2.0, 2.0))
        .show(ui, |ui| {
            for (bit, name) in names.iter().enumerate().take(64) {
                let bit = bit as u32;
                let on = bit_on(mask, bit);
                let height = ui.spacing().interact_size.y;
                let (rect, response) =
                    ui.allocate_exact_size(egui::vec2(CELL_WIDTH, height), egui::Sense::click());
                let spoken = format!("{field}: {name}");
                let enabled = response.enabled();
                response.widget_info(|| {
                    egui::WidgetInfo::selected(egui::WidgetType::Checkbox, enabled, on, spoken.clone())
                });
                let response = response.on_hover_text(format!("{name} (bit {bit})"));
                let pressed_here = response.is_pointer_button_down_on() && stroke.is_none();
                if pressed_here {
                    // The press starts a stroke: this cell flips, the stroke remembers the state.
                    let next = !on;
                    mask = set_bit(mask, bit, next);
                    stroke = Some(Stroke { on: next });
                    ui.data_mut(|d| d.insert_temp(stroke_id, Stroke { on: next }));
                } else if let Some(Stroke { on: paint }) = stroke
                    && primary_down
                    && pointer.is_some_and(|p| rect.contains(p))
                    && on != paint
                {
                    mask = set_bit(mask, bit, paint);
                } else if response.clicked() && !ending && stroke.is_none() {
                    // Keyboard activation (Space or Enter on the focused cell).
                    mask = set_bit(mask, bit, !on);
                }
                let on = bit_on(mask, bit);
                let fill = if on {
                    visuals.selection.bg_fill
                } else {
                    visuals.widgets.inactive.bg_fill
                };
                let text_color = if on {
                    visuals.selection.stroke.color
                } else {
                    visuals.widgets.inactive.fg_stroke.color
                };
                ui.painter().rect_filled(rect, 2.0, fill);
                if response.has_focus() {
                    ui.painter()
                        .rect_stroke(rect, 2.0, visuals.selection.stroke, egui::StrokeKind::Inside);
                }
                let short: String = name.chars().take(7).collect();
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    short,
                    egui::TextStyle::Small.resolve(ui.style()),
                    text_color,
                );
                if (bit as usize + 1).is_multiple_of(GRID_COLUMNS) {
                    ui.end_row();
                }
            }
        });
    (mask != bits).then_some(mask)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bits_round_trip_through_every_integer_type() {
        let mut a: u8 = 0b1010;
        assert_eq!(read_bits(&a), Some((0b1010, 8)));
        assert!(write_bits(&mut a, 0x1ff));
        assert_eq!(a, 0xff, "masked to the width");
        let mut b: i32 = -1;
        assert_eq!(read_bits(&b), Some((0xffff_ffff, 32)));
        assert!(write_bits(&mut b, 0x7fff_ffff));
        assert_eq!(b, i32::MAX);
        let mut c: u64 = 0;
        assert!(write_bits(&mut c, 1 << 63 | 1));
        assert_eq!(read_bits(&c), Some((1 << 63 | 1, 64)));
        let mut d = 0.5f32;
        assert_eq!(read_bits(&d), None);
        assert!(!write_bits(&mut d, 1));
    }

    #[test]
    fn a_paint_stroke_gives_every_crossed_cell_the_first_cells_new_state() {
        // Bit 0 off: the stroke turns 0, 1, 2 on (1 was already on, 3 untouched).
        assert_eq!(paint_stroke(0b0010, 0, &[1, 2]), 0b0111);
        // Bit 1 on: the stroke turns 1, 2, 3 off.
        assert_eq!(paint_stroke(0b1111, 1, &[2, 3]), 0b0001);
        assert_eq!(set_bit(5, 64, true), 5);
        assert!(!bit_on(u64::MAX, 64));
    }

    #[test]
    fn non_physics_domains_number_their_layers() {
        assert_eq!(
            layer_names("render", None, 3),
            vec!["Layer 0", "Layer 1", "Layer 2"]
        );
        assert_eq!(
            layer_names("physics", None, 32),
            regia_physics::LayerFile::default().layers
        );
    }
}
