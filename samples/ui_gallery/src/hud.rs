// SPDX-License-Identifier: MIT-0
//! A dense DO27-style tactical HUD: resource bars, mission timer, objectives, a toast, a framed
//! minimap with unit markers, a chat/order log, unit cards, a command grid and a tooltip. Used
//! for by-eye review at Deck/1080p/1440p/4K (`examples/ui_shots.rs`) and the UI cost bench
//! (`tests/hud_cost.rs`). Every image is generated procedurally at startup.

use regia::core::glam::{Affine3A, Vec3};
use regia::prelude::*;
use regia_scene::GlobalTransform;
use regia_ui::game_widgets::{Radar, RadarBlip};
use regia_ui::render::SharedUiRenderer;
use regia_ui::style::{Align, FlexDirection, JustifyContent};
use regia_ui::{BackgroundKind, ScaleMode, UiPlugin, WrapMode};

/// Image id: minimap terrain.
pub const IMG_MINIMAP: u64 = 0xD027_0001;
/// Image id: 9-slice panel frame.
pub const IMG_FRAME: u64 = 0xD027_0002;
/// Image id: unit portrait.
pub const IMG_PORTRAIT: u64 = 0xD027_0003;
/// First resource / command icon id (`IMG_ICON + n`).
pub const IMG_ICON: u64 = 0xD027_0100;
/// 9-slice insets of [`IMG_FRAME`] in source pixels.
pub const FRAME_INSETS: [f32; 4] = [20.0, 20.0, 20.0, 20.0];

const ACCENT: &str = "#e9b949";
const PANEL: &str = "#0c1117d0";
const LINE: &str = "#ffffff1c";
const DIM: &str = "#8d9aa8";
const TEXT: &str = "#e8edf2";

/// The HUD sample plugin. `HudSample::live` makes the timer and one bar change every frame (the
/// cost bench's "live" scenario); the default HUD only changes once per second.
#[derive(Default)]
pub struct HudSample;

/// HUD animation state.
#[derive(Resource, Default)]
pub struct HudClock {
    /// Elapsed mission seconds.
    pub seconds: f64,
    /// Animate a resource bar every frame (cost bench).
    pub live: bool,
    last_whole: i64,
}

#[derive(Component)]
struct TimerText;

#[derive(Component)]
struct FuelFill;

impl Plugin for HudSample {
    fn build(&self, app: &mut App) {
        if !app.has_plugin(std::any::type_name::<UiPlugin>()) {
            app.add_plugins(UiPlugin::code_only());
        }
        app.init_resource::<HudClock>()
            .insert_resource(ClearColor([0.02, 0.025, 0.03, 1.0]))
            .add_systems(Startup, (register_images, spawn_hud))
            .add_systems(Update, tick_hud);
    }
}

fn c(hex: &str) -> UiColor {
    UiColor::from_hex(hex).unwrap_or(UiColor::WHITE)
}

fn pt(v: f32) -> Dimension {
    Dimension::Points(v)
}

fn pct(v: f32) -> Dimension {
    Dimension::Percent(v)
}

fn text(s: &str, px: f32, color: &str, bold: bool) -> Text {
    Text {
        spans: vec![TextSpan::lit(s).with_style(TextStyle {
            font_px: px,
            color: c(color),
            bold,
            ..Default::default()
        })],
        wrap: WrapMode::None,
        ..Default::default()
    }
}

fn rich(s: &str, px: f32, wrap: WrapMode) -> Text {
    Text {
        spans: parse_markup(
            s,
            TextStyle {
                font_px: px,
                color: c(TEXT),
                ..Default::default()
            },
        ),
        wrap,
        ..Default::default()
    }
}

fn abs(left: Option<f32>, top: Option<f32>, right: Option<f32>, bottom: Option<f32>) -> Style {
    let side = |v: Option<f32>| v.map_or(Dimension::Auto, Dimension::Points);
    Style {
        position: PositionType::Absolute,
        inset: Edges {
            left: side(left),
            top: side(top),
            right: side(right),
            bottom: side(bottom),
        },
        ..Default::default()
    }
}

fn panel_bits(radius: f32) -> (Background, Border, Shadow) {
    (
        Background {
            kind: BackgroundKind::Color(c(PANEL)),
        },
        Border {
            color: c(LINE),
            widths: [1.0; 4],
            radius: [radius; 4],
        },
        Shadow {
            color: UiColor([0.0, 0.0, 0.0, 0.55]),
            blur: 14.0,
            offset: [0.0, 4.0],
        },
    )
}

