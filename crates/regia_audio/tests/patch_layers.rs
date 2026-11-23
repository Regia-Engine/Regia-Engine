// SPDX-License-Identifier: MPL-2.0
//! `.audiopatch.ron` v2 acceptance (ADR-7226, `docs/specs/16-audio.md` § 3a): one-shot,
//! granular and reflected-field-bound layers rendered offline, and the v1 → v2 migrator over every
//! v1 fixture.
//!
//! Where the mixer is involved, a layer's render is compared sample for sample against a hand
//! computation (the source samples, the window sums) scaled by the one constant the mixer adds,
//! which a plain one-shot of a unit clip measures in the same render setup.

use std::collections::HashMap;

use regia_audio::{
    Audio, AudioError, AudioPatch, AudioPatchDef, AudioPatchSource, AudioSettings, PATCH_VERSION,
    PatchBindingState, PatchLayerKind, PlaybackParams, SoundAsset, SoundClip, patch_binding_sync,
};
use regia_ecs::prelude::*;
use regia_ecs::reflect::{AppTypeRegistry, ReflectComponent};

const RATE: u32 = 48_000;
const FRAME: f64 = 1.0 / 60.0;
/// One tick's worth of frames at 60 Hz.
const BLOCK: usize = 800;

fn headless() -> Audio {
    Audio::new(&AudioSettings {
        headless: true,
        ..AudioSettings::default()
    })
}

fn clip(samples: Vec<f32>) -> SoundAsset {
    SoundAsset::from_clip(&SoundClip::new(samples, RATE).expect("a valid clip"))
}

/// The left channel of an interleaved stereo render.
fn left(frames: &[f32]) -> Vec<f32> {
    frames.as_chunks::<2>().0.iter().map(|f| f[0]).collect()
}

fn db_to_gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// The mixer's constant for a centred mono clip at 0 dB (pan law and master stages): a one-sample
/// unit impulse played as a plain one-shot, read back from the first rendered frame.
fn mixer_scale() -> f32 {
    let mut audio = headless();
    let mut impulse = vec![0.0f32; 64];
    impulse[0] = 1.0;
    audio
        .start_asset(&clip(impulse), PlaybackParams::on_bus("Sfx"))
        .expect("an impulse plays");
    audio.tick(FRAME);
    let out = left(&audio.render_offline(64).expect("offline render"));
    assert!(
        out[0] > 0.1,
        "the impulse is heard on the first frame: {:?}",
        &out[..4]
    );
    assert!(out[1..].iter().all(|s| s.abs() < 1e-6), "and nowhere else");
    out[0]
}

fn resolve_with(
    sounds: HashMap<&'static str, SoundAsset>,
) -> impl Fn(&str) -> Result<SoundAsset, AudioError> {
    move |path: &str| {
        sounds
            .get(path)
            .cloned()
            .ok_or_else(|| AudioError::UnknownEntry(path.to_string()))
    }
}

fn assert_close(got: &[f32], want: &[f32], tolerance: f32, what: &str) {
    assert_eq!(got.len(), want.len(), "{what}: length");
    for (n, (g, w)) in got.iter().zip(want).enumerate() {
        assert!((g - w).abs() <= tolerance, "{what}: frame {n}: got {g}, want {w}");
    }
}

// ----------------------------------------------------------------------------- one-shot layers

/// 64 frames of a deterministic, non-repeating shape.
fn click_samples() -> Vec<f32> {
    (0..64)
        .map(|i| (((i * 37) % 64) as f32 / 64.0 - 0.5) * 0.8)
        .collect()
}

const ONE_SHOT_PATCH: &str = r#"(
    version: 2,
    name: "gearbox",
    parameters: [(name: "gear", min: 0.0, max: 5.0, smoothing_secs: 0.001)],
    layers: [(
        name: "clunk",
        volume_db: -6.0,
        one_shot: Some((sounds: ["click"], trigger: Rising(parameter: "gear", threshold: 1.0))),
    )],
)"#;

