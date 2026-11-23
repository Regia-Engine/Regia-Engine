// SPDX-License-Identifier: MPL-2.0
//! Retarget setups: the saved bone map and fixes of one source → target pairing.
//!
//! Specification: `docs/specs/85-retarget-crowds.md` § 2.3. A [`RetargetSetup`]
//! (`.retarget.ron`, format v2, `docs/formats/retarget-ron.md`) says how each side's roles are
//! found (guessed from names, paired by name, from a built-in [`SkeletonProfile`], or an explicit
//! role → joint map, plus per-role overrides), per-role settings (skip, translation mode, rest
//! offset), the translation and root-motion scale rules, facing alignment and the IK goals solved
//! at bake (v2, ADR-8476). [`RetargetSetup::build`] turns it into a [`Retargeter`]. Version 1
//! loads as version 2 with no goals ([`migrate_retarget`]).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::profile::SkeletonProfile;
use crate::retarget::{
    IkGoalSpec, RetargetError, RetargetOptions, RetargetProfile, Retargeter, RoleSettings, RootMotionScale,
    ScaleRule,
};
use crate::skeleton::Skeleton;

/// The `.retarget.ron` format version. Version 1 is the first freeze; version 2 adds `ik_goals`
/// (ADR-8476).
pub const RETARGET_FORMAT_VERSION: u32 = 2;

fn yes() -> bool {
    true
}

/// How one side's roles are found.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum RoleSource {
    /// [`RetargetProfile::guess_humanoid`] from the joint names (default).
    #[default]
    Guess,
    /// Every joint plays the role of its own name ([`RetargetProfile::by_name`]).
    ByName,
    /// A built-in skeleton profile by name (`"generic-humanoid"`, `"vrm"`), matched against the
    /// skeleton ([`SkeletonProfile::retarget_profile`]).
    Profile(String),
    /// An explicit role → joint name map.
    Explicit(BTreeMap<String, String>),
}

/// One side of a setup: where its roles come from, and per-role joint overrides on top.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RigRoles {
    /// The role source.
    #[serde(default)]
    pub roles: RoleSource,
    /// Role → joint name, replacing what the source found.
    #[serde(default)]
    pub overrides: BTreeMap<String, String>,
}

/// The `.retarget.ron` document.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RetargetSetup {
    /// Format version of this file.
    pub format_version: u32,
    /// Setup name (`"mixamo-to-guard"`).
    #[serde(default)]
    pub name: String,
    /// How the source skeleton's roles are found.
    #[serde(default)]
    pub source: RigRoles,
    /// How the target skeleton's roles are found.
    #[serde(default)]
    pub target: RigRoles,
    /// Per-role settings, keyed by role name.
    #[serde(default)]
    pub roles: BTreeMap<String, RoleSettings>,
    /// Translation scale rule.
    #[serde(default)]
    pub translation_scale: ScaleRule,
    /// Root-motion scale rule for baked clips.
    #[serde(default)]
    pub root_motion: RootMotionScale,
    /// Turn the source by the facing yaw between the rigs (snapped to 90°).
    #[serde(default = "yes")]
    pub align_facing: bool,
    /// IK goals solved at bake, in order (v2; empty in a v1 file).
    #[serde(default)]
    pub ik_goals: Vec<IkGoalSpec>,
}

impl Default for RetargetSetup {
    fn default() -> Self {
        RetargetSetup {
            format_version: RETARGET_FORMAT_VERSION,
            name: String::new(),
            source: RigRoles::default(),
            target: RigRoles::default(),
            roles: BTreeMap::new(),
            translation_scale: ScaleRule::HipHeight,
            root_motion: RootMotionScale::Auto,
            align_facing: true,
            ik_goals: Vec::new(),
        }
    }
}

/// Errors loading or building a [`RetargetSetup`].
#[derive(Debug, thiserror::Error)]
pub enum RetargetSetupError {
    /// RON parse error.
    #[error("retarget RON parse error: {0}")]
    RonParse(#[from] ron::error::SpannedError),
    /// RON write error.
    #[error("retarget RON write error: {0}")]
    RonWrite(#[from] ron::Error),
    /// The file's version is neither the current one nor one the loader migrates.
    #[error("retarget format version {0} is unknown (current is {RETARGET_FORMAT_VERSION})")]
    UnknownVersion(u32),
    /// The document is malformed (missing version field, bad values).
    #[error("invalid retarget document: {0}")]
    Invalid(String),
    /// A `Profile` role source names no built-in profile.
    #[error("unknown skeleton profile {0:?} (built-ins: generic-humanoid, vrm)")]
    UnknownProfile(String),
    /// Pairing the skeletons failed.
    #[error(transparent)]
    Retarget(#[from] RetargetError),
}

impl RetargetSetupError {
    /// The stable reason code (agent protocol, `book/agents/retarget-crowds.md`).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            RetargetSetupError::RonParse(_)
            | RetargetSetupError::RonWrite(_)
            | RetargetSetupError::UnknownVersion(_)
            | RetargetSetupError::Invalid(_)
            | RetargetSetupError::UnknownProfile(_) => "bad_setup",
            RetargetSetupError::Retarget(RetargetError::MissingJoint { .. }) => "missing_joint",
            RetargetSetupError::Retarget(RetargetError::NoCommonRoles) => "no_common_roles",
            RetargetSetupError::Retarget(RetargetError::BadSkeleton { .. }) => "bad_skeleton",
        }
    }
}

/// The serde field-list fingerprint of the format.
#[must_use]
pub fn retarget_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names, serde_variant_names};
    join_field_lists(&[
        serde_field_names::<RetargetSetup>(),
        serde_field_names::<RigRoles>(),
        serde_variant_names::<RoleSource>(),
        serde_field_names::<RoleSettings>(),
        serde_variant_names::<crate::retarget::TranslationMode>(),
        serde_variant_names::<ScaleRule>(),
        serde_variant_names::<RootMotionScale>(),
        serde_field_names::<IkGoalSpec>(),
        serde_variant_names::<crate::retarget::IkGoalMode>(),
    ])
}

