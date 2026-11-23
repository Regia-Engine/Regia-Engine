// SPDX-License-Identifier: MPL-2.0
use super::*;

fn opts(n: usize, disabled: &[usize]) -> Vec<ChoiceOption> {
    (0..n)
        .map(|i| {
            if disabled.contains(&i) {
                ChoiceOption::disabled(format!("o{i}"), format!("why{i}"))
            } else {
                ChoiceOption::new(format!("o{i}"))
            }
        })
        .collect()
}

fn center(r: Rect) -> [f32; 2] {
    [r.x + r.w * 0.5, r.y + r.h * 0.5]
}

fn picked(events: &[ChoiceEvent]) -> Option<usize> {
    events.iter().find_map(|e| match e {
        ChoiceEvent::Picked { index, .. } => Some(*index),
        _ => None,
    })
}

/// Walks focus with `key` and returns every focused index seen in one lap.
fn lap(w: &mut ChoiceWidget, key: ChoiceInput) -> Vec<usize> {
    let mut seen = vec![w.focus().unwrap()];
    for _ in 0..w.options().len() * 2 {
        w.input(key);
        seen.push(w.focus().unwrap());
    }
    seen.sort();
    seen.dedup();
    seen
}

#[test]
fn focus_lands_on_first_enabled_on_open() {
    let mut w = ChoiceWidget::open(ChoiceLayout::List, None, opts(4, &[0, 1]), false);
    assert_eq!(w.focus(), Some(2));
    assert!(w.take_focus_on_open());
    assert!(!w.take_focus_on_open());
}

#[test]
fn list_keyboard_and_gamepad_reach_every_enabled_and_skip_disabled() {
    for key in [
        ChoiceInput::Down,
        ChoiceInput::Up,
        ChoiceInput::Next,
        ChoiceInput::Prev,
    ] {
        let mut w = ChoiceWidget::open(ChoiceLayout::List, None, opts(6, &[1, 4]), false);
        assert_eq!(lap(&mut w, key), vec![0, 2, 3, 5], "{key:?}");
    }
}

#[test]
fn wheel_keys_and_stick_reach_every_enabled_and_skip_disabled() {
    for key in [
        ChoiceInput::Left,
        ChoiceInput::Right,
        ChoiceInput::Next,
        ChoiceInput::Prev,
    ] {
        let mut w = ChoiceWidget::open(ChoiceLayout::Wheel, None, opts(6, &[1, 4]), false);
        assert_eq!(lap(&mut w, key), vec![0, 2, 3, 5], "{key:?}");
    }
    let mut w = ChoiceWidget::open(ChoiceLayout::Wheel, None, opts(6, &[1, 4]), false);
    let mut seen = Vec::new();
    for i in 0..6 {
        let v = w.stick_for(i);
        w.input(ChoiceInput::Stick([0.0, 0.0]));
        w.input(ChoiceInput::Stick(v));
        if w.options()[i].enabled() {
            assert_eq!(w.focus(), Some(i));
            seen.push(i);
        } else {
            assert_ne!(w.focus(), Some(i), "disabled sector took focus");
            assert_eq!(w.hover(), Some(i));
            assert_eq!(w.reason_shown(), Some(format!("why{i}").as_str()));
        }
    }
    assert_eq!(seen, vec![0, 2, 3, 5]);
}

#[test]
fn pointer_and_touch_reach_every_enabled_and_reject_disabled() {
    for layout in [ChoiceLayout::List, ChoiceLayout::Wheel] {
        let probe = ChoiceWidget::open(layout, None, opts(5, &[2]), false);
        let parts = probe.layout().parts;
        for (i, part) in parts.iter().enumerate() {
            for touch in [false, true] {
                let mut w = ChoiceWidget::open(layout, None, opts(5, &[2]), false);
                let px = center(part.rect);
                let ev = w.input(ChoiceInput::PointerMove(px));
                if i != 2 {
                    assert_eq!(w.focus(), Some(i));
                    assert!(ev.contains(&ChoiceEvent::Focused(i)) || i == 0);
                } else {
                    assert_eq!(w.hover(), Some(2));
                }
                let ev = w.input(if touch {
                    ChoiceInput::Touch(px)
                } else {
                    ChoiceInput::PointerPress(px)
                });
                if i == 2 {
                    assert_eq!(
                        ev,
                        vec![ChoiceEvent::Rejected {
                            index: 2,
                            reason: "why2".into()
                        }]
                    );
                    assert!(w.is_open());
                } else {
                    assert_eq!(picked(&ev), Some(i), "{layout:?} {i} touch={touch}");
                    assert!(!w.is_open());
                }
            }
        }
    }
}

#[test]
fn submit_picks_focus_and_digit_picks_by_number() {
    let mut w = ChoiceWidget::open(ChoiceLayout::List, None, opts(3, &[]), false);
    w.input(ChoiceInput::Down);
    assert_eq!(picked(&w.input(ChoiceInput::Submit)), Some(1));
    assert!(w.input(ChoiceInput::Submit).is_empty(), "closed ignores input");
    let mut w = ChoiceWidget::open(ChoiceLayout::List, None, opts(3, &[1]), false);
    assert_eq!(
        w.input(ChoiceInput::Digit(2)),
        vec![ChoiceEvent::Rejected {
            index: 1,
            reason: "why1".into()
        }]
    );
    assert_eq!(picked(&w.input(ChoiceInput::Digit(3))), Some(2));
}