/// A rising crossing fires the one-shot at the start of the next rendered block, and the block
/// is the click at the layer's volume, sample for sample; nothing plays before or after.
#[test]
fn a_one_shot_layer_fires_on_a_rising_crossing_sample_exact() {
    let scale = mixer_scale();
    let def = AudioPatchDef::from_ron(ONE_SHOT_PATCH).expect("a v2 one-shot patch");
    let click = click_samples();
    let patch = AudioPatch::from_def(def, resolve_with(HashMap::from([("click", clip(click.clone()))])))
        .expect("resolves");
    let mut audio = headless();
    let id = audio.spawn_patch(patch).expect("spawns");
    audio.tick(FRAME);
    let quiet = left(&audio.render_offline(BLOCK).expect("render"));
    assert!(quiet.iter().all(|s| *s == 0.0), "no crossing, no sound");

    audio.patch_set_param(id, "gear", 2.0).expect("gear exists");
    audio.tick(FRAME);
    let block = left(&audio.render_offline(BLOCK).expect("render"));
    let gain = db_to_gain(-6.0) * scale;
    let mut want: Vec<f32> = click.iter().map(|s| s * gain).collect();
    want.resize(BLOCK, 0.0);
    assert_close(&block, &want, 1e-5, "the clunk");

    // Staying above the threshold fires nothing more.
    audio.tick(FRAME);
    let after = left(&audio.render_offline(BLOCK).expect("render"));
    assert!(after.iter().all(|s| *s == 0.0), "one crossing, one shot");
    let state = audio.patch_state();
    assert_eq!(state[0].layers[0].kind, "one_shot");
    assert_eq!(state[0].layers[0].fired, 1);
}

/// An `Every` trigger fires once per step crossed and walks its sounds round-robin; each tick's
/// block starts with the right sound.
#[test]
fn an_every_trigger_fires_round_robin_once_per_step() {
    let scale = mixer_scale();
    let def = AudioPatchDef::from_ron(
        r#"(
        version: 2,
        name: "odometer",
        parameters: [(name: "distance", min: 0.0, max: 100.0, smoothing_secs: 0.001)],
        layers: [(
            name: "tick",
            one_shot: Some((sounds: ["a", "b"], trigger: Every(parameter: "distance", step: 1.0))),
        )],
    )"#,
    )
    .expect("parses");
    let a = vec![0.25f32; 32];
    let b = vec![-0.5f32; 32];
    let patch = AudioPatch::from_def(
        def,
        resolve_with(HashMap::from([("a", clip(a.clone())), ("b", clip(b.clone()))])),
    )
    .expect("resolves");
    let mut audio = headless();
    let id = audio.spawn_patch(patch).expect("spawns");
    for step in 1..=4u32 {
        audio
            .patch_set_param(id, "distance", step as f32 + 0.5)
            .expect("distance exists");
        audio.tick(FRAME);
        let block = left(&audio.render_offline(BLOCK).expect("render"));
        let source = if step % 2 == 1 { &a } else { &b };
        let mut want: Vec<f32> = source.iter().map(|s| s * scale).collect();
        want.resize(BLOCK, 0.0);
        assert_close(&block, &want, 1e-5, &format!("step {step}"));
    }
    assert_eq!(audio.patch_state()[0].layers[0].fired, 4);
}

/// A manual layer fires only through `patch_trigger`, and its cooldown skips a second shot.
#[test]
fn a_manual_one_shot_respects_its_cooldown() {
    let def = AudioPatchDef::from_ron(
        r#"(
        version: 2,
        name: "horn",
        parameters: [],
        layers: [(name: "horn", one_shot: Some((sounds: ["h"], trigger: Manual, cooldown_secs: 0.5)))],
    )"#,
    )
    .expect("parses");
    let patch = AudioPatch::from_def(def, resolve_with(HashMap::from([("h", clip(vec![0.1; 16]))])))
        .expect("resolves");
    let mut audio = headless();
    let id = audio.spawn_patch(patch).expect("spawns");
    assert_eq!(audio.patch_trigger(id, "horn"), Ok(true));
    assert_eq!(audio.patch_trigger(id, "horn"), Ok(false), "inside the cooldown");
    for _ in 0..40 {
        audio.tick(FRAME);
    }
    assert_eq!(audio.patch_trigger(id, "horn"), Ok(true), "after the cooldown");
    let layer = &audio.patch_state()[0].layers[0];
    assert_eq!((layer.fired, layer.skipped), (2, 1));
    assert!(matches!(
        audio.patch_trigger(id, "nope"),
        Err(AudioError::UnknownEntry(_))
    ));
    assert!(matches!(
        audio.patch_trigger(99, "horn"),
        Err(AudioError::UnknownEntry(_))
    ));
}