fn child(commands: &mut Commands, parent: Entity, bundle: impl Bundle) -> Entity {
    let id = commands.spawn((UiNode, bundle)).id();
    commands.entity(parent).add_child(id);
    id
}

/// A horizontal bar: rounded track + fill at `value` (0..1).
fn bar(commands: &mut Commands, parent: Entity, w: Dimension, h: f32, value: f32, fill: &str) -> Entity {
    let track = child(
        commands,
        parent,
        (
            NodeStyle::new(Style {
                size: [w, pt(h)],
                ..Default::default()
            }),
            Background {
                kind: BackgroundKind::Color(c("#ffffff14")),
            },
            Border {
                radius: [h * 0.5; 4],
                ..Default::default()
            },
        ),
    );
    child(
        commands,
        track,
        (
            NodeStyle::new(Style {
                size: [pct(value * 100.0), pct(100.0)],
                ..Default::default()
            }),
            Background {
                kind: BackgroundKind::Gradient {
                    start: c(fill),
                    end: c(fill).lerp(UiColor::WHITE, 0.25),
                    angle: 90.0,
                },
            },
            Border {
                radius: [h * 0.5; 4],
                ..Default::default()
            },
        ),
    )
}

fn spawn_hud(mut commands: Commands) {
    let root = commands
        .spawn((
            UiRoot {
                scale_mode: ScaleMode::ScaleWithScreen {
                    reference: [1920.0, 1080.0],
                    match_width: 0.5,
                },
                ..Default::default()
            },
            NodeStyle::new(Style {
                size: [pct(100.0), pct(100.0)],
                ..Default::default()
            }),
        ))
        .id();
    spawn_top_bar(&mut commands, root);
    spawn_toast(&mut commands, root);
    spawn_minimap(&mut commands, root);
    spawn_log(&mut commands, root);
    spawn_unit_cards(&mut commands, root);
    spawn_commands(&mut commands, root);
    spawn_tooltip(&mut commands, root);
}

