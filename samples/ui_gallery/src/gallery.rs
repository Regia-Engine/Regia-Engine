// SPDX-License-Identifier: MIT-0
//! M7 UI gallery: every widget, theme switch, RTL/CJK/emoji, 1 000-row virtual list,
//! keyboard/gamepad focus nav, language switch, safe-area root (`specs/18` § 7).

use regia::prelude::*;
use regia_ui::a11y::Accessible;
use regia_ui::radial::{RadialConfirm, RadialEvent, RadialEventKind, RadialOpen, RadialSector};
use regia_ui::style::{Align, FlexDirection, JustifyContent, Overflow};
use regia_ui::theme::UiThemes;
use regia_ui::widgets::{
    Dropdown, Modal, ProgressBar, Slider, TabBar, TextInput, Toggle, Tooltip, VirtualButton, VirtualStick,
    spawn_button,
};
use regia_ui::{
    BackgroundKind, ItemCount, LayoutDirty, ListItem, ListItems, Localization, NodeName, SafeAreaPadding,
    ScaleMode, Themed, UiPlugin, WrapMode,
};

/// Bound score for data-binding demos.
#[derive(Resource, Reflect, Default, Clone)]
#[reflect(Resource, Default)]
pub struct GalleryScore {
    /// Score value.
    pub value: u32,
}

/// Which content page is visible.
#[derive(Resource, Clone, Copy, PartialEq, Eq, Default)]
enum GalleryPage {
    #[default]
    Widgets,
    Text,
    List,
    Wheel,
    ThemeSheet,
    Parity,
}

/// The ui_gallery sample plugin.
#[derive(Default)]
pub struct UiGallery;

impl Plugin for UiGallery {
    fn build(&self, app: &mut App) {
        // `DefaultPlugins` already installs `UiPlugin` when the umbrella `ui` feature is on; keep an
        // explicit install for builds that disable that feature or construct the app without
        // `DefaultPlugins`.
        if !app.has_plugin(std::any::type_name::<UiPlugin>()) {
            app.add_plugins(UiPlugin::code_only());
        }
        app.register_type::<GalleryScore>()
            .insert_resource(GalleryScore { value: 42 })
            .insert_resource(GalleryPage::Widgets)
            .insert_resource(ClearColor([0.06, 0.07, 0.09, 1.0]))
            .add_systems(Startup, (setup_localization_and_fonts, spawn_gallery).chain())
            .add_systems(Update, (handle_gallery_input, tick_score, show_wheel_result))
            .add_systems(Update, crate::parity_page::drive_parity_page);
        app.add_plugins(crate::menus_page::MenusPage);
    }
}

fn setup_localization_and_fonts(mut loc: ResMut<Localization>) {
    let sources = vec![
        (
            "en".to_owned(),
            include_str!("../assets/localization/en/ui.ftl").to_owned(),
        ),
        (
            "ar".to_owned(),
            include_str!("../assets/localization/ar/ui.ftl").to_owned(),
        ),
        (
            "zh".to_owned(),
            include_str!("../assets/localization/zh/ui.ftl").to_owned(),
        ),
    ];
    match Localization::new(sources, Some("en".to_owned())) {
        Ok(loaded) => *loc = loaded,
        Err(err) => error!("gallery localization: {err}"),
    }
}

/// Text samples on the widgets page (`Text samples` panel). Rendered unpinned: the
/// engine's default font set (Inter + bundled Noto subsets + fallback chain) covers them,
/// which is exactly what the font-coverage test asserts.
const WIDGET_PAGE_SAMPLES: &[&str] = &[
    "The quick brown fox jumps over the lazy dog.",
    "مرحبا بالعالم — النص العربي",
    "你好世界 — 日本語のかなと漢字",
    "Regia Engine UI 🚀🎮✨",
];

