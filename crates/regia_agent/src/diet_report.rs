// SPDX-License-Identifier: MPL-2.0
//! `assets.diet_report { profile, platform? }` (ADR-8461, `book/agents/cook-diet.md`): what the cook
//! diet did to a build profile's pak, read from the `diet-report.json` that `regia package
//! --profile <name>` writes beside it (`build/profiles/<name>/paks/<platform>/`): the textures a
//! cap applies to, the meshes that lost LODs under the triangle ceiling, the variant picks, and the
//! totals before and after. Read-only.
//!
//! The diet itself runs in the desktop pipeline (`regia_asset_pipeline::cook_diet`), which this
//! crate must not depend on; the report is JSON, so it is returned as written.

use regia_app::prelude::*;
use serde_json::{Value, json};

use crate::protocol::RpcError;

const METHOD: &str = "assets.diet_report";

/// The report file name (`regia_asset_pipeline::cook_diet::DIET_REPORT_FILE`).
pub const DIET_REPORT_FILE: &str = "diet-report.json";

/// The newest report layout this build reads (`regia_asset_pipeline::cook_diet::DIET_REPORT_VERSION`).
pub const DIET_REPORT_VERSION: u64 = 1;

/// Profile and platform names are path segments: letters, digits, `_` and `-` only.
fn is_name(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 64
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn bad_param(message: String) -> RpcError {
    RpcError::invalid_params(format!("{METHOD}: {message} [diet_report_bad_param]"))
}

/// The platform of the running asset server, else of this build.
fn default_platform(world: &World) -> String {
    if let Some(server) = world.get_resource::<regia_asset::AssetServer>() {
        return server.platform().to_string();
    }
    if cfg!(target_os = "android") {
        "android".into()
    } else if cfg!(target_os = "windows") {
        "windows".into()
    } else if cfg!(target_os = "macos") {
        "macos".into()
    } else {
        "linux".into()
    }
}

/// `report` checked: an object whose `version` this build reads.
///
/// # Errors
///
/// `diet_report_unreadable`: not a diet report, or from a newer build.
pub fn check_report(report: Value, path: &str) -> Result<Value, RpcError> {
    let version = report.get("version").and_then(Value::as_u64);
    match version {
        Some(v) if (1..=DIET_REPORT_VERSION).contains(&v) => Ok(report),
        _ => Err(RpcError::new(
            crate::protocol::INVALID_PARAMS,
            "F0013",
            format!(
                "{METHOD}: {path} is not a diet report this build reads (version {}) [diet_report_unreadable]",
                version.map_or_else(|| "missing".to_string(), |v| v.to_string())
            ),
            "package the profile again with this build (regia package --profile <name>)",
        )),
    }
}

/// `assets.diet_report { profile, platform? }`: the diet report of the profile's pak for `platform`
/// (default: the running asset server's platform), with `path` (project-relative) added.
///
/// # Errors
///
/// - `diet_report_bad_param` (F0002): `profile` is missing, or a name is not letters, digits, `_`
///   and `-`.
/// - `diet_report_not_written` (F0016): the profile's pak has no diet report (no diet, or not
///   packaged yet).
/// - `diet_report_unreadable` (F0013): the file does not parse, or is from a newer build.
pub fn diet_report(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let profile = match crate::params::opt_str(params, "profile")? {
        Some(p) if is_name(p) => p.to_string(),
        Some(p) => return Err(bad_param(format!("`profile` {p:?} is not a profile name"))),
        None => {
            return Err(bad_param(
                "`profile` (a [profiles.<name>] of Regia.toml) is required".into(),
            ));
        }
    };
    let platform = match crate::params::opt_str(params, "platform")? {
        Some(p) if is_name(p) => p.to_string(),
        Some(p) => return Err(bad_param(format!("`platform` {p:?} is not a platform name"))),
        None => default_platform(world),
    };
    let root = crate::req_path::project_root(world);
    let rel = format!("build/profiles/{profile}/paks/{platform}/{DIET_REPORT_FILE}");
    let file = root.join(&rel);
    let bytes = match crate::req_path::read_file_capped(&file, crate::req_path::MAX_READ_BYTES) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(RpcError::new(
                crate::protocol::NOT_FOUND,
                "F0016",
                format!(
                    "{METHOD}: profile `{profile}` has no diet report for {platform} ({rel}) [diet_report_not_written]"
                ),
                "add a [content.diet] (or [profiles.<name>.diet]) table to Regia.toml and run regia package --profile <name>",
            ));
        }
        Err(e) => {
            return Err(RpcError::new(
                crate::protocol::INVALID_PARAMS,
                "F0013",
                format!("{METHOD}: {rel} does not read: {e} [diet_report_unreadable]"),
                "package the profile again (regia package --profile <name>)",
            ));
        }
    };
    let report: Value = serde_json::from_slice(&bytes).map_err(|e| {
        RpcError::new(
            crate::protocol::INVALID_PARAMS,
            "F0013",
            format!("{METHOD}: {rel} is not JSON: {e} [diet_report_unreadable]"),
            "package the profile again (regia package --profile <name>)",
        )
    })?;
    let mut report = check_report(report, &rel)?;
    if let Some(obj) = report.as_object_mut() {
        obj.insert("path".into(), json!(rel));
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_path_segments() {
        assert!(is_name("release"));
        assert!(is_name("mobile-low_2"));
        for bad in ["", "..", "a/b", "a\\b", "a b", &"x".repeat(65)] {
            assert!(!is_name(bad), "{bad:?}");
        }
    }

    fn code(err: &RpcError) -> String {
        err.data.as_ref().expect("data")["code"]
            .as_str()
            .expect("code")
            .to_string()
    }

    /// Through the dispatcher: the report the desktop pipeline writes beside a profile's pak comes
    /// back as written, with its path; a profile without one is `diet_report_not_written` (F0016),
    /// a bad name `diet_report_bad_param` (F0002), a damaged file `diet_report_unreadable` (F0013).
    #[test]
    fn diet_report_through_the_dispatcher() {
        use regia_asset_pipeline::cook_diet::{DietReport, DietTotals, LodDrop, write_report};
        let dir = regia_test_utils::unique_temp_dir("agent-diet-report");
        let pak_dir = dir.join("build/profiles/release/paks/linux");
        std::fs::create_dir_all(&pak_dir).expect("pak dir");
        let report = DietReport {
            version: regia_asset_pipeline::cook_diet::DIET_REPORT_VERSION,
            profile: "release".into(),
            platform: "linux".into(),
            max_triangles: 1_500_000,
            seed: 0,
            ceiling_reached: true,
            before: DietTotals {
                assets: 3,
                bytes: 3000,
                triangles: 1_550_000,
            },
            after: DietTotals {
                assets: 3,
                bytes: 2000,
                triangles: 850_000,
            },
            lods: vec![LodDrop {
                path: "assets/hulls/a.glb".into(),
                id: "x".into(),
                levels: 3,
                dropped: 1,
                triangles_before: 900_000,
                triangles_after: 200_000,
            }],
            ..DietReport::default()
        };
        write_report(&pak_dir.join(DIET_REPORT_FILE), &report).expect("write");
        assert_eq!(
            regia_asset_pipeline::cook_diet::DIET_REPORT_FILE,
            DIET_REPORT_FILE,
            "the agent reads the file the pipeline writes"
        );
        assert_eq!(
            u64::from(regia_asset_pipeline::cook_diet::DIET_REPORT_VERSION),
            DIET_REPORT_VERSION
        );

        let mut world = World::new();
        world.insert_resource(crate::AgentSettings {
            project_dir: Some(dir.clone()),
        });
        let mut call = |params: Value| {
            crate::dispatch::call_in_process(&mut world, METHOD, params).expect("synchronous method")
        };
        let got = call(json!({ "profile": "release", "platform": "linux" })).expect("report");
        assert_eq!(got["path"], "build/profiles/release/paks/linux/diet-report.json");
        assert_eq!(got["after"]["triangles"], 850_000);
        assert_eq!(got["lods"][0]["dropped"], 1);
        let mut back = got.clone();
        back.as_object_mut().expect("object").remove("path");
        let parsed: DietReport = serde_json::from_value(back).expect("the pipeline's report type");
        assert_eq!(parsed, report);

        let none = call(json!({ "profile": "mobile", "platform": "linux" })).expect_err("no report");
        assert_eq!(code(&none), "F0016");
        assert!(
            none.message.contains("[diet_report_not_written]"),
            "{}",
            none.message
        );
        let bad = call(json!({ "profile": "../x" })).expect_err("bad name");
        assert_eq!(code(&bad), "F0002");
        assert!(bad.message.contains("[diet_report_bad_param]"), "{}", bad.message);
        // A missing `profile` may be refused by the dispatcher's parameter gate before the handler:
        // either way it is F0002.
        let missing = call(json!({})).expect_err("no profile");
        assert_eq!(code(&missing), "F0002");

        std::fs::write(pak_dir.join(DIET_REPORT_FILE), b"{ not json").expect("damage");
        let damaged = call(json!({ "profile": "release", "platform": "linux" })).expect_err("damaged");
        assert_eq!(code(&damaged), "F0013");
        assert!(
            damaged.message.contains("[diet_report_unreadable]"),
            "{}",
            damaged.message
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_newer_or_missing_version_is_unreadable() {
        assert!(check_report(json!({ "version": 1 }), "r").is_ok());
        for bad in [json!({ "version": 2 }), json!({}), json!([1])] {
            let err = check_report(bad, "r").expect_err("unreadable");
            assert!(
                err.message.contains("[diet_report_unreadable]"),
                "{}",
                err.message
            );
        }
    }
}
