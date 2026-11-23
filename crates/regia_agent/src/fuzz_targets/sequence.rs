// SPDX-License-Identifier: MPL-2.0
//! The evaluating fuzz target of `.seq.ron` v2 and v3 (ADR-2135, ADR-2405): past the parse and the round trip of the
//! `sequence` target, every input that loads is sampled the way a player and the curve editor use it.
//!
//! The loader builds a sequence library around the input (its own sub-sequence names resolve to a
//! small built-in child that holds Bezier keys, a clip, a sound and a section that names the input
//! again, and the input itself is in the library, so cycles are reachable), checks the tree,
//! samples it on a fixed grid and at every key, section and marker time, flattens its markers and
//! targets, resolves every curve's tangent slopes (exercised, not checked: they are finite by
//! construction) and runs a fixed list of section edits on every
//! section track. A non-finite sampled value, a clip weight or sound fade outside `0..=1`, a
//! non-finite clip or sound time, or an edited sequence that no longer validates is an engine bug
//! (`bug: `); a sequence the tree check refuses still samples (refused parts are skipped). v3 adds
//! the time scale, spatial and repeat edits to the list, a pass report (`sub_passes`) at every
//! sample time whose local times must be finite, and spatial sounds must name their emitter.

use std::fmt::Display;

use regia_framework::sequencer::sections::{SectionEdit, apply_section_edit};
use regia_framework::sequencer::{
    AudioSection, AudioSpatial, Channel, ChannelValue, ClipSection, Ease, ScalarKey, Sequence,
    SequenceLibrary, SoundSource, SubRepeat, SubSection, Tangent, TimeScaleMode, Track, Vec3Key,
    VideoSection, VideoTarget,
};

use super::formats::text;
use super::{BUG_PREFIX, FuzzTarget};

/// The library name the input is known by (a sub-sequence section naming it makes a cycle).
const ROOT_NAME: &str = "root.seq.ron";

/// At most this many event times (keys, section ends, markers) are sampled per input, on top of
/// the grid, so one input's cost stays bounded.
const MAX_EVENT_TIMES: usize = 192;

/// Grid samples over the duration.
const GRID: usize = 48;

fn e<E: Display>(err: E) -> String {
    err.to_string()
}

fn bug(message: String) -> String {
    format!("{BUG_PREFIX}{message}")
}