fn spawn_top_bar(commands: &mut Commands, root: Entity) {
    let bar_row = child(
        commands,
        root,
        NodeStyle::new(Style {
            direction: FlexDirection::Row,
            justify_content: JustifyContent::SpaceBetween,
            align_items: Align::Start,
            padding: Edges::px(16.0),
            ..abs(Some(0.0), Some(0.0), Some(0.0), None)
        }),
    );
    // Resources.
    let resources = child(
        commands,
        bar_row,
        NodeStyle::new(Style {
            direction: FlexDirection::Row,
            gap: [pt(10.0), pt(10.0)],
            ..Default::default()
        }),
    );
    let items = [
        ("SUPPLY", "2,450", "/4,000", 0.61, "#e9b949", 0),
        ("FUEL", "1,180", "/2,000", 0.59, "#5fb7ff", 1),
        ("AMMO", "860", "/1,500", 0.57, "#ff7a59", 2),
        ("MANPOWER", "312", "/400", 0.78, "#7ddc8a", 3),
    ];
    for (label, value, cap, frac, color, icon) in items {
        let chip = child(
            commands,
            resources,
            (
                NodeStyle::new(Style {
                    direction: FlexDirection::Row,
                    align_items: Align::Center,
                    gap: [pt(10.0), pt(10.0)],
                    padding: Edges {
                        left: pt(10.0),
                        top: pt(8.0),
                        right: pt(14.0),
                        bottom: pt(8.0),
                    },
                    ..Default::default()
                }),
                panel_bits(6.0),
            ),
        );
        child(
            commands,
            chip,
            (
                NodeStyle::new(Style {
                    size: [pt(28.0), pt(28.0)],
                    ..Default::default()
                }),
                UiImage {
                    id: IMG_ICON + icon,
                    tint: c(color),
                    ..Default::default()
                },
            ),
        );
        let col = child(
            commands,
            chip,
            NodeStyle::new(Style {
                direction: FlexDirection::Column,
                gap: [pt(3.0), pt(3.0)],
                ..Default::default()
            }),
        );
        child(commands, col, text(label, 11.0, DIM, true));
        child(
            commands,
            col,
            rich(
                &format!("<b>{value}</b><color={DIM}><size=13>{cap}</size></color>"),
                18.0,
                WrapMode::None,
            ),
        );
        let fill = bar(commands, col, pt(112.0), 4.0, frac, color);
        if label == "FUEL" {
            commands.entity(fill).insert(FuelFill);
        }
    }
    // Mission timer.
    let mission = child(
        commands,
        bar_row,
        (
            NodeStyle::new(Style {
                direction: FlexDirection::Column,
                align_items: Align::Center,
                padding: Edges {
                    left: pt(28.0),
                    top: pt(6.0),
                    right: pt(28.0),
                    bottom: pt(10.0),
                },
                ..Default::default()
            }),
            panel_bits(6.0),
        ),
    );
    child(commands, mission, (text("12:47", 30.0, TEXT, true), TimerText));
    child(
        commands,
        mission,
        text("OPERATION IRON VEIL  ·  PHASE 2", 11.0, ACCENT, true),
    );
    // Objectives.
    let objectives = child(
        commands,
        bar_row,
        (
            NodeStyle::new(Style {
                direction: FlexDirection::Column,
                gap: [pt(8.0), pt(8.0)],
                padding: Edges::px(14.0),
                size: [pt(340.0), Dimension::Auto],
                ..Default::default()
            }),
            panel_bits(6.0),
        ),
    );
    child(commands, objectives, text("OBJECTIVES", 12.0, ACCENT, true));
    let rows = [
        (true, "Secure the river crossing at Brod", ""),
        (false, "Destroy enemy supply depots", "2/3"),
        (false, "Hold Hill 214 until extraction", "04:12"),
    ];
    for (done, label, note) in rows {
        let row = child(
            commands,
            objectives,
            NodeStyle::new(Style {
                direction: FlexDirection::Row,
                align_items: Align::Center,
                gap: [pt(8.0), pt(8.0)],
                ..Default::default()
            }),
        );
        child(
            commands,
            row,
            (
                NodeStyle::new(Style {
                    size: [pt(12.0), pt(12.0)],
                    ..Default::default()
                }),
                Background {
                    kind: BackgroundKind::Color(if done { c(ACCENT) } else { c("#00000000") }),
                },
                Border {
                    color: c(if done { ACCENT } else { DIM }),
                    widths: [1.5; 4],
                    radius: [2.0; 4],
                },
            ),
        );
        let label_node = child(
            commands,
            row,
            (
                NodeStyle::new(Style {
                    flex_grow: 1.0,
                    ..Default::default()
                }),
                text(label, 14.0, if done { DIM } else { TEXT }, false),
            ),
        );
        let _ = label_node;
        if !note.is_empty() {
            child(commands, row, text(note, 13.0, ACCENT, true));
        }
    }
}

fn spawn_toast(commands: &mut Commands, root: Entity) {
    let row = child(
        commands,
        root,
        NodeStyle::new(Style {
            direction: FlexDirection::Row,
            justify_content: JustifyContent::Center,
            ..abs(Some(0.0), Some(118.0), Some(0.0), None)
        }),
    );
    let toast = child(
        commands,
        row,
        (
            NodeStyle::new(Style {
                direction: FlexDirection::Row,
                align_items: Align::Center,
                gap: [pt(10.0), pt(10.0)],
                padding: Edges {
                    left: pt(16.0),
                    top: pt(9.0),
                    right: pt(18.0),
                    bottom: pt(9.0),
                },
                ..Default::default()
            }),
            Background {
                kind: BackgroundKind::Color(c("#141b23e6")),
            },
            Border {
                color: c(ACCENT),
                widths: [3.0, 0.0, 0.0, 0.0],
                radius: [4.0; 4],
            },
            Shadow {
                color: UiColor([0.0, 0.0, 0.0, 0.5]),
                blur: 18.0,
                offset: [0.0, 6.0],
            },
        ),
    );
    child(
        commands,
        toast,
        rich(
            "Reinforcements arriving in <b><color=#e9b949>0:45</color></b>  —  LZ Falcon",
            15.0,
            WrapMode::None,
        ),
    );
}

