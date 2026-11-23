// SPDX-License-Identifier: MPL-2.0
//! `regia render --job` with a `product` block (ADR-2757) through the real binary: no app runs, and the frames and the
//! manifest land on disk. On a machine with no adapter the job fails with exit 3, and the frame checks skip.

use std::path::Path;
use std::process::{Command, Output};

use regia_test_utils::unique_temp_dir;
use serde_json::Value;

fn regia(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_regia"))
        .args(args)
        .output()
        .expect("spawn the regia binary")
}

fn text(path: &Path) -> &str {
    path.to_str().expect("temp paths are UTF-8")
}

/// A project with a stand-in package as `products/a`.
fn project(tag: &str) -> std::path::PathBuf {
    let root = unique_temp_dir(&format!("cli-product-job-{tag}"));
    let standin = root.join("standin");
    let out = regia(&["viewer", "standin", "-o", text(&standin), "--pack"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let packages = standin.join("packages");
    let first = std::fs::read_dir(&packages)
        .expect("packages")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.join("manifest.json").is_file())
        .expect("a package");
    std::fs::create_dir_all(root.join("products")).expect("products");
    std::fs::rename(first, root.join("products").join("a")).expect("move");
    root
}

const JOB: &str = r#"(version: 3, name: "clip", out: "out",
    output: (width: Some(48), height: Some(48), fps: Some(10), writers: Some(1)),
    takes: [(name: "all", range: Frames(start: 0, end: 3))],
    product: Some((package: "products/a", view: [(at: Frame(2), tilt: Some((8.0, 0.0)))])))"#;

#[test]
fn a_product_job_needs_no_app_and_writes_its_frames() {
    let root = project("run");
    let file = root.join("clip.render_job.ron");
    std::fs::write(&file, JOB).expect("job file");
    let args = [
        "render",
        "--job",
        text(&file),
        "--project",
        text(&root),
        "--date",
        "2026-11-05",
    ];
    let run = regia(&args);
    let stderr = String::from_utf8_lossy(&run.stderr).into_owned();
    if run.status.code() == Some(2) && stderr.contains("F0003") {
        assert!(
            std::env::var_os("REGIA_VIEWER_REQUIRE_ADAPTER").is_none(),
            "REGIA_VIEWER_REQUIRE_ADAPTER is set and there is no adapter: {stderr}"
        );
        return;
    }
    if run.status.code() == Some(3) && stderr.contains("no GPU adapter") {
        assert!(
            std::env::var_os("REGIA_VIEWER_REQUIRE_ADAPTER").is_none(),
            "REGIA_VIEWER_REQUIRE_ADAPTER is set and there is no adapter: {stderr}"
        );
        return;
    }
    assert_eq!(run.status.code(), Some(0), "{stderr}");
    let manifest = String::from_utf8_lossy(&run.stdout).trim().to_owned();
    let block: Value =
        serde_json::from_slice(&std::fs::read(&manifest).expect("the manifest it printed")).expect("json");
    assert_eq!(block["schema"], "jewelry.render_out/1");
    assert_eq!(block["delivered"], 3);
    for frame in 0..3 {
        assert!(
            root.join(format!("out/all/all_{frame:06}.png")).is_file(),
            "frame {frame}: {block}"
        );
    }
    let again = regia(&args);
    assert_eq!(again.status.code(), Some(0));
    let second: Value = serde_json::from_slice(&std::fs::read(&manifest).expect("manifest")).expect("json");
    assert_eq!(block["frame_hashes"], second["frame_hashes"], "the same frames");
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_dry_run_prints_the_plan_and_a_refused_job_exits_2() {
    let root = project("plan");
    let file = root.join("clip.render_job.ron");
    std::fs::write(&file, JOB).expect("job file");
    let dry = regia(&[
        "render",
        "--job",
        text(&file),
        "--project",
        text(&root),
        "--dry-run",
    ]);
    assert_eq!(
        dry.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&dry.stderr)
    );
    let plan: Value = serde_json::from_slice(&dry.stdout).expect("a plan");
    assert_eq!(plan["frames"], 3);
    assert_eq!(plan["product"]["renderer_used"], "regia_viewer");

    let far = JOB.replace("(8.0, 0.0)", "(80.0, 0.0)");
    std::fs::write(&file, far).expect("job file");
    let refused = regia(&["render", "--job", text(&file), "--project", text(&root)]);
    assert_eq!(refused.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("render_job_outside_view_range"), "{stderr}");
    std::fs::remove_dir_all(&root).ok();
}

