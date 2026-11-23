// SPDX-License-Identifier: MPL-2.0
//! Procedural loops (`docs/formats/loopgen-ron.md`, ADR-299): the documented example renders
//! deterministically and seamlessly, a patch layer can name a `.loopgen.ron`, and the patch plays
//! through the offline backend with no audio device.

use regia_audio::{
    Audio, AudioError, AudioPatch, AudioPatchDef, AudioSettings, LoopGenCache, LoopGenDef, PatchLayerDef,
    PatchParamDef, SoundAsset, SoundAssets, SoundClip,
};

const EXAMPLE: &str = include_str!("../../../docs/formats/loopgen.example.ron");
const RATE: u32 = 48_000;

fn headless() -> Audio {
    Audio::new(&AudioSettings {
        headless: true,
        ..AudioSettings::default()
    })
}

fn layer(name: &str, sound: &str) -> PatchLayerDef {
    PatchLayerDef {
        name: name.into(),
        sound: Some(sound.into()),
        keys: vec![],
        parameter: None,
        volume_db: -6.0,
        volume_curves: vec![],
        pitch_curves: vec![],
        filter_curves: vec![],
        one_shot: None,
        granular: None,
    }
}

fn patch_def(layers: Vec<PatchLayerDef>) -> AudioPatchDef {
    AudioPatchDef {
        version: 1,
        name: "machinery".into(),
        bus: Some("Sfx".into()),
        parameters: vec![PatchParamDef {
            name: "load".into(),
            min: 0.0,
            max: 1.0,
            smoothing_secs: 0.1,
            binding: None,
        }],
        layers,
    }
}

fn no_file(path: &str) -> Result<SoundAsset, AudioError> {
    Err(AudioError::UnknownEntry(path.to_string()))
}

#[test]
fn the_documented_example_is_deterministic_and_seamless() {
    let def = LoopGenDef::from_ron(EXAMPLE).expect("the example parses");
    let a = def.render().expect("renders").stats();
    let b = LoopGenDef::from_ron(EXAMPLE)
        .expect("parses")
        .render()
        .expect("renders")
        .stats();
    assert_eq!(a.hash, b.hash);
    assert_eq!(a.frames, 96_000);
    assert!(a.rms > 0.01, "audible: {}", a.rms);
    assert!(a.peak <= 1.0);
    assert!(
        a.seam_jump <= a.max_step * 1.1 + 2e-4,
        "seam {} vs step {}",
        a.seam_jump,
        a.max_step
    );

    let mut other = def.clone();
    other.seed += 1;
    assert_ne!(a.hash, other.render().expect("renders").stats().hash);
}

#[test]
fn a_patch_layer_names_a_loopgen_and_plays_on_the_offline_backend() {
    let cache = LoopGenCache::default();
    let def = patch_def(vec![
        layer("hum", "audio/machinery.loopgen.ron"),
        layer("hum_again", "audio/Machinery.LOOPGEN.RON"),
        layer("beep", "audio/beep.ogg"),
    ]);
    let patch = AudioPatch::from_def_with_loopgen(
        def,
        &cache,
        |_| Ok(EXAMPLE.to_string()),
        |path| {
            assert_eq!(
                path, "audio/beep.ogg",
                "only non-loopgen paths reach the resolver"
            );
            Ok(SoundAsset::from_clip(
                &SoundClip::new(vec![0.0; 64], RATE).expect("clip"),
            ))
        },
    )
    .expect("resolves");
    // Both loopgen layers share one render.
    assert_eq!((cache.misses(), cache.hits(), cache.len()), (1, 1, 1));
    assert_eq!(patch.layer_sounds.len(), 3);

    let mut audio = headless();
    let id = audio.spawn_patch(patch).expect("the patch spawns");
    audio.patch_set_param(id, "load", 0.5).expect("param");
    audio.tick(1.0 / 60.0);
    let out = audio.render_offline(RATE as usize / 2).expect("offline render");
    let rms = (out.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / out.len() as f64).sqrt();
    assert!(rms > 0.005, "the procedural layer is heard: {rms}");
    audio.despawn_patch(id);
}

#[test]
fn an_invalid_graph_in_a_patch_reports_the_key_path() {
    let bad = EXAMPLE.replace("cutoff_hz: 300.0", "cutoff_hz: 300000.0");
    let cache = LoopGenCache::default();
    let error = AudioPatch::from_def_with_loopgen(
        patch_def(vec![layer("hum", "audio/machinery.loopgen.ron")]),
        &cache,
        |_| Ok(bad.clone()),
        no_file,
    )
    .err()
    .expect("an invalid graph is an error");
    assert!(
        error.to_string().contains("nodes[2].node.Filter.cutoff_hz"),
        "{error}"
    );
}

#[test]
fn sound_assets_register_a_loopgen_by_name() {
    let mut sounds = SoundAssets::default();
    let handle = sounds.add_loopgen("hum", EXAMPLE).expect("registers");
    assert_eq!(sounds.handle("hum"), Some(handle));
    sounds.add_loopgen("hum2", EXAMPLE).expect("registers");
    assert_eq!(sounds.loopgens().misses(), 1, "identical text renders once");
    assert_eq!(sounds.loopgens().hits(), 1);
    assert_eq!(sounds.loopgen_names().count(), 2);
    let sound = sounds.by_name("hum").expect("registered");
    assert!((sound.duration_secs() - 2.0).abs() < 1e-6);
    assert!(sounds.add_loopgen("bad", "(version: 1)").is_err());
}
