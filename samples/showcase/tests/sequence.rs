// SPDX-License-Identifier: MIT-0
//! The showcase intro sequence (`example_scene/cinematics/intro.seq.ron`, `.seq.ron` v2, ADR-2135)
//! without a GPU: it reads its two-level sub-sequence tree from disk, uses every v2 track kind,
//! samples the same bits on every run (pinned), plays on named entities through the sequencer, and
//! its render job (`intro.render_job.ron`) binds what it plays. The render itself is
//! `regia_agent/tests/render_out_showcase_sequence.rs` (lavapipe).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use regia_agent::render_job::RenderJob;
use regia_app::prelude::*;
use regia_framework::sequencer::{
    Binding, Channel, ChannelValue, ClockSource, Ease, SEQUENCE_FORMAT_VERSION, Sequence, SequenceBindings,
    SequenceLibrary, SequencePlayer, SequencerPlugin, Tangent,
};
use regia_render::{DirectionalLight, PointLight};
use regia_scene::Transform;

const INTRO: &str = "cinematics/intro.seq.ron";

/// FNV-1a over every sampled bit of the intro at 30 fps, frames 0..=180 (see
/// `sampling_is_deterministic_and_pinned`). It moves only when the files or the evaluator change.
const INTRO_SAMPLE_HASH: u64 = 0x232e_ddfb_9d20_90e1;

fn scene_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("example_scene")
}

fn read(name: &str) -> Result<String, String> {
    std::fs::read_to_string(scene_dir().join(name)).map_err(|e| format!("{name}: {e}"))
}

/// The intro and its sub-sequence library, read from disk the way the editor and render jobs do.
fn intro() -> (Sequence, SequenceLibrary) {
    let root = Sequence::from_ron(&read(INTRO).expect("intro")).expect("the intro parses");
    let subs = SequenceLibrary::read_tree(&root, read, |m| m).expect("the sub-sequence tree");
    let mut library = SequenceLibrary::default();
    for (name, sub) in subs {
        library.insert(name, sub);
    }
    library.check(&root).expect("the tree checks");
    (root, library)
}

#[test]
fn the_intro_uses_every_v2_track_kind_and_reads_its_tree_from_disk() {
    let (root, library) = intro();
    assert_eq!(
        root.version, SEQUENCE_FORMAT_VERSION,
        "the v2 files load as the current version (v6 since ADR-8105 (c); ADR-2405, ADR-2862, ADR-3197); the pinned \
         hash below proves they sample the same"
    );
    let kinds: BTreeSet<&str> = root.tracks.iter().map(|t| t.keys.kind()).collect();
    assert_eq!(
        kinds,
        BTreeSet::from(["Audio", "Bool", "Clip", "Float", "Sub", "Vec3"]),
        "every v2 track kind"
    );
    let names: Vec<&str> = library.names().collect();
    assert_eq!(
        names,
        ["cinematics/fx/sparkle.seq.ron", "cinematics/fx/twinkle.seq.ron"]
    );
    // Bezier keys with weighted handles and broken tangents; a sub-sequence with offset and speed.
    let weighted = root.tracks.iter().any(|t| match &t.keys {
        Channel::Vec3(keys) => keys.iter().any(|k| k.ease == Ease::Bezier && k.weights.is_some()),
        _ => false,
    });
    let broken = root.tracks.iter().any(|t| match &t.keys {
        Channel::Float(keys) => keys.iter().any(|k| matches!(k.tangent, Tangent::Broken(..))),
        _ => false,
    });
    assert!(weighted && broken);
    let Some(Channel::Sub(sparkles)) = root.tracks.iter().find(|t| t.name == "sparkle").map(|t| &t.keys)
    else {
        panic!("the sparkle track");
    };
    assert_eq!((sparkles[1].offset, sparkles[1].speed), (0.25, 1.5));
    // The sparkle's marker fires on the intro's clock once per section, named after the intro's
    // track: 2.5 + 0.4 and 4.5 + (0.4 - 0.25) / 1.5.
    let flashes: Vec<f64> = root
        .flat_markers(Some(&library))
        .into_iter()
        .filter(|m| m.name == "sparkle/flash")
        .map(|m| m.time)
        .collect();
    assert_eq!(flashes.len(), 2, "{flashes:?}");
    assert!(
        (flashes[0] - 2.9).abs() < 1.0e-5 && (flashes[1] - 4.6).abs() < 1.0e-5,
        "{flashes:?}"
    );
    // What the intro binds: the sparkle's `lamp` plays on the beacon.
    let targets = root.target_names(Some(&library));
    assert_eq!(targets, ["beacon", "keeper", "sun"], "{targets:?}");
    // Re-saving is stable.
    let text = root.to_ron().expect("ron");
    assert_eq!(Sequence::from_ron(&text).expect("again"), root);
}

