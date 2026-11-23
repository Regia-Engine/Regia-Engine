// SPDX-License-Identifier: MPL-2.0
//! Check relative links in the repository documentation tree.

use anyhow::{Context, Result};
use std::collections::VecDeque;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

const BASELINE: &str = "docs/ci/docs-links.toml";

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn denied(root: &Path) -> Result<Vec<PathBuf>> {
    let text =
        std::fs::read_to_string(root.join("CLEANROOM-DENY.txt")).context("reading CLEANROOM-DENY.txt")?;
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(PathBuf::from)
        .collect())
}

fn forbidden(root: &Path, path: &Path, denies: &[PathBuf]) -> bool {
    let relative = path.strip_prefix(root).unwrap_or(path);
    denies.iter().any(|deny| {
        if deny.is_absolute() {
            path.starts_with(deny)
        } else {
            relative.starts_with(deny)
        }
    }) || relative.components().any(|part| {
        matches!(
            part.as_os_str().to_str(),
            Some("generic-game-engine-specs" | "public-sources")
        )
    })
}

fn inspect_target(root: &Path, path: &Path, denies: &[PathBuf]) -> Result<&'static str> {
    let mut pending: VecDeque<OsString> = path
        .strip_prefix(root)
        .unwrap_or(path)
        .components()
        .filter_map(|part| match part {
            Component::Normal(name) => Some(name.to_os_string()),
            _ => None,
        })
        .collect();
    let mut current = root.to_path_buf();
    let mut symlink_hops = 0;
    while let Some(part) = pending.pop_front() {
        current.push(part);
        if forbidden(root, &current, denies) {
            return Ok("forbidden path");
        }
        let metadata = match std::fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok("missing file"),
            Err(error) => {
                return Err(error).with_context(|| format!("checking link target {}", path.display()));
            }
        };
        if metadata.file_type().is_symlink() {
            symlink_hops += 1;
            if symlink_hops > 40 {
                return Ok("missing file");
            }
            let target = std::fs::read_link(&current)
                .with_context(|| format!("reading symlink {}", current.display()))?;
            current = normalize(&current.parent().unwrap_or(root).join(target));
            if forbidden(root, &current, denies) {
                return Ok("forbidden path");
            }
            if !current.starts_with(root) {
                return Ok("leaves repository");
            }
            let mut expanded: VecDeque<OsString> = current
                .strip_prefix(root)
                .unwrap_or(&current)
                .components()
                .filter_map(|part| match part {
                    Component::Normal(name) => Some(name.to_os_string()),
                    _ => None,
                })
                .collect();
            expanded.append(&mut pending);
            pending = expanded;
            current = root.to_path_buf();
        } else if !pending.is_empty() && !metadata.is_dir() {
            return Ok("missing file");
        } else if pending.is_empty() {
            return Ok(if metadata.is_file() { "" } else { "missing file" });
        }
    }
    Ok("missing file")
}

fn destinations(text: &str) -> Vec<String> {
    struct Collector(Vec<String>);
    impl crate::markdown::Hooks for Collector {
        fn link(&mut self, dest: &str) -> String {
            self.0.push(dest.to_owned());
            dest.to_owned()
        }
    }
    let mut collector = Collector(Vec::new());
    crate::markdown::render(text, &mut collector);
    collector.0
}

fn misses(root: &Path) -> Result<BTreeMap<(String, String), String>> {
    let root = root.canonicalize().context("resolving repository root")?;
    let denies = denied(&root)?;
    let mut output = BTreeMap::new();
    let docs = root.join("docs");
    walk(&root, &docs, &denies, &mut output)?;
    Ok(output)
}

fn walk(
    root: &Path,
    dir: &Path,
    denies: &[PathBuf],
    output: &mut BTreeMap<(String, String), String>,
) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if forbidden(root, &path, denies) {
            continue;
        }
        let metadata =
            std::fs::symlink_metadata(&path).with_context(|| format!("inspecting {}", path.display()))?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            walk(root, &path, denies, output)?;
            continue;
        }
        if path.extension().is_none_or(|ext| ext != "md") {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let source = std::fs::read_to_string(&path).with_context(|| format!("reading {rel}"))?;
        let parent = path.parent().unwrap_or(root);
        for target in destinations(&source) {
            if target.starts_with("http:")
                || target.starts_with("https:")
                || target.starts_with("mailto:")
                || target.starts_with('#')
                || target.contains("::")
            {
                continue;
            }
            let path_part = target.split('#').next().unwrap_or("");
            if path_part.is_empty() {
                continue;
            }
            let resolved = normalize(&parent.join(path_part));
            if forbidden(root, &resolved, denies) {
                output.insert((rel.clone(), target.clone()), "forbidden path".to_owned());
                continue;
            }
            if !resolved.starts_with(root) {
                output.insert((rel.clone(), target.clone()), "leaves repository".to_owned());
                continue;
            }
            let status = inspect_target(root, &resolved, denies)?;
            if !status.is_empty() {
                output.insert((rel.clone(), target.clone()), status.to_owned());
            }
        }
    }
    Ok(())
}