/// Text samples on the Text page: (caption, sample).
const TEXT_PAGE_SAMPLES: &[(&str, &str)] = &[
    ("Latin", "The quick brown fox jumps over the lazy dog."),
    ("Arabic RTL", "مرحبا بالعالم — النص العربي مع التشكيل."),
    ("CJK", "你好世界 — 日本語のかなと漢字、한국어."),
    ("Emoji", "Regia Engine UI 🚀🎮✨ — flags 🇺🇸🇯🇵"),
];

/// Every sample string the gallery renders; `tests/font_coverage.rs` shapes each of these
/// (plus the localized strings) with the engine's default set and rejects `.notdef`.
pub fn sample_strings() -> Vec<String> {
    WIDGET_PAGE_SAMPLES
        .iter()
        .map(|s| (*s).to_owned())
        .chain(TEXT_PAGE_SAMPLES.iter().map(|(_, sample)| (*sample).to_owned()))
        .collect()
}

fn spawn_gallery(mut commands: Commands, loc: Res<Localization>) {
    let root = commands
        .spawn((
            UiRoot {
                scale_mode: ScaleMode::ConstantPixelSize,
                ..Default::default()
            },
            SafeAreaPadding,
            NodeStyle::new(Style {
                direction: FlexDirection::Column,
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                padding: Edges::px(16.0),
                gap: [Dimension::Points(12.0), Dimension::Points(12.0)],
                align_items: Align::Stretch,
                ..Default::default()
            }),
            Background {
                kind: BackgroundKind::Color(
                    UiColor::from_hex("#10141a").unwrap_or(UiColor([0.06, 0.08, 0.1, 1.0])),
                ),
            },
            Classes(vec!["root".into(), "surface".into()]),
            Themed::default(),
        ))
        .id();

    let title = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Percent(100.0), Dimension::Auto],
                ..Default::default()
            }),
            Text {
                spans: vec![TextSpan::loc("gallery-title").with_style(TextStyle {
                    font_px: 28.0,
                    ..Default::default()
                })],
                wrap: WrapMode::None,
                ..Default::default()
            },
        ))
        .id();
    commands.entity(root).add_child(title);

    let toolbar = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                direction: FlexDirection::Row,
                size: [Dimension::Percent(100.0), Dimension::Auto],
                gap: [Dimension::Points(8.0), Dimension::Points(8.0)],
                flex_wrap: true,
                align_items: Align::Center,
                ..Default::default()
            }),
        ))
        .id();
    commands.entity(root).add_child(toolbar);

    let theme_btn = spawn_button(&mut commands, toolbar, &loc.resolve("btn-toggle-theme", &[]), &[]);
    commands.entity(theme_btn).insert(GalleryAction::ToggleTheme);
    let lang_btn = spawn_button(&mut commands, toolbar, &loc.resolve("btn-cycle-locale", &[]), &[]);
    commands.entity(lang_btn).insert(GalleryAction::CycleLocale);
    let widgets_btn = spawn_button(&mut commands, toolbar, &loc.resolve("gallery-widgets", &[]), &[]);
    commands
        .entity(widgets_btn)
        .insert(GalleryAction::Page(GalleryPage::Widgets));
    let text_btn = spawn_button(&mut commands, toolbar, &loc.resolve("gallery-text", &[]), &[]);
    commands
        .entity(text_btn)
        .insert(GalleryAction::Page(GalleryPage::Text));
    let list_btn = spawn_button(&mut commands, toolbar, &loc.resolve("gallery-list", &[]), &[]);
    commands
        .entity(list_btn)
        .insert(GalleryAction::Page(GalleryPage::List));

    spawn_widgets_page(&mut commands, root, &loc);
    spawn_text_page(&mut commands, root);
    spawn_list_page(&mut commands, root);
    let wheel_btn = spawn_button(&mut commands, toolbar, "Wheel", &[]);
    commands
        .entity(wheel_btn)
        .insert(GalleryAction::Page(GalleryPage::Wheel));
    spawn_wheel_page(&mut commands, root);
    // The theme sheet (lane D3): the toolbar button is focusable, so a gamepad reaches the page.
    let sheet_btn = spawn_button(&mut commands, toolbar, "Theme sheet", &[]);
    commands.entity(sheet_btn).insert((
        GalleryAction::Page(GalleryPage::ThemeSheet),
        NodeName("gallery.page.theme_sheet".into()),
    ));
    spawn_theme_sheet_page(&mut commands, root);
    // Text effects, UI materials and the drawn tree, grid and color picker (ADR-2045).
    let parity_btn = spawn_button(&mut commands, toolbar, "Parity", &[]);
    commands.entity(parity_btn).insert((
        GalleryAction::Page(GalleryPage::Parity),
        NodeName("gallery.page.parity".into()),
    ));
    spawn_parity_page(&mut commands, root);
}

