// SPDX-License-Identifier: MPL-2.0
//! Test-quality lane (ADR-1743): edge cases the pure-logic corners of the audio crate had no test
//! for. Fixed inputs, no wall clock, no device.

use std::collections::HashMap;
use std::time::Duration;

use regia_audio::{
    BarGate, DuckEngine, DuckRule, GroundMaterial, LoudnessError, LoudnessMeter, MusicStem, RenderStats,
    Spreading, ground_arrival, interface_transmission, measure_loudness, normalize_gain,
};

fn rule(amount_db: f32, attack_ms: u64, release_ms: u64) -> DuckRule {
    DuckRule {
        when: "Voice".into(),
        target: "Music".into(),
        amount_db,
        attack: Duration::from_millis(attack_ms),
        release: Duration::from_millis(release_ms),
    }
}

fn voice_active() -> HashMap<String, u32> {
    HashMap::from([("Voice".to_string(), 1)])
}

#[test]
fn a_non_finite_or_negative_step_does_not_poison_a_duck_envelope() {
    let rules = [rule(-6.0, 100, 100)];
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0] {
        let mut engine = DuckEngine::new(&rules);
        engine.tick(0.05, &rules, &voice_active());
        let before = engine.envelope(0);
        engine.tick(bad, &rules, &voice_active());
        assert!(engine.envelope(0).is_finite(), "step {bad}");
        assert!(
            (engine.envelope(0) - before).abs() < 1e-6 || bad == f64::INFINITY,
            "a bad step ({bad}) holds the envelope, got {} from {before}",
            engine.envelope(0)
        );
        // The engine still works afterwards.
        let offsets = engine.tick(1.0, &rules, &voice_active());
        assert!((offsets["Music"] + 6.0).abs() < 1e-5, "step {bad}: {offsets:?}");
        let offsets = engine.tick(1.0, &rules, &HashMap::new());
        assert!(offsets.is_empty(), "step {bad}: released, got {offsets:?}");
    }
}

#[test]
fn a_zero_step_holds_every_envelope() {
    let rules = [rule(-6.0, 100, 100)];
    let mut engine = DuckEngine::new(&rules);
    engine.tick(0.05, &rules, &voice_active());
    let held = engine.envelope(0);
    engine.tick(0.0, &rules, &voice_active());
    engine.tick(0.0, &rules, &HashMap::new());
    assert_eq!(engine.envelope(0), held);
}

#[test]
fn bar_gate_handles_a_huge_tick_jump_without_walking_every_tick() {
    let mut gate = BarGate::default();
    assert!(gate.crossed_boundary(u64::MAX, 4));
    assert!(!gate.crossed_boundary(u64::MAX, 4));
}

#[test]
fn bar_gate_fires_for_a_jump_that_spans_a_boundary_and_not_for_one_that_does_not() {
    let mut gate = BarGate::default();
    assert!(!gate.crossed_boundary(2, 4));
    assert!(gate.crossed_boundary(5, 4), "3 -> 5 passes the boundary at 4");
    assert!(!gate.crossed_boundary(7, 4), "5 -> 7 passes none");
    assert!(gate.crossed_boundary(8, 4), "lands exactly on a boundary");
    // beats_per_bar 0 behaves like 1: every tick is a boundary.
    assert!(gate.crossed_boundary(9, 0));
    assert!(!gate.crossed_boundary(9, 0));
}

#[test]
fn bar_gate_ignores_a_clock_that_goes_backwards() {
    let mut gate = BarGate::default();
    assert!(gate.crossed_boundary(8, 4));
    assert!(!gate.crossed_boundary(3, 4), "a rewound clock fires nothing");
    assert!(
        !gate.crossed_boundary(8, 4),
        "and does not re-fire the boundary it already passed"
    );
    assert!(gate.crossed_boundary(12, 4));
}

#[test]
fn a_nan_intensity_reads_as_the_lowest_intensity() {
    let stem = MusicStem {
        name: "drums".into(),
        asset: test_asset(),
        base_db: 0.0,
        intensity: vec![(0.0, -18.0), (1.0, 0.0)],
    };
    assert_eq!(stem.offset_db_at(f32::NAN), -18.0);
    assert_eq!(stem.offset_db_at(f32::NEG_INFINITY), -18.0);
    assert_eq!(stem.offset_db_at(f32::INFINITY), 0.0);
}

