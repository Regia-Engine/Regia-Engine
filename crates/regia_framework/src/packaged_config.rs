// SPDX-License-Identifier: MPL-2.0
//! Startup configuration in `regia package`'s unfrozen `build_info.json` report (ADR-7893).
//!
//! Desktop reads beside the executable, never relative to the working directory. Android reads
//! the APK assets root. Missing reports preserve older startup behavior; malformed fields are
//! diagnosed independently, so a bad render profile does not discard a valid boot list.

use std::path::{Path, PathBuf};

use regia_ecs::prelude::*;
use serde_json::Value;

const FILE: &str = "build_info.json";

/// Package metadata read once at startup by the quality, settings or boot-movie plugin.
///
/// This resource is not persisted in the player's settings. Insert a default resource before
/// those plugins to disable automatic package discovery in a custom host.
#[derive(Resource, Clone, Debug, Default)]
pub struct PackagedConfig {
    data: Value,
    location: Option<String>,
    directory: Option<PathBuf>,
    read_error: Option<String>,
    #[cfg(feature = "client")]
    profile: Option<regia_render::ProfileName>,
    #[cfg(feature = "client")]
    profile_error: Option<String>,
}

impl PackagedConfig {
    /// Reads a staged desktop package directory. Errors are retained in [`Self::error`]; a
    /// missing file is an empty configuration with no error or location.
    #[must_use]
    pub fn read_directory(root: &Path) -> Self {
        let path = root.join(FILE);
        Self::read_result(
            std::fs::read(&path),
            path.display().to_string(),
            Some(root.to_path_buf()),
        )
    }

    fn read_result(bytes: std::io::Result<Vec<u8>>, location: String, directory: Option<PathBuf>) -> Self {
        if bytes
            .as_ref()
            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
        {
            return Self::default();
        }
        let mut config = Self {
            location: Some(location.clone()),
            directory,
            ..Self::default()
        };
        match bytes
            .map_err(|e| e.to_string())
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).map_err(|e| e.to_string()))
        {
            Ok(data) if data.is_object() => config.data = data,
            Ok(_) => config.read_error = Some(format!("{location}: expected a JSON object")),
            Err(error) => config.read_error = Some(format!("{location}: {error}")),
        }
        #[cfg(feature = "client")]
        if let Some(value) = config.data.get("render_profile") {
            // A preset row or a tier with no row of its own (`VeryLow`, ADR-8844).
            config.profile = value.as_str().and_then(regia_render::ProfileName::parse);
            if config.profile.is_none() {
                config.profile_error = Some(format!("{location}: invalid render_profile {value}"));
            }
        }
        config
    }

    #[cfg(any(target_os = "android", test))]
    fn read_mount(mount: &dyn regia_platform::VfsMount) -> Self {
        Self::read_result(mount.read(FILE), "apk:assets/build_info.json".to_string(), None)
    }

    /// The report's desktop path or `apk:assets/build_info.json`; absent when no report exists.
    #[must_use]
    pub fn location(&self) -> Option<&str> {
        self.location.as_deref()
    }

    /// Desktop package directory, if the report came from disk; APK reports have no host path.
    #[must_use]
    pub fn directory(&self) -> Option<&Path> {
        self.directory.as_deref()
    }

    /// A document or render-profile diagnostic. A valid boot table can still be used when only
    /// the render profile is invalid.
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        #[cfg(feature = "client")]
        {
            self.read_error.as_deref().or(self.profile_error.as_deref())
        }
        #[cfg(not(feature = "client"))]
        {
            self.read_error.as_deref()
        }
    }

    /// The exact packaged render preset, independent of player settings and detection. For a
    /// packaged tier name (`VeryLow`) this is the preset it starts from (Desktop Low); read
    /// [`Self::render_profile_name`] for the name itself.
    #[cfg(feature = "client")]
    #[must_use]
    pub fn render_profile(&self) -> Option<regia_render::RenderProfilePreset> {
        self.profile.map(regia_render::ProfileName::preset)
    }

    /// The packaged profile name as written: a preset row or a quality-ladder tier with no row of
    /// its own (`VeryLow`, `Medium`; ADR-8844).
    #[cfg(feature = "client")]
    #[must_use]
    pub fn render_profile_name(&self) -> Option<regia_render::ProfileName> {
        self.profile
    }

    /// The optional boot object, for the media plugin or a custom host to deserialize.
    ///
    /// # Errors
    /// The report could not be read or was not a JSON object. An invalid render profile does not
    /// prevent reading this independent field.
    pub fn boot(&self) -> Result<Option<&Value>, &str> {
        match self.read_error.as_deref() {
            Some(error) => Err(error),
            None => Ok(self.data.get("boot")),
        }
    }

    /// The optional `[game]` object (the pause key and its screen, ADR-10104).
    ///
    /// # Errors
    /// The report could not be read or was not a JSON object.
    pub fn game(&self) -> Result<Option<&Value>, &str> {
        match self.read_error.as_deref() {
            Some(error) => Err(error),
            None => Ok(self.data.get("game")),
        }
    }
}