/// The shot list published as `capture_hooks/1/fixtures/move.shots.ron`, verbatim: a Catmull-Rom push on a diagonal
/// with a level look-at camera, read in mm.
const PUBLISHED_SHOTS: &str = r#"// A camera path as a shot list, the other way to give one: numbers read as mm in the piece's frame. Give it in the job's
// `shots`, and leave `product.view` out. Fades and depth of field are not drawn.
(version: 2, fps: 30.0, shots: [
    (name: "push", duration: 3.0, camera: (path: (interp: CatmullRom,
        keys: [(time: 0.0, position: (0.0, 0.0, 60.0)), (time: 3.0, position: (8.0, 4.0, 50.0))],
        look_at: Point((0.0, 0.0, 0.0))), fov: [(time: 0.0, value: 16.0)])),
])
"#;

/// The published shot list plans over its whole 3 s with no refusal (it was once refused for a second-order roll of a
/// level camera on a diagonal), and its first frames render through the real binary, the same every run.
#[test]
fn the_published_shot_list_plans_whole_and_renders() {
    let root = project("published-shots");
    std::fs::create_dir_all(root.join("capture")).expect("capture dir");
    std::fs::write(root.join("capture").join("move.shots.ron"), PUBLISHED_SHOTS).expect("shots file");
    let file = root.join("move.render_job.ron");
    let job = |takes: &str| {
        format!(
            r#"(version: 3, name: "move", out: "out", shots: Some("capture/move.shots.ron"),
                output: (width: Some(32), height: Some(32), fps: Some(30), writers: Some(1)),
                takes: [{takes}],
                product: Some((package: "products/a")))"#
        )
    };
    std::fs::write(&file, job(r#"(name: "all", range: Whole)"#)).expect("job file");
    let dry = regia(&[
        "render",
        "--job",
        text(&file),
        "--project",
        text(&root),
        "--dry-run",
    ]);
    let stderr = String::from_utf8_lossy(&dry.stderr).into_owned();
    assert_eq!(dry.status.code(), Some(0), "{stderr}");
    let plan: Value = serde_json::from_slice(&dry.stdout).expect("a plan");
    assert_eq!(plan["frames"], 90, "3 s at 30 fps: {plan}");

    std::fs::write(&file, job(r#"(name: "all", range: Frames(start: 0, end: 3))"#)).expect("job file");
    let args = [
        "render",
        "--job",
        text(&file),
        "--project",
        text(&root),
        "--date",
        "2026-11-05",
    ];
    let run = regia(&args);
    let stderr = String::from_utf8_lossy(&run.stderr).into_owned();
    if matches!(run.status.code(), Some(2) | Some(3))
        && (stderr.contains("F0003") || stderr.contains("no GPU adapter"))
    {
        assert!(
            std::env::var_os("REGIA_VIEWER_REQUIRE_ADAPTER").is_none(),
            "REGIA_VIEWER_REQUIRE_ADAPTER is set and there is no adapter: {stderr}"
        );
        return;
    }
    assert_eq!(run.status.code(), Some(0), "{stderr}");
    let manifest = String::from_utf8_lossy(&run.stdout).trim().to_owned();
    let block: Value =
        serde_json::from_slice(&std::fs::read(&manifest).expect("the manifest it printed")).expect("json");
    assert_eq!(block["delivered"], 3, "{block}");
    let again = regia(&args);
    assert_eq!(
        again.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&again.stderr)
    );
    let second: Value = serde_json::from_slice(&std::fs::read(&manifest).expect("manifest")).expect("json");
    assert_eq!(block["frame_hashes"], second["frame_hashes"], "the same frames");
    std::fs::remove_dir_all(&root).ok();
}