// ----------------------------------------------------------------------------- granular layers

/// A source with no repeating structure: the reference catches any misplaced read.
fn grain_source() -> Vec<f32> {
    (0..4_800)
        .map(|i| (((i * 7_919) % 4_801) as f32 / 4_801.0 - 0.5) * 0.6)
        .collect()
}

/// The overlap-add written out from the definition (`docs/formats/audio-patch-v2.md`): grain `k`
/// starts at frame `k · interval`, lasts `grain` frames, is shaped by the periodic Hann window
/// `0.5 - 0.5 cos(2π a / grain)` and reads `src[(start + a) mod len]`.
fn overlap_add(src: &[f32], start: usize, grain: usize, interval: usize, frames: usize) -> Vec<f32> {
    let mut out = vec![0.0f32; frames];
    let mut onset = 0;
    while onset < frames {
        for age in 0..grain {
            if onset + age >= frames {
                break;
            }
            let w = 0.5 - 0.5 * (std::f64::consts::TAU * age as f64 / grain as f64).cos();
            out[onset + age] += src[(start + age) % src.len()] * w as f32;
        }
        onset += interval;
    }
    out
}

/// A granular layer through the mixer is the hand-computed overlap-add of Hann grains at the
/// layer's volume: 96-frame grains every 48 frames from a quarter into the source.
#[test]
fn a_granular_layer_renders_the_hand_computed_overlap_add() {
    let scale = mixer_scale();
    let def = AudioPatchDef::from_ron(
        r#"(
        version: 2,
        name: "gravel",
        parameters: [],
        layers: [(
            name: "grains",
            volume_db: -6.0,
            granular: Some((sound: "src", grain_secs: 0.002, density_hz: 1000.0, position: 0.25)),
        )],
    )"#,
    )
    .expect("parses");
    let src = grain_source();
    let patch = AudioPatch::from_def(def, resolve_with(HashMap::from([("src", clip(src.clone()))])))
        .expect("resolves");
    let mut audio = headless();
    audio.spawn_patch(patch).expect("spawns");
    let mut rendered = Vec::new();
    for _ in 0..3 {
        audio.tick(FRAME);
        rendered.extend(left(&audio.render_offline(BLOCK).expect("render")));
    }
    let gain = db_to_gain(-6.0) * scale;
    let want: Vec<f32> = overlap_add(&src, 1_200, 96, 48, 3 * BLOCK)
        .into_iter()
        .map(|s| s * gain)
        .collect();
    assert_close(&rendered, &want, 1e-5, "the grains");
    let layer = &audio.patch_state()[0].layers[0];
    assert_eq!(layer.kind, "granular");
    assert_eq!(layer.grains, 50, "one grain every 48 frames over 2400 frames");
}

/// The position curve moves where new grains read: after the parameter changes, the grains that
/// start in the next block read from the new place (the reference again, from the new start).
#[test]
fn a_granular_position_follows_its_parameter() {
    let scale = mixer_scale();
    let def = AudioPatchDef::from_ron(
        r#"(
        version: 2,
        name: "scrub",
        parameters: [(name: "scrub", min: 0.0, max: 1.0, smoothing_secs: 0.001)],
        layers: [(
            name: "grains",
            granular: Some((
                sound: "src",
                grain_secs: 0.002,
                density_hz: 1000.0,
                position_curves: [(parameter: "scrub", points: [(0.0, 0.0), (1.0, 1.0)])],
            )),
        )],
    )"#,
    )
    .expect("parses");
    let src = grain_source();
    let patch = AudioPatch::from_def(def, resolve_with(HashMap::from([("src", clip(src.clone()))])))
        .expect("resolves");
    let mut audio = headless();
    let id = audio.spawn_patch(patch).expect("spawns");
    audio.tick(FRAME);
    let _ = audio.render_offline(BLOCK).expect("render");
    audio.patch_set_param(id, "scrub", 0.5).expect("scrub exists");
    audio.tick(FRAME);
    let block = left(&audio.render_offline(BLOCK).expect("render"));
    // Grains start every 48 frames from frame 0 of the first block, so the second block's first
    // grain starts at frame 16 (816 = 17 · 48) and reads from the new position, 0.5 · 4800 =
    // 2400. The last grain of the first block (from position 0) sounds until frame 64; from there
    // on only new grains sound.
    let first_onset = (48 - BLOCK % 48) % 48;
    assert_eq!(first_onset, 16);
    let new = overlap_add(&src, 2_400, 96, 48, BLOCK - first_onset);
    let want: Vec<f32> = new[64 - first_onset..].iter().map(|s| s * scale).collect();
    assert_close(&block[64..], &want, 1e-5, "grains from the new position");
}

