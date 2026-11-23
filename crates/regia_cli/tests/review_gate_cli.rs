// SPDX-License-Identifier: MPL-2.0
//! Spec 23 § 5 end to end: with `[provenance] require_review_for_shipping = true` in `Regia.toml`,
//! `regia cook --quality final` refuses agent-made assets still pending review, names them, and
//! lets them through once the owner has reviewed them (ADR-2346; test added by ADR-5628). The unit
//! rules (which tools count, which folders are skipped) are in `commands/review_gate.rs`.

use std::path::Path;
use std::process::{Command, Output};

use regia_asset::{MetaSidecar, Provenance, ProvenanceTool, ReviewerState};

/// The gate's refusal text (`commands/review_gate.rs`, `check`).
const REFUSAL: &str = "are not reviewed";

fn write_meta(project: &Path, source: &str, reviewer: ReviewerState) {
    let mut meta = MetaSidecar::new("material_ron", regia_asset::AssetId::new_random());
    meta.provenance = Some(Provenance {
        tool: ProvenanceTool::Agent,
        reviewer,
        ..Provenance::default()
    });
    let path = regia_asset::meta_path_for(&project.join(source));
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
    std::fs::write(path, meta.to_ron_string().expect("ron")).expect("meta");
}

fn cook(project: &Path, quality: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_regia"))
        .args(["cook", "--quality", quality, "--project"])
        .arg(project)
        .output()
        .expect("run regia cook")
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
fn a_final_cook_refuses_unreviewed_agent_assets_until_the_owner_reviews_them() {
    let project = regia_test_utils::unique_temp_dir("review-gate-cli");
    std::fs::create_dir_all(project.join("assets")).expect("assets");
    std::fs::write(
        project.join("Regia.toml"),
        "[project]\nname = \"Gate\"\nid = \"com.example.gate\"\n\n[provenance]\nrequire_review_for_shipping = true\n",
    )
    .expect("manifest");
    write_meta(&project, "assets/agent_rock.material.ron", ReviewerState::Pending);

    let refused = cook(&project, "final");
    let said = text(&refused);
    assert!(!refused.status.success(), "the final cook is refused: {said}");
    assert!(said.contains(REFUSAL), "{said}");
    assert!(
        said.contains("agent_rock.material.ron"),
        "names the asset: {said}"
    );

    // A fast (development) cook is not a shipping cook: the gate does not run.
    let fast = cook(&project, "fast");
    assert!(!text(&fast).contains(REFUSAL), "{}", text(&fast));

    // Once the owner has reviewed it, the final cook gets past the gate.
    write_meta(&project, "assets/agent_rock.material.ron", ReviewerState::Owner);
    let reviewed = cook(&project, "final");
    assert!(!text(&reviewed).contains(REFUSAL), "{}", text(&reviewed));

    let _ = std::fs::remove_dir_all(&project);
}
