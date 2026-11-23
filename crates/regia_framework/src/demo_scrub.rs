// SPDX-License-Identifier: MPL-2.0
//! The demo scrub bar (`book/agents/demo.md`, ADR-1878): a runtime-UI seek bar for the
//! [`DemoDriver`] of [`crate::demo_world`].
//!
//! A strip along the bottom of the screen holds a play/pause button, a [`Slider`] spanning the
//! demo's steps with a tick mark under every keyframe, and a label with the time, the tick and the
//! mode. Dragging or clicking the slider requests a seek (several drag events in one frame collapse
//! to the last); the button pauses, resumes, or restarts a finished demo. While nobody drags, the
//! slider follows the playhead.
//!
//! [`DemoScrubPlugin`] adds [`DemoScrubBar`]; set [`DemoScrubBar::open`] (or call the agent method
//! `demo.scrub_bar`) to show it. The tick/fraction maths is [`tick_at_fraction`] and
//! [`fraction_of_tick`], usable by a game drawing its own bar.

use regia_app::prelude::*;
use regia_ecs::ecs::message::MessageCursor;
use regia_ui::interaction::{UiEvent, UiEventKind, UiValue};
use regia_ui::node::{Background, BackgroundKind, Interactable, NodeStyle, UiNode, UiRoot};
use regia_ui::style::{Align, Dimension, Edges, FlexDirection, JustifyContent, PositionType, Style};
use regia_ui::text::{Text, TextSpan, WrapMode};
use regia_ui::widgets::{Button, Slider};
use regia_ui::{Classes, ScaleMode, Themed, UiColor};

use crate::demo_world::{DemoDriver, DemoMode, DemoRequest, DemoStatus, demo_status};

/// Height of the bar in UI pixels.
pub const SCRUB_BAR_HEIGHT: f32 = 48.0;

/// The fraction `0..=1` of the bar at `tick` of a `steps`-step demo (0 for an empty demo).
#[must_use]
pub fn fraction_of_tick(tick: u64, steps: u64) -> f32 {
    if steps == 0 {
        0.0
    } else {
        (tick.min(steps) as f64 / steps as f64) as f32
    }
}

/// The tick at `fraction` of the bar (rounded to the nearest tick, clamped to `0..=steps`; a
/// non-finite fraction is 0).
#[must_use]
pub fn tick_at_fraction(fraction: f32, steps: u64) -> u64 {
    if !fraction.is_finite() {
        return 0;
    }
    let tick = (f64::from(fraction.clamp(0.0, 1.0)) * steps as f64).round();
    (tick as u64).min(steps)
}

/// `m:ss.cc` for a tick at `rate_hz` (`--` for a zero rate).
#[must_use]
pub fn format_tick_time(tick: u64, rate_hz: u32) -> String {
    if rate_hz == 0 {
        return "--".to_string();
    }
    let centis = tick.saturating_mul(100) / u64::from(rate_hz);
    format!("{}:{:02}.{:02}", centis / 6000, (centis / 100) % 60, centis % 100)
}

/// The bar's label for a status: `time / total  tick t/n  mode`.
#[must_use]
pub fn scrub_label(status: &DemoStatus) -> String {
    format!(
        "{} / {}   tick {}/{}   {}",
        format_tick_time(status.tick, status.tick_rate_hz),
        format_tick_time(status.steps, status.tick_rate_hz),
        status.tick,
        status.steps,
        status.mode.name()
    )
}

/// The scrub bar's state. Set `open` to show or hide it; the entities are managed by the plugin.
#[derive(Resource, Debug, Default)]
pub struct DemoScrubBar {
    /// Whether the bar is shown.
    pub open: bool,
    nodes: Option<ScrubNodes>,
    cursor: MessageCursor<UiEvent>,
    /// Keyframe ticks the markers were built for.
    marker_ticks: Vec<u64>,
    marker_steps: u64,
}

impl DemoScrubBar {
    /// The bar's slider entity while it is shown.
    #[must_use]
    pub fn slider(&self) -> Option<Entity> {
        self.nodes.map(|nodes| nodes.slider)
    }

    /// The play/pause button entity while the bar is shown.
    #[must_use]
    pub fn button(&self) -> Option<Entity> {
        self.nodes.map(|nodes| nodes.button)
    }

    /// The label entity while the bar is shown.
    #[must_use]
    pub fn label(&self) -> Option<Entity> {
        self.nodes.map(|nodes| nodes.label)
    }

    /// Keyframe markers currently drawn.
    #[must_use]
    pub fn marker_count(&self) -> usize {
        self.marker_ticks.len()
    }
}

#[derive(Debug, Clone, Copy)]
struct ScrubNodes {
    root: Entity,
    track: Entity,
    slider: Entity,
    button: Entity,
    button_text: Entity,
    label: Entity,
}

