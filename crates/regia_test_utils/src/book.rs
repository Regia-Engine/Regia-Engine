// SPDX-License-Identifier: MPL-2.0
//! Book-coverage helpers for the runtime side of spec 96 (`docs/specs/96-guides-book.md`).
//!
//! `cargo xtask book-coverage` finds host extension methods and CLI commands by scanning the
//! source. Each crate that registers some also has a test that installs its real methods (or runs
//! its real binary) and checks them against `book/COVERAGE.md` with these helpers, so a method the
//! scan misses still fails a test.

use std::path::{Path, PathBuf};

/// The workspace root (two levels above this crate's manifest).
#[must_use]
pub fn workspace_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(Path::parent)
        .map_or(manifest.clone(), Path::to_path_buf)
}

/// One row of the extension-method table (§ 5 of `book/COVERAGE.md`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionRow {
    /// The method (`editor.select`).
    pub method: String,
    /// The `Registered by` cell, verbatim.
    pub registered_by: String,
    /// The first agent-page link target, relative to `book/` (fragment kept).
    pub page: String,
}

/// Splits a table row into trimmed cells. An escaped `\|` stays inside its cell (as `|`), the way
/// `cargo xtask book-coverage` and markdown read it (spec 96 § 8).
fn cells(line: &str) -> Vec<String> {
    let trimmed = line.trim();
    let inner = trimmed.strip_prefix('|').unwrap_or(trimmed);
    let inner = inner.strip_suffix('|').unwrap_or(inner);
    let mut out = Vec::new();
    let mut cell = String::new();
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                cell.push('|');
                chars.next();
            }
            '|' => out.push(std::mem::take(&mut cell).trim().to_string()),
            _ => cell.push(c),
        }
    }
    out.push(cell.trim().to_string());
    out
}

/// The data rows (as cells) of every table whose header cells are exactly `header`, in order (a
/// section may continue its table after a paragraph by repeating the header), as
/// `cargo xtask book-coverage` reads them.
fn table(text: &str, header: &[&str]) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut in_table = false;
    for line in text.lines() {
        if !line.trim_start().starts_with('|') {
            in_table = false;
            continue;
        }
        let row = cells(line);
        if row.iter().map(String::as_str).eq(header.iter().copied()) {
            in_table = true;
            continue;
        }
        let separator = row
            .iter()
            .all(|c| !c.is_empty() && c.chars().all(|ch| matches!(ch, '-' | ':')));
        if in_table && !separator {
            rows.push(row);
        }
    }
    rows
}

fn first_code(cell: &str) -> Option<&str> {
    cell.split('`').nth(1).map(str::trim).filter(|s| !s.is_empty())
}

fn first_link(cell: &str) -> Option<&str> {
    let at = cell.find("](")?;
    let rest = &cell[at + 2..];
    rest.find(')').map(|end| rest[..end].trim())
}

/// The rows of the extension-method table in `coverage_md` (`book/COVERAGE.md`).
#[must_use]
pub fn extension_rows(coverage_md: &str) -> Vec<ExtensionRow> {
    table(coverage_md, &["Extension method", "Registered by", "Agent page"])
        .into_iter()
        .filter_map(|row| {
            Some(ExtensionRow {
                method: first_code(row.first()?)?.to_string(),
                registered_by: row.get(1)?.clone(),
                page: first_link(row.get(2)?)?.to_string(),
            })
        })
        .collect()
}

/// Every way the book fails to document `names`, the methods `krate` registers: no row in the
/// extension table, a row that does not name `krate`, a page that does not exist, or a page that
/// does not name the method (`mentions(page_text, method)`, pass `regia_agent::mentions_token`).
/// Empty when every method is documented.
#[must_use]
pub fn extension_gaps(
    root: &Path,
    names: &[&str],
    krate: &str,
    mentions: &dyn Fn(&str, &str) -> bool,
) -> Vec<String> {
    let coverage = match std::fs::read_to_string(root.join("book/COVERAGE.md")) {
        Ok(text) => text,
        Err(err) => return vec![format!("cannot read book/COVERAGE.md: {err}")],
    };
    let rows = extension_rows(&coverage);
    let registrar = format!("`{krate}`");
    let mut gaps = Vec::new();
    for name in names {
        let own: Vec<&ExtensionRow> = rows.iter().filter(|row| row.method == *name).collect();
        if own.is_empty() {
            gaps.push(format!(
                "`{name}` ({krate}) has no row in the book/COVERAGE.md extension table"
            ));
        }
        for row in own {
            if !row.registered_by.contains(&registrar) {
                gaps.push(format!(
                    "`{name}`: its row says it is registered by {}, not {registrar}",
                    row.registered_by
                ));
            }
            let page = row.page.split('#').next().unwrap_or_default();
            match std::fs::read_to_string(root.join("book").join(page)) {
                Ok(text) if mentions(&text, name) => {}
                Ok(_) => gaps.push(format!("`{name}` maps to book/{page}, which does not mention it")),
                Err(_) => gaps.push(format!("`{name}` maps to book/{page}, which does not exist")),
            }
        }
    }
    gaps
}

