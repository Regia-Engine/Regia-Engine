// SPDX-License-Identifier: MPL-2.0
//! Render-out HUD (batch v020 `hud-render-out`): the game UI is laid out for the output size and
//! orientation (16:9 or native 9:16), roots tagged `UiVariant` follow the chosen layout, and the
//! HUD is present in the captured pixels.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_app::prelude::*;
use regia_render::{HudLayout, RenderDevice, RenderOutHud, RenderOutSpec};
use regia_ui::prelude::*;
use regia_ui::style::{Align, FlexDirection, JustifyContent};
use regia_ui::{OffscreenTarget, SharedUiRenderer, UiLayoutVariant, UiScales, UiVariant, default_safe_area};

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

/// A safe-area root whose single `size`-px white panel sits at the bottom-right of the safe area.
fn corner_hud(world: &mut World, variant: Option<UiLayoutVariant>, size: [f32; 2]) -> Entity {
    let mut root = world.spawn((
        UiRoot {
            scale_mode: ScaleMode::ConstantPixelSize,
            ..Default::default()
        },
        SafeAreaPadding,
        NodeStyle::new(Style {
            direction: FlexDirection::Column,
            justify_content: JustifyContent::End,
            align_items: Align::End,
            size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
            ..Default::default()
        }),
    ));
    if let Some(variant) = variant {
        root.insert(UiVariant(variant));
    }
    let root = root.id();
    let panel = world
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Points(size[0]), Dimension::Points(size[1])],
                ..Default::default()
            }),
            Background {
                kind: regia_ui::node::BackgroundKind::Color(UiColor::from_srgb8(255, 255, 255, 255)),
            },
        ))
        .id();
    world.entity_mut(root).add_child(panel);
    panel
}

fn rect(world: &World, e: Entity) -> [f32; 4] {
    world.get::<ComputedNode>(e).expect("laid out").rect
}

#[test]
fn portrait_anchors_sit_inside_the_9_16_safe_area() {
    let mut app = ui_app();
    let panel = corner_hud(app.world_mut(), None, [100.0, 40.0]);
    let hud = app.world().resource::<RenderOutHud>().clone();
    let spec = RenderOutSpec::new(1080, 1920, HudLayout::Portrait);
    hud.begin(app.world_mut(), &spec);
    let safe = default_safe_area(UiLayoutVariant::Portrait, [1080.0, 1920.0]);
    let r = rect(app.world(), panel);
    assert_eq!(r[2..], [100.0, 40.0]);
    assert_eq!(
        r[0],
        1080.0 - safe.right as f32 - 100.0,
        "right edge at the safe area"
    );
    assert_eq!(
        r[1],
        1920.0 - safe.bottom as f32 - 40.0,
        "bottom edge at the safe area"
    );
    // The same HUD at 16:9 anchors to the (smaller) landscape safe area.
    let spec = RenderOutSpec::new(1920, 1080, HudLayout::Landscape);
    hud.begin(app.world_mut(), &spec);
    let r = rect(app.world(), panel);
    assert_eq!([r[0] + r[2], r[1] + r[3]], [1920.0 - 96.0, 1080.0 - 54.0]);
}

#[test]
fn layout_variants_pick_their_roots_and_the_window_comes_back() {
    let mut app = ui_app();
    let land = corner_hud(app.world_mut(), Some(UiLayoutVariant::Landscape), [200.0, 60.0]);
    let port = corner_hud(app.world_mut(), Some(UiLayoutVariant::Portrait), [90.0, 120.0]);
    app.world_mut().resource_mut::<UiScales>().window_size = [1280.0, 720.0];
    let hud = app.world().resource::<RenderOutHud>().clone();
    let quads = |app: &App| app.world().resource::<SharedUiRenderer>().0.clone();
    let sizes = |app: &App| -> Vec<[f32; 2]> {
        let handle = quads(app);
        let guard = handle.lock().unwrap();
        guard.quads().iter().map(|q| q.size).collect()
    };

    let spec = RenderOutSpec::new(1080, 1920, HudLayout::Auto);
    hud.begin(app.world_mut(), &spec);
    assert_eq!(
        sizes(&app),
        [[90.0, 120.0]],
        "auto at 9:16 draws only the portrait root"
    );
    assert_eq!(app.world().resource::<UiScales>().window_size, [1080.0, 1920.0]);
    let r = rect(app.world(), port);
    assert_eq!(r[2..], [90.0, 120.0]);

    hud.end(app.world_mut(), &spec);
    assert_eq!(app.world().resource::<UiScales>().window_size, [1280.0, 720.0]);
    assert_eq!(
        sizes(&app),
        [[200.0, 60.0]],
        "the window's own layout is drawn again"
    );
    assert_eq!(rect(app.world(), land)[2..], [200.0, 60.0]);

    // An explicit layout wins over the output's orientation.
    let spec = RenderOutSpec::new(1920, 1080, HudLayout::Portrait);
    hud.begin(app.world_mut(), &spec);
    assert_eq!(sizes(&app), [[90.0, 120.0]]);
    hud.end(app.world_mut(), &spec);
}