fn test_asset() -> regia_audio::SoundAsset {
    regia_audio::SoundAsset::from_clip(
        &regia_audio::SoundClip::new(vec![0.0; 16], 48_000).expect("a valid clip"),
    )
}

#[test]
fn ground_arrival_stays_finite_for_degenerate_distances() {
    let ice = GroundMaterial::named("ice").expect("ice");
    for distance in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.0, -5.0] {
        for amplitude in [1.0, f32::NAN, -1.0, 0.0] {
            let a = ground_arrival(&ice, distance, Spreading::Surface, 1.0, amplitude);
            assert!(
                a.amplitude.is_finite() && a.amplitude >= 0.0,
                "distance {distance}, amplitude {amplitude}: {a:?}"
            );
            assert!(
                a.felt.is_finite() && a.felt >= 0.0,
                "{distance} {amplitude}: {a:?}"
            );
            assert!(a.cutoff_hz.is_finite() && a.coda_secs.is_finite() && a.coda_mix.is_finite());
            assert!(a.delay_secs >= 0.0 && !a.delay_secs.is_nan(), "{distance}: {a:?}");
        }
    }
}

#[test]
fn interface_transmission_is_a_fraction_even_for_extreme_impedances() {
    for (z1, z2) in [
        (f32::INFINITY, 1.0),
        (f32::INFINITY, f32::INFINITY),
        (f32::NAN, 1.0),
        (-1.0, 5.0),
        (0.0, 0.0),
        (f32::MAX, f32::MAX),
    ] {
        let t = interface_transmission(z1, z2);
        assert!((0.0..=1.0).contains(&t), "({z1}, {z2}) -> {t}");
    }
}

#[test]
fn render_stats_of_nothing_and_of_an_odd_tail() {
    let empty = RenderStats::measure(&[]);
    assert_eq!(empty.frames, 0);
    assert!(empty.is_clean());
    assert_eq!(empty.peak_db(), -100.0);
    let odd = RenderStats::measure(&[0.5, -0.5, 0.9]);
    assert_eq!(odd.frames, 1, "the trailing odd sample is ignored");
    assert_eq!(odd.peak_left, 0.5);
}

#[test]
fn render_stats_counts_non_finite_samples_without_letting_them_into_peaks_or_steps() {
    let stats = RenderStats::measure(&[0.1, 0.1, f32::NAN, f32::INFINITY, 0.1, 0.1]);
    assert_eq!(stats.nonfinite_samples, 2);
    assert!(!stats.is_clean());
    assert_eq!(stats.peak_left, 0.1);
    assert_eq!(stats.max_step, 0.0);
    // A non-finite first frame has nothing to hold: it reads as silence.
    let first = RenderStats::measure(&[f32::NAN, f32::NAN, 0.2, 0.2]);
    assert_eq!(first.nonfinite_samples, 2);
    assert!(first.max_step <= 0.2 + 1e-6);
    assert!(first.rms_left.is_finite() && first.rms_right.is_finite());
}

#[test]
fn loudness_input_errors_are_typed() {
    assert_eq!(measure_loudness(&[], 2, 48_000).err(), Some(LoudnessError::Empty));
    assert_eq!(
        measure_loudness(&[0.1], 3, 48_000).err(),
        Some(LoudnessError::UnsupportedChannels(3))
    );
    assert_eq!(
        measure_loudness(&[0.1], 0, 48_000).err(),
        Some(LoudnessError::UnsupportedChannels(0))
    );
    assert_eq!(
        measure_loudness(&[0.1], 1, 0).err(),
        Some(LoudnessError::ZeroSampleRate)
    );
    assert!(!LoudnessError::Empty.to_string().is_empty());
}

#[test]
fn loudness_meter_ignores_a_trailing_partial_frame_and_pushes_chunk_independently() {
    let rate = 48_000u32;
    let samples: Vec<f32> = (0..rate as usize * 2)
        .flat_map(|i| {
            let s = ((i as f32 / rate as f32) * 997.0 * std::f32::consts::TAU).sin() * 0.25;
            [s, s * 0.5]
        })
        .collect();
    let whole = measure_loudness(&samples, 2, rate).expect("valid");
    let mut chunked = LoudnessMeter::new(2, rate).expect("valid");
    // Uneven chunk sizes that all hold whole frames.
    let mut rest = &samples[..];
    for len in [2usize, 1_000, 14, 9_998].iter().cycle() {
        if rest.is_empty() {
            break;
        }
        let n = (*len).min(rest.len());
        chunked.push(&rest[..n]);
        rest = &rest[n..];
    }
    assert_eq!(
        chunked.finish(),
        whole,
        "chunking does not change the measurement"
    );
    let mut trailing = LoudnessMeter::new(2, rate).expect("valid");
    trailing.push(&samples);
    trailing.push(&[0.9]);
    assert_eq!(trailing.finish(), whole, "a dangling half frame is ignored");
}