/// The child every sub-sequence name of the input resolves to.
fn child() -> Sequence {
    Sequence {
        name: "child".into(),
        tracks: vec![
            Track {
                name: "glow".into(),
                target: "lamp".into(),
                property: "intensity".into(),
                keys: Channel::Float(vec![
                    ScalarKey {
                        tangent: Tangent::Aligned(4.0),
                        weights: Some((0.8, 0.3)),
                        ..ScalarKey::new(0.0, 0.0, Ease::Bezier)
                    },
                    ScalarKey::new(0.5, 3.0, Ease::Smooth),
                    ScalarKey::new(1.0, 1.0, Ease::Linear),
                ]),
                enabled: true,
                locked: false,
            },
            Track {
                name: "drift".into(),
                target: "lamp".into(),
                property: "transform.position".into(),
                keys: Channel::Vec3(vec![
                    Vec3Key::new(0.0, [0.0, 1.0, 0.0], Ease::Bezier),
                    Vec3Key {
                        tangent: Tangent::Broken([1.0, 0.0, -1.0], [0.0, 2.0, 0.0]),
                        ..Vec3Key::new(1.0, [1.0, 1.5, 0.0], Ease::Bezier)
                    },
                ]),
                enabled: true,
                locked: false,
            },
            Track {
                name: "flicker".into(),
                target: "lamp".into(),
                property: String::new(),
                keys: Channel::Clip(vec![ClipSection {
                    clip: "flicker".into(),
                    start: 0.0,
                    end: 1.0,
                    offset: 0.25,
                    speed: 2.0,
                    looping: true,
                    blend_in: 0.25,
                    blend_out: 0.25,
                    weight: 0.75,
                }]),
                enabled: true,
                locked: false,
            },
            Track {
                name: "hum".into(),
                target: String::new(),
                property: String::new(),
                keys: Channel::Audio(vec![AudioSection {
                    sound: "hum".into(),
                    start: 0.1,
                    end: 0.9,
                    offset: 0.0,
                    volume_db: -12.0,
                    fade_in: 0.2,
                    fade_out: 0.2,
                    bus: Some("Sfx".into()),
                    time_scale: 1.0,
                    spatial: None,
                    time_scale_mode: TimeScaleMode::Varispeed,
                }]),
                enabled: true,
                locked: false,
            },
            Track {
                name: "wall".into(),
                target: String::new(),
                property: String::new(),
                keys: Channel::Video(vec![VideoSection {
                    video: "movies/hum.webm".into(),
                    start: 0.2,
                    end: 0.8,
                    offset: 0.5,
                    target: VideoTarget::UiImage("lamp".into()),
                    volume_db: -6.0,
                    looping: true,
                }]),
                enabled: true,
                locked: false,
            },
            // Back into the input: the walk must skip it, not loop.
            Track {
                name: "again".into(),
                target: String::new(),
                property: String::new(),
                keys: Channel::Sub(vec![SubSection {
                    sequence: ROOT_NAME.into(),
                    start: 0.0,
                    end: 0.5,
                    offset: 0.0,
                    speed: 1.0,
                    bindings: Default::default(),
                    repeat: Default::default(),
                    loop_length: None,
                }]),
                enabled: true,
                locked: false,
            },
        ],
        ..Sequence::default()
    }
}

/// The times an input is sampled at: a grid over the duration, every event time (capped at
/// [`MAX_EVENT_TIMES`]) and its neighbors one `f32` step away, and the edges.
fn sample_times(sequence: &Sequence) -> Vec<f64> {
    let duration = sequence.duration();
    let mut events: Vec<f32> = Vec::new();
    for track in &sequence.tracks {
        match &track.keys {
            Channel::Float(keys) => events.extend(keys.iter().map(|k| k.time)),
            Channel::Vec3(keys) => events.extend(keys.iter().map(|k| k.time)),
            Channel::Bool(keys) => events.extend(keys.iter().map(|k| k.time)),
            Channel::Clip(s) => events.extend(s.iter().flat_map(|s| [s.start, s.end])),
            Channel::Audio(s) => events.extend(s.iter().flat_map(|s| [s.start, s.end])),
            Channel::Sub(s) => events.extend(s.iter().flat_map(|s| [s.start, s.end])),
            Channel::Video(s) => events.extend(s.iter().flat_map(|s| [s.start, s.end])),
            Channel::Event(keys) => events.extend(keys.iter().map(|k| k.time)),
        }
    }
    events.extend(sequence.markers.iter().map(|m| m.time));
    events.truncate(MAX_EVENT_TIMES);
    let mut times: Vec<f64> = (0..=GRID).map(|i| duration * i as f64 / GRID as f64).collect();
    for t in events {
        let next = f32::from_bits(t.to_bits().saturating_add(1));
        let prev = if t > 0.0 {
            f32::from_bits(t.to_bits() - 1)
        } else {
            0.0
        };
        times.extend([prev, t, next].map(f64::from));
    }
    times.extend([-1.0, f64::NAN, f64::INFINITY, duration + 1.0]);
    times
}

fn check_value(value: ChannelValue) -> bool {
    match value {
        ChannelValue::Float(v) => v.is_finite(),
        ChannelValue::Vec3(v) => v.iter().all(|c| c.is_finite()),
        ChannelValue::Bool(_) => true,
    }
}

