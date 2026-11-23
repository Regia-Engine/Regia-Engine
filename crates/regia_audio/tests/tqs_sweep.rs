// SPDX-License-Identifier: MPL-2.0
//! Test-quality sweep (ADR-2077): deterministic tests for the public functions of `regia_audio` that
//! had no test naming them (counted per crate: pub fns vs references from tests). Boundary values,
//! NaN and infinite inputs, empty inputs, determinism and round trips. No device, no clock, no sleeps:
//! the mixer cases render fixed steps through the offline backend.
//!
//! A test whose name ends in `_regression` failed on the code before its fix; the fix carries a
//! comment naming the test.

use std::time::Duration;

use regia_audio::voice_chat::{
    JitterBuffer, JitterSettings, Member, MembershipTable, Pcg32, Playout, VoiceChannel, VoiceChannelKind,
    VoiceCodecId, VoiceQuality, downmix_to_mono, tail_period, voice_chat_config_migration_sources,
    voice_chat_config_schema_fingerprint,
};
use regia_audio::{
    Audio, AudioError, AudioPatchDef, AudioReverbZone, AudioSettings, AudioSnapshot, BusEffect, BusLayout,
    LoopGenCache, LoopRegion, LoudnessMeter, LoudnessPolicy, LoudnessTable, MusicAsset, MusicStem,
    MusicSystem, MusicTransitionRequest, PATCH_VERSION, PlacedZone, PlaybackParams, RenderStats,
    ReverbPreset, ReverbZoneShape, SoundAsset, SoundClip, SoundLoudness, ZoneField, ZonePortal, reverb_mix,
};
use regia_core::math::{Quat, Vec3};

const RATE: u32 = 48_000;
const DT: f64 = 1.0 / 60.0;
const TICK_FRAMES: usize = 800;

fn headless() -> Audio {
    Audio::new(&AudioSettings {
        headless: true,
        buses: BusLayout::default_tree(),
        ..AudioSettings::default()
    })
}

fn tone(hz: f32, amp: f32, seconds: f64) -> SoundAsset {
    let len = (seconds * f64::from(RATE)) as usize;
    let samples: Vec<f32> = (0..len)
        .map(|i| (i as f32 / RATE as f32 * hz * std::f32::consts::TAU).sin() * amp)
        .collect();
    SoundAsset::from_clip(&SoundClip::new(samples, RATE).expect("a sine is a valid clip"))
}

fn looping_on(bus: &str) -> PlaybackParams {
    PlaybackParams {
        bus: Some(bus.into()),
        loop_region: Some(LoopRegion {
            start_secs: 0.0,
            end_secs: None,
        }),
        ..PlaybackParams::default()
    }
}

fn run(audio: &mut Audio, ticks: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(ticks * TICK_FRAMES * 2);
    for _ in 0..ticks {
        audio.tick(DT);
        out.extend_from_slice(&audio.render_offline(TICK_FRAMES).expect("offline backend"));
    }
    out
}

fn rms(samples: &[f32]) -> f32 {
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len().max(1) as f32).sqrt()
}

// ------------------------------------------------------------------ 1. downmix_to_mono

#[test]
fn downmix_averages_frames_and_drops_a_partial_tail() {
    let mut out = vec![9.0];
    downmix_to_mono(&[1.0, 3.0, -2.0, 2.0, 5.0], 2, &mut out);
    assert_eq!(
        out,
        vec![9.0, 2.0, 0.0],
        "appends; the odd trailing sample is not a frame"
    );

    let mut mono = Vec::new();
    downmix_to_mono(&[0.25, -0.5], 1, &mut mono);
    assert_eq!(mono, vec![0.25, -0.5], "mono passes through");

    let mut none = Vec::new();
    downmix_to_mono(&[1.0, 2.0, 3.0], 0, &mut none);
    assert!(
        none.is_empty(),
        "zero channels produce nothing (no division by zero)"
    );

    let mut six = Vec::new();
    downmix_to_mono(&[1.0; 12], 6, &mut six);
    assert_eq!(six, vec![1.0, 1.0], "a 5.1 frame of ones averages to one");

    let mut short = Vec::new();
    downmix_to_mono(&[1.0, 1.0], 3, &mut short);
    assert!(short.is_empty(), "less than one frame produces nothing");
}

