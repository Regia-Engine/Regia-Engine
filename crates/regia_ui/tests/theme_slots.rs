// SPDX-License-Identifier: MPL-2.0
//! Theme slots of the UI parity roadmap § 2a (ADR-7208): full padding and min/max size widget
//! properties that reach layout (lane 7), scrollbar color slots that reach the draw list (lane 8),
//! and the `subtitle` caption tokens with the high-contrast caption theme (spec 36 § 8.3).
//! Headless: no window, no GPU, no files, fixed 50 ms steps, no goldens.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use regia_app::TimeUpdateStrategy;
use regia_app::caption::{
    CaptionId, CaptionKind, CaptionPiece, CaptionSource, CaptionStart, CaptionStyleHint, CaptionTime,
};
use regia_app::prelude::*;
use regia_ui::captions::{CaptionPlugin, CaptionRow};
use regia_ui::prelude::*;
use regia_ui::render::SharedUiRenderer;
use regia_ui::scroll::{ScrollOffset, ScrollView};
use regia_ui::style::Align;
use regia_ui::theme::VisualOverrides;
use regia_ui::{
    ActiveTheme, BackgroundKind, ClipChildren, ScrollAxis, ScrollbarNodes, Scrollbars, ThemeOverride,
    ThemeResolver, UiThemes,
};

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
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(50)));
    app
}

/// A game theme over `regia-dark` that sets every new slot.
const SLOTS_THEME: &str = r##"(format_version: 1, name: "slots", extends: "regia-dark",
    tokens: {
        "color.scrollbar-track": "#00ff00",
        "color.scrollbar-thumb": "#ff0000",
        "color.scrollbar-thumb-hover": "#ff8000",
        "color.scrollbar-thumb-active": "#0000ff",
    },
    types: {"Card": (props: {"padding": [10, 20, 30, 40], "min_width": 300, "min_height": 120})},
    variations: {
        "Card.capped": (extends: "Card", props: {"padding": 4, "min_width": 0, "min_height": 0,
            "max_width": 50, "max_height": 40}),
        "Card.pair": (extends: "Card", props: {"padding": [5, 7]}),
    })"##;

fn load_slots(app: &mut App) {
    app.world_mut()
        .resource_mut::<UiThemes>()
        .set
        .load_ron(SLOTS_THEME, "slots.theme.ron")
        .expect("slots theme loads");
    app.world_mut().insert_resource(ActiveTheme::named("slots"));
}

fn settle(app: &mut App) {
    for _ in 0..8 {
        app.update();
    }
}

fn root(app: &mut App) -> Entity {
    app.world_mut()
        .spawn((
            UiRoot {
                scale_mode: ScaleMode::ConstantPixelSize,
                ..Default::default()
            },
            NodeStyle::new(Style {
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                align_items: Align::Start,
                ..Default::default()
            }),
        ))
        .id()
}

fn fixed(app: &mut App, parent: Entity, w: f32, h: f32) -> Entity {
    let e = app
        .world_mut()
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Points(w), Dimension::Points(h)],
                ..Default::default()
            }),
        ))
        .id();
    app.world_mut().entity_mut(parent).add_child(e);
    e
}

fn classed(app: &mut App, parent: Entity, class: &str) -> Entity {
    let e = app
        .world_mut()
        .spawn((UiNode, Classes(vec![class.to_owned()]), NodeStyle::default()))
        .id();
    app.world_mut().entity_mut(parent).add_child(e);
    e
}

fn rect(app: &App, e: Entity) -> [f32; 4] {
    let c = app.world().get::<ComputedNode>(e).unwrap();
    let s = c.scale.max(0.001);
    [c.rect[0] / s, c.rect[1] / s, c.rect[2] / s, c.rect[3] / s]
}

fn themed(app: &App, e: Entity) -> Themed {
    *app.world().get::<Themed>(e).unwrap()
}

fn slot(app: &App, key: &str) -> UiColor {
    app.world()
        .resource::<ThemeResolver>()
        .resolved_theme()
        .and_then(|t| t.color(key))
        .unwrap_or_else(|| panic!("no {key}"))
}

