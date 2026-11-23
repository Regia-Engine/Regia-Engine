// SPDX-License-Identifier: MIT-0
//! The "Parity" gallery page (ADR-2045): text effects, decorations and spacing, the UI materials,
//! a drawn tree view, a drawn data grid, a drawn color picker and separators. Every entry carries
//! a `NodeName` (`gallery.parity.*`) so agent checks can find it with `ui.query`.

use regia::prelude::*;
use regia_ui::color_picker::ColorPicker;
use regia_ui::parity_widgets::{spawn_color_picker, spawn_data_grid, spawn_separator, spawn_tree_view};
use regia_ui::style::FlexDirection;
use regia_ui::text_effects::TextEffect;
use regia_ui::tree::{DataGrid, GridColumn, TreeSpec};
use regia_ui::ui_material::{UiMaterial, UiMaterialKind};
use regia_ui::{
    Border, Dimension, NodeName, NodeStyle, Style, Text, TextSpan, TextStyle, UiColor, UiNode, WrapMode,
};

fn label(commands: &mut Commands, parent: Entity, name: &str, text: Text) -> Entity {
    let e = commands.spawn((UiNode, NodeName(name.to_owned()), text)).id();
    commands.entity(parent).add_child(e);
    e
}

fn styled(text: &str, style: TextStyle) -> Text {
    Text {
        spans: vec![TextSpan::lit(text).with_style(style)],
        wrap: WrapMode::None,
        ..Default::default()
    }
}

fn row(commands: &mut Commands, parent: Entity) -> Entity {
    let e = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                direction: FlexDirection::Row,
                gap: [Dimension::Points(16.0), Dimension::Points(16.0)],
                flex_wrap: true,
                size: [Dimension::Percent(100.0), Dimension::Auto],
                ..Default::default()
            }),
        ))
        .id();
    commands.entity(parent).add_child(e);
    e
}

