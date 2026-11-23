// SPDX-License-Identifier: MPL-2.0
//! Public-API tests for the pure-logic corners of the audio crate: ducking envelopes, the snapshot
//! stack, clip validation, error messages, settings round trips, rolloff curves, cones, Doppler and
//! absorption. No device, no wall clock: fixed steps and exact inputs.

use std::collections::HashMap;
use std::time::Duration;

use regia_audio::{
    AudioError, AudioMedium, AudioSnapshot, AudioUserSettings, DuckEngine, DuckRule, MAX_CUTOFF_HZ,
    MIN_CUTOFF_HZ, RolloffCurve, SETTINGS_BUSES, SPEED_OF_SOUND, SnapshotStack, SoundClip, SoundClipError,
    absorption_cutoff, absorption_cutoff_hz, cone_gain, db_to_gain, default_duck_rules, default_voice_limit,
    direction_pan, distance_gain, doppler_ratio, doppler_ratio_in, doppler_ratio_moving,
    doppler_ratio_moving_in, gain_to_db,
};
use regia_core::math::Vec3;

fn rule(when: &str, target: &str, amount_db: f32, attack_ms: u64, release_ms: u64) -> DuckRule {
    DuckRule {
        when: when.into(),
        target: target.into(),
        amount_db,
        attack: Duration::from_millis(attack_ms),
        release: Duration::from_millis(release_ms),
    }
}

fn activity(source: &str, count: u32) -> HashMap<String, u32> {
    HashMap::from([(source.to_string(), count)])
}

// ---- ducking -------------------------------------------------------------------------------

#[test]
fn zero_attack_ducks_fully_in_one_tick_and_zero_release_recovers_in_one_tick() {
    let rules = [rule("Voice", "Music", -12.0, 0, 0)];
    let mut engine = DuckEngine::new(&rules);
    let ducked = engine.tick(0.001, &rules, &activity("Voice", 1));
    assert_eq!(ducked.get("Music").copied(), Some(-12.0));
    assert_eq!(engine.envelope(0), 1.0);
    let released = engine.tick(0.001, &rules, &HashMap::new());
    assert!(released.is_empty(), "a fully released rule contributes no offset");
    assert_eq!(engine.envelope(0), 0.0);
}

#[test]
fn partial_attack_scales_the_offset_linearly() {
    let rules = [rule("Voice", "Music", -10.0, 1000, 1000)];
    let mut engine = DuckEngine::new(&rules);
    let offsets = engine.tick(0.25, &rules, &activity("Voice", 3));
    let offset = offsets
        .get("Music")
        .copied()
        .expect("a ramping rule contributes an offset");
    assert!(
        (offset - -2.5).abs() < 1e-4,
        "quarter of the attack gives a quarter of -10 dB, got {offset}"
    );
    assert!((engine.envelope(0) - 0.25).abs() < 1e-5);
}

#[test]
fn envelope_never_leaves_zero_to_one_whatever_the_step() {
    let rules = [rule("Voice", "Music", -6.0, 100, 100)];
    let mut engine = DuckEngine::new(&rules);
    engine.tick(10.0, &rules, &activity("Voice", 1));
    assert_eq!(engine.envelope(0), 1.0, "a huge attack step clamps at 1");
    engine.tick(10.0, &rules, &HashMap::new());
    assert_eq!(engine.envelope(0), 0.0, "a huge release step clamps at 0");
}

#[test]
fn zero_count_activity_is_inactive_and_unknown_sources_are_ignored() {
    let rules = [rule("Voice", "Music", -6.0, 0, 0)];
    let mut engine = DuckEngine::new(&rules);
    assert!(engine.tick(0.1, &rules, &activity("Voice", 0)).is_empty());
    assert!(engine.tick(0.1, &rules, &activity("Sfx", 5)).is_empty());
    assert_eq!(engine.envelope(0), 0.0);
}

#[test]
fn envelope_of_an_out_of_range_rule_index_is_zero() {
    let engine = DuckEngine::new(&[rule("Voice", "Music", -6.0, 0, 0)]);
    assert_eq!(engine.envelope(7), 0.0);
    assert_eq!(DuckEngine::default().envelope(0), 0.0);
}

#[test]
fn a_rule_list_longer_than_the_engine_is_only_partly_driven() {
    // The engine is built for one rule; ticking with two must not panic and must drive only the
    // rule it has an envelope for.
    let one = [rule("Voice", "Music", -6.0, 0, 0)];
    let two = [one[0].clone(), rule("Voice", "Sfx", -3.0, 0, 0)];
    let mut engine = DuckEngine::new(&one);
    let offsets = engine.tick(0.1, &two, &activity("Voice", 1));
    assert!(offsets.contains_key("Music"));
    assert!(!offsets.contains_key("Sfx"));
}