/// `from` versions of the registered migrations (for `cargo xtask check-formats`): v1 → v2.
#[must_use]
pub fn retarget_migration_sources() -> Vec<u32> {
    vec![1]
}

/// Migrate `.retarget.ron` text of any supported version to the current one. v1 → v2 adds the
/// `ik_goals` list, empty, so the setup bakes what it baked before. `regia upgrade-project` calls
/// this.
///
/// # Errors
///
/// The errors of [`RetargetSetup::from_ron_str`] and [`RetargetSetup::to_ron_string`].
pub fn migrate_retarget(text: &str) -> Result<String, RetargetSetupError> {
    RetargetSetup::from_ron_str(text)?.to_ron_string()
}

impl RigRoles {
    /// The role profile of `skeleton` on this side.
    ///
    /// # Errors
    ///
    /// [`RetargetSetupError::UnknownProfile`] for a `Profile` source naming no built-in.
    pub fn profile_for(&self, skeleton: &Skeleton) -> Result<RetargetProfile, RetargetSetupError> {
        let mut profile = match &self.roles {
            RoleSource::Guess => RetargetProfile::guess_humanoid(skeleton),
            RoleSource::ByName => RetargetProfile::by_name(skeleton),
            RoleSource::Profile(name) => SkeletonProfile::builtin(name)
                .ok_or_else(|| RetargetSetupError::UnknownProfile(name.clone()))?
                .retarget_profile(skeleton),
            RoleSource::Explicit(map) => RetargetProfile { roles: map.clone() },
        };
        for (role, joint) in &self.overrides {
            profile.roles.insert(role.clone(), joint.clone());
        }
        Ok(profile)
    }
}

impl RetargetSetup {
    /// Parses `.retarget.ron` text. A version 1 file loads as version 2 with no IK goals.
    ///
    /// # Errors
    ///
    /// Syntax errors, versions newer than the current one (or 0), and bad values.
    pub fn from_ron_str(text: &str) -> Result<Self, RetargetSetupError> {
        let version = regia_core::version::peek_ron_version(text)
            .map_err(|e| RetargetSetupError::Invalid(format!("cannot read format_version: {e}")))?;
        if version != RETARGET_FORMAT_VERSION && !retarget_migration_sources().contains(&version) {
            return Err(RetargetSetupError::UnknownVersion(version));
        }
        let mut setup: RetargetSetup = ron::from_str(text)?;
        // v1 → v2: the goal list did not exist; its default (empty) is the v1 behavior.
        if version == 1 {
            setup.ik_goals.clear();
        }
        setup.format_version = RETARGET_FORMAT_VERSION;
        setup.validate()?;
        Ok(setup)
    }

    /// Serializes at the current version.
    ///
    /// # Errors
    ///
    /// Serialization failure.
    pub fn to_ron_string(&self) -> Result<String, RetargetSetupError> {
        let mut document = self.clone();
        document.format_version = RETARGET_FORMAT_VERSION;
        let pretty = regia_core::ron_pretty();
        Ok(ron::ser::to_string_pretty(&document, pretty)?)
    }

    /// Checks the values: scales finite and positive, rest offsets finite, IK goals valid
    /// ([`IkGoalSpec::validate`]).
    ///
    /// # Errors
    ///
    /// [`RetargetSetupError::Invalid`] naming the bad value.
    pub fn validate(&self) -> Result<(), RetargetSetupError> {
        let positive = |k: f32| k.is_finite() && k > 0.0;
        if let ScaleRule::Fixed(k) = self.translation_scale
            && !positive(k)
        {
            return Err(RetargetSetupError::Invalid(format!(
                "translation_scale Fixed({k}) must be finite and positive"
            )));
        }
        if let RootMotionScale::Fixed(k) = self.root_motion
            && !(k.is_finite() && k >= 0.0)
        {
            return Err(RetargetSetupError::Invalid(format!(
                "root_motion Fixed({k}) must be finite and not negative"
            )));
        }
        for (role, settings) in &self.roles {
            if !settings.rest_offset_deg.is_finite() {
                return Err(RetargetSetupError::Invalid(format!(
                    "role {role:?} has a non-finite rest_offset_deg"
                )));
            }
        }
        for goal in &self.ik_goals {
            goal.validate().map_err(RetargetSetupError::Invalid)?;
        }
        Ok(())
    }

