// SPDX-License-Identifier: MPL-2.0
//! The RON examples in `docs/formats/audio-*.md` must parse with the real types: these tests
//! embed the documented files verbatim (modulo sound paths, which resolve to generated tones
//! here) so the frozen-format docs cannot drift from the loaders.

use std::time::Duration;

use regia_audio::{
    AudioError, AudioPatch, AudioPatchDef, MusicAsset, MusicDef, SoundAsset, SoundClip, SoundCue,
    SoundCueDef, StealPolicy,
};

const RATE: u32 = 48_000;

/// A distinct tone per referenced path (deterministic, so assertions can tell them apart).
fn tone_for_path(path: &str) -> SoundAsset {
    let _ = path;
    SoundAsset::from_clip(&SoundClip::new(vec![0.1; 64], RATE).expect("clip"))
}

/// The patch example from `docs/formats/audio-patch-v1.md`, verbatim.
const PATCH_EXAMPLE: &str = r#"
(
    version: 1,
    name: "hover_engine",
    bus: Some("Sfx"),
    parameters: [
        (
            name: "speed",
            min: 0.0,
            max: 100.0,
            smoothing_secs: 0.1,
        ),
        (
            name: "throttle",
            min: 0.0,
            max: 1.0,
            smoothing_secs: 0.1,
        ),
    ],
    layers: [
        (
            name: "loop_set",
            sound: None,
            keys: [
                (value: 0.0,   sound: "audio/engine/idle.ogg",  gain_db: 0.0, pitch: 1.0),
                (value: 50.0,  sound: "audio/engine/mid.ogg",   gain_db: 0.0, pitch: 1.0),
                (value: 100.0, sound: "audio/engine/redline.ogg", gain_db: 0.0, pitch: 1.0),
            ],
            parameter: Some("speed"),
            volume_db: -6.0,
            volume_curves: [
                (parameter: "throttle", points: [(0.0, 0.0), (1.0, 6.0)]),
            ],
            pitch_curves: [
                (parameter: "speed", points: [(0.0, 0.6), (100.0, 1.4)]),
            ],
            filter_curves: [
                (parameter: "speed", points: [(0.0, 1200.0), (100.0, 20000.0)]),
            ],
        ),
    ],
)
"#;

/// The cue example from `docs/formats/audio-cue-v1.md`, verbatim.
const CUE_EXAMPLE: &str = r#"
(
    name: "impact_flesh",
    bus: Some("World"),
    sounds: [
        (sound: "audio/impacts/flesh_a.ogg", weight: 1.0),
        (sound: "audio/impacts/flesh_b.ogg", weight: 1.0),
        (sound: "audio/impacts/flesh_c.ogg", weight: 0.5),
    ],
    mode: Random,
    no_repeat_last: 2,
    pitch_min: 0.95,
    pitch_max: 1.05,
    volume_db_min: -2.0,
    volume_db_max: 0.0,
    max_concurrent: Some(4),
    steal: Oldest,
    cooldown: (secs: 0, nanos: 50000000),
    volume_db: 0.0,
)
"#;

/// A music track definition in the same style (`.music.ron`).
const MUSIC_EXAMPLE: &str = r#"
(
    name: "combat_light",
    bpm: 132.0,
    beats_per_bar: 4,
    stems: [
        (
            name: "strings",
            sound: "audio/music/strings.ogg",
            volume_db: 0.0,
            intensity: [(0.0, 0.0), (1.0, 0.0)],
        ),
        (
            name: "drums",
            sound: "audio/music/drums.ogg",
            volume_db: 0.0,
            intensity: [(0.0, -120.0), (1.0, 0.0)],
        ),
    ],
)
"#;

#[test]
fn patch_example_parses_and_resolves() {
    let def = AudioPatchDef::from_ron(PATCH_EXAMPLE).expect("the documented patch example parses");
    // The documented example is a v1 file: it loads migrated to the current version (ADR-7226).
    assert_eq!(def.version, regia_audio::PATCH_VERSION);
    assert_eq!(def.layers.len(), 1);
    assert_eq!(def.layers[0].keys.len(), 3);
    let patch = AudioPatch::from_def(def, |path| Ok(tone_for_path(path))).expect("resolves");
    assert_eq!(patch.key_sounds.len(), 3);
}

#[test]
fn wrong_patch_version_is_rejected() {
    // Version 2 is current since ADR-7226; the first unknown version is 3.
    let rejected = PATCH_EXAMPLE.replace("version: 1,", "version: 3,");
    let error = AudioPatchDef::from_ron(&rejected).expect_err("v3 is rejected");
    assert!(error.to_string().contains("version 3"), "{error}");
}

