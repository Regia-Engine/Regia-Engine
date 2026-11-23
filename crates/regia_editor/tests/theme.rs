// SPDX-License-Identifier: MPL-2.0
//! F11: the editor's overlays, audio gizmos, measurements and panels follow the world's theme
//! tokens, and the default tokens reproduce the look the editor had before them.

use egui::Color32;
use regia_core::math::{Vec2, Vec3};
use regia_ecs::ecs::entity::Entity;
use regia_ecs::prelude::*;
use regia_editor::overlays::{self, OverlayShape};
use regia_editor::selection::{SelectMode, Selection};
use regia_editor::theme::{self, ThemeTokens};
use regia_editor::viewport::{self, EditorCamera};
use regia_editor::views::{ShowFlags, ViewCamera, ViewKind};
use regia_scene::prelude::Transform;

fn light_world() -> (World, Entity) {
    let mut world = World::new();
    let light = world
        .spawn((
            Transform::from_translation(Vec3::new(0.0, 1.0, 0.0)),
            regia_render::PointLight::default(),
        ))
        .id();
    let mut selection = Selection::default();
    selection.select(light, SelectMode::Replace);
    world.insert_resource(selection);
    (world, light)
}

fn shapes(world: &World, grid: bool) -> Vec<OverlayShape> {
    let view = ViewCamera::of(ViewKind::Perspective, &EditorCamera::default());
    overlays::build_overlays(
        world,
        &view,
        ViewKind::Perspective,
        Vec2::new(320.0, 240.0),
        ShowFlags {
            grid,
            ..ShowFlags::default()
        },
    )
}

fn colors_of(shapes: &[OverlayShape]) -> Vec<Color32> {
    shapes
        .iter()
        .filter_map(|s| match s {
            OverlayShape::Line { color, .. } | OverlayShape::Circle { color, .. } => Some(*color),
            OverlayShape::Text { .. } => None,
        })
        .collect()
}

#[test]
fn default_tokens_paint_the_old_overlay_colors() {
    let (world, _) = light_world();
    let colors = colors_of(&shapes(&world, true));
    for expected in [
        overlays::ICON_LIGHT,
        overlays::BOUNDS,
        viewport::GRID_MAJOR,
        viewport::GRID_MINOR,
    ] {
        assert!(
            colors.contains(&expected),
            "{expected:?} missing from the default overlays"
        );
    }
}

#[test]
fn custom_tokens_recolor_the_overlays() {
    let (mut world, _) = light_world();
    let mut tokens = ThemeTokens::regia_default();
    tokens.overlay.icon_light = Color32::from_rgb(1, 2, 3);
    tokens.overlay.bounds = Color32::from_rgb(4, 5, 6);
    tokens.overlay.grid_major = Color32::from_rgb(7, 8, 9);
    tokens.overlay.grid_minor = Color32::from_rgb(10, 11, 12);
    world.insert_resource(tokens.clone());
    let colors = colors_of(&shapes(&world, true));
    for expected in [
        tokens.overlay.icon_light,
        tokens.overlay.bounds,
        tokens.overlay.grid_major,
        tokens.overlay.grid_minor,
    ] {
        assert!(colors.contains(&expected), "{expected:?} missing after retheming");
    }
    for old in [
        overlays::ICON_LIGHT,
        overlays::BOUNDS,
        viewport::GRID_MAJOR,
        viewport::GRID_MINOR,
    ] {
        assert!(!colors.contains(&old), "{old:?} still painted after retheming");
    }
}

#[test]
fn measurement_follows_the_themed_colors() {
    let mut tokens = ThemeTokens::regia_default();
    tokens.overlay.selection_outline = Color32::from_rgb(1, 2, 3);
    let mut image = egui::ColorImage::filled([32, 32], Color32::from_rgb(5, 5, 6));
    for x in 8..24 {
        image.pixels[16 * 32 + x] = tokens.overlay.selection_outline;
    }
    assert!(viewport::measure_viewport_with(&image, &tokens.overlay).selection_highlight);
    assert!(
        !viewport::measure_viewport(&image).selection_highlight,
        "the default measurement does not know the themed outline"
    );
}

