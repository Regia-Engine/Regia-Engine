// SPDX-License-Identifier: MPL-2.0
//! Renaming a binding: every track, spatial sound section, video section and sub-sequence binding
//! table of a sequence that refers to one binding name moves to a new one in a single edit (`.seq.ron` v3,
//! ADR-2405). The editor's section strip runs it as one undo step (`regia_editor::seq_sections`)
//! and the agent method `sequencer.rename_binding` runs it on a player's sequence.
//!
//! A binding name is what a player resolves to an entity (see
//! [`Sequence::target_names`](super::Sequence::target_names)). A sequence refers to a name `from`
//! in four places:
//!
//! - a keyed or clip track whose `target` is `from`;
//! - a spatial sound section that emits from `Binding(from)`;
//! - a video section shown on the binding `from` (v4);
//! - a sub-sequence section whose `bindings` table maps a sub-sequence target to `from`, or that
//!   plays a sub-sequence binding `from` with no entry for it (an unlisted name keeps its name).
//!   The first gets its value changed; the second gets an entry `from -> to`, so the
//!   sub-sequence's own file is never edited.
//!
//! The second sub-sequence case needs the sub-sequence, so it is looked up in the
//! [`SequenceLibrary`]; a section whose sub-sequence the library does not hold only has its
//! explicit entries renamed, and is counted in [`BindingRename::unresolved`].
//!
//! ```
//! use regia_framework::sequencer::rename::rename_binding;
//! use regia_framework::sequencer::Sequence;
//!
//! let mut sequence = Sequence::from_ron(r#"(version: 3, tracks: [
//!     (name: "walk", target: "hero", property: "transform.position",
//!         keys: Vec3([(time: 0.0, value: (0.0, 0.0, 0.0))])),
//! ])"#).expect("valid");
//! let done = rename_binding(&mut sequence, None, "hero", "guard").expect("renamed");
//! assert_eq!((done.tracks, done.references()), (1, 1));
//! assert_eq!(sequence.tracks[0].target, "guard");
//! ```

use super::asset::{Channel, Sequence, SoundSource};
use super::edit::EditError;
use super::sample::SequenceLibrary;

/// What a [`rename_binding`] changed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BindingRename {
    /// Keyed and clip tracks whose `target` was renamed.
    pub tracks: usize,
    /// Spatial sound sections whose emitter binding was renamed.
    pub sounds: usize,
    /// Video sections whose target binding was renamed (v4).
    pub videos: usize,
    /// Sub-sequence sections whose binding table changed (an entry's value renamed, or an entry
    /// added for a name the sub-sequence binds implicitly).
    pub sub_sections: usize,
    /// Sub-sequence sections whose sub-sequence is not in the library: only their explicit
    /// entries could be renamed.
    pub unresolved: usize,
}

impl BindingRename {
    /// Every reference changed.
    #[must_use]
    pub fn references(&self) -> usize {
        self.tracks + self.sounds + self.videos + self.sub_sections
    }
}

fn refuse<T>(code: &'static str, message: impl Into<String>) -> Result<T, EditError> {
    Err(EditError {
        code,
        message: message.into(),
    })
}