#[test]
fn loudness_of_silence_and_of_non_finite_input_is_finite_where_documented() {
    let silence = measure_loudness(&vec![0.0; 96_000], 1, 48_000).expect("valid");
    assert_eq!(silence.integrated_lufs, f32::NEG_INFINITY);
    assert_eq!(silence.loudness_range_lu, 0.0);
    assert_eq!(silence.true_peak_db, -100.0);
    let mut noisy = vec![0.2f32; 96_000];
    noisy[10] = f32::NAN;
    noisy[11] = f32::INFINITY;
    let loud = measure_loudness(&noisy, 1, 48_000).expect("valid");
    assert!(loud.integrated_lufs.is_finite(), "{loud:?}");
    assert!(loud.true_peak_db.is_finite());
}

#[test]
fn normalize_gain_clamps_and_survives_non_finite() {
    assert_eq!(normalize_gain(f32::NEG_INFINITY, -16.0), 1.0);
    assert_eq!(normalize_gain(-23.0, f32::NAN), 1.0);
    let max = normalize_gain(-200.0, 0.0);
    let min = normalize_gain(0.0, -200.0);
    assert!((max * min - 1.0).abs() < 1e-4, "symmetric clamp: {max} * {min}");
    assert!(max.is_finite() && max > 1.0 && min < 1.0);
    assert!((normalize_gain(-16.0, -16.0) - 1.0).abs() < 1e-6);
}

// ---------------------------------------------------------------- announcer

use regia_audio::{AnnounceOutcome, AnnouncePriority, Announcer, AnnouncerBank, SoundAsset, SoundClip};
use regia_core::math::{Rng, Vec3};

fn line() -> SoundAsset {
    SoundAsset::from_clip(&SoundClip::new(vec![0.1; 480], 48_000).expect("clip"))
}

#[test]
fn a_single_line_bank_repeats_and_a_pair_alternates() {
    let mut rng = Rng::new(5);
    let mut one = AnnouncerBank::new(vec![line()], AnnouncePriority::Normal);
    assert_eq!(one.pick(&mut rng), Some(0));
    assert_eq!(one.pick(&mut rng), Some(0), "nothing else to pick");
    let mut two = AnnouncerBank::new(vec![line(), line()], AnnouncePriority::Normal);
    let picks: Vec<usize> = (0..8).filter_map(|_| two.pick(&mut rng)).collect();
    assert!(picks.windows(2).all(|w| w[0] != w[1]), "{picks:?}");
}

#[test]
fn the_same_seed_gives_the_same_rotation() {
    let run = |seed| {
        let mut rng = Rng::new(seed);
        let mut bank = AnnouncerBank::new((0..6).map(|_| line()).collect(), AnnouncePriority::Low);
        (0..64).filter_map(|_| bank.pick(&mut rng)).collect::<Vec<_>>()
    };
    assert_eq!(run(7), run(7));
    assert_ne!(run(7), run(8), "a different seed picks a different order");
}

#[test]
fn a_low_line_is_rejected_when_it_is_the_low_line_already_playing() {
    let mut announcer = Announcer::new();
    announcer.set_bank_priority("flavour", AnnouncePriority::Low);
    announcer.note_started("flavour");
    assert_eq!(announcer.decide("flavour"), AnnounceOutcome::Rejected);
    assert_eq!(announcer.current_line(), Some("flavour"));
    assert_eq!(announcer.queued(), None);
}

#[test]
fn a_preempting_line_leaves_the_queued_low_line_for_later() {
    let mut announcer = Announcer::new();
    announcer.set_bank_priority("low", AnnouncePriority::Low);
    announcer.set_bank_priority("normal", AnnouncePriority::Normal);
    announcer.set_bank_priority("crit", AnnouncePriority::Critical);
    announcer.note_started("normal");
    assert_eq!(announcer.decide("low"), AnnounceOutcome::Queued);
    announcer.note_queued("low");
    assert_eq!(announcer.decide("crit"), AnnounceOutcome::Preempted);
    announcer.note_started("crit");
    assert_eq!(
        announcer.queued(),
        Some("low"),
        "pre-emption does not drop the queue"
    );
    assert_eq!(announcer.note_finished(), Some("low".to_string()));
    assert_eq!(announcer.queued(), None);
    assert!(!announcer.is_active());
    assert_eq!(announcer.current_line(), None);
}