// ------------------------------------------------------------------ 2. blocked_either_way

#[test]
fn blocking_is_symmetric_and_unknown_slots_block_nothing() {
    let mut table = MembershipTable::default();
    let mut a = Member::default();
    a.blocked.insert(2);
    table.members.insert(1, a);
    table.members.insert(2, Member::default());
    table.members.insert(3, Member::default());

    assert!(table.blocked_either_way(1, 2));
    assert!(table.blocked_either_way(2, 1), "the block is mutual");
    assert!(!table.blocked_either_way(1, 3));
    assert!(!table.blocked_either_way(2, 3));
    assert!(!table.blocked_either_way(7, 8), "unknown slots");
    assert!(
        !table.blocked_either_way(1, 1),
        "nobody blocks themselves by default"
    );

    // The routing honours it both ways: 2 never hears 1 and 1 never hears 2.
    table.channels.insert(
        5,
        VoiceChannel {
            id: 5,
            kind: VoiceChannelKind::Team,
            ..VoiceChannel::default()
        },
    );
    for member in table.members.values_mut() {
        member.channels.insert(5);
    }
    assert_eq!(table.recipients(1, 5), vec![3]);
    assert_eq!(table.recipients(2, 5), vec![3]);
    assert_eq!(table.recipients(3, 5), vec![1, 2]);
}

// ------------------------------------------------------------------ 3. from_code (quality, codec)

#[test]
fn quality_and_codec_codes_round_trip_and_reject_the_rest() {
    for quality in [VoiceQuality::Narrow, VoiceQuality::Standard, VoiceQuality::High] {
        assert_eq!(VoiceQuality::from_code(quality.code()), Some(quality));
    }
    for codec in [VoiceCodecId::Pcm, VoiceCodecId::Opus] {
        assert_eq!(VoiceCodecId::from_code(codec.code()), Some(codec));
    }
    let qualities = (0..=u8::MAX)
        .filter(|c| VoiceQuality::from_code(*c).is_some())
        .count();
    let codecs = (0..=u8::MAX)
        .filter(|c| VoiceCodecId::from_code(*c).is_some())
        .count();
    assert_eq!((qualities, codecs), (3, 2), "every other byte is rejected");
    // The codes fit their nibbles (quality low, codec high).
    assert!(VoiceQuality::High.code() < 16 && VoiceCodecId::Opus.code() < 16);
}

// ------------------------------------------------------------------ 4. Pcg32::next_f32

#[test]
fn pcg_floats_are_half_open_unit_and_seed_deterministic() {
    let mut a = Pcg32::new(42);
    let mut b = Pcg32::new(42);
    let mut c = Pcg32::new(43);
    let mut differs = false;
    let mut sum = 0.0f64;
    const N: usize = 20_000;
    for _ in 0..N {
        let x = a.next_f32();
        assert!((0.0..1.0).contains(&x), "{x}");
        assert_eq!(x.to_bits(), b.next_f32().to_bits(), "same seed, same stream");
        differs |= x != c.next_f32();
        sum += f64::from(x);
    }
    assert!(differs, "another seed gives another stream");
    let mean = sum / N as f64;
    assert!((mean - 0.5).abs() < 0.01, "uniform mean {mean}");
    // 24 bits of mantissa: every value is a multiple of 2^-24 (so 1.0 is never reached by rounding).
    let mut d = Pcg32::new(7);
    for _ in 0..1000 {
        let x = d.next_f32();
        assert_eq!((x * 16_777_216.0).fract(), 0.0);
    }
}

// ------------------------------------------------------------------ 5. tail_period

