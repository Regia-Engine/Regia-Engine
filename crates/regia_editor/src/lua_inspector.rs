// SPDX-License-Identifier: MPL-2.0
//! The inspector drawer for Lua script components ([`regia_script_lua::LuaComponent`], ADR-3356):
//! typed widgets driven by the mod's declaration (defaults, ranges, enum options, asset filters),
//! reusing the panel's asset list. Edits replace the whole component through one `SetProperty`
//! command, so they are undoable like any inspector edit.

use std::sync::Arc;

use regia_app::prelude::*;
use regia_ecs::reflect::PartialReflect;
use regia_script_lua::{LuaComponent, LuaMods, LuaValue, PropType, ScriptValue};

use crate::dnd::ContentKind;
use crate::inspector::Change;

/// The reflected type path the panel special-cases.
pub(crate) const TYPE_PATH: &str = "regia_script_lua::components::LuaComponent";

/// Draws one entity's script component and returns the edits made (one whole-component change).
pub(crate) fn component_ui(
    ui: &mut egui::Ui,
    world: &mut World,
    value: &mut dyn PartialReflect,
    assets: &Arc<[(String, ContentKind)]>,
) -> Vec<Change> {
    let Some(component) = value.try_downcast_mut::<LuaComponent>() else {
        return Vec::new();
    };
    // The declaration lives in the mod's Lua state; clone it out so the world is free below.
    let decl = world.get_resource::<LuaMods>().and_then(|mods| {
        mods.iter().find_map(|m| {
            m.components()
                .into_iter()
                .find(|decl| decl.name == component.decl)
        })
    });
    let Some(decl) = decl else {
        ui.weak(format!(
            "script component `{}` — its mod is not loaded; values cannot be edited",
            component.decl
        ));
        return Vec::new();
    };
    ui.weak(format!("declared by {}", decl.mod_id));
    let mut edited = false;
    for prop in &decl.props {
        ui.horizontal(|ui| {
            ui.label(&prop.name);
            edited |= prop_ui(ui, world, component, prop, assets);
        });
    }
    if !edited {
        return Vec::new();
    }
    vec![Change {
        path: String::new(),
        discrete: false,
    }]
}