/// The parity page (ADR-2045): its content lives in `parity_page.rs`.
fn spawn_parity_page(commands: &mut Commands, parent: Entity) {
    let page = commands
        .spawn((
            UiNode,
            PageRoot(GalleryPage::Parity),
            NodeStyle::new(Style {
                direction: FlexDirection::Column,
                size: [Dimension::Percent(100.0), Dimension::Auto],
                gap: [Dimension::Points(12.0), Dimension::Points(12.0)],
                padding: Edges::px(16.0),
                flex_grow: 1.0,
                display: Display::None,
                ..scrolling_page()
            }),
            page_scroll(),
            ScrollOffset::default(),
            Interactable::default(),
            Classes(vec!["surface".into(), "panel".into()]),
            Themed::default(),
            NodeName("gallery.parity".into()),
        ))
        .id();
    commands.entity(parent).add_child(page);
    crate::parity_page::fill(commands, page);
}

/// The theme sheet page: every widget in every state and size (`theme_sheet.rs`).
fn spawn_theme_sheet_page(commands: &mut Commands, parent: Entity) {
    let page = commands
        .spawn((
            UiNode,
            PageRoot(GalleryPage::ThemeSheet),
            NodeStyle::new(Style {
                direction: FlexDirection::Column,
                size: [Dimension::Percent(100.0), Dimension::Auto],
                display: Display::None,
                gap: [Dimension::Points(6.0), Dimension::Points(6.0)],
                padding: Edges::px(12.0),
                flex_grow: 1.0,
                align_items: Align::Stretch,
                ..scrolling_page()
            }),
            page_scroll(),
            ScrollOffset::default(),
            Interactable::default(),
            Classes(vec!["surface".into(), "panel".into()]),
            Themed::default(),
        ))
        .id();
    commands.entity(parent).add_child(page);
    crate::theme_sheet::spawn_theme_sheet(commands, page);
}

/// Marks the text that shows the last radial-wheel result.
#[derive(Component)]
struct WheelResult;

/// The radial-wheel page: hold the button (or press Space on a gamepad via the `Wheel` action) and
/// point with the mouse, a stick or a finger; release to select.
fn spawn_wheel_page(commands: &mut Commands, parent: Entity) {
    let page = commands
        .spawn((
            UiNode,
            PageRoot(GalleryPage::Wheel),
            NodeStyle::new(Style {
                direction: FlexDirection::Column,
                size: [Dimension::Percent(100.0), Dimension::Auto],
                gap: [Dimension::Points(8.0), Dimension::Points(8.0)],
                padding: Edges::px(12.0),
                flex_grow: 1.0,
                align_items: Align::Center,
                display: Display::None,
                ..Default::default()
            }),
            Classes(vec!["surface".into(), "panel".into()]),
            Themed::default(),
        ))
        .id();
    commands.entity(parent).add_child(page);
    let hold = spawn_button(commands, page, "Hold to open the wheel", &["primary"]);
    commands.entity(hold).insert(VirtualButton {
        action: "Wheel".to_owned(),
    });
    let result = commands
        .spawn((
            UiNode,
            WheelResult,
            Text {
                spans: vec![TextSpan::lit("Last: none")],
                wrap: WrapMode::None,
                ..Default::default()
            },
        ))
        .id();
    commands.entity(page).add_child(result);
    let stage = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Points(460.0), Dimension::Points(460.0)],
                ..Default::default()
            }),
        ))
        .id();
    commands.entity(page).add_child(stage);
    let sectors = [
        "Rifle", "Pistol", "Grenade", "Smoke", "Medkit", "Knife", "Flare", "Radio",
    ]
    .iter()
    .map(|name| RadialSector::new(name.to_lowercase(), *name))
    .collect();
    let mut menu = RadialMenu::new(sectors)
        .with_center(RadialSector::new("holster", "Holster"))
        .with_open(RadialOpen::Hold)
        .with_confirm(RadialConfirm::Release)
        .with_radii(64.0, 210.0)
        .with_open_action("Wheel");
    menu.sectors[6].enabled = false;
    regia_ui::spawn_radial_menu(commands, stage, menu);
}

