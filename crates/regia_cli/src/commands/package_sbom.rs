// SPDX-License-Identifier: MPL-2.0
//! Deterministic dependency bill written into packages (ADR-3935).

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result as AnyResult, bail};
use serde::Serialize;

/// Errors returned when Cargo metadata cannot be converted into a package bill.
#[derive(Debug, thiserror::Error)]
pub enum SbomError {
    /// The metadata document is not valid JSON or has an unexpected shape.
    #[error("invalid cargo metadata: {0}")]
    InvalidMetadata(String),
    /// Cargo metadata omitted a required package identity field.
    #[error("cargo metadata package is missing {0}")]
    MissingField(&'static str),
    /// Serialization of the deterministic SBOM failed.
    #[error("serializing SBOM JSON failed: {0}")]
    Serialize(String),
}

#[derive(Serialize)]
struct SbomDocument {
    format_version: u32,
    packages: Vec<SbomPackage>,
}

#[derive(Serialize)]
struct SbomPackage {
    name: String,
    version: String,
    license: String,
}

/// Builds a stable, path-free JSON bill from `cargo metadata --format-version 1` output.
pub fn sbom_from_cargo_metadata(json: &str) -> std::result::Result<String, SbomError> {
    let metadata: serde_json::Value =
        serde_json::from_str(json).map_err(|error| SbomError::InvalidMetadata(error.to_string()))?;
    let packages = metadata
        .get("packages")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| SbomError::InvalidMetadata("missing packages array".to_owned()))?;
    let mut entries = Vec::with_capacity(packages.len());
    for package in packages {
        let name = package
            .get("name")
            .and_then(serde_json::Value::as_str)
            .ok_or(SbomError::MissingField("name"))?;
        let version = package
            .get("version")
            .and_then(serde_json::Value::as_str)
            .ok_or(SbomError::MissingField("version"))?;
        let license = package
            .get("license")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .or_else(|| {
                package
                    .get("license_file")
                    .and_then(serde_json::Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(safe_license_file)
            })
            .unwrap_or_else(|| "unknown".to_owned());
        entries.push(SbomPackage {
            name: name.to_owned(),
            version: version.to_owned(),
            license,
        });
    }
    entries.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then(left.version.cmp(&right.version))
            .then(left.license.cmp(&right.license))
    });
    serde_json::to_string_pretty(&SbomDocument {
        format_version: 1,
        packages: entries,
    })
    .map_err(|error| SbomError::Serialize(error.to_string()))
}

fn safe_license_file(value: &str) -> String {
    let bytes = value.as_bytes();
    let windows_absolute = bytes.get(1) == Some(&b':')
        && bytes
            .get(2)
            .is_some_and(|separator| matches!(*separator, b'/' | b'\\'));
    let rooted = value.starts_with('/') || value.starts_with('\\');
    if rooted || windows_absolute {
        value
            .rsplit(['/', '\\'])
            .find(|part| !part.is_empty())
            .unwrap_or("license-file")
            .to_owned()
    } else {
        value.to_owned()
    }
}

/// The `cargo metadata` arguments for the SBOM of a package built for `targets`: one
/// `--filter-platform` per target triple. Without the filter, cargo resolves every platform of the
/// lockfile and, being `--frozen`, needs every one of those packages already downloaded, which a host
/// that only built this package's platform does not have (a Windows `regia package` stopped on
/// `ab_glyph`, a crate no Windows build uses; ADR-5489 lane). With it, the SBOM lists what the
/// package's platform resolves, not other platforms' crates. No targets: no filter.
pub(crate) fn metadata_args(targets: &[String]) -> Vec<String> {
    let mut args: Vec<String> = ["metadata", "--format-version", "1", "--frozen"]
        .map(String::from)
        .into();
    for target in targets {
        args.push("--filter-platform".into());
        args.push(target.clone());
    }
    args
}