fn spawn_minimap(commands: &mut Commands, root: Entity) {
    let frame = child(
        commands,
        root,
        (
            NodeStyle::new(Style {
                size: [pt(300.0), pt(300.0)],
                padding: Edges::px(14.0),
                ..abs(Some(16.0), None, None, Some(16.0))
            }),
            Background {
                kind: BackgroundKind::Image {
                    id: IMG_FRAME,
                    uv: [0.0, 0.0, 1.0, 1.0],
                    nine_slice: Some(FRAME_INSETS),
                },
            },
            Shadow {
                color: UiColor([0.0, 0.0, 0.0, 0.6]),
                blur: 16.0,
                offset: [0.0, 4.0],
            },
        ),
    );
    // The radar widget of `regia_ui` draws the map and the blips; the area is the unit square, so
    // a unit's map position is its world position.
    let map = child(
        commands,
        frame,
        (
            NodeStyle::new(Style {
                size: [pct(100.0), pct(100.0)],
                ..Default::default()
            }),
            Radar::minimap([0.0, 0.0, 1.0, 1.0]).with_map(IMG_MINIMAP, [0.0; 4]),
        ),
    );
    let dots: [(f32, f32, &str); 13] = [
        (0.30, 0.62, "#5fb7ff"),
        (0.34, 0.66, "#5fb7ff"),
        (0.28, 0.69, "#5fb7ff"),
        (0.41, 0.58, "#5fb7ff"),
        (0.46, 0.71, "#5fb7ff"),
        (0.38, 0.74, "#5fb7ff"),
        (0.52, 0.64, "#5fb7ff"),
        (0.36, 0.60, "#5fb7ff"),
        (0.71, 0.27, "#ff5a4f"),
        (0.76, 0.31, "#ff5a4f"),
        (0.69, 0.35, "#ff5a4f"),
        (0.81, 0.22, "#ff5a4f"),
        (0.63, 0.30, "#ff5a4f"),
    ];
    for (x, z, color) in dots {
        commands.spawn((
            RadarBlip::dot(c(color)),
            GlobalTransform(Affine3A::from_translation(Vec3::new(x, 0.0, z))),
        ));
    }
    // Camera frustum.
    child(
        commands,
        map,
        (
            NodeStyle::new(Style {
                size: [pt(96.0), pt(64.0)],
                ..abs(Some(70.0), Some(130.0), None, None)
            }),
            Border {
                color: c("#ffffffd0"),
                widths: [1.5; 4],
                radius: [2.0; 4],
            },
        ),
    );
    child(
        commands,
        map,
        (
            NodeStyle::new(abs(Some(130.0), Some(4.0), None, None)),
            text("N", 14.0, TEXT, true),
        ),
    );
    child(
        commands,
        map,
        (
            NodeStyle::new(abs(Some(8.0), None, None, Some(6.0))),
            text("GRID 34T-EK 4471", 11.0, TEXT, true),
        ),
    );
}

fn spawn_log(commands: &mut Commands, root: Entity) {
    let log = child(
        commands,
        root,
        (
            NodeStyle::new(Style {
                direction: FlexDirection::Column,
                gap: [pt(3.0), pt(3.0)],
                padding: Edges::px(10.0),
                size: [pt(440.0), Dimension::Auto],
                ..abs(Some(16.0), None, None, Some(332.0))
            }),
            Background {
                kind: BackgroundKind::Gradient {
                    start: c("#05080b40"),
                    end: c("#05080bc0"),
                    angle: 0.0,
                },
            },
        ),
    );
    let lines = [
        (
            0.6,
            "<color=#8d9aa8>[12:31]</color> <color=#5fb7ff><b>Alpha-1:</b></color> Contact front, two hundred meters.",
        ),
        (
            0.7,
            "<color=#8d9aa8>[12:33]</color> <color=#e9b949><b>HQ:</b></color> Artillery available in 45 seconds.",
        ),
        (
            0.8,
            "<color=#8d9aa8>[12:38]</color> <color=#5fb7ff><b>Bravo-2:</b></color> Depot two destroyed, moving to rally.",
        ),
        (
            0.9,
            "<color=#8d9aa8>[12:41]</color> <color=#ff7a59><b>Warning:</b></color> Fuel convoy under fire near Brod bridge.",
        ),
        (
            1.0,
            "<color=#8d9aa8>[12:44]</color> <color=#7ddc8a><b>Charlie-3:</b></color> In position on Hill 214. Holding.",
        ),
        (
            1.0,
            "<color=#8d9aa8>[12:46]</color> <color=#5fb7ff><b>You:</b></color> All units, hold fire until my mark.",
        ),
    ];
    for (opacity, line) in lines {
        child(
            commands,
            log,
            (
                NodeStyle::new(Style {
                    size: [pct(100.0), Dimension::Auto],
                    ..Default::default()
                }),
                rich(line, 14.0, WrapMode::Word),
                Opacity(opacity),
            ),
        );
    }
}