#[test]
fn the_theme_reaches_an_egui_context_and_back() {
    let (mut world, _) = light_world();
    let ctx = egui::Context::default();
    theme::sync(&ctx, &world);
    assert_eq!(
        ctx.global_style().visuals,
        theme::visuals(&ThemeTokens::regia_default()),
        "no theme resource: the default look"
    );
    // ADR-8585: the default look is Piercing Graphite (panels #18181B).
    assert_eq!(
        ctx.global_style().visuals.panel_fill,
        Color32::from_rgb(0x18, 0x18, 0x1b)
    );
    let mut tokens = ThemeTokens::regia_default();
    tokens.signal = Color32::WHITE;
    tokens.on_signal = Color32::BLACK;
    tokens.accents.unsaved = Color32::from_rgb(9, 9, 9);
    world.insert_resource(tokens);
    theme::sync(&ctx, &world);
    let visuals = ctx.global_style().visuals.clone();
    assert_eq!(visuals.selection.bg_fill, Color32::WHITE);
    assert_eq!(visuals.selection.stroke.color, Color32::BLACK);
    assert_eq!(theme::ui_tokens(&ctx).accents.unsaved, Color32::from_rgb(9, 9, 9));
}

// ------------------------------------------------------------------------------------------------
// Themes, high contrast, text scale, diagnostic colors and color vision (spec addendum § 21,
// ADR-7838). The first five tests use only the agent surface and the preference functions, so they
// compile at the base and fail there with the refusal they now pass.

mod a11y_theme_text {
    use egui::Color32;
    use regia_agent::extension::AgentExtension;
    use regia_ecs::prelude::*;
    use regia_editor::editor_prefs::{self, EditorConfigPaths};
    use regia_editor::theme;
    use serde_json::{Value, json};

    /// Calls an editor agent method on `world` (installing the surface once); the error code on
    /// failure.
    fn call(world: &mut World, method: &str, params: Value) -> Result<Value, String> {
        if world.get_resource::<AgentExtension>().is_none() {
            regia_editor::agent_ext::install(world);
        }
        let extension = world.resource::<AgentExtension>().clone();
        let Some((_, handler)) = extension.method(method) else {
            return Err(format!("{method} is not registered"));
        };
        handler(world, &params).map_err(|e| {
            e.data
                .as_ref()
                .and_then(|d| d.get("code"))
                .and_then(Value::as_str)
                .map_or_else(|| e.message.clone(), str::to_owned)
        })
    }

    fn hex(text: &str) -> Color32 {
        let v = |i: usize| u8::from_str_radix(&text[i..i + 2], 16).expect("hex");
        Color32::from_rgb(v(1), v(3), v(5))
    }

    fn session(dir: &std::path::Path) -> World {
        let mut world = World::new();
        world.insert_resource(EditorConfigPaths::at(dir));
        editor_prefs::load_prefs(&mut world);
        world
    }

    /// P-358 (high contrast): the built-in theme is a Theme choice, and its text pairs meet WCAG
    /// AAA (7:1) as the a11y audit measures them, by name and as the active theme.
    #[test]
    fn the_high_contrast_theme_meets_aaa_in_the_audit() {
        let mut world = World::new();
        call(
            &mut world,
            "editor.prefs_set",
            json!({ "values": { "appearance.theme": "high-contrast" } }),
        )
        .expect("high-contrast is a Theme choice");
        editor_prefs::apply_prefs(None, &mut world);
        assert_eq!(theme::tokens(&world).panel, Color32::BLACK);
        for params in [
            json!({ "level": "aaa" }),
            json!({ "theme": "high-contrast", "level": "aaa" }),
        ] {
            let out = call(&mut world, "editor.a11y_contrast", params.clone()).expect("contrast");
            assert_eq!(out["failing"], json!([]), "{params}: {out}");
            for row in out["rows"].as_array().expect("rows") {
                let ratio = row["ratio"].as_f64().expect("ratio");
                let required = row["required"].as_f64().expect("required");
                assert!(ratio >= required, "{row}");
                if required > 3.0 {
                    assert!(ratio >= 7.0, "text pair below AAA: {row}");
                }
            }
        }
    }