/// Writes `sbom.json` using the resolved package graph without network access, filtered to the
/// packages `targets` resolve ([`metadata_args`]).
pub(crate) fn write_sbom(staging: &Path, workspace: &Path, targets: &[String]) -> AnyResult<()> {
    let output = Command::new("cargo")
        .args(metadata_args(targets))
        .current_dir(workspace)
        .output()
        .context("running cargo metadata for package SBOM")?;
    if !output.status.success() {
        bail!(
            "cargo metadata failed while building package SBOM: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let metadata = String::from_utf8(output.stdout).context("cargo metadata output was not UTF-8")?;
    write_sbom_from_metadata(staging, &metadata)
}

fn write_sbom_from_metadata(staging: &Path, metadata: &str) -> AnyResult<()> {
    let sbom = sbom_from_cargo_metadata(metadata)
        .map_err(|error| anyhow::anyhow!("building package SBOM: {error}"))?;
    std::fs::write(staging.join("sbom.json"), sbom)
        .with_context(|| format!("writing {}", staging.join("sbom.json").display()))
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::write_sbom_from_metadata;
    use super::{metadata_args, sbom_from_cargo_metadata};

    const REVERSED_METADATA: &str = r#"{
        "packages": [
            {"name":"zulu","version":"2.0.0","license":"MIT","license_file":null},
            {"name":"alpha","version":"1.2.3","license":"Apache-2.0","license_file":null}
        ]
    }"#;

    #[test]
    fn metadata_is_sorted_and_contains_no_machine_paths() {
        let sbom = sbom_from_cargo_metadata(REVERSED_METADATA).expect("fixture metadata parses");
        let value: serde_json::Value = serde_json::from_str(&sbom).expect("SBOM is JSON");
        assert_eq!(value["format_version"], 1);
        assert_eq!(value["packages"][0]["name"], "alpha");
        assert_eq!(value["packages"][1]["name"], "zulu");
        assert_eq!(value["packages"][0]["license"], "Apache-2.0");
        assert!(!sbom.contains("/home"));
        assert!(!sbom.contains("/srv"));
    }

    #[test]
    fn metadata_is_filtered_to_the_targets_the_package_builds() {
        let base = ["metadata", "--format-version", "1", "--frozen"];
        assert_eq!(
            metadata_args(&[]),
            base,
            "no target known: every platform, as before"
        );
        assert_eq!(
            metadata_args(&["x86_64-pc-windows-msvc".into()]),
            [&base[..], &["--filter-platform", "x86_64-pc-windows-msvc"]].concat()
        );
        assert_eq!(
            metadata_args(&["aarch64-linux-android".into(), "x86_64-linux-android".into()]),
            [
                &base[..],
                &[
                    "--filter-platform",
                    "aarch64-linux-android",
                    "--filter-platform",
                    "x86_64-linux-android"
                ]
            ]
            .concat()
        );
    }

    #[test]
    fn null_license_is_unknown() {
        let fixture =
            r#"{"packages":[{"name":"unlicensed","version":"0.1.0","license":null,"license_file":null}]}"#;
        let sbom = sbom_from_cargo_metadata(fixture).expect("null licence is accepted");
        let value: serde_json::Value = serde_json::from_str(&sbom).expect("SBOM is JSON");
        assert_eq!(value["packages"][0]["license"], "unknown");
    }

    #[test]
    fn absolute_license_file_is_reduced_to_its_file_name() {
        let fixture = r#"{"packages":[{"name":"path-license","version":"1.0.0","license":null,"license_file":"/srv/private/LICENSE.txt"}]}"#;
        let sbom = sbom_from_cargo_metadata(fixture).expect("license file metadata parses");
        let value: serde_json::Value = serde_json::from_str(&sbom).expect("SBOM is JSON");
        assert_eq!(value["packages"][0]["license"], "LICENSE.txt");
        assert!(!sbom.contains("/srv"));
    }

    #[cfg(unix)]
    #[test]
    fn archive_writer_includes_sbom_member() -> anyhow::Result<()> {
        let root = regia_test_utils::unique_temp_dir("package_sbom_archive");
        let staging = root.join("tiny");
        std::fs::create_dir_all(&staging)?;
        let archive = root.join("tiny.tar.gz");
        assert!(crate::commands::package::write_tarball(&archive, &root, "tiny").is_err());
        write_sbom_from_metadata(&staging, REVERSED_METADATA)?;
        crate::commands::package::write_tarball(&archive, &root, "tiny")?;
        let listed = std::process::Command::new("tar")
            .args(["-tzf", archive.to_str().expect("temp path is UTF-8")])
            .output()?;
        assert!(listed.status.success());
        let members = String::from_utf8(listed.stdout)?;
        assert!(members.lines().any(|member| member == "tiny/sbom.json"));
        Ok(())
    }
}
