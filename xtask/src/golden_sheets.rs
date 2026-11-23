// SPDX-License-Identifier: MPL-2.0
//! `golden-sheets`: contact sheets of every golden PNG that changed between two revisions.
//!
//! ```text
//! cargo xtask golden-sheets --from <rev> --to <rev>
//! ```
//!
//! The command lists PNGs under any `goldens/` directory via `git diff -z --name-status`,
//! draws each change as **old | new | |diff|×8** (per-channel absolute difference × 8),
//! and writes at most 12 comparisons per sheet plus `index.md`. The index names the commit
//! in `<from>..<to>` that touched each golden, with that commit's message.
//!
//! Output defaults to `target/golden-sheets/`. `--out` replaces `index.md` and `sheet-NN.png`
//! in the directory you give it.

mod diff;
pub(crate) mod font;
mod history;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use image::RgbaImage;

use history::{ChangeKind, CommitNote, PathChange};

/// Most comparisons placed on one contact sheet.
pub const MAX_PER_SHEET: usize = 12;

/// Command-line options of `cargo xtask golden-sheets`.
#[derive(clap::Args)]
pub struct Args {
    /// Older revision. Its tree is the left-hand image.
    #[arg(long)]
    pub from: String,
    /// Newer revision. Its tree is the right-hand image.
    #[arg(long)]
    pub to: String,
    /// Directory this command owns. It writes `index.md` and `sheet-NN.png`, replacing
    /// any previous files with those names.
    ///
    /// Relative paths are resolved from the workspace root. The default is
    /// `target/golden-sheets`.
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Comparisons per contact sheet, from 1 to 12.
    #[arg(long, default_value_t = MAX_PER_SHEET)]
    pub per_sheet: usize,
}

/// One golden in the index.
struct GoldenRecord {
    index: usize,
    sheet: usize,
    sheet_file: String,
    path: String,
    previous: Option<String>,
    status: String,
    changed: u64,
    total: u64,
    max_diff: u8,
    old_size: Option<(u32, u32)>,
    new_size: Option<(u32, u32)>,
    commits: Vec<CommitNote>,
    commits_outside_range: bool,
}

struct SheetIndex<'a> {
    from_rev: &'a str,
    to_rev: &'a str,
    from_hash: &'a str,
    to_hash: &'a str,
    per_sheet: usize,
    records: &'a [GoldenRecord],
}

/// Write contact sheets for golden PNGs that differ between `args.from` and `args.to`.
///
/// Returns [`ExitCode::SUCCESS`] after the index is written, including when nothing changed.
/// A bad revision, a corrupt PNG, or a git failure is an error.
pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    check_args(args)?;
    let from_hash = history::resolve_commit(root, &args.from)?;
    let to_hash = history::resolve_commit(root, &args.to)?;
    let changes = history::changed_goldens(root, &from_hash, &to_hash)?;
    let range = history::commit_order(root, &from_hash, &to_hash)?;
    let out = output_dir(root, args.out.as_deref());
    std::fs::create_dir_all(&out).with_context(|| format!("creating {}", out.display()))?;
    clear_previous(&out)?;

    let records = build_sheets(root, args, &from_hash, &to_hash, &changes, &range, &out)?;
    let sheet_count = records.iter().map(|record| record.sheet).max().unwrap_or(0);
    let index = render_index(&SheetIndex {
        from_rev: &args.from,
        to_rev: &args.to,
        from_hash: &from_hash,
        to_hash: &to_hash,
        per_sheet: args.per_sheet,
        records: &records,
    });
    let index_path = out.join("index.md");
    std::fs::write(&index_path, index).with_context(|| format!("writing {}", index_path.display()))?;
    println!(
        "golden-sheets: {} golden(s), {} sheet(s) -> {}",
        records.len(),
        sheet_count,
        index_path.display()
    );
    Ok(ExitCode::SUCCESS)
}

