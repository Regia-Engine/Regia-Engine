// SPDX-License-Identifier: MPL-2.0
//! A v6 `.flmap` of the old-bytes format corpus through `regia upgrade-project` to the current version,
//! then `regia cook --lightmap-tiers --json` over it (L03 / L06 of the v0.5.0 evidence ledger, ADR-5382).
//!
//! The project holds one lightmap: the corpus's version 6 file. The test checks that the upgrade reports and
//! writes version 7 (keeping the original as `.bak`), that the upgraded file cooks, and that the tier report of the
//! cook names the derived LOW and ULTRA_LOW lightmaps with their bytes and budget verdicts. The bytes are checked
//! against the tier files the cook wrote (each a cooked wrapper around a current-version `.flmap`), and the
//! ULTRA_LOW encoding against the platform: BC6H on the desktop, ASTC HDR on Android (the default table's `auto`).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use regia_asset::lightmap::{LIGHTMAP_FORMAT_VERSION, LIGHTMAP_MAGIC, Lightmap};
use serde_json::Value;

/// Removes a directory tree when dropped (even on assertion failure).
struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Where the lightmap sits in the test project.
const MAP: &str = "assets/maps/room.flmap";

fn corpus_v6() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../regia_agent/tests/fixtures/format-corpus/lightmap/v6.flmap")
}

fn regia(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_regia"))
        .args(args)
        .output()
        .expect("spawn the regia binary")
}

fn json_of(out: &Output, what: &str) -> Value {
    assert!(
        out.status.success(),
        "{what} failed: {}\n{}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "{what}: stdout is not JSON ({e}): {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

/// The `FLMP` header's version of `bytes`.
fn header_version(bytes: &[u8]) -> u32 {
    assert_eq!(&bytes[..4], &LIGHTMAP_MAGIC, "not a .flmap");
    u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]])
}

/// The project: a manifest and the corpus's v6 lightmap.
fn project(dir: &Path) {
    std::fs::create_dir_all(dir.join("assets/maps")).expect("dirs");
    std::fs::write(
        dir.join("Regia.toml"),
        "[project]\nname = \"Tier upgrade\"\nid = \"com.example.tier_upgrade\"\n",
    )
    .expect("manifest");
    std::fs::copy(corpus_v6(), dir.join(MAP)).expect("copy the v6 fixture");
}

/// `upgrade-project` (dry run, then `--write`) takes the v6 file to the current version.
fn upgrade(dir: &Path) {
    let project = dir.to_str().expect("UTF-8");
    let original = std::fs::read(dir.join(MAP)).expect("read");
    assert_eq!(header_version(&original), 6, "the corpus fixture is version 6");

    let dry = json_of(
        &regia(&["upgrade-project", "--project", project, "--json"]),
        "upgrade-project",
    );
    let file = dry["files"]
        .as_array()
        .expect("files")
        .iter()
        .find(|f| f["path"].as_str().is_some_and(|p| p.ends_with("room.flmap")))
        .unwrap_or_else(|| panic!("the dry run lists the lightmap: {dry}"))
        .clone();
    assert_eq!(file["status"], "migrate", "{file}");
    assert_eq!(file["found"], 6, "{file}");
    assert_eq!(
        std::fs::read(dir.join(MAP)).expect("read"),
        original,
        "the dry run wrote the file"
    );

    let written = json_of(
        &regia(&["upgrade-project", "--project", project, "--json", "--write"]),
        "upgrade-project --write",
    );
    assert_eq!(written["ok"], true, "{written}");
    let upgraded = std::fs::read(dir.join(MAP)).expect("read");
    assert_eq!(header_version(&upgraded), LIGHTMAP_FORMAT_VERSION);
    assert_eq!(LIGHTMAP_FORMAT_VERSION, 7, "this test pins the v6 -> v7 step");
    assert_eq!(
        std::fs::read(dir.join(format!("{MAP}.bak"))).expect("the original is kept as a backup"),
        original
    );
    let map = Lightmap::from_bytes(&upgraded).expect("the upgraded file reads");
    let before = Lightmap::from_bytes(&original).expect("the reader migrates the original in memory");
    assert_eq!(map.entries, before.entries, "the upgrade keeps every entry");
    assert_eq!((map.width, map.height), (before.width, before.height));

    let again = json_of(
        &regia(&["upgrade-project", "--project", project, "--json"]),
        "upgrade-project (after)",
    );
    let file = again["files"]
        .as_array()
        .expect("files")
        .iter()
        .find(|f| f["path"].as_str().is_some_and(|p| p.ends_with("room.flmap")))
        .unwrap_or_else(|| panic!("the second run lists the lightmap: {again}"))
        .clone();
    assert_eq!(file["status"], "current", "{file}");
}

/// The tier row `tier` of the report's only lightmap.
fn tier_row<'a>(report: &'a Value, tier: &str) -> &'a Value {
    let lightmaps = report["lightmaps"].as_array().expect("lightmaps");
    assert_eq!(lightmaps.len(), 1, "one cooked lightmap: {report}");
    lightmaps[0]["tiers"]
        .as_array()
        .expect("tiers")
        .iter()
        .find(|t| t["tier"] == tier)
        .unwrap_or_else(|| panic!("tier {tier} in the report: {report}"))
}