#[test]
fn changing_a_bank_priority_applies_to_the_next_decision() {
    let mut announcer = Announcer::new();
    announcer.set_bank_priority("a", AnnouncePriority::Normal);
    announcer.set_bank_priority("b", AnnouncePriority::Normal);
    announcer.note_started("a");
    assert_eq!(announcer.decide("b"), AnnounceOutcome::Rejected);
    announcer.set_bank_priority("b", AnnouncePriority::High);
    assert_eq!(announcer.decide("b"), AnnounceOutcome::Preempted);
    assert_eq!(announcer.bank_priority("b"), Some(AnnouncePriority::High));
    assert_eq!(announcer.bank_priority("missing"), None);
}

// ---------------------------------------------------------------- spatial helpers

use regia_audio::{
    ListenerFrame, MotionTracker, SPEED_OF_SOUND, cone_gain, direction_pan, doppler_ratio,
    doppler_ratio_moving, nearest_listener,
};

#[test]
fn nearest_listener_picks_the_first_of_equals_and_ignores_nan_listeners() {
    assert_eq!(nearest_listener(&[], Vec3::ZERO), None);
    let at = |x: f32| ListenerFrame::looking_at(Vec3::new(x, 0.0, 0.0), Vec3::NEG_Z);
    assert_eq!(nearest_listener(&[at(-2.0), at(2.0)], Vec3::ZERO), Some(0));
    assert_eq!(nearest_listener(&[at(f32::NAN), at(5.0)], Vec3::ZERO), Some(1));
    assert_eq!(
        nearest_listener(&[at(9.0), at(3.0), at(4.0)], Vec3::ZERO),
        Some(1)
    );
}

#[test]
fn doppler_stays_in_range_for_extreme_inputs() {
    for closing in [
        0.0,
        1.0e9,
        -1.0e9,
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::NAN,
        SPEED_OF_SOUND,
    ] {
        let ratio = doppler_ratio(closing, 1.0);
        assert!(ratio.is_finite() && ratio > 0.0, "closing {closing}: {ratio}");
        let both = doppler_ratio_moving(closing, closing, 1.0);
        assert!(
            both.is_finite() && (0.5..=4.0).contains(&both),
            "moving {closing}: {both}"
        );
    }
    assert_eq!(doppler_ratio(30.0, 0.0), 1.0, "factor 0 disables the shift");
    assert_eq!(doppler_ratio(30.0, -1.0), 1.0, "a negative factor is no shift");
}

#[test]
fn cone_gain_is_finite_for_degenerate_directions() {
    for (forward, to_listener) in [
        (Vec3::ZERO, Vec3::X),
        (Vec3::X, Vec3::ZERO),
        (Vec3::NAN, Vec3::X),
        (Vec3::X, Vec3::NAN),
        (Vec3::INFINITY, Vec3::X),
    ] {
        let gain = cone_gain(forward, to_listener, 30.0, 90.0, 0.25);
        if forward == Vec3::ZERO {
            assert_eq!(gain, 1.0, "no forward axis means omnidirectional");
        }
        assert!(gain.is_finite(), "{forward:?} {to_listener:?}: {gain}");
        assert!(
            (0.25..=1.0).contains(&gain),
            "{forward:?} {to_listener:?}: {gain}"
        );
    }
}

#[test]
fn pan_of_a_zero_or_nan_direction_is_centred() {
    assert_eq!(direction_pan(Vec3::ZERO, Vec3::X), 0.0);
    assert_eq!(direction_pan(Vec3::NAN, Vec3::X), 0.0);
    assert_eq!(direction_pan(Vec3::X, Vec3::NAN), 0.0);
    assert_eq!(direction_pan(Vec3::X * 1.0e15, Vec3::X), 1.0);
}

