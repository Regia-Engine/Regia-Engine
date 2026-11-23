// SPDX-License-Identifier: MIT-0
//! A dynamic extension for the extension host tests (`specs/57` § 8 tests 11 and 12).
//!
//! `ext_fixture.marker` answers the marker baked at build time (`EXT_FIXTURE_MARKER`), which is how
//! a test tells two builds apart, and a counter carried across reloads by `on_unload`/`on_load`.
//! `build` fails while the file `fail-build` exists in the extension directory. A system spawns
//! one `LevelFeatures` entity per frame naming the marker, an engine type every copy of the library
//! shares, so a host can count which copy ran.

use std::sync::Arc;

use regia_framework::extension::prelude::*;
use serde_json::{Value, json};

#[cfg(not(feature = "dynamic-extension"))]
use regia_app::prelude::*;
#[cfg(feature = "dynamic-extension")]
use regia_dylib::prelude::*;

/// The marker of this build.
pub const MARKER: &str = match option_env!("EXT_FIXTURE_MARKER") {
    Some(marker) => marker,
    None => "unset",
};

/// State a reload carries over.
#[derive(Resource, Default)]
pub struct FixtureCounter(pub u32);

/// Leaves one entity per frame saying which build ran.
fn tick(mut commands: Commands) {
    commands.spawn(LevelFeatures {
        features: vec![format!("tick:{MARKER}")],
    });
}

/// The fixture extension.
#[derive(Default)]
pub struct ExtFixture;

impl RegiaExtension for ExtFixture {
    fn id(&self) -> &str {
        "ext_fixture"
    }

    fn build(&self, cx: &mut ExtensionContext<'_>) -> Result<(), ExtensionError> {
        if cx.dir().join("fail-build").exists() {
            return Err(ExtensionError::BuildFailed("the fixture was told to fail".into()));
        }
        cx.add_systems(Update, tick)?;
        cx.register_agent_method(AgentMethodDesc {
            name: "ext_fixture.marker".into(),
            summary: "The marker of this build and the counter carried across reloads.".into(),
            access: MethodAccess::Read,
            // No params: the host refuses any field before the handler runs (`invalid_params`).
            params_schema: json!({ "type": "object", "additionalProperties": false }),
            result_schema: json!({ "type": "object" }),
            handler: Arc::new(|world: &mut World, _params: Value| {
                let count = world.get_resource::<FixtureCounter>().map_or(0, |c| c.0);
                Ok(json!({ "marker": MARKER, "count": count }))
            }),
        })
    }

    fn on_unload(&self, world: &mut World) -> Option<DynamicStateBlob> {
        let count = world.get_resource::<FixtureCounter>().map_or(0, |c| c.0);
        Some(DynamicStateBlob(count.to_le_bytes().to_vec()))
    }

    fn on_load(&self, world: &mut World, prior: Option<DynamicStateBlob>) {
        let count = prior
            .and_then(|blob| <[u8; 4]>::try_from(blob.0.as_slice()).ok())
            .map_or(0, u32::from_le_bytes);
        world.insert_resource(FixtureCounter(count + 1));
    }
}

regia_framework::extension!(ExtFixture);