    /// P-358 (saved custom themes): edited tokens saved as a theme file come back in the next
    /// session, and export then import moves a theme to another editor.
    #[test]
    fn a_saved_custom_theme_persists_across_sessions_and_travels_by_file() {
        let dir = regia_test_utils::unique_temp_dir("theme-custom-persist");
        let mut first = session(&dir);
        call(
            &mut first,
            "editor.set_theme",
            json!({ "tokens": { "signal": "#123456", "accents": { "ok": "#00aa00" } } }),
        )
        .expect("edit tokens");
        let saved = call(&mut first, "editor.theme_save", json!({ "name": "Studio Night" })).expect("save");
        let path = std::path::PathBuf::from(saved["path"].as_str().expect("path"));
        assert!(
            path.ends_with("themes/Studio Night.editor_theme.ron"),
            "{}",
            path.display()
        );
        assert!(path.is_file());
        assert_eq!(saved["themes"], json!(["Studio Night"]));
        assert!(
            editor_prefs::persist_prefs(&mut first),
            "the active theme is a preference"
        );

        let mut second = session(&dir);
        editor_prefs::apply_prefs(None, &mut second);
        assert_eq!(theme::tokens(&second).signal, hex("#123456"));
        assert_eq!(theme::tokens(&second).accents.ok, hex("#00aa00"));
        let got = call(&mut second, "editor.get_theme", json!({})).expect("get");
        assert_eq!(got["theme_source"], "ui:custom:Studio Night");
        // An unsaved edit, then the Theme page's Revert edits (`from_prefs`).
        call(
            &mut second,
            "editor.set_theme",
            json!({ "tokens": { "signal": "#ffffff" } }),
        )
        .expect("edit");
        call(&mut second, "editor.set_theme", json!({ "from_prefs": true })).expect("revert");
        assert_eq!(theme::tokens(&second).signal, hex("#123456"));

        // Export to a file under the temp directory, import it into a fresh editor.
        let exported = dir.join("share.editor_theme.ron");
        let out = call(
            &mut second,
            "editor.theme_export",
            json!({ "path": exported.display().to_string() }),
        )
        .expect("export");
        assert_eq!(out["name"], "share");
        let other_dir = regia_test_utils::unique_temp_dir("theme-custom-import");
        let mut third = session(&other_dir);
        let imported = call(
            &mut third,
            "editor.theme_import",
            json!({ "path": exported.display().to_string(), "name": "Shared" }),
        )
        .expect("import");
        assert_eq!(imported["name"], "Shared");
        assert_eq!(theme::tokens(&third).signal, hex("#123456"));
        let listed = call(&mut third, "editor.theme_list", json!({})).expect("list");
        assert_eq!(listed["custom_theme"], "Shared");
        assert!(
            listed["builtin"]
                .as_array()
                .is_some_and(|b| b.contains(&json!("high-contrast")))
        );
        // Refusals: a bad name, a wrong file suffix, an unknown saved theme.
        assert_eq!(
            call(&mut third, "editor.theme_save", json!({ "name": "../x" })).expect_err("refused"),
            "bad_theme_name"
        );
        assert_eq!(
            call(
                &mut third,
                "editor.theme_export",
                json!({ "path": dir.join("x.ron").display().to_string() })
            )
            .expect_err("refused"),
            "bad_path"
        );
        assert_eq!(
            call(
                &mut third,
                "editor.theme_export",
                json!({ "path": dir.join("y.editor_theme.ron").display().to_string(), "name": "Nope" })
            )
            .expect_err("refused"),
            "theme_not_found"
        );
        assert_eq!(
            call(
                &mut third,
                "editor.theme_import",
                json!({ "text": "(format_version: 9)" })
            )
            .expect_err("refused"),
            "theme_newer"
        );
        // A world without a config directory cannot save.
        assert_eq!(
            call(&mut World::new(), "editor.theme_save", json!({ "name": "A" })).expect_err("refused"),
            "theme_io"
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&other_dir);
    }