#[test]
fn default_rules_have_negative_amounts_and_known_targets() {
    let rules = default_duck_rules();
    assert!(!rules.is_empty());
    for r in &rules {
        assert!(r.amount_db < 0.0, "rule {r:?} must reduce level");
        assert!(!r.when.is_empty() && !r.target.is_empty());
    }
}

// ---- snapshots -----------------------------------------------------------------------------

#[test]
fn pushing_an_unregistered_snapshot_is_an_error_and_leaves_the_stack_alone() {
    let mut stack = SnapshotStack::default();
    assert_eq!(
        stack.push("Nope"),
        Err(AudioError::UnknownSnapshot("Nope".into()))
    );
    assert_eq!(stack.depth(), 0);
    assert_eq!(stack.active(), None);
}

#[test]
fn popping_an_empty_stack_is_an_error() {
    let mut stack = SnapshotStack::default();
    assert!(matches!(stack.pop(), Err(AudioError::UnknownSnapshot(_))));
}

#[test]
fn re_pushing_a_snapshot_moves_it_to_the_top_without_growing_the_stack() {
    let mut stack = SnapshotStack::default();
    stack.register(AudioSnapshot::new("A"));
    stack.register(AudioSnapshot::new("B"));
    stack.push("A").expect("A is registered");
    stack.push("B").expect("B is registered");
    stack.push("A").expect("A is registered");
    assert_eq!(stack.depth(), 2);
    assert_eq!(stack.active(), Some("A"));
    assert_eq!(stack.pop().as_deref(), Ok("A"));
    assert_eq!(stack.active(), Some("B"));
}

#[test]
fn targets_fall_back_to_the_base_level_for_buses_the_snapshot_does_not_name() {
    let mut stack = SnapshotStack::default();
    stack.register(
        AudioSnapshot::new("Paused")
            .with_bus_volume("Music", -40.0)
            .with_filter_cutoff("Sfx", 800.0),
    );
    assert_eq!(
        stack.target_volume_db("Music", -3.0),
        -3.0,
        "no snapshot: base level"
    );
    assert_eq!(stack.target_filter_cutoff("Sfx"), None);
    stack.push("Paused").expect("registered");
    assert_eq!(stack.target_volume_db("Music", -3.0), -40.0);
    assert_eq!(
        stack.target_volume_db("Voice", -3.0),
        -3.0,
        "unnamed bus keeps its base"
    );
    assert_eq!(stack.target_filter_cutoff("Sfx"), Some(800.0));
    assert_eq!(stack.target_filter_cutoff("Music"), None);
    stack.pop().expect("one pushed");
    assert_eq!(
        stack.target_volume_db("Music", -3.0),
        -3.0,
        "popped: base level again"
    );
}

#[test]
fn registering_a_snapshot_twice_replaces_the_first() {
    let mut stack = SnapshotStack::default();
    stack.register(AudioSnapshot::new("S").with_bus_volume("Music", -10.0));
    stack.register(AudioSnapshot::new("S").with_bus_volume("Music", -20.0));
    stack.push("S").expect("registered");
    assert_eq!(stack.target_volume_db("Music", 0.0), -20.0);
}

// ---- clips, errors, settings ---------------------------------------------------------------

#[test]
fn clip_rejects_empty_samples_and_zero_rate_with_distinct_errors() {
    assert_eq!(
        SoundClip::new(Vec::<f32>::new(), 48_000).err(),
        Some(SoundClipError::Empty)
    );
    assert_eq!(
        SoundClip::new(vec![0.1], 0).err(),
        Some(SoundClipError::ZeroSampleRate)
    );
    // Both wrong: emptiness is reported first.
    assert_eq!(
        SoundClip::new(Vec::<f32>::new(), 0).err(),
        Some(SoundClipError::Empty)
    );
    assert_ne!(
        SoundClipError::Empty.to_string(),
        SoundClipError::ZeroSampleRate.to_string()
    );
}

#[test]
fn clip_keeps_its_samples_and_rate() {
    let clip = SoundClip::new(vec![0.5, -0.5, 0.25], 22_050).expect("valid");
    assert_eq!(clip.samples(), &[0.5, -0.5, 0.25]);
    assert_eq!(clip.len(), 3);
    assert!(!clip.is_empty());
    assert_eq!(clip.sample_rate(), 22_050);
}

