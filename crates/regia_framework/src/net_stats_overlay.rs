// SPDX-License-Identifier: MPL-2.0
//! The in-game network stats overlay (spec 77 § 10.2, ADR-2413), features `net` + `client`.
//!
//! A small panel in the top-right corner, drawn with the runtime UI (`regia_ui` text and nodes),
//! listing the session role and, per remote player (server) or for the link to the server (client),
//! the state, ping, jitter and loss from [`NetPlayerStats`]. Nothing is measured here: the numbers
//! are the transport's own estimates, refreshed by the session at 4 Hz, and the text is rebuilt only
//! when they refresh.
//!
//! **Off by default.** [`NetStatsOverlay::visible`] switches it; agents use `net.stats_overlay`,
//! games bind a key to [`NetStatsOverlay::toggle`]. Hidden, the plugin does one resource read per
//! frame and spawns nothing.

use regia_app::prelude::*;
use regia_net::session::{NetPlayerStats, NetStatsOverlay};
use regia_ui::node::{Background, BackgroundKind, NodeStyle, UiNode, UiRoot};
use regia_ui::style::{Align, Dimension, Display, Edges, FlexDirection, Style};
use regia_ui::text::{Text, TextAlign, TextSpan, TextStyle, WrapMode};
use regia_ui::{ScaleMode, UiColor};

const PANEL_BG: UiColor = UiColor::linear(0.0, 0.0, 0.0, 0.62);
const TEXT: UiColor = UiColor::linear(0.85, 0.85, 0.85, 1.0);
const FONT_PX: f32 = 13.0;

/// The overlay's entities and the refresh it last drew.
#[derive(Resource, Debug, Default)]
pub struct NetStatsOverlayState {
    root: Option<Entity>,
    text: Option<Entity>,
    drawn_at: Option<Option<u64>>,
    /// Lines drawn last.
    pub lines: Vec<String>,
}

impl NetStatsOverlayState {
    /// The overlay's root UI entity once it was shown.
    pub fn root(&self) -> Option<Entity> {
        self.root
    }

    /// The text entity once it was shown.
    pub fn text(&self) -> Option<Entity> {
        self.text
    }
}

/// Draws the overlay (added by [`NetSessionBridgePlugin`](crate::net_session_bridge::NetSessionBridgePlugin)
/// with the `client` feature).
#[derive(Debug, Default, Clone, Copy)]
pub struct NetStatsOverlayPlugin;

impl Plugin for NetStatsOverlayPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<NetStatsOverlay>()
            .init_resource::<NetStatsOverlayState>()
            .add_systems(
                PostUpdate,
                update_net_stats_overlay.before(PostUpdateSet::UiLayout),
            );
    }
}

/// The overlay's text lines for the world as it is (the same lines `net.stats_overlay` returns).
pub fn overlay_lines(world: &World) -> Vec<String> {
    regia_net::session::players::overlay_lines(world)
}

fn spawn(world: &mut World, state: &mut NetStatsOverlayState) {
    let root = world
        .spawn((
            UiRoot {
                scale_mode: ScaleMode::ConstantPhysicalSize,
                // Over the game UI, under the performance overlay.
                sort_order: i32::MAX - 2,
                ..Default::default()
            },
            UiNode,
            NodeStyle::new(Style {
                align_items: Align::End,
                direction: FlexDirection::Column,
                ..Default::default()
            }),
        ))
        .id();
    let panel = world
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                margin: Edges::px(10.0),
                padding: Edges::px(8.0),
                size: [Dimension::Auto, Dimension::Auto],
                align_self: Align::End,
                ..Default::default()
            }),
            Background {
                kind: BackgroundKind::Color(PANEL_BG),
            },
        ))
        .id();
    let text = world
        .spawn((
            UiNode,
            NodeStyle::new(Style::default()),
            Text {
                spans: vec![TextSpan {
                    style: TextStyle {
                        font_px: FONT_PX,
                        color: TEXT,
                        ..Default::default()
                    },
                    ..TextSpan::lit(" ")
                }],
                align: TextAlign::Start,
                wrap: WrapMode::None,
                ..Default::default()
            },
        ))
        .id();
    world.entity_mut(panel).add_child(text);
    world.entity_mut(root).add_child(panel);
    state.root = Some(root);
    state.text = Some(text);
    state.lines.clear();
}

fn set_display(world: &mut World, entity: Option<Entity>, display: Display) {
    if let Some(e) = entity
        && let Some(mut style) = world.get_mut::<NodeStyle>(e)
        && style.0.display != display
    {
        style.0.display = display;
    }
}

/// `PostUpdate`, before UI layout: shows, refreshes or hides the overlay.
pub fn update_net_stats_overlay(world: &mut World) {
    let visible = world.get_resource::<NetStatsOverlay>().is_some_and(|o| o.visible);
    let Some(mut state) = world.remove_resource::<NetStatsOverlayState>() else {
        return;
    };
    if !visible {
        if state.drawn_at.is_some() {
            set_display(world, state.root, Display::None);
            state.drawn_at = None;
        }
        world.insert_resource(state);
        return;
    }
    let _span = tracing::trace_span!(target: "regia_framework", "net_stats_overlay").entered();
    if state.root.is_none_or(|r| world.get_entity(r).is_err()) {
        spawn(world, &mut state);
        state.drawn_at = None;
    }
    let refreshed = world
        .get_resource::<NetPlayerStats>()
        .and_then(|s| s.refreshed_ms);
    if state.drawn_at != Some(refreshed) {
        set_display(world, state.root, Display::Flex);
        let lines = overlay_lines(world);
        if lines != state.lines
            && let Some(e) = state.text
            && let Some(mut text) = world.get_mut::<Text>(e)
        {
            text.spans = vec![TextSpan {
                style: TextStyle {
                    font_px: FONT_PX,
                    color: TEXT,
                    ..Default::default()
                },
                ..TextSpan::lit(lines.join("\n"))
            }];
        }
        state.lines = lines;
        state.drawn_at = Some(refreshed);
    }
    world.insert_resource(state);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_by_default_and_toggled_by_the_switch() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(NetStatsOverlayPlugin);
        app.update();
        assert!(!app.world().resource::<NetStatsOverlay>().visible);
        assert!(app.world().resource::<NetStatsOverlayState>().root().is_none());
        app.world_mut().resource_mut::<NetStatsOverlay>().toggle();
        app.update();
        let state = app.world().resource::<NetStatsOverlayState>();
        let root = state.root().expect("spawned when shown");
        assert_eq!(state.lines, vec!["net: no session".to_owned()]);
        app.world_mut().resource_mut::<NetStatsOverlay>().toggle();
        app.update();
        assert_eq!(
            app.world().get::<NodeStyle>(root).map(|s| s.0.display),
            Some(Display::None)
        );
    }
}