fn baseline(root: &Path) -> Result<BTreeSet<(String, String)>> {
    let text = std::fs::read_to_string(root.join(BASELINE)).with_context(|| format!("reading {BASELINE}"))?;
    let value: toml::Value = toml::from_str(&text).context("parsing docs links baseline")?;
    if value.get("format_version").and_then(toml::Value::as_integer) != Some(1) {
        anyhow::bail!("{BASELINE} must use format_version = 1");
    }
    let rows = value
        .get("miss")
        .and_then(toml::Value::as_array)
        .context("baseline has no [[miss]] rows")?;
    rows.iter()
        .map(|row| {
            let file = row
                .get("file")
                .and_then(toml::Value::as_str)
                .context("baseline row has no file")?;
            let target = row
                .get("target")
                .and_then(toml::Value::as_str)
                .context("baseline row has no target")?;
            row.get("reason")
                .and_then(toml::Value::as_str)
                .filter(|reason| !reason.trim().is_empty())
                .context("baseline row has no reason")?;
            Ok((file.to_owned(), target.to_owned()))
        })
        .collect()
}

/// Check every local Markdown link and image target in `docs/` against the baseline.
pub fn check(root: &Path) -> Result<Vec<String>> {
    let actual = misses(root)?;
    let expected = baseline(root)?;
    let mut errors = Vec::new();
    let actual_keys: BTreeSet<_> = actual.keys().cloned().collect();
    for (file, target) in actual_keys.difference(&expected) {
        let reason = actual
            .get(&(file.clone(), target.clone()))
            .map(String::as_str)
            .unwrap_or("missing file");
        errors.push(format!("{file}: {target}: {reason}"));
    }
    for (file, target) in expected.difference(&actual_keys) {
        errors.push(format!("stale baseline row: {file}: {target}"));
    }
    Ok(errors)
}

/// Run `docs-links` and print its result.
pub fn run(root: &Path) -> Result<std::process::ExitCode> {
    let errors = check(root)?;
    if errors.is_empty() {
        println!("docs-links: ok");
        Ok(std::process::ExitCode::SUCCESS)
    } else {
        for error in &errors {
            eprintln!("error: {error}");
        }
        eprintln!("docs-links: {} problem(s)", errors.len());
        Ok(std::process::ExitCode::FAILURE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_test_utils::unique_temp_dir;

    fn fixture() -> Result<PathBuf> {
        let root = unique_temp_dir("docs-links");
        std::fs::create_dir_all(root.join("docs"))?;
        std::fs::create_dir_all(root.join("docs/ci"))?;
        std::fs::write(root.join("CLEANROOM-DENY.txt"), "denied/\n")?;
        std::fs::write(root.join(BASELINE), "format_version = 1\nmiss = []\n")?;
        Ok(root)
    }

    #[test]
    fn checks_relative_markdown_and_image_targets() -> Result<()> {
        let root = fixture()?;
        std::fs::write(root.join("docs/sibling.md"), "ok")?;
        std::fs::write(
            root.join("docs/page.md"),
            "[sibling](sibling.md) [missing](missing.md) ![image](pic.png) [web](https://example.test) [anchor](#part/anchor) Arc<winit::Window>",
        )?;
        let found = misses(&root)?;
        assert!(
            found
                .iter()
                .any(|((_, target), reason)| target == "missing.md" && reason == "missing file")
        );
        assert!(
            found
                .iter()
                .any(|((_, target), reason)| target == "pic.png" && reason == "missing file")
        );
        std::fs::write(
            root.join("docs/page.md"),
            "[sibling](sibling.md) [web](https://example.test)\n\n```md\n[fake](codeghost.md)\n```\nArc<winit::Window>",
        )?;
        assert!(misses(&root)?.is_empty());
        Ok(())
    }

    #[test]
    fn rejects_leaving_and_forbidden_paths_without_reading_them() -> Result<()> {
        let root = fixture()?;
        std::fs::create_dir_all(root.join("denied"))?;
        std::fs::write(root.join("denied/secret.md"), "not read")?;
        std::fs::write(
            root.join("docs/page.md"),
            "[outside](../../outside.md) [denied](../denied/secret.md)",
        )?;
        let found = misses(&root)?;
        assert!(found.iter().any(|(_, reason)| reason == "leaves repository"));
        assert!(found.iter().any(|(_, reason)| reason == "forbidden path"));
        Ok(())
    }

    #[test]
    fn baseline_rejects_new_and_stale_misses() -> Result<()> {
        let root = fixture()?;
        std::fs::write(root.join("docs/page.md"), "[broken](gone.md)")?;
        let errors = check(&root)?;
        assert_eq!(errors, ["docs/page.md: gone.md: missing file"]);

        std::fs::write(
            root.join(BASELINE),
            "format_version = 1\n\n[[miss]]\nfile = \"docs/page.md\"\ntarget = \"gone.md\"\nreason = \"missing file\"\n",
        )?;
        assert!(check(&root)?.is_empty());

        std::fs::write(root.join("docs/gone.md"), "restored")?;
        assert_eq!(check(&root)?, ["stale baseline row: docs/page.md: gone.md"]);
        Ok(())
    }
}