#[test]
fn tail_period_finds_the_pitch_and_survives_short_or_silent_input() {
    let rate = 48_000;
    // A 200 Hz tone: period 240 samples.
    let x: Vec<f32> = (0..960)
        .map(|i| (i as f32 * std::f32::consts::TAU / 240.0).sin())
        .collect();
    let p = tail_period(&x, rate);
    // One period, or two (a perfect sine matches itself at every multiple).
    assert!(
        (238..=242).contains(&p) || (478..=482).contains(&p),
        "200 Hz period {p}"
    );

    assert_eq!(
        tail_period(&[], rate),
        1,
        "empty input: a 1-sample period, no panic"
    );
    let tiny = tail_period(&[0.5; 3], rate);
    assert!((1..=3).contains(&tiny), "never longer than the input: {tiny}");
    let silent = tail_period(&[0.0; 960], rate);
    assert!((1..=960).contains(&silent));
    let nan = tail_period(&[f32::NAN; 960], rate);
    assert!((1..=960).contains(&nan), "NaN input still gives a usable period");
    assert_eq!(
        tail_period(&x, 0),
        tail_period(&x, 0),
        "rate 0 does not panic and is deterministic"
    );
}

// ------------------------------------------------------------------ 6. AudioReverbZone::with_preset

#[test]
fn presets_fill_the_zone_and_the_accessors_read_them_back() {
    let shape = ReverbZoneShape::Sphere { radius: 3.0 };
    for preset in [
        ReverbPreset::Custom,
        ReverbPreset::Tube,
        ReverbPreset::MineGallery,
        ReverbPreset::Open,
    ] {
        let zone = AudioReverbZone::with_preset(preset, shape);
        let p = preset.params();
        assert_eq!(zone.shape, shape);
        assert_eq!(zone.preset, preset);
        assert_eq!(zone.wet_db(), p.send_db);
        assert_eq!(zone.decay(), p.feedback);
        assert_eq!(zone.blend_distance(), p.blend_distance);
        assert_eq!(zone.distance_scale, p.distance_scale);
        assert_eq!(zone.cavity, p.cavity);
        assert_eq!(zone.priority, 0);
        assert_eq!(ReverbPreset::from_name(preset.name()), Some(preset));
    }
    // Custom equals the plain default apart from the shape.
    let custom = AudioReverbZone::with_preset(ReverbPreset::Custom, shape);
    assert_eq!(
        custom,
        AudioReverbZone {
            shape,
            ..AudioReverbZone::default()
        }
    );
    assert_eq!(
        ReverbPreset::from_name("gallery"),
        Some(ReverbPreset::MineGallery)
    );
    assert_eq!(ReverbPreset::from_name("Tube"), None, "names are lowercase");
}

/// A zone with a NaN room parameter (a hand-edited scene) used to hand NaN to the reverb's comb
/// filters (`NaN.clamp(0, 1)` is NaN), which poisons the reverb bus for good.
#[test]
fn reverb_mix_keeps_room_parameters_finite_for_nan_zones_regression() {
    let mut zone = AudioReverbZone::with_preset(ReverbPreset::Tube, ReverbZoneShape::Global);
    zone.feedback = f32::NAN;
    zone.damping = f32::NAN;
    zone.stereo_width = f32::NAN;
    let mix = reverb_mix(Vec3::ZERO, [(&zone, Vec3::ZERO, Quat::IDENTITY)]);
    assert_eq!(mix.weight, 1.0);
    for v in [mix.feedback, mix.damping, mix.stereo_width] {
        assert!(v.is_finite() && (0.0..=1.0).contains(&v), "{mix:?}");
    }
    // Layered under a finite room it stays finite too.
    let room = AudioReverbZone::small_room(Vec3::splat(2.0));
    let mix = reverb_mix(
        Vec3::ZERO,
        [
            (&zone, Vec3::ZERO, Quat::IDENTITY),
            (&room, Vec3::new(3.0, 0.0, 0.0), Quat::IDENTITY),
        ],
    );
    for v in [mix.feedback, mix.damping, mix.stereo_width, mix.weight] {
        assert!(v.is_finite(), "{mix:?}");
    }
    assert!(mix.effective_send_db().is_finite());
}

// ------------------------------------------------------------------ 7. cavity_zone_index / in_cavity

// `distance_outside` used `max(0)` on a NaN distance, which glam and `f32::max` turn into 0: a NaN
// point (a NaN transform upstream) counted as inside every zone, at full reverb weight, and inside
// every cavity (so the occlusion ray counted no rock). A non-finite point is now outside every
// finite zone.

fn placed(zone: AudioReverbZone, centre: Vec3) -> PlacedZone {
    PlacedZone::new(zone, centre, Quat::IDENTITY, &[])
}

