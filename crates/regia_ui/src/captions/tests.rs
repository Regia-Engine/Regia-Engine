// SPDX-License-Identifier: MPL-2.0
//! Board behavior tests with a fixed em measure (exact, no GPU). They cover spec 56 § 8 items
//! 5-9.

use regia_app::caption::CaptionStop;
use regia_app::caption::{
    CaptionId, CaptionKind, CaptionPiece, CaptionSource, CaptionStart, CaptionStyleHint, CaptionTime,
};
use serde_json::json;

use super::*;

const MS: u64 = 1_000;

fn layout() -> FixedLayout {
    FixedLayout {
        metrics: BlockMetrics {
            viewport: (1920.0, 1080.0),
            inset: Insets::default(),
        },
    }
}

fn cue(id: u64, text: &str, start_ms: u64, planned_ms: u64) -> CaptionStart {
    CaptionStart {
        id: CaptionId(id),
        source: CaptionSource::Game("test".into()),
        kind: CaptionKind::Speech,
        locale: "en".into(),
        pieces: vec![CaptionPiece::literal(0, text)],
        speaker: None,
        priority: 0,
        start: CaptionTime::AtMicros(start_ms * MS),
        planned_us: planned_ms * MS,
        keep_breaks: false,
        hint: CaptionStyleHint::default(),
    }
}

fn state(board: &CaptionBoard, id: u64) -> Option<EntryState> {
    board.state_of(CaptionId(id))
}

fn step(board: &mut CaptionBoard, now_ms: u64, opts: &CaptionOptions) {
    board.advance(now_ms * MS, opts, &mut layout());
}

#[test]
fn future_cue_is_pending_then_visible_on_the_exact_step() {
    let opts = CaptionOptions::default();
    let mut board = CaptionBoard::default();
    board.push(cue(1, "hello", 100, 2000), 0, &opts);
    step(&mut board, 0, &opts);
    assert_eq!(state(&board, 1), Some(EntryState::Pending));
    step(&mut board, 99, &opts);
    assert_eq!(state(&board, 1), Some(EntryState::Pending));
    step(&mut board, 100, &opts);
    assert_eq!(state(&board, 1), Some(EntryState::Visible));
}

#[test]
fn a_short_cue_is_held_for_reading_speed_but_not_past_the_extension_cap() {
    let opts = CaptionOptions::default();
    let mut board = CaptionBoard::default();
    // 40 graphemes at 17 per second = 2.353 s; planned 400 ms; cap is planned + 2000 ms.
    let text = "abcdefghij abcdefghij abcdefghij abcdefg";
    assert_eq!(grapheme_count(text), 40);
    board.push(cue(1, text, 0, 400), 0, &opts);
    step(&mut board, 0, &opts);
    let hold = 40u64 * 1_000_000;
    let expected = hold.div_ceil(17);
    let snap = board.snapshot();
    assert_eq!(snap.entries[0]["hold_until_us"], json!(expected));
    step(&mut board, 2352, &opts);
    assert_eq!(state(&board, 1), Some(EntryState::Visible));
    step(&mut board, 2354, &opts);
    assert_eq!(state(&board, 1), Some(EntryState::Done));
}

#[test]
fn the_hold_never_exceeds_the_extension_cap_and_counts_an_underrun() {
    let opts = CaptionOptions::default();
    let mut board = CaptionBoard::default();
    let text = "a".repeat(150); // 8.8 s at 17 cps; planned 400 ms; cap 2.4 s
    board.push(cue(1, &text, 0, 400), 0, &opts);
    step(&mut board, 0, &opts);
    let snap = board.snapshot();
    assert!(snap.entries[0]["hold_until_us"].as_u64().unwrap_or(0) <= 2_400 * MS + 3 * MS * 1000);
    assert_eq!(snap.underrun_total, 1);
}

#[test]
fn stop_before_min_display_still_holds_to_min_display() {
    let opts = CaptionOptions::default();
    let mut board = CaptionBoard::default();
    board.push(cue(1, "hi", 0, 5000), 0, &opts);
    step(&mut board, 0, &opts);
    board.stop(CaptionId(1), 200 * MS);
    step(&mut board, 900, &opts);
    assert_eq!(state(&board, 1), Some(EntryState::Visible));
    step(&mut board, 1000, &opts);
    assert_eq!(state(&board, 1), Some(EntryState::Done));
}

#[test]
fn done_entries_are_removed_on_the_next_advance() {
    let opts = CaptionOptions::default();
    let mut board = CaptionBoard::default();
    board.push(cue(1, "hi", 0, 1000), 0, &opts);
    step(&mut board, 0, &opts);
    step(&mut board, 3000, &opts);
    assert_eq!(state(&board, 1), Some(EntryState::Done));
    step(&mut board, 3001, &opts);
    assert_eq!(state(&board, 1), None);
}

#[test]
fn stack_evicts_lowest_priority_then_oldest() {
    let opts = CaptionOptions::default();
    let mut board = CaptionBoard::default();
    for (id, priority) in [(1, 5u8), (2, 1), (3, 1)] {
        let mut c = cue(id, "line", 0, 10_000);
        c.priority = priority;
        board.push(c, 0, &opts);
    }
    step(&mut board, 0, &opts);
    let mut high = cue(4, "urgent", 10, 10_000);
    high.priority = 9;
    board.push(high, 10 * MS, &opts);
    step(&mut board, 10, &opts);
    // Priority 1 ties: the oldest (id 2) is evicted.
    assert_eq!(state(&board, 2), Some(EntryState::Dropped));
    assert_eq!(state(&board, 3), Some(EntryState::Visible));
    assert_eq!(state(&board, 1), Some(EntryState::Visible));
    assert_eq!(state(&board, 4), Some(EntryState::Visible));
    assert_eq!(board.snapshot().dropped_total, 1);
}

#[test]
fn an_equal_priority_cue_waits_when_full_and_expires() {
    let opts = CaptionOptions::default();
    let mut board = CaptionBoard::default();
    for id in 1..=3 {
        board.push(cue(id, "line", 0, 60_000), 0, &opts);
    }
    board.push(cue(4, "late", 0, 500), 0, &opts);
    step(&mut board, 0, &opts);
    assert_eq!(state(&board, 4), Some(EntryState::Waiting));
    assert_eq!(board.snapshot().queue_len, 1);
    // planned end 500 ms + max_wait 1000 ms.
    step(&mut board, 1501, &opts);
    assert_eq!(state(&board, 4), Some(EntryState::Dropped));
}

#[test]
fn replace_same_speaker_ends_only_speech_rows_of_that_speaker() {
    let opts = CaptionOptions::default();
    let mut board = CaptionBoard::new(CaptionPolicy {
        overlap: OverlapPolicy::ReplaceSameSpeaker,
        ..CaptionPolicy::default()
    });
    let mut a = cue(1, "one", 0, 10_000);
    a.speaker = Some("guard".into());
    let mut b = cue(2, "two", 0, 10_000);
    b.speaker = Some("cook".into());
    let mut s = cue(3, "[bang]", 0, 10_000);
    s.kind = CaptionKind::Sound;
    s.speaker = Some("guard".into());
    let mut caption_mode = opts.clone();
    caption_mode.caption_mode = true;
    for c in [a, b, s] {
        board.push(c, 0, &caption_mode);
    }
    step(&mut board, 0, &caption_mode);
    let mut again = cue(4, "three", 5, 10_000);
    again.speaker = Some("guard".into());
    board.push(again, 5 * MS, &caption_mode);
    step(&mut board, 5, &caption_mode);
    assert_eq!(state(&board, 1), Some(EntryState::Done));
    assert_eq!(state(&board, 2), Some(EntryState::Visible));
    assert_eq!(state(&board, 3), Some(EntryState::Visible));
    assert_eq!(state(&board, 4), Some(EntryState::Visible));
}

#[test]
fn queue_shows_one_row_in_priority_start_id_order() {
    let opts = CaptionOptions::default();
    let mut board = CaptionBoard::new(CaptionPolicy {
        overlap: OverlapPolicy::Queue,
        max_wait_us: u64::MAX / 4,
        ..CaptionPolicy::default()
    });
    let mut low = cue(1, "low", 0, 1000);
    low.priority = 1;
    let mut high = cue(2, "high", 0, 1000);
    high.priority = 7;
    let mid = cue(3, "mid", 0, 1000);
    for c in [low, high, mid] {
        board.push(c, 0, &opts);
    }
    step(&mut board, 0, &opts);
    assert_eq!(state(&board, 2), Some(EntryState::Visible));
    assert_eq!(state(&board, 1), Some(EntryState::Waiting));
    step(&mut board, 1000, &opts); // high is Done (min display 1000)
    step(&mut board, 1001, &opts);
    assert_eq!(state(&board, 1), Some(EntryState::Visible));
    assert_eq!(state(&board, 3), Some(EntryState::Waiting));
}

