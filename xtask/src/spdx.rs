// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask spdx [--check | --write]`: the SPDX licence line at the top of every first-party
//! `.rs` and `.wgsl` file (ADR-10225).
//!
//! Every file in scope starts with exactly one line `// SPDX-License-Identifier: <id>` (WGSL uses the
//! same comment). The id is:
//!
//! - the upstream licence of a file listed under `files_to` of a `[[source]]` row in
//!   `docs/ported-sources.toml` (ported code keeps its own licence; several sources are joined with
//!   ` AND `). The ported file keeps its attribution header below the line;
//! - `MIT-0` under `samples/` and `crates/regia_templates/templates/` (sample and starter-template code
//!   anyone may copy without conditions);
//! - `MPL-2.0` everywhere else in scope (`crates/`, `xtask/`, `extensions/`, `fuzz/`, `examples/`).
//!
//! Never in scope: the repository-root `vendor/` (third-party code under its own licence — currently
//! `kira` and `lewton`), `target/`, hidden directories and test inputs under `tests/**/fixtures/`
//! ([`is_test_fixture`]). A `vendor` directory nested under an engine root (for example Regia's own
//! `crates/regia_render/src/upscaler/vendor/`) is first-party code and stays in scope. The walk reads the file system, not git, so it works in a tree without `.git` (the
//! merge gate's). `--check` (the default) lists every file whose first line is missing or wrong and
//! exits 1; `--write` inserts or corrects the line and changes nothing else in the file (a wrong
//! SPDX line in the first position is replaced; the line ending of the file is kept).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The comment prefix of the line this check owns.
pub const PREFIX: &str = "// SPDX-License-Identifier: ";

/// The licence of Regia Engine's own code.
pub const ENGINE_LICENCE: &str = "MPL-2.0";

/// The licence of sample and starter-template code.
pub const SAMPLE_LICENCE: &str = "MIT-0";

/// Directories (relative to the workspace root) whose files are engine code.
pub const ENGINE_ROOTS: &[&str] = &["crates", "xtask", "extensions", "fuzz", "examples"];

/// Directories (relative to the workspace root) whose files are sample or template code. They win
/// over [`ENGINE_ROOTS`] (the templates live inside `crates/regia_templates`).
pub const SAMPLE_ROOTS: &[&str] = &["samples", "crates/regia_templates/templates"];

/// Directory names in-scope paths never fall under, at any depth (build output and third-party
/// JavaScript). The repository-root `vendor/` is exempt separately in [`expected_id`] because it is
/// not under an engine root; a `vendor` directory nested under an engine root is first-party and
/// stays in scope (see the module docs).
const SKIP_DIRS: &[&str] = &["target", "node_modules"];

/// File extensions in scope.
const EXTENSIONS: &[&str] = &["rs", "wgsl"];

/// One file whose first line is not the expected SPDX line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// Workspace-relative path, `/`-separated.
    pub path: String,
    /// The id the file must carry.
    pub expected: String,
    /// The id its first line carries now, when it has an SPDX line there.
    pub found: Option<String>,
}

impl std::fmt::Display for Finding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.found {
            Some(found) => write!(
                f,
                "{}: SPDX id `{found}`, expected `{}`",
                self.path, self.expected
            ),
            None => write!(f, "{}: no `{PREFIX}{}` first line", self.path, self.expected),
        }
    }
}