fn quad_colors(app: &App) -> Vec<UiColor> {
    app.world()
        .resource::<SharedUiRenderer>()
        .lock()
        .quads()
        .iter()
        .map(|q| q.color)
        .collect()
}

// --- lane 7: full padding and min/max size -------------------------------------------------------

#[test]
fn theme_padding_and_min_max_sizes_resolve_and_reach_layout() {
    let mut app = ui_app();
    load_slots(&mut app);
    let r = root(&mut app);
    let card = classed(&mut app, r, "Card");
    let inner = fixed(&mut app, card, 10.0, 10.0);
    let capped = classed(&mut app, r, "Card.capped");
    let big = fixed(&mut app, capped, 200.0, 200.0);
    let pair = classed(&mut app, r, "Card.pair");
    settle(&mut app);

    // Resolution: CSS order in the file (top, right, bottom, left), stored left, top, right, bottom.
    let t = themed(&app, card);
    assert_eq!(t.padding, Some([40.0, 10.0, 20.0, 30.0]));
    assert_eq!((t.min_width, t.min_height), (Some(300.0), Some(120.0)));
    assert_eq!((t.max_width, t.max_height), (None, None));
    let c = themed(&app, capped);
    assert_eq!(c.padding, Some([4.0; 4]), "one number pads all four edges");
    assert_eq!((c.max_width, c.max_height), (Some(50.0), Some(40.0)));
    assert_eq!(
        themed(&app, pair).padding,
        Some([7.0, 5.0, 7.0, 5.0]),
        "two numbers are vertical, horizontal"
    );

    // Layout: the minimum grows an empty card, the padding offsets its child.
    let (cr, ir) = (rect(&app, card), rect(&app, inner));
    assert_eq!((cr[2], cr[3]), (300.0, 120.0), "min size {cr:?}");
    assert_eq!((ir[0] - cr[0], ir[1] - cr[1]), (40.0, 10.0), "left/top padding");
    // The maximum caps a card whose child is larger; its padding is the variation's 4.
    let (kr, br) = (rect(&app, capped), rect(&app, big));
    assert_eq!((kr[2], kr[3]), (50.0, 40.0), "max size {kr:?}");
    assert_eq!((br[0] - kr[0], br[1] - kr[1]), (4.0, 4.0));

    // A per-node override wins over the variation chain, and the layout follows it.
    app.world_mut()
        .entity_mut(card)
        .insert(ThemeOverride::new(VisualOverrides {
            padding: Some([2.0, 3.0, 2.0, 3.0]),
            max_width: Some(310.0),
            ..Default::default()
        }));
    settle(&mut app);
    let (cr, ir) = (rect(&app, card), rect(&app, inner));
    assert_eq!(themed(&app, card).padding, Some([2.0, 3.0, 2.0, 3.0]));
    assert_eq!((ir[0] - cr[0], ir[1] - cr[1]), (2.0, 3.0), "override padding");
    assert_eq!(cr[2], 300.0, "min 300 under max 310");
}

#[test]
fn nodes_without_theme_geometry_keep_their_own_style() {
    let mut app = ui_app();
    load_slots(&mut app);
    let r = root(&mut app);
    // A themed button (the built-in theme sets no padding or min/max) keeps its style's box.
    let e = app
        .world_mut()
        .spawn((
            UiNode,
            Classes(vec!["button".to_owned()]),
            NodeStyle::new(Style {
                size: [Dimension::Points(90.0), Dimension::Points(30.0)],
                padding: Edges::px(6.0),
                ..Default::default()
            }),
        ))
        .id();
    app.world_mut().entity_mut(r).add_child(e);
    let child = fixed(&mut app, e, 5.0, 5.0);
    settle(&mut app);
    let t = themed(&app, e);
    assert_eq!(t.padding, None);
    assert_eq!((t.min_height, t.max_width, t.max_height), (None, None, None));
    let (er, cr) = (rect(&app, e), rect(&app, child));
    assert_eq!((er[2], er[3]), (90.0, 30.0));
    assert_eq!((cr[0] - er[0], cr[1] - er[1]), (6.0, 6.0));
}