fn spawn_unit_cards(commands: &mut Commands, root: Entity) {
    let row = child(
        commands,
        root,
        NodeStyle::new(Style {
            direction: FlexDirection::Row,
            justify_content: JustifyContent::Center,
            align_items: Align::End,
            gap: [pt(8.0), pt(8.0)],
            ..abs(Some(332.0), None, Some(362.0), Some(16.0))
        }),
    );
    let units = [
        ("Sgt. Okafor", "Squad lead", 0.92, 0.70, false, true),
        ("Cpl. Varga", "Rifleman", 0.64, 0.45, false, false),
        ("Pvt. Lindqvist", "Medic", 1.0, 0.90, false, false),
        ("Pvt. Haddad", "Machine gunner", 0.38, 0.22, true, false),
        ("Pvt. Ito", "Marksman", 0.81, 0.60, false, false),
        ("Spc. Moreau", "Engineer", 0.55, 0.80, false, false),
        ("Pvt. Novak", "Rifleman", 0.12, 0.35, true, false),
        ("Pvt. Achebe", "AT gunner", 0.73, 0.15, false, false),
    ];
    for (i, (name, role, hp, ammo, suppressed, selected)) in units.into_iter().enumerate() {
        let (bg, border, mut shadow) = panel_bits(6.0);
        let border = if selected {
            Border {
                color: c(ACCENT),
                widths: [2.0; 4],
                ..border
            }
        } else {
            border
        };
        if selected {
            shadow = Shadow {
                color: c("#e9b94966"),
                blur: 16.0,
                offset: [0.0, 0.0],
            };
        }
        let card = child(
            commands,
            row,
            (
                NodeStyle::new(Style {
                    direction: FlexDirection::Column,
                    gap: [pt(4.0), pt(4.0)],
                    padding: Edges::px(6.0),
                    size: [pt(118.0), pt(166.0)],
                    ..Default::default()
                }),
                bg,
                border,
                shadow,
            ),
        );
        let portrait = child(
            commands,
            card,
            (
                NodeStyle::new(Style {
                    size: [pct(100.0), pt(76.0)],
                    ..Default::default()
                }),
                UiImage {
                    id: IMG_PORTRAIT,
                    preserve_aspect: false,
                    tint: UiColor::WHITE.lerp(c(["#9fc7ff", "#ffd59a", "#b9f0c0"][i % 3]), 0.35),
                    ..Default::default()
                },
                Border {
                    radius: [4.0; 4],
                    ..Default::default()
                },
            ),
        );
        child(
            commands,
            portrait,
            (
                NodeStyle::new(abs(Some(4.0), Some(3.0), None, None)),
                text(&format!("{}", i + 1), 12.0, TEXT, true),
            ),
        );
        if suppressed {
            child(
                commands,
                portrait,
                (
                    NodeStyle::new(Style {
                        padding: Edges {
                            left: pt(5.0),
                            top: pt(1.0),
                            right: pt(5.0),
                            bottom: pt(1.0),
                        },
                        ..abs(None, None, Some(4.0), Some(4.0))
                    }),
                    Background {
                        kind: BackgroundKind::Color(c("#c8342bdd")),
                    },
                    Border {
                        radius: [7.0; 4],
                        ..Default::default()
                    },
                    text("PINNED", 9.0, TEXT, true),
                ),
            );
        }
        child(
            commands,
            card,
            (
                NodeStyle::new(Style {
                    size: [pct(100.0), Dimension::Auto],
                    ..Default::default()
                }),
                text(name, 13.0, TEXT, true).with_overflow(TextOverflow::Ellipsis),
            ),
        );
        child(commands, card, text(role, 11.0, DIM, false));
        let hp_color = if hp > 0.6 {
            "#7ddc8a"
        } else if hp > 0.3 {
            "#e9b949"
        } else {
            "#ff5a4f"
        };
        bar(commands, card, pct(100.0), 5.0, hp, hp_color);
        bar(commands, card, pct(100.0), 3.0, ammo, "#5fb7ff");
    }
}