/// Despawning a patch ends its granular voice: after the 10 ms declick fade the output is
/// silent, and the grain count stops moving.
#[test]
fn despawning_a_patch_silences_its_granular_layer() {
    let def = AudioPatchDef::from_ron(
        r#"(
        version: 2,
        name: "gravel",
        parameters: [],
        layers: [(name: "grains", granular: Some((sound: "src", grain_secs: 0.002, density_hz: 1000.0)))],
    )"#,
    )
    .expect("parses");
    let patch = AudioPatch::from_def(def, resolve_with(HashMap::from([("src", clip(grain_source()))])))
        .expect("resolves");
    let mut audio = headless();
    let id = audio.spawn_patch(patch).expect("spawns");
    audio.tick(FRAME);
    let playing = left(&audio.render_offline(BLOCK).expect("render"));
    assert!(playing.iter().any(|s| s.abs() > 1e-3), "the grains are heard");
    audio.despawn_patch(id);
    assert!(audio.patch_state().is_empty(), "the patch is gone");
    audio.tick(FRAME);
    let fading = left(&audio.render_offline(BLOCK).expect("render"));
    // 10 ms at 48 kHz is 480 frames of fade; nothing after it.
    assert!(fading[480..].iter().all(|s| *s == 0.0), "silent after the fade");
    audio.tick(FRAME);
    let after = left(&audio.render_offline(BLOCK).expect("render"));
    assert!(after.iter().all(|s| *s == 0.0), "and stays silent");
}

// ----------------------------------------------------------------------------- reflected bindings

/// A gameplay component the patch reads through reflection.
#[derive(Component, Reflect, Default)]
#[reflect(Component)]
struct Rover {
    speed: f32,
    gear: i32,
    boosting: bool,
}

const BOUND_PATCH: &str = r#"(
    version: 2,
    name: "rover",
    parameters: [
        (name: "speed", min: 0.0, max: 1.0, smoothing_secs: 0.001,
         binding: Some((component: "Rover", field: "speed", scale: 0.1, offset: 0.0))),
        (name: "gear", min: 0.0, max: 5.0, binding: Some((component: "Rover", field: "gear"))),
        (name: "boost", min: 0.0, max: 1.0, binding: Some((component: "Rover", field: "boosting"))),
    ],
    layers: [(
        name: "hum",
        sound: Some("dc"),
        volume_curves: [(parameter: "speed", points: [(0.0, -60.0), (1.0, 0.0)])],
    )],
)"#;

fn bound_world(rover: Rover) -> (World, Entity, u64) {
    let def = AudioPatchDef::from_ron(BOUND_PATCH).expect("parses");
    let patch = AudioPatch::from_def(def, resolve_with(HashMap::from([("dc", clip(vec![0.5; 4_800]))])))
        .expect("resolves");
    let mut world = World::new();
    let registry = AppTypeRegistry::default();
    registry.write().register::<Rover>();
    world.insert_resource(registry);
    let mut audio = headless();
    let id = audio.spawn_patch(patch).expect("spawns");
    world.insert_resource(audio);
    let entity = world.spawn((rover, AudioPatchSource { patch: id })).id();
    (world, entity, id)
}