/// A newer patch version whose body no longer deserialises (the required `name` field is absent)
/// is still a *version* error, not a RON "missing field" error: the version is peeked first.
#[test]
fn patch_rejects_newer_version_even_when_body_changed() {
    let error =
        AudioPatchDef::from_ron("(version: 3, parameters: [], layers: [])").expect_err("v3 is rejected");
    assert!(
        matches!(error, AudioError::UnsupportedPatchVersion(3)),
        "expected a version error, got: {error}"
    );
}

#[test]
fn cue_example_parses_and_resolves() {
    let def = SoundCueDef::from_ron(CUE_EXAMPLE).expect("the documented cue example parses");
    assert_eq!(def.sounds.len(), 3);
    assert_eq!(def.steal, StealPolicy::Oldest);
    assert_eq!(def.cooldown, Duration::from_millis(50));
    let cue: SoundCue = def.into_cue(|path| Ok(tone_for_path(path))).expect("resolves");
    assert_eq!(cue.entries.len(), 3);
    assert_eq!(cue.no_repeat_last, 2);
    assert_eq!(cue.max_concurrent, Some(4));
    assert_eq!(cue.bus.as_deref(), Some("World"));
}

/// The `.music.ron` v2 example of `docs/formats/music-ron.md`, comments and all.
const MUSIC_V2_EXAMPLE: &str = r#"
(
    version: 2,
    name: "combat",
    bpm: 132.0,
    beats_per_bar: 4,            // default 4
    segment_bars: Some(8),       // default None: the length of the longest stem
    stems: [
        (
            name: "drums",
            sound: "audio/music/combat_drums.ogg",
            volume_db: 0.0,                         // default 0
            intensity: [(0.0, -60.0), (1.0, 0.0)],  // default none: the base volume
        ),
    ],
    stingers: [                  // default none
        (
            name: "victory",
            sound: "audio/music/victory.ogg",
            volume_db: -3.0,     // default 0
            sync: NextBar,       // default NextBar
        ),
    ],
    transitions: [               // default none
        (
            to: "calm",          // a track name, or "*" for every track without its own rule
            sync: EndOfSegment,  // Immediate, NextBeat, NextBar (default), EndOfSegment
            crossfade_secs: 2.0, // default 0: equal power, from the switch
            segment: Some((      // default None: a sound between the two tracks
                sound: "audio/music/combat_to_calm.ogg",
                volume_db: 0.0,
            )),
        ),
    ],
)
"#;

#[test]
fn music_v2_example_parses_resolves_and_round_trips() {
    let def = MusicDef::from_ron(MUSIC_V2_EXAMPLE).expect("the documented example parses");
    assert_eq!(def.version, regia_audio::MUSIC_VERSION);
    let again = MusicDef::from_ron(&def.to_ron().expect("serializes")).expect("re-parses");
    assert_eq!(again, def);
    let asset = MusicAsset::from_def(&def, |path| Ok(tone_for_path(path))).expect("resolves");
    assert_eq!(asset.segment_beats(), Some(32));
    assert_eq!(asset.stingers[0].name, "victory");
    assert_eq!(asset.stingers[0].volume_db, -3.0);
    let rule = asset.rule_to("calm");
    assert_eq!(rule.sync, regia_audio::MusicSync::EndOfSegment);
    assert_eq!(rule.crossfade_secs, 2.0);
    assert!(rule.segment.is_some());
}

#[test]
fn the_documented_music_example_is_the_one_tested() {
    let doc = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/formats/music-ron.md"
    ))
    .expect("the format page");
    assert!(
        doc.contains(MUSIC_V2_EXAMPLE.trim()),
        "docs/formats/music-ron.md drifted from the test"
    );
}

#[test]
fn the_music_fingerprint_lists_the_nested_shapes() {
    let fingerprint = regia_audio::music_schema_fingerprint();
    for name in [
        "segment_bars",
        "stingers",
        "transitions",
        "crossfade_secs",
        "EndOfSegment",
    ] {
        assert!(fingerprint.contains(name), "{name} in {fingerprint}");
    }
}

#[test]
fn music_example_parses_and_resolves() {
    let def = MusicDef::from_ron(MUSIC_EXAMPLE).expect("the documented music example parses");
    assert_eq!(def.beats_per_bar, 4);
    let asset = MusicAsset::from_def(&def, |path| Ok(tone_for_path(path))).expect("resolves");
    assert_eq!(asset.stems.len(), 2);
    assert!((asset.bpm - 132.0).abs() < 1e-9);
}