/// Marks keyframe tick marks (rebuilt when the keyframe list changes).
#[derive(Component, Debug, Clone, Copy)]
struct ScrubMarker;

/// Adds [`DemoScrubBar`] and its system (`Update`). Needs the runtime UI and
/// [`crate::demo_world::DemoWorldPlugin`].
pub struct DemoScrubPlugin;

impl Plugin for DemoScrubPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<DemoScrubBar>()
            .add_systems(Update, drive_scrub_bar);
    }
}

fn text_node(world: &mut World, content: &str) -> Entity {
    world
        .spawn((
            UiNode,
            Text {
                spans: vec![TextSpan::lit(content.to_string())],
                wrap: WrapMode::None,
                ..Default::default()
            },
        ))
        .id()
}

fn set_text(world: &mut World, entity: Entity, content: &str) {
    if let Some(mut text) = world.get_mut::<Text>(entity) {
        let unchanged =
            matches!(text.spans.as_slice(), [span] if span == &TextSpan::lit(content.to_string()));
        if !unchanged {
            text.spans = vec![TextSpan::lit(content.to_string())];
        }
    }
}

fn spawn_bar(world: &mut World) -> ScrubNodes {
    let root = world
        .spawn((
            UiRoot {
                scale_mode: ScaleMode::ConstantPixelSize,
                sort_order: 900,
                ..Default::default()
            },
            UiNode,
            NodeStyle::new(Style {
                direction: FlexDirection::Column,
                justify_content: JustifyContent::End,
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                ..Default::default()
            }),
        ))
        .id();
    let strip = world
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                direction: FlexDirection::Row,
                align_items: Align::Center,
                gap: [Dimension::Points(12.0), Dimension::Points(12.0)],
                padding: Edges::px(8.0),
                size: [Dimension::Percent(100.0), Dimension::Points(SCRUB_BAR_HEIGHT)],
                ..Default::default()
            }),
            Background {
                kind: BackgroundKind::Color(UiColor::linear(0.0, 0.0, 0.0, 0.6)),
            },
        ))
        .id();
    let button_text = text_node(world, "Pause");
    let button = world
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                padding: Edges::px(6.0),
                justify_content: JustifyContent::Center,
                align_items: Align::Center,
                size: [Dimension::Points(72.0), Dimension::Auto],
                ..Default::default()
            }),
            Interactable::default(),
            Button,
            Classes(vec!["button".to_string()]),
            Themed::default(),
        ))
        .id();
    world.entity_mut(button).add_child(button_text);
    // The track holds the slider and, positioned over it, one tick mark per keyframe.
    let track = world
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                flex_grow: 1.0,
                size: [Dimension::Auto, Dimension::Points(28.0)],
                ..Default::default()
            }),
        ))
        .id();
    let slider = world
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                ..Default::default()
            }),
            Interactable::default(),
            Slider {
                value: 0.0,
                min: 0.0,
                max: 1.0,
                step: 1.0,
            },
            Classes(vec!["slider-track".to_string()]),
            Themed::default(),
        ))
        .id();
    world.entity_mut(track).add_child(slider);
    let label = text_node(world, "");
    world.entity_mut(label).insert(NodeStyle::new(Style {
        size: [Dimension::Points(320.0), Dimension::Auto],
        ..Default::default()
    }));
    world.entity_mut(strip).add_children(&[button, track, label]);
    world.entity_mut(root).add_child(strip);
    ScrubNodes {
        root,
        track,
        slider,
        button,
        button_text,
        label,
    }
}

fn rebuild_markers(world: &mut World, track: Entity, keyframes: &[u64], steps: u64) {
    let old: Vec<Entity> = world
        .query_filtered::<Entity, With<ScrubMarker>>()
        .iter(world)
        .collect();
    for marker in old {
        world.despawn(marker);
    }
    let markers: Vec<Entity> = keyframes
        .iter()
        .map(|&tick| {
            world
                .spawn((
                    UiNode,
                    ScrubMarker,
                    NodeStyle::new(Style {
                        position: PositionType::Absolute,
                        inset: Edges {
                            left: Dimension::Percent(fraction_of_tick(tick, steps) * 100.0),
                            top: Dimension::Points(0.0),
                            right: Dimension::Auto,
                            bottom: Dimension::Auto,
                        },
                        size: [Dimension::Points(2.0), Dimension::Points(6.0)],
                        ..Default::default()
                    }),
                    Background {
                        kind: BackgroundKind::Color(UiColor::linear(1.0, 0.8, 0.2, 0.9)),
                    },
                ))
                .id()
        })
        .collect();
    world.entity_mut(track).add_children(&markers);
}