    /// P-363: error, warning, info and success colors are preferences that default to the theme
    /// and reach every token the log, the Problems panel, status lines and badges read.
    #[test]
    fn diagnostic_color_preferences_recolor_log_problems_and_badges() {
        let mut world = World::new();
        call(
            &mut world,
            "editor.prefs_set",
            json!({ "values": {
                "appearance.color_error": "#ff00ff",
                "appearance.color_warning": "#00ffff",
                "appearance.color_info": "#123456",
                "appearance.color_success": "#654321",
            } }),
        )
        .expect("diagnostic colors are preferences");
        editor_prefs::apply_prefs(None, &mut world);
        let t = theme::tokens(&world).clone();
        assert_eq!(t.accents.log_error, hex("#ff00ff"), "log ERROR lines");
        assert_eq!(t.accents.error_text, hex("#ff00ff"), "status-line errors");
        assert_eq!(t.error, hex("#ff00ff"));
        assert_eq!(t.accents.log_warn, hex("#00ffff"), "log WARN lines");
        assert_eq!(t.warn, hex("#00ffff"), "warning text and badges");
        assert_eq!(t.accents.log_info, hex("#123456"));
        assert_eq!(t.accents.ok, hex("#654321"));
        // Badges paint with egui's warn color, which the theme sets.
        let ctx = egui::Context::default();
        theme::sync(&ctx, &world);
        assert_eq!(ctx.global_style().visuals.warn_fg_color, hex("#00ffff"));
        // Empty returns to the theme's color; a non-color is refused.
        call(
            &mut world,
            "editor.prefs_set",
            json!({ "values": { "appearance.color_error": "" } }),
        )
        .expect("reset by empty");
        editor_prefs::apply_prefs(None, &mut world);
        assert_eq!(
            theme::tokens(&world).accents.log_error,
            regia_editor::theme::ThemeTokens::regia_default()
                .accents
                .log_error,
            "the default theme's"
        );
        assert_eq!(
            call(
                &mut world,
                "editor.prefs_set",
                json!({ "values": { "appearance.color_warning": "orange" } })
            )
            .expect_err("refused"),
            "bad_pref_value"
        );
    }

    /// P-362: `appearance.text_scale` grows egui's text styles and leaves the whole-UI zoom alone.
    #[test]
    fn text_scale_scales_text_separately_from_the_ui_scale() {
        let ctx = egui::Context::default();
        let body = |ctx: &egui::Context| ctx.global_style().text_styles[&egui::TextStyle::Body].size;
        let base = body(&ctx);
        let mut world = World::new();
        call(
            &mut world,
            "editor.prefs_set",
            json!({ "values": { "appearance.text_scale": 2.0 } }),
        )
        .expect("text_scale is a preference");
        editor_prefs::apply_prefs(Some(&ctx), &mut world);
        assert!((body(&ctx) - base * 2.0).abs() < 1e-3, "{} vs {base}", body(&ctx));
        assert!((ctx.zoom_factor() - 1.0).abs() < 1e-6, "the UI zoom is separate");
        // Changing it again scales from the original size, not the scaled one.
        call(
            &mut world,
            "editor.prefs_set",
            json!({ "values": { "appearance.text_scale": 1.25 } }),
        )
        .expect("set");
        editor_prefs::apply_prefs(Some(&ctx), &mut world);
        assert!((body(&ctx) - base * 1.25).abs() < 1e-3);
        for bad in [0.5, 2.5] {
            assert_eq!(
                call(
                    &mut world,
                    "editor.prefs_set",
                    json!({ "values": { "appearance.text_scale": bad } })
                )
                .expect_err("refused"),
                "bad_pref_value",
                "{bad} is outside 0.8 to 2.0"
            );
        }
    }

    /// P-359: View ▸ Color Vision is in the menu list with its agent route, and the agent method
    /// sets the chrome's and the viewport's filter together.
    #[test]
    fn color_vision_reaches_the_menu_the_chrome_and_the_viewport() {
        let mut world = World::new();
        let out = call(
            &mut world,
            "editor.color_vision",
            json!({ "mode": "simulate", "deficiency": "protanopia" }),
        )
        .expect("editor.color_vision");
        assert_eq!(out["chrome"], true);
        assert_eq!(out["viewport"], true);
        let menus = call(&mut world, "editor.menu_list", json!({})).expect("menu list");
        let text = menus.to_string();
        for label in [
            "Color Vision/Off",
            "Color Vision/Simulate protanopia",
            "Color Vision/Simulate deuteranopia",
            "Color Vision/Simulate tritanopia",
        ] {
            assert!(text.contains(label), "{label} missing from {text}");
        }
        assert!(text.contains("editor.color_vision"));
        call(&mut world, "editor.color_vision", json!({ "mode": "off" })).expect("off");
    }

