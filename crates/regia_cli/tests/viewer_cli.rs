// SPDX-License-Identifier: MPL-2.0
//! The real `regia viewer` binary (ADR-2540): `standin --pack` writes the stand-in kit, recipes,
//! three verified packages and the demo page; `verify` passes on a package and fails with exit 1
//! after a byte changes; `pack --json --strict-budget` reports the size and stays in budget.

use std::path::Path;
use std::process::{Command, Output};

fn regia(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_regia"))
        .args(args)
        .output()
        .expect("run regia")
}

fn ok(out: &Output) -> String {
    assert!(
        out.status.success(),
        "regia failed: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn s(p: &Path) -> &str {
    p.to_str().expect("utf-8 temp path")
}

#[test]
fn standin_pack_verify_and_tamper() {
    let dir = regia_test_utils::unique_temp_dir("viewer-cli");
    let text = ok(&regia(&["viewer", "standin", "-o", s(&dir), "--pack"]));
    assert!(text.contains("package"), "{text}");
    assert!(dir.join("demo/index.html").is_file());
    for name in [
        "standin-marquise-bezel-end",
        "standin-round-prong-end",
        "standin-dangle-ring",
    ] {
        let pkg = dir.join("packages").join(name);
        let text = ok(&regia(&["viewer", "verify", s(&pkg), "--json"]));
        let v: serde_json::Value = serde_json::from_str(&text).expect("json");
        assert_eq!(v["ok"], true, "{text}");
        assert_eq!(v["report"]["within_budget"], true);
    }
    // One changed byte: verify fails with the hash reason.
    let mesh = dir.join("packages/standin-round-prong-end/mesh.rvm");
    let mut bytes = std::fs::read(&mesh).expect("mesh");
    let mid = bytes.len() / 2;
    bytes[mid] ^= 1;
    std::fs::write(&mesh, bytes).expect("write");
    let out = regia(&[
        "viewer",
        "verify",
        s(&dir.join("packages/standin-round-prong-end")),
    ]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("pack_hash_mismatch"));
    // Cook one again by hand, with the JSON report and the budget enforced.
    let out_dir = dir.join("again");
    let text = ok(&regia(&[
        "viewer",
        "pack",
        s(&dir.join("kits/shared/standin-parts")),
        s(&dir.join("recipes/house-b/standin-round-prong-end.json")),
        "-o",
        s(&out_dir),
        "--option-graph",
        s(&dir.join("optiongraph/house-b/standin-round-prong-end.json")),
        "--materials",
        s(&dir.join("materials/standin/library.json")),
        "--json",
        "--strict-budget",
    ]));
    let v: serde_json::Value = serde_json::from_str(&text).expect("json");
    assert_eq!(v["manifest"]["schema"], "regia.viewer_package/1");
    assert!(v["report"]["total_bytes"].as_u64().unwrap_or(u64::MAX) < 3_000_000);
    assert!(v["report"]["total_gzip_bytes"].as_u64().is_some());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_recipe_outside_the_view_rules_is_refused() {
    let dir = regia_test_utils::unique_temp_dir("viewer-cli-bad");
    ok(&regia(&["viewer", "standin", "-o", s(&dir)]));
    let recipe = dir.join("recipes/house-a/standin-marquise-bezel-end.json");
    let mut v: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&recipe).expect("recipe")).expect("json");
    v["view_range"]["tilt_max_deg"] = 95.into();
    std::fs::write(&recipe, serde_json::to_vec(&v).expect("json")).expect("write");
    let out = regia(&[
        "viewer",
        "pack",
        s(&dir.join("kits/shared/standin-parts")),
        s(&recipe),
        "-o",
        s(&dir.join("out")),
    ]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("pack_bad_recipe"));
    let _ = std::fs::remove_dir_all(&dir);
}
