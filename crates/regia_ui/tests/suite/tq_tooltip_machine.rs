// SPDX-License-Identifier: MPL-2.0
//! Test-quality lane (ADR-1744): the tooltip state machine with fixed steps: delay, warm-up,
//! dismiss and suppression, auto-hide, hostile step sizes and a seeded invariant run. No clock.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_app::prelude::*;
use regia_test_utils::fuzz::FuzzRng;
use regia_ui::hints::{TooltipCandidate, TooltipDismiss, TooltipMachine, TooltipPolicy, TooltipTrigger};

/// Binary-exact step so sums of steps are exact.
const DT: f32 = 0.125;

fn e(n: u64) -> Entity {
    Entity::from_bits(n)
}

fn cand(n: u64, delay: f32) -> Option<TooltipCandidate> {
    Some(TooltipCandidate {
        entity: e(n),
        trigger: TooltipTrigger::Hover,
        delay,
    })
}

fn policy() -> TooltipPolicy {
    TooltipPolicy::default()
}

/// Steps until the tooltip opens; returns how many steps it took (at most `limit`).
fn steps_to_open(
    m: &mut TooltipMachine,
    p: &TooltipPolicy,
    c: Option<TooltipCandidate>,
    limit: usize,
) -> Option<usize> {
    (1..=limit).find(|_| m.step(DT, p, c, None))
}

#[test]
fn a_cold_tooltip_waits_its_full_delay_and_not_a_step_less() {
    let mut m = TooltipMachine::default();
    assert_eq!(
        steps_to_open(&mut m, &policy(), cand(1, 0.5), 20),
        Some(4),
        "0.5 s at 0.125 s steps"
    );
    assert_eq!(m.shown_entity(), Some(e(1)));
    assert_eq!(m.shown_trigger(), Some(TooltipTrigger::Hover));
    // A zero delay opens in the first step.
    let mut m = TooltipMachine::default();
    assert!(m.step(DT, &policy(), cand(1, 0.0), None));
    // A NaN or negative delay is not a reason to wait forever or to panic.
    for delay in [f32::NAN, -3.0] {
        let mut m = TooltipMachine::default();
        let opened = steps_to_open(&mut m, &policy(), cand(1, delay), 3);
        assert!(opened.is_some() || delay.is_nan(), "delay {delay}");
    }
}

#[test]
fn leaving_the_node_closes_and_restarts_the_wait() {
    let mut m = TooltipMachine::default();
    let p = policy();
    assert!(!m.step(DT, &p, cand(1, 1.0), None));
    assert!(!m.step(DT, &p, cand(1, 1.0), None));
    assert_eq!(m.waiting(), Some((e(1), 0.25)));
    assert!(!m.step(DT, &p, None, None));
    assert_eq!(m.waiting(), None);
    // Coming back starts from zero (a cold machine: warm-up applies only after a close).
    assert!(!m.step(DT, &p, cand(1, 1.0), None));
    assert_eq!(m.waiting(), Some((e(1), DT)));
}

#[test]
fn warm_up_skips_the_delay_inside_the_window_only() {
    let p = policy();
    let open = |m: &mut TooltipMachine| steps_to_open(m, &p, cand(1, 0.5), 8).expect("opens");
    // Close, then a new node within the window (0.4 s): opens at once.
    let mut m = TooltipMachine::default();
    open(&mut m);
    assert!(!m.step(DT, &p, None, None), "pointer left");
    assert!(m.step(DT, &p, cand(2, 5.0), None), "0.125 s later: still warm");
    // Sweeping across nodes keeps it warm.
    assert!(m.step(DT, &p, cand(3, 5.0), None));
    assert_eq!(m.shown_entity(), Some(e(3)));
    // Close, wait longer than the window, and the full delay applies again.
    assert!(!m.step(DT, &p, None, None));
    for _ in 0..4 {
        assert!(!m.step(DT, &p, None, None));
    }
    assert!(!m.step(DT, &p, cand(4, 0.5), None), "cold again after 0.625 s");
    // A zero warm window disables the shortcut.
    let cold = TooltipPolicy {
        warm_window: 0.0,
        auto_hide: None,
    };
    let mut m = TooltipMachine::default();
    steps_to_open(&mut m, &cold, cand(1, 0.25), 8).expect("opens");
    assert!(!m.step(DT, &cold, None, None));
    assert!(!m.step(DT, &cold, cand(2, 0.5), None), "no warm window: waits");
}

