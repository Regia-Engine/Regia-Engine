// SPDX-License-Identifier: MPL-2.0
//! audit-v035-formats 16: the `.seq.ron` v1 → v2 coverage was one inline string, which can be edited
//! in the same commit as a field rename; a checked-in v1 file cannot drift that way
//! (`book/changing-a-content-format.md`, step 3).

use regia_framework::sequencer::{SEQUENCE_FORMAT_VERSION, Sequence};

#[test]
fn the_checked_in_v1_sequence_loads_at_the_current_version() {
    let text = include_str!("fixtures/fade.v1.seq.ron");
    assert!(text.contains("version: 1"), "the fixture stays v1");
    let seq = Sequence::from_ron(text).expect("a v1 sequence loads");
    assert_eq!(seq.version, SEQUENCE_FORMAT_VERSION);
    assert_eq!(seq.name, "fade");
    assert_eq!(seq.fps, 24.0);
    assert_eq!(seq.tracks.len(), 1);
    assert!(seq.tracks[0].enabled, "v2 defaults fill in");
    seq.validate().expect("valid");
    let saved = seq.to_ron().expect("save");
    assert_eq!(Sequence::from_ron(&saved).expect("reload"), seq);
}
