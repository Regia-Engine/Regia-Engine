// SPDX-License-Identifier: MPL-2.0
//! The mixer's effect chain, bypass, sends and snapshots (`audio-buses` v1, ADR-8105 (g), ADR-8354;
//! parity row P-227). Headless offline mixer, fixed frame steps, deterministic sample buffers; no
//! wall clock.

use std::time::Duration;

use regia_audio::{
    AUDIO_BUSES_FORMAT_VERSION, Audio, AudioSettings, AudioSnapshot, BusDef, BusEffect, BusLayout, BusSend,
    LoopRegion, MUTE_DB, PlaybackParams, REVERB_BUS, SILENT_SEND_DB, SendBusDef, SendTap, SoundAsset,
    SoundClip,
};

const RATE: u32 = 48_000;
const FRAME: f64 = 1.0 / 60.0;

fn bus(name: &str, parent: Option<&str>) -> BusDef {
    BusDef {
        name: name.into(),
        parent: parent.map(str::to_string),
        volume_db: 0.0,
        mute: false,
        effects: Vec::new(),
        sends: Vec::new(),
        bypass_medium: false,
    }
}

/// `Master → Fx`, plus a fully wet `Reverb` send bus.
fn layout(fx_effects: Vec<BusEffect>, send_db: Option<f32>) -> BusLayout {
    let mut fx = bus("Fx", Some("Master"));
    fx.effects = fx_effects;
    if let Some(db) = send_db {
        fx.sends.push(BusSend::new(REVERB_BUS, db));
    }
    BusLayout {
        version: AUDIO_BUSES_FORMAT_VERSION,
        buses: vec![bus("Master", None), fx],
        send_buses: vec![SendBusDef {
            name: REVERB_BUS.into(),
            effects: vec![BusEffect::Reverb {
                feedback: 0.9,
                damping: 0.3,
                stereo_width: 1.0,
                mix: 1.0,
            }],
        }],
        snapshots: Vec::new(),
    }
}

fn audio_with(layout: BusLayout) -> Audio {
    Audio::new(&AudioSettings {
        headless: true,
        buses: layout,
        ..AudioSettings::default()
    })
}

fn tone(hz: f32, amp: f32, secs: f32) -> SoundAsset {
    let len = (RATE as f32 * secs) as usize;
    let samples: Vec<f32> = (0..len)
        .map(|i| ((i as f32 / RATE as f32) * hz * std::f32::consts::TAU).sin() * amp)
        .collect();
    SoundAsset::from_clip(&SoundClip::new(samples, RATE).expect("sine clip"))
}

fn on(bus: &str, looping: bool) -> PlaybackParams {
    PlaybackParams {
        bus: Some(bus.into()),
        loop_region: looping.then_some(LoopRegion {
            start_secs: 0.0,
            end_secs: None,
        }),
        ..PlaybackParams::default()
    }
}

/// `frames` frames of the mixer, one game tick per 1/60 s block.
fn run(audio: &mut Audio, blocks: usize) -> Vec<f32> {
    let mut out = Vec::new();
    for _ in 0..blocks {
        audio.tick(FRAME);
        out.extend_from_slice(
            &audio
                .render_offline((f64::from(RATE) * FRAME) as usize)
                .expect("offline"),
        );
    }
    out
}

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
}

fn db(ratio: f32) -> f32 {
    20.0 * ratio.max(1e-12).log10()
}

/// The output of `layout` playing a looping 2 kHz tone on `Fx` for 30 blocks.
fn play_tone_through(layout: BusLayout) -> Vec<f32> {
    let mut audio = audio_with(layout);
    audio
        .play_asset(&tone(2_000.0, 0.3, 0.5), on("Fx", true))
        .expect("enqueue");
    audio.drain();
    run(&mut audio, 30)
}

fn low_pass_200() -> BusEffect {
    BusEffect::LowPass {
        cutoff_hz: 200.0,
        resonance: 0.0,
    }
}