fn show_wheel_result(mut events: MessageReader<RadialEvent>, mut texts: Query<&mut Text, With<WheelResult>>) {
    for event in events.read() {
        let label = match &event.kind {
            RadialEventKind::Select { id, .. } => format!("Last: {id}"),
            RadialEventKind::Cancel => "Last: cancelled".to_owned(),
            _ => continue,
        };
        for mut text in &mut texts {
            text.spans = vec![TextSpan::lit(label.clone())];
        }
    }
}

#[derive(Component, Clone, Copy)]
enum GalleryAction {
    ToggleTheme,
    CycleLocale,
    Page(GalleryPage),
}

#[derive(Component)]
struct PageRoot(GalleryPage);

/// What a page that may not fit needs in its style: it may shrink below its content and clips it, so
/// its height comes from the window and the rest scrolls. A page that ran off the window would lose
/// text at any text scale (found by the accessibility audit, ADR-3025).
fn scrolling_page() -> Style {
    Style {
        min_size: [Dimension::Auto, Dimension::Points(0.0)],
        // Flex items do not shrink by default in this engine.
        flex_shrink: 1.0,
        overflow: [Overflow::Clip, Overflow::Clip],
        ..Default::default()
    }
}

/// The scroll view of a page, with a visible bar.
fn page_scroll() -> ScrollView {
    let mut scroll = ScrollView::vertical();
    scroll.show_scrollbar = true;
    scroll
}

fn labeled_row(commands: &mut Commands, parent: Entity, label: &str) -> Entity {
    let row = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                direction: FlexDirection::Column,
                size: [Dimension::Percent(100.0), Dimension::Auto],
                gap: [Dimension::Points(4.0), Dimension::Points(4.0)],
                align_items: Align::Stretch,
                ..Default::default()
            }),
        ))
        .id();
    commands.entity(parent).add_child(row);
    let caption = commands
        .spawn((
            UiNode,
            Text {
                spans: vec![TextSpan::lit(label).with_style(TextStyle {
                    font_px: 14.0,
                    ..Default::default()
                })],
                wrap: WrapMode::None,
                ..Default::default()
            },
        ))
        .id();
    commands.entity(row).add_child(caption);
    row
}

