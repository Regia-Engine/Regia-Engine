// SPDX-License-Identifier: MPL-2.0
//! The standalone timeline widget driven by synthetic egui input across frames: scrub the
//! ruler, drag a key, and read the events it returned.

use egui::{Event, Modifiers, PointerButton, Pos2, Rect, vec2};
use regia_editor::anim::timeline::{
    MARKER_HEIGHT, MarkerKind, RULER_HEIGHT, TOOLBAR_HEIGHT, TRACK_HEIGHT, Timeline, TimelineEvent,
    TimelineMarker, TimelineTrack,
};

struct Harness {
    ctx: egui::Context,
    time: f64,
}

impl Harness {
    fn new() -> Self {
        Self {
            ctx: egui::Context::default(),
            time: 0.0,
        }
    }

    fn frame(&mut self, timeline: &mut Timeline, events: Vec<Event>) -> Vec<TimelineEvent> {
        self.time += 0.05;
        let raw = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(700.0, 240.0))),
            events,
            time: Some(self.time),
            ..Default::default()
        };
        let mut got = Vec::new();
        let mut output = self.ctx.run_ui(raw, |ui| {
            egui::CentralPanel::default().show(ui, |ui| got = timeline.show(ui));
        });
        output.textures_delta.clear();
        got
    }
}

fn button(pos: Pos2, pressed: bool) -> Event {
    Event::PointerButton {
        pos,
        button: PointerButton::Primary,
        pressed,
        modifiers: Modifiers::NONE,
    }
}

#[test]
fn scrub_then_drag_a_key_through_egui_frames() {
    let mut harness = Harness::new();
    let mut timeline = Timeline::new(4.0);
    timeline.tracks.push(TimelineTrack::new("hips", vec![1.0, 3.0]));
    timeline
        .markers
        .push(TimelineMarker::new(2.0, "Footstep_L", MarkerKind::FootContact));
    harness.frame(&mut timeline, vec![]);
    harness.frame(&mut timeline, vec![]);
    let layout = timeline.layout().expect("drawn");
    assert_eq!(layout.rect.height(), timeline.height());

    // Scrub: press on the ruler at t = 2.5, drag to 3.5.
    let ruler = layout.rect.top() + TOOLBAR_HEIGHT + RULER_HEIGHT * 0.5;
    let view = timeline.clone();
    let at = |t: f32, y: f32| Pos2::new(view.x_of(&layout, t), y);
    let (p1, p2) = (at(2.5, ruler), at(3.5, ruler));
    harness.frame(&mut timeline, vec![Event::PointerMoved(p1)]);
    let down = harness.frame(&mut timeline, vec![button(p1, true)]);
    assert!(
        down.iter()
            .any(|e| matches!(e, TimelineEvent::Scrub(t) if (t - 2.5).abs() < 0.02)),
        "{down:?}"
    );
    let drag = harness.frame(&mut timeline, vec![Event::PointerMoved(p2)]);
    assert!(
        drag.iter()
            .any(|e| matches!(e, TimelineEvent::Scrub(t) if (t - 3.5).abs() < 0.02)),
        "{drag:?}"
    );
    harness.frame(&mut timeline, vec![button(p2, false)]);
    assert!((timeline.playhead - 3.5).abs() < 0.02);

    // Drag the first key from t = 1 to t = 1.5.
    let row = layout.rect.top() + TOOLBAR_HEIGHT + RULER_HEIGHT + MARKER_HEIGHT + TRACK_HEIGHT * 0.5;
    let (k1, k2) = (at(1.0, row), at(1.5, row));
    harness.frame(&mut timeline, vec![Event::PointerMoved(k1)]);
    harness.frame(&mut timeline, vec![button(k1, true)]);
    harness.frame(&mut timeline, vec![Event::PointerMoved(k2)]);
    let up = harness.frame(&mut timeline, vec![button(k2, false)]);
    let moved: Vec<_> = up
        .iter()
        .filter(|e| matches!(e, TimelineEvent::KeyMoved { finished: true, .. }))
        .collect();
    assert_eq!(moved.len(), 1, "{up:?}");
    assert!(
        (timeline.tracks[0].keys[0] - 1.5).abs() < 0.02,
        "{:?}",
        timeline.tracks[0].keys
    );
    assert_eq!(timeline.tracks[0].keys[1], 3.0);
}
