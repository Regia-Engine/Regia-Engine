// SPDX-License-Identifier: MPL-2.0
//! A project's engine pin: the engine dependency lines of its `Cargo.toml` files and the
//! `Regia.toml [project] engine` line (spec 94 § 3.2).
//!
//! The rewrite is line based so a manifest keeps its comments, order and formatting: only the
//! source keys of an engine dependency's one-line inline table change. A dependency written as a
//! `[dependencies.regia_x]` table, or as an inline table over several lines, is refused with
//! `engine_dep_layout` and its line, for the developer to edit by hand.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use regia_core::engine_version::{EngineRequirement, ReleaseLine};

use super::{DistError, EngineSource, is_absolute_text, is_engine_tree, slash_path};

/// The keys of a dependency that name where it comes from; a rewrite replaces exactly these.
const SOURCE_KEYS: &[&str] = &["path", "git", "tag", "branch", "rev", "version", "registry"];

/// Where one engine dependency line points.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DepSource {
    /// `path = "…"`, as written (relative to the manifest's directory unless absolute).
    Path(String),
    /// `git = "…"` with its `tag`, `branch` or `rev` (empty when none is given).
    Git {
        /// Repository URL.
        url: String,
        /// The `tag`, `branch` or `rev` value.
        reference: String,
    },
    /// `version = "…"` from a registry (`None` = the default registry).
    Registry {
        /// The `registry` key.
        registry: Option<String>,
        /// The version requirement as written.
        version: String,
    },
    /// `workspace = true`: the source is the workspace table's line (also a site when the
    /// workspace manifest is in the project).
    Workspace,
}

/// One engine dependency line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DepSite {
    /// The manifest, relative to the project root, with `/` separators.
    pub file: String,
    /// 1-based line number.
    pub line: usize,
    /// The dependency key (`regia`, `regia_dylib`, …).
    pub krate: String,
    /// Where it points.
    pub source: DepSource,
}

impl DepSite {
    /// The engine source this line names, in `EngineSource` text form (`path:` + the engine root
    /// relative to the project when it is inside it), or `None` for a workspace reference.
    pub fn engine_source_text(&self, project: &Path) -> Option<String> {
        match &self.source {
            DepSource::Path(path) => {
                let root = strip_crate_suffix(path, &self.krate);
                if is_absolute_text(&root) {
                    return Some(format!("path:{root}"));
                }
                let manifest_dir = Path::new(&self.file).parent().unwrap_or(Path::new(""));
                let joined = normalize(&manifest_dir.join(&root));
                if joined.starts_with("..") {
                    // Outside the project: show it absolute.
                    let abs = normalize(&project.join(&joined));
                    Some(format!("path:{}", slash_path(&abs)))
                } else {
                    Some(format!("path:{}", slash_path(&joined)))
                }
            }
            DepSource::Git { url, reference } => Some(format!("git:{url}#{reference}")),
            DepSource::Registry { registry, version } => Some(format!(
                "registry:{}@{}",
                registry.as_deref().unwrap_or("crates-io"),
                version.trim_start_matches('=')
            )),
            DepSource::Workspace => None,
        }
    }
}

/// Every engine dependency line of a project.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EnginePin {
    /// The lines, by file then line.
    pub sites: Vec<DepSite>,
}

impl EnginePin {
    /// The distinct engine sources the sites name, sorted (more than one is `engine_sources_mixed`).
    pub fn sources(&self, project: &Path) -> Vec<String> {
        let mut out: Vec<String> = self
            .sites
            .iter()
            .filter_map(|site| site.engine_source_text(project))
            .collect();
        out.sort();
        out.dedup();
        out
    }
}

/// The new text of one manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinEdit {
    /// The manifest, relative to the project root, with `/` separators.
    pub file: String,
    /// Its full new text.
    pub text: String,
    /// How many dependency lines changed.
    pub changed_lines: usize,
}

/// `path` without a trailing `/crates/<krate>` (the engine root of a path dependency).
fn strip_crate_suffix(path: &str, krate: &str) -> String {
    let path = path.replace('\\', "/");
    let path = path.trim_end_matches('/');
    let suffix = format!("/crates/{krate}");
    match path.strip_suffix(&suffix) {
        Some(root) if !root.is_empty() => root.to_string(),
        Some(_) => "/".to_string(),
        None if path == format!("crates/{krate}") => ".".to_string(),
        None => path.to_string(),
    }
}

/// Lexically removes `.` and resolves `..` against earlier components (leading `..` are kept).
fn normalize(path: &Path) -> PathBuf {
    let mut out: Vec<Component<'_>> = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match out.last() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => out.push(component),
            },
            other => out.push(other),
        }
    }
    if out.is_empty() {
        PathBuf::from(".")
    } else {
        out.iter().collect()
    }
}

