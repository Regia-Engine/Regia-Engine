// SPDX-License-Identifier: MPL-2.0
//! Project launch profiles v1 (P-342, ADR-7843). Desktop tool data, never a runtime dependency.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Project-relative path of the launch profile collection.
pub const FILE: &str = "config/launch_profiles.ron";
/// Current launch-profile format version. No earlier version exists.
pub const FORMAT_VERSION: u32 = 1;

/// One named recipe for the editor launcher.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchProfile {
    /// Human-readable unique name (1–128 bytes, no control characters).
    pub name: String,
    /// `standalone` or `android`.
    pub target: String,
    /// `dev`, `shipping`, or a named project build profile.
    pub build_profile: String,
    /// `incremental`, `full`, or `skip` (packaging may still cook required content).
    pub cook: String,
    /// Whether to package before deployment/start.
    pub package: bool,
    /// Android device serial; `None` chooses the only usable device.
    pub device: Option<String>,
    /// Optional level path under the project's assets directory.
    pub start_level: Option<String>,
    /// Application arguments, already separated into tokens. Never shell text.
    pub extra_arguments: Vec<String>,
}

impl Default for LaunchProfile {
    fn default() -> Self {
        Self {
            name: "New profile".into(),
            target: "standalone".into(),
            build_profile: "dev".into(),
            cook: "incremental".into(),
            package: false,
            device: None,
            start_level: None,
            extra_arguments: Vec::new(),
        }
    }
}

impl LaunchProfile {
    /// Checks the portable data contract (target-specific launch support is checked separately).
    ///
    /// # Errors
    /// Empty names, invalid choices, unsafe relative paths, control characters or excessive input.
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() || self.name.len() > 128 || self.name.chars().any(char::is_control) {
            return Err("Profile name must contain 1–128 bytes without control characters".into());
        }
        if !matches!(self.target.as_str(), "standalone" | "android") {
            return Err("Target is standalone or android".into());
        }
        if self.build_profile.is_empty()
            || self.build_profile.len() > 128
            || !self
                .build_profile
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err("Build profile must be a name containing letters, digits, _ or -".into());
        }
        if !matches!(self.cook.as_str(), "incremental" | "full" | "skip") {
            return Err("Cook is incremental, full or skip".into());
        }
        if let Some(level) = &self.start_level
            && (level.is_empty()
                || level.len() > 4096
                || level.starts_with('/')
                || level.contains(['\\', ':'])
                || level.split('/').any(|part| part == ".." || part.is_empty())
                || level.chars().any(char::is_control))
        {
            return Err("Start level must be a relative path under assets/ without ..".into());
        }
        if let Some(device) = &self.device
            && (device.is_empty() || device.len() > 512 || device.chars().any(char::is_control))
        {
            return Err("Device must be a nonempty adb serial without control characters".into());
        }
        if self.extra_arguments.len() > 128
            || self
                .extra_arguments
                .iter()
                .any(|a| a.len() > 4096 || a.contains('\0'))
        {
            return Err("At most 128 arguments, each at most 4096 bytes without NUL".into());
        }
        Ok(())
    }
}

/// The complete `config/launch_profiles.ron` document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchProfiles {
    /// Explicit format version (currently 1).
    pub format_version: u32,
    /// Profiles in user order. Names are unique and case-sensitive.
    pub profiles: Vec<LaunchProfile>,
}

impl Default for LaunchProfiles {
    fn default() -> Self {
        Self {
            format_version: FORMAT_VERSION,
            profiles: Vec::new(),
        }
    }
}

impl LaunchProfiles {
    /// Validates the collection, rejecting older/unsupported versions and duplicate names.
    ///
    /// # Errors
    /// A bad version, duplicate name, more than 256 profiles or an invalid profile.
    pub fn validate(&self) -> Result<(), String> {
        if self.format_version != FORMAT_VERSION {
            return Err(format!(
                "launch_profiles format_version {} is unsupported; expected {FORMAT_VERSION}",
                self.format_version
            ));
        }
        if self.profiles.len() > 256 {
            return Err("At most 256 launch profiles are supported".into());
        }
        let mut names = BTreeSet::new();
        for profile in &self.profiles {
            profile.validate()?;
            if !names.insert(&profile.name) {
                return Err(format!("Duplicate launch profile {:?}", profile.name));
            }
        }
        Ok(())
    }

    /// Parses and validates a v1 document. Future versions are refused without rewriting it.
    ///
    /// # Errors
    /// Malformed RON or failed validation.
    pub fn from_ron(text: &str) -> Result<Self, String> {
        if text.len() > 4 * 1024 * 1024 {
            return Err("Launch profiles exceed 4 MiB".into());
        }
        let file: Self = ron::from_str(text).map_err(|e| e.to_string())?;
        file.validate()?;
        Ok(file)
    }

    /// Serializes a valid collection.
    ///
    /// # Errors
    /// Invalid data or serialization failure.
    pub fn to_ron(&self) -> Result<String, String> {
        self.validate()?;
        let text = ron::ser::to_string_pretty(self, regia_core::ron_pretty()).map_err(|e| e.to_string())?;
        if text.len() > 4 * 1024 * 1024 {
            return Err("Launch profiles exceed 4 MiB".into());
        }
        Ok(text)
    }
}

/// Serde field lists for the frozen-format checker; no enum wire spellings are implicit.
#[must_use]
pub fn schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<LaunchProfiles>(),
        regia_core::schema::serde_field_names::<LaunchProfile>(),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_profiles_refuse_duplicates_paths_and_future_versions() {
        let profile = LaunchProfile::default();
        let mut file = LaunchProfiles {
            profiles: vec![profile.clone()],
            ..Default::default()
        };
        assert_eq!(
            LaunchProfiles::from_ron(&file.to_ron().expect("write")).expect("read"),
            file
        );
        file.profiles.push(profile);
        assert!(file.validate().is_err());
        file.profiles.pop();
        file.profiles[0].start_level = Some("../outside.level.ron".into());
        assert!(file.validate().is_err());
        assert!(
            LaunchProfiles::from_ron("(format_version:2,profiles:[])")
                .expect_err("future")
                .contains('2')
        );
        assert!(LaunchProfiles::from_ron("(format_version:1,profiles:[],unknown:true)").is_err());
    }
}