#[test]
fn a_bypassed_effect_passes_audio_unchanged() {
    let without = play_tone_through(layout(Vec::new(), None));
    let bypassed = play_tone_through(layout(vec![low_pass_200().with_bypass(true)], None));
    let active = play_tone_through(layout(vec![low_pass_200()], None));
    assert!(
        rms(&without) > 0.05,
        "the tone reaches the output: {}",
        rms(&without)
    );
    assert_eq!(without.len(), bypassed.len());
    // Bit-exact: a bypassed slot builds nothing, so the chain is the chain without it.
    let worst = without
        .iter()
        .zip(&bypassed)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0f32, f32::max);
    assert_eq!(
        worst, 0.0,
        "the bypassed low-pass changed the signal by up to {worst}"
    );
    // The same effect active cuts the 2 kHz tone hard: the bypass is what kept it out.
    assert!(
        db(rms(&active) / rms(&without)) < -12.0,
        "an active 200 Hz low-pass attenuates 2 kHz: {} dB",
        db(rms(&active) / rms(&without))
    );
    // Its parameters survive the bypass round trip.
    let back = low_pass_200().with_bypass(true).with_bypass(false);
    assert_eq!(back, low_pass_200());
    assert_eq!(
        low_pass_200().with_bypass(true).with_bypass(true).active(),
        &low_pass_200()
    );
}

#[test]
fn bypass_and_reorder_reach_a_live_mixer_through_set_bus_layout() {
    let mut audio = audio_with(layout(vec![low_pass_200()], None));
    let play = |audio: &mut Audio| {
        audio
            .play_asset(&tone(2_000.0, 0.3, 0.5), on("Fx", true))
            .expect("enqueue");
        audio.drain();
        rms(&run(audio, 20)[RATE as usize / 6..])
    };
    let filtered = play(&mut audio);
    audio
        .set_bus_layout(layout(vec![low_pass_200().with_bypass(true)], None))
        .expect("bypass");
    assert!(
        audio.bus_layout().buses[1].effects[0].is_bypassed(),
        "the live layout is the new one"
    );
    let open = play(&mut audio);
    assert!(
        db(open / filtered) > 12.0,
        "un-filtered after the bypass: {open} vs {filtered}"
    );
    // A chain that refuses validation leaves the mixer as it was.
    let twice = BusEffect::Bypassed(Box::new(BusEffect::Bypassed(Box::new(low_pass_200()))));
    let error = audio
        .set_bus_layout(layout(vec![twice], None))
        .expect_err("refused");
    assert!(error.to_string().contains("bypassed twice"), "{error}");
    assert!(audio.bus_layout().buses[1].effects[0].is_bypassed());
}

#[test]
fn a_send_routes_signal_to_the_reverb_bus() {
    // A 0.1 s burst on `Fx`, then silence: whatever plays after the burst is the reverb's tail,
    // which only exists when the send routes the burst into the wet-only `Reverb` bus.
    // From 0.25 s on: the burst (and kira's short output fade) is long gone.
    let tail = |audio: &mut Audio| burst(audio).1;
    let mut silent = audio_with(layout(Vec::new(), Some(SILENT_SEND_DB)));
    let dry_tail = tail(&mut silent);
    let mut sent = audio_with(layout(Vec::new(), Some(0.0)));
    let wet_tail = tail(&mut sent);
    assert!(dry_tail < 1e-4, "a silent send leaves no tail: {dry_tail}");
    assert!(wet_tail > 1e-3, "the send feeds the reverb: tail rms {wet_tail}");

    // A send added to a live mixer (a rebuild: new tracks, so no tail is left over) routes too,
    // and a level-only change applies live with no rebuild.
    let mut live = audio_with(layout(Vec::new(), None));
    let none = tail(&mut live);
    live.set_bus_layout(layout(Vec::new(), Some(0.0)))
        .expect("add the send");
    let added = tail(&mut live);
    let mut level = audio_with(layout(Vec::new(), Some(0.0)));
    level
        .set_bus_layout(layout(Vec::new(), Some(SILENT_SEND_DB)))
        .expect("silence the send");
    let silenced = tail(&mut level);
    assert!(none < 1e-4, "no route, no tail: {none}");
    assert!(added > 1e-3, "the added send routes: {added}");
    assert!(silenced < 1e-4, "the live level change silences it: {silenced}");
}