/// Bound parameters follow their reflected fields (`f32` scaled, `i32`, `bool`), and the render
/// equals the same patch driven by `patch_set_param` with the same value, sample for sample.
#[test]
fn a_bound_parameter_follows_its_reflected_field() {
    let (mut world, entity, id) = bound_world(Rover {
        speed: 5.0,
        gear: 3,
        boosting: true,
    });
    patch_binding_sync(&mut world);
    let targets: Vec<f32> = world.resource::<Audio>().patch_state()[0]
        .parameters
        .iter()
        .map(|p| p.target)
        .collect();
    assert_eq!(targets, [0.5, 3.0, 1.0], "speed 5 × 0.1, gear 3, boosting true");
    assert_eq!(
        world.resource::<Audio>().patch_state()[0].parameters[0]
            .binding
            .as_deref(),
        Some("Rover.speed")
    );

    // The bound render against the same patch set from code.
    let mut control = headless();
    let def = AudioPatchDef::from_ron(BOUND_PATCH).expect("parses");
    let patch = AudioPatch::from_def(def, resolve_with(HashMap::from([("dc", clip(vec![0.5; 4_800]))])))
        .expect("resolves");
    let control_id = control.spawn_patch(patch).expect("spawns");
    control.patch_set_param(control_id, "speed", 0.5).expect("speed");
    let mut bound_out = Vec::new();
    let mut control_out = Vec::new();
    for _ in 0..4 {
        world.resource_mut::<Audio>().tick(FRAME);
        control.tick(FRAME);
        bound_out.extend(left(
            &world
                .resource_mut::<Audio>()
                .render_offline(BLOCK)
                .expect("render"),
        ));
        control_out.extend(left(&control.render_offline(BLOCK).expect("render")));
    }
    assert_eq!(
        bound_out, control_out,
        "bound and set-from-code render the same samples"
    );
    // -30 dB at speed 0.5 (half way along the -60..0 curve) on a 0.5 DC loop, once settled.
    let settled = bound_out[3 * BLOCK + 400].abs();
    let want = 0.5 * db_to_gain(-30.0) * mixer_scale();
    assert!(
        (settled - want).abs() < want * 0.02,
        "settled level {settled}, want {want}"
    );

    // The field moves: the parameter follows on the next sync.
    world.get_mut::<Rover>(entity).expect("rover").speed = 8.0;
    patch_binding_sync(&mut world);
    let speed = &world.resource::<Audio>().patch_state()[0].parameters[0];
    assert!((speed.target - 0.8).abs() < 1e-6, "{}", speed.target);
    assert_eq!(world.resource::<PatchBindingState>().reads(), 6);
    let _ = id;
}

/// A source entity without the bound component leaves the parameters alone and counts failures;
/// it never panics.
#[test]
fn a_missing_component_leaves_the_parameter_and_counts_a_failure() {
    let (mut world, entity, _) = bound_world(Rover::default());
    world.entity_mut(entity).remove::<Rover>();
    patch_binding_sync(&mut world);
    patch_binding_sync(&mut world);
    let state = world.resource::<PatchBindingState>();
    assert_eq!((state.reads(), state.failures()), (0, 6));
    let params = &world.resource::<Audio>().patch_state()[0].parameters;
    assert!(params.iter().all(|p| p.target == 0.0), "{params:?}");
}

// ----------------------------------------------------------------------------- migrator

/// The first fenced RON block of a markdown file.
fn ron_block(markdown: &str) -> String {
    let start = markdown.find("```ron\n").expect("a ron block") + "```ron\n".len();
    let end = start + markdown[start..].find("```").expect("closed");
    markdown[start..end].to_string()
}

/// Every v1 fixture in the repository.
fn v1_fixtures() -> Vec<(&'static str, String)> {
    vec![
        (
            "regia_audio tests/fixtures/hover_engine",
            include_str!("fixtures/hover_engine.audiopatch.ron").to_string(),
        ),
        (
            "regia_agent fuzz-target fixture",
            include_str!("../../regia_agent/src/fuzz_targets/fixtures/hover_engine.audiopatch.ron")
                .to_string(),
        ),
        (
            "regia_agent format-corpus v1",
            include_str!("../../regia_agent/tests/fixtures/format-corpus/audiopatch/v1.ron").to_string(),
        ),
        (
            "docs/formats/audio-patch-v1.md example",
            ron_block(include_str!("../../../docs/formats/audio-patch-v1.md")),
        ),
    ]
}

