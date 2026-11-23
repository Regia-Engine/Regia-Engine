// SPDX-License-Identifier: MPL-2.0
//! Shipped tool presets (spec 67 § 5.4): graphs with `flags.tool` set that the placement tools
//! bind a generator (or, for a biome preset, a `BiomeArea`) to. A project that does not hold the file yet gets it written the first time a
//! tool session names it, so a session never fails for want of a starter.

/// The project path of the road preset (Path tool).
pub const TOOL_ROAD_PATH: &str = "starter/tool_road.pgraph.ron";
/// The project path of the fence preset (Path tool).
pub const TOOL_FENCE_PATH: &str = "starter/tool_fence.pgraph.ron";
/// The project path of the forest preset (Area tool).
pub const TOOL_FOREST_AREA_PATH: &str = "starter/tool_forest_area.pgraph.ron";
/// The project path of the biome area preset (Area tool, a biome graph).
pub const TOOL_BIOME_AREA_PATH: &str = "starter/tool_biome_area.pgraph.ron";
/// The project path of the rubble preset (Box tool).
pub const TOOL_RUBBLE_BOX_PATH: &str = "starter/tool_rubble_box.pgraph.ron";
/// The project path of the painted props preset (Paint tool).
pub const TOOL_PAINTED_PROPS_PATH: &str = "starter/tool_painted_props.pgraph.ron";

/// `tool_road`: the Path tool's spline becomes a road (junctions, ribbon, markings).
pub const TOOL_ROAD: &str = include_str!("../../starter/tool_road.pgraph.ron");
/// `tool_fence`: posts and panels along the Path tool's spline (`spline.modules`).
pub const TOOL_FENCE: &str = include_str!("../../starter/tool_fence.pgraph.ron");
/// `tool_forest_area`: trees scattered inside the Area tool's outline.
pub const TOOL_FOREST_AREA: &str = include_str!("../../starter/tool_forest_area.pgraph.ron");
/// `tool_biome_area`: the Area tool's outline becomes a `BiomeArea` of the `biome` parameter.
pub const TOOL_BIOME_AREA: &str = include_str!("../../starter/tool_biome_area.pgraph.ron");
/// `tool_rubble_box`: rocks scattered over the Box tool's box `Shape`.
pub const TOOL_RUBBLE_BOX: &str = include_str!("../../starter/tool_rubble_box.pgraph.ron");
/// `tool_painted_props`: a prop on every painted point.
pub const TOOL_PAINTED_PROPS: &str = include_str!("../../starter/tool_painted_props.pgraph.ron");

/// The shipped preset text for a project path, if `path` names one.
#[must_use]
pub fn preset(path: &str) -> Option<&'static str> {
    match path {
        TOOL_ROAD_PATH => Some(TOOL_ROAD),
        TOOL_FENCE_PATH => Some(TOOL_FENCE),
        TOOL_FOREST_AREA_PATH => Some(TOOL_FOREST_AREA),
        TOOL_BIOME_AREA_PATH => Some(TOOL_BIOME_AREA),
        TOOL_RUBBLE_BOX_PATH => Some(TOOL_RUBBLE_BOX),
        TOOL_PAINTED_PROPS_PATH => Some(TOOL_PAINTED_PROPS),
        _ => None,
    }
}

/// Every shipped preset as `(project path, text)`.
#[must_use]
pub fn all() -> [(&'static str, &'static str); 6] {
    [
        (TOOL_ROAD_PATH, TOOL_ROAD),
        (TOOL_FENCE_PATH, TOOL_FENCE),
        (TOOL_FOREST_AREA_PATH, TOOL_FOREST_AREA),
        (TOOL_BIOME_AREA_PATH, TOOL_BIOME_AREA),
        (TOOL_RUBBLE_BOX_PATH, TOOL_RUBBLE_BOX),
        (TOOL_PAINTED_PROPS_PATH, TOOL_PAINTED_PROPS),
    ]
}

/// The tool kind each shipped preset serves (its `flags.tool`; a test checks they agree).
const KINDS: [(&str, super::tools::ToolKind); 6] = [
    (TOOL_ROAD_PATH, super::tools::ToolKind::Path),
    (TOOL_FENCE_PATH, super::tools::ToolKind::Path),
    (TOOL_FOREST_AREA_PATH, super::tools::ToolKind::Area),
    (TOOL_BIOME_AREA_PATH, super::tools::ToolKind::Area),
    (TOOL_RUBBLE_BOX_PATH, super::tools::ToolKind::Box),
    (TOOL_PAINTED_PROPS_PATH, super::tools::ToolKind::Paint),
];

/// The shipped presets for `kind`, as project paths (no parsing: the panel calls it every frame).
pub fn for_kind(kind: super::tools::ToolKind) -> impl Iterator<Item = &'static str> {
    KINDS.into_iter().filter(move |(_, k)| *k == kind).map(|(p, _)| p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::procgen::tools::{ALL_KINDS, ToolKind};

    /// Spec 67 § 5.4: every shipped preset parses, compiles against the built-in nodes, names its
    /// tool kind, and every tool kind has at least one preset.
    #[test]
    fn every_shipped_preset_compiles_and_every_kind_has_one() {
        let registry = regia_procgen::ProcNodeRegistry::with_builtins();
        for (path, text) in all() {
            let g = regia_procgen::ProcGraph::from_ron(text).unwrap_or_else(|e| panic!("{path}: {e}"));
            let kind = g.flags.tool.as_deref().and_then(ToolKind::parse);
            assert!(kind.is_some(), "{path}");
            assert_eq!(
                KINDS.iter().find(|(p, _)| *p == path).map(|(_, k)| *k),
                kind,
                "{path}"
            );
            if let Err(e) = g.compile(&registry) {
                panic!("{path}: {e}");
            }
        }
        for kind in ALL_KINDS {
            assert!(for_kind(kind).next().is_some(), "{}", kind.name());
        }
        assert_eq!(
            for_kind(ToolKind::Area).collect::<Vec<_>>(),
            vec![TOOL_FOREST_AREA_PATH, TOOL_BIOME_AREA_PATH]
        );
    }
}