#[test]
fn applying_a_snapshot_blends_faders() {
    let mut with_snapshot = layout(Vec::new(), None);
    with_snapshot.snapshots.push(
        AudioSnapshot::new("Quiet")
            .with_bus_volume("Fx", -20.0)
            .with_blend_secs(0.5),
    );
    let mut audio = audio_with(with_snapshot);
    audio
        .play_asset(&tone(1_000.0, 0.3, 0.5), on("Fx", true))
        .expect("enqueue");
    audio.drain();
    let before = rms(&run(&mut audio, 10)[(RATE as usize / 12) * 2..]);
    audio.apply_snapshot("Quiet").expect("registered from the layout");
    assert_eq!(audio.active_snapshot(), Some("Quiet"));
    let fx = |audio: &Audio| {
        audio
            .bus_status()
            .into_iter()
            .find(|b| b.name == "Fx")
            .expect("Fx")
            .current_db
    };
    let mut path = Vec::new();
    for _ in 0..40 {
        run(&mut audio, 1);
        path.push(fx(&audio));
    }
    // 0.5 s at 60 ticks a second: 30 steps of 20/30 dB, then it holds.
    assert!(
        path.windows(2).take(29).all(|w| w[1] < w[0]),
        "the fader falls every tick of the blend: {path:?}"
    );
    assert!((path[14] - -10.0).abs() < 0.05, "halfway at 0.25 s: {}", path[14]);
    assert!(path[13] > -10.0 && path[13] < 0.0, "not there yet: {}", path[13]);
    assert!((path[29] - -20.0).abs() < 1e-3, "arrived at 0.5 s: {}", path[29]);
    assert!(path[30..].iter().all(|db| (db - -20.0).abs() < 1e-3), "{path:?}");
    let after = rms(&run(&mut audio, 10));
    assert!(
        (db(after / before) - -20.0).abs() < 1.0,
        "the output follows the fader: {} dB",
        db(after / before)
    );
    // Popping blends back over the fade it is given.
    audio.pop_snapshot(Duration::ZERO).expect("pop");
    run(&mut audio, 1);
    assert!(
        fx(&audio).abs() < 1e-3,
        "back at the layout level: {}",
        fx(&audio)
    );
    assert!(
        audio.apply_snapshot("Nope").is_err(),
        "unknown snapshots are errors"
    );
}

/// A 0.1 s burst on `Fx`: the rms of the burst and of the output from 0.25 s on (the tail).
fn burst(audio: &mut Audio) -> (f32, f32) {
    audio
        .play_asset(&tone(1_000.0, 0.4, 0.1), on("Fx", false))
        .expect("enqueue");
    audio.drain();
    let out = run(audio, 40);
    (
        rms(&out[..RATE as usize / 10]),
        rms(&out[(RATE as usize / 4) * 2..]),
    )
}

#[test]
fn a_snapshot_sets_a_send_level_and_mutes_a_bus() {
    let mut l = layout(Vec::new(), Some(SILENT_SEND_DB));
    l.snapshots
        .push(AudioSnapshot::new("Hall").with_send_volume("Fx", REVERB_BUS, 0.0));
    l.snapshots
        .push(AudioSnapshot::new("Hush").with_bus_mute("Master", true));
    let mut hall = audio_with(l.clone());
    let (_, dry) = burst(&mut hall);
    hall.apply_snapshot("Hall").expect("hall");
    run(&mut hall, 3);
    let (_, wet) = burst(&mut hall);
    assert!(
        dry < 1e-4 && wet > 1e-3,
        "the snapshot's send level routes: {dry} → {wet}"
    );

    let mut hush = audio_with(l);
    hush.apply_snapshot("Hush").expect("hush");
    run(&mut hush, 3);
    let (muted, _) = burst(&mut hush);
    assert!(
        muted < 1e-4,
        "the snapshot's Master mute silences the output: {muted}"
    );
    hush.pop_snapshot(Duration::ZERO).expect("pop");
    run(&mut hush, 3);
    let (heard, _) = burst(&mut hush);
    assert!(heard > 0.05, "popping it unmutes: {heard}");
}