fn spawn_commands(commands: &mut Commands, root: Entity) {
    let frame = child(
        commands,
        root,
        (
            NodeStyle::new(Style {
                direction: FlexDirection::Row,
                flex_wrap: true,
                gap: [pt(8.0), pt(8.0)],
                padding: Edges::px(18.0),
                size: [pt(330.0), Dimension::Auto],
                ..abs(None, None, Some(16.0), Some(16.0))
            }),
            Background {
                kind: BackgroundKind::Image {
                    id: IMG_FRAME,
                    uv: [0.0, 0.0, 1.0, 1.0],
                    nine_slice: Some(FRAME_INSETS),
                },
            },
            Shadow {
                color: UiColor([0.0, 0.0, 0.0, 0.6]),
                blur: 16.0,
                offset: [0.0, 4.0],
            },
        ),
    );
    let keys = ["Q", "W", "E", "R", "A", "S", "D", "F", "Z", "X", "C", "V"];
    for (i, key) in keys.iter().enumerate() {
        let hovered = i == 7;
        let disabled = i == 10;
        let button = child(
            commands,
            frame,
            (
                NodeStyle::new(Style {
                    size: [pt(66.0), pt(66.0)],
                    justify_content: JustifyContent::Center,
                    align_items: Align::Center,
                    ..Default::default()
                }),
                Background {
                    kind: BackgroundKind::Gradient {
                        start: c(if hovered { "#2a3644" } else { "#18212b" }),
                        end: c(if hovered { "#1a232d" } else { "#0e141a" }),
                        angle: 0.0,
                    },
                },
                Border {
                    color: c(if hovered { ACCENT } else { "#ffffff24" }),
                    widths: [1.0; 4],
                    radius: [6.0; 4],
                },
                Opacity(if disabled { 0.4 } else { 1.0 }),
            ),
        );
        child(
            commands,
            button,
            (
                NodeStyle::new(Style {
                    size: [pt(34.0), pt(34.0)],
                    ..Default::default()
                }),
                UiImage {
                    id: IMG_ICON + (i as u64 % 4),
                    tint: c(if hovered { ACCENT } else { "#c9d3dd" }),
                    ..Default::default()
                },
            ),
        );
        child(
            commands,
            button,
            (
                NodeStyle::new(abs(Some(5.0), Some(3.0), None, None)),
                text(key, 11.0, DIM, true),
            ),
        );
    }
}

fn spawn_tooltip(commands: &mut Commands, root: Entity) {
    let tip = child(
        commands,
        root,
        (
            NodeStyle::new(Style {
                direction: FlexDirection::Column,
                gap: [pt(6.0), pt(6.0)],
                padding: Edges::px(12.0),
                size: [pt(290.0), Dimension::Auto],
                ..abs(None, None, Some(16.0), Some(280.0))
            }),
            Background {
                kind: BackgroundKind::Color(c("#0a0e13f2")),
            },
            Border {
                color: c("#e9b94980"),
                widths: [1.0; 4],
                radius: [6.0; 4],
            },
            Shadow {
                color: UiColor([0.0, 0.0, 0.0, 0.65]),
                blur: 20.0,
                offset: [0.0, 8.0],
            },
        ),
    );
    child(commands, tip, text("Call Artillery", 16.0, TEXT, true));
    child(
        commands,
        tip,
        rich(
            "<color=#e9b949><b>150</b></color> Supply   <color=#5fb7ff><b>45 s</b></color> cooldown",
            13.0,
            WrapMode::None,
        ),
    );
    child(
        commands,
        tip,
        (
            NodeStyle::new(Style {
                size: [pct(100.0), Dimension::Auto],
                ..Default::default()
            }),
            rich(
                "Six-round 155 mm barrage on the target area. Friendly units inside the \
                 <color=#ff7a59>danger radius</color> are suppressed.",
                13.0,
                WrapMode::Word,
            ),
        ),
    );
    child(
        commands,
        tip,
        rich(
            "<color=#8d9aa8><b>[F]</b> Place    <b>[Esc]</b> Cancel</color>",
            11.0,
            WrapMode::None,
        ),
    );
}

fn tick_hud(
    time: Res<Time>,
    mut clock: ResMut<HudClock>,
    mut timer: Query<&mut Text, With<TimerText>>,
    mut fuel: Query<&mut NodeStyle, With<FuelFill>>,
) {
    clock.seconds += f64::from(time.delta_secs());
    let whole = clock.seconds as i64;
    if whole != clock.last_whole {
        clock.last_whole = whole;
        let total = 12 * 60 + 47 + whole;
        for mut t in &mut timer {
            *t = text(&format!("{:02}:{:02}", total / 60, total % 60), 30.0, TEXT, true);
        }
    }
    if clock.live {
        let v = 59.0 + 10.0 * (clock.seconds * 0.7).sin() as f32;
        for mut style in &mut fuel {
            style.0.size[0] = pct(v);
        }
    }
}

// ---- Procedural images ------------------------------------------------------------------