/// True for `regia` and `regia_*` dependency keys (the engine crates), unless the project defines
/// that package itself (`own`): a game may be called `regia_arena`, and its launcher depends on it.
fn is_engine_crate(key: &str, own: &BTreeSet<String>) -> bool {
    (key == "regia" || key.starts_with("regia_")) && !own.contains(key)
}

/// The packages the project's own manifests define.
fn own_packages(project: &Path, manifests: &[String]) -> BTreeSet<String> {
    let name = |file: &String| {
        let text = std::fs::read_to_string(project.join(file)).ok()?;
        let table: toml::Table = toml::from_str(&text).ok()?;
        Some(table.get("package")?.get("name")?.as_str()?.to_string())
    };
    manifests.iter().filter_map(name).collect()
}

/// The section a `[header]` opens, as far as engine dependencies care.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Section {
    /// A `*dependencies` table.
    Deps,
    /// Any other table.
    Other,
}

/// Splits a TOML table header into its dotted keys (quotes kept off; dots inside quotes kept).
fn header_keys(header: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for c in header.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => current.push(c),
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c == '.' => keys.push(std::mem::take(&mut current).trim().to_string()),
            None => current.push(c),
        }
    }
    keys.push(current.trim().to_string());
    keys
}

fn is_deps_key(key: &str) -> bool {
    matches!(key, "dependencies" | "dev-dependencies" | "build-dependencies")
}

/// Reads a header line (`[a.b]` or `[[a]]`). `Ok(None)` for a non-header line; `Err` for a
/// `[dependencies.regia_x]` table (an engine dependency in a layout the rewrite does not edit).
fn parse_header(trimmed: &str, own: &BTreeSet<String>) -> Result<Option<Section>, String> {
    if !trimmed.starts_with('[') {
        return Ok(None);
    }
    let inner = trimmed.trim_start_matches('[');
    let Some(end) = inner.find(']') else {
        return Ok(Some(Section::Other));
    };
    let keys = header_keys(&inner[..end]);
    if keys.last().is_some_and(|k| is_deps_key(k)) {
        return Ok(Some(Section::Deps));
    }
    if keys.len() >= 2
        && is_deps_key(&keys[keys.len() - 2])
        && keys.last().is_some_and(|k| is_engine_crate(k, own))
    {
        return Err(keys.last().cloned().unwrap_or_default());
    }
    Ok(Some(Section::Other))
}