#[test]
fn sound_captions_never_evict_speech_and_need_caption_mode() {
    let mut opts = CaptionOptions::default();
    opts.caption_mode = true;
    let mut board = CaptionBoard::default();
    for id in 1..=3 {
        board.push(cue(id, "speech", 0, 10_000), 0, &opts);
    }
    step(&mut board, 0, &opts);
    let mut sound = cue(4, "[boom]", 10, 10_000);
    sound.kind = CaptionKind::Sound;
    sound.priority = 200;
    board.push(sound, 10 * MS, &opts);
    step(&mut board, 10, &opts);
    assert_eq!(state(&board, 4), Some(EntryState::Waiting));
    assert!((1..=3).all(|id| state(&board, id) == Some(EntryState::Visible)));

    // With caption mode off a sound caption is dropped at once.
    let off = CaptionOptions::default();
    let mut board = CaptionBoard::default();
    let mut sound = cue(1, "[boom]", 0, 1000);
    sound.kind = CaptionKind::Sound;
    board.push(sound, 0, &off);
    assert_eq!(state(&board, 1), Some(EntryState::Dropped));
}

#[test]
fn disabled_drops_speech_but_keeps_sound_captions() {
    let mut opts = CaptionOptions::default();
    opts.enabled = false;
    opts.caption_mode = true;
    let mut board = CaptionBoard::default();
    board.push(cue(1, "speech", 0, 1000), 0, &opts);
    let mut sound = cue(2, "[door]", 0, 1000);
    sound.kind = CaptionKind::Sound;
    board.push(sound, 0, &opts);
    step(&mut board, 0, &opts);
    let snap = board.snapshot();
    assert_eq!(snap.dropped_total, 1);
    assert_eq!(state(&board, 2), Some(EntryState::Visible));
}

#[test]
fn input_order_does_not_change_the_snapshot() {
    let opts = CaptionOptions::default();
    let make = |order: &[u64]| {
        let mut board = CaptionBoard::default();
        for &id in order {
            let mut c = cue(id, "same text", 0, 5000);
            c.priority = (id % 3) as u8;
            board.push(c, 0, &opts);
        }
        step(&mut board, 0, &opts);
        step(&mut board, 50, &opts);
        board.snapshot()
    };
    let a = make(&[1, 2, 3, 4, 5]);
    let b = make(&[5, 3, 1, 4, 2]);
    assert_eq!(a, b);
}

#[test]
fn identical_inputs_give_bit_identical_snapshots() {
    let opts = CaptionOptions::default();
    let run = || {
        let mut board = CaptionBoard::default();
        board.push(
            cue(
                1,
                "a fairly long line of text that should wrap onto a second line",
                0,
                3000,
            ),
            0,
            &opts,
        );
        for t in [0, 16, 33, 50, 2000, 3500] {
            step(&mut board, t, &opts);
        }
        board.snapshot().to_json(true)
    };
    assert_eq!(run(), run());
}

#[test]
fn long_text_paginates_and_pages_advance_over_time() {
    let opts = CaptionOptions::default();
    let mut board = CaptionBoard::default();
    let text = "the quick brown fox jumps over the lazy dog and keeps running through the whole forest until night falls on the hills";
    board.push(cue(1, text, 0, 8000), 0, &opts);
    step(&mut board, 0, &opts);
    let snap = board.snapshot();
    let pages = snap.entries[0]["pages"].as_u64().unwrap_or(0);
    assert!(pages >= 2, "expected pagination, got {pages} page(s)");
    assert!(board.lines_of(CaptionId(1)).is_some_and(|l| l.len() <= 2));
    let first = snap.entries[0]["text"].clone();
    step(&mut board, 7000, &opts);
    let later = board.snapshot().entries[0]["text"].clone();
    assert_ne!(first, later);
    assert!(board.lines_of(CaptionId(1)).is_some_and(|l| l.len() <= 2));
}

#[test]
fn large_text_scale_allows_three_lines() {
    let mut opts = CaptionOptions::default();
    opts.text_scale = 1.5;
    let mut board = CaptionBoard::default();
    board.push(
        cue(1, "aaaa bbbb cccc dddd eeee ffff gggg hhhh", 0, 4000),
        0,
        &opts,
    );
    step(&mut board, 0, &opts);
    assert!(board.lines_of(CaptionId(1)).is_some_and(|l| l.len() <= 3));
}

#[test]
fn rtl_locale_sets_the_direction_from_the_locale() {
    let opts = CaptionOptions::default();
    let mut board = CaptionBoard::default();
    let mut c = cue(1, "abc", 0, 1000);
    c.locale = "ar".into();
    board.push(c, 0, &opts);
    step(&mut board, 0, &opts);
    assert_eq!(board.snapshot().entries[0]["rtl"], json!(true));
}

#[test]
fn cjk_locale_reads_slower() {
    let opts = CaptionOptions::default();
    let mut board = CaptionBoard::default();
    let mut c = cue(1, "今日はいい天気ですね散歩に行きましょう", 0, 500); // 19 graphemes at 9 cps
    c.locale = "ja".into();
    board.push(c, 0, &opts);
    step(&mut board, 0, &opts);
    let hold = board.snapshot().entries[0]["hold_until_us"].as_u64().unwrap_or(0);
    assert_eq!(hold, (19u64 * 1_000_000).div_ceil(9).min(500 * MS + 2_000 * MS));
}

#[test]
fn sub_cues_switch_text_at_their_offsets() {
    let opts = CaptionOptions::default();
    let mut board = CaptionBoard::default();
    let mut c = cue(1, "", 0, 3000);
    c.pieces = vec![
        CaptionPiece::literal(0, "first part"),
        CaptionPiece::literal(1500 * MS, "second part"),
    ];
    board.push(c, 0, &opts);
    step(&mut board, 0, &opts);
    assert_eq!(board.snapshot().entries[0]["text"], json!("first part"));
    step(&mut board, 1499, &opts);
    assert_eq!(board.snapshot().entries[0]["text"], json!("first part"));
    step(&mut board, 1500, &opts);
    assert_eq!(board.snapshot().entries[0]["text"], json!("second part"));
}

#[test]
fn labels_follow_the_option() {
    let mut opts = CaptionOptions::default();
    let mut board = CaptionBoard::default();
    board.set_speaker(
        "guard",
        SpeakerStyle {
            name: "Guard".into(),
            color: Some(Rgb(0x20, 0x20, 0x60)),
            side: LabelSide::Start,
        },
    );
    for id in 1..=2 {
        let mut c = cue(id, "text", 0, 5000);
        c.speaker = Some("guard".into());
        board.push(c, 0, &opts);
    }
    step(&mut board, 0, &opts);
    let snap = board.snapshot();
    // OnChange: only the first row has a label.
    assert_eq!(snap.entries[0]["label"], json!("Guard"));
    assert_eq!(snap.entries[1]["label"], json!(null));
    // The dark speaker color fails 4.5:1 and is reported.
    assert_eq!(snap.contrast_overrides, vec!["guard".to_string()]);
    opts.caption_mode = true;
    step(&mut board, 16, &opts);
    let snap = board.snapshot();
    assert_eq!(snap.entries[1]["label"], json!("Guard"));
}

#[test]
fn safe_rect_respects_the_platform_inset() {
    let metrics = BlockMetrics {
        viewport: (1000.0, 400.0),
        inset: Insets {
            left: 24.0,
            top: 0.0,
            right: 0.0,
            bottom: 48.0,
        },
    };
    let safe = safe_rect(&metrics, 0.0);
    // 5% margins: 50 px left/right, 20 px top; the 48 px inset wins at the bottom.
    assert_eq!((safe.x, safe.y), (50.0, 20.0));
    assert_eq!(safe.y + safe.h, 400.0 - 48.0);
    let raised = safe_rect(&metrics, 100.0);
    assert_eq!(raised.y + raised.h, 400.0 - 148.0);
}