/// Every top-level `regia` subcommand in `commands` that has no row in the CLI table of
/// `coverage_md` (a command cell starting with `` `regia <name>``).
#[must_use]
pub fn cli_gaps(coverage_md: &str, commands: &[&str]) -> Vec<String> {
    let documented: Vec<String> = table(coverage_md, &["Command", "What it does", "Page"])
        .iter()
        .filter_map(|row| first_code(row.first()?))
        .filter_map(|code| code.strip_prefix("regia "))
        .map(|rest| {
            rest.chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                .collect()
        })
        .collect();
    commands
        .iter()
        .filter(|command| !documented.iter().any(|d| d == *command))
        .map(|command| format!("`regia {command}` has no row in the book/COVERAGE.md CLI table"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const COVERAGE: &str = "\
## 3. CLI

| Command | What it does | Page |
|---|---|---|
| `regia cook [--x]` | Cook. | [a](a.md) |
| `regia-editor` | Editor. | [e](e.md) |

## 5. Extension methods

| Extension method | Registered by | Agent page |
|---|---|---|
| `editor.select` | `regia_editor` (`agent_ext::install`) | [editor](agents/editor.md#select) |
| `steam.status` | `regia_steam` | [steam](agents/steam.md) |
";

    #[test]
    fn parses_extension_rows() {
        let rows = extension_rows(COVERAGE);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].method, "editor.select");
        assert!(rows[0].registered_by.contains("`regia_editor`"));
        assert_eq!(rows[0].page, "agents/editor.md#select");
        assert!(extension_rows("no table").is_empty());
    }

    /// An escaped pipe inside a cell (`a\|b`) does not split the row, so the page link is still the third cell
    /// (ADR-5660: the xtask scan already read rows this way; this helper used to split there and drop the row).
    #[test]
    fn escaped_pipes_stay_inside_their_cell() {
        let text = "| Extension method | Registered by | Agent page |\n|---|---|---|\n\
                    | `editor.mode` | `regia_editor` (`a\\|b`) | [editor](agents/editor.md) |\n";
        let rows = extension_rows(text);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].registered_by, "`regia_editor` (`a|b`)");
        assert_eq!(rows[0].page, "agents/editor.md");
        let cli = "| Command | What it does | Page |\n|---|---|---|\n| `regia new [blank\\|fps]` | New. | [a](a.md) |\n";
        assert!(cli_gaps(cli, &["new"]).is_empty());
    }

    #[test]
    fn cli_gaps_name_undocumented_commands() {
        assert_eq!(
            cli_gaps(COVERAGE, &["cook", "run"]),
            ["`regia run` has no row in the book/COVERAGE.md CLI table"]
        );
        assert!(cli_gaps(COVERAGE, &["cook"]).is_empty());
        // A table continued after a paragraph by repeating its header counts; an orphan row does not.
        let continued = "| Command | What it does | Page |\n|---|---|---|\n| `regia cook` | C. | [a](a.md) |\n\nText.\n| `regia orphan` | O. | [a](a.md) |\n\n| Command | What it does | Page |\n|---|---|---|\n| `regia run` | R. | [a](a.md) |\n";
        assert!(cli_gaps(continued, &["cook", "run"]).is_empty());
        assert_eq!(cli_gaps(continued, &["orphan"]).len(), 1);
    }

    #[test]
    fn extension_gaps_check_rows_registrars_and_pages() {
        let root = crate::unique_temp_dir("book-ext-gaps");
        let write = |rel: &str, text: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            std::fs::write(path, text).expect("write");
        };
        write("book/COVERAGE.md", COVERAGE);
        write("book/agents/editor.md", "# Editor\n\n`editor.select`\n");
        let mentions = |text: &str, name: &str| text.contains(name);
        assert!(extension_gaps(&root, &["editor.select"], "regia_editor", &mentions).is_empty());
        let gaps = extension_gaps(
            &root,
            &["editor.select", "editor.missing"],
            "regia_steam",
            &mentions,
        );
        assert_eq!(gaps.len(), 2, "{gaps:?}");
        let steam = extension_gaps(&root, &["steam.status"], "regia_steam", &mentions);
        assert_eq!(steam.len(), 1, "the steam page does not exist: {steam:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_workspace_root_holds_the_book() {
        assert!(workspace_root().join("book/COVERAGE.md").is_file());
    }
}