#[test]
fn motion_tracker_recovers_from_a_nan_position_and_forgets_the_unseen() {
    use regia_ecs::ecs::entity::EntityIndex;
    use regia_ecs::prelude::Entity;
    let entity =
        |index: u32| Entity::from_index(EntityIndex::from_raw_u32(index).expect("a valid entity index"));
    let mut tracker = MotionTracker::default();
    let (a, b) = (entity(1), entity(2));
    assert_eq!(tracker.update(a, Vec3::ZERO, 0.016), Vec3::ZERO, "first sighting");
    let moving = tracker.update(a, Vec3::new(1.0, 0.0, 0.0), 0.016);
    assert!(moving.x > 0.0 && moving.is_finite());
    assert_eq!(
        tracker.update(a, Vec3::NAN, 0.016),
        Vec3::ZERO,
        "NaN position: no velocity"
    );
    let after = tracker.update(a, Vec3::new(1.0, 0.0, 0.0), 0.016);
    assert!(after.is_finite(), "the tracker recovers: {after:?}");
    // A teleport faster than twice the speed of sound is not a pitch spike.
    assert_eq!(tracker.update(a, Vec3::new(1.0e6, 0.0, 0.0), 0.016), Vec3::ZERO);
    // Non-positive and NaN steps keep the velocity.
    let held = tracker.update(a, Vec3::new(1.0e6, 0.0, 0.0), 0.016);
    assert_eq!(tracker.update(a, Vec3::new(1.0e6, 5.0, 0.0), 0.0), held);
    assert_eq!(tracker.update(a, Vec3::new(1.0e6, 5.0, 0.0), f32::NAN), held);
    // retain_seen drops entities not updated since the last call.
    tracker.update(b, Vec3::ZERO, 0.016);
    tracker.retain_seen();
    tracker.update(b, Vec3::new(0.5, 0.0, 0.0), 0.016);
    tracker.retain_seen();
    assert_eq!(
        tracker.update(a, Vec3::new(9.0, 0.0, 0.0), 0.016),
        Vec3::ZERO,
        "a was forgotten: first sighting again"
    );
}

// ---------------------------------------------------------------- cues

use regia_audio::{
    AudioError, AudioPatchDef, CUE_VERSION, CueEntry, CuePlayMode, CueSelector, PATCH_VERSION, PatchParamDef,
    PlaybackParams, SmoothedParam, SoundCue, SoundCueDef, cue_event_seed,
};

fn cue_of(count: usize, mode: CuePlayMode, no_repeat: usize) -> SoundCue {
    let entries = (0..count)
        .map(|_| CueEntry {
            asset: line(),
            weight: 1.0,
        })
        .collect();
    let mut cue = SoundCue::new("c", entries);
    cue.mode = mode;
    cue.no_repeat_last = no_repeat;
    cue
}

#[test]
fn a_sequential_cue_cycles_in_order_and_a_shuffle_cue_plays_each_entry_once_per_round() {
    let mut rng = Rng::new(3);
    let cue = cue_of(4, CuePlayMode::Sequential, 0);
    let mut selector = CueSelector::new(&cue);
    let picks: Vec<usize> = (0..9).map(|_| selector.pick(&mut rng)).collect();
    assert_eq!(picks, [0, 1, 2, 3, 0, 1, 2, 3, 0]);

    let cue = cue_of(5, CuePlayMode::Shuffle, 0);
    let mut selector = CueSelector::new(&cue);
    for round in 0..4 {
        let mut round_picks: Vec<usize> = (0..5).map(|_| selector.pick(&mut rng)).collect();
        round_picks.sort_unstable();
        assert_eq!(round_picks, [0, 1, 2, 3, 4], "round {round}");
    }
}

#[test]
fn no_repeat_bigger_than_the_cue_still_terminates_and_never_repeats_back_to_back() {
    for mode in [CuePlayMode::Random, CuePlayMode::Shuffle, CuePlayMode::Sequential] {
        for len in 1..=4usize {
            let cue = cue_of(len, mode, 99);
            let mut selector = CueSelector::new(&cue);
            let mut rng = Rng::new(11);
            let picks: Vec<usize> = (0..40).map(|_| selector.pick(&mut rng)).collect();
            assert!(picks.iter().all(|&p| p < len), "{mode:?} len {len}: {picks:?}");
            if len > 1 {
                assert!(
                    picks.windows(2).all(|w| w[0] != w[1]),
                    "{mode:?} len {len}: {picks:?}"
                );
            }
        }
    }
}