/// Fills the parity page `page` with one example of each ADR-2045 feature.
pub fn fill(commands: &mut Commands, page: Entity) {
    let black = UiColor([0.0, 0.0, 0.0, 1.0]);
    let text_row = row(commands, page);
    label(
        commands,
        text_row,
        "gallery.parity.outline",
        styled(
            "Outline",
            TextStyle {
                font_px: 28.0,
                effects: vec![TextEffect::outline(2.0, black)],
                ..Default::default()
            },
        ),
    );
    label(
        commands,
        text_row,
        "gallery.parity.shadow",
        styled(
            "Shadow",
            TextStyle {
                font_px: 28.0,
                effects: vec![TextEffect::shadow([3.0, 3.0], 4.0, UiColor([0.0, 0.0, 0.0, 0.8]))],
                ..Default::default()
            },
        ),
    );
    label(
        commands,
        text_row,
        "gallery.parity.stacked",
        styled(
            "Stacked",
            TextStyle {
                font_px: 28.0,
                color: UiColor([1.0, 0.85, 0.3, 1.0]),
                effects: vec![
                    TextEffect::outline(1.5, black),
                    TextEffect {
                        color: UiColor([1.0, 0.4, 0.1, 0.6]),
                        width: 3.0,
                        blur: 3.0,
                        ..Default::default()
                    },
                ],
                ..Default::default()
            },
        ),
    );
    let deco_row = row(commands, page);
    label(
        commands,
        deco_row,
        "gallery.parity.decorations",
        Text {
            spans: regia_ui::text::parse_markup(
                "<u>underline</u> and <s>strikethrough</s>",
                TextStyle {
                    font_px: 20.0,
                    ..Default::default()
                },
            ),
            wrap: WrapMode::None,
            ..Default::default()
        },
    );
    label(
        commands,
        deco_row,
        "gallery.parity.spacing",
        styled(
            "LETTER SPACING",
            TextStyle {
                font_px: 18.0,
                letter_spacing: 0.2,
                weight: Some(700),
                ..Default::default()
            },
        ),
    );
    label(
        commands,
        deco_row,
        "gallery.parity.features",
        styled(
            "Tabular 0123456789",
            TextStyle {
                font_px: 18.0,
                features: vec!["tnum".into()],
                ..Default::default()
            },
        ),
    );
    // Distance-field text (L07): sharp at a large size from one field per glyph.
    let sdf = label(
        commands,
        page,
        "gallery.parity.sdf",
        styled(
            "Distance field 48 px",
            TextStyle {
                font_px: 48.0,
                ..Default::default()
            },
        ),
    );
    commands.entity(sdf).insert(regia_ui::sdf_text::SdfText);
    spawn_separator(commands, page, false);

    // UI materials: one swatch per kind.
    let mats = row(commands, page);
    for kind in UiMaterialKind::ALL {
        let mut m = UiMaterial::new(kind);
        m.color = UiColor([0.12, 0.16, 0.22, 1.0]);
        m.color2 = UiColor([0.35, 0.65, 1.0, 1.0]);
        if kind == UiMaterialKind::Dissolve {
            m.params[0] = 0.35;
        }
        let e = commands
            .spawn((
                UiNode,
                NodeStyle::new(Style {
                    size: [Dimension::Points(96.0), Dimension::Points(48.0)],
                    ..Default::default()
                }),
                Border {
                    color: UiColor([1.0, 1.0, 1.0, 0.2]),
                    widths: [1.0; 4],
                    radius: [8.0; 4],
                },
                m,
                NodeName(format!("gallery.parity.material.{}", kind.name())),
            ))
            .id();
        commands.entity(mats).add_child(e);
    }
    spawn_separator(commands, page, false);

    let widgets = row(commands, page);
    let tree_box = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Points(240.0), Dimension::Auto],
                ..Default::default()
            }),
        ))
        .id();
    commands.entity(widgets).add_child(tree_box);
    let leaf = |l: &str| TreeSpec {
        label: l.into(),
        children: vec![],
    };
    let roots = vec![
        TreeSpec {
            label: "assets".into(),
            children: vec![
                TreeSpec {
                    label: "meshes".into(),
                    children: vec![leaf("rock.glb"), leaf("tree.glb")],
                },
                leaf("sky.hdr"),
            ],
        },
        leaf("README.md"),
    ];
    if let Ok(tree) = spawn_tree_view(commands, tree_box, &roots, 180.0) {
        commands
            .entity(tree)
            .insert(NodeName("gallery.parity.tree".into()));
    }
    let grid_box = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Points(260.0), Dimension::Auto],
                ..Default::default()
            }),
        ))
        .id();
    commands.entity(widgets).add_child(grid_box);
    let grid = DataGrid {
        columns: vec![
            GridColumn {
                title: "Unit".into(),
                width: 140.0,
            },
            GridColumn {
                title: "HP".into(),
                width: 60.0,
            },
            GridColumn {
                title: "Cost".into(),
                width: 60.0,
            },
        ],
        rows: vec![
            vec!["Scout".into(), "40".into(), "50".into()],
            vec!["Tank".into(), "300".into(), "400".into()],
            vec!["Medic".into(), "60".into(), "120".into()],
            vec!["Sniper".into(), "35".into(), "180".into()],
        ],
        sort: None,
    };
    let g = spawn_data_grid(commands, grid_box, grid, 180.0);
    commands.entity(g).insert(NodeName("gallery.parity.grid".into()));
    spawn_separator(commands, widgets, true);
    let mut picker = ColorPicker::default();
    picker.set_hex("#3399FF");
    let p = spawn_color_picker(commands, widgets, picker, 160.0);
    commands
        .entity(p)
        .insert(NodeName("gallery.parity.color_picker".into()));
    spawn_separator(commands, page, false);

    // Search box over a command list, and a progress dialog behind a button.
    let tools = row(commands, page);
    let column = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                direction: FlexDirection::Column,
                gap: [Dimension::Points(6.0), Dimension::Points(6.0)],
                size: [Dimension::Points(260.0), Dimension::Auto],
                ..Default::default()
            }),
        ))
        .id();
    commands.entity(tools).add_child(column);
    let results = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Percent(100.0), Dimension::Points(120.0)],
                ..Default::default()
            }),
            regia_ui::ScrollView::vertical(),
            regia_ui::ListView::new(24.0, 2),
            regia_ui::ListItems::default(),
            NodeName("gallery.parity.search_results".into()),
        ))
        .id();
    let search = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Percent(100.0), Dimension::Points(32.0)],
                ..Default::default()
            }),
            // The text input's other fields are private: the default (empty, no placeholder).
            regia_ui::TextInput::default(),
            regia_ui::parity_widgets::SearchBox {
                target: Some(results),
                items: [
                    "Open level",
                    "Save level",
                    "Bake lighting",
                    "Toggle grid",
                    "Frame selection",
                    "Quit",
                ]
                .into_iter()
                .map(String::from)
                .collect(),
                ..Default::default()
            },
            regia_ui::Interactable::default(),
            regia_ui::Focusable::default(),
            regia_ui::a11y::Accessible::default().with_label("Search actions"),
            regia_ui::Classes(vec!["Panel.field".into()]),
            regia_ui::Themed::default(),
            NodeName("gallery.parity.search".into()),
        ))
        .id();
    commands.entity(column).add_child(search);
    commands.entity(column).add_child(results);
    let open = regia_ui::widgets::spawn_button(commands, tools, "Progress dialog", &[]);
    commands.entity(open).insert(ParityAction::OpenProgress);
    let dialog = regia_ui::parity_widgets::spawn_progress_dialog(commands, page, "Baking lighting", true);
    commands.entity(dialog).insert((
        NodeName("gallery.parity.progress".into()),
        regia_ui::widgets::Modal {
            open: false,
            close_on_cancel: true,
        },
    ));
}

/// Buttons of the parity page.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParityAction {
    /// Open the progress dialog.
    OpenProgress,
}

/// Opens the progress dialog and advances it while open (a fixed step per frame).
pub fn drive_parity_page(
    mut events: MessageReader<UiEvent>,
    actions: Query<&ParityAction>,
    mut dialogs: Query<(
        &mut regia_ui::parity_widgets::ProgressDialog,
        &mut regia_ui::widgets::Modal,
    )>,
) {
    for e in events.read() {
        if matches!(e.kind, UiEventKind::Click) && actions.get(e.target).is_ok() {
            for (mut d, mut m) in &mut dialogs {
                d.progress = 0.0;
                d.cancelled = false;
                m.open = true;
            }
        }
    }
    for (mut d, m) in &mut dialogs {
        if m.open && d.progress < 1.0 {
            d.progress = (d.progress + 0.01).min(1.0);
            d.status = format!("cell {} of 100", (d.progress * 100.0).round() as u32);
        }
    }
}
