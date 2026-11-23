// SPDX-License-Identifier: MIT-0
//! The theme sheet page (lane D3): structure, state resolution, size, gamepad reachability, and
//! the baseline goldens for both built-in schemes.
//!
//! Goldens: `tests/goldens/theme_sheet_{dark,light}.png`. Bless on the lavapipe reference with
//! `REGIA_BLESS=1 cargo test -p ui_gallery --test theme_sheet` (reason for the commit message:
//! "baseline before default-design lanes"). Until a golden file exists the capture is only
//! checked for being non-blank and the comparison is reported as not done.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::path::PathBuf;

use regia::prelude::*;
use regia_ui::theme::UiThemes;
use regia_ui::{
    Button, ComputedNode, DISABLED_OPACITY, Focusable, NodeName, ScaleMode, Themed, UiDensity, UiEvent,
    UiEventKind,
};
use ui_gallery::gallery::UiGallery;
use ui_gallery::shots::{capture, headless_app, resize};
use ui_gallery::theme_sheet::{SHEET_SIZES, SHEET_STATES, SHEET_WIDGETS, SheetCell, SheetState, sheet_cells};

fn main() {}

const SHEET_W: u32 = 1600;
const SHEET_H: u32 = 1500;

/// A gallery app showing the theme sheet page (opened the way a gamepad does: a click event on the
/// focusable toolbar button).
fn sheet_app() -> (App, HashMap<String, Entity>) {
    let mut app = headless_app(UiGallery);
    for _ in 0..4 {
        app.update();
    }
    let button = find(&mut app, "gallery.page.theme_sheet");
    app.world_mut()
        .write_message(UiEvent::new(button, UiEventKind::Click));
    resize(&mut app, SHEET_W, SHEET_H, Some(ScaleMode::ConstantPixelSize));
    for _ in 0..8 {
        app.update();
    }
    let world = app.world_mut();
    let names = world
        .query::<(Entity, &NodeName)>()
        .iter(world)
        .map(|(e, n)| (n.0.clone(), e))
        .collect();
    (app, names)
}

fn find(app: &mut App, name: &str) -> Entity {
    let world = app.world_mut();
    world
        .query::<(Entity, &NodeName)>()
        .iter(world)
        .find(|(_, n)| n.0 == name)
        .map(|(e, _)| e)
        .unwrap_or_else(|| panic!("no node named {name}"))
}

fn themed(app: &App, e: Entity) -> Themed {
    *app.world().get::<Themed>(e).expect("themed")
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn sheet_has_every_widget_state_and_size() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (app, names) = sheet_app();
    let cells = sheet_cells();
    assert_eq!(
        cells.len(),
        SHEET_WIDGETS.len() * SHEET_SIZES.len() * SHEET_STATES.len()
    );
    for cell in &cells {
        let name = cell.node_name();
        let e = *names.get(&name).unwrap_or_else(|| panic!("missing {name}"));
        let computed = app.world().get::<ComputedNode>(e).expect("computed");
        assert!(
            computed.rect[2] > 8.0 && computed.rect[3] > 8.0,
            "{name} was not laid out: {:?}",
            computed.rect
        );
    }
}

#[test]
fn sheet_sizes_follow_the_control_height_table() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (app, names) = sheet_app();
    for size in SHEET_SIZES {
        let cell = SheetCell {
            widget: SHEET_WIDGETS[0],
            state: SheetState::Normal,
            size,
        };
        let e = names[&cell.node_name()];
        let h = app.world().get::<ComputedNode>(e).unwrap().rect[3];
        let want = UiDensity::Comfortable.control_height(size);
        assert!((h - want).abs() < 0.5, "{size:?}: height {h}, want {want}");
    }
}

#[test]
fn disabled_cells_dim_and_normal_cells_do_not() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (app, names) = sheet_app();
    for widget in SHEET_WIDGETS {
        let at = |state| {
            let cell = SheetCell {
                widget,
                state,
                size: SHEET_SIZES[1],
            };
            themed(&app, names[&cell.node_name()])
        };
        let normal = at(SheetState::Normal);
        let disabled = at(SheetState::Disabled);
        assert!(
            normal.opacity.is_none_or(|o| o >= 1.0),
            "{widget:?} normal is dimmed: {:?}",
            normal.opacity
        );
        assert!(
            disabled.opacity.is_some_and(|o| o <= DISABLED_OPACITY + 1e-3),
            "{widget:?} disabled is not dimmed: {:?}",
            disabled.opacity
        );
    }
}

#[test]
fn button_states_resolve_to_distinct_looks() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (app, names) = sheet_app();
    let look = |state| {
        let cell = SheetCell {
            widget: SHEET_WIDGETS[0],
            state,
            size: SHEET_SIZES[1],
        };
        themed(&app, names[&cell.node_name()])
    };
    let normal = look(SheetState::Normal);
    for state in [SheetState::Hover, SheetState::Pressed, SheetState::Disabled] {
        assert_ne!(look(state), normal, "{state:?} looks the same as normal");
    }
}

#[test]
fn both_schemes_resolve_differently() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (mut app, names) = sheet_app();
    let cell = SheetCell {
        widget: SHEET_WIDGETS[0],
        state: SheetState::Normal,
        size: SHEET_SIZES[1],
    };
    let e = names[&cell.node_name()];
    let dark = themed(&app, e).background;
    app.world_mut().resource_mut::<UiThemes>().toggle();
    for _ in 0..12 {
        app.update();
    }
    let light = themed(&app, e).background;
    assert!(dark.is_some() && light.is_some());
    assert_ne!(dark, light, "toggling the scheme must change the button fill");
}

#[test]
fn page_button_is_a_focusable_button_for_gamepad_navigation() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (app, names) = sheet_app();
    let e = names["gallery.page.theme_sheet"];
    assert!(app.world().get::<Focusable>(e).is_some());
    assert!(app.world().get::<Button>(e).is_some());
}

fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("goldens")
        .join(format!("{name}.png"))
}

fn check_scheme(name: &str, light: bool) {
    let (mut app, _) = sheet_app();
    if light {
        app.world_mut().resource_mut::<UiThemes>().toggle();
        for _ in 0..12 {
            app.update();
        }
    }
    let device = match regia_render::RenderDevice::headless() {
        Ok(d) => d,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe): {err}"),
    };
    let pixels = capture(&app, &device, SHEET_W, SHEET_H, None).expect("capture");
    let (texels, _) = pixels.as_chunks::<4>();
    let first = texels[0];
    assert!(
        texels.iter().any(|px| *px != first),
        "{name}: capture is one flat color"
    );
    let path = golden_path(name);
    let blessing = std::env::var("REGIA_BLESS").as_deref() == Ok("1");
    if !blessing && !path.exists() {
        eprintln!(
            "TODO(lead): golden {} is not blessed yet; comparison skipped",
            path.display()
        );
        return;
    }
    regia_test_utils::golden::compare_rgba8_to_png_on(
        &device.adapter.get_info(),
        &pixels,
        SHEET_W,
        SHEET_H,
        &path,
        0.98,
    )
    .unwrap_or_else(|err| panic!("{name}: {err}"));
}

#[test]
fn theme_sheet_dark_golden() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    check_scheme("theme_sheet_dark", false);
}

#[test]
fn theme_sheet_light_golden() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    check_scheme("theme_sheet_light", true);
}
