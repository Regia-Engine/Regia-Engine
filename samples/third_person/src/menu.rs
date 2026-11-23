// SPDX-License-Identifier: MIT-0
//! Title screen and settings menu (ADR-2596), built with `regia_ui` on the framework's flow.
//!
//! The title shows while `GameFlow::MainMenu` is active (the level runs behind it, the enemies
//! idle). **Play** and **Continue** (load the save) go to `GameFlow::Loading`, which the flow
//! turns into `Playing`. **Settings** opens the settings panel, from the title or the pause menu:
//! master, music and effects volume, mouse sensitivity, invert Y and the graphics preset. Every
//! change edits `regia_framework::settings::GameSettings`; `SettingsPlugin` applies it live
//! (volumes to the audio buses, the preset to the renderer) and persists it to
//! `<data dir>/settings.ron`. The input code reads sensitivity and invert Y from the same resource.
//!
//! Every button carries a [`MenuAction`] and a `Name` (`menu-play`, `settings-master-up`, ...), so
//! an agent can click it with the UI methods; [`apply_action`] is the same code path for tests.

use regia::prelude::*;
use regia_framework::flow::GameFlow;
use regia_framework::settings::{GameSettings, GraphicsPreset};
use regia_ui::node::{Background, BackgroundKind, Classes, NodeStyle, UiNode};
use regia_ui::style::{Align, Dimension, Display, FlexDirection, JustifyContent};
use regia_ui::text::{Text, TextSpan, TextStyle, WrapMode};
use regia_ui::widgets::Button;
use regia_ui::{Edges, Themed, UiColor};

/// The demo's title.
pub const TITLE: &str = "Third Person Demo";

/// Which menu shows (reflected for agent checks).
#[derive(Resource, Reflect, Debug, Clone, Default, PartialEq)]
#[reflect(Resource, Default)]
pub struct MenuState {
    /// `title`, `settings`, `pause` or `none`.
    pub screen: String,
    /// The settings panel is open.
    pub settings_open: bool,
    /// Button presses handled.
    pub presses: u32,
}

/// The menu roots.
#[derive(Resource, Debug, Clone, Copy, Default)]
pub struct MenuRoots {
    /// The title panel.
    pub title: Option<Entity>,
    /// The settings panel.
    pub settings: Option<Entity>,
    /// The settings values text.
    pub values: Option<Entity>,
}

/// What a menu button does.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    /// Start playing.
    Play,
    /// Load the save, then play.
    Continue,
    /// Open the settings panel.
    OpenSettings,
    /// Close it.
    CloseSettings,
    /// Quit the app.
    Quit,
    /// Master volume up / down.
    Master(bool),
    /// Music volume up / down.
    Music(bool),
    /// Effects volume up / down.
    Effects(bool),
    /// Mouse sensitivity up / down.
    Sensitivity(bool),
    /// Toggle invert Y.
    InvertY,
    /// Next / previous graphics preset.
    Preset(bool),
}

/// Volume step of one press.
const VOLUME_STEP: f32 = 0.1;
/// Sensitivity step of one press.
const SENSITIVITY_STEP: f32 = 0.1;

/// Spawns the title panel and the settings panel under `parent` (both hidden until shown).
pub fn setup(world: &mut World, parent: Entity) {
    let title = panel(world, parent, "title-menu");
    label(world, title, TITLE, 46.0);
    label(world, title, "Collect the coins. Watch for the patrols.", 18.0);
    for (caption, name, action) in [
        ("Play", "menu-play", MenuAction::Play),
        ("Continue", "menu-continue", MenuAction::Continue),
        ("Settings", "menu-settings", MenuAction::OpenSettings),
        ("Quit", "menu-quit", MenuAction::Quit),
    ] {
        button(world, title, caption, name, action);
    }

    let settings = panel(world, parent, "settings-menu");
    label(world, settings, "Settings", 34.0);
    let values = label(world, settings, "", 18.0);
    for (caption, name, action) in [
        ("Master -", "settings-master-down", MenuAction::Master(false)),
        ("Master +", "settings-master-up", MenuAction::Master(true)),
        ("Music -", "settings-music-down", MenuAction::Music(false)),
        ("Music +", "settings-music-up", MenuAction::Music(true)),
        ("Effects -", "settings-effects-down", MenuAction::Effects(false)),
        ("Effects +", "settings-effects-up", MenuAction::Effects(true)),
        (
            "Sensitivity -",
            "settings-sensitivity-down",
            MenuAction::Sensitivity(false),
        ),
        (
            "Sensitivity +",
            "settings-sensitivity-up",
            MenuAction::Sensitivity(true),
        ),
        ("Invert Y", "settings-invert-y", MenuAction::InvertY),
        ("Graphics <", "settings-preset-prev", MenuAction::Preset(false)),
        ("Graphics >", "settings-preset-next", MenuAction::Preset(true)),
        ("Back", "settings-back", MenuAction::CloseSettings),
    ] {
        button(world, settings, caption, name, action);
    }
    world.insert_resource(MenuRoots {
        title: Some(title),
        settings: Some(settings),
        values: Some(values),
    });
    world.init_resource::<MenuState>();
}