/// One property's widget, writing into the component's value map.
fn prop_ui(
    ui: &mut egui::Ui,
    world: &mut World,
    component: &mut LuaComponent,
    prop: &regia_script_lua::PropDecl,
    assets: &Arc<[(String, ContentKind)]>,
) -> bool {
    let current = component.props.get(&prop.name).cloned().unwrap_or(LuaValue::Nil);
    match prop.ty {
        PropType::Number => {
            let LuaValue::Number(mut n) = current else {
                return false;
            };
            let mut widget = egui::DragValue::new(&mut n).speed(prop.step.unwrap_or(0.01));
            if let (Some(min), Some(max)) = (prop.min, prop.max) {
                widget = widget.range(min..=max);
            }
            if egui::Widget::ui(widget, ui).changed() {
                let coerced = prop
                    .coerce(ScriptValue::Number(n))
                    .unwrap_or(ScriptValue::Number(n));
                if let ScriptValue::Number(n) = coerced {
                    component.props.insert(prop.name.clone(), LuaValue::Number(n));
                    return true;
                }
            }
            false
        }
        PropType::Bool => {
            let LuaValue::Bool(mut b) = current else {
                return false;
            };
            if ui.checkbox(&mut b, "").changed() {
                component.props.insert(prop.name.clone(), LuaValue::Bool(b));
                return true;
            }
            false
        }
        PropType::String => {
            let LuaValue::String(mut s) = current else {
                return false;
            };
            let changed = if prop.multiline {
                ui.add(egui::TextEdit::multiline(&mut s).desired_rows(3))
                    .changed()
            } else {
                ui.add(egui::TextEdit::singleline(&mut s).desired_width(160.0))
                    .changed()
            };
            if changed {
                component.props.insert(prop.name.clone(), LuaValue::String(s));
                return true;
            }
            false
        }
        PropType::Asset => {
            let LuaValue::String(mut s) = current else {
                return false;
            };
            let filter = prop.filter.clone().unwrap_or_default();
            let current_value = s.clone();
            let choices: Vec<&str> = assets
                .iter()
                .filter(|(path, kind)| crate::inspector::impls::asset_matches(&filter, path, *kind))
                .map(|(path, _)| path.as_str())
                .collect();
            let changed = ui
                .add(
                    crate::inspector::dropdown::DropDownBox::from_iter(
                        choices,
                        ui.id().with(("script_asset", &prop.name)),
                        &mut s,
                        |ui, text| ui.selectable_label(text == current_value, text),
                    )
                    .hint_text(&filter)
                    .filter_by_input(false)
                    .desired_width(180.0),
                )
                .changed();
            if changed {
                component.props.insert(prop.name.clone(), LuaValue::String(s));
                return true;
            }
            false
        }
        PropType::Vec3 => {
            let LuaValue::Vec3(mut v) = current else {
                return false;
            };
            let mut changed = false;
            for (index, prefix) in v.iter_mut().zip(["x ", "y ", "z "]) {
                changed |= ui
                    .add(egui::DragValue::new(index).speed(0.01).prefix(prefix))
                    .changed();
            }
            if changed {
                component.props.insert(prop.name.clone(), LuaValue::Vec3(v));
                return true;
            }
            false
        }
        PropType::Color => {
            // Stored as a list of four linear channels.
            let mut rgba = match &current {
                LuaValue::List(items) => {
                    let mut channels = [1.0f32; 4];
                    for (index, item) in items.iter().take(4).enumerate() {
                        if let LuaValue::Number(n) = item {
                            channels[index] = *n as f32;
                        }
                    }
                    channels
                }
                _ => [1.0; 4],
            };
            let mut srgb = [
                crate::inspector::linear_to_srgb_u8(rgba[0]),
                crate::inspector::linear_to_srgb_u8(rgba[1]),
                crate::inspector::linear_to_srgb_u8(rgba[2]),
                (rgba[3].clamp(0.0, 1.0) * 255.0).round() as u8,
            ];
            let picker = ui.color_edit_button_srgba_unmultiplied(&mut srgb);
            let mut numbers_changed = false;
            for (channel, prefix) in rgba.iter_mut().zip(["r ", "g ", "b ", "a "]) {
                numbers_changed |= ui
                    .add(
                        egui::DragValue::new(channel)
                            .speed(0.01)
                            .range(0.0..=f32::MAX)
                            .prefix(prefix),
                    )
                    .changed();
            }
            let next: [f32; 4] = if picker.changed() {
                [
                    crate::inspector::srgb8_to_linear(srgb[0]),
                    crate::inspector::srgb8_to_linear(srgb[1]),
                    crate::inspector::srgb8_to_linear(srgb[2]),
                    f32::from(srgb[3]) / 255.0,
                ]
            } else if numbers_changed {
                rgba
            } else {
                return false;
            };
            component.props.insert(
                prop.name.clone(),
                LuaValue::List(next.iter().map(|c| LuaValue::Number(f64::from(*c))).collect()),
            );
            true
        }
        PropType::Entity => {
            let bits = match current {
                LuaValue::Entity(bits) => Some(bits),
                _ => None,
            };
            let name = bits
                .and_then(Entity::try_from_bits)
                .and_then(|entity| world.get::<regia_scene::components::Name>(entity))
                .map(|name| name.as_str().to_owned());
            let mut cleared = false;
            match (&bits, name) {
                (Some(bits), Some(name)) => {
                    ui.weak(format!("{name} ({bits})"));
                }
                (Some(bits), None) => {
                    ui.weak(format!("entity {bits}"));
                }
                _ => {
                    ui.weak("none");
                }
            }
            if bits.is_some() && ui.small_button("clear").clicked() {
                cleared = true;
            }
            if cleared {
                component.props.insert(prop.name.clone(), LuaValue::Nil);
                return true;
            }
            false
        }
        PropType::Enum => {
            let LuaValue::String(mut s) = current else {
                return false;
            };
            let id = ui.id().with(("script_enum", &prop.name));
            let mut changed = false;
            egui::ComboBox::from_id_salt(id)
                .selected_text(&s)
                .show_ui(ui, |ui| {
                    for option in &prop.options {
                        changed |= ui.selectable_value(&mut s, option.clone(), option).changed();
                    }
                });
            if changed {
                component.props.insert(prop.name.clone(), LuaValue::String(s));
                return true;
            }
            false
        }
    }
}