/// Renames binding `from` to `to` everywhere `sequence` refers to it (see the module docs), as
/// one edit: on an error nothing changed. Disabled tracks are renamed too (they still name the
/// binding). Errors: `seq_binding_empty` (an empty name), `seq_binding_same` (`from == to`),
/// `seq_binding_in_use` (`to` already names a binding of the sequence: the two would merge),
/// `seq_binding_unknown` (nothing refers to `from`), and `seq_section_out_of_range` should the
/// renamed sequence fail validation.
pub fn rename_binding(
    sequence: &mut Sequence,
    library: Option<&SequenceLibrary>,
    from: &str,
    to: &str,
) -> Result<BindingRename, EditError> {
    if from.is_empty() || to.is_empty() {
        return refuse("seq_binding_empty", "binding names must not be empty");
    }
    if from == to {
        return refuse("seq_binding_same", format!("`{from}` is already called `{to}`"));
    }
    let names = binding_names(sequence, library);
    if names.iter().any(|n| n == to) {
        return refuse(
            "seq_binding_in_use",
            format!(
                "`{to}` already names a binding of this sequence; renaming `{from}` to it would merge the two"
            ),
        );
    }
    let mut edited = sequence.clone();
    let mut done = BindingRename::default();
    for track in &mut edited.tracks {
        match &mut track.keys {
            Channel::Float(_)
            | Channel::Vec3(_)
            | Channel::Bool(_)
            | Channel::Clip(_)
            | Channel::Event(_) => {
                if track.target == from {
                    track.target = to.to_string();
                    done.tracks += 1;
                }
            }
            Channel::Audio(sections) => {
                for section in sections {
                    if let Some(spatial) = section.spatial.as_mut()
                        && let SoundSource::Binding(name) = &mut spatial.source
                        && name == from
                    {
                        *name = to.to_string();
                        done.sounds += 1;
                    }
                }
            }
            Channel::Video(sections) => {
                for section in sections {
                    if let Some(name) = section.target.binding_mut()
                        && name == from
                    {
                        *name = to.to_string();
                        done.videos += 1;
                    }
                }
            }
            Channel::Sub(sections) => {
                for section in sections {
                    let mut changed = false;
                    for value in section.bindings.values_mut() {
                        if value == from {
                            *value = to.to_string();
                            changed = true;
                        }
                    }
                    match library.and_then(|l| l.get(&section.sequence).map(|sub| (l, sub))) {
                        Some((library, sub)) => {
                            let implicit = sub
                                .target_names(Some(library))
                                .into_iter()
                                .any(|n| n == from && !section.bindings.contains_key(&n));
                            if implicit {
                                section.bindings.insert(from.to_string(), to.to_string());
                                changed = true;
                            }
                        }
                        None => done.unresolved += 1,
                    }
                    done.sub_sections += usize::from(changed);
                }
            }
        }
    }
    if done.references() == 0 {
        return refuse(
            "seq_binding_unknown",
            format!("nothing in the sequence refers to the binding `{from}`"),
        );
    }
    if let Err(e) = edited.validate() {
        return refuse("seq_section_out_of_range", e.to_string());
    }
    *sequence = edited;
    Ok(done)
}

/// Every binding name `sequence` refers to, disabled tracks included, through the sub-sequences
/// `library` holds: [`Sequence::target_names`] of the sequence with every track enabled.
#[must_use]
pub fn binding_names(sequence: &Sequence, library: Option<&SequenceLibrary>) -> Vec<String> {
    let mut all = sequence.clone();
    for track in &mut all.tracks {
        track.enabled = true;
    }
    all.target_names(library)
}

#[cfg(test)]
mod tests {
    use super::super::asset::fixtures::{float_track, sequence};
    use super::super::asset::{AudioSection, AudioSpatial, SubSection, Track};
    use super::*;

    fn audio(binding: &str) -> Track {
        let mut s = AudioSection::new("step", 0.0, 1.0);
        s.spatial = Some(AudioSpatial::new(SoundSource::Binding(binding.into())));
        Track {
            name: "steps".into(),
            target: String::new(),
            property: String::new(),
            keys: Channel::Audio(vec![s, AudioSection::new("theme", 0.0, 2.0)]),
            enabled: true,
            locked: false,
        }
    }

    fn sub(sections: Vec<SubSection>) -> Track {
        Track {
            name: "fx".into(),
            target: String::new(),
            property: String::new(),
            keys: Channel::Sub(sections),
            enabled: true,
            locked: false,
        }
    }

    fn library() -> SequenceLibrary {
        let mut library = SequenceLibrary::default();
        library.insert(
            "inner",
            sequence(
                vec![float_track("glow", "lamp", "intensity", 1.0), audio("hero")],
                vec![],
            ),
        );
        library
    }