/// Samples `sequence` through `library` at every time of [`sample_times`]; the first broken
/// invariant is the error.
fn check_samples(sequence: &Sequence, library: &SequenceLibrary) -> Result<(), String> {
    for time in sample_times(sequence) {
        let frame = sequence.sample_with(time, library);
        if !frame.time.is_finite() {
            return Err(bug(format!("sampled time {} at {time}", frame.time)));
        }
        for v in &frame.values {
            if !check_value(v.value) {
                return Err(bug(format!("track `{}` sampled {:?} at {time}", v.name, v.value)));
            }
        }
        for clip in &frame.clips {
            for b in &clip.blends {
                if !(b.weight.is_finite() && (0.0..=1.0).contains(&b.weight) && b.clip_time.is_finite()) {
                    return Err(bug(format!(
                        "clip `{}` of track `{}`: weight {} clip time {} at {time}",
                        b.clip, clip.name, b.weight, b.clip_time
                    )));
                }
            }
        }
        for pass in sequence.sub_passes(time, library) {
            if !pass.local_time.is_finite() || pass.pass_length.is_some_and(|l| !(l.is_finite() && l > 0.0)) {
                return Err(bug(format!(
                    "sub-sequence `{}` of track `{}`: local time {} pass length {:?} at {time}",
                    pass.sequence, pass.track, pass.local_time, pass.pass_length
                )));
            }
        }
        for s in &frame.sounds {
            let named =
                matches!(s.spatial.map(|p| &p.source), Some(SoundSource::Binding(_))) == s.emitter.is_some();
            if !named {
                return Err(bug(format!(
                    "sound `{}` of track `{}`: spatial {:?} but emitter {:?} at {time}",
                    s.sound, s.track, s.spatial, s.emitter
                )));
            }
            let fine = s.position.is_finite()
                && s.rate.is_finite()
                && s.rate > 0.0
                && s.remaining.is_finite()
                && s.fade.is_finite()
                && (0.0..=1.0).contains(&s.fade)
                && s.volume_db.is_finite();
            if !fine {
                return Err(bug(format!(
                    "sound `{}` of track `{}`: position {} rate {} fade {} remaining {} at {time}",
                    s.sound, s.track, s.position, s.rate, s.fade, s.remaining
                )));
            }
        }
        for v in &frame.videos {
            let fine = v.position.is_finite()
                && v.rate.is_finite()
                && v.rate > 0.0
                && v.volume_db.is_finite()
                && v.binding.is_some() == v.target.binding().is_some();
            if !fine {
                return Err(bug(format!(
                    "video `{}` of track `{}`: position {} rate {} volume {} binding {:?} at {time}",
                    v.video, v.track, v.position, v.rate, v.volume_db, v.binding
                )));
            }
        }
    }
    Ok(())
}

/// The fixed section edits run on every section track (each may be refused; one that lands must
/// leave a valid sequence).
fn section_edits(first: f32, end: f32) -> [SectionEdit; 13] {
    [
        SectionEdit::VideoTarget {
            index: 0,
            target: VideoTarget::Player("hero".into()),
        },
        SectionEdit::VideoLoop {
            index: 0,
            looping: true,
        },
        SectionEdit::TimeScale { index: 0, scale: 1.5 },
        SectionEdit::Spatial {
            index: 0,
            spatial: Some(AudioSpatial::new(SoundSource::Binding("hero".into()))),
        },
        SectionEdit::Repeat {
            index: 0,
            repeat: SubRepeat::UntilStop,
            loop_length: Some(0.25),
        },
        SectionEdit::Move {
            index: 0,
            start: first + 0.5,
        },
        SectionEdit::TrimStart {
            index: 0,
            start: 0.5 * (first + end),
        },
        SectionEdit::TrimEnd {
            index: 0,
            end: end + 0.25,
        },
        SectionEdit::Duplicate { index: 0, start: end },
        SectionEdit::Speed { index: 0, speed: 1.5 },
        SectionEdit::Offset {
            index: 0,
            offset: 0.125,
        },
        SectionEdit::Ramps {
            index: 0,
            rise: 0.0,
            fall: 0.0,
        },
        SectionEdit::Remove { index: 0 },
    ]
}

