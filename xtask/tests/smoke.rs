// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask smoke --no-gpu` covers the eighteen alpha rows and the three v0.1.5
//! scene rows without a device.

use std::path::PathBuf;
use std::process::ExitCode;

use xtask::smoke::{SmokeArgs, engine_checks, resolve_content, run};

fn workspace() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(PathBuf::from)
        .expect("xtask has a parent")
}

#[test]
fn no_gpu_smoke_writes_a_summary_for_every_area() {
    let root = workspace();
    let checks = engine_checks(&root, None);
    assert_eq!(
        checks.len(),
        21,
        "18 alpha areas plus ultrawide, black sky and many units"
    );
    let out = std::env::temp_dir().join(format!("regia-smoke-{}", std::process::id()));
    std::fs::create_dir_all(&out).expect("temp smoke dir");
    let empty = out.join("content");
    std::fs::create_dir_all(&empty).expect("empty content dir");
    assert!(resolve_content(Some(&empty)).is_some());
    assert!(resolve_content(Some(out.join("missing").as_path())).is_none());

    let code = run(
        &SmokeArgs {
            out: Some(out.clone()),
            no_gpu: true,
            soak_seconds: 60.0,
            frames: 4,
            warmup: 1,
            width: 160,
            height: 90,
            content: Some(empty),
            scenes: false,
        },
        &root,
    )
    .expect("smoke summary");
    assert_eq!(code, ExitCode::SUCCESS);
    let json = std::fs::read_to_string(out.join("alpha-smoke.json")).expect("json");
    let md = std::fs::read_to_string(out.join("alpha-smoke.md")).expect("markdown");
    assert!(json.contains("\"N-A\""), "{json}");
    assert!(md.contains("| 14 | Hulls | N-A |"));
    assert!(md.contains("| 18 | Packaging | N-A |"));
    assert!(md.contains("| 19 | Ultrawide | N-A |"));
    assert!(md.contains("| 20 | Black sky | N-A |"));
    assert!(md.contains("| 21 | Many units | N-A |"));
    assert!(json.contains("\"area\": \"Combat\""));
    let _ = std::fs::remove_dir_all(&out);
}