#[test]
fn the_layout_file_loads_v0_writes_v1_and_refuses_newer_versions() {
    // A v0.5.0 file: no version, no snapshots.
    let v0 = "(buses: [(name: \"Master\"), (name: \"Fx\", parent: Some(\"Master\"), effects: [LowPass(cutoff_hz: 500.0, resonance: 0.0)])])";
    let raw = BusLayout::from_ron(v0).expect("parses");
    assert_eq!(raw.version, 0, "a versionless file is version 0");
    let layout = BusLayout::parse(v0).expect("loads");
    assert_eq!(layout.version, AUDIO_BUSES_FORMAT_VERSION);
    let text = layout.to_ron().expect("writes");
    assert!(
        text.trim_start().starts_with("(\n    version: 1,"),
        "the writer stamps the version first: {text}"
    );
    assert_eq!(BusLayout::parse(&text).expect("reloads"), layout);
    let newer = text.replacen("version: 1", "version: 2", 1);
    let error = BusLayout::parse(&newer).expect_err("newer");
    assert!(error.contains("version 2"), "{error}");
    // A full v1 layout round-trips byte for byte.
    let mut full = layout.clone();
    full.buses[1].effects.push(
        BusEffect::Eq {
            low_hz: 100.0,
            low_db: 2.0,
            mid_hz: 900.0,
            mid_db: -3.0,
            high_hz: 6_000.0,
            high_db: 1.0,
            quality: 0.8,
        }
        .with_bypass(true),
    );
    full.snapshots.push(
        AudioSnapshot::new("Quiet")
            .with_bus_volume("Fx", -12.0)
            .with_bus_mute("Master", false)
            .with_blend_secs(0.25),
    );
    let text = full.to_ron().expect("writes");
    let back = BusLayout::parse(&text).expect("reloads");
    assert_eq!(back, full);
    assert_eq!(back.to_ron().expect("writes"), text);
}

#[test]
fn snapshots_must_name_real_buses_and_routes() {
    let mut l = layout(Vec::new(), Some(0.0));
    l.snapshots
        .push(AudioSnapshot::new("A").with_bus_volume("Nope", -3.0));
    assert!(l.validate().expect_err("bus").contains("Nope"));
    let mut l = layout(Vec::new(), None);
    l.snapshots
        .push(AudioSnapshot::new("A").with_send_volume("Fx", REVERB_BUS, -3.0));
    assert!(l.validate().expect_err("route").contains("does not route"));
    let mut l = layout(Vec::new(), None);
    l.snapshots.push(AudioSnapshot::new("A"));
    l.snapshots.push(AudioSnapshot::new("A"));
    assert!(l.validate().expect_err("twice").contains("twice"));
    let mut l = layout(Vec::new(), None);
    l.snapshots.push(AudioSnapshot::new(" "));
    assert!(l.validate().is_err(), "a blank name is refused");
    let mut l = layout(Vec::new(), None);
    l.snapshots
        .push(AudioSnapshot::new("A").with_blend_secs(f32::NAN));
    assert!(l.validate().is_err(), "a NaN blend time is refused");
}

// ------------------------------------------------------------------ pre-fader sends (ADR-8590)

/// `Master → Fx` with a 0 dB send from `Fx` into the wet-only `Reverb` bus, tapped at `tap`.
fn layout_tapped(tap: SendTap) -> BusLayout {
    let mut l = layout(Vec::new(), Some(0.0));
    l.buses[1].sends[0].tap = tap;
    l
}

/// The burst and tail rms ([`burst`]) of a mixer whose `Fx` fader the host pulled to −∞: kira's
/// silence ([`MUTE_DB`], amplitude exactly 0, the fader's floor).
fn burst_with_the_fader_down(layout: BusLayout) -> (f32, f32) {
    let mut audio = audio_with(layout);
    audio
        .set_bus_volume("Fx", MUTE_DB, Duration::ZERO)
        .expect("the fader");
    run(&mut audio, 3);
    burst(&mut audio)
}