fn panel(world: &mut World, parent: Entity, name: &str) -> Entity {
    let panel = world
        .spawn((
            UiNode,
            NodeStyle::new(regia_ui::style::Style {
                direction: FlexDirection::Column,
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                justify_content: JustifyContent::Center,
                align_items: Align::Center,
                gap: [Dimension::Points(6.0), Dimension::Points(8.0)],
                display: Display::None,
                ..Default::default()
            }),
            Background {
                kind: BackgroundKind::Color(UiColor([0.02, 0.03, 0.05, 0.78])),
            },
            Themed::default(),
            Name::new(name.to_owned()),
        ))
        .id();
    world.entity_mut(parent).add_child(panel);
    panel
}

fn label(world: &mut World, parent: Entity, text: &str, px: f32) -> Entity {
    let label = world
        .spawn((
            UiNode,
            Text {
                spans: vec![TextSpan::lit(text.to_owned()).with_style(TextStyle {
                    font_px: px,
                    ..Default::default()
                })],
                wrap: WrapMode::Word,
                ..Default::default()
            },
        ))
        .id();
    world.entity_mut(parent).add_child(label);
    label
}

fn button(world: &mut World, parent: Entity, caption: &str, name: &str, action: MenuAction) {
    let button = world
        .spawn((
            UiNode,
            NodeStyle::new(regia_ui::style::Style {
                padding: Edges::px(8.0),
                justify_content: JustifyContent::Center,
                align_items: Align::Center,
                ..Default::default()
            }),
            regia_ui::node::Interactable::default(),
            regia_ui::focus::Focusable::default(),
            Button,
            Classes(vec!["button".into()]),
            Themed::default(),
            action,
            Name::new(name.to_owned()),
        ))
        .id();
    let text = world
        .spawn((
            UiNode,
            Text {
                spans: vec![TextSpan::lit(caption.to_owned())],
                wrap: WrapMode::None,
                ..Default::default()
            },
        ))
        .id();
    world.entity_mut(button).add_child(text);
    world.entity_mut(parent).add_child(button);
}

/// Steps a volume by one press, kept in `0..=1` on the 0.1 grid.
fn step_volume(value: f32, up: bool) -> f32 {
    let next = value + if up { VOLUME_STEP } else { -VOLUME_STEP };
    (next.clamp(0.0, 1.0) * 10.0).round() / 10.0
}

/// The next (or previous) desktop preset.
fn step_preset(preset: GraphicsPreset, up: bool) -> GraphicsPreset {
    const ORDER: [GraphicsPreset; 4] = [
        GraphicsPreset::Low,
        GraphicsPreset::Medium,
        GraphicsPreset::High,
        GraphicsPreset::Ultra,
    ];
    let i = ORDER.iter().position(|p| *p == preset).unwrap_or(1);
    let next = if up {
        (i + 1).min(ORDER.len() - 1)
    } else {
        i.saturating_sub(1)
    };
    ORDER[next]
}

/// Does what `action` says (the click handler and the test entry point).
pub fn apply_action(world: &mut World, action: MenuAction) {
    if let Some(mut menu) = world.get_resource_mut::<MenuState>() {
        menu.presses += 1;
    }
    let flow = world.get_resource::<State<GameFlow>>().map(|s| *s.get());
    match action {
        MenuAction::Play => {
            if flow == Some(GameFlow::MainMenu) {
                world.resource_mut::<NextState<GameFlow>>().set(GameFlow::Loading);
            }
            set_settings_open(world, false);
        }
        MenuAction::Continue => {
            crate::save::load_now(world);
            if flow == Some(GameFlow::MainMenu) {
                world.resource_mut::<NextState<GameFlow>>().set(GameFlow::Loading);
            }
            set_settings_open(world, false);
        }
        MenuAction::OpenSettings => set_settings_open(world, true),
        MenuAction::CloseSettings => set_settings_open(world, false),
        MenuAction::Quit => {
            world.write_message(AppExit::Success);
        }
        MenuAction::Master(up) => edit_settings(world, |s| s.audio.master = step_volume(s.audio.master, up)),
        MenuAction::Music(up) => edit_settings(world, |s| s.audio.music = step_volume(s.audio.music, up)),
        MenuAction::Effects(up) => edit_settings(world, |s| s.audio.sfx = step_volume(s.audio.sfx, up)),
        MenuAction::Sensitivity(up) => edit_settings(world, |s| {
            let next = s.input.sensitivity + if up { SENSITIVITY_STEP } else { -SENSITIVITY_STEP };
            s.input.sensitivity = (next.clamp(0.1, 3.0) * 10.0).round() / 10.0;
        }),
        MenuAction::InvertY => edit_settings(world, |s| s.input.invert_y = !s.input.invert_y),
        MenuAction::Preset(up) => edit_settings(world, |s| {
            let next = step_preset(s.graphics.preset, up);
            s.graphics.choose_preset(next);
        }),
    }
}

