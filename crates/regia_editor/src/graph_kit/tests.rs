// SPDX-License-Identifier: MPL-2.0
//! The graph toolkit's own suite (P-186): each shared piece on its own, headless, with the
//! keyboard path of the search list.

use egui::accesskit::Role;
use egui::{Key, Modifiers, pos2, vec2};
use egui_snarl::ui::{PinInfo, SnarlPin, SnarlViewer, SnarlWidget};
use egui_snarl::{InPin, OutPin, Snarl};

use super::badges::{self, Severity};
use super::search::{self, SearchList, fuzzy_score, rank};
use super::{canvas, frames, preview, reroute};
use crate::canvas_keys::test_input::{focused_name, frame, key, labels_of, tab_to, text};

const SIZE: [f32; 2] = [800.0, 600.0];

const KINDS: [&str; 5] = ["Add", "Multiply", "Lerp", "Sample", "Saturate"];

const LIST: SearchList<'static> = SearchList {
    hint: "search nodes…",
    spoken: "Search nodes",
    max_hits: 12,
    max_height: 240.0,
    empty: Some("no kind matches"),
    width: None,
};

fn kinds(query: &str) -> Vec<&'static str> {
    rank(KINDS, query, |_| true, |k| vec![(*k).to_string()])
}

/// One frame of the search list in a central panel; a pick is pushed to `picked`.
fn list_frame(
    ctx: &egui::Context,
    events: Vec<egui::Event>,
    query: &mut String,
    picked: &mut Vec<&'static str>,
) -> egui::accesskit::TreeUpdate {
    frame(ctx, SIZE, events, |ui| {
        if let Some(hit) = LIST.show(ui, query, kinds, |k| (*k).to_string()) {
            picked.push(hit);
        }
    })
}

#[test]
fn fuzzy_score_prefers_prefixes_and_runs_and_refuses_non_subsequences() {
    assert_eq!(fuzzy_score("", "Anything"), Some(0));
    assert_eq!(fuzzy_score("xyz", "Multiply"), None);
    let prefix = fuzzy_score("sa", "Sample").expect("subsequence");
    let inner = fuzzy_score("sa", "Saturate").expect("subsequence");
    let gap = fuzzy_score("sa", "Subtract Add").expect("subsequence");
    assert!(prefix > gap, "a run beats letters far apart: {prefix} vs {gap}");
    assert!(prefix > inner, "the shorter name wins a tie: {prefix} vs {inner}");
    assert!(
        fuzzy_score("Add", "Add") > fuzzy_score("Add", "AddScaled"),
        "an exact match wins"
    );
}

#[test]
fn rank_orders_best_first_keeps_ties_in_order_and_applies_the_pin_filter() {
    assert_eq!(kinds(""), KINDS.to_vec(), "an empty query keeps the given order");
    assert_eq!(kinds("mul"), vec!["Multiply"]);
    let s = kinds("sa");
    assert_eq!(s.first(), Some(&"Sample"));
    assert!(s.contains(&"Saturate"));
    // A dragged pin that only some kinds can take.
    let wired = rank(KINDS, "", |k| *k != "Sample", |k| vec![(*k).to_string()]);
    assert!(!wired.contains(&"Sample"), "{wired:?}");
    // The best of several texts counts ("group label").
    let grouped = rank(
        [("Events", "On start"), ("Actions", "Spawn")],
        "actsp",
        |_| true,
        |(g, l)| vec![(*l).to_string(), format!("{g} {l}")],
    );
    assert_eq!(grouped, vec![("Actions", "Spawn")]);
}

#[test]
fn the_search_list_takes_the_focus_when_it_opens_and_enter_picks_the_best_hit() {
    let ctx = egui::Context::default();
    let (mut query, mut picked) = (String::new(), Vec::new());
    list_frame(&ctx, vec![], &mut query, &mut picked);
    let update = list_frame(&ctx, vec![], &mut query, &mut picked);
    assert_eq!(focused_name(&update).as_deref(), Some("Search nodes"));
    list_frame(&ctx, vec![text("mul")], &mut query, &mut picked);
    assert_eq!(query, "mul");
    list_frame(
        &ctx,
        vec![key(Key::Enter, Modifiers::NONE)],
        &mut query,
        &mut picked,
    );
    assert_eq!(picked, vec!["Multiply"]);
    assert!(query.is_empty(), "a pick clears the query");
}

