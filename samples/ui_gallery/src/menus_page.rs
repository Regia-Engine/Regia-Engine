// SPDX-License-Identifier: MIT-0
//! Menus and popups page (ADR-273 lane 5): a menu bar, a long popup menu with a search field, a
//! context menu, a dropdown inside a clipped scroll view, a dropdown at the bottom edge (its list
//! flips above) and a tooltip that also shows on focus. Press 5 (or F5) to show or hide it.

use regia::prelude::*;
use regia_ui::menu::{ContextMenu, MenuActivations, MenuBar, MenuEntry, MenuItem, MenuSearch, PopupMenu};
use regia_ui::style::{Align, FlexDirection};
use regia_ui::widgets::{Dropdown, Tooltip, spawn_button};
use regia_ui::{Themed, WrapMode};

/// Adds the menus page to the gallery.
pub struct MenusPage;

impl Plugin for MenusPage {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, spawn_menus_page)
            .add_systems(Update, (toggle_menus_page, show_last_activation));
    }
}

/// The page's root node (hidden until 5 / F5).
#[derive(Component)]
struct MenusPageRoot;

/// The text that shows the last activated menu item.
#[derive(Component)]
struct LastActivation;

/// The page's close button.
#[derive(Component)]
struct ClosePage;

fn column(gap: f32) -> Style {
    Style {
        direction: FlexDirection::Column,
        gap: [Dimension::Points(gap), Dimension::Points(gap)],
        align_items: Align::Start,
        ..Default::default()
    }
}

fn label(commands: &mut Commands, parent: Entity, text: &str) {
    let node = commands
        .spawn((
            UiNode,
            Text {
                spans: vec![TextSpan::lit(text)],
                wrap: WrapMode::None,
                ..Default::default()
            },
        ))
        .id();
    commands.entity(parent).add_child(node);
}

fn file_menu() -> Vec<MenuEntry> {
    vec![
        MenuItem::new("new", "New").into(),
        MenuItem::new("open", "Open...").into(),
        MenuItem::new("recent", "Open recent")
            .with_submenu(vec![
                MenuItem::new("level1", "level_01.level.ron").into(),
                MenuItem::new("level2", "level_02.level.ron").into(),
            ])
            .into(),
        MenuEntry::Separator,
        MenuItem::new("quit", "Quit").disabled().into(),
    ]
}

fn view_menu() -> Vec<MenuEntry> {
    vec![
        MenuItem::new("grid", "Show grid").checkable(true).into(),
        MenuItem::new("stats", "Show stats").checkable(false).into(),
        MenuEntry::Separator,
        MenuItem::new("zoom", "Zoom")
            .with_submenu(vec![
                MenuItem::new("50", "50 %").into(),
                MenuItem::new("100", "100 %").into(),
                MenuItem::new("200", "200 %").into(),
            ])
            .into(),
    ]
}