#[test]
fn a_prefader_send_still_reaches_the_reverb_bus_with_the_fader_at_minus_inf() {
    let (_, wet_tail) = burst_with_the_fader_down(layout_tapped(SendTap::PreFader));
    assert!(
        wet_tail > 1e-3,
        "the pre-fader send feeds the reverb with the fader at -inf: tail rms {wet_tail}"
    );
    // The fader still silences the dry path: with the send silent, nothing is heard at all.
    let mut quiet = layout_tapped(SendTap::PreFader);
    quiet.buses[1].sends[0].volume_db = SILENT_SEND_DB;
    let (dry, tail) = burst_with_the_fader_down(quiet);
    assert!(
        dry < 1e-4 && tail < 1e-4,
        "the fader at -inf silences the dry signal of a split bus: burst {dry}, tail {tail}"
    );
    // The bus mute is after the tap too.
    let mut muted = audio_with(layout_tapped(SendTap::PreFader));
    muted.set_bus_mute("Fx", true).expect("mute");
    run(&mut muted, 3);
    let (_, muted_tail) = burst(&mut muted);
    assert!(
        muted_tail > 1e-3,
        "a muted bus still feeds its pre-fader send: tail rms {muted_tail}"
    );
}

#[test]
fn a_postfader_send_is_silent_with_the_fader_at_minus_inf() {
    let (dry, tail) = burst_with_the_fader_down(layout_tapped(SendTap::PostFader));
    assert!(
        dry < 1e-4 && tail < 1e-4,
        "the post-fader send follows the fader to -inf: burst {dry}, tail {tail}"
    );
    // The default tap is post-fader.
    let (dry, tail) = burst_with_the_fader_down(layout(Vec::new(), Some(0.0)));
    assert!(dry < 1e-4 && tail < 1e-4, "default tap: burst {dry}, tail {tail}");
}

#[test]
fn with_the_fader_at_unity_a_prefader_send_renders_the_same_samples_as_a_postfader_one() {
    // The split (an internal track carrying the pre-fader sends under the fader track) adds no
    // processing of its own: at 0 dB the two taps render the same samples, dry and wet.
    let render = |tap: SendTap| {
        let mut audio = audio_with(layout_tapped(tap));
        audio
            .play_asset(&tone(1_000.0, 0.4, 0.1), on("Fx", false))
            .expect("enqueue");
        audio.drain();
        run(&mut audio, 40)
    };
    let post = render(SendTap::PostFader);
    let pre = render(SendTap::PreFader);
    assert!(rms(&post) > 0.05, "the burst is heard: {}", rms(&post));
    assert_eq!(post.len(), pre.len());
    let worst = post
        .iter()
        .zip(&pre)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0f32, f32::max);
    assert!(worst < 1e-6, "pre and post differ at unity by up to {worst}");
}

#[test]
fn a_prefader_send_carries_child_buses_and_the_bus_effects() {
    // `Fx → Child`: a voice on the child reaches `Fx`'s pre-fader send with `Fx`'s fader down,
    // after `Fx`'s own effects (a 200 Hz low-pass cuts a 2 kHz tone before the send).
    let tail_of = |effects: Vec<BusEffect>| {
        let mut l = layout_tapped(SendTap::PreFader);
        l.buses[1].effects = effects;
        l.buses.push(bus("Child", Some("Fx")));
        let mut audio = audio_with(l);
        audio
            .set_bus_volume("Fx", MUTE_DB, Duration::ZERO)
            .expect("the fader");
        run(&mut audio, 3);
        audio
            .play_asset(&tone(2_000.0, 0.4, 0.1), on("Child", false))
            .expect("enqueue");
        audio.drain();
        let out = run(&mut audio, 40);
        rms(&out[(RATE as usize / 4) * 2..])
    };
    let open = tail_of(Vec::new());
    let filtered = tail_of(vec![low_pass_200()]);
    assert!(
        open > 1e-3,
        "the child's voice reaches the pre-fader send: {open}"
    );
    assert!(
        db(filtered / open) < -12.0,
        "the send hears the bus after its effects: {} dB",
        db(filtered / open)
    );
}