/// Every v1 fixture migrates to v2 with its meaning unchanged (the same layers, all loops or key
/// sets, no v2 fields), saves as v2 and reads back equal; migrating again changes nothing.
#[test]
fn every_v1_fixture_migrates_to_v2_and_round_trips() {
    for (name, text) in v1_fixtures() {
        assert!(text.contains("version: 1"), "{name} is a v1 file");
        // The file read as-is (serde only, no migration): what v1 said.
        let as_written: AudioPatchDef = ron::from_str(&text).expect("plain serde read");
        assert_eq!(as_written.version, 1);
        let def = AudioPatchDef::from_ron(&text).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(def.version, PATCH_VERSION, "{name}: migrated");
        assert_eq!(PATCH_VERSION, 2);
        assert!(!def.uses_v2_fields(), "{name}: nothing new is invented");
        for layer in &def.layers {
            let kind = layer.kind().unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(
                matches!(kind, PatchLayerKind::Loop | PatchLayerKind::Keys),
                "{name}"
            );
        }
        let saved = def.to_ron().expect("saves");
        assert!(saved.contains("version: 2"), "{name}: saved as v2");
        let back = AudioPatchDef::from_ron(&saved).expect("reloads");
        assert_eq!(back, def, "{name}: round trip");
        assert_eq!(back.to_ron().expect("saves again"), saved, "{name}: idempotent");
        // Nothing but the version changed: every parameter, layer, key, curve and path survives.
        let mut expected = as_written;
        expected.version = PATCH_VERSION;
        assert_eq!(def, expected, "{name}: only the version moves");
    }
}

/// A file that says version 1 but uses a v2 field is refused by the migrator (it is a v2 file
/// with a wrong number), with a message that says what to do.
#[test]
fn a_v1_file_with_a_v2_field_is_refused() {
    let text = ONE_SHOT_PATCH.replace("version: 2", "version: 1");
    let error = AudioPatchDef::from_ron(&text).expect_err("refused");
    assert!(matches!(error, AudioError::Migration(_)), "{error}");
    assert!(error.to_string().contains("version: 2"), "{error}");
}

/// The v2 example fixture parses with one layer of each kind and resolves every sound.
#[test]
fn the_v2_example_has_every_layer_kind_and_resolves() {
    let def = AudioPatchDef::from_ron(include_str!("fixtures/rover.audiopatch.ron")).expect("v2 example");
    assert_eq!(def.version, 2);
    let kinds: Vec<PatchLayerKind> = def.layers.iter().map(|l| l.kind().expect("one kind")).collect();
    assert_eq!(
        kinds,
        [
            PatchLayerKind::Loop,
            PatchLayerKind::Granular,
            PatchLayerKind::OneShot,
            PatchLayerKind::OneShot
        ]
    );
    let patch = AudioPatch::from_def(def, |_| Ok(clip(vec![0.1; 480]))).expect("resolves");
    assert_eq!(patch.layer_sounds.len(), 2, "motor loop and gravel source");
    assert_eq!(patch.key_sounds.len(), 3, "two clunks and the horn");
    let mut audio = headless();
    let id = audio.spawn_patch(patch).expect("spawns");
    assert_eq!(audio.patch_trigger(id, "horn"), Ok(true));
    assert!(matches!(
        audio.patch_trigger(id, "motor"),
        Err(AudioError::Invalid(_))
    ));
}

/// Bad v2 layers are load errors, not panics: two kinds at once, an empty one-shot list, an
/// undeclared trigger parameter, a zero grain length or step.
#[test]
fn invalid_v2_layers_are_rejected() {
    let base = |layer: &str| {
        format!(
            r#"(version: 2, name: "x", parameters: [(name: "p", min: 0.0, max: 1.0)], layers: [{layer}])"#
        )
    };
    let cases = [
        r#"(name: "both", sound: Some("s"), one_shot: Some((sounds: ["s"], trigger: Manual)))"#,
        r#"(name: "none")"#,
        r#"(name: "empty", one_shot: Some((sounds: [], trigger: Manual)))"#,
        r#"(name: "undeclared", one_shot: Some((sounds: ["s"], trigger: Rising(parameter: "q", threshold: 0.5))))"#,
        r#"(name: "step", one_shot: Some((sounds: ["s"], trigger: Every(parameter: "p", step: 0.0))))"#,
        r#"(name: "grain", granular: Some((sound: "s", grain_secs: 0.0)))"#,
        r#"(name: "density", granular: Some((sound: "s", density_hz: -1.0)))"#,
        r#"(name: "curve", granular: Some((sound: "s", position_curves: [(parameter: "q", points: [])])))"#,
    ];
    for case in cases {
        let def = AudioPatchDef::from_ron(&base(case)).expect("the shape parses");
        let result = AudioPatch::from_def(def, |_| Ok(clip(vec![0.1; 16])));
        assert!(result.is_err(), "{case} must be rejected");
    }
}