/// Splits `text` at top-level occurrences of `sep` (outside strings, arrays and inline tables).
/// Stops at a `#` comment outside a string; returns the parts and the byte offset where the
/// comment starts (or `text.len()`).
fn split_top(text: &str, sep: char) -> (Vec<&str>, usize) {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut start = 0usize;
    for (i, c) in text.char_indices() {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if c == '\\' && q == '"' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => quote = Some(c),
            '[' | '{' => depth += 1,
            ']' | '}' => depth -= 1,
            '#' => {
                parts.push(&text[start..i]);
                return (parts, i);
            }
            c if c == sep && depth == 0 => {
                parts.push(&text[start..i]);
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(&text[start..]);
    (parts, text.len())
}

/// The unquoted key of `key = value` text.
fn unquote_key(key: &str) -> String {
    key.trim().trim_matches(|c| c == '"' || c == '\'').to_string()
}

/// A parsed engine dependency line.
struct DepLine {
    /// Everything up to and including the `=` (kept verbatim).
    head: String,
    /// The key, unquoted.
    krate: String,
    /// The inline table's entries other than the source keys, verbatim and in order (`None` for a
    /// plain version string).
    kept: Vec<String>,
    /// The source the line names.
    source: DepSource,
    /// A trailing comment (with its leading whitespace), kept verbatim.
    comment: String,
}

/// The string value of `key = "value"` entry text, or `None`.
fn string_value(value: &str) -> Option<String> {
    let parsed: toml::Table = toml::from_str(&format!("v = {}", value.trim())).ok()?;
    parsed.get("v")?.as_str().map(str::to_string)
}

/// Parses one line of a dependencies table. `Ok(None)`: not an engine dependency. `Err`: an engine
/// dependency whose layout the rewrite does not edit (`why`).
fn parse_dep_line(line: &str, own: &BTreeSet<String>) -> Result<Option<DepLine>, String> {
    let trimmed = line.trim_start();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return Ok(None);
    }
    let (parts, comment_at) = split_top(line, '=');
    if parts.len() < 2 {
        return Ok(None);
    }
    let key_text = parts[0];
    let key = unquote_key(key_text);
    // A dotted key (`regia.workspace = true`, `regia.path = "…"`).
    let (key, dotted) = match key.split_once('.') {
        Some((first, rest)) => (unquote_key(first), Some(rest.trim().to_string())),
        None => (key.clone(), None),
    };
    if !is_engine_crate(&key, own) {
        return Ok(None);
    }
    let eq = key_text.len();
    let value = line[eq + 1..comment_at].trim();
    let comment = line[comment_at..].to_string();
    let comment = if comment.is_empty() {
        String::new()
    } else {
        // Keep the spacing before the `#`.
        let before = &line[eq + 1..comment_at];
        let pad = &before[before.trim_end().len()..];
        format!("{pad}{comment}")
    };
    if let Some(dotted) = dotted {
        return if dotted == "workspace" {
            Ok(Some(DepLine {
                head: line[..=eq].to_string(),
                krate: key,
                kept: Vec::new(),
                source: DepSource::Workspace,
                comment,
            }))
        } else {
            Err(format!("`{key}` uses dotted keys"))
        };
    }
    let head = line[..=eq].to_string();
    if let Some(version) = string_value(value) {
        return Ok(Some(DepLine {
            head,
            krate: key,
            kept: Vec::new(),
            source: DepSource::Registry {
                registry: None,
                version,
            },
            comment,
        }));
    }
    if !value.starts_with('{') || !value.ends_with('}') {
        return Err(format!("`{key}` is not a one-line inline table"));
    }
    let body = &value[1..value.len() - 1];
    let (entries, _) = split_top(body, ',');
    let mut kept = Vec::new();
    let mut found: Vec<(String, String)> = Vec::new();
    let mut workspace = false;
    for entry in entries {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let Some((k, v)) = entry.split_once('=') else {
            return Err(format!("`{key}` has an entry without `=`"));
        };
        let k = unquote_key(k);
        if k == "workspace" {
            workspace = true;
            kept.push(entry.to_string());
        } else if SOURCE_KEYS.contains(&k.as_str()) {
            let Some(v) = string_value(v) else {
                return Err(format!("`{key}.{k}` is not a string"));
            };
            found.push((k, v));
        } else {
            kept.push(entry.to_string());
        }
    }
    let get = |name: &str| found.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
    let source = if workspace {
        DepSource::Workspace
    } else if let Some(path) = get("path") {
        DepSource::Path(path)
    } else if let Some(url) = get("git") {
        DepSource::Git {
            url,
            reference: get("tag")
                .or_else(|| get("branch"))
                .or_else(|| get("rev"))
                .unwrap_or_default(),
        }
    } else if let Some(version) = get("version") {
        DepSource::Registry {
            registry: get("registry"),
            version,
        }
    } else {
        return Err(format!("`{key}` names no source"));
    };
    Ok(Some(DepLine {
        head,
        krate: key,
        kept,
        source,
        comment,
    }))
}

/// The engine dependency lines of one manifest's text: `(line number, parsed line)`.
fn dep_lines(file: &str, text: &str, own: &BTreeSet<String>) -> Result<Vec<(usize, DepLine)>, DistError> {
    let layout = |line: usize, why: String| {
        DistError::new(
            "engine_dep_layout",
            format!(
                "{file}:{line}: {why}; write engine dependencies as one-line inline tables \
                 (`regia = {{ path = \"…\" }}`) or edit this line by hand"
            ),
        )
    };
    let mut section = Section::Other;
    let mut out = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let number = index + 1;
        match parse_header(line.trim(), own) {
            Ok(Some(next)) => {
                section = next;
                continue;
            }
            Ok(None) => {}
            Err(krate) => return Err(layout(number, format!("`{krate}` is a dependency table"))),
        }
        if section != Section::Deps {
            continue;
        }
        match parse_dep_line(line, own) {
            Ok(Some(dep)) => out.push((number, dep)),
            Ok(None) => {}
            Err(why) => return Err(layout(number, why)),
        }
    }
    Ok(out)
}

/// Every `Cargo.toml` of the project, relative and sorted, skipping `target/`, `.git*` and engine
/// trees (a vendored release or checkout is never rewritten).
fn manifests(project: &Path) -> Result<Vec<String>, DistError> {
    let mut out = Vec::new();
    let walker = walkdir::WalkDir::new(project).into_iter().filter_entry(|entry| {
        let name = entry.file_name().to_string_lossy();
        if entry.depth() == 0 {
            return true;
        }
        if entry.file_type().is_dir() {
            return !name.starts_with(".git") && name != "target" && !is_engine_tree(entry.path());
        }
        true
    });
    for entry in walker {
        let entry =
            entry.map_err(|err| DistError::new("io_error", format!("{}: {err}", project.display())))?;
        if entry.file_type().is_file() && entry.file_name() == "Cargo.toml" {
            let rel = entry.path().strip_prefix(project).unwrap_or(entry.path());
            out.push(slash_path(rel));
        }
    }
    out.sort();
    Ok(out)
}