fn spawn_widgets_page(commands: &mut Commands, parent: Entity, loc: &Localization) {
    let page = commands
        .spawn((
            UiNode,
            PageRoot(GalleryPage::Widgets),
            NodeStyle::new(Style {
                direction: FlexDirection::Column,
                size: [Dimension::Percent(100.0), Dimension::Auto],
                gap: [Dimension::Points(8.0), Dimension::Points(8.0)],
                padding: Edges::px(12.0),
                flex_grow: 1.0,
                align_items: Align::Stretch,
                ..scrolling_page()
            }),
            page_scroll(),
            ScrollOffset::default(),
            Interactable::default(),
            Classes(vec!["surface".into(), "panel".into()]),
            Themed::default(),
        ))
        .id();
    commands.entity(parent).add_child(page);

    let btn_row = labeled_row(commands, page, "Buttons");
    let actions = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                direction: FlexDirection::Row,
                gap: [Dimension::Points(8.0), Dimension::Points(8.0)],
                flex_wrap: true,
                ..Default::default()
            }),
        ))
        .id();
    commands.entity(btn_row).add_child(actions);
    spawn_button(commands, actions, &loc.resolve("menu-start", &[]), &["primary"]);
    spawn_button(commands, actions, "Secondary", &[]);

    let toggle_row = labeled_row(commands, page, &loc.resolve("label-toggle", &[]));
    let toggle = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Points(64.0), Dimension::Points(32.0)],
                ..Default::default()
            }),
            Interactable::default(),
            Focusable::default(),
            Toggle { checked: true },
            Accessible::default().with_label(loc.resolve("label-toggle", &[])),
            Classes(vec!["toggle".into()]),
            Themed::default(),
        ))
        .id();
    commands.entity(toggle_row).add_child(toggle);

    let slider_row = labeled_row(commands, page, &loc.resolve("label-slider", &[]));
    let slider = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Percent(100.0), Dimension::Points(28.0)],
                ..Default::default()
            }),
            Interactable::default(),
            Focusable::default(),
            Slider {
                value: 0.6,
                min: 0.0,
                max: 1.0,
                step: 0.05,
            },
            Accessible::default().with_label(loc.resolve("label-slider", &[])),
            Classes(vec!["slider-track".into()]),
            Themed::default(),
        ))
        .id();
    commands.entity(slider_row).add_child(slider);

    let progress_row = labeled_row(commands, page, &loc.resolve("label-progress", &[]));
    let progress = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Percent(100.0), Dimension::Points(16.0)],
                ..Default::default()
            }),
            ProgressBar { value: 0.35 },
            Classes(vec!["surface".into()]),
            Themed::default(),
        ))
        .id();
    commands.entity(progress_row).add_child(progress);

    let input_row = labeled_row(commands, page, &loc.resolve("label-input", &[]));
    let input = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Percent(100.0), Dimension::Auto],
                min_size: [Dimension::Auto, Dimension::Points(36.0)],
                padding: Edges::px(8.0),
                justify_content: JustifyContent::Center,
                align_items: Align::Center,
                ..Default::default()
            }),
            Interactable::default(),
            Focusable::default(),
            {
                let mut input = TextInput::default();
                input.placeholder = loc.resolve("label-input", &[]);
                input
            },
            Classes(vec!["Panel.field".into()]),
            Themed::default(),
        ))
        .id();
    commands.entity(input_row).add_child(input);

    let drop_row = labeled_row(commands, page, "Dropdown");
    let dropdown = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Points(220.0), Dimension::Points(36.0)],
                padding: Edges::px(8.0),
                justify_content: JustifyContent::Center,
                align_items: Align::Center,
                ..Default::default()
            }),
            Interactable::default(),
            Focusable::default(),
            Dropdown {
                options: vec!["Alpha".into(), "Beta".into(), "Gamma".into()],
                selected: Some(0),
                open: false,
            },
            Accessible::default().with_label("Dropdown"),
            Classes(vec!["Button.field".into()]),
            Themed::default(),
        ))
        .id();
    commands.entity(drop_row).add_child(dropdown);

    let tabs_row = labeled_row(commands, page, "Tabs");
    let tabs = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Percent(100.0), Dimension::Points(40.0)],
                direction: FlexDirection::Row,
                gap: [Dimension::Points(4.0), Dimension::Points(4.0)],
                ..Default::default()
            }),
            TabBar {
                tabs: vec!["One".into(), "Two".into(), "Three".into()],
                active: 0,
            },
            Accessible::default().with_label("Tabs"),
            Classes(vec!["tab".into()]),
            Themed::default(),
            Focusable::default(),
            Interactable::default(),
        ))
        .id();
    commands.entity(tabs_row).add_child(tabs);

    // Text samples + list sit above the fold at 1280×720 so the default evidence shot shows them.
    let text_samples = labeled_row(commands, page, "Text samples (RTL / CJK / emoji)");
    let samples_panel = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                direction: FlexDirection::Column,
                size: [Dimension::Percent(100.0), Dimension::Auto],
                gap: [Dimension::Points(6.0), Dimension::Points(6.0)],
                padding: Edges::px(10.0),
                align_items: Align::Stretch,
                ..Default::default()
            }),
            Classes(vec!["surface".into()]),
            Themed::default(),
        ))
        .id();
    commands.entity(text_samples).add_child(samples_panel);
    for sample in WIDGET_PAGE_SAMPLES {
        let sample = *sample;
        let line = commands
            .spawn((
                UiNode,
                NodeStyle::new(Style {
                    size: [Dimension::Percent(100.0), Dimension::Auto],
                    padding: Edges::px(2.0),
                    ..Default::default()
                }),
                Text {
                    spans: vec![TextSpan::lit(sample).with_style(TextStyle {
                        font_px: 16.0,
                        // Unpinned: the default stack must cover Latin, Arabic, CJK and
                        // emoji by itself (engine-bundled subsets + fallback chain).
                        ..Default::default()
                    })],
                    wrap: WrapMode::None,
                    ..Default::default()
                },
            ))
            .id();
        commands.entity(samples_panel).add_child(line);
    }

    let list_preview = labeled_row(commands, page, "List preview");
    let items: Vec<ListItem> = (0..40)
        .map(|i| ListItem {
            label: format!("Item {i:02}"),
            ..Default::default()
        })
        .collect();
    let list = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Percent(100.0), Dimension::Points(96.0)],
                ..Default::default()
            }),
            {
                let mut scroll = ScrollView::default();
                scroll.inertia = 0.2;
                scroll.show_scrollbar = true;
                scroll
            },
            ScrollOffset::default(),
            {
                let mut list = ListView::default();
                list.item_height = 28.0;
                list.overscan = 2;
                list
            },
            ListItems(items),
            ItemCount(40),
            ClipChildren,
            Interactable::default(),
            Classes(vec!["surface".into()]),
            Themed::default(),
        ))
        .id();
    commands.entity(list_preview).add_child(list);

    let tip_row = labeled_row(commands, page, "Tooltip");
    let tooltip_host = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                padding: Edges::px(8.0),
                justify_content: JustifyContent::Center,
                align_items: Align::Center,
                ..Default::default()
            }),
            Interactable::default(),
            Focusable::default(),
            {
                let mut tip = Tooltip::default();
                tip.text = "Hello tooltip".into();
                tip.delay = 0.35;
                tip
            },
            Classes(vec!["button".into()]),
            Themed::default(),
        ))
        .with_children(|b| {
            b.spawn((
                UiNode,
                Text {
                    spans: vec![TextSpan::lit("Hover me")],
                    wrap: WrapMode::None,
                    ..Default::default()
                },
            ));
        })
        .id();
    commands.entity(tip_row).add_child(tooltip_host);

    // Touch controls in-flow (not absolute overlays) so they don't look like stray popups.
    let touch_row = labeled_row(commands, page, "Touch controls");
    let touch = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                direction: FlexDirection::Row,
                gap: [Dimension::Points(16.0), Dimension::Points(16.0)],
                align_items: Align::Center,
                ..Default::default()
            }),
        ))
        .id();
    commands.entity(touch_row).add_child(touch);
    let stick = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Points(96.0), Dimension::Points(96.0)],
                ..Default::default()
            }),
            Interactable::default(),
            {
                let mut stick = VirtualStick::default();
                stick.action = "Move".into();
                stick.radius = 40.0;
                stick
            },
            Accessible::default().with_label("Move stick"),
            Classes(vec!["surface".into()]),
            Themed::default(),
        ))
        .id();
    commands.entity(touch).add_child(stick);
    let vbtn = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Points(56.0), Dimension::Points(56.0)],
                justify_content: JustifyContent::Center,
                align_items: Align::Center,
                ..Default::default()
            }),
            Interactable::default(),
            Focusable::default(),
            VirtualButton {
                action: "Jump".into(),
            },
            Classes(vec!["button".into()]),
            Themed::default(),
        ))
        .with_children(|b| {
            b.spawn((
                UiNode,
                Text {
                    spans: vec![TextSpan::lit("A")],
                    wrap: WrapMode::None,
                    ..Default::default()
                },
            ));
        })
        .id();
    commands.entity(touch).add_child(vbtn);

    let modal_row = labeled_row(commands, page, "Modal (press M)");
    let modal = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Points(280.0), Dimension::Points(48.0)],
                padding: Edges::px(8.0),
                ..Default::default()
            }),
            Modal {
                open: false,
                close_on_cancel: true,
            },
            FocusScope,
            Classes(vec!["surface".into()]),
            Themed::default(),
            Text {
                spans: vec![TextSpan::lit("Modal (press M to open)")],
                wrap: WrapMode::None,
                ..Default::default()
            },
        ))
        .id();
    commands.entity(modal_row).add_child(modal);

    let scroll_row = labeled_row(commands, page, "ScrollView");
    let scroll = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Percent(100.0), Dimension::Points(72.0)],
                ..Default::default()
            }),
            {
                let mut scroll = ScrollView::default();
                scroll.inertia = 0.18;
                scroll.show_scrollbar = true;
                scroll
            },
            ScrollOffset::default(),
            ClipChildren,
            Interactable::default(),
            Classes(vec!["surface".into()]),
            Themed::default(),
        ))
        .with_children(|c| {
            c.spawn((
                UiNode,
                NodeStyle::new(Style {
                    size: [Dimension::Auto, Dimension::Points(180.0)],
                    padding: Edges::px(8.0),
                    ..Default::default()
                }),
                Text {
                    spans: vec![TextSpan::lit(
                        "ScrollView with inertia.\nLine 2\nLine 3\nLine 4\nLine 5\nLine 6",
                    )],
                    ..Default::default()
                },
            ));
        })
        .id();
    commands.entity(scroll_row).add_child(scroll);
}

