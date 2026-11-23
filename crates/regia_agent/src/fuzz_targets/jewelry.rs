// SPDX-License-Identifier: MPL-2.0
//! The product viewer contracts Regia owns (`jewelry.<name>/1` JSON, ADR-2535): each file goes through
//! `regia_configurator::check_bytes`, the envelope, the shipped schema and the contract checks, as
//! `configurator.contract_check` and every viewer load do.
//!
//! The kit builder's `option_graph` and `piece_recipe` have no target here: the engine ships no schema for
//! them, so a load without the published one stops at `contract_no_schema`.

use regia_configurator::{CONTRACT_MAJOR, Interface, check_bytes};

use super::FuzzTarget;
use super::formats::text;

/// Checked, then re-encoded as compact JSON. A build or package hash covers the canonical form of the
/// content, not these bytes, so the encoding cannot break it.
fn load(interface: Interface, input: &str) -> Result<String, String> {
    check_bytes(input.as_bytes(), Some(interface), None).map_err(|errors| {
        errors
            .iter()
            .map(|e| e.to_json().to_string())
            .collect::<Vec<_>>()
            .join("; ")
    })?;
    let value: serde_json::Value = serde_json::from_str(input).map_err(|e| e.to_string())?;
    serde_json::to_string(&value).map_err(|e| e.to_string())
}

pub(super) fn targets() -> Vec<FuzzTarget> {
    vec![
        text(
            "jewelry.part_kit",
            Some("jewelry-part-kit"),
            CONTRACT_MAJOR,
            true,
            || include_str!("fixtures/jewelry-part-kit.json").into(),
            |s| load(Interface::PartKit, s),
        ),
        text(
            "jewelry.material_library",
            Some("jewelry-material-library"),
            CONTRACT_MAJOR,
            true,
            || include_str!("fixtures/jewelry-material-library.json").into(),
            |s| load(Interface::MaterialLibrary, s),
        ),
        text(
            "jewelry.build",
            Some("jewelry-build"),
            CONTRACT_MAJOR,
            true,
            || include_str!("fixtures/jewelry-build.json").into(),
            |s| load(Interface::Build, s),
        ),
        text(
            "jewelry.product_package",
            Some("jewelry-product-package"),
            CONTRACT_MAJOR,
            true,
            || include_str!("fixtures/jewelry-product-package.json").into(),
            |s| load(Interface::ProductPackage, s),
        ),
    ]
}

#[cfg(test)]
mod tests {
    /// The seeds are copies (ADR-1485: compiled code reads nothing outside its crate), so they must
    /// stay the configurator's committed fixtures.
    #[test]
    fn seeds_are_byte_identical_to_the_configurators_fixtures() {
        let pairs = [
            (
                include_str!("fixtures/jewelry-part-kit.json"),
                include_str!(
                    "../../../regia_configurator/tests/fixtures/jewelry-part-kit/kit-shared-basic.json"
                ),
            ),
            (
                include_str!("fixtures/jewelry-material-library.json"),
                include_str!(
                    "../../../regia_configurator/tests/fixtures/jewelry-material-library/library-basic.json"
                ),
            ),
            (
                include_str!("fixtures/jewelry-build.json"),
                include_str!("../../../regia_configurator/tests/fixtures/jewelry-build/build-house-a.json"),
            ),
            (
                include_str!("fixtures/jewelry-product-package.json"),
                include_str!(
                    "../../../regia_configurator/tests/fixtures/jewelry-product-package/package-basic.json"
                ),
            ),
        ];
        for (seed, fixture) in pairs {
            assert_eq!(seed, fixture, "copy the fixture over src/fuzz_targets/fixtures/");
        }
    }
}