#[test]
fn the_innermost_cavity_wins_and_a_nan_point_is_in_none_regression() {
    let big = AudioReverbZone::tube(ReverbZoneShape::Box {
        half_extents: Vec3::splat(10.0),
    });
    let small = AudioReverbZone::mine_gallery(ReverbZoneShape::Sphere { radius: 2.0 });
    let open = AudioReverbZone::open(ReverbZoneShape::Global);
    assert!(!open.cavity && big.cavity && small.cavity);
    let field = ZoneField {
        zones: vec![
            placed(open, Vec3::ZERO),
            placed(big, Vec3::ZERO),
            placed(small, Vec3::new(5.0, 0.0, 0.0)),
        ],
    };
    assert_eq!(
        field.cavity_zone_index(Vec3::new(5.0, 0.0, 0.0)),
        Some(2),
        "nested: smallest volume"
    );
    assert_eq!(field.cavity_zone_index(Vec3::new(-5.0, 0.0, 0.0)), Some(1));
    assert_eq!(
        field.cavity_zone_index(Vec3::splat(50.0)),
        None,
        "only the open (non-cavity) zone"
    );
    assert!(field.in_cavity(Vec3::new(5.0, 0.0, 0.0)));
    assert!(!field.in_cavity(Vec3::splat(50.0)));
    assert!(
        field.in_cavity(Vec3::new(10.0, 0.0, 0.0)),
        "the wall counts as inside"
    );
    assert!(
        !field.in_cavity(Vec3::splat(f32::NAN)),
        "a NaN point is in no cavity"
    );
    assert_eq!(field.cavity_zone_index(Vec3::splat(f32::NAN)), None);
    assert_eq!(field.cavity_zone_index(Vec3::new(f32::INFINITY, 0.0, 0.0)), None);
    // ... and has no reverb weight, except in a global zone (which is everywhere).
    let tube = AudioReverbZone::tube(ReverbZoneShape::Capsule {
        radius: 2.0,
        half_length: 5.0,
    });
    assert_eq!(
        tube.weight(Vec3::ZERO, Quat::IDENTITY, Vec3::splat(f32::NAN)),
        0.0
    );
    let global = AudioReverbZone::open(ReverbZoneShape::Global);
    assert_eq!(
        global.weight(Vec3::ZERO, Quat::IDENTITY, Vec3::splat(f32::NAN)),
        1.0
    );
    assert!(!ZoneField::default().in_cavity(Vec3::ZERO));

    // Equal volumes: the first placed wins (stable).
    let twin = || AudioReverbZone::tube(ReverbZoneShape::Sphere { radius: 1.0 });
    let field = ZoneField {
        zones: vec![placed(twin(), Vec3::ZERO), placed(twin(), Vec3::ZERO)],
    };
    assert_eq!(field.cavity_zone_index(Vec3::ZERO), Some(0));

    // Portal positions are carried into world space.
    let with_mouth = PlacedZone::new(
        twin(),
        Vec3::new(1.0, 0.0, 0.0),
        Quat::IDENTITY,
        &[ZonePortal {
            position: Vec3::new(0.0, 1.0, 0.0),
            radius: 0.5,
        }],
    );
    assert_eq!(with_mouth.portals, vec![(Vec3::new(1.0, 1.0, 0.0), 0.5)]);
}

// ------------------------------------------------------------------ 8. jitter buffer API