fn check_section_edits(sequence: &Sequence) -> Result<(), String> {
    for (i, track) in sequence.tracks.iter().enumerate() {
        if track.keys.is_keyed() {
            continue;
        }
        let span = match &track.keys {
            Channel::Clip(s) => s.first().map(|s| (s.start, s.end)),
            Channel::Audio(s) => s.first().map(|s| (s.start, s.end)),
            Channel::Sub(s) => s.first().map(|s| (s.start, s.end)),
            Channel::Video(s) => s.first().map(|s| (s.start, s.end)),
            Channel::Float(_) | Channel::Vec3(_) | Channel::Bool(_) | Channel::Event(_) => None,
        };
        let Some((start, end)) = span else { continue };
        let mut edited = sequence.clone();
        for edit in section_edits(start, end) {
            let before = edited.tracks[i].keys.clone();
            match apply_section_edit(&mut edited.tracks[i].keys, edit.clone()) {
                Ok(_) => {
                    if let Err(err) = edited.validate() {
                        return Err(bug(format!(
                            "track `{}`: {edit:?} left an invalid sequence: {err}",
                            track.name
                        )));
                    }
                }
                Err(_) => {
                    if edited.tracks[i].keys != before {
                        return Err(bug(format!(
                            "track `{}`: a refused {edit:?} changed the track",
                            track.name
                        )));
                    }
                }
            }
        }
    }
    Ok(())
}

/// Parse, build the library, check, sample, edit sections, re-encode.
fn load(s: &str) -> Result<String, String> {
    let root = Sequence::from_ron(s).map_err(e)?;
    let mut library = SequenceLibrary::default();
    library.insert(ROOT_NAME, root.clone());
    for name in root.sub_sequence_names() {
        if library.get(name).is_none() {
            library.insert(name.to_string(), child());
        }
    }
    library.insert("cinematics/fx/sparkle.seq.ron", child());
    // A refused tree (cycles, depth) still samples: the refused parts are skipped.
    let _ = library.check(&root);
    check_samples(&root, &library)?;
    let _ = root.flat_markers(Some(&library));
    let _ = root.target_names(Some(&library));
    let _ = root.markers_between(0.0, root.duration());
    // Every curve's slopes, the way the curve editor draws its handles (finite by construction
    // since ADR-2135; this exercises the resolution on hostile keys).
    for track in &root.tracks {
        for key in 0..track.keys.len().min(MAX_EVENT_TIMES) {
            for c in 0..3 {
                let _ = track.keys.tangent_slopes(key, c);
            }
        }
    }
    check_section_edits(&root)?;
    root.to_ron().map_err(e)
}

/// The evaluating `.seq.ron` target.
pub(super) fn targets() -> Vec<FuzzTarget> {
    vec![text(
        "sequence.eval",
        Some("sequence"),
        regia_framework::sequencer::SEQUENCE_FORMAT_VERSION,
        true,
        || include_str!("fixtures/seq.example.ron").into(),
        load,
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fixture_and_the_child_sample_clean() {
        let fixture = include_str!("fixtures/seq.example.ron");
        let out = load(fixture).expect("fixture");
        assert_eq!(load(&out).expect("again"), out, "round trip");
        child().validate().expect("the child validates");
        let child_text = child().to_ron().expect("ron");
        load(&child_text).expect("the child as an input (a cycle through itself)");
    }

    #[test]
    fn extreme_keys_are_not_bugs() {
        let text = "(version: 2, tracks: [(name: \"a\", target: \"t\", property: \"p\", keys: Float([\
            (time: 0.0, value: -3.4e38), (time: 1.0e-45, value: 3.4e38, ease: Bezier), (time: 1.0, value: -3.4e38)]))])";
        load(text).expect("finite samples");
    }
}