#[test]
fn audio_errors_name_the_offending_entity_in_their_message() {
    assert!(
        AudioError::UnknownBus("Radio".into())
            .to_string()
            .contains("Radio")
    );
    assert!(
        AudioError::UnknownSnapshot("Paused".into())
            .to_string()
            .contains("Paused")
    );
    assert!(
        AudioError::UnknownEntry("boom".into())
            .to_string()
            .contains("boom")
    );
    assert!(AudioError::UnsupportedPatchVersion(9).to_string().contains('9'));
    let load = AudioError::LoadFailed {
        path: "sfx/a.ogg".into(),
        message: "bad header".into(),
    };
    let text = load.to_string();
    assert!(text.contains("sfx/a.ogg") && text.contains("bad header"));
    assert!(
        AudioError::PlayFailed("limit".into())
            .to_string()
            .contains("limit")
    );
    assert!(
        AudioError::Migration("step 2".into())
            .to_string()
            .contains("step 2")
    );
    assert!(
        AudioError::Capture("no mic".into())
            .to_string()
            .contains("no mic")
    );
    assert!(!AudioError::NeedsOffline.to_string().is_empty());
    assert!(!AudioError::EmptyClip.to_string().is_empty());
}

#[test]
fn user_settings_round_trip_through_ron() {
    let settings = AudioUserSettings {
        bus_volumes: HashMap::from([("Music".to_string(), -6.0), ("Sfx".to_string(), -1.5)]),
        muted: vec!["Voice".to_string()],
        ..AudioUserSettings::default()
    };
    let text = ron::ser::to_string(&settings).expect("serializes");
    let back: AudioUserSettings = ron::de::from_str(&text).expect("parses its own output");
    assert_eq!(back, settings);
}

#[test]
fn user_settings_default_is_empty_and_accepts_an_empty_ron_struct() {
    let back: AudioUserSettings = ron::de::from_str("(bus_volumes: {}, muted: [])").expect("parses");
    assert_eq!(back, AudioUserSettings::default());
}

#[test]
fn user_settings_reject_malformed_ron_instead_of_panicking() {
    assert!(ron::de::from_str::<AudioUserSettings>("(bus_volumes: 3").is_err());
}

#[test]
fn settings_buses_start_with_master_and_have_no_duplicates() {
    assert_eq!(SETTINGS_BUSES[0], "Master");
    for (i, a) in SETTINGS_BUSES.iter().enumerate() {
        for b in &SETTINGS_BUSES[i + 1..] {
            assert_ne!(a, b);
        }
    }
}

#[test]
fn default_voice_limit_is_a_platform_constant() {
    let limit = default_voice_limit();
    #[cfg(target_os = "android")]
    assert_eq!(limit, 32);
    #[cfg(not(target_os = "android"))]
    assert_eq!(limit, 64);
}

// ---- rolloff, cone, pan --------------------------------------------------------------------

#[test]
fn every_curve_is_silent_past_max_distance_and_unity_inside_min_distance() {
    let custom = RolloffCurve::Custom(vec![(1.0, 1.0), (50.0, 0.0)]);
    for curve in [
        RolloffCurve::Inverse,
        RolloffCurve::Linear,
        RolloffCurve::Log { db_per_doubling: 6.0 },
        custom,
    ] {
        assert_eq!(
            distance_gain(&curve, 1.0, 50.0, 50.001),
            0.0,
            "{curve:?} past max"
        );
        let near = distance_gain(&curve, 1.0, 50.0, 0.5);
        assert!(
            (near - 1.0).abs() < 1e-5,
            "{curve:?} inside min distance gave {near}"
        );
    }
}

#[test]
fn inverse_rolloff_halves_per_doubling_and_log_follows_its_slope() {
    let inverse = distance_gain(&RolloffCurve::Inverse, 1.0, 1000.0, 2.0);
    assert!((inverse - 0.5).abs() < 1e-5);
    let log = distance_gain(&RolloffCurve::Log { db_per_doubling: 6.0 }, 1.0, 1000.0, 4.0);
    assert!(
        (gain_to_db(log) - -12.0).abs() < 0.01,
        "two doublings at 6 dB each"
    );
}

#[test]
fn linear_rolloff_is_one_half_at_the_midpoint() {
    let mid = distance_gain(&RolloffCurve::Linear, 10.0, 30.0, 20.0);
    assert!((mid - 0.5).abs() < 1e-5);
}

#[test]
fn negative_distance_is_treated_as_zero() {
    let gain = distance_gain(&RolloffCurve::Inverse, 1.0, 10.0, -5.0);
    assert!((gain - 1.0).abs() < 1e-6);
}