fn register_images(renderer: Option<Res<SharedUiRenderer>>) {
    let Some(renderer) = renderer else { return };
    let mut r = renderer.lock();
    r.queue_image_rgba(IMG_MINIMAP, 256, 256, minimap_image(256));
    r.queue_image_rgba(IMG_FRAME, 64, 64, frame_image(64));
    r.queue_image_rgba(IMG_PORTRAIT, 96, 64, portrait_image(96, 64));
    for i in 0..4 {
        r.queue_image_rgba(IMG_ICON + i, 64, 64, icon_image(i, 64));
    }
}

/// sRGB-encode a linear value to 8 bits.
fn enc(v: f32) -> u8 {
    let v = v.clamp(0.0, 1.0);
    let s = if v <= 0.003_130_8 {
        v * 12.92
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    };
    (s * 255.0 + 0.5) as u8
}

fn hash(x: i32, y: i32) -> f32 {
    let mut h = (x as u32).wrapping_mul(374_761_393) ^ (y as u32).wrapping_mul(668_265_263);
    h = (h ^ (h >> 13)).wrapping_mul(1_274_126_177);
    ((h ^ (h >> 16)) & 0xFFFF) as f32 / 65535.0
}

fn value_noise(x: f32, y: f32) -> f32 {
    let (xi, yi) = (x.floor() as i32, y.floor() as i32);
    let (fx, fy) = (x - x.floor(), y - y.floor());
    let s = |t: f32| t * t * (3.0 - 2.0 * t);
    let (sx, sy) = (s(fx), s(fy));
    let a = hash(xi, yi) + (hash(xi + 1, yi) - hash(xi, yi)) * sx;
    let b = hash(xi, yi + 1) + (hash(xi + 1, yi + 1) - hash(xi, yi + 1)) * sx;
    a + (b - a) * sy
}

fn minimap_image(n: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity((n * n * 4) as usize);
    for y in 0..n {
        for x in 0..n {
            let (u, v) = (x as f32 / n as f32, y as f32 / n as f32);
            let mut h = 0.0;
            let mut amp = 0.5;
            let mut f = 4.0;
            for _ in 0..4 {
                h += amp * value_noise(u * f, v * f);
                amp *= 0.5;
                f *= 2.0;
            }
            // River: a sine band; roads: two straight lines.
            let river = ((v - 0.5 - 0.12 * (u * 7.0).sin()).abs() - 0.012).max(0.0);
            let road = ((u - 0.55 + 0.3 * v).abs()).min((v - 0.2 - 0.1 * u).abs());
            let mut col = [0.10 + 0.25 * h, 0.13 + 0.22 * h, 0.07 + 0.12 * h];
            if (h * 10.0).fract() < 0.06 {
                col = col.map(|c| c * 0.8);
            }
            if river < 0.004 {
                col = [0.05, 0.12, 0.2];
            }
            if road < 0.004 {
                col = [0.32, 0.3, 0.26];
            }
            out.extend_from_slice(&[enc(col[0]), enc(col[1]), enc(col[2]), 255]);
        }
    }
    out
}

/// Rounded-box signed distance (pixel units) for procedural images.
fn sd_box(p: [f32; 2], c: [f32; 2], half: [f32; 2], r: f32) -> f32 {
    let q = [
        (p[0] - c[0]).abs() - half[0] + r,
        (p[1] - c[1]).abs() - half[1] + r,
    ];
    let outside = (q[0].max(0.0).powi(2) + q[1].max(0.0).powi(2)).sqrt();
    outside + q[0].max(q[1]).min(0.0) - r
}

fn cover(d: f32) -> f32 {
    (0.5 - d).clamp(0.0, 1.0)
}