fn check_args(args: &Args) -> Result<()> {
    if args.from.trim().is_empty() || args.to.trim().is_empty() {
        bail!("--from and --to must be non-empty git revisions");
    }
    if args.per_sheet == 0 || args.per_sheet > MAX_PER_SHEET {
        bail!("--per-sheet must be from 1 to {MAX_PER_SHEET}");
    }
    Ok(())
}

fn output_dir(root: &Path, out: Option<&Path>) -> PathBuf {
    match out {
        Some(path) if path.is_absolute() => path.to_path_buf(),
        Some(path) => root.join(path),
        None => root.join("target/golden-sheets"),
    }
}

fn clear_previous(dir: &Path) -> Result<()> {
    let entries = std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("reading {}", dir.display()))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name == "index.md" || is_sheet_name(name) {
            std::fs::remove_file(entry.path())
                .with_context(|| format!("removing {}", entry.path().display()))?;
        }
    }
    Ok(())
}

fn is_sheet_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("sheet-") else {
        return false;
    };
    let Some(number) = rest.strip_suffix(".png") else {
        return false;
    };
    !number.is_empty() && number.chars().all(|ch| ch.is_ascii_digit())
}

fn build_sheets(
    root: &Path,
    args: &Args,
    from_hash: &str,
    to_hash: &str,
    changes: &[PathChange],
    range: &std::collections::HashMap<String, usize>,
    out: &Path,
) -> Result<Vec<GoldenRecord>> {
    let pages = diff::page_bounds(changes.len(), args.per_sheet);
    let sheet_total = pages.len();
    let mut records = Vec::with_capacity(changes.len());
    let mut rendered: Vec<RgbaImage> = Vec::new();
    let mut sheet_idx = 0usize;

    for (ordinal, change) in changes.iter().enumerate() {
        let (old_img, new_img) = load_pair(root, from_hash, to_hash, change)?;
        let row = diff::render_comparison(&diff::Comparison {
            index: ordinal + 1,
            path: &change.path,
            status: change.kind.label(),
            old: old_img.as_ref(),
            new: new_img.as_ref(),
        });
        println!(
            "  {} {}  changed {}  max {}",
            change.kind.label(),
            display_path(change),
            row.stats.changed,
            row.stats.max_diff
        );
        let paths = commit_paths(change);
        let (commits, outside) = history::file_commits(root, from_hash, to_hash, &paths, range)?;
        let sheet_number = sheet_idx + 1;
        records.push(GoldenRecord {
            index: ordinal + 1,
            sheet: sheet_number,
            sheet_file: sheet_name(sheet_number),
            path: change.path.clone(),
            previous: change.previous.clone(),
            status: change.kind.label().to_string(),
            changed: row.stats.changed,
            total: row.stats.total,
            max_diff: row.stats.max_diff,
            old_size: row.old_size,
            new_size: row.new_size,
            commits,
            commits_outside_range: outside,
        });
        rendered.push(row.image);

        let end_of_page = rendered.len() == args.per_sheet || ordinal + 1 == changes.len();
        if end_of_page && !rendered.is_empty() {
            sheet_idx += 1;
            let header = format!(
                "{} -> {}   sheet {sheet_idx}/{sheet_total}",
                short_rev(&args.from, from_hash),
                short_rev(&args.to, to_hash)
            );
            let sheet = diff::compose_sheet(&header, &rendered);
            let path = out.join(sheet_name(sheet_idx));
            sheet
                .save(&path)
                .with_context(|| format!("writing {}", path.display()))?;
            rendered.clear();
        }
    }
    Ok(records)
}

fn commit_paths(change: &PathChange) -> Vec<&str> {
    let mut paths = Vec::with_capacity(2);
    if let Some(previous) = change.previous.as_deref() {
        paths.push(previous);
    }
    paths.push(change.path.as_str());
    paths
}

fn display_path(change: &PathChange) -> String {
    match &change.previous {
        Some(previous) => format!("{previous} -> {}", change.path),
        None => change.path.clone(),
    }
}