/// `Update`: opens/closes the bar, turns slider and button events into driver requests, and
/// syncs the slider, markers and label with the driver.
#[tracing::instrument(skip_all, target = "regia_framework", name = "demo_scrub_bar")]
fn drive_scrub_bar(world: &mut World) {
    let Some(mut bar) = world.remove_resource::<DemoScrubBar>() else {
        return;
    };
    let events: Vec<UiEvent> = world
        .get_resource::<Messages<UiEvent>>()
        .map(|messages| bar.cursor.read(messages).cloned().collect())
        .unwrap_or_default();
    match (bar.open, bar.nodes) {
        (true, None) => bar.nodes = Some(spawn_bar(world)),
        (false, Some(nodes)) => {
            world.despawn(nodes.root);
            bar.nodes = None;
            bar.marker_ticks.clear();
        }
        _ => {}
    }
    if let Some(nodes) = bar.nodes {
        sync_bar(world, &mut bar, nodes, &events);
    }
    world.insert_resource(bar);
}

fn sync_bar(world: &mut World, bar: &mut DemoScrubBar, nodes: ScrubNodes, events: &[UiEvent]) {
    let Some(status) = demo_status(world) else {
        set_text(world, nodes.label, "no demo driver");
        return;
    };
    let mut seek = None;
    let mut toggle = false;
    for event in events {
        match &event.kind {
            UiEventKind::ValueChanged(UiValue::Number(value)) if event.target == nodes.slider => {
                seek = Some(tick_at_fraction(
                    fraction_of_tick_value(*value, status.steps),
                    status.steps,
                ));
            }
            UiEventKind::Click if event.target == nodes.button || event.target == nodes.button_text => {
                toggle = true;
            }
            _ => {}
        }
    }
    let loaded = !matches!(status.mode, DemoMode::Idle | DemoMode::Recording);
    if let Some(mut driver) = world.get_resource_mut::<DemoDriver>()
        && loaded
    {
        if let Some(tick) = seek {
            driver.request(DemoRequest::Seek(tick));
        }
        if toggle {
            match status.mode {
                DemoMode::Playing => driver.request(DemoRequest::Pause(true)),
                DemoMode::Finished => {
                    driver.request(DemoRequest::Seek(0));
                    driver.request(DemoRequest::Pause(false));
                }
                _ => driver.request(DemoRequest::Pause(false)),
            }
        }
    }
    if let Some(mut slider) = world.get_mut::<Slider>(nodes.slider) {
        let max = status.steps.max(1) as f32;
        if slider.max != max {
            slider.max = max;
        }
        // A seek requested this frame owns the slider until the driver applies it.
        let value = seek.unwrap_or(status.tick) as f32;
        if slider.value != value {
            slider.value = value;
        }
    }
    if bar.marker_ticks != status.keyframe_ticks || bar.marker_steps != status.steps {
        rebuild_markers(world, nodes.track, &status.keyframe_ticks, status.steps);
        bar.marker_ticks = status.keyframe_ticks.clone();
        bar.marker_steps = status.steps;
    }
    let button = if status.mode == DemoMode::Playing {
        "Pause"
    } else {
        "Play"
    };
    set_text(world, nodes.button_text, button);
    set_text(world, nodes.label, &scrub_label(&status));
}

/// The slider's value is a tick (`0..=steps`); as a fraction of the bar.
fn fraction_of_tick_value(value: f32, steps: u64) -> f32 {
    if steps == 0 || !value.is_finite() {
        0.0
    } else {
        (f64::from(value) / steps as f64) as f32
    }
}

#[cfg(test)]
mod tests {
    use regia_input::{ActionKind, InputConfig, InputPlugin};

    use super::*;
    use crate::demo::DemoHeader;
    use crate::demo_world::{DemoTracked, DemoWorldPlugin};

    #[test]
    fn ticks_and_fractions_round_trip() {
        assert_eq!(fraction_of_tick(0, 0), 0.0);
        assert_eq!(fraction_of_tick(50, 100), 0.5);
        assert_eq!(fraction_of_tick(150, 100), 1.0, "clamped");
        for steps in [1u64, 7, 600, 36_000] {
            for tick in [0, steps / 3, steps / 2, steps] {
                assert_eq!(tick_at_fraction(fraction_of_tick(tick, steps), steps), tick);
            }
        }
        assert_eq!(tick_at_fraction(-1.0, 10), 0);
        assert_eq!(tick_at_fraction(2.0, 10), 10);
        assert_eq!(tick_at_fraction(f32::NAN, 10), 0);
        assert_eq!(tick_at_fraction(0.5, 0), 0);
    }

    #[test]
    fn time_labels_use_the_tick_rate() {
        assert_eq!(format_tick_time(0, 60), "0:00.00");
        assert_eq!(format_tick_time(90, 60), "0:01.50");
        assert_eq!(format_tick_time(60 * 75, 60), "1:15.00");
        assert_eq!(format_tick_time(5, 0), "--");
        let status = DemoStatus {
            mode: DemoMode::Paused,
            tick: 30,
            steps: 120,
            tick_rate_hz: 60,
            keyframe_ticks: vec![0, 60, 120],
        };
        assert_eq!(scrub_label(&status), "0:00.50 / 0:02.00   tick 30/120   paused");
    }