#[test]
fn rows_sit_inside_the_block_above_the_inset_and_a_resize_recomputes() {
    let opts = CaptionOptions::default();
    let mut board = CaptionBoard::default();
    board.push(
        cue(
            1,
            "a line of text for the resize test that wraps a bit more",
            0,
            20_000,
        ),
        0,
        &opts,
    );
    let mut tall = FixedLayout {
        metrics: BlockMetrics {
            viewport: (1280.0, 720.0),
            inset: Insets {
                left: 24.0,
                top: 0.0,
                right: 0.0,
                bottom: 80.0,
            },
        },
    };
    board.advance(0, &opts, &mut tall);
    let snap = board.snapshot();
    let rect = &snap.entries[0]["rect"];
    let bottom = rect["y"].as_f64().unwrap_or(0.0) + rect["h"].as_f64().unwrap_or(0.0);
    assert!(
        bottom <= 720.0 - 80.0 + 0.01,
        "row bottom {bottom} is below the inset"
    );
    let first_lines = board
        .lines_of(CaptionId(1))
        .map(<[String]>::to_vec)
        .unwrap_or_default();
    let first_pages = snap.entries[0]["pages"].clone();

    // A narrow viewport rewraps the page that is showing but never repaginates.
    let mut narrow = FixedLayout {
        metrics: BlockMetrics {
            viewport: (480.0, 800.0),
            inset: Insets::default(),
        },
    };
    board.advance(16 * MS, &opts, &mut narrow);
    let after = board.snapshot();
    assert_eq!(after.entries[0]["pages"], first_pages);
    let second_lines = board
        .lines_of(CaptionId(1))
        .map(<[String]>::to_vec)
        .unwrap_or_default();
    assert_ne!(first_lines, second_lines);
}

#[test]
fn position_top_hangs_rows_from_the_top() {
    let mut opts = CaptionOptions::default();
    opts.position = CaptionPosition::Top;
    let mut board = CaptionBoard::default();
    board.push(cue(1, "hi", 0, 3000), 0, &opts);
    step(&mut board, 0, &opts);
    let snap = board.snapshot();
    let y = snap.entries[0]["rect"]["y"].as_f64().unwrap_or(-1.0);
    assert!((y - f64::from(snap.safe_rect.y)).abs() < 0.01);
}

#[test]
fn max_entries_bounds_the_board() {
    let opts = CaptionOptions::default();
    let mut board = CaptionBoard::new(CaptionPolicy {
        max_entries: 2,
        ..CaptionPolicy::default()
    });
    for id in 1..=3 {
        board.push(cue(id, "x", 0, 1000), 0, &opts);
    }
    assert_eq!(board.len(), 3); // the rejected one is kept as Dropped until the next advance
    assert_eq!(state(&board, 3), Some(EntryState::Dropped));
    assert_eq!(board.snapshot().dropped_total, 1);
}

#[test]
fn key_pieces_show_their_key_when_unresolved_and_adapter_converts_times() {
    let line = SubtitleLine::new("guard", "Halt!", 1.5, 2.0);
    let start = caption_from_subtitle(&line, CaptionId(9), "en");
    assert_eq!(start.start, CaptionTime::AtMicros(1_500_000));
    assert_eq!(start.planned_us, 2_000_000);
    assert_eq!(start.kind, CaptionKind::Speech);
    assert_eq!(start.speaker.as_deref(), Some("guard"));
}

#[test]
fn plugin_feeds_the_board_from_messages_and_the_clock() {
    use regia_app::prelude::*;
    let mut app = App::new();
    app.add_plugins(CaptionPlugin);
    app.world_mut().write_message(cue(1, "from a message", 0, 2000));
    app.update();
    let board = app.world().resource::<CaptionBoard>();
    assert_eq!(board.state_of(CaptionId(1)), Some(EntryState::Visible));
    app.world_mut().write_message(CaptionStop { id: CaptionId(1) });
    app.world_mut().resource_mut::<CaptionClock>().0 = 5_000_000;
    app.update();
    let board = app.world().resource::<CaptionBoard>();
    assert_eq!(board.state_of(CaptionId(1)), Some(EntryState::Done));
}

#[test]
fn rows_become_nodes_under_the_caption_block_and_leave_with_the_entry() {
    use regia_app::prelude::*;

    use crate::node::{NodeName, UiRoot};
    use crate::text::Text;

    let mut app = App::new();
    app.add_plugins(CaptionPlugin);
    // No caption yet: no root, nothing spawned.
    app.update();
    assert!(
        app.world_mut()
            .query::<&CaptionBlock>()
            .iter(app.world())
            .next()
            .is_none()
    );

    app.world_mut().write_message(cue(1, "A visible line", 0, 2000));
    app.update();
    let world = app.world_mut();
    let roots: Vec<_> = world
        .query_filtered::<(&NodeName, &UiRoot), With<CaptionBlock>>()
        .iter(world)
        .map(|(n, r)| (n.0.clone(), r.sort_order))
        .collect();
    assert_eq!(roots, vec![("CaptionBlock".to_string(), CAPTION_SORT_ORDER)]);
    let rows: Vec<_> = world.query::<&CaptionRow>().iter(world).map(|r| r.0).collect();
    assert_eq!(rows, vec![CaptionId(1)]);
    let texts: Vec<String> = world
        .query::<&Text>()
        .iter(world)
        .map(|t| {
            format!(
                "{:?}",
                t.spans
                    .iter()
                    .map(|s| format!("{:?}", s.content))
                    .collect::<Vec<_>>()
            )
        })
        .collect();
    assert!(texts.iter().any(|t| t.contains("A visible line")), "{texts:?}");

    // The entry ends: its row node is despawned.
    app.world_mut().resource_mut::<CaptionClock>().0 = 5_000_000;
    app.update();
    app.update();
    let world = app.world_mut();
    assert!(world.query::<&CaptionRow>().iter(world).next().is_none());
}

/// Spec 56 § 3.1: a `CaptionCueEvent` whose name `cue_events.ron` maps becomes a caption with the
/// mapped key, kind and planned length at the event's time; an unmapped name is ignored.
#[test]
fn a_mapped_cue_event_becomes_a_caption_and_an_unmapped_one_is_ignored() {
    use regia_app::caption::CaptionCueEvent;
    use regia_app::prelude::*;

    let mut app = App::new();
    app.add_plugins(CaptionPlugin);
    // Sound captions show only in caption mode.
    app.world_mut().resource_mut::<CaptionOptions>().caption_mode = true;
    app.world_mut().resource_mut::<CueEventMap>().0.insert(
        "door_slam".into(),
        CueEventDef {
            key: "cap-door-slam".into(),
            kind: CaptionKindDef::Sound,
            planned_ms: 1500,
        },
    );
    app.world_mut().write_message(CaptionCueEvent {
        name: "not_mapped".into(),
        at_us: 0,
    });
    app.world_mut().write_message(CaptionCueEvent {
        name: "door_slam".into(),
        at_us: 0,
    });
    app.update();
    let snapshot = app.world().resource::<CaptionBoard>().snapshot();
    assert_eq!(snapshot.entries.len(), 1, "only the mapped event: {snapshot:?}");
    let entry = &snapshot.entries[0];
    assert_eq!(entry["source"], "game:cue_event:door_slam");
    assert_eq!(entry["kind"], "sound");
    // No localization in this app, so the key shows as the text.
    assert_eq!(entry["text"], "cap-door-slam");
    assert_eq!(entry["end_us"], 1_500_000);
}

/// Spec 56 § 3.3: the caption block root is an accessible `Status` region with polite
/// politeness, so screen readers announce new captions.
#[test]
fn the_caption_block_is_a_polite_status_region() {
    use regia_app::prelude::*;

    use crate::a11y::{A11yRole, Accessible, Politeness};

    let mut app = App::new();
    app.add_plugins(CaptionPlugin);
    app.world_mut().write_message(cue(1, "A visible line", 0, 2000));
    app.update();
    let world = app.world_mut();
    let a11y: Vec<(Option<A11yRole>, Politeness)> = world
        .query_filtered::<&Accessible, With<CaptionBlock>>()
        .iter(world)
        .map(|a| (a.role, a.live))
        .collect();
    assert_eq!(a11y, vec![(Some(A11yRole::Status), Politeness::Polite)]);
}

/// A shaper measured the slow way: every range shaped on its own.
struct EachRangeShaped<'a>(ShapedLayout<'a>);

