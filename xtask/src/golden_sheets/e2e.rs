// SPDX-License-Identifier: MPL-2.0
//! End-to-end: a temporary git repository with added, edited, renamed, and deleted goldens.

use std::path::{Path, PathBuf};
use std::process::Command;

use image::{Rgba, RgbaImage};

use super::{Args, run};

struct TempGit(PathBuf);

impl Drop for TempGit {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn temp_git() -> TempGit {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!("regia-golden-sheets-{nanos}-{}", std::process::id()));
    std::fs::create_dir_all(&path).expect("temp dir");
    TempGit(path)
}

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .arg("-c")
        .arg("user.email=regia-test@example.com")
        .arg("-c")
        .arg("user.name=regia-test")
        .arg("-c")
        .arg("commit.gpgsign=false")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .args(args)
        .output()
        .expect("spawn git");
    if !output.status.success() {
        panic!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

fn rev(dir: &Path) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("spawn git");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("hash").trim().to_string()
}

fn save(path: &Path, image: &RgbaImage) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("parent");
    }
    image.save(path).expect("png");
}

#[test]
fn golden_sheets_writes_contact_sheets_for_a_git_range() {
    let repo = temp_git();
    let root = &repo.0;
    git(root, &["init", "-b", "trunk"]);

    let mut box_png = RgbaImage::from_pixel(8, 8, Rgba([255, 0, 0, 255]));
    save(&root.join("keep/goldens/box.png"), &box_png);
    save(
        &root.join("keep/goldens/old_name.png"),
        &RgbaImage::from_pixel(2, 2, Rgba([0, 255, 0, 255])),
    );
    save(
        &root.join("keep/goldens/gone.png"),
        &RgbaImage::from_pixel(2, 2, Rgba([255, 255, 255, 255])),
    );
    save(
        &root.join("notes/picture.png"),
        &RgbaImage::from_pixel(2, 2, Rgba([8, 8, 8, 255])),
    );
    std::fs::write(root.join("keep/goldens/notes.md"), "not a png\n").expect("notes");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-m", "add base goldens"]);
    let from = rev(root);

    box_png.put_pixel(3, 4, Rgba([255, 1, 0, 255]));
    save(&root.join("keep/goldens/box.png"), &box_png);
    save(
        &root.join("notes/picture.png"),
        &RgbaImage::from_pixel(2, 2, Rgba([9, 9, 9, 255])),
    );
    std::fs::write(root.join("keep/goldens/notes.md"), "still not a png\n").expect("notes");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-m", "re-bless box: one green channel step"]);

    git(
        root,
        &["mv", "keep/goldens/old_name.png", "keep/goldens/new_name.png"],
    );
    git(root, &["commit", "-m", "rename old_name golden to new_name"]);

    save(
        &root.join("keep/goldens/extra.png"),
        &RgbaImage::from_pixel(4, 4, Rgba([0, 0, 255, 255])),
    );
    git(root, &["rm", "keep/goldens/gone.png"]);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-m", "add extra golden and drop gone"]);
    let to = rev(root);

    let out = root.join("sheets");
    let code = run(
        root,
        &Args {
            from: from.clone(),
            to: to.clone(),
            out: Some(out.clone()),
            per_sheet: 2,
        },
    )
    .expect("golden-sheets");
    assert_eq!(code, std::process::ExitCode::SUCCESS);

    let index = std::fs::read_to_string(out.join("index.md")).expect("index");
    assert!(index.contains("re-bless box: one green channel step"), "{index}");
    assert!(index.contains("rename old_name golden to new_name"), "{index}");
    assert!(index.contains("add extra golden and drop gone"), "{index}");
    assert!(!index.contains("add base goldens"), "{index}");
    assert!(index.contains("`keep/goldens/box.png`"), "{index}");
    assert!(index.contains("`keep/goldens/new_name.png`"), "{index}");
    assert!(index.contains("`keep/goldens/old_name.png`"), "{index}");
    assert!(index.contains("`keep/goldens/extra.png`"), "{index}");
    assert!(index.contains("`keep/goldens/gone.png`"), "{index}");
    assert!(index.contains("- Status: modified"), "{index}");
    assert!(index.contains("- Status: added"), "{index}");
    assert!(index.contains("- Status: deleted"), "{index}");
    assert!(index.contains("- Status: renamed"), "{index}");
    assert!(index.contains("Pixels changed: 1 / 64 (1.5%)"), "{index}");
    assert!(index.contains("Max channel diff: 1 / 255"), "{index}");
    assert!(index.contains("[sheet-01.png](sheet-01.png)"), "{index}");
    assert!(index.contains("[sheet-02.png](sheet-02.png)"), "{index}");
    assert!(!index.contains("picture.png"), "{index}");
    assert!(!index.contains("notes.md"), "{index}");
    assert!(!out.join("sheet-03.png").exists());

    let sheet = image::open(out.join("sheet-01.png"))
        .expect("sheet png")
        .into_rgba8();
    assert!(
        sheet.pixels().any(|pixel| pixel.0 == [0, 8, 0, 255]),
        "amplified one-channel step missing from sheet-01"
    );

    let empty = root.join("empty");
    let code = run(
        root,
        &Args {
            from: from.clone(),
            to: from,
            out: Some(empty.clone()),
            per_sheet: 12,
        },
    )
    .expect("empty range");
    assert_eq!(code, std::process::ExitCode::SUCCESS);
    let empty_index = std::fs::read_to_string(empty.join("index.md")).expect("empty index");
    assert!(empty_index.contains("No golden PNG changed"), "{empty_index}");
    assert!(!empty.join("sheet-01.png").exists());

    let err = run(
        root,
        &Args {
            from: "no-such-rev-regia".into(),
            to,
            out: Some(root.join("bad")),
            per_sheet: 12,
        },
    )
    .expect_err("bad rev");
    let message = format!("{err:#}");
    assert!(
        message.contains("no-such-rev-regia") || message.contains("git "),
        "{message}"
    );
}