fn load_pair(
    root: &Path,
    from_hash: &str,
    to_hash: &str,
    change: &PathChange,
) -> Result<(Option<RgbaImage>, Option<RgbaImage>)> {
    let old_path = change.previous.as_deref().unwrap_or(change.path.as_str());
    let new_path = change.path.as_str();
    let old = match change.kind {
        ChangeKind::Added => None,
        _ => decode_blob(root, from_hash, old_path)?,
    };
    let new = match change.kind {
        ChangeKind::Deleted => None,
        _ => decode_blob(root, to_hash, new_path)?,
    };
    Ok((old, new))
}

fn decode_blob(root: &Path, rev: &str, path: &str) -> Result<Option<RgbaImage>> {
    let Some(bytes) = history::show_blob(root, rev, path)? else {
        return Ok(None);
    };
    let image = image::load_from_memory(&bytes)
        .with_context(|| format!("decoding {rev}:{path}"))?
        .into_rgba8();
    if image.width() == 0 || image.height() == 0 {
        bail!("{rev}:{path} has a zero dimension");
    }
    Ok(Some(image))
}

fn sheet_name(number: usize) -> String {
    format!("sheet-{number:02}.png")
}

fn short_rev(name: &str, hash: &str) -> String {
    let short = hash.get(..12).unwrap_or(hash);
    if name == hash {
        short.to_string()
    } else {
        format!("{name} ({short})")
    }
}

fn render_index(index: &SheetIndex<'_>) -> String {
    let sheets = index.records.iter().map(|record| record.sheet).max().unwrap_or(0);
    let mut out = String::new();
    out.push_str("# Golden sheets\n\n");
    out.push_str(&format!(
        "Old `{}` (`{}`)\n\nNew `{}` (`{}`)\n\n",
        inline(index.from_rev),
        index.from_hash,
        inline(index.to_rev),
        index.to_hash
    ));
    if index.records.is_empty() {
        out.push_str("No golden PNG changed between these revisions.\n");
        return out;
    }
    out.push_str(&format!(
        "{} golden PNG(s) changed. {} contact sheet(s), at most {} comparisons each.\n\n",
        index.records.len(),
        sheets,
        index.per_sheet
    ));
    out.push_str(
        "Each comparison is **old | new | |diff|×8**: the per-channel absolute difference \
         multiplied by 8 and clamped to 255. `changed` counts pixels in the union rectangle \
         whose RGBA differs (a missing side is zero). `max` is the largest channel delta before \
         that gain, from 0 to 255. A shrunk diff panel keeps the brightest sample in each block, \
         so a single changed pixel stays visible. An added or deleted image is diffed against \
         empty pixels, which saturates the diff panel; the picture itself is the other panel.\n\n",
    );
    out.push_str("## Sheets\n\n");
    for number in 1..=sheets {
        let name = sheet_name(number);
        out.push_str(&format!("- [{name}]({name})\n"));
    }
    out.push('\n');
    for record in index.records {
        out.push_str(&format!("## {}. `{}`\n\n", record.index, inline(&record.path)));
        out.push_str(&format!(
            "- Sheet: [{}]({})\n",
            record.sheet_file, record.sheet_file
        ));
        out.push_str(&format!("- Status: {}\n", record.status));
        if let Some(previous) = &record.previous {
            out.push_str(&format!("- Previous path: `{}`\n", inline(previous)));
        }
        out.push_str(&format!(
            "- Size: {}\n",
            size_text(record.old_size, record.new_size)
        ));
        out.push_str(&format!(
            "- Pixels changed: {} / {} ({})\n",
            record.changed,
            record.total,
            diff::percent(record.changed, record.total)
        ));
        out.push_str(&format!("- Max channel diff: {} / 255\n", record.max_diff));
        if record.commits.is_empty() {
            out.push_str("\nNo commit message found for this path.\n\n");
            continue;
        }
        if record.commits_outside_range {
            out.push_str(
                "\nNo commit in this revision range touches the path. Latest commit at the newer revision:\n\n",
            );
        } else {
            out.push_str("\nCommits in this revision range:\n\n");
        }
        for commit in &record.commits {
            out.push_str(&format!("- `{}` {}\n", commit.hash, inline(&commit.subject)));
            if !commit.body.is_empty() {
                out.push('\n');
                for line in commit.body.lines() {
                    out.push_str("    ");
                    out.push_str(line.trim_end_matches('\r'));
                    out.push('\n');
                }
                out.push('\n');
            }
        }
        out.push('\n');
    }
    out
}