#[test]
fn zero_and_negative_weights_fall_back_to_a_uniform_pick() {
    let mut cue = cue_of(3, CuePlayMode::Random, 0);
    for entry in &mut cue.entries {
        entry.weight = 0.0;
    }
    cue.entries[1].weight = -4.0;
    cue.entries[2].weight = f32::NAN;
    let mut selector = CueSelector::new(&cue);
    let mut rng = Rng::new(2);
    let mut seen = [0usize; 3];
    for _ in 0..300 {
        seen[selector.pick(&mut rng)] += 1;
    }
    assert!(seen.iter().all(|&n| n > 40), "{seen:?}");
}

#[test]
fn a_weighted_cue_never_picks_a_zero_weight_entry_when_others_have_weight() {
    let mut cue = cue_of(3, CuePlayMode::Random, 0);
    cue.entries[0].weight = 0.0;
    cue.entries[1].weight = 1.0;
    cue.entries[2].weight = 3.0;
    let mut selector = CueSelector::new(&cue);
    let mut rng = Rng::new(21);
    let picks: Vec<usize> = (0..400).map(|_| selector.pick(&mut rng)).collect();
    assert!(!picks.contains(&0));
    let twos = picks.iter().filter(|&&p| p == 2).count();
    assert!((250..350).contains(&twos), "about 3/4 of the picks: {twos}");
}

#[test]
fn variation_with_a_non_finite_or_inverted_range_leaves_the_params_alone() {
    let mut cue = cue_of(1, CuePlayMode::Random, 0);
    let mut rng = Rng::new(1);
    for (pitch, volume) in [
        ((f32::NAN, f32::NAN), (f32::NAN, f32::NAN)),
        ((2.0, 0.5), (3.0, -3.0)),
        ((f32::INFINITY, f32::INFINITY), (f32::NEG_INFINITY, f32::INFINITY)),
    ] {
        cue.pitch_range = pitch;
        cue.volume_range_db = volume;
        let selector = CueSelector::new(&cue);
        let mut params = PlaybackParams::default();
        let before = (params.pitch, params.volume_db);
        selector.vary(&mut params, &mut rng);
        assert_eq!((params.pitch, params.volume_db), before, "{pitch:?} {volume:?}");
    }
}

#[test]
fn a_cue_filter_cutoff_is_clamped_and_non_finite_is_open() {
    let mut cue = cue_of(1, CuePlayMode::Random, 0);
    assert_eq!(cue.effective_filter_cutoff_hz(), None);
    cue.filter_cutoff_hz = Some(1_000.0);
    assert_eq!(cue.effective_filter_cutoff_hz(), Some(1_000.0));
    cue.filter_offset_octaves = 1.0;
    assert_eq!(cue.effective_filter_cutoff_hz(), Some(2_000.0));
    cue.filter_offset_octaves = -20.0;
    assert_eq!(cue.effective_filter_cutoff_hz(), Some(20.0), "floor");
    cue.filter_offset_octaves = 20.0;
    assert_eq!(cue.effective_filter_cutoff_hz(), Some(20_000.0), "ceiling");
    cue.filter_offset_octaves = f32::NAN;
    assert_eq!(cue.effective_filter_cutoff_hz(), Some(20_000.0));
    cue.filter_cutoff_hz = Some(f32::NAN);
    assert_eq!(cue.effective_filter_cutoff_hz(), Some(20_000.0));
    cue.filter_cutoff_hz = None;
    cue.filter_offset_octaves = -1.0;
    assert_eq!(
        cue.effective_filter_cutoff_hz(),
        Some(10_000.0),
        "an offset alone starts open"
    );
}

#[test]
fn cue_event_seeds_depend_on_both_arguments_in_order() {
    assert_ne!(cue_event_seed(1, 2), cue_event_seed(2, 1));
    assert_ne!(cue_event_seed(0, 0), cue_event_seed(0, 1));
    assert_ne!(cue_event_seed(0, 0), cue_event_seed(1, 0));
    // Pinned values: the seed is part of replay determinism and must not drift across platforms.
    assert_eq!(cue_event_seed(0, 0), 0xe220_a839_7b1d_cdaf);
    assert_eq!(cue_event_seed(1, 2), 0x5a1e_518f_cfcf_0d64);
    let distinct: std::collections::HashSet<u64> =
        (0..1_000u64).map(|tick| cue_event_seed(42, tick)).collect();
    assert_eq!(distinct.len(), 1_000, "no collisions over a run of ticks");
}