#[test]
fn jitter_spare_following_idle_and_reset() {
    let mut jb = JitterBuffer::new(JitterSettings {
        min_ms: 40,
        max_ms: 200,
        start_ms: 40,
    });
    assert!(jb.is_idle());
    // A spare offered while idle is ignored (it anchors nothing).
    jb.insert_spare(0, b"x");
    assert!(jb.is_idle());
    assert_eq!(jb.stats().spare_filled, 0);

    jb.insert(10, 0, false, b"a", 0);
    assert!(!jb.is_idle());
    // 11 lost, 12 arrives as the second frame of a packet: no jitter sample.
    jb.insert_following(12, false, b"c");
    assert_eq!(jb.depth_frames(), 3);
    // A spare for 11 fills the hole; another for 12 (already held) or 9 (behind) does nothing.
    jb.insert_spare(11, b"b");
    jb.insert_spare(12, b"zz");
    jb.insert_spare(9, b"old");
    jb.insert_spare(40, b"far");
    assert_eq!(jb.stats().spare_filled, 1);
    assert_eq!(jb.depth_frames(), 3, "a spare never grows the buffer");
    assert_eq!(
        jb.stats().jitter_ms,
        0.0,
        "insert_following adds no jitter sample"
    );

    let played: Vec<Playout> = (0..3).map(|_| jb.pull()).collect();
    assert_eq!(
        played,
        vec![
            Playout::Frame(b"a".to_vec()),
            Playout::Frame(b"b".to_vec()),
            Playout::Frame(b"c".to_vec())
        ]
    );

    jb.reset();
    assert!(jb.is_idle());
    assert_eq!(jb.depth_frames(), 0);
    assert_eq!(jb.pull(), Playout::Finished);
    assert_eq!(jb.stats().played, 3, "reset keeps the counters");
    // A fresh spurt after reset anchors on its own sequence number.
    jb.insert_following(500, true, b"e");
    assert_eq!(jb.pull(), Playout::Frame(b"e".to_vec()));
    assert!(jb.is_idle(), "the end-of-talk frame finishes the spurt");
}

/// `target_ms` multiplied the frame count by 20 in `u32`: a config with `max_ms`/`start_ms` near
/// `u32::MAX` (a hand-edited `voice_chat.ron`) overflowed (a panic in debug builds).
#[test]
fn jitter_target_ms_does_not_overflow_for_huge_settings_regression() {
    let jb = JitterBuffer::new(JitterSettings {
        min_ms: u32::MAX,
        max_ms: u32::MAX,
        start_ms: u32::MAX,
    });
    assert!(jb.target_ms() >= u32::MAX - 20, "{}", jb.target_ms());
    let zero = JitterBuffer::new(JitterSettings {
        min_ms: 0,
        max_ms: 0,
        start_ms: 0,
    });
    assert_eq!(zero.target_ms(), 20, "at least one frame");
}

// ------------------------------------------------------------------ 9. LoudnessMeter::short_term_lufs

fn sine_block(seconds: f64, amp: f32) -> Vec<f32> {
    (0..(seconds * f64::from(RATE)) as usize)
        .map(|i| (i as f32 * 997.0 * std::f32::consts::TAU / RATE as f32).sin() * amp)
        .collect()
}

#[test]
fn short_term_loudness_is_minus_infinity_before_input_and_tracks_the_last_three_seconds() {
    let mut meter = LoudnessMeter::new(1, RATE).expect("mono 48 kHz");
    assert_eq!(meter.short_term_lufs(), f32::NEG_INFINITY);
    meter.push(&sine_block(4.0, 0.5));
    let loud = meter.short_term_lufs();
    assert!(loud.is_finite() && loud > -12.0 && loud < 0.0, "{loud}");
    // 3.5 s of silence later the short-term window holds only silence (the K-weighting filters
    // ring out over the first milliseconds after the tone, so 3 s exactly still holds their tail).
    meter.push(&vec![0.0; 7 * RATE as usize / 2]);
    let after = meter.short_term_lufs();
    assert!(
        after == f32::NEG_INFINITY || after < -70.0,
        "the window slid past the tone (only the filters' ring-out is left): {after}"
    );
    // Same input, same reading, bit for bit.
    let mut again = LoudnessMeter::new(1, RATE).expect("mono");
    again.push(&sine_block(4.0, 0.5));
    assert_eq!(again.short_term_lufs().to_bits(), loud.to_bits());
}