#[test]
fn cancel_closes() {
    let mut w = ChoiceWidget::open(ChoiceLayout::List, None, opts(2, &[]), false);
    assert_eq!(w.input(ChoiceInput::Cancel), vec![ChoiceEvent::Cancelled]);
    assert!(!w.is_open());
}

#[test]
fn timer_picks_default_at_the_exact_fixed_step() {
    let mut w = ChoiceWidget::open(ChoiceLayout::List, None, opts(3, &[]), false).with_timer(5, 2);
    for step in 1..=4 {
        assert!(w.tick().is_empty(), "step {step}");
        assert_eq!(w.timer().unwrap().remaining, 5 - step);
    }
    let ev = w.tick();
    assert_eq!(
        ev,
        vec![ChoiceEvent::Picked {
            index: 2,
            source: PickSource::Timer
        }]
    );
    assert!(!w.is_open());
    assert!(w.tick().is_empty());
}

#[test]
fn timer_default_disabled_falls_back_to_first_enabled() {
    let mut w = ChoiceWidget::open(ChoiceLayout::List, None, opts(3, &[0, 2]), false).with_timer(1, 2);
    assert_eq!(picked(&w.tick()), Some(1));
    let mut w = ChoiceWidget::open(ChoiceLayout::List, None, opts(2, &[0, 1]), false).with_timer(1, 0);
    assert_eq!(w.tick(), vec![ChoiceEvent::Cancelled]);
}

#[test]
fn board_timer_publishes_one_pick_and_removes_widget() {
    let mut board = ChoiceBoard::default();
    let id = board.show(ChoiceWidget::open(ChoiceLayout::List, None, opts(2, &[]), false).with_timer(2, 1));
    assert!(board.step(None).is_empty());
    let p = board.step(None);
    assert_eq!(
        p,
        vec![ChoicePicked {
            widget: id,
            index: Some(1),
            source: PickSource::Timer
        }]
    );
    assert!(board.open_ids().is_empty());
}

#[test]
fn board_routes_nav_actions_to_newest_widget() {
    let mut board = ChoiceBoard::default();
    let _a = board.show(ChoiceWidget::open(ChoiceLayout::List, None, opts(2, &[]), false));
    let b = board.show(ChoiceWidget::open(ChoiceLayout::List, None, opts(3, &[]), false));
    let nav = UiNavActions {
        down: true,
        ..Default::default()
    };
    assert!(board.step(Some(&nav)).is_empty());
    let nav = UiNavActions {
        submit: true,
        ..Default::default()
    };
    let p = board.step(Some(&nav));
    assert_eq!(
        p,
        vec![ChoicePicked {
            widget: b,
            index: Some(1),
            source: PickSource::Player
        }]
    );
}

#[test]
fn rtl_mirrors_the_list() {
    let ltr = ChoiceWidget::open(ChoiceLayout::List, None, opts(3, &[]), false).layout();
    let rtl = ChoiceWidget::open(ChoiceLayout::List, None, opts(3, &[]), true).layout();
    for (l, r) in ltr.parts.iter().zip(&rtl.parts) {
        assert_eq!(l.rect, r.rect);
        let (lm, rm) = (l.marker.unwrap(), r.marker.unwrap());
        assert_eq!(lm.x, l.rect.x);
        assert_eq!(rm.x + rm.w, r.rect.x + r.rect.w);
        assert!(!l.align_end && r.align_end);
    }
}

#[test]
fn rtl_wheel_mirrors_ring_and_turns_keys() {
    let ltr = ChoiceWidget::open(ChoiceLayout::Wheel, None, opts(4, &[]), false);
    let rtl = ChoiceWidget::open(ChoiceLayout::Wheel, None, opts(4, &[]), true);
    let (l, r) = (ltr.layout(), rtl.layout());
    let cx = ltr.wheel_center()[0];
    assert!((center(l.parts[1].rect)[0] - cx + (center(r.parts[1].rect)[0] - cx)).abs() < 1e-3);
    let mut a = ltr.clone();
    let mut b = rtl.clone();
    a.input(ChoiceInput::Right);
    b.input(ChoiceInput::Left);
    assert_eq!(a.focus(), b.focus());
}

#[test]
fn parts_carry_classes_and_a11y_names() {
    let mut w =
        ChoiceWidget::open(ChoiceLayout::List, Some("Pick".into()), opts(3, &[1]), false).with_timer(10, 0);
    let l = w.layout();
    assert_eq!(l.a11y_name, "Pick, 3 options");
    assert_eq!(l.parts[0].classes, vec![CLASS_OPTION, CLASS_FOCUSED]);
    assert_eq!(l.parts[1].classes, vec![CLASS_OPTION, CLASS_DISABLED]);
    assert_eq!(l.parts[1].a11y_name, "o1, 2 of 3, unavailable: why1");
    assert_eq!(l.parts[0].a11y_name, "o0, 1 of 3");
    assert_eq!(l.timer_bar.unwrap().w, 960.0);
    w.tick();
    assert!((w.layout().timer_bar.unwrap().w - 864.0).abs() < 1e-3);
    let wheel = ChoiceWidget::open(ChoiceLayout::Wheel, None, opts(3, &[]), false).layout();
    assert_eq!(wheel.parts[0].classes[0], CLASS_SECTOR);
}

#[test]
fn list_rows_do_not_overlap_and_stay_in_area() {
    let w = ChoiceWidget::open(ChoiceLayout::List, None, opts(5, &[]), false);
    let parts = w.layout().parts;
    for pair in parts.windows(2) {
        assert!(pair[0].rect.y + pair[0].rect.h <= pair[1].rect.y);
    }
}
