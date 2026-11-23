// SPDX-License-Identifier: MPL-2.0
//! The editor's importer fuzz targets (ADR-2084): `import.gltf`, `import.obj`, `import.bvh`, `import.ply`,
//! `import.stl`, `import.psd`, `import.font_subset`, `import.usd`, `import.usdz` (ADR-3235), `import.svg`,
//! `import.aseprite` `import.materialx`, `import.ldtk` and `import.dds`, the importers of
//! `regia_asset_pipeline`, and `import.gltf_ext` and `import.meshopt` for the glTF data-layout extensions and
//! the compressed-stream decoder (ADR-3016), with a valid fixture each (`regia_asset_pipeline::fuzz_entry`).
//! The pipeline is editor-side and the runtime registry must not depend on it, so the editor adds them to the
//! agent's `FuzzTargetExtensions`; `fuzz.targets` lists them with `source: "extension"` and `fuzz.run` runs
//! them.

use regia_agent::fuzz_targets::{FuzzInput, FuzzTarget, FuzzTargetExtensions};
use regia_asset_pipeline::fuzz_entry;
use regia_ecs::prelude::*;

/// The importer targets: those of ADR-2084 and later lanes, and the entries of `fuzz_entry::more_entries` (ADR-3262).
pub fn targets() -> Vec<FuzzTarget> {
    let mut all = Vec::from([
        FuzzTarget {
            name: "import.gltf",
            format: None,
            format_version: 2,
            round_trip: false,
            input: FuzzInput::Bytes {
                fixture: fuzz_entry::glb_fixture,
                load: fuzz_entry::glb_load,
            },
        },
        FuzzTarget {
            name: "import.gltf_ext",
            format: None,
            format_version: 0,
            round_trip: false,
            input: FuzzInput::Bytes {
                fixture: fuzz_entry::gltf_ext_fixture,
                load: fuzz_entry::glb_load,
            },
        },
        FuzzTarget {
            name: "import.meshopt",
            format: None,
            format_version: 0,
            round_trip: false,
            input: FuzzInput::Bytes {
                fixture: fuzz_entry::meshopt_fixture,
                load: fuzz_entry::meshopt_load,
            },
        },
        FuzzTarget {
            name: "import.obj",
            format: None,
            format_version: 0,
            round_trip: false,
            input: FuzzInput::Text {
                fixture: fuzz_entry::obj_fixture,
                load: fuzz_entry::obj_load,
            },
        },
        FuzzTarget {
            name: "import.bvh",
            format: None,
            format_version: 0,
            round_trip: false,
            input: FuzzInput::Text {
                fixture: fuzz_entry::bvh_fixture,
                load: fuzz_entry::bvh_load,
            },
        },
        FuzzTarget {
            name: "import.ply",
            format: None,
            format_version: 0,
            round_trip: false,
            input: FuzzInput::Bytes {
                fixture: fuzz_entry::ply_fixture,
                load: fuzz_entry::ply_load,
            },
        },
        FuzzTarget {
            name: "import.stl",
            format: None,
            format_version: 0,
            round_trip: false,
            input: FuzzInput::Text {
                fixture: fuzz_entry::stl_fixture,
                load: fuzz_entry::stl_load,
            },
        },
        FuzzTarget {
            name: "import.psd",
            format: None,
            format_version: 0,
            round_trip: false,
            input: FuzzInput::Bytes {
                fixture: fuzz_entry::psd_fixture,
                load: fuzz_entry::psd_load,
            },
        },
        FuzzTarget {
            name: "import.font_subset",
            format: None,
            format_version: 0,
            round_trip: false,
            input: FuzzInput::Bytes {
                fixture: fuzz_entry::font_fixture,
                load: fuzz_entry::font_load,
            },
        },
        FuzzTarget {
            name: "import.svg",
            format: None,
            format_version: 0,
            round_trip: false,
            input: FuzzInput::Text {
                fixture: fuzz_entry::svg_fixture,
                load: fuzz_entry::svg_load,
            },
        },
        FuzzTarget {
            name: "import.aseprite",
            format: None,
            format_version: 0,
            round_trip: false,
            input: FuzzInput::Bytes {
                fixture: fuzz_entry::aseprite_fixture,
                load: fuzz_entry::aseprite_load,
            },
        },
        FuzzTarget {
            name: "import.ldtk",
            format: None,
            format_version: 0,
            round_trip: false,
            input: FuzzInput::Text {
                fixture: fuzz_entry::ldtk_fixture,
                load: fuzz_entry::ldtk_load,
            },
        },
        FuzzTarget {
            name: "import.materialx",
            format: None,
            format_version: 0,
            round_trip: false,
            input: FuzzInput::Text {
                fixture: fuzz_entry::materialx_fixture,
                load: fuzz_entry::materialx_load,
            },
        },
        FuzzTarget {
            name: "import.dds",
            format: None,
            format_version: 0,
            round_trip: false,
            input: FuzzInput::Bytes {
                fixture: fuzz_entry::dds_fixture,
                load: fuzz_entry::dds_load,
            },
        },
        // The USD importer (ADR-3235): text layers, and packages holding a binary crate layer.
        FuzzTarget {
            name: "import.usd",
            format: None,
            format_version: 0,
            round_trip: false,
            input: FuzzInput::Text {
                fixture: fuzz_entry::usd_fixture,
                load: fuzz_entry::usd_load,
            },
        },
        FuzzTarget {
            name: "import.usdz",
            format: None,
            format_version: 0,
            round_trip: false,
            input: FuzzInput::Bytes {
                fixture: fuzz_entry::usdz_fixture,
                load: fuzz_entry::usdz_load,
            },
        },
    ]);
    all.extend(fuzz_entry::more_entries().into_iter().map(|entry| FuzzTarget {
        name: entry.name,
        format: None,
        format_version: 0,
        round_trip: false,
        input: match entry.kind {
            fuzz_entry::ImportEntryKind::Bytes { fixture, load } => FuzzInput::Bytes { fixture, load },
            fuzz_entry::ImportEntryKind::Text { fixture, load } => FuzzInput::Text { fixture, load },
        },
    }));
    all
}

/// Adds [`targets`] to the app's `FuzzTargetExtensions`.
pub fn install(world: &mut World) {
    let mut extensions = world.get_resource_or_init::<FuzzTargetExtensions>();
    for target in targets() {
        if let Err(e) = extensions.add(target) {
            tracing::warn!(target: "regia_editor", "fuzz target not added: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_importer_targets_are_added_and_run_clean() {
        let mut world = World::new();
        install(&mut world);
        let extensions = world.resource::<FuzzTargetExtensions>();
        assert_eq!(extensions.targets().len(), 16 + fuzz_entry::more_entries().len());
        for t in extensions.targets() {
            let report = regia_agent::fuzz_targets::run(t, None, 5, 60).expect("built-in fixture");
            assert!(report.is_clean(), "{}: {report:?}", t.name);
            assert!(report.accepted > 0, "{}: nothing loaded: {report:?}", t.name);
        }
    }
}