/// One NaN sample (a broken clip, a NaN gain upstream) must not poison the K-weighting filters or
/// the running window sums: every later reading equals a meter that heard silence instead.
#[test]
fn a_nan_sample_does_not_poison_the_loudness_meter() {
    let mut meter = LoudnessMeter::new(2, RATE).expect("stereo");
    let mut reference = LoudnessMeter::new(2, RATE).expect("stereo");
    meter.push(&[f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.0]);
    reference.push(&[0.0, 0.0, 0.0, 0.0]);
    let tone: Vec<f32> = sine_block(4.0, 0.25).iter().flat_map(|s| [*s, *s]).collect();
    meter.push(&tone);
    reference.push(&tone);
    assert!(meter.short_term_lufs().is_finite(), "{}", meter.short_term_lufs());
    assert_eq!(meter.short_term_lufs(), reference.short_term_lufs());
    assert_eq!(meter.momentary_lufs(), reference.momentary_lufs());
    assert_eq!(meter.integrated_lufs(), reference.integrated_lufs());
}

// ------------------------------------------------------------------ 10. LoudnessTable::entry_for

fn record(gain_db: f32) -> SoundLoudness {
    SoundLoudness {
        integrated_lufs: Some(-20.0),
        loudness_range_lu: 0.0,
        true_peak_db: Some(-1.0),
        short_clip: false,
        dual_mono: false,
        sample_rate: RATE,
        frames: 48_000,
        normalize: true,
        target_lufs: -23.0,
        gain_db,
        peak_limited: false,
    }
}

#[test]
fn entry_lookup_matches_whole_components_and_prefers_the_longest_key() {
    let mut table = LoudnessTable::default();
    table.entries.insert("sfx/a.wav".into(), record(1.0));
    table.entries.insert("assets/sfx/a.wav".into(), record(2.0));
    table.entries.insert("b.wav".into(), record(3.0));

    assert_eq!(table.gain_db_for("assets/sfx/a.wav"), Some(2.0), "exact");
    assert_eq!(
        table.gain_db_for("/home/me/game/assets/sfx/a.wav"),
        Some(2.0),
        "longest suffix"
    );
    assert_eq!(
        table.gain_db_for("C:\\game\\assets\\sfx\\a.wav"),
        Some(2.0),
        "backslashes"
    );
    assert_eq!(table.gain_db_for("mods/sfx/a.wav"), Some(1.0));
    assert_eq!(table.gain_db_for("x/b.wav"), Some(3.0));
    assert_eq!(
        table.gain_db_for("xb.wav"),
        None,
        "suffix must start at a component boundary"
    );
    assert_eq!(table.gain_db_for("b.wav.bak"), None);
    assert_eq!(table.gain_db_for(""), None);
    assert_eq!(table.gain_db_for("/"), None);
    assert_eq!(LoudnessTable::default().entry_for("a.wav"), None);
    // Round trip through the frozen RON form keeps the lookup.
    let text = table.to_ron_string().expect("finite records serialize");
    let back = LoudnessTable::from_ron(text.as_bytes()).expect("parses");
    assert_eq!(back.gain_db_for("z/assets/sfx/a.wav"), Some(2.0));
    assert_eq!(back.policy, LoudnessPolicy::default());
}

// ------------------------------------------------------------------ 11. bus filter cutoffs

/// A snapshot with a NaN filter cutoff (hand-edited, or a NaN computed by game code) went straight
/// to the mixer's filter, whose state turned NaN for good: the bus stayed silent (the limiter
/// replaces NaN with silence) even after the snapshot was popped. Non-finite cutoffs are now
/// ignored by `Buses::set_filter_cutoffs`.
#[test]
fn a_nan_snapshot_cutoff_does_not_kill_the_bus_regression() {
    let reference = {
        let mut audio = headless();
        audio
            .play_asset(&tone(440.0, 0.25, 0.5), looping_on("World"))
            .expect("enqueue");
        audio.drain();
        run(&mut audio, 30);
        rms(&run(&mut audio, 6))
    };
    assert!(reference > 0.05, "{reference}");

    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let mut audio = headless();
        audio.register_snapshot(AudioSnapshot::new("Bad").with_filter_cutoff("World", bad));
        audio
            .play_asset(&tone(440.0, 0.25, 0.5), looping_on("World"))
            .expect("enqueue");
        audio.drain();
        audio.push_snapshot("Bad", Duration::ZERO).expect("registered");
        let during = run(&mut audio, 12);
        assert_eq!(RenderStats::measure(&during).nonfinite_samples, 0, "{bad}");
        audio.pop_snapshot(Duration::ZERO).expect("pushed");
        run(&mut audio, 18);
        let after = rms(&run(&mut audio, 6));
        assert!(
            (after / reference - 1.0).abs() < 0.05,
            "{bad}: {after} vs {reference}"
        );
        assert_eq!(
            audio.output_meter().nonfinite_samples,
            0,
            "{bad}: nothing to clean up"
        );
    }
}