fn spawn_text_page(commands: &mut Commands, parent: Entity) {
    let page = commands
        .spawn((
            UiNode,
            PageRoot(GalleryPage::Text),
            NodeStyle::new(Style {
                direction: FlexDirection::Column,
                size: [Dimension::Percent(100.0), Dimension::Auto],
                gap: [Dimension::Points(12.0), Dimension::Points(12.0)],
                padding: Edges::px(16.0),
                flex_grow: 1.0,
                align_items: Align::Stretch,
                display: Display::None,
                ..Default::default()
            }),
            Classes(vec!["surface".into(), "panel".into()]),
            Themed::default(),
        ))
        .id();
    commands.entity(parent).add_child(page);

    for (label, sample) in TEXT_PAGE_SAMPLES {
        let (label, sample) = (*label, *sample);
        let row = commands
            .spawn((
                UiNode,
                NodeStyle::new(Style {
                    direction: FlexDirection::Column,
                    gap: [Dimension::Points(4.0), Dimension::Points(4.0)],
                    padding: Edges::px(8.0),
                    size: [Dimension::Percent(100.0), Dimension::Auto],
                    ..Default::default()
                }),
                Classes(vec!["surface".into()]),
                Themed::default(),
            ))
            .id();
        commands.entity(page).add_child(row);
        let caption = commands
            .spawn((
                UiNode,
                Text {
                    spans: vec![TextSpan::lit(label).with_style(TextStyle {
                        font_px: 14.0,
                        ..Default::default()
                    })],
                    ..Default::default()
                },
            ))
            .id();
        commands.entity(row).add_child(caption);
        let body = commands
            .spawn((
                UiNode,
                Text {
                    spans: vec![TextSpan::lit(sample).with_style(TextStyle {
                        font_px: 22.0,
                        ..Default::default()
                    })],
                    wrap: WrapMode::Word,
                    ..Default::default()
                },
            ))
            .id();
        commands.entity(row).add_child(body);
    }
}

