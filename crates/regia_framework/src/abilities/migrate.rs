// SPDX-License-Identifier: MPL-2.0
//! Migrators of the three formats (frozen-format rule, spec 12 § 4a). Every format is at its first
//! version, so every step list is empty; a shape change bumps the version and adds a step here.

pub use super::assets::{
    ABILITY_FORMAT_VERSION, ATTRIBUTES_FORMAT_VERSION, EFFECT_FORMAT_VERSION, ability_migration_sources,
    ability_schema_fingerprint, attributes_migration_sources, attributes_schema_fingerprint,
    effect_migration_sources, effect_schema_fingerprint,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_format_starts_at_version_one_without_steps() {
        assert_eq!(ATTRIBUTES_FORMAT_VERSION, 1);
        assert_eq!(EFFECT_FORMAT_VERSION, 1);
        assert_eq!(ABILITY_FORMAT_VERSION, 1);
        assert!(attributes_migration_sources().is_empty());
        assert!(effect_migration_sources().is_empty());
        assert!(ability_migration_sources().is_empty());
        for fp in [
            attributes_schema_fingerprint(),
            effect_schema_fingerprint(),
            ability_schema_fingerprint(),
        ] {
            assert!(fp.contains("format_version"), "{fp}");
        }
    }
}