#[test]
fn tab_leaves_the_search_field_and_a_focused_hit_is_picked_from_the_keyboard() {
    let ctx = egui::Context::default();
    let (mut query, mut picked) = (String::new(), Vec::new());
    list_frame(&ctx, vec![], &mut query, &mut picked);
    // The field does not take the focus back after the frame it opened (review 3616 L5).
    let mut draw = |ui: &mut egui::Ui| {
        if let Some(hit) = LIST.show(ui, &mut query, kinds, |k| (*k).to_string()) {
            picked.push(hit);
        }
    };
    tab_to(&ctx, SIZE, "Lerp", false, 8, &mut draw);
    frame(&ctx, SIZE, vec![key(Key::Enter, Modifiers::NONE)], &mut draw);
    assert_eq!(picked, vec!["Lerp"], "the focused hit, not the first");
}

#[test]
fn the_search_list_says_when_nothing_matches() {
    let ctx = egui::Context::default();
    let (mut query, mut picked) = ("zzz".to_string(), Vec::new());
    let update = list_frame(&ctx, vec![], &mut query, &mut picked);
    assert!(
        crate::a11y_audit::label_names(&update)
            .iter()
            .any(|l| l == "no kind matches"),
        "{:?}",
        crate::a11y_audit::label_names(&update)
    );
    assert!(labels_of(&update, Role::Button).is_empty());
}

#[test]
fn opened_this_frame_is_true_once_per_opening_and_stable_across_passes() {
    let ctx = egui::Context::default();
    let id = egui::Id::new("graph-kit-test-menu");
    let mut seen = Vec::new();
    for shown in [true, true, false, true] {
        // egui may run a frame's UI twice (a discarded first pass): keep the last pass's answer.
        let mut answer = None;
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            if shown {
                let first = search::opened_this_frame(ui.ctx(), id);
                // A second draw in the same pass answers the same.
                assert_eq!(search::opened_this_frame(ui.ctx(), id), first);
                answer = Some(first);
            }
        });
        out.textures_delta.clear();
        seen.extend(answer);
    }
    assert_eq!(seen, vec![true, false, true], "reopened after a frame unseen");
}

#[test]
fn the_popup_reports_escape() {
    let ctx = egui::Context::default();
    let id = egui::Id::new("graph-kit-test-popup");
    let mut answers = Vec::new();
    for events in [vec![], vec![key(Key::Escape, Modifiers::NONE)]] {
        // A new area's first frame may run twice (its sizing pass); the second pass sees no
        // input, so a frame's answer is "drawn in its last pass, Escape seen in any pass".
        let (mut drawn, mut escape) = (false, false);
        frame(&ctx, SIZE, events, |ui| {
            let (shown, esc) = search::popup(ui.ctx(), id, pos2(40.0, 40.0), |ui| {
                ui.label("x").rect.width() > 0.0
            });
            drawn = shown;
            escape |= esc;
        });
        answers.push((drawn, escape));
    }
    assert_eq!(answers, vec![(true, false), (true, true)]);
}

#[test]
fn a_comment_frame_carries_the_nodes_inside_it() {
    let (min, max) = ([0.0, 0.0], [200.0, 100.0]);
    let positions = [
        ("in", [10.0, 10.0]),
        ("edge", [200.0, 100.0]),
        ("out", [210.0, 50.0]),
    ];
    let inside = frames::nodes_inside(min, max, positions);
    assert_eq!(inside, vec!["in", "edge"]);
    let held: Vec<(&str, [f32; 2])> = positions
        .iter()
        .filter(|(n, _)| inside.contains(n))
        .copied()
        .collect();
    let travel = vec2(30.0, -5.0);
    let (lo, hi) = frames::drag_frame(min, max, frames::FrameGrip::Move, travel);
    let moved = frames::carry(&held, travel);
    assert_eq!(moved, vec![("in", [40.0, 5.0]), ("edge", [230.0, 95.0])]);
    for (_, at) in &moved {
        assert!(
            frames::holds(lo, hi, *at),
            "a carried node stays inside the moved frame"
        );
    }
}

#[test]
fn a_rerouted_wire_passes_through_its_points() {
    let points = [pos2(100.0, -50.0)];
    let path = reroute::reroute_path(pos2(0.0, 0.0), &points, pos2(200.0, 0.0));
    assert_eq!(path[reroute::REROUTE_SAMPLES], points[0]);
    assert_eq!(path.len(), 2 * reroute::REROUTE_SAMPLES + 1);
}