#[test]
fn custom_curve_clamps_at_both_ends_and_an_empty_curve_is_silent() {
    let curve = RolloffCurve::Custom(vec![(5.0, 0.8), (10.0, 0.2)]);
    assert!(
        (distance_gain(&curve, 0.0, 100.0, 1.0) - 0.8).abs() < 1e-5,
        "before the first point"
    );
    assert!(
        (distance_gain(&curve, 0.0, 100.0, 7.5) - 0.5).abs() < 1e-5,
        "interpolated"
    );
    assert!(
        (distance_gain(&curve, 0.0, 100.0, 50.0) - 0.2).abs() < 1e-5,
        "after the last point"
    );
    assert_eq!(
        distance_gain(&RolloffCurve::Custom(Vec::new()), 0.0, 100.0, 5.0),
        0.0
    );
}

#[test]
fn db_gain_conversion_round_trips_and_floors_at_minus_100() {
    assert_eq!(db_to_gain(-100.0), 0.0);
    assert_eq!(db_to_gain(-250.0), 0.0);
    assert!((db_to_gain(0.0) - 1.0).abs() < 1e-6);
    assert!((db_to_gain(-6.0) - 0.501_187).abs() < 1e-4);
    assert_eq!(gain_to_db(0.0), -100.0);
    assert_eq!(gain_to_db(1e-7), -100.0);
    for db in [-60.0_f32, -20.0, -3.0, 0.0, 6.0] {
        assert!(
            (gain_to_db(db_to_gain(db)) - db).abs() < 1e-3,
            "round trip of {db}"
        );
    }
}

#[test]
fn cone_gain_is_unity_inside_outer_gain_outside_and_linear_between() {
    let forward = Vec3::new(0.0, 0.0, -1.0);
    let ahead = Vec3::new(0.0, 0.0, -1.0);
    let behind = Vec3::new(0.0, 0.0, 1.0);
    assert_eq!(cone_gain(forward, ahead, 30.0, 90.0, 0.2), 1.0);
    assert!((cone_gain(forward, behind, 30.0, 90.0, 0.2) - 0.2).abs() < 1e-5);
    // 60 degrees off axis: halfway between the 30 and 90 degree half-angles.
    let off = Vec3::new(60f32.to_radians().sin(), 0.0, -60f32.to_radians().cos());
    assert!((cone_gain(forward, off, 30.0, 90.0, 0.2) - 0.6).abs() < 1e-3);
}

#[test]
fn omnidirectional_cones_and_zero_forward_never_attenuate() {
    let to = Vec3::new(1.0, 0.0, 0.0);
    assert_eq!(
        cone_gain(Vec3::ZERO, to, 10.0, 20.0, 0.0),
        1.0,
        "no facing direction"
    );
    assert_eq!(
        cone_gain(Vec3::new(0.0, 0.0, -1.0), -to, 360.0, 360.0, 0.0),
        1.0,
        "full-sphere cone"
    );
}

#[test]
fn cone_with_outer_narrower_than_inner_does_not_blow_up() {
    let gain = cone_gain(
        Vec3::new(0.0, 0.0, -1.0),
        Vec3::new(0.0, 0.0, 1.0),
        90.0,
        10.0,
        0.3,
    );
    assert!(gain.is_finite());
    assert!(
        (gain - 0.3).abs() < 1e-5,
        "behind the source is outside the (collapsed) cone"
    );
}

#[test]
fn pan_is_the_projection_on_the_listener_right_axis_and_zero_on_the_listener() {
    let right = Vec3::new(1.0, 0.0, 0.0);
    assert!((direction_pan(Vec3::new(5.0, 0.0, 0.0), right) - 1.0).abs() < 1e-6);
    assert!((direction_pan(Vec3::new(-2.0, 0.0, 0.0), right) + 1.0).abs() < 1e-6);
    assert!(
        direction_pan(Vec3::new(0.0, 0.0, -4.0), right).abs() < 1e-6,
        "dead ahead"
    );
    assert_eq!(direction_pan(Vec3::ZERO, right), 0.0, "emitter on the listener");
}

#[test]
fn absorption_cutoff_ramps_from_20k_to_the_floor_and_clamps() {
    assert_eq!(absorption_cutoff(0.0, 100.0, 2000.0), 20_000.0);
    assert!((absorption_cutoff(100.0, 100.0, 2000.0) - 2000.0).abs() < 1e-3);
    assert!(
        (absorption_cutoff(500.0, 100.0, 2000.0) - 2000.0).abs() < 1e-3,
        "clamped past max"
    );
    let mid = absorption_cutoff(50.0, 100.0, 2000.0);
    assert!((mid - 11_000.0).abs() < 1.0);
    assert!(
        absorption_cutoff(10.0, 0.0, 2000.0).is_finite(),
        "zero max distance must not divide by zero"
    );
}