/// Every sampled bit of one instant, in order, into `hash` (FNV-1a).
fn hash_frame(hash: &mut u64, root: &Sequence, library: &SequenceLibrary, time: f64) -> usize {
    let mut mix = |bits: u64| {
        for byte in bits.to_le_bytes() {
            *hash ^= u64::from(byte);
            *hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    };
    let frame = root.sample_with(time, library);
    let mut deepest = 0;
    for v in &frame.values {
        deepest = deepest.max(v.depth);
        mix(v.track as u64);
        match v.value {
            ChannelValue::Float(x) => mix(u64::from(x.to_bits())),
            ChannelValue::Vec3(x) => x.iter().for_each(|c| mix(u64::from(c.to_bits()))),
            ChannelValue::Bool(b) => mix(u64::from(b)),
        }
    }
    for clip in &frame.clips {
        for b in &clip.blends {
            mix(b.section as u64);
            mix(u64::from(b.weight.to_bits()));
            mix(b.clip_time.to_bits());
        }
    }
    for s in &frame.sounds {
        s.path.iter().for_each(|&p| mix(p as u64));
        mix(s.position.to_bits());
        mix(s.rate.to_bits());
        mix(u64::from(s.fade.to_bits()));
    }
    deepest
}

#[test]
fn sampling_is_deterministic_and_pinned() {
    let (root, library) = intro();
    let run = || {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        let mut deepest = 0;
        for frame in 0..=180u32 {
            deepest = deepest.max(hash_frame(&mut hash, &root, &library, f64::from(frame) / 30.0));
        }
        (hash, deepest)
    };
    let (first, deepest) = run();
    assert_eq!(run(), (first, deepest), "two runs agree");
    assert_eq!(deepest, 2, "the twinkle plays two levels down");
    // The twinkle's tint reaches the beacon (lamp renamed by the intro's section).
    let frame = root.sample_with(3.2, &library);
    let tint = frame
        .values
        .iter()
        .find(|v| v.name == "tint")
        .expect("the twinkle plays at 3.2 s");
    assert_eq!(
        (tint.target, tint.property, tint.depth),
        ("beacon", "light.color", 2)
    );
    assert_eq!(
        first, INTRO_SAMPLE_HASH,
        "the intro samples differently: {first:#018x}"
    );
}

#[test]
fn the_intro_plays_on_named_entities() {
    let (root, library) = intro();
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, SequencerPlugin))
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_nanos(
            33_333_333,
        )));
    let beacon = app
        .world_mut()
        .spawn((
            Transform::default(),
            PointLight::default(),
            regia_scene::components::Name::new("beacon"),
        ))
        .id();
    let sun = app
        .world_mut()
        .spawn(DirectionalLight {
            direction: [0.2, -0.6, -0.4],
            color: [1.0, 0.97, 0.92],
            illuminance: 0.0,
            cast_shadows: false,
            static_shadow: false,
        })
        .id();
    let mut player = SequencePlayer::new(root.clone());
    player.set_library(&library);
    player.clock = ClockSource::External;
    player.play();
    let bindings = SequenceBindings::default()
        .with("beacon", Binding::Entity(beacon))
        .with("sun", Binding::Entity(sun));
    let entity = app.world_mut().spawn((player, bindings)).id();
    app.update();
    for (time, frame) in [(1.0, 30u32), (3.2, 96), (4.9, 147)] {
        app.world_mut()
            .get_mut::<SequencePlayer>(entity)
            .expect("player")
            .set_external_time(f64::from(frame) / 30.0);
        app.update();
        // The player's clock is in nanoseconds: sample where it actually is.
        let at = app.world().get::<SequencePlayer>(entity).expect("player").time();
        assert!((at - f64::from(frame) / 30.0).abs() < 1.0e-6, "{at}");
        let expected = root.sample_with(at, &library);
        let value = |name: &str| {
            expected
                .values
                .iter()
                .rev()
                .find(|v| v.name == name)
                .map(|v| v.value)
                .unwrap_or_else(|| panic!("{name} at {time}"))
        };
        let transform = *app.world().get::<Transform>(beacon).expect("transform");
        let ChannelValue::Vec3(position) = value("beacon-rise") else {
            panic!("vec3")
        };
        assert_eq!(transform.translation.to_array(), position, "at {time}");
        let ChannelValue::Float(lux) = value("sun-up") else {
            panic!("float")
        };
        let light = app.world().get::<DirectionalLight>(sun).expect("sun");
        assert_eq!(light.illuminance, lux, "at {time}");
    }
    // At 3.2 s the first sparkle flares the beacon's light, two levels of renames down.
    let frame = root.sample_with(3.2, &library);
    assert!(
        frame
            .values
            .iter()
            .any(|v| v.name == "flare" && v.target == "beacon")
    );
}

#[test]
fn the_render_job_plays_the_intro_and_binds_its_beacon() {
    let job =
        RenderJob::from_ron(&read("cinematics/intro.render_job.ron").expect("job")).expect("the job parses");
    assert_eq!(
        job.version, 4,
        "the v2 file loads as v4 (ADR-2757); the binding asserts below prove it migrates"
    );
    assert_eq!(job.sequence.as_deref(), Some(INTRO));
    let (root, library) = intro();
    let targets = root.target_names(Some(&library));
    for target in job.bindings.keys() {
        assert!(
            targets.contains(target),
            "{target} is not a target of the intro: {targets:?}"
        );
    }
    for marker in ["rise", "pulse"] {
        assert!(root.marker(marker).is_some(), "{marker}");
    }
}
