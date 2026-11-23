// SPDX-License-Identifier: MPL-2.0
//! The one-object `--json` mode of `regia cook`, `regia package` and `regia run` through the real
//! binary (`docs/specs/21-tooling-and-packaging.md` § 1, ADR-7217): stdout is exactly one compact
//! JSON object on one line (`ok`, and for package and run the `artifact` path), every other line
//! goes to stderr, and a failure still prints one object with `ok: false` and an `error` code.

use std::path::Path;
use std::process::{Command, Output};

use image::ImageEncoder;

fn regia(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_regia"))
        .args(args)
        .env("REGIA_LOG", "warn")
        .output()
        .expect("run regia")
}

/// Stdout as exactly one JSON object on one line.
fn one_object(out: &Output) -> serde_json::Value {
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(
        lines.len(),
        1,
        "stdout must be one line, got {lines:?}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value: serde_json::Value = serde_json::from_str(lines[0]).unwrap_or_else(|e| {
        panic!(
            "stdout is not JSON ({e}): {stdout}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stderr)
        )
    });
    assert!(value.is_object(), "{value}");
    value
}

fn png() -> Vec<u8> {
    let pixels: Vec<u8> = (0..32 * 32)
        .flat_map(|i| [i as u8, 90, 255 - i as u8, 255])
        .collect();
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(&pixels, 32, 32, image::ExtendedColorType::Rgba8)
        .expect("png");
    out
}

fn project(name: &str, cook_table: &str) -> std::path::PathBuf {
    let dir = regia_test_utils::unique_temp_dir(name);
    std::fs::create_dir_all(dir.join("assets")).expect("assets");
    std::fs::write(dir.join("assets/wall_albedo.png"), png()).expect("png");
    std::fs::write(
        dir.join("Regia.toml"),
        format!("[project]\nname = \"Json\"\nid = \"com.example.json\"\n{cook_table}"),
    )
    .expect("manifest");
    dir
}

fn path_str(dir: &Path) -> &str {
    dir.to_str().expect("utf-8 temp path")
}

#[test]
fn plain_cook_json_prints_one_object_naming_the_paks() {
    let dir = project("json-cli-cook", "[cook]\ntexture_quality = \"draft\"\n");
    let out = regia(&[
        "cook",
        "--project",
        path_str(&dir),
        "--platform",
        "linux",
        "--quality",
        "fast",
        "--json",
    ]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let value = one_object(&out);
    assert_eq!(value["ok"], true, "{value}");
    assert_eq!(value["platform"], "linux");
    assert_eq!(value["quality"], "fast");
    assert_eq!(value["texture_quality"], "draft");
    let pak_dir = value["pak_dir"].as_str().expect("pak_dir");
    assert!(Path::new(pak_dir).is_dir(), "{pak_dir}");
    let paks = value["paks"].as_array().expect("paks");
    assert!(!paks.is_empty(), "{value}");
    for pak in paks {
        assert!(Path::new(pak.as_str().expect("path")).is_file(), "{pak}");
    }
    assert!(value["cooked"].as_u64().is_some_and(|n| n >= 1), "{value}");
    // The human summary still goes out, on stderr.
    assert!(String::from_utf8_lossy(&out.stderr).contains("cook:"));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_failed_cook_json_prints_one_failure_object() {
    let dir = project("json-cli-cook-bad", "");
    let out = regia(&[
        "cook",
        "--project",
        path_str(&dir),
        "--platform",
        "linux",
        "--quality",
        "best",
        "--json",
    ]);
    assert!(!out.status.success());
    let value = one_object(&out);
    assert_eq!(value["ok"], false, "{value}");
    assert_eq!(value["error"]["code"], "cook_failed");
    assert!(
        value["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("--quality")),
        "{value}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// `--dry-run` cooks and prints the size report without building a game: with `--json` the report
/// goes to stderr and the object names the would-be package directory as `artifact`.
#[test]
fn package_dry_run_json_prints_one_object_with_the_artifact() {
    let dir = project("json-cli-package-dry", "");
    let out = regia(&[
        "package",
        "--platform",
        "linux",
        "--project",
        path_str(&dir),
        "--dry-run",
        "--json",
    ]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let value = one_object(&out);
    assert_eq!(value["ok"], true, "{value}");
    assert_eq!(value["platform"], "linux");
    assert_eq!(value["profile"], "dev");
    assert_eq!(value["dry_run"], true);
    let artifact = value["artifact"].as_str().expect("artifact");
    assert!(Path::new(artifact).is_absolute(), "{artifact}");
    assert!(artifact.ends_with("linux"), "{artifact}");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn package_json_outside_a_project_prints_one_failure_object() {
    let dir = regia_test_utils::unique_temp_dir("json-cli-package-none");
    let out = regia(&[
        "package",
        "--platform",
        "linux",
        "--project",
        path_str(&dir),
        "--json",
    ]);
    assert!(!out.status.success());
    let value = one_object(&out);
    assert_eq!(value["ok"], false, "{value}");
    // Package hygiene validates the manifest before general project loading (ADR-8062).
    // The failure retains its specific code and only the fixed, privacy-safe field name.
    assert_eq!(value["error"]["code"], "package_hygiene_io");
    assert_eq!(value["error"]["message"], "Regia.toml");
    assert!(value["error"]["location_id"].is_null());
    assert!(value["hygiene"].is_null());
    assert!(String::from_utf8_lossy(&out.stderr).contains("error:"));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn run_json_outside_a_project_prints_one_failure_object() {
    let dir = regia_test_utils::unique_temp_dir("json-cli-run-none");
    let out = regia(&["run", "--project", path_str(&dir), "--json"]);
    assert!(!out.status.success());
    let value = one_object(&out);
    assert_eq!(value["ok"], false, "{value}");
    assert_eq!(value["error"]["code"], "run_failed");
    assert!(
        value["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("not a Regia Engine project")),
        "{value}"
    );
    let _ = std::fs::remove_dir_all(dir);
}