/// Reads every engine dependency line of the project.
///
/// # Errors
///
/// `engine_dep_layout` for a line the rewrite cannot edit; `io_error` for an unreadable manifest.
pub fn read_pin(project: &Path) -> Result<EnginePin, DistError> {
    let mut sites = Vec::new();
    let files = manifests(project)?;
    let own = own_packages(project, &files);
    for file in files {
        let path = project.join(&file);
        let text = std::fs::read_to_string(&path).map_err(|err| DistError::io(&path, &err))?;
        for (line, dep) in dep_lines(&file, &text, &own)? {
            sites.push(DepSite {
                file: file.clone(),
                line,
                krate: dep.krate,
                source: dep.source,
            });
        }
    }
    Ok(EnginePin { sites })
}

/// The source entries for `krate` in a manifest at `file` (project-relative).
fn source_entries(source: &EngineSource, file: &str, krate: &str) -> Vec<String> {
    match source {
        EngineSource::Path(root) => {
            let root = slash_path(root);
            let root = root.trim_end_matches('/');
            let path = if is_absolute_text(root) {
                format!("{root}/crates/{krate}")
            } else {
                let depth = Path::new(file).parent().map_or(0, |dir| dir.components().count());
                let up = "../".repeat(depth);
                let rel = root.trim_start_matches("./");
                if rel == "." || rel.is_empty() {
                    format!("{up}crates/{krate}")
                } else {
                    format!("{up}{rel}/crates/{krate}")
                }
            };
            vec![format!("path = \"{path}\"")]
        }
        EngineSource::Git { url, tag } => vec![format!("git = \"{url}\""), format!("tag = \"{tag}\"")],
        EngineSource::Registry { name, version } => vec![
            format!("version = \"={version}\""),
            format!("registry = \"{name}\""),
        ],
    }
}

/// The new text of every manifest whose engine dependencies `source` changes (workspace
/// references are left alone). Line endings are kept; a manifest that would not change is not
/// listed.
///
/// # Errors
///
/// As [`read_pin`]; `engine_crate_missing` when a path source has no `crates/<krate>` for a
/// dependency (checked only when the directory exists).
pub fn rewrite_pin(project: &Path, source: &EngineSource) -> Result<Vec<PinEdit>, DistError> {
    if let EngineSource::Path(root) = source {
        let dir = if root.is_absolute() {
            root.clone()
        } else {
            project.join(root)
        };
        if dir.is_dir() {
            for site in read_pin(project)?.sites {
                if site.source != DepSource::Workspace
                    && !dir.join("crates").join(&site.krate).join("Cargo.toml").is_file()
                {
                    return Err(DistError::new(
                        "engine_crate_missing",
                        format!(
                            "{}:{}: `{}` is not in {} (crates/{}/Cargo.toml)",
                            site.file,
                            site.line,
                            site.krate,
                            dir.display(),
                            site.krate
                        ),
                    ));
                }
            }
        }
    }
    let mut edits = Vec::new();
    let files = manifests(project)?;
    let own = own_packages(project, &files);
    for file in files {
        let path = project.join(&file);
        let text = std::fs::read_to_string(&path).map_err(|err| DistError::io(&path, &err))?;
        let deps = dep_lines(&file, &text, &own)?;
        if deps.is_empty() {
            continue;
        }
        let mut lines: Vec<String> = text.split_inclusive('\n').map(str::to_string).collect();
        let mut changed = 0usize;
        for (number, dep) in deps {
            if dep.source == DepSource::Workspace {
                continue;
            }
            let mut entries = source_entries(source, &file, &dep.krate);
            entries.extend(dep.kept.iter().cloned());
            let ending = {
                let original = &lines[number - 1];
                &original[original.trim_end_matches(['\n', '\r']).len()..]
            }
            .to_string();
            let new_line = format!("{} {{ {} }}{}{ending}", dep.head, entries.join(", "), dep.comment);
            if lines[number - 1] != new_line {
                lines[number - 1] = new_line;
                changed += 1;
            }
        }
        if changed > 0 {
            edits.push(PinEdit {
                file,
                text: lines.concat(),
                changed_lines: changed,
            });
        }
    }
    Ok(edits)
}

/// Reads `Regia.toml [project] engine` (the legacy manifest name too, ADR-1388). `Ok(None)` when
/// the key is missing.
///
/// # Errors
///
/// `manifest_missing` without a manifest; `engine_requirement_invalid` for a value that is not a
/// requirement string; `io_error`.
pub fn read_requirement(project: &Path) -> Result<Option<EngineRequirement>, DistError> {
    let Some(path) = regia_core::brand::project_manifest(project) else {
        return Err(DistError::new(
            "manifest_missing",
            format!(
                "{} has no {}",
                project.display(),
                regia_core::brand::MANIFEST_FILE
            ),
        ));
    };
    let text = std::fs::read_to_string(&path).map_err(|err| DistError::io(&path, &err))?;
    let table: toml::Table = toml::from_str(&text)
        .map_err(|err| DistError::new("manifest_invalid", format!("{}: {err}", path.display())))?;
    let Some(value) = table.get("project").and_then(|p| p.get("engine")) else {
        return Ok(None);
    };
    let invalid = |why: String| {
        DistError::new(
            "engine_requirement_invalid",
            format!("{}: [project] engine {why}", path.display()),
        )
    };
    let Some(text) = value.as_str() else {
        return Err(invalid("is not a string".into()));
    };
    EngineRequirement::parse(text)
        .map(Some)
        .map_err(|err| invalid(err.to_string()))
}