impl TextMeasure for EachRangeShaped<'_> {
    fn width(&mut self, text: &str, size_px: f32, rtl: bool) -> f32 {
        self.0.width(text, size_px, rtl)
    }
}

#[test]
fn range_widths_from_one_shaping_run_wrap_like_each_range_shaped_alone() {
    use crate::text::TextSystem;

    const CUES: [&str; 5] = [
        "The convoy leaves at first light, so check your gear tonight and keep the radio on; if the \
         bridge is out we take the river road, and nobody stops for anything.",
        "Hard break here\nand a much longer second paragraph that has to wrap at least once more over here.",
        "Averyveryveryveryveryveryverylongwordwithoutanyspacesthatmustsplitbetweengraphemesandkeepsgoing.",
        "Sentinelle : le pont est coupé, prenez la route du fleuve à l'aube, sans vous arrêter.",
        "\u{63a}\u{62f}\u{627}\u{64b} mixed with left to right words that wrap around the block edge.",
    ];
    let mut text = TextSystem::embedded_only();
    let loc = Localization::default();
    let (mut one_run, mut each) = (MeasureCache::default(), MeasureCache::default());
    for cue in CUES {
        for width in [180.0, 420.0, 900.0] {
            let fast = wrap(
                cue,
                width,
                2,
                false,
                24.0,
                &mut ShapedLayout {
                    text: &mut text,
                    loc: &loc,
                    cache: &mut one_run,
                    metrics: BlockMetrics::default(),
                },
            );
            let slow = wrap(
                cue,
                width,
                2,
                false,
                24.0,
                &mut EachRangeShaped(ShapedLayout {
                    text: &mut text,
                    loc: &loc,
                    cache: &mut each,
                    metrics: BlockMetrics::default(),
                }),
            );
            assert_eq!(fast, slow, "{cue} at {width}");
        }
    }
}

#[test]
fn the_shaper_measure_wraps_by_real_glyph_widths() {
    use regia_app::prelude::*;

    use crate::text::TextSystem;

    let mut app = App::new();
    app.add_plugins(CaptionPlugin);
    app.insert_resource(TextSystem::embedded_only());
    app.world_mut().write_message(cue(
        1,
        "The quick brown fox jumps over the lazy dog while the guard keeps watching the old gate carefully",
        0,
        4000,
    ));
    app.update();
    let board = app.world().resource::<CaptionBoard>();
    let lines = board
        .lines_of(CaptionId(1))
        .map(<[String]>::to_vec)
        .unwrap_or_default();
    assert!(!lines.is_empty() && lines.len() <= 2, "{lines:?}");
    // Every shown line fits the block width the board computed (real widths, not the em model).
    let snapshot = board.snapshot();
    let block = f64::from(snapshot.block_rect.w);
    let row_w = snapshot.entries[0]["rect"]["w"].as_f64().unwrap_or(f64::MAX);
    assert!(row_w <= block + 0.01, "row {row_w} wider than block {block}");
    assert!(row_w > 10.0, "a measured row has a width: {row_w}");
}

#[test]
fn a_gap_between_cues_shows_nothing() {
    let opts = CaptionOptions::default();
    let mut board = CaptionBoard::default();
    let mut caption = cue(1, "", 0, 4000);
    caption.pieces = vec![
        CaptionPiece::literal(0, "first"),
        CaptionPiece::literal(1_000 * MS, ""),
        CaptionPiece::literal(2_000 * MS, "second"),
    ];
    board.push(caption, 0, &opts);
    step(&mut board, 0, &opts);
    assert_eq!(
        board.lines_of(CaptionId(1)),
        Some(["first".to_string()].as_slice())
    );
    assert_eq!(board.visible_rows(&opts).len(), 1);
    step(&mut board, 1_500, &opts);
    assert_eq!(state(&board, 1), Some(EntryState::Visible));
    assert!(
        board.visible_rows(&opts).is_empty(),
        "no row, no plate, in the gap"
    );
    step(&mut board, 2_000, &opts);
    assert_eq!(
        board.lines_of(CaptionId(1)),
        Some(["second".to_string()].as_slice())
    );
    assert_eq!(board.visible_rows(&opts).len(), 1);
}

#[test]
fn nothing_shows_before_the_first_cue() {
    // Audit ui1 correctness 4: a set whose first cue starts late must not leak its text
    // before the cue; the page fallback used to copy page 0.
    let opts = CaptionOptions::default();
    let mut board = CaptionBoard::default();
    let mut caption = cue(1, "", 0, 5000);
    caption.pieces = vec![CaptionPiece::literal(2_000 * MS, "hello")];
    board.push(caption, 0, &opts);
    step(&mut board, 0, &opts);
    assert_eq!(state(&board, 1), Some(EntryState::Visible));
    assert!(
        board.visible_rows(&opts).is_empty(),
        "no row before the first cue starts"
    );
    step(&mut board, 2_000, &opts);
    assert_eq!(
        board.lines_of(CaptionId(1)),
        Some(["hello".to_string()].as_slice())
    );
    assert_eq!(board.visible_rows(&opts).len(), 1);
}

fn device_at(device_us: u64) -> CaptionDeviceClock {
    CaptionDeviceClock {
        device_us: Some(device_us),
        ..CaptionDeviceClock::default()
    }
}

#[test]
fn the_caption_clock_is_fixed_step_without_a_device() {
    let mut clock = CaptionDeviceClock::default();
    assert_eq!(clock.step(0, Some(16_000), false), 16_000);
    assert_eq!(
        clock.step(5_000_000, Some(32_000), false),
        5_000_000,
        "never backwards"
    );
    assert!(!clock.following());
    assert_eq!(clock.to_caption(1_000), None);
}

#[test]
fn the_caption_clock_equals_the_device_clock_from_a_fresh_start() {
    let mut clock = device_at(100_000);
    assert_eq!(clock.step(0, Some(0), false), 100_000);
    assert!(clock.following());
    assert_eq!(clock.to_caption(250_000), Some(250_000));
    clock.device_us = Some(250_000);
    assert_eq!(
        clock.step(100_000, Some(16_000), false),
        250_000,
        "the device paces it, not the step"
    );
}

#[test]
fn a_device_that_starts_late_continues_the_caption_clock() {
    let mut clock = device_at(1_000);
    assert_eq!(
        clock.step(3_000_000, Some(3_000_000), false),
        3_000_000,
        "no jump, no freeze"
    );
    clock.device_us = Some(11_000);
    assert_eq!(clock.step(3_000_000, Some(3_016_000), false), 3_010_000);
    assert_eq!(clock.to_caption(21_000), Some(3_020_000));
}

#[test]
fn a_pause_holds_the_caption_clock_while_the_device_runs_on() {
    let mut clock = device_at(1_000_000);
    let held = clock.step(0, Some(0), false);
    clock.device_us = Some(4_000_000);
    assert_eq!(clock.step(held, Some(0), true), held, "paused");
    assert!(!clock.following());
    assert_eq!(
        clock.step(held, Some(0), false),
        held,
        "picks up where it stopped"
    );
    clock.device_us = Some(4_100_000);
    assert_eq!(clock.step(held, Some(16_000), false), held + 100_000);
}

#[test]
fn a_lost_device_falls_back_to_fixed_steps_without_a_jump() {
    let mut clock = device_at(9_000_000);
    let on_device = clock.step(0, Some(1_000_000), false);
    assert_eq!(on_device, 9_000_000);
    clock.device_us = None;
    assert_eq!(clock.step(on_device, Some(1_016_000), false), 9_016_000);
}

#[test]
fn the_plugin_follows_a_fed_device_clock() {
    use regia_app::prelude::*;
    let mut app = App::new();
    app.add_plugins(CaptionPlugin);
    app.world_mut().resource_mut::<CaptionDeviceClock>().device_us = Some(2_000_000);
    app.update();
    assert_eq!(app.world().resource::<CaptionClock>().0, 2_000_000);
    assert_eq!(app.world().resource::<CaptionBoard>().clock_us(), 2_000_000);
}

fn set_cue(at: f32, until: Option<f32>, text: &str) -> regia_asset::dialogue::Cue {
    regia_asset::dialogue::Cue {
        at,
        until,
        text_key: None,
        text: Some(text.into()),
    }
}

fn set_app() -> regia_app::App {
    let mut app = regia_app::App::new();
    app.add_plugins(CaptionPlugin);
    app
}