fn spawn_list_page(commands: &mut Commands, parent: Entity) {
    let page = commands
        .spawn((
            UiNode,
            PageRoot(GalleryPage::List),
            NodeStyle::new(Style {
                direction: FlexDirection::Column,
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                flex_grow: 1.0,
                align_items: Align::Stretch,
                display: Display::None,
                padding: Edges::px(8.0),
                // Under the title and the toolbar, 100 % ran past the window.
                ..scrolling_page()
            }),
            Classes(vec!["surface".into(), "panel".into()]),
            Themed::default(),
        ))
        .id();
    commands.entity(parent).add_child(page);

    let items: Vec<ListItem> = (0..1000)
        .map(|i| ListItem {
            label: format!("Item {i:04}"),
            ..Default::default()
        })
        .collect();

    let list = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                min_size: [Dimension::Points(200.0), Dimension::Points(320.0)],
                ..Default::default()
            }),
            {
                let mut scroll = ScrollView::default();
                scroll.inertia = 0.2;
                scroll.show_scrollbar = true;
                scroll
            },
            ScrollOffset::default(),
            {
                let mut list = ListView::default();
                list.item_height = 40.0;
                list.overscan = 4;
                list
            },
            ListItems(items),
            ItemCount(1000),
            ClipChildren,
            Interactable::default(),
            Classes(vec!["surface".into()]),
            Themed::default(),
        ))
        .id();
    commands.entity(page).add_child(list);
}