#[test]
fn badges_are_spoken_as_their_message_in_theme_colors() {
    let ctx = egui::Context::default();
    let tokens = crate::theme::ui_tokens(&ctx);
    assert_eq!(Severity::Error.color(&tokens), tokens.error);
    assert_eq!(Severity::Warning.color(&tokens), tokens.warn);
    let mut zero = None;
    let update = frame(&ctx, SIZE, vec![], |ui| {
        badges::badge(ui, Severity::Error, "logic_unknown_task: no task `jump`");
        badges::count_badge(ui, Severity::Warning, 3, "3 warnings in the last run");
        zero = Some(badges::count_badge(ui, Severity::Warning, 0, "none").is_none());
    });
    let labels = crate::a11y_audit::label_names(&update);
    assert!(
        labels.iter().any(|l| l == "logic_unknown_task: no task `jump`"),
        "{labels:?}"
    );
    assert!(
        labels.iter().any(|l| l == "3 warnings in the last run"),
        "{labels:?}"
    );
    assert!(
        !labels.iter().any(|l| l.contains('\u{26a0}')),
        "no bare glyph is spoken: {labels:?}"
    );
    assert_eq!(zero, Some(true), "a zero count draws nothing");
    assert_eq!(badges::count_text(3), "\u{26a0}3");
}

#[test]
fn the_preview_slot_shows_a_swatch_or_why_there_is_none() {
    let ctx = egui::Context::default();
    let mut drawn = Vec::new();
    let update = frame(&ctx, SIZE, vec![], |ui| {
        let texture = ui.ctx().load_texture(
            "graph-kit-test-swatch",
            egui::ColorImage::filled([2, 2], egui::Color32::BLACK),
            egui::TextureOptions::LINEAR,
        );
        let side = preview::NODE_SWATCH_SIDE;
        drawn.push(preview::show(ui, preview::Slot::Ready(&texture), side).map(|r| r.rect.size()));
        drawn.push(preview::show(ui, preview::Slot::Failed("compile error"), side).map(|_| vec2(0.0, 0.0)));
        drawn.push(preview::show(ui, preview::Slot::Empty, side).map(|_| vec2(0.0, 0.0)));
    });
    assert_eq!(drawn[0], Some(vec2(48.0, 48.0)));
    assert!(drawn[1].is_some() && drawn[2].is_none());
    assert!(
        crate::a11y_audit::label_names(&update)
            .iter()
            .any(|l| l == "no preview")
    );
}

/// A canvas of plain numbered nodes, one input and one output each.
struct Numbers;

impl SnarlViewer<u32> for Numbers {
    fn title(&mut self, node: &u32) -> String {
        format!("node {node}")
    }

    fn inputs(&mut self, _node: &u32) -> usize {
        1
    }

    fn outputs(&mut self, _node: &u32) -> usize {
        1
    }

    fn show_input(
        &mut self,
        _pin: &InPin,
        _ui: &mut egui::Ui,
        _snarl: &mut Snarl<u32>,
    ) -> impl SnarlPin + 'static {
        PinInfo::circle()
    }

    fn show_output(
        &mut self,
        _pin: &OutPin,
        _ui: &mut egui::Ui,
        _snarl: &mut Snarl<u32>,
    ) -> impl SnarlPin + 'static {
        PinInfo::circle()
    }
}

#[test]
fn the_canvas_hookup_names_the_canvas_and_frames_one_node() {
    let ctx = egui::Context::default();
    let mut snarl = Snarl::new();
    let first = snarl.insert_node(pos2(0.0, 0.0), 1u32);
    snarl.insert_node(pos2(400.0, 0.0), 2u32);
    for framed in [None, Some(first)] {
        let update = frame(&ctx, SIZE, vec![], |ui| {
            let widget = SnarlWidget::new().id_salt(("graph-kit-test-canvas", framed.is_some()));
            canvas::show(
                ui,
                &widget,
                &mut snarl,
                &mut Numbers,
                framed,
                "Test graph canvas, 2 nodes",
            );
        });
        let named = labels_of(&update, Role::Canvas);
        assert!(
            named.iter().any(|l| l == "Test graph canvas, 2 nodes"),
            "{named:?}"
        );
    }
    assert_eq!(
        snarl.nodes().count(),
        2,
        "framing draws a copy, the graph keeps its nodes"
    );
}

#[test]
fn the_nodes_list_focus_follows_its_node_when_rows_move() {
    let mut keys = crate::canvas_keys::GraphKeys::default();
    keys.list.current = Some(1);
    let before = ["a", "b", "c"];
    let anchor = canvas::anchor_of(&keys, &before);
    assert_eq!(anchor, Some("b"));
    // A node was added before it: its row is now 2.
    let after = ["z", "a", "b", "c"];
    canvas::follow_anchor(&mut keys, anchor.as_ref(), &after);
    assert_eq!(keys.list.current, Some(2));
    // Its node was removed: the focus stays where it was.
    canvas::follow_anchor(&mut keys, Some(&"gone"), &after);
    assert_eq!(keys.list.current, Some(2));
}