#[test]
fn a_finite_snapshot_cutoff_darkens_and_restores_the_bus() {
    let mut audio = headless();
    audio.register_snapshot(AudioSnapshot::new("Muffled").with_filter_cutoff("World", 200.0));
    audio
        .play_asset(&tone(4_000.0, 0.25, 0.5), looping_on("World"))
        .expect("enqueue");
    audio.drain();
    run(&mut audio, 18);
    let open = rms(&run(&mut audio, 6));
    audio
        .push_snapshot("Muffled", Duration::ZERO)
        .expect("registered");
    run(&mut audio, 18);
    let muffled = rms(&run(&mut audio, 6));
    audio.pop_snapshot(Duration::ZERO).expect("pushed");
    run(&mut audio, 18);
    let restored = rms(&run(&mut audio, 6));
    assert!(muffled < open * 0.25, "{muffled} vs {open}");
    assert!((restored / open - 1.0).abs() < 0.05, "{restored} vs {open}");
    assert_eq!(BusEffect::default(), BusEffect::neutral_low_pass());
}

// ------------------------------------------------------------------ 12. MusicSystem accessors

#[test]
fn an_idle_music_system_reports_nothing() {
    let mut music = MusicSystem::new();
    assert_eq!(music.current_track(), None);
    assert!(music.stem_levels().is_empty());
    assert!(!music.is_crossfading());
    assert_eq!(music.clock_tick(), None);
    music.set_intensity(f32::NAN);
    music.set_intensity(0.7);
    assert_eq!(
        music.intensity(),
        0.0,
        "no track: pending intensity is not applied"
    );

    // The same through the mixer's tick, which is crate-private since ADR-2517.
    let mut audio = headless();
    audio.set_music_intensity(f32::NAN);
    audio.set_music_intensity(0.7);
    run(&mut audio, 3);
    assert_eq!(audio.music_state().intensity, 0.0);
    assert_eq!(audio.current_music(), None);
}

fn track(bpm: f64) -> MusicAsset {
    MusicAsset::from_stems(
        "t",
        bpm,
        4,
        vec![MusicStem {
            name: "base".into(),
            asset: tone(220.0, 0.2, 0.5),
            base_db: -3.0,
            intensity: vec![(0.0, 0.0), (1.0, 0.0)],
        }],
    )
}

/// `MusicAsset::from_stems` takes any bpm; an infinite (or absurdly high) bpm made the mixer's
/// clock count ticks one by one forever (`while timer >= 1.0 { timer -= 1.0 }` with `timer = inf`),
/// hanging the render. NaN or non-positive bpm left a clock that never ticks. `play_music` now
/// rejects them as `from_def` does.
#[test]
fn music_with_an_unplayable_bpm_is_rejected_regression() {
    for bpm in [f64::INFINITY, f64::NAN, 0.0, -120.0, 1e300] {
        let mut audio = headless();
        let err = audio.play_music(&track(bpm), Duration::ZERO);
        assert!(matches!(err, Err(AudioError::Invalid(_))), "{bpm}: {err:?}");
        assert_eq!(audio.current_music(), None);
        let out = run(&mut audio, 3);
        assert_eq!(RenderStats::measure(&out).nonfinite_samples, 0);
    }
    let mut audio = headless();
    audio
        .play_music(&track(120.0), Duration::ZERO)
        .expect("120 bpm plays");
    assert_eq!(audio.current_music(), Some("t"));
    assert_eq!(audio.music_stem_levels(), vec![("base".to_string(), -3.0)]);
    run(&mut audio, 3);
}