    /// The text runs the editor's default layout cuts off at `text_scale` (1920×1080 points, the
    /// main panels: menu bar, toolbar, outliner, viewport, inspector, content browser and log).
    fn clipped_main_panels(text_scale: f64) -> Vec<String> {
        use regia_app::prelude::*;
        let dir = regia_test_utils::unique_temp_dir("theme-text-scale-panels");
        std::fs::create_dir_all(dir.join("assets/levels")).expect("assets");
        std::fs::write(dir.join("Regia.toml"), "[project]\nname = \"T\"\n").expect("manifest");
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(regia_scene::ScenePlugin);
        app.init_resource::<regia_editor::command::UndoStack>()
            .init_resource::<regia_editor::selection::Selection>()
            .init_resource::<regia_editor::viewport::EditorCamera>()
            .init_resource::<regia_editor::viewport::GizmoState>()
            .init_resource::<regia_editor::viewport::ViewportSettings>();
        let open = regia_editor::level_io::new_level(app.world_mut());
        app.world_mut().insert_resource(open);
        regia_editor::agent_ext::install(app.world_mut());
        app.update();
        call(
            app.world_mut(),
            "editor.prefs_set",
            json!({ "values": { "appearance.text_scale": text_scale } }),
        )
        .expect("text scale");
        let mut shell = regia_editor::shell::EditorShell::new(None, Some(dir.clone()));
        let ctx = egui::Context::default();
        let mut last = Vec::new();
        for frame in 0..4 {
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1920.0, 1080.0),
                )),
                time: Some(f64::from(frame) / 60.0),
                predicted_dt: 1.0 / 60.0,
                ..Default::default()
            };
            let mut output = ctx.run_ui(raw, |ui| shell.ui(ui, app.world_mut(), None));
            output.textures_delta.clear();
            last = output.shapes;
        }
        let body = ctx.global_style().text_styles[&egui::TextStyle::Body].size;
        let stock = egui::style::default_text_styles()[&egui::TextStyle::Body].size;
        assert!(
            (f64::from(body / stock) - text_scale).abs() < 0.05,
            "the shell applied text_scale {text_scale}: body {body}, stock {stock}"
        );
        let _ = std::fs::remove_dir_all(&dir);
        // A dock tab strip scrolls sideways when a group has more tabs than fit: a title cut only
        // at the strip's left or right edge is scrolled to, not lost. Cut vertically it is lost.
        let strip = regia_editor::theme::dock_style(&ctx.global_style())
            .tab_bar
            .height;
        let scrolled_tab = |c: &regia_editor::a11y_audit::ClippedText| {
            (c.clip.height() - strip).abs() < 1.0
                && c.bounds.top() >= c.clip.top() - 0.5
                && c.bounds.bottom() <= c.clip.bottom() + 0.5
        };
        let mut texts: Vec<String> = regia_editor::a11y_audit::clipped_text(&last, 0.5)
            .into_iter()
            .filter(|c| !scrolled_tab(c))
            .map(|c| format!("{:?} in {:?}: {}", c.bounds, c.clip, c.text))
            .collect();
        texts.sort();
        texts
    }

    /// P-362: at text scale 2.0 the main panels still fit: the a11y audit's clipped-text measure
    /// finds no text cut off by its clip edge (reported against the 1.0 layout for context).
    #[test]
    fn main_panels_at_text_scale_two_clip_no_text() {
        let at_one = clipped_main_panels(1.0);
        let at_two = clipped_main_panels(2.0);
        assert!(
            at_two.is_empty(),
            "clipped at 2.0 ({}):\n{}\n-- clipped at 1.0 ({}):\n{}",
            at_two.len(),
            at_two.join("\n"),
            at_one.len(),
            at_one.join("\n")
        );
    }
}

/// The `light` family a host installs (the v0.6.0 host report, ADR-8888).
fn light() -> egui::FontFamily {
    egui::FontFamily::Name("light".into())
}

/// A host's font table: egui's defaults plus its own face (`host-light`, egui's Ubuntu Light bytes
/// under a key of the host's) in its own `light` family.
fn host_fonts() -> egui::FontDefinitions {
    let mut defs = egui::FontDefinitions::default();
    let face = defs.font_data.get("Ubuntu-Light").cloned();
    if let Some(face) = face {
        defs.font_data.insert("host-light".into(), face);
    }
    defs.families.insert(light(), vec!["host-light".into()]);
    defs
}