// --- lane 8: scrollbar slots ---------------------------------------------------------------------

/// A 200 x 100 scroll view over 500 px of content inside a non-scrolling wrapper.
fn scroll_view(app: &mut App, bars: Option<Scrollbars>) -> Entity {
    let r = root(app);
    let wrapper = fixed(app, r, 200.0, 100.0);
    let view = app
        .world_mut()
        .spawn((
            UiNode,
            ScrollView::default(),
            ScrollOffset::default(),
            ClipChildren,
            NodeStyle::new(Style {
                size: [Dimension::Points(200.0), Dimension::Points(100.0)],
                ..Default::default()
            }),
        ))
        .id();
    if let Some(bars) = bars {
        app.world_mut().entity_mut(view).insert(bars);
    }
    app.world_mut().entity_mut(wrapper).add_child(view);
    fixed(app, view, 200.0, 500.0);
    view
}

#[test]
fn the_built_in_thumb_draws_in_the_theme_scrollbar_thumb_slot() {
    // Control: without the slot, the thin thumb is white at 0.35.
    let mut plain = ui_app();
    scroll_view(&mut plain, None);
    settle(&mut plain);
    let white = UiColor([1.0, 1.0, 1.0, 0.35]);
    assert!(quad_colors(&plain).contains(&white), "control thumb drawn");

    let mut app = ui_app();
    load_slots(&mut app);
    scroll_view(&mut app, None);
    settle(&mut app);
    let thumb = slot(&app, "color.scrollbar-thumb");
    let colors = quad_colors(&app);
    assert!(colors.contains(&thumb), "slot color in the draw list");
    assert!(!colors.contains(&white), "the default thumb color is gone");
}

#[test]
fn scrollbar_presets_take_the_track_and_thumb_slots() {
    let mut app = ui_app();
    load_slots(&mut app);
    let view = scroll_view(&mut app, Some(Scrollbars::classic()));
    settle(&mut app);
    let (track, thumb) = app
        .world()
        .get::<ScrollbarNodes>(view)
        .and_then(|n| n.bar(ScrollAxis::Vertical))
        .expect("vertical bar");
    let fill = |e: Entity| match app.world().get::<Background>(e).map(|b| &b.kind) {
        Some(BackgroundKind::Color(c)) => Some(*c),
        _ => None,
    };
    let (track_slot, thumb_slot) = (
        slot(&app, "color.scrollbar-track"),
        slot(&app, "color.scrollbar-thumb"),
    );
    assert_eq!(fill(track), Some(track_slot));
    assert_eq!(fill(thumb), Some(thumb_slot));
    let colors = quad_colors(&app);
    assert!(colors.contains(&track_slot) && colors.contains(&thumb_slot));

    // A custom (non-preset) config is painted as stored: the slots never override it.
    let mut custom = ui_app();
    load_slots(&mut custom);
    let mine = UiColor([0.2, 0.4, 0.6, 1.0]);
    let view = scroll_view(
        &mut custom,
        Some(Scrollbars {
            thumb_color: Some(mine),
            ..Scrollbars::default()
        }),
    );
    settle(&mut custom);
    let (_, thumb) = custom
        .world()
        .get::<ScrollbarNodes>(view)
        .and_then(|n| n.bar(ScrollAxis::Vertical))
        .expect("vertical bar");
    assert_eq!(
        custom.world().get::<Background>(thumb).map(|b| b.kind.clone()),
        Some(BackgroundKind::Color(mine))
    );
}

// --- spec 36 § 8.3: subtitle tokens and the high-contrast caption theme ---------------------------

fn cue(text: &str) -> CaptionStart {
    CaptionStart {
        id: CaptionId(1),
        source: CaptionSource::Game("test".into()),
        kind: CaptionKind::Speech,
        locale: "en".into(),
        pieces: vec![CaptionPiece::literal(0, text)],
        speaker: None,
        priority: 0,
        start: CaptionTime::AtMicros(0),
        planned_us: 60_000_000,
        keep_breaks: false,
        hint: CaptionStyleHint::default(),
    }
}

