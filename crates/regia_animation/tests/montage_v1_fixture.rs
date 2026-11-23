// SPDX-License-Identifier: MPL-2.0
//! audit-v035-formats 16: the montage v1 → v2 coverage was one inline string, which can be edited in
//! the same commit as a field rename; a checked-in v1 file cannot drift that way
//! (`book/changing-a-content-format.md`, step 3).

use regia_animation::montage::{MONTAGE_FORMAT_VERSION, Montage};

#[test]
fn the_checked_in_v1_montage_loads_at_the_current_version() {
    let text = include_str!("fixtures/hit.v1.montage.ron");
    assert!(text.contains("format_version: 1"), "the fixture stays v1");
    let montage = Montage::from_ron_str(text).expect("a v1 montage loads");
    assert_eq!(montage.format_version, MONTAGE_FORMAT_VERSION);
    assert_eq!(montage.name, "hit");
    assert_eq!(montage.rate, 1.25);
    assert_eq!(montage.tracks[0].segments.len(), 2);
    assert_eq!(montage.tracks[0].segments[1].loops, 2);
    assert_eq!(montage.sections.len(), 2);
    assert!(!montage.auto_blend_out && !montage.root_motion);
    // v2 fields take their defaults.
    assert!(montage.warps.is_empty());
    // Saved, it reloads equal at the current version.
    let saved = montage.to_ron_string().expect("save");
    assert_eq!(Montage::from_ron_str(&saved).expect("reload"), montage);
}