fn set_settings_open(world: &mut World, open: bool) {
    if let Some(mut menu) = world.get_resource_mut::<MenuState>() {
        menu.settings_open = open;
    }
}

fn edit_settings(world: &mut World, edit: impl FnOnce(&mut GameSettings)) {
    if !world.contains_resource::<GameSettings>() {
        world.init_resource::<GameSettings>();
    }
    let mut settings = world.resource_mut::<GameSettings>();
    edit(&mut settings);
}

/// `Update`: menu button clicks.
#[tracing::instrument(skip_all, target = "third_person", name = "menu_clicks")]
pub fn menu_clicks(
    world: &mut World,
    mut cursor: Local<regia_ecs::ecs::message::MessageCursor<regia_ui::interaction::UiEvent>>,
) {
    let events: Vec<regia_ui::interaction::UiEvent> = world
        .get_resource::<Messages<regia_ui::interaction::UiEvent>>()
        .map(|m| cursor.read(m).cloned().collect())
        .unwrap_or_default();
    for event in events {
        if !matches!(event.kind, regia_ui::interaction::UiEventKind::Click) {
            continue;
        }
        if let Some(action) = world.get::<MenuAction>(event.target).copied() {
            apply_action(world, action);
        }
    }
}

/// `Update`: shows the title in `MainMenu`, the settings panel while open, and writes the
/// current settings into the panel.
#[tracing::instrument(skip_all, target = "third_person", name = "menu_sync")]
pub fn sync_menus(world: &mut World, mut last: Local<String>) {
    let flow = world.get_resource::<State<GameFlow>>().map(|s| *s.get());
    let settings_open = world.get_resource::<MenuState>().is_some_and(|m| m.settings_open);
    let roots = world.get_resource::<MenuRoots>().copied().unwrap_or_default();
    let on_title = flow == Some(GameFlow::MainMenu);
    let screen = if settings_open {
        "settings"
    } else if on_title {
        "title"
    } else if flow == Some(GameFlow::Paused) {
        "pause"
    } else {
        "none"
    };
    if let Some(mut menu) = world.get_resource_mut::<MenuState>()
        && menu.screen != screen
    {
        screen.clone_into(&mut menu.screen);
    }
    for (root, shown) in [
        (roots.title, on_title && !settings_open),
        (roots.settings, settings_open),
    ] {
        if let Some(root) = root
            && let Some(mut style) = world.get_mut::<NodeStyle>(root)
        {
            let display = if shown { Display::Flex } else { Display::None };
            if style.0.display != display {
                style.0.display = display;
            }
        }
    }
    if settings_open
        && let Some(values) = roots.values
        && let Some(settings) = world.get_resource::<GameSettings>()
    {
        let line = format!(
            "Master {:.0}%   Music {:.0}%   Effects {:.0}%\nMouse sensitivity {:.1}   Invert Y {}   Graphics {:?}",
            settings.audio.master * 100.0,
            settings.audio.music * 100.0,
            settings.audio.sfx * 100.0,
            settings.input.sensitivity,
            if settings.input.invert_y { "on" } else { "off" },
            settings.graphics.preset,
        );
        if *last != line
            && let Some(mut text) = world.get_mut::<Text>(values)
        {
            text.spans = vec![TextSpan::lit(line.clone())];
            *last = line;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_steps_stay_on_the_grid_and_in_range() {
        assert_eq!(step_volume(1.0, true), 1.0);
        assert_eq!(step_volume(0.0, false), 0.0);
        assert_eq!(step_volume(0.7, false), 0.6);
        assert_eq!(step_volume(0.62, true), 0.7);
    }

    #[test]
    fn presets_step_and_clamp() {
        assert_eq!(step_preset(GraphicsPreset::Medium, true), GraphicsPreset::High);
        assert_eq!(step_preset(GraphicsPreset::Ultra, true), GraphicsPreset::Ultra);
        assert_eq!(step_preset(GraphicsPreset::Low, false), GraphicsPreset::Low);
        // A non-desktop preset steps from Medium.
        assert_eq!(step_preset(GraphicsPreset::Deck, false), GraphicsPreset::Low);
    }
}