fn size_text(old: Option<(u32, u32)>, new: Option<(u32, u32)>) -> String {
    match (old, new) {
        (Some(a), Some(b)) if a == b => format!("{}×{}", a.0, a.1),
        (Some(a), Some(b)) => format!("{}×{} → {}×{}", a.0, a.1, b.0, b.1),
        (Some(a), None) => format!("{}×{} (deleted)", a.0, a.1),
        (None, Some(b)) => format!("{}×{} (added)", b.0, b.1),
        (None, None) => "no image".to_string(),
    }
}

fn inline(text: &str) -> String {
    text.replace('`', "'")
}

#[cfg(test)]
mod e2e;

#[cfg(test)]
mod tests {
    use super::*;

    fn record(index: usize, sheet: usize) -> GoldenRecord {
        GoldenRecord {
            index,
            sheet,
            sheet_file: sheet_name(sheet),
            path: format!("demo/goldens/n{index}.png"),
            previous: None,
            status: "modified".to_string(),
            changed: 1,
            total: 64,
            max_diff: 1,
            old_size: Some((8, 8)),
            new_size: Some((8, 8)),
            commits: vec![CommitNote {
                hash: "abcdef1234567890".to_string(),
                subject: "re-bless the square".to_string(),
                body: "one pixel stepped".to_string(),
            }],
            commits_outside_range: false,
        }
    }

    #[test]
    fn per_sheet_above_twelve_is_rejected_before_git() {
        let err = run(
            Path::new("/tmp"),
            &Args {
                from: "a".into(),
                to: "b".into(),
                out: None,
                per_sheet: 13,
            },
        )
        .expect_err("per-sheet");
        let message = format!("{err:#}");
        assert!(message.contains("12"), "{message}");
    }

    #[test]
    fn index_names_the_commit_and_the_diff_stats() {
        let records = vec![record(1, 1)];
        let md = render_index(&SheetIndex {
            from_rev: "green",
            to_rev: "main",
            from_hash: "aaa",
            to_hash: "bbb",
            per_sheet: 12,
            records: &records,
        });
        assert!(md.contains("old | new | |diff|×8"), "{md}");
        assert!(md.contains("`demo/goldens/n1.png`"), "{md}");
        assert!(md.contains("Pixels changed: 1 / 64 (1.5%)"), "{md}");
        assert!(md.contains("Max channel diff: 1 / 255"), "{md}");
        assert!(md.contains("`abcdef1234567890` re-bless the square"), "{md}");
        assert!(md.contains("one pixel stepped"), "{md}");
        assert!(md.contains("[sheet-01.png](sheet-01.png)"), "{md}");
        assert!(md.contains("Old `green` (`aaa`)"), "{md}");
    }

    #[test]
    fn empty_range_says_nothing_changed() {
        let md = render_index(&SheetIndex {
            from_rev: "a",
            to_rev: "a",
            from_hash: "abc",
            to_hash: "abc",
            per_sheet: 12,
            records: &[],
        });
        assert!(md.contains("No golden PNG changed"), "{md}");
        assert!(!md.contains("sheet-01.png"));
    }

    #[test]
    fn sheet_names_are_the_files_this_command_owns() {
        assert!(is_sheet_name("sheet-01.png"));
        assert!(is_sheet_name("sheet-100.png"));
        assert!(!is_sheet_name("sheet-.png"));
        assert!(!is_sheet_name("sheet-1.txt"));
        assert!(!is_sheet_name("index.md"));
    }
}