    /// The runtime options of this setup.
    #[must_use]
    pub fn options(&self) -> RetargetOptions {
        RetargetOptions {
            roles: self.roles.clone(),
            translation_scale: self.translation_scale,
            root_motion: self.root_motion,
            align_facing: self.align_facing,
            ik_goals: self.ik_goals.clone(),
        }
    }

    /// Pair `source` and `target` under this setup.
    ///
    /// # Errors
    ///
    /// Bad values, an unknown built-in profile, a role naming an absent joint, or no shared role.
    pub fn build(&self, source: &Skeleton, target: &Skeleton) -> Result<Retargeter, RetargetSetupError> {
        self.validate()?;
        let source_profile = self.source.profile_for(source)?;
        let target_profile = self.target.profile_for(target)?;
        Ok(Retargeter::with_options(
            source,
            &source_profile,
            target,
            &target_profile,
            &self.options(),
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::retarget::TranslationMode;
    use regia_core::math::Vec3;

    const RON: &str = r#"(
        format_version: 1,
        name: "mixamo-to-guard",
        source: (roles: Guess, overrides: { "leftHand": "hand" }),
        target: (roles: Explicit({ "hips": "pelvis" })),
        roles: {
            "hips": (translation: Some(Scaled)),
            "leftUpperArm": (rest_offset_deg: (0.0, 0.0, -10.0)),
            "rightToes": (skip: true),
        },
        translation_scale: Fixed(1.5),
        root_motion: Off,
    )"#;

    #[test]
    fn setups_parse_round_trip_and_default_facing_on() {
        let setup = RetargetSetup::from_ron_str(RON).expect("parses");
        assert_eq!(setup.name, "mixamo-to-guard");
        assert!(setup.align_facing, "align_facing defaults to true");
        assert_eq!(setup.translation_scale, ScaleRule::Fixed(1.5));
        assert_eq!(setup.root_motion, RootMotionScale::Off);
        assert_eq!(
            setup.roles.get("hips").and_then(|r| r.translation),
            Some(TranslationMode::Scaled)
        );
        assert_eq!(
            setup.roles.get("leftUpperArm").map(|r| r.rest_offset_deg),
            Some(Vec3::new(0.0, 0.0, -10.0))
        );
        let text = setup.to_ron_string().expect("writes");
        assert_eq!(RetargetSetup::from_ron_str(&text).expect("reparses"), setup);
        // A minimal document takes every default (a v1 one loads as v2).
        let minimal = RetargetSetup::from_ron_str("(format_version: 1)").expect("minimal");
        assert_eq!(minimal, RetargetSetup::default());
        let minimal = RetargetSetup::from_ron_str("(format_version: 2)").expect("minimal v2");
        assert_eq!(minimal, RetargetSetup::default());
    }

    #[test]
    fn unknown_versions_bad_values_and_profiles_are_refused() {
        let v3 = RON.replace("format_version: 1", "format_version: 3");
        assert!(matches!(
            RetargetSetup::from_ron_str(&v3),
            Err(RetargetSetupError::UnknownVersion(3))
        ));
        let v0 = RON.replace("format_version: 1", "format_version: 0");
        assert!(matches!(
            RetargetSetup::from_ron_str(&v0),
            Err(RetargetSetupError::UnknownVersion(0))
        ));
        let bad = RON.replace("Fixed(1.5)", "Fixed(-1.0)");
        let err = RetargetSetup::from_ron_str(&bad).expect_err("negative scale");
        assert_eq!(err.code(), "bad_setup");
        assert!(RetargetSetup::from_ron_str("(name: \"x\")").is_err());
        let side = RigRoles {
            roles: RoleSource::Profile("nope".into()),
            overrides: BTreeMap::new(),
        };
        let skeleton = Skeleton::from_local_bind(
            vec!["root".into()],
            vec![None],
            vec![crate::skeleton::LocalBind::default()],
        )
        .expect("skeleton");
        let err = side.profile_for(&skeleton).expect_err("unknown profile");
        assert_eq!(err.code(), "bad_setup");
        // Schema fingerprint covers the nested types.
        let fingerprint = retarget_schema_fingerprint();
        for field in [
            "align_facing",
            "overrides",
            "rest_offset_deg",
            "Explicit",
            "Scaled",
            "HipHeight",
            "Off",
            "ik_goals",
            "weight",
            "Contacts",
        ] {
            assert!(fingerprint.contains(field), "{field} missing from {fingerprint}");
        }
        assert_eq!(retarget_migration_sources(), vec![1]);
    }
}