fn set_request() -> SetRequest {
    SetRequest {
        cues: vec![set_cue(0.0, Some(1.0), "one"), set_cue(2.0, None, "two")],
        duration: 4.0,
        ..SetRequest::default()
    }
}

fn clock_to(app: &mut regia_app::App, us: u64) {
    app.world_mut().resource_mut::<CaptionClock>().0 = us;
    app.update();
}

fn shown(app: &regia_app::App) -> Vec<String> {
    let opts = app.world().resource::<CaptionOptions>();
    let rows = app.world().resource::<CaptionBoard>().visible_rows(opts);
    rows.iter().flat_map(|r| r.lines.clone()).collect()
}

#[test]
fn a_set_runs_its_cues_on_the_board_and_leaves() {
    let mut app = set_app();
    let mut req = set_request();
    req.start_delay = 0.5;
    let id = queue_set(app.world_mut(), &req).expect("valid").expect("shown");
    app.update();
    assert!(shown(&app).is_empty(), "the delay has not passed");
    assert!(app.world().resource::<SubtitleSets>().is_active());
    clock_to(&mut app, 500_000);
    assert_eq!(shown(&app), ["one"]);
    clock_to(&mut app, 1_500_000);
    assert!(shown(&app).is_empty(), "a gap");
    clock_to(&mut app, 2_500_000);
    assert_eq!(shown(&app), ["two"]);
    assert_eq!(
        app.world().resource::<CaptionBoard>().state_of(id),
        Some(EntryState::Visible)
    );
    clock_to(&mut app, 4_500_000);
    clock_to(&mut app, 4_600_000);
    assert!(
        !app.world().resource::<SubtitleSets>().is_active(),
        "gone when it finished"
    );
}

#[test]
fn set_requests_are_checked() {
    let mut app = set_app();
    let world = app.world_mut();
    let bad = |edit: &dyn Fn(&mut SetRequest)| {
        let mut req = set_request();
        edit(&mut req);
        sets::set_problem(&req)
    };
    assert!(bad(&|r| r.duration = 0.0).is_some());
    assert!(bad(&|r| r.duration = f32::NAN).is_some());
    assert!(bad(&|r| r.start_delay = -1.0).is_some());
    assert!(bad(&|r| r.priority = -1).is_some());
    assert!(
        bad(&|r| r.cues[1].at = 0.5).is_none(),
        "ordered, overlapping is the author's call"
    );
    assert!(bad(&|r| r.cues[1].at = -0.5).is_some(), "out of order");
    assert!(bad(&|r| r.cues[0].text = None).is_some(), "no text");
    let mut zero = set_request();
    zero.priority = 0;
    assert_eq!(queue_set(world, &zero), Ok(None), "priority 0 is never shown");
    assert!(!world.resource::<SubtitleSets>().is_active());
    assert!(queue_set(world, &SetRequest::default()).is_err());
}

#[test]
fn sets_stop_one_by_one_by_owner_and_all() {
    let mut app = set_app();
    let owner = app.world_mut().spawn_empty().id();
    let mut owned = set_request();
    owned.owner = Some(owner);
    let world = app.world_mut();
    let first = queue_set(world, &owned).expect("valid").expect("shown");
    let second = queue_set(world, &set_request()).expect("valid").expect("shown");
    let third = queue_set(world, &set_request()).expect("valid").expect("shown");
    assert_eq!(stop_owner(world, owner), 1);
    assert!(stop_set(world, second));
    assert!(!stop_set(world, second), "already stopped");
    assert!(!stop_set(world, first));
    assert_eq!(stop_all_sets(world), 1);
    assert!(!world.resource::<SubtitleSets>().is_active());
    assert_eq!(stop_all_sets(world), 0);
    assert!(third > second);
}

#[test]
fn a_caption_set_shows_only_in_caption_mode() {
    let mut app = set_app();
    let mut caption = set_request();
    caption.kind = SubtitleKind::Caption;
    queue_set(app.world_mut(), &caption).expect("valid");
    app.update();
    assert!(
        !app.world().resource::<SubtitleSets>().is_active(),
        "caption mode is off"
    );
    assert!(shown(&app).is_empty());
    app.world_mut().resource_mut::<CaptionOptions>().caption_mode = true;
    queue_set(app.world_mut(), &caption).expect("valid");
    app.update();
    assert_eq!(shown(&app), ["one"]);
}

#[test]
fn a_recorded_set_sequence_reproduces_the_same_text() {
    let run = || {
        let mut app = set_app();
        queue_set(app.world_mut(), &set_request()).expect("valid");
        let mut frames = Vec::new();
        for ms in (0..5000).step_by(250) {
            clock_to(&mut app, ms * MS);
            frames.push(shown(&app));
        }
        frames
    };
    assert_eq!(run(), run());
}

// Spec 36 § 8.1 `one_at_a_time` and § 8.2 `sync` (ADR-2137).

const LONG: &str = "the quick brown fox jumps over the lazy dog and keeps running through the whole forest until night falls on the hills";

fn long_request(one_at_a_time: bool) -> SetRequest {
    SetRequest {
        cues: vec![set_cue(0.0, None, LONG)],
        duration: 12.0,
        one_at_a_time,
        ..SetRequest::default()
    }
}

/// `(pages, lines of the visible page)` of a set's entry.
fn pages_and_lines(app: &regia_app::App, id: CaptionId) -> (u64, usize) {
    let board = app.world().resource::<CaptionBoard>();
    let snap = board.snapshot();
    let entry = snap
        .entries
        .iter()
        .find(|e| e["id"] == id.0)
        .cloned()
        .unwrap_or_default();
    (
        entry["pages"].as_u64().unwrap_or(0),
        board.lines_of(id).map_or(0, <[String]>::len),
    )
}

#[test]
fn one_at_a_time_shows_a_long_cue_one_line_per_page_in_sequence() {
    let mut stacked = set_app();
    let id = queue_set(stacked.world_mut(), &long_request(false))
        .expect("valid")
        .expect("shown");
    stacked.update();
    let (stacked_pages, stacked_lines) = pages_and_lines(&stacked, id);
    assert_eq!(stacked_lines, 2, "stacked: two lines on the first page");

    let mut single = set_app();
    let id = queue_set(single.world_mut(), &long_request(true))
        .expect("valid")
        .expect("shown");
    single.update();
    let (pages, lines) = pages_and_lines(&single, id);
    assert_eq!(lines, 1);
    assert!(
        pages > stacked_pages && pages >= 3,
        "one line per page gives more pages ({pages} vs {stacked_pages})"
    );
    let snap = single.world().resource::<CaptionBoard>().snapshot();
    assert_eq!(snap.entries[0]["one_at_a_time"], true);
    // Every page shows one line, and the pages come in order over the cue's time.
    let mut seen = Vec::new();
    for ms in (0..12_000).step_by(250) {
        clock_to(&mut single, ms * MS);
        if single.world().resource::<CaptionBoard>().state_of(id) != Some(EntryState::Visible) {
            break;
        }
        assert_eq!(pages_and_lines(&single, id).1, 1, "at {ms} ms");
        let page = single.world().resource::<CaptionBoard>().snapshot().entries[0]["page"]
            .as_u64()
            .unwrap_or(0);
        if seen.last() != Some(&page) {
            seen.push(page);
        }
    }
    let expected: Vec<u64> = (0..pages).collect();
    assert_eq!(seen, expected, "each page once, in order");
}

#[test]
fn pieces_from_drops_ended_cues_and_starts_the_running_one_at_zero() {
    let req = set_request();
    let caption = sets::set_caption(&req, CaptionId(1), "en", 0);
    // Pieces: "one" 0..1 s, gap 1..2 s, "two" 2..4 s.
    let (pieces, planned) = pieces_from(&caption, 500_000).expect("running");
    assert_eq!(planned, 3_500_000);
    let offsets: Vec<u64> = pieces.iter().map(|p| p.offset_us).collect();
    assert_eq!(offsets, [0, 500_000, 1_500_000]);
    let (pieces, planned) = pieces_from(&caption, 1_200_000).expect("in the gap");
    assert_eq!(planned, 2_800_000);
    assert_eq!(pieces.len(), 2, "the gap and two");
    assert_eq!(pieces[1].offset_us, 800_000);
    let (pieces, _) = pieces_from(&caption, 3_000_000).expect("last cue");
    assert_eq!(pieces.len(), 1);
    assert_eq!(pieces[0].offset_us, 0);
    assert_eq!(pieces_from(&caption, 4_000_000), None, "at the end");
    assert_eq!(pieces_from(&caption, 9_000_000), None, "past the end");
}

