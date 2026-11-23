// SPDX-License-Identifier: MPL-2.0
//! Spec 12 conformance checks for behaviour that had no direct test (lane spec-04-12-assets,
//! ADR-5611).

use regia_asset::AssetId;

/// Spec 12 § 2: a first import's id is a random RFC 4122 version-4 GUID (version nibble 4,
/// variant bits `10`), and two draws differ.
#[test]
fn a_new_asset_id_is_a_random_version_4_guid() {
    let mut seen = std::collections::HashSet::new();
    for _ in 0..64 {
        let id = AssetId::new_random();
        let value = id.0.as_u128();
        assert_eq!((value >> 76) & 0xF, 4, "version nibble of {id}");
        assert_eq!((value >> 62) & 0b11, 0b10, "variant bits of {id}");
        assert!(!id.is_nil());
        assert!(seen.insert(id), "{id} drawn twice");
    }
    // The text form shows the same version digit (8-4-4-4-12 layout, digit 13 of the hex).
    let text = AssetId::new_random().to_string().replace('-', "");
    assert_eq!(text.len(), 32);
    assert_eq!(&text[12..13], "4", "{text}");
}