/// `text` (a `Regia.toml`) with `[project] engine = "<line>"`: the existing key's line is
/// replaced (indentation and a trailing comment kept); without one, the key is added after the
/// table's last key. Every other line is untouched. `None` when the text has no `[project]` table.
pub fn stamp_engine_line(text: &str, line: ReleaseLine) -> Option<String> {
    // Parser spans locate real keys and complete multiline values, including quoted text
    // that resembles another table or an engine assignment. Never scan inside user strings.
    let document = toml::de::DeTable::parse(text).ok()?;
    let (project_key, project_value) = document
        .get_ref()
        .iter()
        .find(|(key, _)| key.get_ref() == "project")?;
    let project = project_value.get_ref().as_table()?;
    let key_start = project_key.span().start;
    let header_start = text[..key_start].rfind('\n').map_or(0, |i| i + 1);
    let header_end = text[key_start..]
        .find('\n')
        .map_or(text.len(), |i| key_start + i + 1);
    let header = text[header_start..header_end].trim();
    let keys = header_keys(header.strip_prefix('[')?.split(']').next()?);
    if header.starts_with("[[") || keys != ["project"] {
        return None;
    }
    let mut after = header_end;
    for (key, value) in project {
        let key_start = key.span().start;
        let line_start = text[..key_start].rfind('\n').map_or(0, |i| i + 1);
        // A nested table belongs to [project.child]; adding the pin there would be wrong.
        if text[line_start..key_start].trim_start().starts_with('[') {
            continue;
        }
        if key.get_ref() == "engine" {
            let mut out = text.to_string();
            out.replace_range(value.span(), &format!("\"{line}\""));
            return Some(out);
        }
        let end = value.span().end;
        let line_end = text[end..].find('\n').map_or(text.len(), |i| end + i + 1);
        after = after.max(line_end);
    }
    let ending = if text[header_start..header_end].ends_with("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let prefix = if text[..after].ends_with('\n') { "" } else { ending };
    let mut out = text.to_string();
    out.insert_str(after, &format!("{prefix}engine = \"{line}\"{ending}"));
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(files: &[(&str, &str)]) -> PathBuf {
        let dir = regia_test_utils::unique_temp_dir("dist_pin");
        for (rel, text) in files {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
            std::fs::write(path, text).expect("write");
        }
        dir
    }

    const GAME: &str = "[package]\nname = \"g\"\n\n[features]\nhot-reload = [\"dep:regia_dylib\"]\n\n\
[dependencies]\nregia = { path = \"/eng/old/crates/regia\" }\n\
regia_dylib = { path = \"/eng/old/crates/regia_dylib\", optional = true } # anchor\n\
serde = { version = \"1\", features = [\"derive\"] }\n\n\
[target.'cfg(target_os = \"android\")'.dependencies]\n\
regia_platform = { features = [\"a\", \"b\"], path = \"/eng/old/crates/regia_platform\", default-features = false }\n";

    #[test]
    fn reads_every_engine_dependency_line() {
        let dir = project(&[
            ("game/Cargo.toml", GAME),
            ("Cargo.toml", "[workspace]\nmembers = [\"game\"]\n"),
        ]);
        let pin = read_pin(&dir).expect("pin");
        let krates: Vec<_> = pin.sites.iter().map(|s| (s.krate.as_str(), s.line)).collect();
        assert_eq!(
            krates,
            vec![("regia", 8), ("regia_dylib", 9), ("regia_platform", 13)]
        );
        assert_eq!(
            pin.sites[0].source,
            DepSource::Path("/eng/old/crates/regia".into())
        );
        assert_eq!(pin.sources(&dir), vec!["path:/eng/old".to_string()]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rewrite_changes_only_source_keys_and_is_idempotent() {
        let dir = project(&[("game/Cargo.toml", GAME)]);
        let source = EngineSource::Git {
            url: "https://example.com/regia.git".into(),
            tag: "v0.2.5".into(),
        };
        let edits = rewrite_pin(&dir, &source).expect("rewrite");
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].changed_lines, 3);
        let text = &edits[0].text;
        assert!(text.contains(
            "regia_dylib = { git = \"https://example.com/regia.git\", tag = \"v0.2.5\", optional = true } # anchor\n"
        ), "{text}");
        assert!(text.contains(
            "regia_platform = { git = \"https://example.com/regia.git\", tag = \"v0.2.5\", features = [\"a\", \"b\"], default-features = false }\n"
        ), "{text}");
        assert!(text.contains("serde = { version = \"1\", features = [\"derive\"] }\n"));
        let parsed: toml::Table = toml::from_str(text).expect("still TOML");
        assert!(parsed.contains_key("dependencies"));
        std::fs::write(dir.join("game/Cargo.toml"), text).expect("write");
        assert!(
            rewrite_pin(&dir, &source).expect("again").is_empty(),
            "idempotent"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn relative_paths_are_written_per_manifest_depth() {
        let dir = project(&[
            (
                "Cargo.toml",
                "[package]\nname = \"q\"\n[dependencies]\nregia = { path = \"/x/crates/regia\" }\n",
            ),
            (
                "game/Cargo.toml",
                "[dependencies]\nregia = { path = \"/x/crates/regia\" }\n",
            ),
            ("a/b/Cargo.toml", "[dependencies]\nregia = \"0.2\"\n"),
        ]);
        let source = EngineSource::Path("vendor/regia-engine-0.2.5".into());
        let edits = rewrite_pin(&dir, &source).expect("rewrite");
        let by: std::collections::BTreeMap<_, _> =
            edits.iter().map(|e| (e.file.as_str(), e.text.as_str())).collect();
        assert!(by["Cargo.toml"].contains("regia = { path = \"vendor/regia-engine-0.2.5/crates/regia\" }"));
        assert!(
            by["game/Cargo.toml"]
                .contains("regia = { path = \"../vendor/regia-engine-0.2.5/crates/regia\" }")
        );
        assert!(
            by["a/b/Cargo.toml"]
                .contains("regia = { path = \"../../vendor/regia-engine-0.2.5/crates/regia\" }")
        );
        for (file, text) in &by {
            std::fs::write(dir.join(file), text).expect("write");
        }
        let pin = read_pin(&dir).expect("pin");
        assert_eq!(
            pin.sources(&dir),
            vec!["path:vendor/regia-engine-0.2.5".to_string()]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// ADR-10616: a path source in Windows' verbatim form (what `canonicalize` returns there) is
    /// pinned as `C:/…`; `//?/C:/…` makes Cargo fail with "invalid path url".
    #[test]
    fn verbatim_windows_path_sources_are_pinned_without_the_prefix() {
        let dir = project(&[(
            "Cargo.toml",
            "[package]\nname = \"q\"\n[dependencies]\nregia = { path = \"/x/crates/regia\" }\n",
        )]);
        let source = EngineSource::Path(r"\\?\C:\Users\me\AppData\Local\Regia\e".into());
        assert_eq!(source.to_string(), "path:C:/Users/me/AppData/Local/Regia/e");
        let edits = rewrite_pin(&dir, &source).expect("rewrite");
        assert_eq!(edits.len(), 1);
        assert!(
            edits[0]
                .text
                .contains("regia = { path = \"C:/Users/me/AppData/Local/Regia/e/crates/regia\" }"),
            "{}",
            edits[0].text
        );
        assert!(!edits[0].text.contains("//?/"), "{}", edits[0].text);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn registry_sources_pin_the_exact_version() {
        let dir = project(&[(
            "Cargo.toml",
            "[dependencies]\nregia = { path = \"../e/crates/regia\" }\n",
        )]);
        let source = EngineSource::Registry {
            name: "studio".into(),
            version: regia_core::engine_version::EngineVersion::new(0, 2, 5),
        };
        let edits = rewrite_pin(&dir, &source).expect("rewrite");
        assert!(
            edits[0]
                .text
                .contains("regia = { version = \"=0.2.5\", registry = \"studio\" }")
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn workspace_references_are_sites_but_not_rewritten() {
        let dir = project(&[
            (
                "Cargo.toml",
                "[workspace]\n[workspace.dependencies]\nregia = { path = \"/e/crates/regia\" }\n",
            ),
            (
                "game/Cargo.toml",
                "[dependencies]\nregia = { workspace = true }\nregia_dylib.workspace = true\n",
            ),
        ]);
        let pin = read_pin(&dir).expect("pin");
        assert_eq!(pin.sites.len(), 3);
        assert_eq!(pin.sites[1].source, DepSource::Workspace);
        assert_eq!(pin.sites[2].source, DepSource::Workspace);
        let edits = rewrite_pin(&dir, &EngineSource::Path("/f".into())).expect("rewrite");
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].file, "Cargo.toml");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unsupported_layouts_are_refused_with_the_line() {
        for (text, line) in [
            ("[dependencies.regia]\npath = \"/e/crates/regia\"\n", 1),
            ("[dependencies]\nregia.path = \"/e/crates/regia\"\n", 2),
            ("[dependencies]\nregia = {\n  path = \"/e\" }\n", 2),
        ] {
            let dir = project(&[("Cargo.toml", text)]);
            let err = read_pin(&dir).expect_err(text);
            assert_eq!(err.code, "engine_dep_layout", "{text}");
            assert!(
                err.message.starts_with(&format!("Cargo.toml:{line}:")),
                "{}",
                err.message
            );
            std::fs::remove_dir_all(&dir).ok();
        }
    }

    #[test]
    fn engine_trees_target_and_git_are_not_walked() {
        let dep = "[dependencies]\nregia = { path = \"/e/crates/regia\" }\n";
        let dir = project(&[
            ("Cargo.toml", dep),
            ("target/x/Cargo.toml", dep),
            (".git/Cargo.toml", dep),
            ("vendor/regia-engine-0.2.5/regia-engine.release.ron", "()"),
            ("vendor/regia-engine-0.2.5/crates/regia_cli/Cargo.toml", dep),
            ("engine/crates/regia/Cargo.toml", dep),
            ("engine/Cargo.toml", dep),
        ]);
        let pin = read_pin(&dir).expect("pin");
        let files: Vec<_> = pin.sites.iter().map(|s| s.file.as_str()).collect();
        assert_eq!(files, vec!["Cargo.toml"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn path_sources_must_hold_every_pinned_crate() {
        let dir = project(&[
            (
                "Cargo.toml",
                "[dependencies]\nregia = { path = \"/e/crates/regia\" }\nregia_dylib = { path = \"/e/crates/regia_dylib\" }\n",
            ),
            ("rel/crates/regia/Cargo.toml", "[package]\n"),
        ]);
        let err = rewrite_pin(&dir, &EngineSource::Path("rel".into())).expect_err("missing crate");
        assert_eq!(err.code, "engine_crate_missing");
        assert!(err.message.contains("regia_dylib"), "{}", err.message);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_game_crate_named_like_an_engine_crate_is_the_projects_own() {
        let launcher = "[package]\nname = \"regia_arena_launcher\"\n\n[dependencies]\n\
                        regia = { path = \"/e/crates/regia\" }\n\
                        regia_arena = { path = \"../game\" }\n\n\
                        [dependencies.regia_arena_gen]\npath = \"../gen\"\n";
        let dir = project(&[
            ("game/Cargo.toml", "[package]\nname = \"regia_arena\"\n"),
            ("gen/Cargo.toml", "[package]\nname = \"regia_arena_gen\"\n"),
            ("launcher/Cargo.toml", launcher),
        ]);
        let pin = read_pin(&dir).expect("the table layout of an own crate is not refused");
        let keys: Vec<_> = pin.sites.iter().map(|s| s.krate.as_str()).collect();
        assert_eq!(keys, vec!["regia"]);

        let git = EngineSource::Git {
            url: "https://example.com/regia.git".into(),
            tag: "v0.2.5".into(),
        };
        let edits = rewrite_pin(&dir, &git).expect("rewrite");
        assert_eq!(edits.len(), 1);
        assert!(
            edits[0].text.contains("regia_arena = { path = \"../game\" }\n"),
            "{}",
            edits[0].text
        );
        assert!(edits[0].text.contains("regia = { git = "), "{}", edits[0].text);

        // A path source holds only the engine's crates: the game must not count as a missing one.
        let release = regia_test_utils::unique_temp_dir("dist_pin_own_rel");
        std::fs::create_dir_all(release.join("crates/regia")).expect("crate dir");
        std::fs::write(release.join("crates/regia/Cargo.toml"), "[package]\n").expect("crate");
        let edits = rewrite_pin(&dir, &EngineSource::Path(release.clone())).expect("path rewrite");
        assert_eq!(edits[0].changed_lines, 1);
        for path in [dir, release] {
            std::fs::remove_dir_all(path).ok();
        }
    }

    #[test]
    fn crlf_manifests_keep_their_line_endings() {
        let dir = project(&[(
            "Cargo.toml",
            "[dependencies]\r\nregia = { path = \"/e/crates/regia\" }\r\n",
        )]);
        let edits = rewrite_pin(&dir, &EngineSource::Path("/f".into())).expect("rewrite");
        assert_eq!(
            edits[0].text,
            "[dependencies]\r\nregia = { path = \"/f/crates/regia\" }\r\n"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn stamp_preserves_multiline_user_text_and_ignores_fake_headers_and_keys() {
        let line = ReleaseLine::pre_1_0(2);
        for quote in ["\"\"\"", "'".repeat(3).as_str()] {
            for ending in ["\n", "\r\n"] {
                let text = format!("[project]\nname = \"G\"\ndescription = {quote}\n[note]\nengine = \"keep this user text\"\n{quote}\n\n[steam]\n").replace('\n', ending);
                let old: toml::Table = toml::from_str(&text).expect("input");
                let stamped = stamp_engine_line(&text, line).expect("stamp");
                let parsed: toml::Table = toml::from_str(&stamped).expect("output");
                assert_eq!(parsed["project"]["engine"].as_str(), Some("0.2"));
                assert_eq!(parsed["project"]["description"], old["project"]["description"]);
                assert_eq!(stamped.replace(&format!("engine = \"0.2\"{ending}"), ""), text);
                assert_eq!(stamp_engine_line(&stamped, line), Some(stamped));
            }
        }
    }

    #[test]
    fn stamp_requires_an_explicit_project_header_and_keeps_nested_tables() {
        let line = ReleaseLine::pre_1_0(2);
        for text in [
            "[project.note]\ntext = 'keep'\n",
            "project.name = 'G'\n",
            "project = { name = 'G' }\n",
        ] {
            assert_eq!(stamp_engine_line(text, line), None, "{text}");
        }
        let text = "[project.note]\ntext = 'keep'\n[project]\nname = 'G'\n";
        let stamped = stamp_engine_line(text, line).expect("explicit parent after child");
        let parsed: toml::Table = toml::from_str(&stamped).expect("valid result");
        assert_eq!(parsed["project"]["engine"].as_str(), Some("0.2"));
        assert_eq!(parsed["project"]["note"]["text"].as_str(), Some("keep"));
    }

    #[test]
    fn stamp_replaces_or_inserts_the_engine_line() {
        let line = ReleaseLine::pre_1_0(2);
        let text = "[project]\nname = \"G\"\nengine = \"0.1\" # pinned\nversion = \"1\"\n\n[platforms.android]\nmin_sdk = 29\n";
        assert_eq!(
            stamp_engine_line(text, line).expect("stamp"),
            "[project]\nname = \"G\"\nengine = \"0.2\" # pinned\nversion = \"1\"\n\n[platforms.android]\nmin_sdk = 29\n"
        );
        let text = "[project]\nname = \"G\"\nid = \"a.b\"\n\n[steam]\nengine = \"x\"\n";
        assert_eq!(
            stamp_engine_line(text, line).expect("insert"),
            "[project]\nname = \"G\"\nid = \"a.b\"\nengine = \"0.2\"\n\n[steam]\nengine = \"x\"\n"
        );
        assert_eq!(
            stamp_engine_line("[project]\nname = \"G\"", line).expect("no newline"),
            "[project]\nname = \"G\"\nengine = \"0.2\"\n"
        );
        assert!(stamp_engine_line("[steam]\n", line).is_none());
        let once = stamp_engine_line(text, line).expect("insert");
        assert_eq!(stamp_engine_line(&once, line).expect("again"), once);
    }

    #[test]
    fn requirements_are_read_from_the_manifest() {
        let dir = project(&[(
            "Regia.toml",
            "[project]\nname = \"G\"\nid = \"a.b\"\nengine = \"0.1\"\n",
        )]);
        let req = read_requirement(&dir).expect("read").expect("present");
        assert_eq!(req.line(), ReleaseLine::pre_1_0(1));
        std::fs::write(dir.join("Regia.toml"), "[project]\nname = \"G\"\n").expect("write");
        assert_eq!(read_requirement(&dir).expect("read"), None);
        std::fs::write(dir.join("Regia.toml"), "[project]\nengine = \"latest\"\n").expect("write");
        assert_eq!(
            read_requirement(&dir).expect_err("bad").code,
            "engine_requirement_invalid"
        );
        std::fs::write(dir.join("Regia.toml"), "[project]\nengine = 2\n").expect("write");
        assert_eq!(
            read_requirement(&dir).expect_err("bad").code,
            "engine_requirement_invalid"
        );
        std::fs::remove_dir_all(&dir).ok();
        let empty = regia_test_utils::unique_temp_dir("dist_pin_none");
        assert_eq!(
            read_requirement(&empty).expect_err("none").code,
            "manifest_missing"
        );
        std::fs::remove_dir_all(&empty).ok();
    }

    #[test]
    fn path_sources_report_the_engine_root() {
        let site = |file: &str, path: &str| DepSite {
            file: file.into(),
            line: 1,
            krate: "regia".into(),
            source: DepSource::Path(path.into()),
        };
        let project = Path::new("/p");
        assert_eq!(
            site("game/Cargo.toml", "../vendor/e/crates/regia").engine_source_text(project),
            Some("path:vendor/e".into())
        );
        assert_eq!(
            site("game/Cargo.toml", "../../e/crates/regia").engine_source_text(project),
            Some("path:/e".into())
        );
        assert_eq!(
            site("Cargo.toml", "C:/e/crates/regia").engine_source_text(project),
            Some("path:C:/e".into())
        );
    }
}