#[cfg(not(target_os = "android"))]
fn discover(_world: &World) -> PackagedConfig {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(PackagedConfig::read_directory))
        .unwrap_or_default()
}

#[cfg(target_os = "android")]
fn discover(world: &World) -> PackagedConfig {
    let Some(handle) = world.get_resource::<regia_platform::AndroidAppHandle>() else {
        return PackagedConfig::default();
    };
    let mount = regia_platform::ApkAssetMount::new(handle.0.asset_manager(), "");
    PackagedConfig::read_mount(&mount)
}

pub(crate) fn ensure_loaded(world: &mut World) {
    if !world.contains_resource::<PackagedConfig>() {
        let config = discover(world);
        if let Some(error) = config.error() {
            tracing::warn!(target: "regia_framework", "packaged configuration: {error}");
        }
        world.insert_resource(config);
    }
}

/// The package's starting preset while neither a player choice nor an explicit re-detection in
/// this launch supersedes it.
#[cfg(feature = "client")]
pub(crate) fn active_profile(world: &World) -> Option<regia_render::ProfileName> {
    if world
        .get_resource::<crate::settings::GameSettings>()
        .is_some_and(|s| s.graphics.source() == crate::settings::PresetSource::Player)
        || world
            .get_resource::<crate::auto_quality::AutoQuality>()
            .is_some_and(|a| a.last_detection().is_some())
    {
        return None;
    }
    world
        .get_resource::<PackagedConfig>()
        .and_then(PackagedConfig::render_profile_name)
}

/// Origin of the configured starting profile: `player`, `packaged`, `detected`, or
/// `runtime_or_host`. Direct host/agent edits to the live renderer are independent of this origin.
#[cfg(feature = "client")]
#[must_use]
pub fn profile_source(world: &World) -> &'static str {
    let source = world
        .get_resource::<crate::settings::GameSettings>()
        .map(|s| s.graphics.source());
    if source == Some(crate::settings::PresetSource::Player) {
        "player"
    } else if active_profile(world).is_some() {
        "packaged"
    } else if source == Some(crate::settings::PresetSource::Detected) {
        "detected"
    } else {
        "runtime_or_host"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apk_mount_reads_named_metadata_without_directory_enumeration() {
        let root = regia_test_utils::unique_temp_dir("packaged-config-apk-mount");
        std::fs::create_dir_all(&root).expect("root");
        std::fs::write(
            root.join(FILE),
            br#"{"render_profile":"MobileLow","boot":{"movies":["movies/intro.webm"]}}"#,
        )
        .expect("metadata");
        let mount = regia_platform::DirMount::read_only(&root);
        let config = PackagedConfig::read_mount(&mount);
        assert_eq!(config.location(), Some("apk:assets/build_info.json"));
        assert!(config.directory().is_none());
        assert_eq!(
            config.boot().expect("boot").expect("table")["movies"][0],
            "movies/intro.webm"
        );
        #[cfg(feature = "client")]
        assert_eq!(
            config.render_profile(),
            Some(regia_render::RenderProfilePreset::MobileLow)
        );
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    #[cfg(feature = "client")]
    fn invalid_profile_preserves_boot_and_missing_report_is_harmless() {
        let root = regia_test_utils::unique_temp_dir("packaged-config-invalid");
        assert!(PackagedConfig::read_directory(&root).error().is_none());
        let config = PackagedConfig::read_result(
            Ok(br#"{"render_profile":7,"boot":{"movies":[]}}"#.to_vec()),
            "fixture".to_string(),
            None,
        );
        assert!(config.error().is_some());
        assert!(config.render_profile().is_none());
        assert!(config.boot().expect("boot").is_some());
    }

    /// ADR-8844: `regia package` writes `"VeryLow"` from `Regia.toml`; the packaged game reads it
    /// as the tier (no diagnostic), reports Desktop Low as its preset, and `"Low"` stays invalid.
    #[test]
    #[cfg(feature = "client")]
    fn packaged_very_low_is_a_profile_name_and_low_stays_invalid() {
        let config = PackagedConfig::read_result(
            Ok(br#"{"render_profile":"VeryLow"}"#.to_vec()),
            "fixture".to_string(),
            None,
        );
        assert_eq!(config.error(), None);
        assert_eq!(
            config.render_profile_name(),
            Some(regia_render::ProfileName::Tier(
                regia_render::QualityTier::VeryLow
            ))
        );
        assert_eq!(
            config.render_profile(),
            Some(regia_render::RenderProfilePreset::DesktopLow)
        );
        let low = PackagedConfig::read_result(
            Ok(br#"{"render_profile":"Low"}"#.to_vec()),
            "fixture".to_string(),
            None,
        );
        assert!(low.error().is_some_and(|e| e.contains("invalid render_profile")));
        assert!(low.render_profile_name().is_none());
    }
}