#[test]
fn cue_text_round_trips_and_migration_is_idempotent() {
    let text = r#"(name: "x", sounds: [(sound: "a.ogg"), (sound: "b.ogg", weight: 3.0)], mode: Shuffle, no_repeat_last: 1)"#;
    assert_eq!(SoundCueDef::peek_version(text).expect("peek"), 1);
    let migrated = SoundCueDef::migrate_ron(text).expect("migrate");
    let again = SoundCueDef::migrate_ron(&migrated).expect("migrate twice");
    assert_eq!(migrated, again, "migrating current text changes nothing");
    let def = SoundCueDef::from_ron(&migrated).expect("parse");
    assert_eq!(def.version, CUE_VERSION);
    assert_eq!(
        SoundCueDef::from_ron(&def.to_ron().expect("to ron")).expect("re-parse"),
        def
    );
}

#[test]
fn cue_text_errors_are_load_errors_not_panics() {
    for bad in [
        "",
        "(",
        "(name: 5)",
        "(version: \"two\", name: \"x\", sounds: [])",
        "\u{0}",
    ] {
        assert!(SoundCueDef::from_ron(bad).is_err(), "{bad:?}");
        assert!(
            SoundCueDef::peek_version(bad).is_err() || bad.starts_with("(name"),
            "{bad:?}"
        );
    }
}

