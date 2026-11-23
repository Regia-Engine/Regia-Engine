// SPDX-License-Identifier: MPL-2.0
//! Runtime default-font selection, fallback, and layout cache invalidation.

use regia_app::prelude::*;
use regia_ui::text::{DefaultFontError, ShapedText, Text, TextStyle, TextSystem};
use regia_ui::{Localization, UiNode, UiPlugin, UiRoot};

#[test]
fn default_font_selection_preserves_explicit_families_and_fallback() {
    let mut fonts = TextSystem::embedded_only();
    let loc = Localization::default();
    let text = Text::new("Hello").with_style(TextStyle {
        family: Some("Inter".into()),
        ..Default::default()
    });
    let before = fonts.shape(&text, 1.0, None, &loc);
    assert!(
        fonts
            .set_default_family(Some("Noto Sans SC"))
            .expect("loaded family")
    );
    assert_eq!(fonts.default_family(), "Noto Sans SC");
    assert_eq!(
        fonts
            .font_system
            .db()
            .family_name(&cosmic_text::Family::SansSerif),
        "Noto Sans SC"
    );
    assert!(
        !fonts
            .set_default_family(Some("noto sans sc"))
            .expect("same family")
    );
    let after = fonts.shape(&text, 1.0, None, &loc);
    assert_eq!(before.glyphs, after.glyphs, "explicit family is untouched");
    for text in ["Hello", "你好", "مرحبا", "こんにちは", "😀"] {
        let shaped = fonts.shape(&Text::new(text), 1.0, None, &loc);
        assert_eq!(shaped.missing_glyphs, 0, "fallback: {text}");
    }
    assert_eq!(
        fonts.set_default_family(Some("missing family")),
        Err(DefaultFontError::Unavailable("missing family".into()))
    );
    assert_eq!(fonts.default_family(), "Noto Sans SC");
    assert!(fonts.set_default_family(None).expect("reset"));
    assert_eq!(fonts.default_family(), "Inter");
}

#[test]
fn default_font_switch_invalidates_live_text_and_layout_once() {
    let mut app = App::new();
    app.insert_resource(TextSystem::embedded_only());
    app.add_plugins(UiPlugin::code_only());
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
    let root = app.world_mut().spawn((UiRoot::default(), UiNode)).id();
    let text = app
        .world_mut()
        .spawn((UiNode, Text::new("Hello"), ChildOf(root)))
        .id();
    app.update();
    let digest = app.world().get::<ShapedText>(text).expect("shaped").digest;
    app.world_mut()
        .resource_mut::<TextSystem>()
        .set_default_family(Some("Noto Sans SC"))
        .expect("switch");
    app.update();
    let changed = app.world().get::<ShapedText>(text).expect("reshaped").digest;
    assert_ne!(digest, changed);
    let generation = app.world().resource::<Localization>().generation.0;
    app.update();
    assert_eq!(generation, app.world().resource::<Localization>().generation.0);
    assert_eq!(
        changed,
        app.world().get::<ShapedText>(text).expect("cached").digest
    );
    app.world_mut()
        .resource_mut::<TextSystem>()
        .set_default_family(None)
        .expect("reset");
    app.update();
    assert_ne!(
        changed,
        app.world().get::<ShapedText>(text).expect("reset shape").digest
    );
}

/// Render-out runs `sync_font_locale` through `run_system_once` (fresh system state each call):
/// a font switch must bump the shared generation exactly once, not on every one-shot run.
#[test]
fn one_shot_font_sync_bumps_the_generation_once_per_switch() {
    use regia_ecs::ecs::system::RunSystemOnce;
    let mut world = World::new();
    world.insert_resource(TextSystem::embedded_only());
    world.insert_resource(Localization::default());
    world
        .run_system_once(regia_ui::text::sync_font_locale)
        .expect("settle start-up loads");
    let settled = world.resource::<Localization>().generation.0;
    world
        .run_system_once(regia_ui::text::sync_font_locale)
        .expect("idle sync");
    assert_eq!(settled, world.resource::<Localization>().generation.0);
    world
        .resource_mut::<TextSystem>()
        .set_default_family(Some("Noto Sans SC"))
        .expect("switch");
    for _ in 0..3 {
        world
            .run_system_once(regia_ui::text::sync_font_locale)
            .expect("one-shot sync");
    }
    assert_eq!(
        settled.wrapping_add(1),
        world.resource::<Localization>().generation.0,
        "one bump per font change, however many one-shot runs follow"
    );
}