// ---- Doppler and the medium ----------------------------------------------------------------

#[test]
fn doppler_is_identity_at_rest_and_when_disabled() {
    assert_eq!(doppler_ratio(0.0, 1.0), 1.0);
    assert_eq!(doppler_ratio(50.0, 0.0), 1.0);
    assert_eq!(doppler_ratio(50.0, -1.0), 1.0);
}

#[test]
fn doppler_approach_raises_pitch_recession_lowers_it() {
    let approaching = doppler_ratio(34.33, 1.0);
    let receding = doppler_ratio(-34.33, 1.0);
    assert!((approaching - SPEED_OF_SOUND / (SPEED_OF_SOUND - 34.33)).abs() < 1e-4);
    assert!(approaching > 1.0 && receding < 1.0);
}

#[test]
fn doppler_is_clamped_and_survives_supersonic_closing_speed() {
    assert_eq!(doppler_ratio(SPEED_OF_SOUND * 10.0, 1.0), 4.0);
    assert_eq!(doppler_ratio(-SPEED_OF_SOUND * 100.0, 1.0), 0.5);
    let moving = doppler_ratio_moving(SPEED_OF_SOUND * 10.0, 0.0, 1.0);
    assert!(moving.is_finite() && (0.5..=4.0).contains(&moving));
}

#[test]
fn moving_doppler_is_symmetric_for_equal_closing_speeds_in_a_static_frame() {
    // A source closing at v with a static listener equals the static-listener formula.
    let both = doppler_ratio_moving(20.0, 0.0, 1.0);
    assert!((both - doppler_ratio(20.0, 1.0)).abs() < 1e-5);
    // A listener receding at v lowers the pitch by (c - v) / c.
    let receding = doppler_ratio_moving(0.0, 20.0, 1.0);
    assert!((receding - (SPEED_OF_SOUND - 20.0) / SPEED_OF_SOUND).abs() < 1e-5);
}

#[test]
fn a_soundless_medium_has_no_doppler_and_no_delay() {
    assert_eq!(doppler_ratio_in(0.0, 30.0, 1.0), 1.0);
    assert_eq!(doppler_ratio_moving_in(1.0, 30.0, 0.0, 1.0), 1.0);
    let vacuum = AudioMedium::airless();
    assert!(!vacuum.carries_sound());
    assert_eq!(vacuum.delay_secs(1000.0), 0.0);
    assert_eq!(vacuum.doppler_ratio(30.0, 1.0), 1.0);
}

#[test]
fn medium_delay_is_distance_over_speed_and_ignores_negative_distance() {
    let earth = AudioMedium::earth();
    let expected = 343.0 / regia_audio::SPEED_OF_SOUND;
    assert!(
        (earth.delay_secs(343.0) - expected).abs() < 1e-4,
        "{}",
        earth.delay_secs(343.0)
    );
    assert_eq!(earth.delay_secs(-10.0), 0.0);
    let mars = AudioMedium::mars();
    assert!(mars.delay_secs(100.0) > earth.delay_secs(100.0), "Mars is slower");
}

#[test]
fn absorption_cutoff_hz_is_clamped_and_free_at_zero_distance() {
    assert_eq!(absorption_cutoff_hz(0.0016, 0.0), MAX_CUTOFF_HZ);
    assert_eq!(
        absorption_cutoff_hz(0.0, 1000.0),
        MAX_CUTOFF_HZ,
        "no absorption coefficient"
    );
    assert_eq!(
        absorption_cutoff_hz(-1.0, 1000.0),
        MAX_CUTOFF_HZ,
        "negative coefficient is ignored"
    );
    assert_eq!(
        absorption_cutoff_hz(1000.0, 1.0e6),
        MIN_CUTOFF_HZ,
        "extreme absorption hits the floor"
    );
    let near = absorption_cutoff_hz(0.0016, 10.0);
    let far = absorption_cutoff_hz(0.0016, 1000.0);
    assert!(far < near, "more distance, lower cutoff");
}

#[test]
fn medium_absorption_db_grows_with_frequency_squared_and_distance() {
    let mars = AudioMedium::mars();
    let one = mars.absorption_db(1000.0, 10.0);
    let two = mars.absorption_db(2000.0, 10.0);
    assert!((two / one - 4.0).abs() < 1e-3, "f squared");
    assert!(
        (mars.absorption_db(1000.0, 20.0) / one - 2.0).abs() < 1e-3,
        "linear in distance"
    );
    assert_eq!(mars.absorption_db(1000.0, -5.0), 0.0);
}
