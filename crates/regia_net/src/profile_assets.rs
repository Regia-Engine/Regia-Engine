// SPDX-License-Identifier: MPL-2.0
//! Net profiles through the asset server (spec 72 § 8, ADR-7232).
//!
//! The built-in profiles are registered as text files of the `engine://` source
//! (`engine://net/profiles/<name>.netprofile.ron`, [`register_builtin_profile_files`]); a project's
//! `.netprofile.ron` files are cooked (the `netprofile` cook row of `regia_asset_pipeline`, payload
//! the canonical RON text, type [`NET_PROFILE_TYPE_PATH`]) and ship in the cooked cache or a pak.
//! At `Startup`, [`load_net_profile_assets`] reads every `.netprofile.ron` of both sources into
//! [`NetProfileAssets`]; [`apply_net_profile_assets`] (`PreUpdate`) moves them into [`NetProfiles`]
//! once that resource exists (a server installs it with `install_net_interest`). A profile equal to
//! the one already loaded under its key is not inserted again, so the built-ins do not bump the
//! profiles' generation. An invalid file is reported (`net.profile_invalid`) and entities naming it
//! fall back to the default, as for any profile that fails to load.

use regia_app::prelude::*;
use regia_asset::AssetServer;
use regia_asset::engine_source::{EngineAssets, TextSource, read_text_asset, text_assets_with_suffix};

use crate::profile::{
    BUILTIN_PROFILE_PREFIX, NetProfile, NetProfiles, ProfileError, builtin_profiles, profile_key,
};

/// The cooked type path of a `.netprofile.ron` (its payload is the canonical RON text).
pub const NET_PROFILE_TYPE_PATH: &str = "regia_net::NetProfile";

/// The file name suffix of a net profile.
pub const NET_PROFILE_SUFFIX: &str = ".netprofile.ron";

/// The `engine://` path of the built-in profile `name`.
#[must_use]
pub fn builtin_profile_path(name: &str) -> String {
    format!("{BUILTIN_PROFILE_PREFIX}{name}{NET_PROFILE_SUFFIX}")
}

/// Registers the built-in profiles as `engine://net/profiles/<name>.netprofile.ron` text files.
pub fn register_builtin_profile_files(engine: &mut EngineAssets) {
    for (name, profile) in builtin_profiles() {
        match profile.to_ron() {
            Ok(text) => {
                if let Err(e) = engine.insert(&builtin_profile_path(name), text) {
                    tracing::error!(target: "net", profile = name, error = %e, "net.profile_builtin_path");
                }
            }
            Err(e) => tracing::error!(target: "net", profile = name, error = %e, "net.profile_builtin_text"),
        }
    }
}

/// One profile file read from the assets.
#[derive(Debug, Clone, PartialEq)]
pub struct ProfileAsset {
    /// Its path (`engine://...` or a project manifest path).
    pub path: String,
    /// Where it came from: `engine`, `pak` or `cache`.
    pub source: &'static str,
    /// The parsed profile, or why it did not load.
    pub profile: Result<NetProfile, ProfileError>,
}

/// The profile files [`load_net_profile_assets`] read, and whether they reached [`NetProfiles`].
#[derive(Resource, Debug, Default, Clone)]
pub struct NetProfileAssets {
    /// Every `.netprofile.ron` read, engine files first, then the project's, each sorted by path.
    pub files: Vec<ProfileAsset>,
    /// Paths that could not be read at all (missing cooked file, wrong type), with the reason.
    pub unreadable: Vec<(String, String)>,
    /// Whether the files were applied to [`NetProfiles`].
    pub applied: bool,
}

/// Reads every `.netprofile.ron` of the `engine://` source and of the asset server (if any).
#[must_use]
pub fn read_profile_assets(engine: &EngineAssets, mut server: Option<&mut AssetServer>) -> NetProfileAssets {
    let mut out = NetProfileAssets::default();
    for path in text_assets_with_suffix(engine, server.as_deref(), NET_PROFILE_SUFFIX) {
        match read_text_asset(engine, server.as_deref_mut(), &path, NET_PROFILE_TYPE_PATH) {
            Ok((text, source)) => {
                let profile = NetProfile::from_ron(&text).and_then(|p| p.validate().map(|()| p));
                out.files.push(ProfileAsset {
                    path,
                    source: match source {
                        TextSource::Engine => "engine",
                        TextSource::Pak(_) => "pak",
                        TextSource::DevCache => "cache",
                    },
                    profile,
                });
            }
            Err(e) => {
                tracing::error!(target: "net", profile = %path, error = %e, "net.profile_unreadable");
                out.unreadable.push((path, e.to_string()));
            }
        }
    }
    out
}

/// Loads the files of `assets` into `profiles`: a valid profile that differs from the one under
/// its key is inserted, an invalid one is recorded as failed. Returns how many profiles changed.
pub fn apply_profile_assets(assets: &NetProfileAssets, profiles: &mut NetProfiles) -> usize {
    let mut changed = 0;
    for file in &assets.files {
        match &file.profile {
            Ok(profile) => {
                if profiles.get(&file.path) == Some(profile) {
                    continue;
                }
                if profiles.insert(&file.path, profile.clone()).is_ok() {
                    changed += 1;
                }
            }
            Err(e) => {
                tracing::error!(target: "net", profile = %file.path, error = %e, "net.profile_invalid");
                // Recorded under the profile's key; entities naming it use the default.
                let key = profile_key(&file.path).to_owned();
                if profiles.failed.insert(key, e.clone()).is_none() {
                    changed += 1;
                }
            }
        }
    }
    changed
}

/// `Startup`: reads the profile files of both sources into [`NetProfileAssets`].
pub fn load_net_profile_assets(world: &mut World) {
    let engine = world.get_resource::<EngineAssets>().cloned().unwrap_or_default();
    let assets = match world.get_resource_mut::<AssetServer>() {
        Some(mut server) => read_profile_assets(&engine, Some(&mut *server)),
        None => read_profile_assets(&engine, None),
    };
    tracing::debug!(target: "net", files = assets.files.len(), unreadable = assets.unreadable.len(), "net profile assets read");
    world.insert_resource(assets);
}

/// `PreUpdate`: applies [`NetProfileAssets`] to [`NetProfiles`] once that resource exists.
pub fn apply_net_profile_assets(world: &mut World) {
    let pending = world
        .get_resource::<NetProfileAssets>()
        .is_some_and(|a| !a.applied && !a.files.is_empty());
    if !pending || world.get_resource::<NetProfiles>().is_none() {
        return;
    }
    world.resource_scope(|world, mut assets: Mut<NetProfileAssets>| {
        if let Some(mut profiles) = world.get_resource_mut::<NetProfiles>() {
            apply_profile_assets(&assets, &mut profiles);
            assets.applied = true;
        }
    });
}

/// Registers the built-in profile files and the two systems (added by `NetPlugin`).
pub(crate) fn install(app: &mut App) {
    app.init_resource::<EngineAssets>();
    register_builtin_profile_files(&mut app.world_mut().resource_mut::<EngineAssets>());
    app.add_systems(Startup, load_net_profile_assets)
        .add_systems(PreUpdate, apply_net_profile_assets);
}