#[allow(clippy::too_many_arguments)]
fn handle_gallery_input(
    mut keys: MessageReader<KeyboardInput>,
    mut themes: ResMut<UiThemes>,
    mut loc: ResMut<Localization>,
    mut page: ResMut<GalleryPage>,
    mut modals: Query<&mut Modal>,
    mut pages: Query<(&PageRoot, &mut NodeStyle)>,
    mut events: MessageReader<UiEvent>,
    actions: Query<&GalleryAction>,
    mut dirty: ResMut<LayoutDirty>,
) {
    for event in events.read() {
        if !matches!(event.kind, UiEventKind::Click) {
            continue;
        }
        let Ok(action) = actions.get(event.target) else {
            continue;
        };
        match *action {
            GalleryAction::ToggleTheme => {
                themes.toggle();
                dirty.mark();
            }
            GalleryAction::CycleLocale => {
                loc.cycle_locale();
                dirty.mark();
            }
            GalleryAction::Page(p) => {
                *page = p;
                dirty.mark();
            }
        }
    }

    for key in keys.read() {
        if key.state != ButtonState::Pressed || key.repeat {
            continue;
        }
        match key.physical_key.as_str() {
            "KeyT" => {
                themes.toggle();
                dirty.mark();
            }
            "KeyL" => {
                loc.cycle_locale();
                dirty.mark();
            }
            "Digit1" => {
                *page = GalleryPage::Widgets;
                dirty.mark();
            }
            "Digit2" => {
                *page = GalleryPage::Text;
                dirty.mark();
            }
            "Digit3" => {
                *page = GalleryPage::List;
                dirty.mark();
            }
            "Digit4" => {
                *page = GalleryPage::Wheel;
                dirty.mark();
            }
            "Digit5" => {
                *page = GalleryPage::ThemeSheet;
                dirty.mark();
            }
            "Digit6" => {
                *page = GalleryPage::Parity;
                dirty.mark();
            }
            "KeyM" => {
                for mut modal in &mut modals {
                    modal.open = !modal.open;
                }
                dirty.mark();
            }
            _ => {}
        }
    }

    for (root, mut style) in &mut pages {
        style.0.display = if root.0 == *page {
            Display::Flex
        } else {
            Display::None
        };
    }
}

fn tick_score(mut score: ResMut<GalleryScore>) {
    score.value = score.value.wrapping_add(1).max(1);
}