/// The caption row's plate fill and its text color.
fn caption_paint(app: &mut App) -> (UiColor, UiColor) {
    let world = app.world_mut();
    let (row, children) = world
        .query::<(Entity, &CaptionRow, &Children)>()
        .iter(world)
        .map(|(e, _, c)| (e, c.iter().next().expect("row text")))
        .next()
        .expect("a caption row");
    let plate = match world.get::<Background>(row).map(|b| &b.kind) {
        Some(BackgroundKind::Color(c)) => *c,
        other => panic!("plate {other:?}"),
    };
    let text = world.get::<Text>(children).expect("text").spans[0].style.color;
    (plate, text)
}

#[test]
fn the_caption_theme_swaps_under_high_contrast() {
    let mut app = ui_app();
    app.add_plugins(CaptionPlugin);
    app.world_mut().write_message(cue("Hold the line"));
    settle(&mut app);
    let dark = app
        .world()
        .resource::<ThemeResolver>()
        .resolved_theme()
        .cloned()
        .expect("dark resolved");
    assert_eq!(dark.name, "regia-dark");
    let (plate, text) = caption_paint(&mut app);
    // The dark theme's subtitle style: its plate color at the player's 0.6 box opacity.
    let dark_plate = dark.color("subtitle.plate").expect("subtitle.plate token");
    assert_eq!(text, dark.color("subtitle.text").expect("subtitle.text token"));
    assert_eq!(plate.0[..3], dark_plate.0[..3]);
    assert!((plate.0[3] - 0.6).abs() < 0.01, "player opacity {plate:?}");

    // High contrast swaps to the high-contrast caption theme: an opaque plate.
    app.world_mut().insert_resource(ActiveTheme {
        high_contrast: true,
        ..ActiveTheme::named("regia-dark")
    });
    settle(&mut app);
    let hc = app
        .world()
        .resource::<ThemeResolver>()
        .resolved_theme()
        .cloned()
        .expect("hc resolved");
    assert_eq!(hc.name, "regia-high-contrast");
    assert_eq!(hc.number("subtitle.plate-opacity"), Some(1.0));
    let (plate, text) = caption_paint(&mut app);
    assert_eq!(text, hc.color("subtitle.text").expect("hc text"));
    assert_eq!(plate.0[3], 1.0, "opaque plate under high contrast: {plate:?}");

    // A player text color other than the default still wins over the theme.
    app.world_mut()
        .resource_mut::<regia_ui::captions::CaptionOptions>()
        .text_color = "yellow".into();
    settle(&mut app);
    let (_, text) = caption_paint(&mut app);
    assert_ne!(text, hc.color("subtitle.text").expect("hc text"));
}

// --- post05w3 (ADR-9340): theme height, padding_x and gap reach layout ---------------------------

/// A theme over `regia-dark` whose `Bar` type sets the control height, inline padding and gap.
const BOX_THEME: &str = r##"(format_version: 1, name: "boxes", extends: "regia-dark",
    types: {"Bar": (props: {"height": 50, "padding_x": 12, "gap": 9})},
    variations: {
        "Bar.padded": (extends: "Bar", props: {"padding": [3, 4, 5, 6]}),
    })"##;

fn load_boxes(app: &mut App) {
    app.world_mut()
        .resource_mut::<UiThemes>()
        .set
        .load_ron(BOX_THEME, "boxes.theme.ron")
        .expect("boxes theme loads");
    app.world_mut().insert_resource(ActiveTheme::named("boxes"));
}

