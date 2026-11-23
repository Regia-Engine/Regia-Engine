// SPDX-License-Identifier: MPL-2.0
//! Widget style surface (lane ui-d2-widget-style): `Selected` / `Invalid` / `WidgetSize` markers,
//! density and the disabled rule flow through `apply_themes` into `Themed`, and nodes without the
//! new markers keep the pre-D2 look.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_app::prelude::*;
use regia_ui::prelude::*;
use regia_ui::theme::{StyleClass, VisualOverrides};
use regia_ui::theme_resolve::ThemeOverride;
use regia_ui::{Hovered, UiPointer, UiThemes};

fn ui_app() -> App {
    let mut app = App::new();
    app.add_plugins(UiPlugin::code_only());
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
    app
}

fn spawn_button(app: &mut App) -> Entity {
    app.world_mut()
        .spawn((UiNode, Classes(vec!["button".to_owned()])))
        .id()
}

fn themed(app: &App, e: Entity) -> Themed {
    *app.world().get::<Themed>(e).unwrap()
}

fn settle(app: &mut App) {
    for _ in 0..4 {
        app.update();
    }
}

#[test]
fn plain_button_keeps_the_old_look() {
    let mut app = ui_app();
    let e = spawn_button(&mut app);
    settle(&mut app);
    let t = themed(&app, e);
    let theme = UiThemes::default();
    assert_eq!(t.border_color, Some(theme.current().colors.border));
    assert_eq!(t.border_width, Some(1.0));
    assert_eq!(t.opacity, None);
    assert_eq!(t.height, None);
    assert_eq!(t.padding_x, None);
}

#[test]
fn size_and_density_resolve_height_and_padding() {
    let mut app = ui_app();
    let e = spawn_button(&mut app);
    app.world_mut().entity_mut(e).insert(WidgetSize::Lg);
    settle(&mut app);
    assert_eq!(themed(&app, e).height, Some(56.0));
    assert_eq!(themed(&app, e).padding_x, Some(24.0));
    app.insert_resource(UiDensity::Compact);
    settle(&mut app);
    assert_eq!(themed(&app, e).height, Some(36.0));
    assert_eq!(themed(&app, e).padding_x, Some(12.0));
}

#[test]
fn selected_and_invalid_change_the_look() {
    let mut app = ui_app();
    let e = spawn_button(&mut app);
    settle(&mut app);
    let before = themed(&app, e);
    app.world_mut().entity_mut(e).insert((Selected, Invalid));
    settle(&mut app);
    let after = themed(&app, e);
    assert_ne!(after.background, before.background, "selected fill");
    assert_eq!(
        after.border_color,
        Some(UiThemes::default().current().colors.danger)
    );
    app.world_mut().entity_mut(e).remove::<(Selected, Invalid)>();
    settle(&mut app);
    assert_eq!(themed(&app, e).border_color, before.border_color);
}

/// Lane D6 (ADR-289) turned the spec § 3 disabled rule on by default: 0.45 over the normal look.
/// `disabled_opacity = 1.0` turns it off.
#[test]
fn disabled_opacity_is_on_by_default_and_off_when_set_to_one() {
    let mut app = ui_app();
    let e = spawn_button(&mut app);
    app.world_mut().entity_mut(e).insert(Disabled);
    settle(&mut app);
    assert_eq!(themed(&app, e).opacity, Some(regia_ui::DISABLED_OPACITY));
    assert_eq!(themed(&app, e).opacity, Some(0.45));
    app.world_mut().resource_mut::<UiThemes>().disabled_opacity = 1.0;
    settle(&mut app);
    assert_eq!(themed(&app, e).opacity, None, "rule off");
    app.world_mut().resource_mut::<UiThemes>().disabled_opacity = regia_ui::DISABLED_OPACITY;
    settle(&mut app);
    assert_eq!(themed(&app, e).opacity, Some(0.45));
    app.world_mut().entity_mut(e).remove::<Disabled>();
    settle(&mut app);
    assert_eq!(themed(&app, e).opacity, None);
}

fn override_class(base: VisualOverrides) -> ThemeOverride {
    ThemeOverride(StyleClass {
        base,
        ..Default::default()
    })
}

#[test]
fn transform_focus_ring_and_extra_visuals_reach_themed() {
    let mut app = ui_app();
    let e = spawn_button(&mut app);
    app.world_mut()
        .entity_mut(e)
        .insert(override_class(VisualOverrides {
            translate: Some([1.0, 2.0]),
            scale: Some(1.05),
            focus_ring_width: Some(3.0),
            focus_ring_offset: Some(2.0),
            min_width: Some(80.0),
            gap: Some(6.0),
            text_px: Some(15.0),
            icon_color: Some(UiColor::linear(0.5, 0.5, 0.5, 1.0)),
            elevation: Some(2),
            ..Default::default()
        }));
    settle(&mut app);
    let t = themed(&app, e);
    assert_eq!(t.translate, Some([1.0, 2.0]));
    assert_eq!(t.scale, Some(1.05));
    assert_eq!(t.focus_ring_width, Some(3.0));
    assert_eq!(t.focus_ring_offset, Some(2.0));
    assert_eq!(t.min_width, Some(80.0));
    assert_eq!(t.gap, Some(6.0));
    assert_eq!(t.text_px, Some(15.0));
    assert_eq!(t.icon_color, Some(UiColor::linear(0.5, 0.5, 0.5, 1.0)));
    assert_eq!(t.elevation, Some(2));
}

#[test]
fn touch_pointer_suppresses_the_hover_layer() {
    let mut app = ui_app();
    let e = spawn_button(&mut app);
    let hover = UiColor::linear(0.9, 0.1, 0.1, 1.0);
    app.world_mut().entity_mut(e).insert(ThemeOverride(StyleClass {
        hover: VisualOverrides {
            border_color: Some(hover),
            ..Default::default()
        },
        ..Default::default()
    }));
    settle(&mut app);
    app.world_mut().entity_mut(e).insert(Hovered);
    settle(&mut app);
    assert_eq!(themed(&app, e).border_color, Some(hover), "mouse hover applies");
    app.world_mut().resource_mut::<UiPointer>().active_touch = Some(1);
    settle(&mut app);
    assert_ne!(themed(&app, e).border_color, Some(hover), "touch skips hover");
    app.world_mut().resource_mut::<UiPointer>().active_touch = None;
    settle(&mut app);
    assert_eq!(themed(&app, e).border_color, Some(hover));
}