#[test]
fn sync_within_tolerance_changes_nothing() {
    let mut app = set_app();
    let id = queue_set(app.world_mut(), &set_request())
        .expect("valid")
        .expect("shown");
    clock_to(&mut app, 500_000);
    let before = app.world().resource::<CaptionBoard>().snapshot().to_json(true);
    let near = (500_000 + SYNC_TOLERANCE_US) as f32 / 1_000_000.0;
    assert_eq!(sync_set(app.world_mut(), id, near), Ok(Some(SyncOutcome::InSync)));
    assert_eq!(sync_set(app.world_mut(), id, 0.48), Ok(Some(SyncOutcome::InSync)));
    app.update();
    let after = app.world().resource::<CaptionBoard>().snapshot().to_json(true);
    assert_eq!(before, after);
}

#[test]
fn sync_jumps_forward_and_back_on_the_same_id() {
    let mut app = set_app();
    let id = queue_set(app.world_mut(), &set_request())
        .expect("valid")
        .expect("shown");
    app.update();
    assert_eq!(shown(&app), ["one"]);
    // The voice is already 2.5 s in: the set jumps to "two" at once.
    assert_eq!(sync_set(app.world_mut(), id, 2.5), Ok(Some(SyncOutcome::Rebased)));
    app.update();
    assert_eq!(shown(&app), ["two"]);
    let board = app.world().resource::<CaptionBoard>();
    assert_eq!(board.state_of(id), Some(EntryState::Visible));
    assert_eq!(board.len(), 1, "replaced, not added");
    // Now in sync: a later playhead that matches the clock is a no-op.
    clock_to(&mut app, 500_000);
    assert_eq!(sync_set(app.world_mut(), id, 3.0), Ok(Some(SyncOutcome::InSync)));
    // Back to 0.2 s: "one" again, still the same id.
    assert_eq!(sync_set(app.world_mut(), id, 0.2), Ok(Some(SyncOutcome::Rebased)));
    app.update();
    assert_eq!(shown(&app), ["one"]);
    clock_to(&mut app, 1_500_000);
    assert!(shown(&app).is_empty(), "the gap, 1.2 s of set time");
    clock_to(&mut app, 2_400_000);
    assert_eq!(shown(&app), ["two"], "2.1 s of set time");
    assert!(app.world().resource::<SubtitleSets>().is_active());
}

#[test]
fn a_rebased_visible_set_keeps_its_row_against_waiting_sets() {
    let mut app = set_app();
    app.world_mut().resource_mut::<CaptionBoard>().policy.overlap = OverlapPolicy::Queue;
    let first = queue_set(app.world_mut(), &set_request())
        .expect("valid")
        .expect("shown");
    app.update();
    let second = queue_set(app.world_mut(), &set_request())
        .expect("valid")
        .expect("shown");
    clock_to(&mut app, 300_000);
    let board = app.world().resource::<CaptionBoard>();
    assert_eq!(board.state_of(first), Some(EntryState::Visible));
    assert_eq!(board.state_of(second), Some(EntryState::Waiting));
    // Re-based, the first set starts later than the second, yet keeps the one row.
    assert_eq!(
        sync_set(app.world_mut(), first, 2.2),
        Ok(Some(SyncOutcome::Rebased))
    );
    app.update();
    let board = app.world().resource::<CaptionBoard>();
    assert_eq!(board.state_of(first), Some(EntryState::Visible));
    assert_eq!(board.state_of(second), Some(EntryState::Waiting));
    assert_eq!(shown(&app), ["two"]);
}

/// A subtitle that stays on one page: a later step must not measure again, and the row `Text`
/// must not be rebuilt.
#[test]
fn held_caption_does_not_allocate_or_reshape() {
    use regia_app::prelude::*;

    use crate::text::Text;

    let opts = CaptionOptions::default();
    let mut board = CaptionBoard::default();
    board.push(cue(1, "Hello subtitle", 0, 60_000), 0, &opts);
    step(&mut board, 0, &opts);
    assert_eq!(state(&board, 1), Some(EntryState::Visible));
    let lines = board.lines_of(CaptionId(1)).expect("lines").to_vec();
    let runs = board.layout_runs();
    let rect = board.visible_rows(&opts)[0].rect;
    // Still the same page 100 ms later. The clock moved; the row did not.
    step(&mut board, 100, &opts);
    assert_eq!(board.layout_runs(), runs, "a held page measured again");
    assert_eq!(board.lines_of(CaptionId(1)).map(<[String]>::to_vec), Some(lines));
    assert_eq!(board.visible_rows(&opts)[0].rect, rect);

    let mut app = App::new();
    app.add_plugins(CaptionPlugin);
    app.world_mut().write_message(cue(1, "Hello subtitle", 0, 60_000));
    app.update();
    let built = rows::row_text_builds();
    app.update();
    assert_eq!(rows::row_text_builds(), built, "the held row built a new Text");
    let world = app.world_mut();
    world.clear_trackers();
    let before = rows::row_text_builds();
    rows::sync_caption_rows(world);
    assert_eq!(rows::row_text_builds(), before);
    let text_entity = world
        .query::<(Entity, &Text)>()
        .iter(world)
        .map(|(e, _)| e)
        .next()
        .expect("row text");
    assert!(
        !world
            .entity(text_entity)
            .get_ref::<Text>()
            .expect("text")
            .is_changed(),
        "the held row Text was marked changed"
    );
}

/// A width that is already remembered must not allocate a key or shape again.
#[test]
fn width_hit_does_not_allocate_or_shape() {
    use crate::text::TextSystem;

    let mut text = TextSystem::embedded_only();
    let loc = Localization::default();
    let mut cache = MeasureCache::default();
    let size = 24.0_f32;
    let measure = |text: &mut TextSystem, cache: &mut MeasureCache, line: &str| {
        let mut layout = ShapedLayout {
            text,
            loc: &loc,
            cache,
            metrics: BlockMetrics::default(),
        };
        layout.width(line, size, false)
    };
    let _ = measure(&mut text, &mut cache, "Hello subtitle");
    let allocs = cache.lookup_allocs;
    let shapes = cache.shapes;
    let cached = cache.cached_widths();
    let _ = measure(&mut text, &mut cache, "Hello subtitle");
    assert_eq!(cache.lookup_allocs, allocs, "a width hit allocated a cache key");
    assert_eq!(cache.shapes, shapes, "a width hit shaped");
    assert_eq!(cache.cached_widths(), cached, "a width hit grew the cache");
}

/// Filling the width cache must evict one old entry, not drop the line a held caption just measured.
#[test]
fn full_width_cache_keeps_the_held_line() {
    use crate::text::TextSystem;

    let mut text = TextSystem::embedded_only();
    let loc = Localization::default();
    let mut cache = MeasureCache::default();
    let size = 24.0_f32;
    cache.insert_test_width("Hello subtitle", size, 12.0);
    for i in 0..2047 {
        cache.insert_test_width(&format!("pad-{i}"), size, 1.0);
    }
    assert_eq!(cache.cached_widths(), 2048);
    let measure = |text: &mut TextSystem, cache: &mut MeasureCache, line: &str| {
        let mut layout = ShapedLayout {
            text,
            loc: &loc,
            cache,
            metrics: BlockMetrics::default(),
        };
        layout.width(line, size, false)
    };
    // The held line is measured every frame, so it is newest before the overflow miss.
    let _ = measure(&mut text, &mut cache, "Hello subtitle");
    let shapes = cache.shapes;
    let _ = measure(&mut text, &mut cache, "one-more-line");
    let _ = measure(&mut text, &mut cache, "Hello subtitle");
    assert_eq!(
        cache.shapes,
        shapes + 1,
        "the overflow miss reshaped the held line (shapes {shapes} -> {})",
        cache.shapes
    );
}

/// No caption: the row sync does no work after the plugin is installed.
#[test]
fn empty_board_does_not_touch_rows() {
    use regia_app::prelude::*;

    let mut app = App::new();
    app.add_plugins(CaptionPlugin);
    let before = rows::row_sync_bodies();
    for _ in 0..4 {
        app.update();
    }
    assert_eq!(rows::row_sync_bodies(), before, "an empty board ran the row sync");
    assert!(
        app.world_mut()
            .query::<&CaptionBlock>()
            .iter(app.world())
            .next()
            .is_none(),
        "an empty board spawned CaptionBlock"
    );
}