#[test]
fn theme_height_padding_x_and_gap_change_the_layout_rects() {
    let mut app = ui_app();
    load_boxes(&mut app);
    let r = root(&mut app);
    let bar = classed(&mut app, r, "Bar");
    let a = fixed(&mut app, bar, 10.0, 10.0);
    let b = fixed(&mut app, bar, 10.0, 10.0);
    // Control: the same node shape without a theme class.
    let plain = classed(&mut app, r, "NotThemed");
    let pa = fixed(&mut app, plain, 10.0, 10.0);
    let pb = fixed(&mut app, plain, 10.0, 10.0);
    settle(&mut app);

    let t = themed(&app, bar);
    assert_eq!(
        (t.height, t.padding_x, t.gap),
        (Some(50.0), Some(12.0), Some(9.0))
    );
    let (br, ar, bbr) = (rect(&app, bar), rect(&app, a), rect(&app, b));
    assert_eq!(br[3], 50.0, "theme height is the node's height: {br:?}");
    assert_eq!(ar[0] - br[0], 12.0, "padding_x pads the left edge");
    assert_eq!(bbr[0] - (ar[0] + ar[2]), 9.0, "gap between the children");
    assert_eq!(
        br[2],
        12.0 + 10.0 + 9.0 + 10.0 + 12.0,
        "padding_x pads the right edge too"
    );
    let (plr, par, pbr) = (rect(&app, plain), rect(&app, pa), rect(&app, pb));
    assert_eq!((plr[2], plr[3]), (20.0, 10.0), "control: no theme box {plr:?}");
    assert_eq!((par[0] - plr[0], pbr[0] - (par[0] + par[2])), (0.0, 0.0));

    // Full `padding` is more specific than `padding_x`: it wins on every edge.
    let padded = classed(&mut app, r, "Bar.padded");
    let c = fixed(&mut app, padded, 10.0, 10.0);
    settle(&mut app);
    let (pr, cr) = (rect(&app, padded), rect(&app, c));
    assert_eq!(
        (cr[0] - pr[0], cr[1] - pr[1]),
        (6.0, 3.0),
        "padding over padding_x"
    );
    assert_eq!(pr[3], 50.0, "the inherited height still applies");

    // A per-node override changes the box again.
    app.world_mut()
        .entity_mut(bar)
        .insert(ThemeOverride::new(VisualOverrides {
            height: Some(30.0),
            padding_x: Some(2.0),
            gap: Some(1.0),
            ..Default::default()
        }));
    settle(&mut app);
    let (br, ar, bbr) = (rect(&app, bar), rect(&app, a), rect(&app, b));
    assert_eq!(br[3], 30.0);
    assert_eq!((ar[0] - br[0], bbr[0] - (ar[0] + ar[2])), (2.0, 1.0));
}

#[test]
fn size_token_heights_stay_with_the_widget_and_do_not_resize_layout() {
    // `size.control.*` / `size.pad-x.*` resolve for a node carrying a `WidgetSize` (agents read
    // them), but only a theme property moves layout: widgets size themselves (`UiDensity`).
    let mut app = ui_app();
    let r = root(&mut app);
    let e = app
        .world_mut()
        .spawn((
            UiNode,
            WidgetSize::Md,
            Classes(vec!["button".to_owned()]),
            NodeStyle::new(Style {
                size: [Dimension::Points(90.0), Dimension::Points(30.0)],
                ..Default::default()
            }),
        ))
        .id();
    app.world_mut().entity_mut(r).add_child(e);
    settle(&mut app);
    let t = themed(&app, e);
    assert_eq!(t.height, Some(44.0), "the md control height token resolves");
    let er = rect(&app, e);
    assert_eq!((er[2], er[3]), (90.0, 30.0), "the node keeps its own size");
}

// --- post05w3 (ADR-9340): caption font, size and outline tokens -----------------------------------

/// A theme over `regia-dark` with a larger, mono, purple-outlined caption style.
const CAPTION_THEME: &str = r##"(format_version: 1, name: "big-captions", extends: "regia-dark",
    tokens: {
        "subtitle.size": 1.5,
        "subtitle.font": "Noto Sans Mono",
        "subtitle.outline": "#800080",
        "subtitle.outline-width": 3,
    })"##;

fn caption_app(theme: Option<&str>) -> App {
    let mut app = ui_app();
    app.insert_resource(TextSystem::embedded_only());
    app.add_plugins(CaptionPlugin);
    if let Some(src) = theme {
        app.world_mut()
            .resource_mut::<UiThemes>()
            .set
            .load_ron(src, "big-captions.theme.ron")
            .expect("caption theme loads");
        app.world_mut()
            .insert_resource(ActiveTheme::named("big-captions"));
    }
    settle(&mut app);
    app.world_mut().write_message(cue("Hold the line"));
    settle(&mut app);
    app
}

