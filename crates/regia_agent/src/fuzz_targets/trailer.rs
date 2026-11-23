// SPDX-License-Identifier: MPL-2.0
//! The `trailer` target (ADR-3281): a `.trailer.ron` (`docs/formats/trailer.md`). The loader parses,
//! validates and writes it back as the runtime and `regia upgrade-project` do. The `bike_ron` target
//! (ADR-8630) does the same for a `.bike.ron` (`docs/formats/bike-ron.md`), from its frozen corpus file.

use super::FuzzTarget;
use super::formats::text;

pub(super) fn targets() -> Vec<FuzzTarget> {
    vec![
        text(
            "trailer",
            Some("trailer"),
            regia_physics::vehicle_trailer_asset::TRAILER_FORMAT_VERSION,
            true,
            || include_str!("fixtures/trailer.ron").into(),
            |s| {
                let asset = regia_physics::vehicle_trailer_asset::TrailerAsset::from_ron(s)
                    .map_err(|e| e.to_string())?;
                asset.validate().map_err(|e| e.to_string())?;
                asset.to_ron().map_err(|e| e.to_string())
            },
        ),
        text(
            "bike_ron",
            Some("bike"),
            regia_physics::vehicle_bike::asset::BIKE_FORMAT_VERSION,
            true,
            || include_str!("../../tests/fixtures/format-corpus/bike/v1.ron").into(),
            |s| {
                let asset =
                    regia_physics::vehicle_bike::asset::BikeAsset::from_ron(s).map_err(|e| e.to_string())?;
                asset.validate().map_err(|e| e.to_string())?;
                asset.to_ron().map_err(|e| e.to_string())
            },
        ),
    ]
}