#[test]
fn sync_past_the_end_stops_the_set_and_unknown_ids_and_bad_playheads_are_reported() {
    let mut app = set_app();
    let id = queue_set(app.world_mut(), &set_request())
        .expect("valid")
        .expect("shown");
    app.update();
    assert_eq!(sync_set(app.world_mut(), CaptionId(9_999), 1.0), Ok(None));
    assert!(sync_set(app.world_mut(), id, -0.5).is_err());
    assert!(sync_set(app.world_mut(), id, f32::NAN).is_err());
    assert!(sync_set(app.world_mut(), id, f32::INFINITY).is_err());
    assert_eq!(
        sync_set(app.world_mut(), id, 4.0),
        Ok(Some(SyncOutcome::Finished))
    );
    assert!(!app.world().resource::<SubtitleSets>().is_active());
    assert_eq!(
        sync_set(app.world_mut(), id, 1.0),
        Ok(None),
        "stopped sets are gone"
    );
    for outcome in [SyncOutcome::InSync, SyncOutcome::Rebased, SyncOutcome::Finished] {
        assert!(
            outcome
                .name()
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b == b'_')
        );
    }
}

#[test]
fn syncing_every_frame_on_time_reproduces_the_unsynced_run() {
    let run = |sync: bool| {
        let mut app = set_app();
        let id = queue_set(app.world_mut(), &set_request())
            .expect("valid")
            .expect("shown");
        let mut frames = Vec::new();
        for ms in (0..5000).step_by(250) {
            app.world_mut().resource_mut::<CaptionClock>().0 = ms * MS;
            if sync {
                let playhead = ms as f32 / 1000.0;
                let outcome = sync_set(app.world_mut(), id, playhead).expect("valid");
                assert!(
                    matches!(outcome, Some(SyncOutcome::InSync) | None),
                    "on time at {ms} ms: {outcome:?}"
                );
            }
            app.update();
            frames.push(shown(&app));
        }
        frames
    };
    assert_eq!(run(true), run(false));
}

/// Spec 56 § 2.8 `enabled` / `caption_mode` and spec 36 § 8.4 (4): turning an option off takes the
/// rows it filters off the screen even when they are already shown (lane spec-15-35; before, a
/// visible row stayed up). Since ADR-5993 (lead decision 6) they are hidden, not dropped: they keep
/// their timing and come back if the option is turned on again before they end.
#[test]
fn turning_captions_off_hides_rows_already_on_screen() {
    let mut opts = CaptionOptions::default();
    opts.caption_mode = true;
    let mut board = CaptionBoard::default();
    board.push(cue(1, "speech", 0, 10_000), 0, &opts);
    let mut sound = cue(2, "[door]", 0, 10_000);
    sound.kind = CaptionKind::Sound;
    board.push(sound, 0, &opts);
    step(&mut board, 0, &opts);
    assert_eq!(state(&board, 1), Some(EntryState::Visible));
    assert_eq!(state(&board, 2), Some(EntryState::Visible));

    opts.enabled = false;
    step(&mut board, 10, &opts);
    assert_eq!(
        state(&board, 1),
        Some(EntryState::Waiting),
        "speech leaves the screen"
    );
    assert_eq!(state(&board, 2), Some(EntryState::Visible), "sound stays");
    let lines: Vec<String> = board
        .visible_rows(&opts)
        .iter()
        .flat_map(|r| r.lines.clone())
        .collect();
    assert_eq!(lines, ["[door]"]);
    assert_eq!(board.snapshot().dropped_total, 0, "a hidden row is not dropped");

    opts.caption_mode = false;
    step(&mut board, 20, &opts);
    assert_eq!(
        state(&board, 2),
        Some(EntryState::Waiting),
        "sound leaves the screen"
    );
    assert!(board.visible_rows(&opts).is_empty());

    // Turning both back on before the rows end shows them again.
    opts.enabled = true;
    opts.caption_mode = true;
    step(&mut board, 30, &opts);
    assert_eq!(state(&board, 1), Some(EntryState::Visible));
    assert_eq!(state(&board, 2), Some(EntryState::Visible));
    // Hidden past its end, a row finishes (Done), not dropped.
    opts.enabled = false;
    step(&mut board, 40, &opts);
    // Past its end (and any reading-time hold the re-show added).
    step(&mut board, 60_000, &opts);
    assert_eq!(state(&board, 1), Some(EntryState::Done));
    assert_eq!(board.snapshot().dropped_total, 0);
}

/// Spec 36 § 8.4 (4) through the sets layer: `enabled = false` (what `subtitle_sets.set_enabled`
/// writes) leaves nothing on screen, but the set still advances (ADR-5993, lead decision 6,
/// which reverts ADR-5778's "the set ends"): a player who turns subtitles off and on again mid-line
/// sees the rest of the line, and the set's next cue.
#[test]
fn disabling_subtitles_hides_a_visible_set_without_ending_it() {
    let mut app = set_app();
    let mut req = set_request();
    req.duration = 10.0;
    queue_set(app.world_mut(), &req).expect("valid").expect("shown");
    clock_to(&mut app, 100_000);
    assert_eq!(shown(&app), ["one"]);
    app.world_mut().resource_mut::<CaptionOptions>().enabled = false;
    clock_to(&mut app, 200_000);
    assert!(shown(&app).is_empty(), "nothing shown while off");
    clock_to(&mut app, 300_000);
    assert!(
        app.world().resource::<SubtitleSets>().is_active(),
        "the set still advances while subtitles are off"
    );
    app.world_mut().resource_mut::<CaptionOptions>().enabled = true;
    clock_to(&mut app, 400_000);
    assert_eq!(shown(&app), ["one"], "the rest of the line shows again");
    clock_to(&mut app, 2_500_000);
    assert_eq!(shown(&app), ["two"], "and the set goes on to its next cue");
}

/// Spec 36 § 8.4 (4): a set whose time runs out while subtitles are off finishes (it is not left
/// waiting for them), and nothing of it shows when they come back on.
#[test]
fn a_set_that_ends_while_subtitles_are_off_finishes() {
    let mut app = set_app();
    queue_set(app.world_mut(), &set_request())
        .expect("valid")
        .expect("shown");
    clock_to(&mut app, 100_000);
    assert_eq!(shown(&app), ["one"]);
    app.world_mut().resource_mut::<CaptionOptions>().enabled = false;
    clock_to(&mut app, 200_000);
    clock_to(&mut app, 4_500_000);
    clock_to(&mut app, 4_600_000);
    assert!(
        !app.world().resource::<SubtitleSets>().is_active(),
        "the set finished at its end"
    );
    app.world_mut().resource_mut::<CaptionOptions>().enabled = true;
    clock_to(&mut app, 4_700_000);
    assert!(shown(&app).is_empty());
}

/// Spec 36 § 8.3: the caption block is announced to the accessibility tree as a status live region
/// with polite priority (lane spec-15-35).
#[test]
fn the_caption_block_is_a_polite_live_region() {
    use regia_app::prelude::*;

    use crate::a11y::{A11yRole, Accessible, Politeness};

    let mut app = App::new();
    app.add_plugins(CaptionPlugin);
    app.world_mut().write_message(cue(1, "A visible line", 0, 2000));
    app.update();
    let world = app.world_mut();
    let blocks: Vec<(Option<A11yRole>, Politeness, bool)> = world
        .query_filtered::<&Accessible, With<CaptionBlock>>()
        .iter(world)
        .map(|a| (a.role, a.live, a.hidden))
        .collect();
    assert_eq!(blocks, vec![(Some(A11yRole::Status), Politeness::Polite, false)]);
}

/// A layout pass rewraps at most two rows; a third row that appeared in the same frame waits
/// for the next pass. The pass must not stamp itself current while a row still waits, or the
/// third caption never shows (and a resize leaves its old wrap) until its page changes.
#[test]
fn a_third_caption_in_one_frame_shows_on_the_next_pass() {
    let opts = CaptionOptions::default();
    let mut board = CaptionBoard::default();
    for id in 1..=3 {
        board.push(cue(id, "a line of speech", 0, 10_000), 0, &opts);
    }
    step(&mut board, 0, &opts);
    step(&mut board, 16, &opts);
    for id in 1..=3 {
        assert!(
            board
                .lines_of(CaptionId(id))
                .is_some_and(|lines| lines.iter().any(|l| !l.is_empty())),
            "caption {id} has no lines"
        );
    }
    assert_eq!(board.visible_count(), 3);
    // Settled: a held frame lays out nothing.
    let runs = board.layout_runs();
    step(&mut board, 32, &opts);
    assert_eq!(board.layout_runs(), runs);
}