/// Metal frame with a bevel and amber corner brackets; the centre is a translucent panel.
fn frame_image(n: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity((n * n * 4) as usize);
    let half = n as f32 * 0.5;
    for y in 0..n {
        for x in 0..n {
            let p = [x as f32 + 0.5, y as f32 + 0.5];
            let outer = sd_box(p, [half, half], [half - 1.0, half - 1.0], 7.0);
            let inner = sd_box(p, [half, half], [half - 9.0, half - 9.0], 3.0);
            let a_outer = cover(outer);
            let ring = a_outer * (1.0 - cover(inner));
            let edge = cover(outer.abs() - 0.75) + cover((inner).abs() - 0.6);
            let t = y as f32 / n as f32;
            let metal = 0.035 + 0.03 * (1.0 - t);
            let mut col = [metal, metal * 1.1, metal * 1.25];
            let mut a = ring + (a_outer - ring) * 0.72;
            let centre = [0.008, 0.011, 0.015];
            if ring < 0.5 {
                col = centre;
            }
            let e = edge.min(1.0) * 0.18;
            col = col.map(|c| c + e);
            // Corner brackets: L-shapes 14 px long, 2 px thick, inside the bevel.
            let (cx, cy) = (p[0].min(n as f32 - p[0]), p[1].min(n as f32 - p[1]));
            let bracket = (cx > 3.0 && cx < 5.5 && cy > 3.0 && cy < 17.0)
                || (cy > 3.0 && cy < 5.5 && cx > 3.0 && cx < 17.0);
            if bracket {
                col = [0.82, 0.48, 0.07];
                a = 1.0;
            }
            out.extend_from_slice(&[enc(col[0]), enc(col[1]), enc(col[2]), (a * 255.0 + 0.5) as u8]);
        }
    }
    out
}

fn portrait_image(w: u32, h: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let p = [x as f32 + 0.5, y as f32 + 0.5];
            let t = y as f32 / h as f32;
            let mut col = [0.05 + 0.06 * t, 0.07 + 0.07 * t, 0.09 + 0.06 * t];
            let head =
                ((p[0] - w as f32 * 0.5).powi(2) + (p[1] - h as f32 * 0.42).powi(2)).sqrt() - h as f32 * 0.2;
            let body = sd_box(
                p,
                [w as f32 * 0.5, h as f32 * 1.02],
                [w as f32 * 0.3, h as f32 * 0.32],
                12.0,
            );
            let helmet = sd_box(
                p,
                [w as f32 * 0.5, h as f32 * 0.28],
                [h as f32 * 0.24, h as f32 * 0.09],
                5.0,
            );
            let k = cover(head).max(cover(body));
            col = col.map(|c| c + (0.22 - c) * k);
            let hk = cover(helmet);
            col = [
                col[0] + (0.12 - col[0]) * hk,
                col[1] + (0.15 - col[1]) * hk,
                col[2] + (0.09 - col[2]) * hk,
            ];
            out.extend_from_slice(&[enc(col[0]), enc(col[1]), enc(col[2]), 255]);
        }
    }
    out
}

/// White alpha-mask icons (tinted by `UiImage::tint`): 0 crate, 1 drop, 2 rounds, 3 person.
fn icon_image(kind: u64, n: u32) -> Vec<u8> {
    let s = n as f32 / 32.0;
    let mut out = Vec::with_capacity((n * n * 4) as usize);
    for y in 0..n {
        for x in 0..n {
            let p = [(x as f32 + 0.5) / s, (y as f32 + 0.5) / s];
            let d = match kind {
                0 => {
                    let outer = sd_box(p, [16.0, 16.0], [11.0, 11.0], 2.0);
                    let inner = sd_box(p, [16.0, 16.0], [8.0, 8.0], 1.0);
                    let cross = sd_box(p, [16.0, 16.0], [9.0, 1.5], 0.0).min(sd_box(
                        p,
                        [16.0, 16.0],
                        [1.5, 9.0],
                        0.0,
                    ));
                    outer.max(-inner).min(cross)
                }
                1 => {
                    let circle = ((p[0] - 16.0).powi(2) + (p[1] - 19.0).powi(2)).sqrt() - 8.0;
                    let tip = (p[0] - 16.0).abs() * 0.8 + (p[1] - 5.0) * 0.6 - 3.0 + 0.0f32.max(p[1] - 19.0);
                    circle.min(tip.max(p[1] - 19.0))
                }
                2 => {
                    let mut d = f32::MAX;
                    for i in 0..3 {
                        let cx = 9.0 + i as f32 * 7.0;
                        let body = sd_box(p, [cx, 19.0], [2.6, 8.0], 0.5);
                        let tip = ((p[0] - cx).powi(2) + (p[1] - 11.0).powi(2)).sqrt() - 2.6;
                        d = d.min(body.min(tip.max(p[1] - 11.0)));
                    }
                    d
                }
                _ => {
                    let head = ((p[0] - 16.0).powi(2) + (p[1] - 10.0).powi(2)).sqrt() - 5.0;
                    let body = sd_box(p, [16.0, 25.0], [9.0, 7.0], 5.0);
                    head.min(body)
                }
            };
            let a = cover(d * s);
            out.extend_from_slice(&[255, 255, 255, (a * 255.0 + 0.5) as u8]);
        }
    }
    out
}