/// The SPDX id of each ported file: `files_to` of every `[[source]]` row, joined with ` AND ` when
/// several sources share a file (each id once, in row order).
pub fn ported_ids(ported: &[crate::notices::PortedSource]) -> BTreeMap<String, String> {
    let mut ids: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for source in ported {
        let licence = source.licence.trim();
        for to in &source.files_to {
            let list = ids.entry(to.trim_start_matches("./").to_owned()).or_default();
            if !list.iter().any(|l| l == licence) {
                list.push(licence.to_owned());
            }
        }
    }
    ids.into_iter()
        .map(|(path, list)| {
            let id = if list.len() == 1 {
                list.concat()
            } else {
                list.iter()
                    .map(|l| {
                        if l.contains(" OR ") {
                            format!("({l})")
                        } else {
                            l.clone()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(" AND ")
            };
            (path, id)
        })
        .collect()
}

/// The id a workspace-relative path must carry, or `None` when it is out of scope.
pub fn expected_id(rel: &str, ported: &BTreeMap<String, String>) -> Option<String> {
    let ext = rel.rsplit_once('.').map(|(_, e)| e)?;
    if !EXTENSIONS.contains(&ext) {
        return None;
    }
    // Only the repository-root `vendor/` tree is third-party and out of scope. A `vendor` directory
    // nested under an engine root (Regia's own `crates/regia_render/src/upscaler/vendor/`) is
    // first-party code and must carry its SPDX line.
    if rel.split('/').next() == Some("vendor") {
        return None;
    }
    let under = |root: &str| rel.strip_prefix(root).is_some_and(|rest| rest.starts_with('/'));
    let in_sample = SAMPLE_ROOTS.iter().any(|r| under(r));
    if !in_sample && !ENGINE_ROOTS.iter().any(|r| under(r)) {
        return None;
    }
    if rel
        .split('/')
        .any(|part| SKIP_DIRS.contains(&part) || part.starts_with('.'))
    {
        return None;
    }
    if is_test_fixture(rel) {
        return None;
    }
    if let Some(id) = ported.get(rel) {
        return Some(id.clone());
    }
    Some(if in_sample { SAMPLE_LICENCE } else { ENGINE_LICENCE }.to_owned())
}

/// Whether `rel` is test input data: a file under a `fixtures` directory inside a `tests`
/// directory (macro-expansion inputs, scanner and API-snapshot fixture crates). Such files are read
/// by tests that pin their line numbers and text, they never ship, and they are out of scope.
pub fn is_test_fixture(rel: &str) -> bool {
    let parts: Vec<&str> = rel.split('/').collect();
    parts
        .iter()
        .position(|part| *part == "tests")
        .is_some_and(|at| parts[at + 1..].contains(&"fixtures"))
}

/// The id on the first line of `text`, when that line is an SPDX line (a leading byte-order mark is
/// skipped).
pub fn first_line_id(text: &str) -> Option<&str> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let line = text.lines().next()?;
    line.strip_prefix(PREFIX).map(str::trim)
}

/// `text` with `// SPDX-License-Identifier: <id>` as its first line: an SPDX first line is replaced,
/// otherwise the line is inserted. Keeps a byte-order mark first and uses `\r\n` when the file does.
pub fn with_header(text: &str, id: &str) -> String {
    let (bom, body) = match text.strip_prefix('\u{feff}') {
        Some(rest) => ("\u{feff}", rest),
        None => ("", text),
    };
    let eol = if body.contains("\r\n") { "\r\n" } else { "\n" };
    let rest = if body.starts_with(PREFIX) {
        body.split_once('\n').map_or("", |(_, rest)| rest)
    } else {
        body
    };
    format!("{bom}{PREFIX}{id}{eol}{rest}")
}

/// Every in-scope file under `root`, workspace-relative and sorted.
pub fn files(root: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut roots: Vec<&str> = ENGINE_ROOTS.to_vec();
    roots.extend(SAMPLE_ROOTS.iter().filter(|r| !r.starts_with("crates/")));
    for top in roots {
        let dir = root.join(top);
        if dir.is_dir() {
            walk(&dir, top, &mut out)?;
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

fn walk(dir: &Path, rel: &str, out: &mut Vec<String>) -> Result<()> {
    let entries = std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("reading {}", dir.display()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let kind = entry.file_type()?;
        let child = format!("{rel}/{name}");
        if kind.is_dir() {
            if SKIP_DIRS.contains(&name.as_str()) || name.starts_with('.') {
                continue;
            }
            walk(&entry.path(), &child, out)?;
        } else if kind.is_file()
            && name
                .rsplit_once('.')
                .is_some_and(|(_, ext)| EXTENSIONS.contains(&ext))
        {
            out.push(child);
        }
    }
    Ok(())
}

/// Checks (or with `write`, fixes) every in-scope file under `root`. Returns the findings: with
/// `write`, the files it changed.
pub fn run(root: &Path, ported: &BTreeMap<String, String>, write: bool) -> Result<Vec<Finding>> {
    let mut findings = Vec::new();
    for rel in files(root)? {
        let Some(expected) = expected_id(&rel, ported) else {
            continue;
        };
        let path: PathBuf = root.join(&rel);
        let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let found = first_line_id(&text).map(str::to_owned);
        if found.as_deref() == Some(expected.as_str()) {
            continue;
        }
        if write {
            let fixed = with_header(&text, &expected);
            let tmp = path.with_extension(format!(
                "{}.spdx-tmp",
                path.extension().and_then(|e| e.to_str()).unwrap_or("")
            ));
            std::fs::write(&tmp, fixed).with_context(|| format!("writing {}", tmp.display()))?;
            std::fs::rename(&tmp, &path).with_context(|| format!("replacing {}", path.display()))?;
        }
        findings.push(Finding {
            path: rel,
            expected,
            found,
        });
    }
    Ok(findings)
}

/// Ported paths that name an in-scope file which does not exist (a stale `files_to` row would
/// otherwise silently give the default id).
pub fn missing_ported(root: &Path, ported: &BTreeMap<String, String>) -> Vec<String> {
    ported
        .keys()
        .filter(|rel| expected_id(rel, ported).is_some() && !root.join(rel.as_str()).is_file())
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ported() -> BTreeMap<String, String> {
        [(
            "crates/regia_render/shaders/sgsr.wgsl".to_owned(),
            "BSD-3-Clause".to_owned(),
        )]
        .into()
    }

    #[test]
    fn scope_and_ids() {
        let p = ported();
        assert_eq!(
            expected_id("crates/regia_core/src/lib.rs", &p).as_deref(),
            Some("MPL-2.0")
        );
        assert_eq!(expected_id("xtask/src/main.rs", &p).as_deref(), Some("MPL-2.0"));
        assert_eq!(
            expected_id("crates/regia_render/shaders/pbr.wgsl", &p).as_deref(),
            Some("MPL-2.0")
        );
        assert_eq!(
            expected_id("crates/regia_render/shaders/sgsr.wgsl", &p).as_deref(),
            Some("BSD-3-Clause")
        );
        assert_eq!(
            expected_id("samples/arena/src/main.rs", &p).as_deref(),
            Some("MIT-0")
        );
        assert_eq!(
            expected_id("crates/regia_templates/templates/blank/game/src/lib.rs", &p).as_deref(),
            Some("MIT-0")
        );
        // Out of scope: vendor, other roots, other extensions, build output, hidden dirs.
        assert_eq!(expected_id("vendor/lewton/src/lib.rs", &p), None);
        // A `vendor` directory nested under an engine root is first-party Regia code (the
        // repository-root `vendor/` is the only exempt tree): it must be reported, never skipped.
        assert_eq!(
            expected_id("crates/regia_x/vendor/y.rs", &p).as_deref(),
            Some("MPL-2.0")
        );
        assert_eq!(
            expected_id("crates/x/src/vendor/foo.rs", &p).as_deref(),
            Some("MPL-2.0")
        );
        assert_eq!(expected_id("crates/regia_x/target/debug/build/out.rs", &p), None);
        assert_eq!(expected_id("crates/.hidden/a.rs", &p), None);
        assert_eq!(expected_id("spikes/hot_reload/src/main.rs", &p), None);
        assert_eq!(expected_id("crates/regia_core/Cargo.toml", &p), None);
        assert_eq!(expected_id("cratesx/a.rs", &p), None);
        // Test inputs under tests/**/fixtures/ (their tests pin line numbers); a crate's own
        // `src/.../fixtures` module is compiled code and stays in scope.
        assert_eq!(
            expected_id("crates/regia_macros/tests/fixtures/script_valid.rs", &p),
            None
        );
        assert_eq!(
            expected_id("xtask/tests/api_snapshot/fixtures/owners/lib.rs", &p),
            None
        );
        assert_eq!(
            expected_id("xtask/tests/fixtures/det_scan_external/planted/src/lib.rs", &p),
            None
        );
        assert_eq!(
            expected_id("crates/regia_world_geom/src/compat/fixtures/onfoot.rs", &p).as_deref(),
            Some("MPL-2.0")
        );
        assert_eq!(
            expected_id("crates/regia_cli/tests/package_e2e.rs", &p).as_deref(),
            Some("MPL-2.0")
        );
    }

    #[test]
    fn ported_rows_join_licences_once() {
        let row = |licence: &str, to: &[&str]| crate::notices::PortedSource {
            project: "p".into(),
            url: "https://example.invalid/p".into(),
            licence: licence.into(),
            copyright: "c".into(),
            commit: "0".into(),
            files_from: vec!["a".into()],
            files_to: to.iter().map(|s| (*s).to_owned()).collect(),
        };
        let ids = ported_ids(&[
            row("MIT", &["crates/a/src/x.rs", "crates/a/src/y.rs"]),
            row("MIT OR Apache-2.0", &["crates/a/src/y.rs"]),
            row("MIT", &["crates/a/src/x.rs"]),
        ]);
        assert_eq!(ids["crates/a/src/x.rs"], "MIT");
        assert_eq!(ids["crates/a/src/y.rs"], "MIT AND (MIT OR Apache-2.0)");
    }

    #[test]
    fn header_is_inserted_or_replaced_and_nothing_else_changes() {
        let plain = "//! Docs.\nfn a() {}\n";
        let fixed = with_header(plain, "MPL-2.0");
        assert_eq!(
            fixed,
            "// SPDX-License-Identifier: MPL-2.0\n//! Docs.\nfn a() {}\n"
        );
        assert_eq!(first_line_id(&fixed), Some("MPL-2.0"));
        // Idempotent, and a wrong id is replaced in place.
        assert_eq!(with_header(&fixed, "MPL-2.0"), fixed);
        assert_eq!(
            with_header(&fixed, "MIT-0"),
            "// SPDX-License-Identifier: MIT-0\n//! Docs.\nfn a() {}\n"
        );
        // CRLF files stay CRLF; a BOM stays first; an empty file gets just the line.
        assert_eq!(
            with_header("a\r\nb\r\n", "MIT-0"),
            "// SPDX-License-Identifier: MIT-0\r\na\r\nb\r\n"
        );
        assert_eq!(
            with_header("\u{feff}x\n", "MPL-2.0"),
            "\u{feff}// SPDX-License-Identifier: MPL-2.0\nx\n"
        );
        assert_eq!(
            with_header("", "MPL-2.0"),
            "// SPDX-License-Identifier: MPL-2.0\n"
        );
        // An indented SPDX line inside a quoted upstream header is not the first-line marker.
        assert_eq!(
            first_line_id("//   SPDX-License-Identifier: BSD-3-Clause\n"),
            None
        );
    }

    #[test]
    fn run_checks_then_writes_then_passes() -> Result<()> {
        let root = regia_test_utils::unique_temp_dir("xtask_spdx");
        let w = |rel: &str, text: &str| -> Result<()> {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().context("parent")?)?;
            std::fs::write(path, text)?;
            Ok(())
        };
        w("crates/regia_a/src/lib.rs", "//! A.\n")?;
        w(
            "crates/regia_a/shaders/s.wgsl",
            "// SPDX-License-Identifier: MIT\nfn f() {}\n",
        )?;
        w(
            "crates/regia_render/shaders/sgsr.wgsl",
            "// Portions ported from ...\n",
        )?;
        w("samples/demo/src/main.rs", "fn main() {}\n")?;
        w(
            "crates/regia_a/src/vendor/b.rs",
            "//! B, Regia's own nested vendor code.\n",
        )?;
        w("crates/regia_a/target/gen.rs", "x\n")?;
        w("vendor/v/src/lib.rs", "x\n")?;
        w("crates/regia_a/README.md", "x\n")?;
        let p = ported();
        let found = run(&root, &p, false)?;
        let paths: Vec<&str> = found.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "crates/regia_a/shaders/s.wgsl",
                "crates/regia_a/src/lib.rs",
                "crates/regia_a/src/vendor/b.rs",
                "crates/regia_render/shaders/sgsr.wgsl",
                "samples/demo/src/main.rs"
            ]
        );
        assert_eq!(found[0].found.as_deref(), Some("MIT"));
        assert_eq!(
            std::fs::read_to_string(root.join("crates/regia_a/src/lib.rs"))?,
            "//! A.\n"
        );
        assert_eq!(run(&root, &p, true)?.len(), 5);
        assert!(run(&root, &p, false)?.is_empty());
        // Nested-vendor Regia code got its header on `--write`; the repository-root vendor tree did not.
        assert_eq!(
            std::fs::read_to_string(root.join("crates/regia_a/src/vendor/b.rs"))?,
            "// SPDX-License-Identifier: MPL-2.0\n//! B, Regia's own nested vendor code.\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("crates/regia_render/shaders/sgsr.wgsl"))?,
            "// SPDX-License-Identifier: BSD-3-Clause\n// Portions ported from ...\n"
        );
        assert_eq!(std::fs::read_to_string(root.join("vendor/v/src/lib.rs"))?, "x\n");
        assert_eq!(
            std::fs::read_to_string(root.join("crates/regia_a/target/gen.rs"))?,
            "x\n"
        );
        assert!(missing_ported(&root, &p).is_empty());
        std::fs::remove_file(root.join("crates/regia_render/shaders/sgsr.wgsl"))?;
        assert_eq!(
            missing_ported(&root, &p),
            ["crates/regia_render/shaders/sgsr.wgsl"]
        );
        std::fs::remove_dir_all(&root)?;
        Ok(())
    }

    /// The repository itself: every in-scope file carries its line (`cargo xtask spdx --write` fixes it).
    #[test]
    fn the_repository_has_every_spdx_line() -> Result<()> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let ported = crate::notices::load_ported(&root)?;
        let ids = ported_ids(&ported);
        assert_eq!(missing_ported(&root, &ids), Vec::<String>::new());
        let findings: Vec<String> = run(&root, &ids, false)?.iter().map(ToString::to_string).collect();
        assert!(
            findings.is_empty(),
            "{} file(s) lack their SPDX line (run `cargo xtask spdx --write`):\n{}",
            findings.len(),
            findings.iter().take(20).cloned().collect::<Vec<_>>().join("\n")
        );
        Ok(())
    }
}