#[test]
fn hud_is_present_in_the_captured_frame() {
    let device = match RenderDevice::shared_headless() {
        Ok(d) => d,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe): {err}"),
    };
    let mut app = ui_app();
    let panel = corner_hud(app.world_mut(), None, [100.0, 40.0]);
    let hud = app.world().resource::<RenderOutHud>().clone();
    let (w, h) = (270u32, 480u32);
    let spec = RenderOutSpec::new(w, h, HudLayout::Portrait);
    hud.begin(app.world_mut(), &spec);
    let r = rect(app.world(), panel);
    let target = OffscreenTarget::new(&device, w, h);
    target.clear(&device, [0.0, 0.0, 0.0, 1.0]);
    {
        let handle = app.world().resource::<SharedUiRenderer>().0.clone();
        let mut renderer = handle.lock().unwrap();
        target.draw_ui(&device, &mut renderer);
    }
    hud.end(app.world_mut(), &spec);
    let pixels = target.read(&device).expect("readback");
    let at = |x: f32, y: f32| {
        let i = ((y as u32 * w + x as u32) * 4) as usize;
        [pixels[i], pixels[i + 1], pixels[i + 2]]
    };
    assert_eq!(
        at(r[0] + r[2] / 2.0, r[1] + r[3] / 2.0),
        [255, 255, 255],
        "HUD panel in the frame"
    );
    assert_eq!(at(5.0, 5.0), [0, 0, 0], "outside the panel is the backdrop");
    let safe = default_safe_area(UiLayoutVariant::Portrait, [w as f32, h as f32]);
    assert_eq!(
        at(w as f32 - safe.right as f32 + 3.0, h as f32 - 3.0),
        [0, 0, 0],
        "safe area stays clear"
    );
}

#[test]
fn os_scale_factor_does_not_leak_into_the_render_out() {
    let mut app = ui_app();
    let panel = corner_hud(app.world_mut(), None, [100.0, 40.0]);
    // A physical-size root on a 2x HiDPI window.
    let root = app.world().get::<ChildOf>(panel).expect("parent").parent();
    app.world_mut().get_mut::<UiRoot>(root).expect("root").scale_mode = ScaleMode::ConstantPhysicalSize;
    {
        let mut scales = app.world_mut().resource_mut::<UiScales>();
        scales.window_size = [2560.0, 1440.0];
        scales.scale_factor = 2.0;
    }
    let hud = app.world().resource::<RenderOutHud>().clone();
    let spec = RenderOutSpec::new(1080, 1920, HudLayout::Portrait);
    hud.begin(app.world_mut(), &spec);
    assert_eq!(app.world().resource::<UiScales>().scale_factor, 1.0);
    assert_eq!(rect(app.world(), panel)[2..], [100.0, 40.0], "not doubled");
    hud.end(app.world_mut(), &spec);
    assert_eq!(app.world().resource::<UiScales>().scale_factor, 2.0);
}

#[test]
fn supersampled_constant_pixel_roots_keep_their_share_of_the_frame() {
    let mut app = ui_app();
    let panel = corner_hud(app.world_mut(), None, [100.0, 40.0]);
    let hud = app.world().resource::<RenderOutHud>().clone();
    // 1080x1920 delivered, drawn at 2x.
    let spec = RenderOutSpec::new(1080, 1920, HudLayout::Portrait).supersampled(2);
    hud.begin(app.world_mut(), &spec);
    assert_eq!(app.world().resource::<UiScales>().window_size, [2160.0, 3840.0]);
    assert_eq!(app.world().resource::<UiScales>().scale_factor, 2.0);
    assert_eq!(
        rect(app.world(), panel)[2..],
        [200.0, 80.0],
        "doubled with the density"
    );
    let safe = default_safe_area(UiLayoutVariant::Portrait, [2160.0, 3840.0]);
    let r = rect(app.world(), panel);
    assert_eq!(
        r[0] + r[2],
        2160.0 - safe.right as f32,
        "safe area scales with the frame"
    );
    hud.end(app.world_mut(), &spec);
    assert_eq!(
        app.world().resource::<UiScales>().scale_factor,
        1.0,
        "window factor restored"
    );
}

#[test]
fn the_supersampled_hud_is_drawn_at_density_and_repeats_byte_for_byte() {
    let device = match RenderDevice::shared_headless() {
        Ok(d) => d,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe): {err}"),
    };
    let mut app = ui_app();
    let panel = corner_hud(app.world_mut(), None, [100.0, 40.0]);
    let hud = app.world().resource::<RenderOutHud>().clone();
    let (w, h) = (270u32, 480u32);
    // Delivered 270x480, drawn at 2x: the 100x40 constant-pixel panel is 200x80 in the frame.
    let spec = RenderOutSpec::new(w, h, HudLayout::Portrait).supersampled(2);
    let mut frames = Vec::new();
    for _ in 0..2 {
        hud.begin(app.world_mut(), &spec);
        let target = OffscreenTarget::new(&device, spec.width, spec.height);
        target.clear(&device, [0.0, 0.0, 0.0, 1.0]);
        {
            let handle = app.world().resource::<SharedUiRenderer>().0.clone();
            let mut renderer = handle.lock().unwrap();
            target.draw_ui(&device, &mut renderer);
        }
        hud.end(app.world_mut(), &spec);
        frames.push(target.read(&device).expect("readback"));
    }
    assert_eq!(frames[0], frames[1], "the same HUD draws the same pixels");
    hud.begin(app.world_mut(), &spec);
    let r = rect(app.world(), panel);
    hud.end(app.world_mut(), &spec);
    assert_eq!(r[2..], [200.0, 80.0]);
    let at = |x: f32, y: f32| {
        let i = ((y as u32 * spec.width + x as u32) * 4) as usize;
        [frames[0][i], frames[0][i + 1], frames[0][i + 2]]
    };
    assert_eq!(at(r[0] + 100.0, r[1] + 40.0), [255, 255, 255], "panel centre");
    assert_eq!(at(r[0] - 4.0, r[1] + 40.0), [0, 0, 0], "outside the panel");
}