fn spawn_menus_page(mut commands: Commands) {
    let root = commands
        .spawn((
            UiRoot {
                scale_mode: ScaleMode::ConstantPixelSize,
                sort_order: 1,
                ..Default::default()
            },
            SafeAreaPadding,
            MenusPageRoot,
            NodeStyle::new(Style {
                display: Display::None,
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                ..column(12.0)
            }),
            Classes(vec!["surface".into(), "panel".into()]),
            Themed::default(),
        ))
        .id();

    let bar = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                direction: FlexDirection::Row,
                size: [Dimension::Percent(100.0), Dimension::Auto],
                ..Default::default()
            }),
            MenuBar {
                menus: vec![
                    MenuItem::new("file", "File").with_submenu(file_menu()),
                    MenuItem::new("edit", "Edit").with_submenu(vec![
                        MenuItem::new("undo", "Undo").into(),
                        MenuItem::new("redo", "Redo").disabled().into(),
                        MenuEntry::Separator,
                        MenuItem::new("copy", "Copy").into(),
                        MenuItem::new("paste", "Paste").into(),
                    ]),
                    MenuItem::new("view", "View").with_submenu(view_menu()),
                ],
            },
        ))
        .id();
    commands.entity(root).add_child(bar);

    let body = commands.spawn((UiNode, NodeStyle::new(column(12.0)))).id();
    commands.entity(root).add_child(body);
    label(
        &mut commands,
        body,
        "Menus and popups (press 5 to go back). Keyboard: arrows, Enter, Escape. Gamepad: d-pad, A, B, Y.",
    );

    let more = spawn_button(&mut commands, body, "Popup menu (long, with search)", &[]);
    let names = [
        "Alpha", "Bravo", "Charlie", "Delta", "Echo", "Foxtrot", "Golf", "Hotel", "India", "Juliett", "Kilo",
        "Lima", "Mike", "November", "Oscar", "Papa",
    ];
    commands.entity(more).insert(PopupMenu {
        entries: names
            .iter()
            .map(|n| MenuItem::new(n.to_lowercase(), *n).into())
            .collect(),
        search: MenuSearch::Auto,
        ..Default::default()
    });

    let context = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Points(420.0), Dimension::Points(80.0)],
                padding: Edges::px(12.0),
                ..Default::default()
            }),
            Interactable::default(),
            Focusable::default(),
            ContextMenu {
                entries: vec![
                    MenuItem::new("cut", "Cut").into(),
                    MenuItem::new("copy", "Copy").into(),
                    MenuItem::new("paste", "Paste").disabled().into(),
                    MenuEntry::Separator,
                    MenuItem::new("pin", "Pinned").checkable(false).into(),
                ],
                ..Default::default()
            },
            Classes(vec!["surface".into()]),
            Themed::default(),
        ))
        .id();
    commands.entity(body).add_child(context);
    label(
        &mut commands,
        context,
        "Right-click, long-press, or focus and press Y / the menu key",
    );

    let scroll = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Points(420.0), Dimension::Points(64.0)],
                padding: Edges::px(8.0),
                ..column(8.0)
            }),
            ClipChildren,
            ScrollView::default(),
            ScrollOffset::default(),
            Classes(vec!["surface".into()]),
            Themed::default(),
        ))
        .id();
    commands.entity(body).add_child(scroll);
    let clipped = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Points(240.0), Dimension::Auto],
                padding: Edges::px(8.0),
                ..Default::default()
            }),
            Interactable::default(),
            Focusable::default(),
            Dropdown {
                options: [
                    "Low",
                    "Medium",
                    "High",
                    "Ultra",
                    "Custom",
                    "Potato",
                    "Cinematic",
                    "Off",
                ]
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
                selected: Some(1),
                open: false,
            },
            Classes(vec!["Button.field".into()]),
            Themed::default(),
        ))
        .id();
    commands.entity(scroll).add_child(clipped);

    let tip = spawn_button(
        &mut commands,
        body,
        "Tooltip (hover, or focus with a gamepad)",
        &[],
    );
    commands.entity(tip).insert({
        let mut t = Tooltip::default();
        t.text = "Tooltips open on the popup layer, outside every clip.".into();
        t
    });
    let close = spawn_button(&mut commands, body, "Back to the gallery", &["primary"]);
    commands.entity(close).insert(ClosePage);

    let last = commands
        .spawn((
            UiNode,
            LastActivation,
            Text {
                spans: vec![TextSpan::lit("Last menu item: none")],
                wrap: WrapMode::None,
                ..Default::default()
            },
        ))
        .id();
    commands.entity(body).add_child(last);

    // A dropdown at the bottom edge: its list flips above.
    let bottom = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                position: PositionType::Absolute,
                inset: Edges {
                    left: Dimension::Points(24.0),
                    top: Dimension::Auto,
                    right: Dimension::Auto,
                    bottom: Dimension::Points(24.0),
                },
                size: [Dimension::Points(240.0), Dimension::Auto],
                padding: Edges::px(8.0),
                ..Default::default()
            }),
            Interactable::default(),
            Focusable::default(),
            Dropdown {
                options: ["English", "Deutsch", "Français", "日本語", "العربية"]
                    .iter()
                    .map(|s| (*s).to_owned())
                    .collect(),
                selected: Some(0),
                open: false,
            },
            Classes(vec!["Button.field".into()]),
            Themed::default(),
        ))
        .id();
    commands.entity(root).add_child(bottom);
}

fn toggle_menus_page(
    mut keys: MessageReader<KeyboardInput>,
    mut events: MessageReader<UiEvent>,
    close: Query<(), With<ClosePage>>,
    mut pages: Query<&mut NodeStyle, With<MenusPageRoot>>,
) {
    let mut toggle = false;
    for key in keys.read() {
        if key.state == ButtonState::Pressed
            && !key.repeat
            && matches!(key.physical_key.as_str(), "Digit5" | "F5")
        {
            toggle = !toggle;
        }
    }
    for event in events.read() {
        if event.kind == UiEventKind::Click && close.get(event.target).is_ok() {
            toggle = !toggle;
        }
    }
    if !toggle {
        return;
    }
    for mut style in &mut pages {
        style.0.display = if style.0.display == Display::None {
            Display::Flex
        } else {
            Display::None
        };
    }
}

fn show_last_activation(
    activations: Option<Res<MenuActivations>>,
    mut texts: Query<&mut Text, With<LastActivation>>,
) {
    let Some(last) = activations.and_then(|a| a.activations.last().cloned()) else {
        return;
    };
    let mut label = format!("Last menu item: {}", last.path.join(" / "));
    if let Some(checked) = last.checked {
        label.push_str(if checked { " (checked)" } else { " (unchecked)" });
    }
    for mut text in &mut texts {
        let current = text.spans.first().and_then(|s| match &s.content {
            regia_ui::text::TextContent::Lit(t) => Some(t.clone()),
            _ => None,
        });
        if current.as_deref() != Some(label.as_str()) {
            text.spans = vec![TextSpan::lit(label.clone())];
        }
    }
}
