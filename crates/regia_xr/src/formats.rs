// SPDX-License-Identifier: MPL-2.0
//! Schema fingerprints of the versioned XR files, for `cargo xtask check-formats`
//! (`docs/specs/12-assets.md` § 4a, ADR-2316): `config/xr.ron`, the action manifest
//! (`actions.xr.ron`) and simulator scripts (`.xrsim.ron`).
//!
//! Each fingerprint lists the serde field names of the format's root struct and of every struct it
//! reaches, plus the variant names and payload shapes of every enum it reaches. Tuple structs
//! (`BindingDecl`) and tuples carry no field names; their arity is fixed by the code that reads them.
//! Every format is at v1 (no migrators), so each `*_migration_sources` is empty.

use regia_core::schema::{join_field_lists, serde_field_names, serde_variant_names, serde_variant_shapes};
use serde::Deserialize;

/// The variant names and payload shapes of the serde enum `T`, as one fingerprint list.
fn enum_list<T: Deserialize<'static>>() -> Vec<String> {
    let mut list = serde_variant_names::<T>();
    list.extend(serde_variant_shapes::<T>());
    list
}

/// `config/xr.ron` (`XrConfig`, `docs/formats/xr-config-ron.md`).
#[must_use]
pub fn xr_config_schema_fingerprint() -> String {
    use crate::comfort::{ComfortSettings, DominantHand};
    use crate::config::{Foveation, Mirror, XrBackendKind, XrConfig};
    join_field_lists(&[
        serde_field_names::<XrConfig>(),
        enum_list::<XrBackendKind>(),
        enum_list::<crate::space::TrackingSpace>(),
        enum_list::<Foveation>(),
        enum_list::<Mirror>(),
        serde_field_names::<ComfortSettings>(),
        enum_list::<DominantHand>(),
    ])
}

/// `config/xr.ron` is at its first version: no migration steps.
#[must_use]
pub fn xr_config_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The XR action manifest (`XrActionManifest`, `docs/formats/xr-actions-ron.md`).
#[must_use]
pub fn xr_actions_schema_fingerprint() -> String {
    use crate::input::{ActionDecl, ActionSetDecl, ActionType, ProfileBindings, XrActionManifest};
    join_field_lists(&[
        serde_field_names::<XrActionManifest>(),
        serde_field_names::<ActionSetDecl>(),
        serde_field_names::<ActionDecl>(),
        enum_list::<ActionType>(),
        serde_field_names::<ProfileBindings>(),
    ])
}

/// The XR action manifest is at its first version: no migration steps.
#[must_use]
pub fn xr_actions_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// `.xrsim.ron` (`XrSimScript`, `docs/formats/xrsim-ron.md`).
#[must_use]
pub fn xrsim_schema_fingerprint() -> String {
    use crate::sim::{ScriptPose, ScriptState, ScriptValue, SimTick, XrSimScript};
    join_field_lists(&[
        serde_field_names::<XrSimScript>(),
        serde_field_names::<SimTick>(),
        serde_field_names::<ScriptPose>(),
        enum_list::<ScriptValue>(),
        enum_list::<ScriptState>(),
    ])
}

/// `.xrsim.ron` is at its first version: no migration steps.
#[must_use]
pub fn xrsim_migration_sources() -> Vec<u32> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprints_list_every_reached_type() {
        let all = [
            xr_config_schema_fingerprint(),
            xr_actions_schema_fingerprint(),
            xrsim_schema_fingerprint(),
        ];
        for f in &all {
            assert!(!f.split(';').any(str::is_empty), "an empty list: {f}");
        }
        assert!(all[0].contains("vignette_ease_frames"));
        assert!(all[1].contains("optional,map"));
        assert!(
            all[2].contains("tuple(2)"),
            "Vec2 is a two-float tuple variant: {}",
            all[2]
        );
    }
}