/// Runs one pass that themes the context with `tokens` (as the editor shell's `sync` does every
/// pass) and lays out a label in the host's `light` family. egui panics on a family its live font
/// table lacks, so the label is the "renders without panic" check.
fn host_pass(ctx: &egui::Context, tokens: &ThemeTokens) {
    let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
        theme::apply(ui.ctx(), tokens);
        ui.label(egui::RichText::new("host light family").family(light()));
    });
    // No renderer here: drop the font atlas uploads.
    out.textures_delta.clear();
}

/// The live table has the host's family and face and the theme's own families.
fn assert_host_and_theme_fonts(ctx: &egui::Context, when: &str) {
    let families = ctx.fonts(|f| f.families());
    assert!(
        families.contains(&light()),
        "{when}: the host's family is gone: {families:?}"
    );
    for name in [theme::DISPLAY_FAMILY, theme::PROSE_FAMILY] {
        assert!(
            families.contains(&egui::FontFamily::Name(name.into())),
            "{when}: the theme's {name} family is missing"
        );
    }
    let keys: Vec<String> = ctx.fonts(|f| f.definitions().font_data.keys().cloned().collect());
    assert!(keys.iter().any(|k| k == "host-light"), "{when}: {keys:?}");
    assert!(
        keys.iter().any(|k| k.starts_with("regia_ui:")),
        "{when}: the fallbacks: {keys:?}"
    );
}

/// ADR-8888 (amends ADR-8589), the v0.6.0 host report: a host that installs its fonts before
/// the theme (`theme::set_host_fonts` at start-up, before any pass) keeps its `light` family
/// through the theme's first apply, a theme change, and a later `set_host_fonts`; a label in the
/// family renders in every pass. Before the fix the first apply replaced the table with the
/// engine's own and the label panicked in the second pass.
#[test]
fn host_fonts_registered_before_the_theme_survive_it() {
    let ctx = egui::Context::default();
    theme::set_host_fonts(&ctx, host_fonts());
    let graphite = ThemeTokens::regia_default();
    assert_eq!(
        graphite.fonts,
        theme::FontSet::brand(),
        "the default theme sets fonts"
    );
    for _ in 0..3 {
        host_pass(&ctx, &graphite);
    }
    assert_host_and_theme_fonts(&ctx, "after the first apply");
    let keys: Vec<String> = ctx.fonts(|f| f.definitions().font_data.keys().cloned().collect());
    assert!(keys.iter().any(|k| k == "theme:JetBrains Mono"), "{keys:?}");

    // A theme without fonts takes the brand faces out and keeps the host's.
    let classic = ThemeTokens::regia_classic();
    for _ in 0..3 {
        host_pass(&ctx, &classic);
    }
    assert_host_and_theme_fonts(&ctx, "after a theme change");
    let keys: Vec<String> = ctx.fonts(|f| f.definitions().font_data.keys().cloned().collect());
    assert!(!keys.iter().any(|k| k.starts_with("theme:")), "{keys:?}");

    // The host replaces its table after the theme: the theme reinstalls over the new one.
    let mut second = host_fonts();
    second
        .families
        .insert(egui::FontFamily::Name("light2".into()), vec!["host-light".into()]);
    theme::set_host_fonts(&ctx, second);
    for _ in 0..2 {
        host_pass(&ctx, &classic);
    }
    assert_host_and_theme_fonts(&ctx, "after a second set_host_fonts");
    assert!(
        ctx.fonts(|f| f.families())
            .contains(&egui::FontFamily::Name("light2".into()))
    );
}

/// ADR-8888: a host that calls `egui::Context::set_fonts` itself, in a pass before the theme's
/// first apply, keeps its family too: once a pass has run, the theme merges over the table the
/// context draws with. Before the fix the label panicked in the pass after the apply.
#[test]
fn fonts_a_host_set_in_an_earlier_pass_survive_the_theme() {
    let ctx = egui::Context::default();
    let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
        ui.ctx().set_fonts(host_fonts());
    });
    out.textures_delta.clear();
    for _ in 0..3 {
        host_pass(&ctx, &ThemeTokens::regia_default());
    }
    assert_host_and_theme_fonts(&ctx, "after the first apply");
}