/// The caption row's text entity.
fn caption_text(app: &mut App) -> Entity {
    let world = app.world_mut();
    world
        .query::<(&CaptionRow, &Children)>()
        .iter(world)
        .map(|(_, c)| c.iter().next().expect("row text"))
        .next()
        .expect("a caption row")
}

/// Font px of the caption's shaped glyphs (all one size).
fn caption_glyph_px(app: &mut App) -> f32 {
    let text = caption_text(app);
    let shaped = app.world().get::<ShapedText>(text).expect("shaped caption");
    let first = shaped.glyphs.first().expect("glyphs");
    f32::from_bits(first.cache_key.font_size_bits)
}

#[test]
fn a_caption_size_token_changes_the_glyph_size() {
    let mut plain = caption_app(None);
    let base = caption_glyph_px(&mut plain);
    let mut big = caption_app(Some(CAPTION_THEME));
    let scaled = caption_glyph_px(&mut big);
    assert!(
        (scaled / base - 1.5).abs() < 0.02,
        "subtitle.size 1.5 scales the glyphs: {base} -> {scaled}"
    );
    // The theme font names the caption's family.
    let text = caption_text(&mut big);
    let family = app_family(&big, text);
    assert_eq!(family.as_deref(), Some("Noto Sans Mono"));
    let text = caption_text(&mut plain);
    assert_eq!(app_family(&plain, text), None, "no token: the UI font");
}

fn app_family(app: &App, text: Entity) -> Option<String> {
    app.world().get::<Text>(text).expect("text").spans[0]
        .style
        .family
        .clone()
}

/// Draw-list quads of the caption: (glyph outline layers, plate-colored fills).
fn caption_quads(app: &App) -> (usize, usize) {
    let renderer = app.world().resource::<SharedUiRenderer>().lock();
    let outline = renderer
        .quads()
        .iter()
        .filter(|q| matches!(q.texture, regia_ui::draw::TextureRef::GlyphEffect(_)))
        .count();
    let plates = renderer
        .quads()
        .iter()
        .filter(|q| matches!(q.texture, regia_ui::draw::TextureRef::Rect) && q.color.0[3] > 0.0)
        .count();
    (outline, plates)
}

#[test]
fn an_outline_caption_adds_outline_quads_to_the_draw_list() {
    let mut boxed = caption_app(None);
    settle(&mut boxed);
    let (box_outline, box_plates) = caption_quads(&boxed);
    assert_eq!(box_outline, 0, "Box draws no outline layer");
    assert!(box_plates >= 1, "Box draws its plate");

    let mut outlined = caption_app(None);
    outlined
        .world_mut()
        .resource_mut::<regia_ui::captions::CaptionOptions>()
        .background = regia_ui::captions::CaptionBackground::Outline;
    settle(&mut outlined);
    let glyphs = {
        let text = caption_text(&mut outlined);
        outlined
            .world()
            .get::<ShapedText>(text)
            .expect("shaped")
            .glyphs
            .len()
    };
    let (outline, plates) = caption_quads(&outlined);
    assert!(
        outline >= glyphs.saturating_sub(2) && outline > box_outline,
        "one outline layer per drawn glyph: {outline} for {glyphs} glyphs (Box: {box_outline})"
    );
    assert_eq!(plates, box_plates - 1, "Outline draws no plate");

    // The theme's outline color and width reach the layer.
    let mut themed_app = caption_app(Some(CAPTION_THEME));
    themed_app
        .world_mut()
        .resource_mut::<regia_ui::captions::CaptionOptions>()
        .background = regia_ui::captions::CaptionBackground::Outline;
    settle(&mut themed_app);
    let text = caption_text(&mut themed_app);
    let effects = themed_app.world().get::<Text>(text).expect("text").spans[0]
        .style
        .effects
        .clone();
    let purple = themed_app
        .world()
        .resource::<ThemeResolver>()
        .resolved_theme()
        .and_then(|t| t.color("subtitle.outline"))
        .expect("subtitle.outline");
    assert_eq!(effects.len(), 1, "{effects:?}");
    assert_eq!((effects[0].color, effects[0].width), (purple, 3.0));
}