/// A caption whose first cue lies in the future shows nothing, and that held state is current:
/// it must not lay the board out again every frame.
#[test]
fn a_caption_before_its_first_cue_does_not_relayout_every_frame() {
    let opts = CaptionOptions::default();
    let mut board = CaptionBoard::default();
    let mut late = cue(1, "", 0, 10_000);
    late.pieces = vec![CaptionPiece::literal(2_000 * MS, "later")];
    board.push(late, 0, &opts);
    step(&mut board, 0, &opts);
    step(&mut board, 16, &opts);
    let runs = board.layout_runs();
    step(&mut board, 32, &opts);
    step(&mut board, 48, &opts);
    assert_eq!(board.layout_runs(), runs, "a held frame laid the board out again");
}

/// Spec 56 § 2.7: a piece held past its planned end for reading speed is not held past the start
/// of the next piece of the same speaker; another speaker's row keeps its hold.
#[test]
fn a_held_piece_ends_at_the_next_piece_of_the_same_speaker() {
    let opts = CaptionOptions::default();
    let mut board = CaptionBoard::default();
    // 40 graphemes at 17 per second: held to 2.353 s although planned for 400 ms.
    let text = "abcdefghij abcdefghij abcdefghij abcdefg";
    let mut guard = cue(1, text, 0, 400);
    guard.speaker = Some("guard".into());
    board.push(guard, 0, &opts);
    let mut pilot = cue(2, text, 0, 400);
    pilot.speaker = Some("pilot".into());
    board.push(pilot, 0, &opts);
    step(&mut board, 0, &opts);
    step(&mut board, 1000, &opts);
    assert_eq!(state(&board, 1), Some(EntryState::Visible));
    // The guard's next piece starts at 1.2 s.
    let mut next = cue(3, "next", 1200, 2000);
    next.speaker = Some("guard".into());
    board.push(next, 1000 * MS, &opts);
    step(&mut board, 1199, &opts);
    assert_eq!(state(&board, 1), Some(EntryState::Visible));
    step(&mut board, 1200, &opts);
    assert_eq!(
        state(&board, 1),
        Some(EntryState::Done),
        "the guard's row is held past the start of the guard's next piece"
    );
    assert_eq!(
        state(&board, 2),
        Some(EntryState::Visible),
        "another speaker keeps its hold"
    );
    assert_eq!(state(&board, 3), Some(EntryState::Visible));
    // The cap never cuts a row before its planned end or its minimum display time.
    let mut board = CaptionBoard::default();
    let mut early = cue(1, text, 0, 400);
    early.speaker = Some("guard".into());
    board.push(early, 0, &opts);
    let mut soon = cue(2, "soon", 300, 2000);
    soon.speaker = Some("guard".into());
    board.push(soon, 0, &opts);
    step(&mut board, 0, &opts);
    step(&mut board, 999, &opts);
    assert_eq!(
        state(&board, 1),
        Some(EntryState::Visible),
        "min_display still holds"
    );
    step(&mut board, 1000, &opts);
    assert_eq!(state(&board, 1), Some(EntryState::Done));
}

/// Spec 56 § 2.1/§ 2.8: `Speaker.side` puts the label on the reading-start or the reading-end side
/// of the row; the row view, the snapshot and the row text all follow it.
#[test]
fn the_label_side_follows_the_speaker() {
    let opts = CaptionOptions {
        speaker_labels: SpeakerLabels::Always,
        ..CaptionOptions::default()
    };
    let mut board = CaptionBoard::default();
    board.set_speaker(
        "guard",
        SpeakerStyle {
            name: "Guard".into(),
            color: None,
            side: LabelSide::End,
        },
    );
    board.set_speaker(
        "pilot",
        SpeakerStyle {
            name: "Pilot".into(),
            color: None,
            side: LabelSide::Start,
        },
    );
    let mut guard = cue(1, "halt", 0, 5000);
    guard.speaker = Some("guard".into());
    board.push(guard, 0, &opts);
    let mut pilot = cue(2, "roger", 0, 5000);
    pilot.speaker = Some("pilot".into());
    board.push(pilot, 0, &opts);
    step(&mut board, 0, &opts);
    let rows = board.visible_rows(&opts);
    assert_eq!(rows.len(), 2);
    assert_eq!((rows[0].id, rows[0].label_side), (CaptionId(1), LabelSide::End));
    assert_eq!((rows[1].id, rows[1].label_side), (CaptionId(2), LabelSide::Start));
    let snap = board.snapshot();
    assert_eq!(snap.entries[0]["label_side"], json!("End"));
    assert_eq!(snap.entries[1]["label_side"], json!("Start"));
    // The row text: the label span comes after the text at the end side, before it at the start.
    let style = crate::text::TextStyle::default();
    let end = super::rows::row_text(&rows[0], &style, None);
    let start = super::rows::row_text(&rows[1], &style, None);
    let lit = |span: &crate::text::TextSpan| match &span.content {
        crate::text::TextContent::Lit(text) => text.clone(),
        _ => String::new(),
    };
    assert_eq!(lit(&end.spans[0]), "halt");
    assert!(lit(&end.spans[1]).ends_with("Guard"), "{:?}", lit(&end.spans[1]));
    assert!(lit(&start.spans[0]).starts_with("Pilot"));
    assert_eq!(lit(&start.spans[1]), "roger");
}

/// Spec 56 § 2.8: with `honor_newlines` on, a literal newline in a translator-supplied (Fluent)
/// message is an author break, like `keep_breaks`; game literals are still flattened, and with the
/// option off the message is flattened too.
#[test]
fn honor_newlines_keeps_the_breaks_of_translated_messages_only() {
    use regia_app::caption::CaptionText;

    let loc = Localization::new(
        vec![("en".into(), "two-lines = First line\n    second line\n".into())],
        None,
    )
    .expect("the test bundle parses");
    let keyed = |id: u64| {
        let mut c = cue(id, "", 0, 5000);
        c.pieces = vec![CaptionPiece {
            offset_us: 0,
            text: CaptionText::Key {
                key: "two-lines".into(),
                args: Vec::new(),
            },
        }];
        c
    };
    let opts = CaptionOptions::default();
    for honor in [true, false] {
        let mut board = CaptionBoard::default();
        board.policy.honor_newlines = honor;
        let mut translated = keyed(1);
        resolve_pieces_honoring_newlines(&mut translated, &loc, board.policy.honor_newlines);
        board.push(translated, 0, &opts);
        let mut literal = cue(2, "game\nliteral", 0, 5000);
        resolve_pieces_honoring_newlines(&mut literal, &loc, board.policy.honor_newlines);
        board.push(literal, 0, &opts);
        step(&mut board, 0, &opts);
        let translated_lines = board.lines_of(CaptionId(1)).map(<[String]>::to_vec);
        let expected: Vec<String> = if honor {
            vec!["First line".into(), "second line".into()]
        } else {
            vec!["First line second line".into()]
        };
        assert_eq!(translated_lines, Some(expected), "honor_newlines = {honor}");
        assert_eq!(
            board.lines_of(CaptionId(2)).map(<[String]>::to_vec),
            Some(vec!["game literal".to_string()]),
            "a game literal is flattened (honor_newlines = {honor})"
        );
    }
}

/// A subtitle line with a huge finite duration passes validation and saturates its planned time
/// to `u64::MAX`: the karaoke page window then saturates too instead of overflowing (a panic with
/// overflow checks, a wrapped window without).
#[test]
fn a_page_window_of_a_saturated_duration_saturates() {
    let opts = CaptionOptions::default();
    let mut board = CaptionBoard::new(CaptionPolicy::default());
    let mut long = cue(1, "a very long line", 5_000, 0);
    long.planned_us = u64::MAX;
    board.push(long, 0, &opts);
    step(&mut board, 5_000, &opts);
    assert_eq!(state(&board, 1), Some(EntryState::Visible));
    let (from, to) = board.page_window(CaptionId(1)).expect("held");
    assert_eq!(from, 5_000 * MS);
    assert_eq!(to, u64::MAX);
}