#[test]
fn a_cue_without_sounds_or_with_an_unloadable_sound_does_not_build() {
    let empty = SoundCueDef::from_ron(r#"(name: "e", sounds: [])"#).expect("parses");
    let error = empty
        .into_cue(|_| Ok(line()))
        .map(|_| ())
        .expect_err("empty")
        .to_string();
    assert!(error.contains("no sounds") && error.contains('e'), "{error}");
    let def = SoundCueDef::from_ron(r#"(name: "m", sounds: [(sound: "a.ogg"), (sound: "gone.ogg")])"#)
        .expect("parses");
    let result = def.into_cue(|path| {
        if path == "gone.ogg" {
            Err(AudioError::UnknownEntry(path.to_string()))
        } else {
            Ok(line())
        }
    });
    assert!(matches!(result, Err(AudioError::UnknownEntry(p)) if p == "gone.ogg"));
}

// ---------------------------------------------------------------- patches

fn param(min: f32, max: f32, smoothing_secs: f32) -> PatchParamDef {
    PatchParamDef {
        name: "speed".into(),
        min,
        max,
        smoothing_secs,
        binding: None,
    }
}

fn smoothed(def: PatchParamDef) -> SmoothedParam {
    SmoothedParam {
        value: def.min,
        target: def.min,
        def,
    }
}

#[test]
fn patch_version_zero_and_missing_version_are_errors_not_panics() {
    let zero = r#"(version: 0, name: "p", parameters: [], layers: [])"#;
    assert!(
        AudioPatchDef::from_ron(zero).is_err(),
        "no migrator exists for version 0"
    );
    let missing = r#"(name: "p", parameters: [], layers: [])"#;
    assert!(AudioPatchDef::from_ron(missing).is_err());
    let ok = format!(r#"(version: {PATCH_VERSION}, name: "p", parameters: [], layers: [])"#);
    assert_eq!(AudioPatchDef::from_ron(&ok).expect("v1").version, PATCH_VERSION);
}

#[test]
fn a_smoothed_param_ignores_a_non_finite_or_negative_step() {
    let mut p = smoothed(param(0.0, 10.0, 1.0));
    p.set_target(10.0);
    assert!(p.tick(0.1));
    let held = p.value;
    for bad in [f64::NAN, -1.0, f64::NEG_INFINITY] {
        p.tick(bad);
        assert_eq!(p.value, held, "step {bad}");
    }
    p.tick(100.0);
    assert_eq!(p.value, 10.0, "the param still reaches its target");
}

#[test]
fn a_smoothed_param_clamps_targets_and_reports_a_change_once() {
    let mut p = smoothed(param(2.0, 4.0, 0.5));
    p.set_target(100.0);
    assert_eq!(p.target, 4.0);
    p.set_target(-100.0);
    assert_eq!(p.target, 2.0);
    p.set_target(f32::NAN);
    assert_eq!(p.target, 2.0, "NaN is ignored");
    p.set_target(f32::INFINITY);
    assert_eq!(p.target, 4.0);
    assert!(p.tick(10.0));
    assert!(!p.tick(10.0), "already there");
    assert_eq!(p.normalised(), 1.0);
    // Inverted bounds are read as a range, not a panic.
    let mut inverted = smoothed(param(5.0, 1.0, 0.1));
    inverted.set_target(3.0);
    assert_eq!(inverted.target, 3.0);
    // A degenerate range normalises to 0.
    assert_eq!(smoothed(param(1.0, 1.0, 0.1)).normalised(), 0.0);
}

#[test]
fn a_zero_smoothing_time_snaps_in_one_step() {
    let mut p = smoothed(param(0.0, 1.0, 0.0));
    p.set_target(1.0);
    assert!(p.tick(0.01));
    assert_eq!(p.value, 1.0);
}

// ---------------------------------------------------------------- bus layouts

use regia_audio::{Audio, AudioSettings, BusDef, BusLayout, SendBusDef};

fn bus(name: &str, parent: Option<&str>) -> BusDef {
    BusDef {
        name: name.into(),
        parent: parent.map(String::from),
        volume_db: 0.0,
        mute: false,
        effects: Vec::new(),
        sends: Vec::new(),
        bypass_medium: false,
    }
}

#[test]
fn a_layout_with_a_cycle_or_unknown_names_never_hangs_the_medium_lookup() {
    let layout = BusLayout {
        buses: vec![bus("A", Some("B")), bus("B", Some("A"))],
        send_buses: Vec::new(),
        ..BusLayout::default()
    };
    assert!(layout.validate().is_err(), "a forward parent is rejected");
    assert!(!layout.bypasses_medium("A"), "the bounded walk ends on a cycle");
    assert!(!layout.bypasses_medium("nowhere"));
    let mut radio = bus("Radio", Some("Master"));
    radio.bypass_medium = true;
    let tree = BusLayout {
        buses: vec![
            bus("Master", None),
            radio,
            bus("Chatter", Some("Radio")),
            bus("Sfx", Some("Master")),
        ],
        send_buses: Vec::new(),
        ..BusLayout::default()
    };
    tree.validate().expect("valid");
    assert!(
        tree.bypasses_medium("Radio") && tree.bypasses_medium("Chatter"),
        "inherited"
    );
    assert!(!tree.bypasses_medium("Sfx") && !tree.bypasses_medium("Master"));
}

#[test]
fn duplicate_names_and_dangling_sends_are_named_in_the_error() {
    let dup = BusLayout {
        buses: vec![bus("Master", None), bus("Master", None)],
        send_buses: Vec::new(),
        ..BusLayout::default()
    };
    assert!(dup.validate().expect_err("duplicate").contains("Master"));
    let mut sender = bus("Sfx", Some("Master"));
    sender.sends.push(regia_audio::BusSend::new("Echo", 0.0));
    let dangling = BusLayout {
        buses: vec![bus("Master", None), sender],
        send_buses: Vec::new(),
        ..BusLayout::default()
    };
    assert!(dangling.validate().expect_err("dangling").contains("Echo"));
    let with_send_bus = BusLayout {
        send_buses: vec![SendBusDef {
            name: "Echo".into(),
            effects: Vec::new(),
        }],
        ..dangling
    };
    with_send_bus.validate().expect("the send bus exists now");
}

#[test]
fn a_headless_audio_survives_odd_layouts() {
    // An empty layout and a layout of one bus must not panic; the system falls back or runs bare.
    for layout in [
        BusLayout::default(),
        BusLayout {
            buses: vec![bus("Master", None)],
            send_buses: Vec::new(),
            ..BusLayout::default()
        },
        // Invalid layout (duplicate): construction must still produce a usable Audio.
        BusLayout {
            buses: vec![bus("Master", None), bus("Master", None)],
            send_buses: Vec::new(),
            ..BusLayout::default()
        },
    ] {
        let mut audio = Audio::new(&AudioSettings {
            headless: true,
            buses: layout,
            ..AudioSettings::default()
        });
        audio.tick(1.0 / 60.0);
        let frames = audio.render_offline(800).expect("an offline render");
        assert!(frames.iter().all(|s| s.is_finite()));
    }
}

#[test]
fn a_bus_layout_from_bad_ron_is_an_error() {
    for bad in ["", "(", "(buses: 3)", "(buses: [(name: 5)])", "\u{1}\u{2}"] {
        assert!(BusLayout::from_ron(bad).is_err(), "{bad:?}");
    }
    let ok = BusLayout::from_ron(r#"(buses: [(name: "Master")])"#).expect("defaults fill in");
    assert_eq!(ok.buses[0].volume_db, 0.0);
    assert!(ok.send_buses.is_empty());
}