    #[test]
    fn a_rename_reaches_tracks_sounds_and_sub_sequence_tables_and_keeps_meaning() {
        let mut explicit = SubSection::new("inner", 0.0, 1.0);
        explicit.bindings.insert("lamp".into(), "hero".into());
        let implicit = SubSection::new("inner", 1.0, 2.0);
        let mut disabled = float_track("old", "hero", "scale", 1.0);
        disabled.enabled = false;
        let mut seq = sequence(
            vec![
                float_track("move", "hero", "transform.position", 2.0),
                float_track("other", "door", "angle", 2.0),
                audio("hero"),
                sub(vec![explicit, implicit]),
                disabled,
            ],
            vec![],
        );
        let library = library();
        let before = seq.target_names(Some(&library));
        assert_eq!(before, ["door", "hero", "lamp"]);
        let done = rename_binding(&mut seq, Some(&library), "hero", "guard").expect("renamed");
        assert_eq!(
            done,
            BindingRename {
                tracks: 2,
                sounds: 1,
                videos: 0,
                sub_sections: 2,
                unresolved: 0
            }
        );
        // Every name that was `hero` is `guard`; the others are untouched.
        assert_eq!(seq.target_names(Some(&library)), ["door", "guard", "lamp"]);
        assert_eq!(seq.tracks[0].target, "guard");
        assert_eq!(seq.tracks[1].target, "door");
        assert_eq!(seq.tracks[4].target, "guard", "disabled tracks are renamed too");
        let Channel::Sub(subs) = &seq.tracks[3].keys else {
            unreachable!()
        };
        assert_eq!(subs[0].bindings.get("lamp").map(String::as_str), Some("guard"));
        // The second section plays `inner`, whose spatial sound binds `hero` implicitly.
        assert_eq!(subs[1].bindings.get("hero").map(String::as_str), Some("guard"));
        // The sampled frame binds the same things under the new name.
        let frame = seq.sample_with(0.5, &library);
        let emitters: Vec<_> = frame.sounds.iter().filter_map(|s| s.emitter).collect();
        assert_eq!(
            emitters,
            ["guard", "guard"],
            "the root's and the explicit section's sounds"
        );
        let frame = seq.sample_with(1.5, &library);
        let emitters: Vec<_> = frame.sounds.iter().filter_map(|s| s.emitter).collect();
        assert_eq!(
            emitters,
            ["guard"],
            "the implicit section's sound, through its new entry"
        );
    }

    #[test]
    fn a_rename_reaches_the_target_of_a_video_section() {
        use super::super::asset::{VideoScreenSlot, VideoSection, VideoTarget};
        let on = |target: VideoTarget| VideoSection {
            target,
            ..VideoSection::new("movies/a.webm", 0.0, 1.0)
        };
        let mut seq = sequence(
            vec![Track {
                name: "wall".into(),
                target: String::new(),
                property: String::new(),
                keys: Channel::Video(vec![
                    on(VideoTarget::Screen {
                        binding: "tv".into(),
                        slot: VideoScreenSlot::Emissive,
                    }),
                    on(VideoTarget::Player("tv".into())),
                    on(VideoTarget::UiImage("tv".into())),
                    on(VideoTarget::Player("other".into())),
                    on(VideoTarget::default()),
                ]),
                enabled: true,
                locked: false,
            }],
            vec![],
        );
        assert_eq!(binding_names(&seq, None), ["other", "tv"]);
        let done = rename_binding(&mut seq, None, "tv", "wall_screen").expect("renamed");
        assert_eq!((done.videos, done.references()), (3, 3));
        assert_eq!(binding_names(&seq, None), ["other", "wall_screen"]);
    }

    #[test]
    fn refusals_change_nothing() {
        let mut seq = sequence(
            vec![
                float_track("move", "hero", "transform.position", 2.0),
                float_track("other", "door", "angle", 2.0),
                sub(vec![SubSection::new("missing", 0.0, 1.0)]),
            ],
            vec![],
        );
        let before = seq.clone();
        let code = |seq: &mut Sequence, from: &str, to: &str| {
            rename_binding(seq, None, from, to).expect_err("refused").code
        };
        assert_eq!(code(&mut seq, "", "x"), "seq_binding_empty");
        assert_eq!(code(&mut seq, "hero", ""), "seq_binding_empty");
        assert_eq!(code(&mut seq, "hero", "hero"), "seq_binding_same");
        assert_eq!(code(&mut seq, "hero", "door"), "seq_binding_in_use");
        assert_eq!(code(&mut seq, "nobody", "x"), "seq_binding_unknown");
        assert_eq!(seq, before);
        // A section whose sub-sequence is not loaded is counted.
        let done = rename_binding(&mut seq, None, "hero", "guard").expect("renamed");
        assert_eq!((done.tracks, done.unresolved), (1, 1));
    }
}