/// The tier's cooked file under the cooked cache, unwrapped: the current-version `.flmap` the tier ships.
fn tier_file(dir: &Path, platform: &str, tier: &str) -> Vec<u8> {
    let tier_dir = dir
        .join(".regia/cooked")
        .join(platform)
        .join("lightmap_tiers")
        .join(tier);
    let files: Vec<PathBuf> = std::fs::read_dir(&tier_dir)
        .unwrap_or_else(|e| panic!("{}: {e}", tier_dir.display()))
        .map(|e| e.expect("entry").path())
        .filter(|p| {
            !p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with('.'))
        })
        .collect();
    assert_eq!(
        files.len(),
        1,
        "one tier file in {}: {files:?}",
        tier_dir.display()
    );
    let cooked = std::fs::read(&files[0]).expect("read the tier file");
    let (_, payload) = regia_asset::decode_cooked_file(&cooked).expect("a cooked file");
    payload.to_vec()
}

/// Cooks for `platform` with `--lightmap-tiers --json` and checks the LOW and ULTRA_LOW rows; returns the report.
fn cook_tiers(dir: &Path, platform: &str, ultra_low_encoding: &str) -> Value {
    let report = json_of(
        &regia(&[
            "cook",
            "--project",
            dir.to_str().expect("UTF-8"),
            "--platform",
            platform,
            "--quality",
            "fast",
            "--lightmap-tiers",
            "--json",
        ]),
        "cook --lightmap-tiers",
    );
    assert_eq!(report["schema"], 1, "{report}");
    assert_eq!(report["platform"], platform, "{report}");
    let lightmap = &report["lightmaps"][0];
    assert!(
        lightmap["path"]
            .as_str()
            .is_some_and(|p| p.ends_with("maps/room.flmap")),
        "{lightmap}"
    );
    for tier in ["low", "ultra_low"] {
        let row = tier_row(&report, tier);
        let verdict = row["verdict"].as_str().expect("verdict");
        assert!(
            ["fits", "over_budget", "none"].contains(&verdict),
            "{tier}: {row}"
        );
        assert_eq!(verdict, "fits", "{tier} fits the default table's budget: {row}");
        let bytes = row["bytes"].as_u64().expect("bytes");
        assert!(bytes > 0, "{tier}: {row}");
        assert!(
            row["budget_bytes"].as_u64().is_some_and(|b| bytes <= b),
            "{tier}: {row}"
        );
        // The figure is the shipped `.flmap`'s size, and that file is a current-version lightmap at the tier's
        // density.
        let file = tier_file(dir, platform, tier);
        assert_eq!(
            file.len() as u64,
            bytes,
            "{tier}: report bytes against the tier file"
        );
        assert_eq!(header_version(&file), LIGHTMAP_FORMAT_VERSION, "{tier}");
        let map = Lightmap::from_bytes(&file).expect("the tier file reads");
        assert_eq!(
            f64::from(map.density),
            row["density"].as_f64().expect("density"),
            "{tier}"
        );
        assert_eq!(
            (u64::from(map.width), u64::from(map.height)),
            (
                row["width"].as_u64().expect("width"),
                row["height"].as_u64().expect("height")
            ),
            "{tier}"
        );
        assert_eq!(map.is_directional(), row["directional"] == true, "{tier}");
    }
    let low = tier_row(&report, "low");
    let ultra_low = tier_row(&report, "ultra_low");
    assert_eq!(ultra_low["encoding"], ultra_low_encoding, "{ultra_low}");
    assert_eq!(ultra_low["directional"], false, "{ultra_low}");
    assert!(
        ultra_low["density"].as_f64() < low["density"].as_f64(),
        "ULTRA_LOW is coarser than LOW: {low} {ultra_low}"
    );
    // The per-tier totals add up the rows.
    for tier in ["low", "ultra_low"] {
        let total = report["tiers"]
            .as_array()
            .expect("tiers")
            .iter()
            .find(|t| t["tier"] == tier)
            .unwrap_or_else(|| panic!("total of {tier}: {report}"));
        assert_eq!(total["bytes"], tier_row(&report, tier)["bytes"], "{total}");
        assert_eq!(total["lightmaps"], 1, "{total}");
        assert_eq!(total["left_out"], 0, "{total}");
    }
    report
}

#[test]
fn a_v6_corpus_lightmap_upgrades_to_v7_and_cooks_its_low_and_ultra_low_tiers() {
    let tmp = Cleanup(regia_test_utils::unique_temp_dir("lightmap_tier_upgrade"));
    let dir = &tmp.0;
    project(dir);
    upgrade(dir);
    let desktop = cook_tiers(dir, "linux", "bc6h");
    let android = cook_tiers(dir, "android", "astc_hdr");
    // Both block encodings take one byte a texel, so the two platforms' ULTRA_LOW atlases match in size; LOW keeps the
    // cooked lightmap's encoding on both.
    for tier in ["low", "ultra_low"] {
        let (d, a) = (tier_row(&desktop, tier), tier_row(&android, tier));
        assert_eq!(
            (&d["width"], &d["height"], &d["density"]),
            (&a["width"], &a["height"], &a["density"]),
            "{tier}: {d} {a}"
        );
    }
    assert_eq!(
        tier_row(&desktop, "low")["encoding"],
        tier_row(&android, "low")["encoding"]
    );
}