/// A registered track reaches the mixer by way of a transition too, so `music_transition` rejects
/// what `play_music` rejects, and the playing track stays.
#[test]
fn a_transition_to_an_unplayable_bpm_is_rejected_regression() {
    let mut audio = headless();
    audio
        .play_music(&track(120.0), Duration::ZERO)
        .expect("120 bpm plays");
    run(&mut audio, 3);
    for bpm in [f64::INFINITY, f64::NAN, 0.0, -120.0, 1e300] {
        let mut bad = track(bpm);
        bad.name = "bad".into();
        audio.register_music(bad);
        let err = audio.music_transition("bad", MusicTransitionRequest::default());
        assert!(matches!(err, Err(AudioError::Invalid(_))), "{bpm}: {err:?}");
        assert_eq!(audio.current_music(), Some("t"));
    }
    run(&mut audio, 3);
}

// ------------------------------------------------------------------ 13. voice counts and kinds

#[test]
fn voice_kinds_count_live_voices_and_omit_empty_kinds() {
    let mut audio = headless();
    assert!(audio.voice_kinds().is_empty());
    assert_eq!((audio.real_voice_count(), audio.virtual_voice_count()), (0, 0));
    audio
        .play_asset(&tone(440.0, 0.1, 0.5), looping_on("Sfx"))
        .expect("enqueue");
    audio
        .play_asset(&tone(660.0, 0.1, 0.5), looping_on("Sfx"))
        .expect("enqueue");
    audio.drain();
    run(&mut audio, 1);
    let kinds = audio.voice_kinds();
    let total: usize = kinds.iter().map(|(_, n)| n).sum();
    assert_eq!(total, audio.real_voice_count() + audio.virtual_voice_count());
    assert!(kinds.iter().all(|(_, n)| *n > 0), "{kinds:?}");
    assert!(audio.voice_limit() >= 1);
    assert!(!audio.output_suspended());
}

// ------------------------------------------------------------------ 14. AudioPatchDef::migrate_to_latest

fn patch_def(version: u32) -> AudioPatchDef {
    AudioPatchDef {
        version,
        name: "p".into(),
        bus: None,
        parameters: Vec::new(),
        layers: Vec::new(),
    }
}

#[test]
fn patch_migration_accepts_current_and_rejects_newer_or_unknown_older_versions() {
    let mut current = patch_def(PATCH_VERSION);
    assert_eq!(current.migrate_to_latest().expect("current"), PATCH_VERSION);
    assert_eq!(
        current,
        patch_def(PATCH_VERSION),
        "a no-op on the current version"
    );

    let mut newer = patch_def(PATCH_VERSION + 1);
    assert!(matches!(
        newer.migrate_to_latest(),
        Err(AudioError::UnsupportedPatchVersion(v)) if v == PATCH_VERSION + 1
    ));
    let mut max = patch_def(u32::MAX);
    assert!(matches!(
        max.migrate_to_latest(),
        Err(AudioError::UnsupportedPatchVersion(_))
    ));

    // v0 never existed and has no migrator: an error, not a silent relabel.
    let mut zero = patch_def(0);
    assert!(
        matches!(zero.migrate_to_latest(), Err(AudioError::Migration(_))),
        "{zero:?}"
    );
    assert_eq!(zero.version, 0, "a failed migration leaves the version alone");
}

// ------------------------------------------------------------------ 15. hashes and fingerprints

#[test]
fn loopgen_content_hash_is_fnv1a_64() {
    // FNV-1a 64 reference values (offset basis; "a"; "foobar").
    assert_eq!(LoopGenCache::content_hash(""), 0xcbf2_9ce4_8422_2325);
    assert_eq!(LoopGenCache::content_hash("a"), 0xaf63_dc4c_8601_ec8c);
    assert_eq!(LoopGenCache::content_hash("foobar"), 0x8594_4171_f739_67e8);
    assert_ne!(LoopGenCache::content_hash("ab"), LoopGenCache::content_hash("ba"));
}

#[test]
fn voice_chat_config_fingerprint_is_stable_and_names_every_section() {
    let a = voice_chat_config_schema_fingerprint();
    assert_eq!(a, voice_chat_config_schema_fingerprint(), "deterministic");
    for field in ["jitter", "min_ms", "max_ms", "start_ms", "radius"] {
        assert!(a.contains(field), "`{field}` missing from {a}");
    }
    assert!(
        voice_chat_config_migration_sources().is_empty(),
        "v1: no migrators yet"
    );
}
