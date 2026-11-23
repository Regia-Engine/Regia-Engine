// SPDX-License-Identifier: MPL-2.0
//! Generated files through the existing command, without image-file input flags.

use super::*;
use clap::Parser;

#[derive(Parser)]
struct Command {
    #[command(flatten)]
    args: Args,
}

struct Files(PathBuf);

impl Files {
    fn new() -> Self {
        Self(regia_test_utils::unique_temp_dir("gi-compare-shapes"))
    }

    fn write(&self, name: &str, text: &str) -> PathBuf {
        let path = self.0.join(name);
        write_atomic(&path, text.as_bytes()).expect("write generated fixture atomically");
        path
    }

    fn args(&self, flags: &[&str]) -> Args {
        Command::try_parse_from(std::iter::once("gi-compare").chain(flags.iter().copied()))
            .expect("documented flags")
            .args
    }
}

impl Drop for Files {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const SCENE: &str = r#"(
    format_version: 1, name: "shapes", sky_lux: 1.0,
    materials: [(name: "grey", albedo: (0.5, 0.5, 0.5))],
    shapes: [(name: "box", kind: Box(half: (1.0, 0.1, 1.0)), material: "grey")],
    layout: (bricks: [(min: (-1.0, 0.5, -1.0), max: (1.0, 1.5, 1.0), spacing: 2.0)],
             moment_resolution: 2),
    bake: (rays_per_probe: 16, samples: 1, max_bounces: 1, seed: 7),
)"#;

const VIEWS: &str = r#"(
    format_version: 1, size: (17, 9), ev100: 0.0,
    views: [(name: "tiny", eye: (0.0, 1.0, 3.0), target: (0.0, 0.0, 0.0), fov_y_deg: 45.0)],
)"#;

#[test]
fn generated_command_files_refuse_an_empty_requested_image_before_gpu_work() {
    let files = Files::new();
    let scene = files.write("shapes.gi_compare.ron", SCENE);
    let views = files.write("shapes.views.ron", VIEWS);
    let args = files.args(&[
        "--scene",
        scene.to_str().expect("path"),
        "--views",
        views.to_str().expect("path"),
        "--size",
        "0x9",
        "--out",
        files.0.to_str().expect("path"),
    ]);
    let err = run_inner(&args, &files.0).expect_err("empty requested size");
    assert!(err.to_string().starts_with("[views_size]"), "{err}");
    assert_eq!(run(&args, &files.0).expect("command result"), ExitCode::from(2));
    assert!(!files.0.join("summary.json").exists());
}

#[test]
fn generated_summary_read_preserves_the_existing_report_and_baseline_size_policy() {
    let files = Files::new();
    let scores = Scores {
        ssim: 1.0,
        ..Scores::default()
    };
    let summary = Summary {
        schema: SUMMARY_SCHEMA,
        fixture: "shapes".into(),
        size: (3, 5),
        spp: 1,
        views: vec![ViewSummary {
            name: "tiny".into(),
            sheet: "tiny.png".into(),
            full: scores,
            indirect: scores,
            gi: summary::GiStatus {
                active: true,
                ..summary::GiStatus::default()
            },
            ..ViewSummary::default()
        }],
        ..Summary::default()
    };
    let text = serde_json::to_string_pretty(&summary).expect("summary");
    let path = files.write("summary.json", &text);
    let baseline = Summary {
        size: (5, 3),
        ..summary.clone()
    };
    let baseline_path = files.write(
        "baseline.json",
        &serde_json::to_string_pretty(&baseline).expect("baseline"),
    );
    let args = files.args(&[
        "--read",
        path.to_str().expect("path"),
        "--baseline",
        baseline_path.to_str().expect("path"),
    ]);
    assert_eq!(run(&args, &files.0).expect("read command"), ExitCode::SUCCESS);
    let loaded = load_summary(&path).expect("read summary");
    let v = verdict(&loaded, Some(&baseline), DEFAULT_FLIP_TOLERANCE);
    assert!(v.flags.is_empty());
    assert!(v.info.iter().any(|info| info.contains("size")));
    assert_eq!(render_table(&loaded, &v), render_table(&summary, &v));
    assert_eq!(std::fs::read_to_string(path).expect("unchanged summary"), text);
}

#[test]
#[ignore = "renders a generated fixture; run explicitly with the lavapipe Vulkan ICD"]
fn generated_scene_runs_the_existing_command_at_odd_dimensions() {
    let files = Files::new();
    let scene = files.write("shapes.gi_compare.ron", SCENE);
    let views = files.write("shapes.views.ron", VIEWS);
    let args = files.args(&[
        "--scene",
        scene.to_str().expect("path"),
        "--views",
        views.to_str().expect("path"),
        "--out",
        files.0.to_str().expect("path"),
        "--spp",
        "1",
        "--ddgi-frames",
        "1",
        "--flythrough-frames",
        "0",
        // The documented override chooses both images' dimensions before rendering.
        "--size",
        "9x17",
    ]);
    let code = run_inner(&args, &files.0).expect("rendered comparison");
    assert!(code == ExitCode::SUCCESS || code == ExitCode::FAILURE, "{code:?}");
    let summary = load_summary(&files.0.join("summary.json")).expect("generated summary");
    assert_eq!(summary.size, (9, 17));
    assert_eq!(summary.views.len(), 1);
    assert!(summary.views[0].full.flip_mean.is_finite());
    assert!(files.0.join("tiny.png").exists());
}