#[test]
fn a_dismiss_keeps_the_tooltip_closed_until_the_node_is_left() {
    let p = policy();
    let mut m = TooltipMachine::default();
    steps_to_open(&mut m, &p, cand(1, 0.25), 8).expect("opens");
    assert!(!m.step(DT, &p, cand(1, 0.25), Some(TooltipDismiss::Press)));
    assert_eq!(m.last_dismiss(), Some(TooltipDismiss::Press));
    for _ in 0..16 {
        assert!(
            !m.step(DT, &p, cand(1, 0.25), None),
            "suppressed while the node stays the candidate"
        );
    }
    // Leaving clears the suppression; coming back opens again and clears the dismiss reason.
    assert!(!m.step(DT, &p, None, None));
    assert_eq!(
        steps_to_open(&mut m, &p, cand(1, 0.25), 4),
        Some(2),
        "cold again: the delay applies"
    );
    assert_eq!(m.last_dismiss(), None);
    // Suppression is per node: a different node opens at once.
    assert!(!m.step(DT, &p, cand(1, 0.25), Some(TooltipDismiss::Cancel)));
    assert!(m.step(DT, &p, cand(2, 0.25), None));
    // Dismissing with no candidate does nothing and records nothing.
    let mut m = TooltipMachine::default();
    assert!(!m.step(DT, &p, None, Some(TooltipDismiss::Scroll)));
    assert_eq!(m.last_dismiss(), None);
}

#[test]
fn auto_hide_closes_once_and_stays_closed_until_the_node_is_left() {
    let p = TooltipPolicy {
        warm_window: 0.4,
        auto_hide: Some(0.5),
    };
    let mut m = TooltipMachine::default();
    steps_to_open(&mut m, &p, cand(1, 0.25), 8).expect("opens");
    let mut open_steps = 0;
    while m.step(DT, &p, cand(1, 0.25), None) {
        open_steps += 1;
        assert!(open_steps < 20, "auto-hide never fired");
    }
    assert_eq!(
        open_steps, 3,
        "0.5 s of shown time at 0.125 s steps closes on the fourth step"
    );
    assert_eq!(m.last_dismiss(), Some(TooltipDismiss::Timeout));
    for _ in 0..8 {
        assert!(!m.step(DT, &p, cand(1, 0.25), None));
    }
    assert!(!m.step(DT, &p, None, None));
    assert_eq!(
        steps_to_open(&mut m, &p, cand(1, 0.25), 4),
        Some(2),
        "re-entering opens again"
    );
    // An auto-hide of zero closes at the first step after opening, without a panic or a loop.
    let zero = TooltipPolicy {
        warm_window: 0.4,
        auto_hide: Some(0.0),
    };
    let mut m = TooltipMachine::default();
    steps_to_open(&mut m, &zero, cand(1, 0.0), 2).expect("opens");
    assert!(!m.step(DT, &zero, cand(1, 0.0), None));
}

#[test]
fn nan_negative_and_infinite_steps_do_not_wedge_the_machine() {
    let p = policy();
    for bad in [f32::NAN, -1.0, f32::NEG_INFINITY] {
        let mut m = TooltipMachine::default();
        assert!(!m.step(bad, &p, cand(1, 0.5), None), "{bad}: no time passed");
        assert_eq!(
            steps_to_open(&mut m, &p, cand(1, 0.5), 8),
            Some(4),
            "{bad}: the wait still completes"
        );
    }
    // An infinite step is a very long wait: the tooltip opens.
    let mut m = TooltipMachine::default();
    assert!(m.step(f32::INFINITY, &p, cand(1, 99.0), None));
}

#[test]
fn a_seeded_run_is_deterministic_and_only_ever_shows_the_current_candidate() {
    let run = |seed: u64| {
        let mut rng = FuzzRng::new(seed);
        let mut m = TooltipMachine::default();
        let p = TooltipPolicy {
            warm_window: 0.4,
            auto_hide: if rng.chance(2) { Some(1.0) } else { None },
        };
        let mut log = Vec::new();
        for _ in 0..2000 {
            let candidate = match rng.below(5) {
                0 => None,
                n => cand(n as u64, rng.below(5) as f32 * 0.25),
            };
            let dismiss = rng.chance(12).then_some(TooltipDismiss::Press);
            let shown = m.step(DT * (1 + rng.below(3)) as f32, &p, candidate, dismiss);
            if shown {
                assert_eq!(m.shown_entity(), candidate.map(|c| c.entity), "seed {seed}");
                assert!(m.shown_trigger().is_some());
            } else {
                assert_eq!(m.shown_entity(), None);
                assert_eq!(m.shown_trigger(), None);
            }
            log.push(shown);
        }
        log
    };
    for seed in [1, 2, 3, 0xBEEF] {
        let a = run(seed);
        assert_eq!(a, run(seed), "seed {seed}");
        assert!(
            a.iter().any(|s| *s) && a.iter().any(|s| !*s),
            "seed {seed} exercises both states"
        );
    }
}

#[test]
fn trigger_names_are_stable() {
    assert_eq!(TooltipTrigger::Hover.name(), "hover");
    assert_eq!(TooltipTrigger::Focus.name(), "focus");
}