#[test]
fn snapshot_and_live_send_levels_reach_a_prefader_send() {
    // A snapshot's send level and a live layout level change both retarget the pre-fader route.
    let mut l = layout_tapped(SendTap::PreFader);
    l.buses[1].sends[0].volume_db = SILENT_SEND_DB;
    l.snapshots
        .push(AudioSnapshot::new("Hall").with_send_volume("Fx", REVERB_BUS, 0.0));
    let mut audio = audio_with(l.clone());
    audio
        .set_bus_volume("Fx", MUTE_DB, Duration::ZERO)
        .expect("the fader");
    run(&mut audio, 3);
    let (_, dry) = burst(&mut audio);
    audio.apply_snapshot("Hall").expect("hall");
    run(&mut audio, 3);
    let (_, wet) = burst(&mut audio);
    assert!(
        dry < 1e-4 && wet > 1e-3,
        "the snapshot's level reaches the pre-fader route: {dry} -> {wet}"
    );

    let mut live = audio_with(l.clone());
    live.set_bus_volume("Fx", MUTE_DB, Duration::ZERO)
        .expect("the fader");
    run(&mut live, 3);
    let mut raised = l;
    raised.buses[1].sends[0].volume_db = 0.0;
    live.set_bus_layout(raised).expect("level only");
    let (_, tail) = burst(&mut live);
    assert!(
        tail > 1e-3,
        "the live level change reaches the pre-fader route: {tail}"
    );
}

#[test]
fn changing_the_tap_on_a_live_mixer_rebuilds_the_route() {
    let post = layout_tapped(SendTap::PostFader);
    let pre = layout_tapped(SendTap::PreFader);
    assert!(
        !post.same_tracks(&pre),
        "a tap is structure, not a level: the tracks differ"
    );
    let mut audio = audio_with(post);
    audio
        .set_bus_volume("Fx", MUTE_DB, Duration::ZERO)
        .expect("the fader");
    // The fader lands on the next tick: settle before the burst.
    run(&mut audio, 3);
    let (_, before) = burst(&mut audio);
    audio.set_bus_layout(pre.clone()).expect("tap");
    assert_eq!(audio.bus_layout().buses[1].sends[0].tap, SendTap::PreFader);
    assert!(
        (audio.bus_volume("Fx").expect("Fx") - MUTE_DB).abs() < 1e-3,
        "the host fader survives the rebuild"
    );
    let (_, after) = burst(&mut audio);
    assert!(
        before < 1e-4 && after > 1e-3,
        "post-fader silent, then pre-fader heard: {before} -> {after}"
    );
}

#[test]
fn audio_buses_v1_without_tap_loads_as_postfader() {
    // The frozen v1 corpus fixture, written before the tap existed.
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../regia_agent/tests/fixtures/format-corpus/audio-buses/v1.ron");
    let text = std::fs::read_to_string(&path).expect("the frozen v1 fixture");
    assert!(!text.contains("tap"), "the old fixture has no tap field");
    let layout = BusLayout::parse(&text).expect("the old v1 file loads");
    assert_eq!(layout.version, AUDIO_BUSES_FORMAT_VERSION);
    let sends: Vec<&BusSend> = layout.buses.iter().flat_map(|b| &b.sends).collect();
    assert!(!sends.is_empty());
    assert!(
        sends.iter().all(|s| s.tap == SendTap::PostFader),
        "every send of a file without the field is post-fader: {sends:?}"
    );
    // The writer leaves a post-fader tap out, so a layout without pre-fader sends writes the
    // same text as before the field existed.
    let written = layout.to_ron().expect("writes");
    assert!(!written.contains("tap"), "{written}");

    // A pre-fader send is written, and round-trips.
    let mut pre = layout.clone();
    pre.buses[2].sends[0].tap = SendTap::PreFader;
    let text = pre.to_ron().expect("writes");
    assert_eq!(text.matches("tap: PreFader").count(), 1, "{text}");
    let back = BusLayout::parse(&text).expect("reloads");
    assert_eq!(back, pre);
    assert_eq!(back.to_ron().expect("writes"), text);
    assert!(!back.same_tracks(&layout), "a tap change is a rebuild");
    // The version stays 1 (an additive v1 fold, ADR-8590).
    assert_eq!(AUDIO_BUSES_FORMAT_VERSION, 1);
    assert!(
        BusLayout::parse(&text.replace("PreFader", "Sideways")).is_err(),
        "an unknown tap is refused"
    );
}