    #[derive(Component, Reflect, Default, Clone)]
    #[reflect(Component, Default)]
    struct Counter(u32);

    fn count(mut counters: Query<&mut Counter>) {
        for mut counter in &mut counters {
            counter.0 += 1;
        }
    }

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins).add_plugins((
            InputPlugin::new(InputConfig::new().action("Move", ActionKind::Axis1D)),
            DemoWorldPlugin,
            DemoScrubPlugin,
        ));
        app.add_message::<UiEvent>();
        app.register_type::<Counter>();
        app.add_systems(FixedUpdate, count);
        enable_fixed_step(&mut app, 60.0);
        app.world_mut().spawn((DemoTracked, Counter(0)));
        app
    }

    fn recorded(app: &mut App, steps: u32) {
        app.world_mut()
            .resource_mut::<DemoDriver>()
            .request(DemoRequest::Record {
                header: DemoHeader::new("bar", 0, 60, 10),
                actions: None,
            });
        // The first frame starts the recording; the next `steps` frames record one step each, and
        // the stop (applied after the last frame's step) is queued with the last of them.
        for frame in 0..=steps {
            if frame == steps {
                app.world_mut()
                    .resource_mut::<DemoDriver>()
                    .request(DemoRequest::StopRecording { path: None });
            }
            app.update();
        }
        let demo = app
            .world()
            .resource::<DemoDriver>()
            .recording()
            .cloned()
            .expect("demo");
        app.world_mut()
            .resource_mut::<DemoDriver>()
            .request(DemoRequest::Play(Box::new(demo)));
        app.update();
    }

    fn counter(app: &mut App) -> u32 {
        let world = app.world_mut();
        world.query::<&Counter>().iter(world).map(|c| c.0).sum()
    }

    #[test]
    fn the_bar_opens_follows_the_playhead_and_closes() {
        let mut app = app();
        app.update();
        assert!(
            app.world().resource::<DemoScrubBar>().slider().is_none(),
            "closed by default"
        );
        recorded(&mut app, 30);
        app.world_mut().resource_mut::<DemoScrubBar>().open = true;
        app.update();
        let bar = app.world().resource::<DemoScrubBar>();
        let slider = bar.slider().expect("open");
        assert_eq!(bar.marker_count(), 4, "keyframes 0, 10, 20, 30");
        let label = bar.label().expect("label");
        let value = app.world().get::<Slider>(slider).map(|s| (s.value, s.max));
        let tick = demo_status(app.world()).expect("status").tick;
        assert_eq!(value, Some((tick as f32, 30.0)));
        let text = app.world().get::<Text>(label).expect("text");
        assert!(
            matches!(text.spans.as_slice(), [span] if span == &TextSpan::lit(scrub_label(&demo_status(app.world()).expect("status")))),
            "the label shows the status"
        );
        app.world_mut().resource_mut::<DemoScrubBar>().open = false;
        app.update();
        assert!(
            app.world().get_entity(slider).is_err(),
            "closing despawns the bar"
        );
    }

    #[test]
    fn dragging_the_slider_seeks_and_the_button_toggles_pause() {
        let mut app = app();
        recorded(&mut app, 30);
        let straight_at_12 = {
            // Counter value at tick 12 of the replay: the recording started after one frame.
            let start = demo_status(app.world()).expect("status");
            assert_eq!(start.tick, 0);
            counter(&mut app)
        } + 12;
        app.world_mut().resource_mut::<DemoScrubBar>().open = true;
        app.update();
        let slider = app.world().resource::<DemoScrubBar>().slider().expect("slider");
        let button = app.world().resource::<DemoScrubBar>().button().expect("button");
        // Pause first so the playhead stays put after the seek.
        app.world_mut()
            .write_message(UiEvent::new(button, UiEventKind::Click));
        app.update();
        app.update();
        assert_eq!(demo_status(app.world()).expect("status").mode, DemoMode::Paused);
        app.world_mut().write_message(UiEvent::new(
            slider,
            UiEventKind::ValueChanged(UiValue::Number(12.0)),
        ));
        app.update();
        app.update();
        let status = demo_status(app.world()).expect("status");
        assert_eq!((status.mode, status.tick), (DemoMode::Paused, 12));
        assert_eq!(counter(&mut app), straight_at_12);
        app.world_mut()
            .write_message(UiEvent::new(button, UiEventKind::Click));
        app.update();
        app.update();
        assert_eq!(demo_status(app.world()).expect("status").mode, DemoMode::Playing);
    }
}
